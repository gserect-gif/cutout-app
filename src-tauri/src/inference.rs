use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use image::RgbImage;
use ort::ep::CPU;
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::Tensor;

use crate::AppError;

/// Identifies which bundled model to run. Each model carries its own
/// preprocessing/postprocessing profile (see `profile()`), because these
/// models genuinely differ in input resolution and normalization — running
/// the wrong pipeline on a model doesn't error, it silently produces a bad
/// mask.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModelId {
    /// U2Netp ("Fast"): 320x320 input, ImageNet normalization, raw saliency
    /// output that must be min-max normalized.
    U2NetP,
    /// IS-Net general-use ("Balanced"): 1024x1024 input, (x/255 - 0.5)
    /// normalization (NOT ImageNet stats).
    IsNetGeneral,
    /// BiRefNet_lite ("Best", the original default): 1024x1024, ImageNet
    /// normalization, sigmoid output.
    BiRefNetLite,
    /// Full BiRefNet fp16 ("Best+"): same pipeline as lite, larger graph.
    BiRefNetFull,
}

impl ModelId {
    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "fast" => Some(Self::U2NetP),
            "balanced" => Some(Self::IsNetGeneral),
            "best" => Some(Self::BiRefNetLite),
            "best-plus" => Some(Self::BiRefNetFull),
            _ => None,
        }
    }

    /// Filename expected inside the app's `models/` directory.
    pub fn filename(&self) -> &'static str {
        match self {
            Self::U2NetP => "u2netp.onnx",
            Self::IsNetGeneral => "isnet-general-use.onnx",
            Self::BiRefNetLite => "model.onnx",
            Self::BiRefNetFull => "model_fp16.onnx",
        }
    }

    fn profile(&self) -> ModelProfile {
        match self {
            Self::U2NetP => ModelProfile {
                input_size: 320,
                mean: [0.485, 0.456, 0.406],
                std: [0.229, 0.224, 0.225],
                output_needs_minmax_normalize: true,
            },
            Self::IsNetGeneral => ModelProfile {
                input_size: 1024,
                mean: [0.5, 0.5, 0.5],
                std: [1.0, 1.0, 1.0],
                output_needs_minmax_normalize: false,
            },
            Self::BiRefNetLite | Self::BiRefNetFull => ModelProfile {
                input_size: 1024,
                mean: [0.485, 0.456, 0.406],
                std: [0.229, 0.224, 0.225],
                output_needs_minmax_normalize: false,
            },
        }
    }
}

struct ModelProfile {
    input_size: u32,
    mean: [f32; 3],
    std: [f32; 3],
    /// U2Net-family models output a raw (unbounded) saliency map rather than
    /// a sigmoid probability, so it must be min-max normalized to 0..1.
    output_needs_minmax_normalize: bool,
}

/// One ONNX Runtime session per model file, created lazily on first use and
/// kept for the app's lifetime so switching back to a model doesn't pay the
/// session-build cost again.
static SESSIONS: OnceLock<Mutex<HashMap<PathBuf, Session>>> = OnceLock::new();

fn with_session<T>(
    model_path: &Path,
    f: impl FnOnce(&mut Session) -> Result<T, AppError>,
) -> Result<T, AppError> {
    let sessions = SESSIONS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut sessions = sessions
        .lock()
        .map_err(|_| AppError::InferenceFailed("model session map lock poisoned".into()))?;

    if !sessions.contains_key(model_path) {
        if !model_path.exists() {
            return Err(AppError::ModelMissing(model_path.display().to_string()));
        }

        let intra_threads = inference_thread_count();

        let session = Session::builder()
            .map_err(|e| AppError::ModelLoadFailed(e.to_string()))?
            // CPU memory arena disabled: on low-core-count machines the
            // arena was directly responsible for multi-hundred-second stalls
            // under memory pressure. Applies to every model uniformly.
            .with_execution_providers([CPU::default().with_arena_allocator(false).build()])
            .map_err(|e| AppError::ModelLoadFailed(e.to_string()))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| AppError::ModelLoadFailed(e.to_string()))?
            // Always leave at least one thread free for Windows/the UI.
            .with_intra_threads(intra_threads)
            .map_err(|e| AppError::ModelLoadFailed(e.to_string()))?
            .with_inter_threads(1)
            .map_err(|e| AppError::ModelLoadFailed(e.to_string()))?
            .with_intra_op_spinning(false)
            .map_err(|e| AppError::ModelLoadFailed(e.to_string()))?
            .commit_from_file(model_path)
            .map_err(|e| AppError::ModelLoadFailed(e.to_string()))?;

        sessions.insert(model_path.to_path_buf(), session);
    }

    let session = sessions
        .get_mut(model_path)
        .expect("session present: inserted above or already existed");

    f(session)
}

