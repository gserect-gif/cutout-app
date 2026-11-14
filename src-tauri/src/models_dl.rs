//! On-demand model downloads, for the Lite installer.
//!
//! The Lite installer contains only the Fast model. The other bundled models
//! (Balanced, Best, Best+) are downloaded from the GitHub release of the same
//! version when the user clicks Download in Settings. Nothing is downloaded
//! unless the user asks for it. The Full installer ships every model, so on a
//! Full install this code is never needed.
//!
//! How it works: the release carries a small `models-manifest.json` (built by
//! the GitHub workflow) that lists each model file with its size and SHA-256.
//! Cutout reads it, streams the model file into a `.part` file with progress,
//! checks the size and the SHA-256, and only then moves it into place. A
//! failed, cancelled or corrupted download never leaves a half-written model
//! behind.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;
use sha2::{Digest, Sha256};
use tauri::{Emitter, Manager};

use crate::inference::{self, ModelId};
use crate::AppError;

const RELEASE_DOWNLOAD_BASE: &str = "https://github.com/gserect-gif/cutout-app/releases/download";

/// The four bundled models, by the key the frontend uses.
const BUNDLED_KEYS: [&str; 4] = ["fast", "balanced", "best", "best-plus"];

/// Set when the user clicks Cancel on a running download.
static CANCEL: AtomicBool = AtomicBool::new(false);

/// True while a download runs. Only one runs at a time: the models are large
/// and running two together would only slow both down.
static BUSY: AtomicBool = AtomicBool::new(false);

struct BusyGuard;

impl Drop for BusyGuard {
    fn drop(&mut self) {
        BUSY.store(false, Ordering::SeqCst);
    }
}

/// Folder for models the user downloaded. Lives in the app's data folder, so it
/// survives updates and is not touched by the installer.
fn downloaded_dir(app: &tauri::AppHandle) -> Result<PathBuf, AppError> {
    app.path()
        .app_data_dir()
        .map(|dir| dir.join("downloaded-models"))
        .map_err(|e| AppError::Io(format!("couldn't resolve app data directory: {e}")))
}

/// Where a downloaded copy of a bundled model would be, if it exists. Used by
/// `resolve_model_path` in lib.rs after the installer's own copy.
pub fn downloaded_path(app: &tauri::AppHandle, model: ModelId) -> Option<PathBuf> {
    let path = downloaded_dir(app).ok()?.join(model.filename());
    path.exists().then_some(path)
}

/// One bundled model, as the Settings screen shows it.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BundledModelStatus {
    key: String,
    /// The model file is on this computer (from the installer or a download).
    installed: bool,
    /// The file is a download (so it can be removed again to free the space).
    downloaded: bool,
}

#[tauri::command]
pub fn bundled_models_status(app: tauri::AppHandle) -> Vec<BundledModelStatus> {
    BUNDLED_KEYS
        .iter()
        .filter_map(|key| {
            let id = ModelId::from_key(key)?;
            let installed = crate::resolve_model_path(&app, id).exists();
            let downloaded = downloaded_path(&app, id).is_some();
            Some(BundledModelStatus {
                key: key.to_string(),
                installed,
                downloaded,
            })
        })
        .collect()
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct DownloadProgress {
    key: String,
    /// "starting", "downloading", "verifying" or "done".
    phase: &'static str,
    downloaded: u64,
    total: u64,
}

fn emit(app: &tauri::AppHandle, key: &str, phase: &'static str, downloaded: u64, total: u64) {
    let _ = app.emit(
        "model-download-progress",
        DownloadProgress {
            key: key.to_string(),
            phase,
            downloaded,
            total,
        },
    );
}

fn http_agent() -> ureq::Agent {
    // No overall time limit: a model is hundreds of MB and a slow connection
    // is allowed to take its time. Only connecting and the first answer from
    // the server have limits, so a dead connection fails instead of hanging.
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(20)))
        .timeout_recv_response(Some(Duration::from_secs(60)))
        .build()
        .into()
}

fn explain(error: ureq::Error, what: &str) -> AppError {
    match error {
        ureq::Error::StatusCode(404) => AppError::Io(format!(
            "{what} isn't published for this version of Cutout yet. Check your connection and try again later, or use the Full installer."
        )),
        other => AppError::Io(format!("Couldn't download {what}: {other}")),
    }
}

/// Asks the release for the model's file name, size and SHA-256.
fn read_manifest_entry(tag: &str, key: &str) -> Result<(String, u64, String), AppError> {
    let url = format!("{RELEASE_DOWNLOAD_BASE}/{tag}/models-manifest.json");
    let mut response = http_agent()
        .get(&url)
        .header("User-Agent", "Cutout-model-download")
        .call()
        .map_err(|e| explain(e, "the model list"))?;
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|e| AppError::Io(format!("Couldn't read the model list: {e}")))?;
    let json: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| AppError::Io(format!("The model list wasn't understood: {e}")))?;

    let entry = json["models"]
        .as_array()
        .and_then(|list| list.iter().find(|m| m["key"].as_str() == Some(key)))
        .ok_or_else(|| AppError::Io("This model isn't in the release's model list.".into()))?;

    let asset = entry["asset"].as_str().map(str::to_string);
    let size = entry["size"].as_u64();
    let sha256 = entry["sha256"].as_str().map(|s| s.to_ascii_lowercase());
    match (asset, size, sha256) {
        (Some(asset), Some(size), Some(sha256))
            if !asset.contains('/') && !asset.contains('\\') && size > 0 && sha256.len() == 64 =>
        {
            Ok((asset, size, sha256))
        }
        _ => Err(AppError::Io("The release's model list is incomplete.".into())),
    }
}

