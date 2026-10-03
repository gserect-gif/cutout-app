mod cpucheck;
mod image_ops;
mod inference;
mod memstat;

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use image::DynamicImage;
use serde::Serialize;
use tauri::{Emitter, Manager};
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
        "This model isn't installed. Try picking a different one from the dropdown, or reinstall Cutout to restore it. (Expected file: {0})"
    )]
    ModelMissing(String),

    #[error("The AI model failed to load: {0}")]
    ModelLoadFailed(String),

    #[error("Background removal failed: {0}")]
    InferenceFailed(String),

    #[error("No image is loaded yet.")]
    NoImageLoaded,

    #[error("Cancelled.")]
    Cancelled,
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

/// One entry in a batch job's outcome: either a successful result or a
/// per-file error, keyed by the original source path so the frontend can
/// match it back to the item in its list. Kept separate from the single-image
/// `RemoveBackgroundResult` (rather than reusing it with an Option) so a
/// batch failure is structurally impossible to mistake for a success on the
/// frontend — the two cases are different variants, not different states of
/// the same shape.
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase", tag = "status")]
enum BatchItemResult {
    #[serde(rename_all = "camelCase")]
    Success {
        source_path: String,
        result_path: String,
        width: u32,
        height: u32,
        elapsed_ms: u64,
    },
    #[serde(rename_all = "camelCase")]
    Failed {
        source_path: String,
        error: String,
    },
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

/// Set when the user cancels an in-progress operation. Checked between
/// pipeline stages (not mid-inference — ONNX Runtime's `run()` call can't be
/// safely interrupted once started) so cancellation takes effect promptly
/// without corrupting state. For a batch, this means "finish the current
/// image, then stop" rather than aborting mid-image, which is both safer
/// and matches what a user asking to cancel actually expects: the last
/// image they can see processing is allowed to complete.
static CANCEL_REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[tauri::command]
fn cancel_processing() {
    CANCEL_REQUESTED.store(true, std::sync::atomic::Ordering::SeqCst);
}

fn clear_cancel_flag() {
    CANCEL_REQUESTED.store(false, std::sync::atomic::Ordering::SeqCst);
}

fn is_cancelled() -> bool {
    CANCEL_REQUESTED.load(std::sync::atomic::Ordering::SeqCst)
}

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

/// Resolve a specific model's file path. In production builds models live
/// in the app's resource directory (see `bundle.resources` in
/// tauri.conf.json); in `tauri dev` there is no bundle, so we fall back to
/// the repo-relative `../models/` used during development.
fn resolve_model_path(app: &tauri::AppHandle, model: inference::ModelId) -> PathBuf {
    let filename = model.filename();

    if let Ok(resource_dir) = app.path().resource_dir() {
        let bundled = resource_dir.join("models").join(filename);
        if bundled.exists() {
            return bundled;
        }
    }

    // Development fallback: relative to src-tauri/ (the cargo working dir).
    PathBuf::from("../models").join(filename)
}

/// Which models are actually present on disk. Some are optional downloads,
/// so the frontend asks this to know which picker options to enable instead
/// of listing all four and failing later on a missing one.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ModelAvailability {
    key: String,
    available: bool,
    /// File size in MB, when the model is present. Lets the frontend show
    /// "224 MB on disk" style detail without a second round trip.
    size_mb: Option<u64>,
    /// One-line description shown as the dropdown option's tooltip, so
    /// picking a model doesn't require memorizing what "Balanced" means.
    description: String,
}

