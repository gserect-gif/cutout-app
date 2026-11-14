//! Custom themes: background images and theme files.
//!
//! The colors of a theme are small and live in the frontend. Its background
//! image is a real file, so it is copied into Cutout's own data folder
//! (`themes/images`). That way the theme keeps working if the original picture
//! is moved or deleted. A theme can also be exported as one `.cutouttheme`
//! file that carries its image inside, and imported again on another computer.
//! Everything here is local: no network is involved.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use tauri::Manager;

use crate::AppError;

/// Picture types accepted as a theme background.
const IMAGE_EXTENSIONS: [&str; 6] = ["png", "jpg", "jpeg", "webp", "bmp", "gif"];

/// Largest background picture accepted (a theme file holds it as text, so it
/// is capped well below what the rest of the app could handle).
const MAX_IMAGE_BYTES: u64 = 40 * 1024 * 1024;

static IMAGE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn images_dir(app: &tauri::AppHandle) -> Result<PathBuf, AppError> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| AppError::Io(format!("couldn't resolve app data directory: {e}")))?
        .join("themes")
        .join("images");
    std::fs::create_dir_all(&dir).map_err(|e| AppError::Io(e.to_string()))?;
    Ok(dir)
}

/// A lowercase image extension from the whitelist, or None.
fn image_extension(name: &str) -> Option<String> {
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())?
        .to_ascii_lowercase();
    IMAGE_EXTENSIONS.contains(&ext.as_str()).then_some(ext)
}

/// A new, unique file path inside the theme images folder.
fn new_image_path(app: &tauri::AppHandle, ext: &str) -> Result<PathBuf, AppError> {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let n = IMAGE_COUNTER.fetch_add(1, Ordering::Relaxed);
    Ok(images_dir(app)?.join(format!("theme-{millis}-{n}.{ext}")))
}

/// Copies a picture into Cutout's own folder and returns the new path.
#[tauri::command]
pub fn import_theme_image(app: tauri::AppHandle, source_path: String) -> Result<String, AppError> {
    let source = PathBuf::from(&source_path);
    let ext = image_extension(&source_path)
        .ok_or_else(|| AppError::Io("Choose a PNG, JPG, WebP, BMP or GIF picture.".into()))?;
    let meta = std::fs::metadata(&source)
        .map_err(|_| AppError::Io(format!("File not found: {source_path}")))?;
    if !meta.is_file() {
        return Err(AppError::Io(format!("File not found: {source_path}")));
    }
    if meta.len() > MAX_IMAGE_BYTES {
        return Err(AppError::Io(
            "That picture is too big for a theme background (the limit is 40 MB).".into(),
        ));
    }

    let dest = new_image_path(&app, &ext)?;
    std::fs::copy(&source, &dest)
        .map_err(|e| AppError::Io(format!("couldn't copy the picture: {e}")))?;
    Ok(dest.display().to_string())
}

/// Deletes every picture in the theme images folder that is not in `keep`
/// (the pictures the working theme and the saved themes still use).
#[tauri::command]
pub fn prune_theme_images(app: tauri::AppHandle, keep: Vec<String>) -> Result<(), AppError> {
    let dir = images_dir(&app)?;
    let keep_names: Vec<String> = keep
        .iter()
        .filter_map(|p| Path::new(p).file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect();

    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.path().is_file() && !keep_names.contains(&name) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    Ok(())
}

/// Writes a theme to a `.cutouttheme` file, with its background picture
/// (when it has one) inside the file.
#[tauri::command]
pub async fn export_theme(
    destination: String,
    theme_json: String,
    image_path: Option<String>,
) -> Result<(), AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let mut theme: serde_json::Value = serde_json::from_str(&theme_json)
            .map_err(|e| AppError::Io(format!("The theme couldn't be read: {e}")))?;
        // The picture travels inside the file, not as a path on this computer.
        theme["image"] = serde_json::Value::Null;

        let image = match image_path.as_deref() {
            Some(path) if Path::new(path).is_file() => {
                let name = Path::new(path)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "background.png".into());
                if image_extension(&name).is_none() {
                    return Err(AppError::Io("The background picture has an unknown type.".into()));
                }
                let bytes = std::fs::read(path).map_err(|e| AppError::Io(e.to_string()))?;
                serde_json::json!({ "name": name, "data": BASE64.encode(bytes) })
            }
            _ => serde_json::Value::Null,
        };

        let bundle = serde_json::json!({
            "format": "cutout-theme",
            "version": 1,
            "theme": theme,
            "image": image,
        });
        let text = serde_json::to_string(&bundle).map_err(|e| AppError::Io(e.to_string()))?;
        std::fs::write(&destination, text)
            .map_err(|e| AppError::Io(format!("Couldn't save the theme file: {e}")))
    })
    .await
    .map_err(|e| AppError::Io(format!("theme export task panicked: {e}")))?
}

/// Reads a `.cutouttheme` file. The background picture inside it (if any) is
/// saved into Cutout's own folder, and the theme is returned as JSON text
/// with its `image` set to that new path. The frontend checks every value.
#[tauri::command]
pub async fn import_theme(app: tauri::AppHandle, source_path: String) -> Result<String, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        let meta = std::fs::metadata(&source_path)
            .map_err(|_| AppError::Io(format!("File not found: {source_path}")))?;
        // The picture is stored as base64 text, about a third bigger.
        if meta.len() > MAX_IMAGE_BYTES * 2 {
            return Err(AppError::Io("That theme file is too big.".into()));
        }
        let text = std::fs::read_to_string(&source_path)
            .map_err(|e| AppError::Io(format!("Couldn't read the theme file: {e}")))?;
        let bundle: serde_json::Value = serde_json::from_str(&text)
            .map_err(|_| AppError::Io("That isn't a Cutout theme file.".into()))?;
        if bundle["format"].as_str() != Some("cutout-theme") || !bundle["theme"].is_object() {
            return Err(AppError::Io("That isn't a Cutout theme file.".into()));
        }

        let mut theme = bundle["theme"].clone();
        theme["image"] = serde_json::Value::Null;

        if let (Some(name), Some(data)) = (bundle["image"]["name"].as_str(), bundle["image"]["data"].as_str()) {
            let ext = image_extension(name).ok_or_else(|| {
                AppError::Io("The theme's background picture has an unknown type.".into())
            })?;
            let bytes = BASE64
                .decode(data.trim())
                .map_err(|_| AppError::Io("The theme's background picture is damaged.".into()))?;
            if bytes.len() as u64 > MAX_IMAGE_BYTES {
                return Err(AppError::Io("The theme's background picture is too big.".into()));
            }
            // Must really be a picture, not just a file with a picture's name.
            image::guess_format(&bytes)
                .map_err(|_| AppError::Io("The theme's background picture is damaged.".into()))?;

            let dest = new_image_path(&app, &ext)?;
            std::fs::write(&dest, bytes)
                .map_err(|e| AppError::Io(format!("Couldn't save the theme's picture: {e}")))?;
            theme["image"] = serde_json::Value::String(dest.display().to_string());
        }

        serde_json::to_string(&theme).map_err(|e| AppError::Io(e.to_string()))
    })
    .await
    .map_err(|e| AppError::Io(format!("theme import task panicked: {e}")))?
}
