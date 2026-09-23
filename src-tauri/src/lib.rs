mod image_ops;
mod inference;
mod memstat;

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use image::DynamicImage;
use serde::Serialize;
use tauri::Manager;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("Unsupported file type: .{0}. Please use PNG, JPG, or WebP.")]
    UnsupportedFormat(String),

    #[error("File not found: {0}")]
    FileNotFound(String),

    #[error("Couldn't read the file: {0}")]
    Io(String),

    #[error("Couldn't decode the {0}. The file may be corrupted.")]
    DecodeFailed(String),

    #[error("Couldn't encode the result image: {0}")]
    EncodeFailed(String),

    #[error(
        "The background-removal model is missing from the app's install. Expected it at: {0}"
    )]
    ModelMissing(String),

    #[error("The AI model failed to load: {0}")]
    ModelLoadFailed(String),

    #[error("Background removal failed: {0}")]
    InferenceFailed(String),

    #[error("No image is loaded yet.")]
    NoImageLoaded,
}

// Tauri requires command errors to implement Serialize so they can cross the
// IPC boundary and be caught as rejected promises on the frontend.
impl Serialize for AppError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LoadedImageInfo {
    path: String,
    width: u32,
    height: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RemoveBackgroundResult {
    result_path: String,
    width: u32,
    height: u32,
    elapsed_ms: u64,
}

/// Monotonically increasing counter used to give each generated result file
/// a unique name within this run, so consecutive results never collide or
/// overwrite each other before being viewed/exported.
static RESULT_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Caches the most recently loaded source image (by path) in memory so
/// `remove_background` doesn't have to re-read and re-decode the same file
/// from disk that `load_image` already decoded moments earlier when the
/// image was dropped/browsed. Holds at most one image at a time — V1 only
/// ever works with a single loaded image, so there's nothing to evict.
///
/// Wrapped in `Arc` so handing a copy to a background thread is a cheap
/// pointer/refcount copy, not a full duplicate of the decoded pixel buffer.
/// A large source photo (e.g. a 24MP JPEG) can be 100-300MB once decoded to
/// raw RGBA — cloning that buffer on every single "Remove Background" click,
/// on top of the image already held here and the ~115-224MB ONNX model
/// resident in memory, measurably raises peak RAM usage. On a 16GB machine
/// already carrying a normal multitasking load (browser tabs, etc.), that
/// added pressure is a plausible contributor to system-wide unresponsiveness
/// under load, separate from CPU thread contention.
struct ImageCache(Mutex<Option<(String, Arc<DynamicImage>)>>);

/// Path of the most recently written result file, if any. Tracked so it can
/// be deleted once a new result supersedes it — without this, every
/// "Remove Background" click leaves its output PNG on disk permanently,
/// accumulating for the lifetime of the app session.
static LAST_RESULT_PATH: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Directory where generated result PNGs are written so the WebView can load
/// them via Tauri's asset protocol (`convertFileSrc`) instead of
/// round-tripping through a base64 data URL. Using the app's own cache
/// directory keeps this narrowly scoped — see the `assetProtocol.scope`
/// entry in tauri.conf.json, which only allows serving files from here, not
/// the whole filesystem. Created once, lazily, on first use rather than on
/// every `remove_background` call.
static RESULTS_DIR: OnceLock<PathBuf> = OnceLock::new();

fn results_dir(app: &tauri::AppHandle) -> Result<&'static PathBuf, AppError> {
    if let Some(dir) = RESULTS_DIR.get() {
        return Ok(dir);
    }

    let dir = app
        .path()
        .app_cache_dir()
        .map_err(|e| AppError::Io(format!("couldn't resolve app cache directory: {e}")))?
        .join("results");

    std::fs::create_dir_all(&dir).map_err(|e| AppError::Io(e.to_string()))?;

    let _ = RESULTS_DIR.set(dir);
    Ok(RESULTS_DIR.get().expect("just set"))
}

/// Resolve the bundled model's path. In production builds this lives in the
/// app's resource directory (see `bundle.resources` in tauri.conf.json); in
/// `tauri dev` there is no bundle, so we fall back to the repo-relative
/// `../models/model.onnx` used during development.
fn resolve_model_path(app: &tauri::AppHandle) -> PathBuf {
    if let Ok(resource_dir) = app.path().resource_dir() {
        let bundled = resource_dir.join("models").join("model.onnx");
        if bundled.exists() {
            return bundled;
        }
    }

    // Development fallback: relative to src-tauri/ (the cargo working dir).
    PathBuf::from("../models/model.onnx")
}

#[tauri::command]
fn load_image(
    path: String,
    cache: tauri::State<'_, ImageCache>,
) -> Result<LoadedImageInfo, AppError> {
    let loaded = image_ops::load_image(&path)?;

    // Store the decoded image so `remove_background` can reuse it without
    // re-reading and re-decoding the same file from disk.
    *cache
        .0
        .lock()
        .map_err(|_| AppError::Io("image cache lock poisoned".into()))? =
        Some((path.clone(), Arc::new(loaded.image)));

    memstat::log_memory("load_image: after decode + cache");

    Ok(LoadedImageInfo {
        path,
        width: loaded.width,
        height: loaded.height,
    })
}