#[tauri::command]
fn list_available_models(app: tauri::AppHandle) -> Vec<ModelAvailability> {
    [
        (
            "fast",
            inference::ModelId::U2NetP,
            "Fastest option. Good for quick previews or simple subjects; edges are less refined than Best.",
        ),
        (
            "balanced",
            inference::ModelId::IsNetGeneral,
            "A middle ground between speed and edge quality \u{2014} solid default for everyday photos.",
        ),
        (
            "best",
            inference::ModelId::BiRefNetLite,
            "High-quality edges, including fine detail like hair. Slower than Fast/Balanced.",
        ),
        (
            "best-plus",
            inference::ModelId::BiRefNetFull,
            "The sharpest results this app can produce. Noticeably slower \u{2014} best for final exports, not quick checks.",
        ),
    ]
    .into_iter()
    .map(|(key, id, description)| {
        let path = resolve_model_path(&app, id);
        let size_mb = std::fs::metadata(&path).ok().map(|m| m.len() / (1024 * 1024));
        ModelAvailability {
            key: key.to_string(),
            available: path.exists(),
            size_mb,
            description: description.to_string(),
        }
    })
    .collect()
}

/// Core single-image pipeline: decode (or reuse an already-decoded) source
/// image, run inference, composite the alpha mask, write the result PNG to
/// disk. Shared by both `remove_background` (single-image UI flow) and
/// `remove_background_batch` (multi-image flow) so the two paths can never
/// silently drift apart — there is exactly one implementation of "how a
/// background gets removed," not two copies maintained in parallel.
///
/// Deliberately does NOT touch `LAST_RESULT_PATH` — that single-slot
/// cleanup mechanism is specific to the single-image workflow, where only
/// one result is ever meant to exist at a time. Batch mode needs every
/// result to survive simultaneously until the user exports them all, so its
/// caller manages result-file lifetime itself instead.
fn run_removal_pipeline(
    app: &tauri::AppHandle,
    path: &str,
    model: inference::ModelId,
    cached: Option<Arc<DynamicImage>>,
) -> Result<(String, u32, u32, u64), AppError> {
    if is_cancelled() {
        return Err(AppError::Cancelled);
    }

    let start = Instant::now();
    memstat::log_memory("run_removal_pipeline: start");

    let t0 = Instant::now();
    let image: Arc<DynamicImage> = match cached {
        Some(img) => img,
        None => {
            let loaded = image_ops::load_image(path)?;
            Arc::new(loaded.image)
        }
    };
    let (width, height) = (image.width(), image.height());
    eprintln!("[perf] get source image (cache hit or decode): {:?}", t0.elapsed());
    memstat::log_memory("run_removal_pipeline: after get source image");

    let model_path = resolve_model_path(app, model);
    let input_size = inference::input_size_for(model);

    let t1 = Instant::now();
    let model_input = image_ops::prepare_model_input(&image, input_size);
    eprintln!("[perf] resize to model input: {:?}", t1.elapsed());

    let t2 = Instant::now();
    let raw_mask = inference::run_inference(model, &model_path, &model_input)?;
    eprintln!(
        "[perf] ONNX inference (incl. first-call model load if not yet cached): {:?}",
        t2.elapsed()
    );
    memstat::log_memory("run_removal_pipeline: after ONNX inference");
    drop(model_input);

    let t3 = Instant::now();
    let full_res_mask = image_ops::resize_mask_to(&raw_mask, input_size, width, height);
    eprintln!("[perf] upscale mask to original resolution: {:?}", t3.elapsed());
    drop(raw_mask);

    // Checked here (after the expensive inference step, before the file
    // write) rather than only at the top of the function, so cancelling
    // during a slow single-image run still stops short of writing a result
    // nobody asked for — the alternative, not checking again, would mean
    // cancel only ever takes effect on the *next* image in a batch, never
    // the one currently running.
    if is_cancelled() {
        return Err(AppError::Cancelled);
    }

    let t4 = Instant::now();
    let composited = image_ops::apply_alpha_mask(&image, &full_res_mask, width, height);
    eprintln!("[perf] composite alpha mask: {:?}", t4.elapsed());
    drop(full_res_mask);
    drop(image);
    memstat::log_memory("run_removal_pipeline: after compositing, source dropped");

    let t5 = Instant::now();
    let dynamic = image::DynamicImage::ImageRgba8(composited);
    let result_dir = results_dir(app)?;
    let n = RESULT_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let result_path = result_dir.join(format!("result-{}-{n}.png", std::process::id()));
    dynamic
        .save_with_format(&result_path, image::ImageFormat::Png)
        .map_err(|e| AppError::EncodeFailed(e.to_string()))?;
    eprintln!("[perf] write result PNG to disk: {:?}", t5.elapsed());

    eprintln!("[perf] TOTAL: {:?}", start.elapsed());
    memstat::log_line(&format!(
        "[perf] SUMMARY model={:?} image={}x{} total={:?}",
        model,
        width,
        height,
        start.elapsed()
    ));
    eprintln!(
        "[perf] source image: {}x{} ({} MB decoded RGBA estimate)",
        width,
        height,
        (width as u64 * height as u64 * 4) / (1024 * 1024)
    );
    memstat::log_memory("run_removal_pipeline: end");

    Ok((
        result_path.display().to_string(),
        width,
        height,
        start.elapsed().as_millis() as u64,
    ))
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
    model: Option<String>,
    cache: tauri::State<'_, ImageCache>,
) -> Result<RemoveBackgroundResult, AppError> {
    let model_id = model
        .as_deref()
        .and_then(inference::ModelId::from_key)
        .unwrap_or(inference::ModelId::BiRefNetLite);

    clear_cancel_flag();

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
        let (result_path, width, height, elapsed_ms) =
            run_removal_pipeline(&app, &path, model_id, cached)?;

        // Now that the new result is safely on disk, remove the previous
        // one (if any) so single-image-mode results don't accumulate
        // indefinitely across a session. Batch mode does not use this path
        // — see `run_removal_pipeline`'s doc comment.
        if let Ok(mut last) = LAST_RESULT_PATH.lock() {
            if let Some(old_path) = last.replace(PathBuf::from(&result_path)) {
                if let Err(e) = std::fs::remove_file(&old_path) {
                    eprintln!("[perf] couldn't remove previous result file (non-fatal): {e}");
                }
            }
        }

        Ok(RemoveBackgroundResult {
            result_path,
            width,
            height,
            elapsed_ms,
        })
    })
    .await
    .map_err(|e| AppError::InferenceFailed(format!("background task panicked: {e}")))?
}