/// How many threads ONNX Runtime may use. Leaves at least one logical thread
/// free for the OS/UI so inference can't starve the rest of the desktop.
fn inference_thread_count() -> usize {
    let total = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);

    let reserved = if total <= 4 { 1 } else { 2 };
    total.saturating_sub(reserved).max(1)
}

/// Convert a resized RGB image into a flat NCHW float32 buffer (plus shape),
/// normalized with the given profile's per-channel mean/std.
fn to_input_tensor(img: &RgbImage, profile: &ModelProfile) -> ([usize; 4], Vec<f32>) {
    let (w, h) = (img.width() as usize, img.height() as usize);
    let mut data = vec![0f32; 3 * h * w];

    for y in 0..h {
        for x in 0..w {
            let px = img.get_pixel(x as u32, y as u32);
            for c in 0..3 {
                let normalized = (px[c] as f32 / 255.0 - profile.mean[c]) / profile.std[c];
                data[c * h * w + y * w + x] = normalized;
            }
        }
    }

    ([1, 3, h, w], data)
}

/// The square input resolution the given model expects. The caller resizes
/// the source image to this size before calling `run_inference`.
pub fn input_size_for(model: ModelId) -> u32 {
    model.profile().input_size
}

/// Run inference on a pre-resized RGB image and return a row-major
/// single-channel mask (0.0-1.0) at the model's input resolution. The caller
/// resizes it back to the source image's dimensions.
pub fn run_inference(
    model: ModelId,
    model_path: &Path,
    resized_input: &RgbImage,
) -> Result<Vec<f32>, AppError> {
    let profile = model.profile();

    with_session(model_path, |session| {
        let (shape, data) = to_input_tensor(resized_input, &profile);
        let input_tensor = Tensor::from_array((shape, data))
            .map_err(|e| AppError::InferenceFailed(format!("failed to build input tensor: {e}")))?;

        // Read names up front as owned Strings so no immutable borrow of
        // `session` is alive when `run()` takes its mutable borrow.
        let input_name: String = session
            .inputs()
            .first()
            .map(|i| i.name().to_string())
            .ok_or_else(|| AppError::InferenceFailed("model has no declared inputs".into()))?;

        let output_name: String = session
            .outputs()
            .first()
            .map(|o| o.name().to_string())
            .ok_or_else(|| AppError::InferenceFailed("model has no declared outputs".into()))?;

        let outputs = session
            .run(ort::inputs![input_name => input_tensor])
            .map_err(|e| AppError::InferenceFailed(format!("inference run failed: {e}")))?;

        let output = outputs
            .get(output_name.as_str())
            .ok_or_else(|| AppError::InferenceFailed("output tensor missing from result".into()))?;

        let (shape, data) = output
            .try_extract_tensor::<f32>()
            .map_err(|e| AppError::InferenceFailed(format!("failed to extract output tensor: {e}")))?;

        let expected_len = (profile.input_size * profile.input_size) as usize;
        if data.len() != expected_len {
            return Err(AppError::InferenceFailed(format!(
                "unexpected output size: got {} values (shape {:?}), expected {}",
                data.len(),
                shape,
                expected_len
            )));
        }

        let mask: Vec<f32> = if profile.output_needs_minmax_normalize {
            let min = data.iter().copied().fold(f32::INFINITY, f32::min);
            let max = data.iter().copied().fold(f32::NEG_INFINITY, f32::max);
            let range = (max - min).max(1e-6);
            data.iter().map(|v| (v - min) / range).collect()
        } else {
            // BiRefNet family and IS-Net output bounded [0,1] probabilities.
            // Defensively apply sigmoid only if values are clearly outside
            // that range (e.g. a raw-logit export).
            let looks_pre_sigmoid = data.iter().any(|v| *v < -0.001 || *v > 1.001);
            if looks_pre_sigmoid {
                data.iter().map(|v| sigmoid(*v)).collect()
            } else {
                data.to_vec()
            }
        };

        Ok(mask)
    })
}

fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}