fn download_blocking(app: &tauri::AppHandle, key: &str, model: ModelId) -> Result<(), AppError> {
    let tag = format!("v{}", env!("CARGO_PKG_VERSION"));
    emit(app, key, "starting", 0, 0);

    let (asset, expected_size, expected_sha) = read_manifest_entry(&tag, key)?;
    if CANCEL.load(Ordering::SeqCst) {
        return Err(AppError::Cancelled);
    }

    let dir = downloaded_dir(app)?;
    std::fs::create_dir_all(&dir).map_err(|e| AppError::Io(e.to_string()))?;
    let final_path = dir.join(model.filename());
    let part_path = dir.join(format!("{}.part", model.filename()));

    let url = format!("{RELEASE_DOWNLOAD_BASE}/{tag}/{asset}");
    let mut response = http_agent()
        .get(&url)
        .header("User-Agent", "Cutout-model-download")
        .call()
        .map_err(|e| explain(e, "the model"))?;
    let mut reader = response.body_mut().with_config().limit(u64::MAX).reader();

    // Everything below must clean up the .part file when it fails.
    let result = (|| -> Result<(), AppError> {
        let mut file = std::fs::File::create(&part_path)
            .map_err(|e| AppError::Io(format!("Couldn't write the model file: {e}")))?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; 256 * 1024];
        let mut downloaded: u64 = 0;
        let mut last_emit = Instant::now() - Duration::from_secs(1);

        loop {
            if CANCEL.load(Ordering::SeqCst) {
                return Err(AppError::Cancelled);
            }
            let read = reader
                .read(&mut buffer)
                .map_err(|e| AppError::Io(format!("The download was interrupted: {e}")))?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            file.write_all(&buffer[..read]).map_err(|e| {
                AppError::Io(format!("Couldn't write the model file (is the disk full?): {e}"))
            })?;
            downloaded += read as u64;
            if last_emit.elapsed() >= Duration::from_millis(120) {
                emit(app, key, "downloading", downloaded, expected_size);
                last_emit = Instant::now();
            }
        }
        file.flush().map_err(|e| AppError::Io(e.to_string()))?;
        drop(file);

        emit(app, key, "verifying", downloaded, expected_size);
        if downloaded != expected_size {
            return Err(AppError::Io(format!(
                "The download is incomplete ({downloaded} of {expected_size} bytes). Please try again."
            )));
        }
        let actual_sha = format!("{:x}", hasher.finalize());
        if actual_sha != expected_sha {
            return Err(AppError::Io(
                "The downloaded file is damaged (its checksum doesn't match). Please try again."
                    .into(),
            ));
        }
        Ok(())
    })();

    if let Err(e) = result {
        let _ = std::fs::remove_file(&part_path);
        return Err(e);
    }

    // A model already loaded from an older copy would keep using it.
    inference::forget_model(&final_path);
    let _ = std::fs::remove_file(&final_path);
    std::fs::rename(&part_path, &final_path).map_err(|e| {
        let _ = std::fs::remove_file(&part_path);
        AppError::Io(format!("Couldn't save the model file: {e}"))
    })?;

    emit(app, key, "done", expected_size, expected_size);
    Ok(())
}

/// Downloads one of the bundled models (key "balanced", "best" or "best-plus").
/// Progress arrives as `model-download-progress` events.
#[tauri::command]
pub async fn download_model(app: tauri::AppHandle, key: String) -> Result<(), AppError> {
    let model = ModelId::from_key(&key)
        .filter(|_| BUNDLED_KEYS.contains(&key.as_str()))
        .ok_or_else(|| AppError::Io("Unknown model.".into()))?;

    if BUSY.swap(true, Ordering::SeqCst) {
        return Err(AppError::Io("Another model is already downloading.".into()));
    }
    CANCEL.store(false, Ordering::SeqCst);

    tauri::async_runtime::spawn_blocking(move || {
        let _busy = BusyGuard;
        download_blocking(&app, &key, model)
    })
    .await
    .map_err(|e| {
        BUSY.store(false, Ordering::SeqCst);
        AppError::Io(format!("download task panicked: {e}"))
    })?
}

/// Stops a running model download.
#[tauri::command]
pub fn cancel_model_download() {
    CANCEL.store(true, Ordering::SeqCst);
}

/// Deletes a model that was downloaded, to free the disk space. Models that
/// came with the installer are not touched.
#[tauri::command]
pub fn delete_downloaded_model(app: tauri::AppHandle, key: String) -> Result<(), AppError> {
    let model = ModelId::from_key(&key)
        .filter(|_| BUNDLED_KEYS.contains(&key.as_str()))
        .ok_or_else(|| AppError::Io("Unknown model.".into()))?;
    if let Some(path) = downloaded_path(&app, model) {
        inference::forget_model(&path);
        std::fs::remove_file(&path).map_err(|e| AppError::Io(e.to_string()))?;
    }
    Ok(())
}