const SUPPORTED_EXTENSIONS: [&str; 8] =
    ["png", "jpg", "jpeg", "webp", "bmp", "tiff", "tif", "gif"];

/// Recursively collects paths of all supported image files under `dir`,
/// including subfolders, so picking a folder for batch processing behaves
/// like "grab every image anywhere inside this," matching how people
/// typically organize photos into nested date/event subfolders. Silently
/// skips directories it can't read (permissions, etc.) rather than failing
/// the whole scan over one inaccessible subfolder.
fn collect_images_recursive(dir: &std::path::Path, out: &mut Vec<String>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_images_recursive(&path, out);
        } else if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if SUPPORTED_EXTENSIONS.contains(&ext.to_lowercase().as_str()) {
                out.push(path.display().to_string());
            }
        }
    }
}

#[tauri::command]
fn scan_folder_for_images(folder_path: String) -> Result<Vec<String>, AppError> {
    let dir = PathBuf::from(&folder_path);
    if !dir.is_dir() {
        return Err(AppError::FileNotFound(folder_path));
    }

    let mut images = Vec::new();
    collect_images_recursive(&dir, &mut images);
    images.sort();
    Ok(images)
}

/// Directory for small batch-list thumbnail previews, separate from the
/// full-resolution `results_dir` so the two purposes (final output vs. a
/// quick 96px preview) never share cleanup logic or naming by accident.
static THUMBS_DIR: OnceLock<PathBuf> = OnceLock::new();