#[tauri::command]
async fn remove_background(
    app: tauri::AppHandle,
    path: String,
    cache: tauri::State<'_, ImageCache>,
) -> Result<RemoveBackgroundResult, AppError> {
    // Reuse the already-decoded image from `load_image` if it matches the
    // requested path, avoiding a redundant disk read + decode. Falls back to
    // reading from disk if the cache is empty or stale. Cloning an `Arc`
    // here is a cheap refcount bump, not a copy of the underlying pixel
    // buffer — see `ImageCache` for why that distinction matters for peak
    // memory usage.
    let cached: Option<Arc<DynamicImage>> = cache
        .0
        .lock()
        .map_err(|_| AppError::Io("image cache lock poisoned".into()))?
        .as_ref()
        .filter(|(cached_path, _)| cached_path == &path)
        .map(|(_, img)| Arc::clone(img));

    // Run the CPU-heavy inference + compositing work on a blocking thread so
    // the async IPC runtime (and therefore the UI) stays responsive.
    tauri::async_runtime::spawn_blocking(move || {
        let start = Instant::now();
        memstat::log_memory("remove_background: start");

        let t0 = Instant::now();
        let image: Arc<DynamicImage> = match cached {
            Some(img) => img,
            None => {
                let loaded = image_ops::load_image(&path)?;
                Arc::new(loaded.image)
            }
        };
        let (width, height) = (image.width(), image.height());
        eprintln!("[perf] get source image (cache hit or decode): {:?}", t0.elapsed());
        memstat::log_memory("remove_background: after get source image");

        let model_path = resolve_model_path(&app);

        let t1 = Instant::now();
        let model_input = image_ops::prepare_model_input(&image);
        eprintln!("[perf] resize to model input: {:?}", t1.elapsed());

        let t2 = Instant::now();
        let raw_mask = inference::run_inference(&model_path, &model_input)?;
        eprintln!(
            "[perf] ONNX inference (incl. first-call model load if not yet cached): {:?}",
            t2.elapsed()
        );
        memstat::log_memory("remove_background: after ONNX inference");
        // The resized model-input buffer (1024x1024x3 floats plus the RGB
        // image it was built from) is no longer needed once inference has
        // produced the mask. Drop it explicitly now rather than letting it
        // stay alive until the end of the function — on a system already
        // under memory pressure, holding unneeded multi-megabyte buffers
        // for longer than necessary makes that pressure worse.
        drop(model_input);

        let t3 = Instant::now();
        let full_res_mask =
            image_ops::resize_mask_to(&raw_mask, image_ops::MODEL_INPUT_SIZE, width, height);
        eprintln!("[perf] upscale mask to original resolution: {:?}", t3.elapsed());
        drop(raw_mask);

        let t4 = Instant::now();
        let composited = image_ops::apply_alpha_mask(&image, &full_res_mask, width, height);
        eprintln!("[perf] composite alpha mask: {:?}", t4.elapsed());
        drop(full_res_mask);
        // The full-resolution source image (potentially 100+MB as decoded
        // RGBA) is only needed up to this point, to read pixel colors for
        // compositing. Drop this thread's reference now; if this was the
        // only remaining reference (i.e. `load_image` was called again
        // before this finishes, replacing the cache entry), the underlying
        // buffer is freed immediately instead of lingering until the
        // function returns.
        drop(image);
        memstat::log_memory("remove_background: after compositing, source dropped");

        let t5 = Instant::now();
        let dynamic = image::DynamicImage::ImageRgba8(composited);
        // Write the result directly to a PNG file in the app's cache
        // directory instead of encoding it as a base64 data URL. The
        // frontend loads it via Tauri's asset protocol (`convertFileSrc`),
        // which streams the file directly into the WebView without ever
        // materializing a ~33%-larger base64 text copy of the image in
        // memory on either the Rust or JS side.
        let result_dir = results_dir(&app)?;
        // Unique per call (not just per process) so consecutive results in
        // the same session don't overwrite each other before being viewed
        // or exported — e.g. if a user reprocesses without exporting first.
        let n = RESULT_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let result_path = result_dir.join(format!("result-{}-{n}.png", std::process::id()));
        dynamic
            .save_with_format(&result_path, image::ImageFormat::Png)
            .map_err(|e| AppError::EncodeFailed(e.to_string()))?;
        eprintln!("[perf] write result PNG to disk: {:?}", t5.elapsed());

        // Now that the new result is safely on disk, remove the previous
        // one (if any) so results don't accumulate indefinitely across a
        // session. Best-effort: a failed delete (e.g. the file is still
        // open in another program) is logged, not fatal — it doesn't affect
        // the current operation's success.
        if let Ok(mut last) = LAST_RESULT_PATH.lock() {
            if let Some(old_path) = last.replace(result_path.clone()) {
                if let Err(e) = std::fs::remove_file(&old_path) {
                    eprintln!("[perf] couldn't remove previous result file (non-fatal): {e}");
                }
            }
        }

        eprintln!("[perf] TOTAL: {:?}", start.elapsed());
        eprintln!(
            "[perf] source image: {}x{} ({} MB decoded RGBA estimate)",
            width,
            height,
            (width as u64 * height as u64 * 4) / (1024 * 1024)
        );
        memstat::log_memory("remove_background: end");

        Ok(RemoveBackgroundResult {
            result_path: result_path.display().to_string(),
            width,
            height,
            elapsed_ms: start.elapsed().as_millis() as u64,
        })
    })
    .await
    .map_err(|e| AppError::InferenceFailed(format!("background task panicked: {e}")))?
}

#[tauri::command]
fn export_png(result_path: String, destination: String) -> Result<(), AppError> {
    std::fs::copy(&result_path, &destination).map_err(|e| AppError::Io(e.to_string()))?;
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    memstat::log_memory("app startup (before model ever loaded)");

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(ImageCache(Mutex::new(None)))
        .invoke_handler(tauri::generate_handler![
            load_image,
            remove_background,
            export_png
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