fn thumbs_dir(app: &tauri::AppHandle) -> Result<&'static PathBuf, AppError> {
    if let Some(dir) = THUMBS_DIR.get() {
        return Ok(dir);
    }

    let dir = app
        .path()
        .app_cache_dir()
        .map_err(|e| AppError::Io(format!("couldn't resolve app cache directory: {e}")))?
        .join("thumbs");

    std::fs::create_dir_all(&dir).map_err(|e| AppError::Io(e.to_string()))?;

    let _ = THUMBS_DIR.set(dir);
    Ok(THUMBS_DIR.get().expect("just set"))
}

/// Generates a small (96px on the long edge) preview of a source image for
/// display in the batch list, so items are recognizable at a glance instead
/// of by filename alone. Returns a file path (loaded via the asset protocol
/// by the frontend), not base64 \u2014 batches can be dozens of images, and
/// base64-encoding each one into the JS heap is exactly the memory pattern
/// this app moved away from for the main preview/result images earlier.
#[tauri::command]
fn generate_thumbnail(app: tauri::AppHandle, path: String) -> Result<String, AppError> {
    let loaded = image_ops::load_image(&path)?;

    const THUMB_MAX: u32 = 96;
    let (w, h) = (loaded.width, loaded.height);
    let (tw, th) = if w >= h {
        (THUMB_MAX, (h as f32 * THUMB_MAX as f32 / w as f32).round() as u32)
    } else {
        ((w as f32 * THUMB_MAX as f32 / h as f32).round() as u32, THUMB_MAX)
    };

    let thumb = loaded
        .image
        .resize(tw.max(1), th.max(1), image::imageops::FilterType::Triangle);

    let dir = thumbs_dir(&app)?;
    let n = RESULT_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let thumb_path = dir.join(format!("thumb-{}-{n}.png", std::process::id()));
    thumb
        .save_with_format(&thumb_path, image::ImageFormat::Png)
        .map_err(|e| AppError::EncodeFailed(e.to_string()))?;

    Ok(thumb_path.display().to_string())
}

/// Path of the most recently written fallback preview, so the previous one
/// can be deleted when a new image is loaded (a preview is only ever needed
/// for the image currently on screen).
static LAST_PREVIEW_PATH: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Longest edge, in pixels, of a fallback preview image.
const PREVIEW_MAX_EDGE: u32 = 2560;

/// Builds a PNG preview of a source image that the WebView can't display
/// directly. TIFF is the main case: Rust decodes it fine, but the WebView
/// (Chromium-based) has no TIFF decoder, so loading the file straight into
/// an <img> fails. The frontend only calls this after that direct load has
/// failed, so formats the WebView handles natively keep their original,
/// full-resolution preview. Processing and export never use this file; they
/// always work from the original.
#[tauri::command]
async fn generate_preview(
    app: tauri::AppHandle,
    path: String,
    cache: tauri::State<'_, ImageCache>,
) -> Result<String, AppError> {
    // Reuse the decoded image from `load_image` when it matches, instead of
    // decoding the same large file a second time.
    let cached: Option<Arc<DynamicImage>> = cache
        .0
        .lock()
        .map_err(|_| AppError::Io("image cache lock poisoned".into()))?
        .as_ref()
        .filter(|(cached_path, _)| cached_path == &path)
        .map(|(_, img)| Arc::clone(img));

    tauri::async_runtime::spawn_blocking(move || {
        let image: Arc<DynamicImage> = match cached {
            Some(img) => img,
            None => Arc::new(image_ops::load_image(&path)?.image),
        };

        let preview: DynamicImage = if image.width().max(image.height()) > PREVIEW_MAX_EDGE {
            image.resize(
                PREVIEW_MAX_EDGE,
                PREVIEW_MAX_EDGE,
                image::imageops::FilterType::Triangle,
            )
        } else {
            (*image).clone()
        };

        let dir = results_dir(&app)?.join("previews");
        std::fs::create_dir_all(&dir).map_err(|e| AppError::Io(e.to_string()))?;

        let n = RESULT_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let preview_path = dir.join(format!("preview-{}-{n}.png", std::process::id()));

        // Normalized to 8-bit RGBA first: TIFFs can be 16-bit or other
        // layouts, and this guarantees PNG can encode whatever came in.
        DynamicImage::ImageRgba8(preview.to_rgba8())
            .save_with_format(&preview_path, image::ImageFormat::Png)
            .map_err(|e| AppError::EncodeFailed(e.to_string()))?;

        if let Ok(mut last) = LAST_PREVIEW_PATH.lock() {
            if let Some(old) = last.replace(preview_path.clone()) {
                let _ = std::fs::remove_file(old);
            }
        }

        Ok(preview_path.display().to_string())
    })
    .await
    .map_err(|e| AppError::InferenceFailed(format!("preview task panicked: {e}")))?
}

/// Processes a list of image paths sequentially, emitting a `batch-progress`
/// event after each one completes (success or failure) so the frontend can
/// show live per-file progress rather than waiting for the entire batch to
/// finish. Failures are recorded and skipped, not fatal to the batch — later
/// images still get processed. Returns the full set of results at the end
/// as well, as a convenience for the caller, though the frontend is expected
/// to build its list incrementally from the events as they arrive.
#[tauri::command]
async fn remove_background_batch(
    app: tauri::AppHandle,
    paths: Vec<String>,
    model: Option<String>,
) -> Result<Vec<BatchItemResult>, AppError> {
    let model_id = model
        .as_deref()
        .and_then(inference::ModelId::from_key)
        .unwrap_or(inference::ModelId::BiRefNetLite);

    clear_cancel_flag();

    tauri::async_runtime::spawn_blocking(move || {
        let mut results = Vec::with_capacity(paths.len());

        for path in paths {
            // No `ImageCache` reuse here — unlike the single-image flow,
            // batch items were never individually previewed/decoded first
            // via `load_image`, so there is nothing to reuse. Each image is
            // decoded fresh from disk as part of `run_removal_pipeline`.
            let item = match run_removal_pipeline(&app, &path, model_id, None) {
                Ok((result_path, width, height, elapsed_ms)) => BatchItemResult::Success {
                    source_path: path.clone(),
                    result_path,
                    width,
                    height,
                    elapsed_ms,
                },
                Err(AppError::Cancelled) => {
                    // Stop the batch here rather than recording this item as
                    // failed and continuing — a cancellation is a request to
                    // stop everything, not a per-file problem. Images
                    // already completed before this point keep their
                    // results; nothing already written is deleted.
                    eprintln!("[batch] cancelled before: {path}");
                    break;
                }
                Err(e) => {
                    eprintln!("[batch] failed on {path}: {e}");
                    BatchItemResult::Failed {
                        source_path: path.clone(),
                        error: e.to_string(),
                    }
                }
            };

            // Best-effort: a failed event emit doesn't abort the batch — the
            // full `results` vec returned at the end is the fallback if the
            // frontend somehow misses a live event.
            if let Err(e) = app.emit("batch-progress", &item) {
                eprintln!("[batch] couldn't emit progress event (non-fatal): {e}");
            }

            results.push(item);
        }

        Ok(results)
    })
    .await
    .map_err(|e| AppError::InferenceFailed(format!("batch task panicked: {e}")))?
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportInfo {
    /// Size of the original uploaded photo, for an actual before/after
    /// comparison (a PNG-with-alpha export can be larger than a compressed
    /// JPEG source, which surprises people if it's invisible).
    original_size_bytes: Option<u64>,
    exported_size_bytes: u64,
}

#[tauri::command]
fn export_png(
    original_path: Option<String>,
    result_path: String,
    destination: String,
) -> Result<ExportInfo, AppError> {
    if !std::path::Path::new(&result_path).exists() {
        return Err(AppError::Io(
            "The processed image is no longer available. Try removing the background again before exporting."
                .to_string(),
        ));
    }

    let original_size_bytes = original_path.and_then(|p| std::fs::metadata(p).ok()).map(|m| m.len());

    std::fs::copy(&result_path, &destination).map_err(|e| AppError::Io(e.to_string()))?;

    let exported_size_bytes = std::fs::metadata(&destination)
        .map_err(|e| AppError::Io(e.to_string()))?
        .len();

    Ok(ExportInfo {
        original_size_bytes,
        exported_size_bytes,
    })
}

/// Writes raw image bytes (from a clipboard paste in the frontend, via the
/// browser's Clipboard API) to a temp file in the app's cache directory,
/// returning its path. This lets pasted images reuse the exact same
/// `load_image` / preview / processing path as any file opened from disk —
/// no separate in-memory code path to keep in sync with the rest of the app.
///
/// Receives the image bytes as a raw IPC request body (not a JSON `Vec<u8>`
/// argument) per Tauri's documented pattern for binary payloads — a
/// full-resolution pasted image can be tens of millions of bytes, and
/// JSON-encoding that as a number array is both much slower to
/// (de)serialize and far larger over the wire than sending raw bytes.
#[tauri::command]
fn save_pasted_image(
    app: tauri::AppHandle,
    request: tauri::ipc::Request,
) -> Result<String, AppError> {
    let tauri::ipc::InvokeBody::Raw(bytes) = request.body() else {
        return Err(AppError::Io("expected a raw image upload".into()));
    };

    let dir = results_dir(&app)?.join("pasted");
    std::fs::create_dir_all(&dir).map_err(|e| AppError::Io(e.to_string()))?;

    let n = RESULT_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = dir.join(format!("paste-{n}.png"));
    std::fs::write(&path, bytes).map_err(|e| AppError::Io(e.to_string()))?;

    Ok(path.display().to_string())
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct BatchExportItem {
    source_path: String,
    result_path: String,
}

/// Copies every successful batch result into `destination_dir`, naming each
/// file after its original source image (e.g. `photo.jpg` ->
/// `photo-cutout.png`) so exported files stay recognizable without the
/// caller needing to track a mapping themselves. Continues past individual
/// copy failures (e.g. a locked file) rather than aborting the whole export,
/// consistent with the batch-processing step's own skip-and-continue design.
#[tauri::command]
fn export_batch(
    items: Vec<BatchExportItem>,
    destination_dir: String,
) -> Result<Vec<String>, AppError> {
    let dest_dir = PathBuf::from(&destination_dir);
    std::fs::create_dir_all(&dest_dir).map_err(|e| AppError::Io(e.to_string()))?;

    let mut failures = Vec::new();

    for item in items {
        let base_name = PathBuf::from(&item.source_path)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "cutout".to_string());

        let mut target = dest_dir.join(format!("{base_name}-cutout.png"));
        // Avoid overwriting if two source files share a name (e.g. two
        // different folders both containing "photo.jpg" selected together).
        let mut suffix = 1;
        while target.exists() {
            target = dest_dir.join(format!("{base_name}-cutout-{suffix}.png"));
            suffix += 1;
        }

        if let Err(e) = std::fs::copy(&item.result_path, &target) {
            eprintln!("[batch export] failed for {}: {e}", item.source_path);
            failures.push(item.source_path);
        }
    }

    Ok(failures)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    memstat::reset_log();
    memstat::log_line(&cpucheck::describe_cpu());

    // The bundled ONNX Runtime needs AVX2. Without it the first AI call
    // crashes silently, so stop here with a clear message instead.
    if !cpucheck::cpu_is_supported() {
        memstat::log_line("[startup] CPU has no AVX2, showing error and exiting");
        cpucheck::show_unsupported_cpu_error();
        std::process::exit(1);
    }

    memstat::log_memory("app startup (before model ever loaded)");

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(ImageCache(Mutex::new(None)))
        .invoke_handler(tauri::generate_handler![
            load_image,
            remove_background,
            remove_background_batch,
            scan_folder_for_images,
            list_available_models,
            generate_thumbnail,
            generate_preview,
            cancel_processing,
            export_png,
            export_batch,
            save_pasted_image
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
