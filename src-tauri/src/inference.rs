use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};

use image::RgbImage;
use ort::ep::{ExecutionProviderDispatch, CPU};
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

/// A loaded model, and what it runs on.
struct LoadedSession {
    session: Session,
    /// "CPU", or the name of the GPU technology this session uses.
    backend: &'static str,
}

/// One ONNX Runtime session per model file, created lazily on first use and
/// kept for the app's lifetime so switching back to a model doesn't pay the
/// session-build cost again.
static SESSIONS: OnceLock<Mutex<HashMap<PathBuf, LoadedSession>>> = OnceLock::new();

/// How the user wants the GPU used (Settings): automatic (the default: only
/// a dedicated graphics card, never integrated graphics), always, or never.
const MODE_AUTO: u8 = 0;
const MODE_ON: u8 = 1;
const MODE_OFF: u8 = 2;
static GPU_MODE: AtomicU8 = AtomicU8::new(MODE_AUTO);

/// Model files whose GPU session failed to build or to run. They stay on the
/// CPU until the GPU setting is changed.
static GPU_FAILED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

/// What the most recent cutout ran on, for the Settings status line.
static LAST_BACKEND: Mutex<Option<&'static str>> = Mutex::new(None);

fn load_err<E: std::fmt::Display>(e: E) -> AppError {
    AppError::ModelLoadFailed(e.to_string())
}

fn gpu_failed_set() -> &'static Mutex<HashSet<PathBuf>> {
    GPU_FAILED.get_or_init(|| Mutex::new(HashSet::new()))
}

fn mark_gpu_failed(model_path: &Path) {
    if let Ok(mut failed) = gpu_failed_set().lock() {
        failed.insert(model_path.to_path_buf());
    }
}

fn gpu_has_failed(model_path: &Path) -> bool {
    gpu_failed_set()
        .lock()
        .map(|failed| failed.contains(model_path))
        .unwrap_or(false)
}

/// What Cutout will do about the GPU right now, from the user's setting and
/// the graphics hardware found.
struct GpuPlan {
    /// Whether the next cutout should try the GPU.
    use_gpu: bool,
    /// Which graphics card to use (Windows only), when one was picked.
    device_index: Option<i32>,
    /// Name of the picked graphics card.
    card: Option<String>,
}

fn gpu_plan() -> GpuPlan {
    let none = GpuPlan {
        use_gpu: false,
        device_index: None,
        card: None,
    };
    if gpu_technology().is_none() {
        return none;
    }

    let best = crate::gpu_detect::best_dedicated();
    match GPU_MODE.load(Ordering::SeqCst) {
        MODE_OFF => none,
        // Always: use the best dedicated card, or the system default adapter
        // when there is no dedicated one.
        MODE_ON => GpuPlan {
            use_gpu: true,
            device_index: best.map(|a| a.index),
            card: best.map(|a| a.name.clone()),
        },
        // Automatic: only a dedicated card. Integrated graphics were slower
        // than the CPU in testing (80.9 s versus 22.1 s on a Ryzen's graphics).
        _ => match best {
            Some(card) => GpuPlan {
                use_gpu: true,
                device_index: Some(card.index),
                card: Some(card.name.clone()),
            },
            None => none,
        },
    }
}

/// The GPU technology Cutout uses on this operating system, if any.
pub fn gpu_technology() -> Option<&'static str> {
    if cfg!(target_os = "windows") {
        Some("DirectML")
    } else if cfg!(target_os = "macos") {
        Some("CoreML")
    } else if cfg!(target_os = "linux") {
        Some("WebGPU")
    } else {
        None
    }
}

// The GPU execution provider for this OS. `error_on_failure` makes a provider
// that can't start return an error, instead of ONNX Runtime quietly using the
// CPU, so Cutout knows what it is really running on.
#[cfg(target_os = "windows")]
fn gpu_provider(device_index: Option<i32>) -> Option<ExecutionProviderDispatch> {
    let mut provider = ort::ep::DirectML::default();
    // On a laptop with two GPUs, point DirectML at the dedicated one.
    if let Some(index) = device_index {
        provider = provider.with_device_id(index);
    }
    Some(provider.build().error_on_failure())
}

#[cfg(target_os = "macos")]
fn gpu_provider(_device_index: Option<i32>) -> Option<ExecutionProviderDispatch> {
    Some(ort::ep::CoreML::default().build().error_on_failure())
}

#[cfg(target_os = "linux")]
fn gpu_provider(_device_index: Option<i32>) -> Option<ExecutionProviderDispatch> {
    Some(ort::ep::WebGPU::default().build().error_on_failure())
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
fn gpu_provider(_device_index: Option<i32>) -> Option<ExecutionProviderDispatch> {
    None
}

/// Sets the GPU mode: "auto", "on" or "off" (anything else means auto).
/// Loaded models are dropped, so the next cutout builds them again on the
/// right device.
pub fn set_gpu_mode(mode: &str) {
    // Look for graphics hardware in the background, so Settings and the first
    // cutout don't wait for it.
    std::thread::spawn(|| {
        let _ = crate::gpu_detect::adapters();
    });

    let wanted = match mode {
        "on" => MODE_ON,
        "off" => MODE_OFF,
        _ => MODE_AUTO,
    };
    let previous = GPU_MODE.swap(wanted, Ordering::SeqCst);
    if previous == wanted {
        return;
    }
    if let Some(sessions) = SESSIONS.get() {
        if let Ok(mut sessions) = sessions.lock() {
            sessions.clear();
        }
    }
    if let Ok(mut failed) = gpu_failed_set().lock() {
        failed.clear();
    }
    if let Ok(mut last) = LAST_BACKEND.lock() {
        *last = None;
    }
}

/// What the Settings screen shows about GPU acceleration.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GpuStatus {
    /// The GPU technology for this OS, or None when there isn't one.
    pub technology: Option<&'static str>,
    /// "auto", "on" or "off".
    pub mode: &'static str,
    /// Whether the next cutout will try the GPU.
    pub using_gpu: bool,
    /// Name of the graphics card that will be used, when one was picked.
    pub card: Option<String>,
    /// Every graphics adapter found in this computer.
    pub adapters: Vec<crate::gpu_detect::GpuAdapter>,
    /// What the most recent cutout ran on ("CPU" or the GPU technology).
    pub last_backend: Option<&'static str>,
}

pub fn gpu_status() -> GpuStatus {
    let plan = gpu_plan();
    GpuStatus {
        technology: gpu_technology(),
        mode: match GPU_MODE.load(Ordering::SeqCst) {
            MODE_ON => "on",
            MODE_OFF => "off",
            _ => "auto",
        },
        using_gpu: plan.use_gpu,
        card: plan.card,
        adapters: crate::gpu_detect::adapters().to_vec(),
        last_backend: LAST_BACKEND.lock().ok().and_then(|last| *last),
    }
}

fn backend_of(model_path: &Path) -> Option<&'static str> {
    SESSIONS
        .get()?
        .lock()
        .ok()?
        .get(model_path)
        .map(|loaded| loaded.backend)
}

/// Builds one session. With `gpu` set, the GPU provider comes first and the
/// CPU is listed after it for any part of the model the GPU can't run.
fn try_build_session(
    model_path: &Path,
    gpu: Option<ExecutionProviderDispatch>,
) -> Result<Session, AppError> {
    let intra_threads = inference_thread_count();
    let on_gpu = gpu.is_some();

    // CPU memory arena disabled: on low-core-count machines the arena was
    // directly responsible for multi-hundred-second stalls under memory
    // pressure. Applies to every model uniformly.
    let mut builder = Session::builder().map_err(load_err)?;
    builder = match gpu {
        Some(provider) => builder
            .with_execution_providers([provider, CPU::default().with_arena_allocator(false).build()]),
        None => builder.with_execution_providers([CPU::default().with_arena_allocator(false).build()]),
    }
    .map_err(load_err)?;

    builder = builder
        .with_optimization_level(GraphOptimizationLevel::Level3)
        .map_err(load_err)?
        // Always leave at least one thread free for Windows/the UI.
        .with_intra_threads(intra_threads)
        .map_err(load_err)?
        .with_inter_threads(1)
        .map_err(load_err)?
        .with_intra_op_spinning(false)
        .map_err(load_err)?;

    if on_gpu {
        // DirectML requires sequential execution and no memory-pattern
        // planning; the other GPU providers don't mind.
        builder = builder
            .with_memory_pattern(false)
            .map_err(load_err)?
            .with_parallel_execution(false)
            .map_err(load_err)?;
    }

    builder.commit_from_file(model_path).map_err(load_err)
}

/// Builds the session for a model: on the GPU when the GPU plan says so and
/// it works, otherwise on the CPU. A GPU that fails to start is never fatal.
fn create_session(model_path: &Path) -> Result<(Session, &'static str), AppError> {
    let plan = gpu_plan();
    if plan.use_gpu && !gpu_has_failed(model_path) {
        if let (Some(provider), Some(technology)) = (gpu_provider(plan.device_index), gpu_technology()) {
            let card = plan.card.as_deref().unwrap_or("the default graphics");
            match try_build_session(model_path, Some(provider)) {
                Ok(session) => {
                    crate::memstat::log_line(&format!(
                        "[gpu] {} runs on {technology} ({card})",
                        model_path.display()
                    ));
                    return Ok((session, technology));
                }
                Err(e) => {
                    crate::memstat::log_line(&format!(
                        "[gpu] {technology} could not start for {}, using the CPU instead: {e}",
                        model_path.display()
                    ));
                    mark_gpu_failed(model_path);
                }
            }
        }
    }

    let session = try_build_session(model_path, None)?;
    Ok((session, "CPU"))
}

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

        let (session, backend) = create_session(model_path)?;
        sessions.insert(model_path.to_path_buf(), LoadedSession { session, backend });
    }

    let loaded = sessions
        .get_mut(model_path)
        .expect("session present: inserted above or already existed");

    if let Ok(mut last) = LAST_BACKEND.lock() {
        *last = Some(loaded.backend);
    }

    f(&mut loaded.session)
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

/// Everything needed to run one model: where its file is and how images
/// must be prepared for it. Built for a bundled model or for one the user
/// imported.
#[derive(Debug, Clone)]
pub struct ModelSpec {
    /// Name used in the performance log.
    pub label: String,
    pub path: PathBuf,
    pub input_size: u32,
    mean: [f32; 3],
    std: [f32; 3],
    minmax: bool,
}

impl ModelSpec {
    pub fn bundled(model: ModelId, path: PathBuf) -> Self {
        let p = model.profile();
        Self {
            label: format!("{model:?}"),
            path,
            input_size: p.input_size,
            mean: p.mean,
            std: p.std,
            minmax: p.output_needs_minmax_normalize,
        }
    }

    /// A model the user imported. `kind` picks the image preparation:
    /// "u2net" (ImageNet stats, raw output), "isnet" (0.5 mean) or anything
    /// else for the BiRefNet style (ImageNet stats, probability output).
    pub fn custom(label: String, path: PathBuf, kind: &str, input_size: u32) -> Self {
        let imagenet_mean = [0.485, 0.456, 0.406];
        let imagenet_std = [0.229, 0.224, 0.225];
        let (mean, std, minmax) = match kind {
            "u2net" => (imagenet_mean, imagenet_std, true),
            "isnet" => ([0.5, 0.5, 0.5], [1.0, 1.0, 1.0], false),
            _ => (imagenet_mean, imagenet_std, false),
        };
        Self {
            label,
            path,
            input_size,
            mean,
            std,
            minmax,
        }
    }
}

/// Drops the loaded session for a model file, so the file can be deleted and
/// its memory is freed.
pub fn forget_model(model_path: &Path) {
    if let Some(sessions) = SESSIONS.get() {
        if let Ok(mut sessions) = sessions.lock() {
            sessions.remove(model_path);
        }
    }
}

/// Run inference on a pre-resized RGB image and return a row-major
/// single-channel mask (0.0-1.0) at the model's input resolution. The caller
/// resizes it back to the source image's dimensions.
///
/// If the model was running on the GPU and the run fails, it is retried once
/// on the CPU, and that model stays on the CPU from then on. GPU runs can fail
/// in ways a CPU run can't (out of video memory, an operator the GPU doesn't
/// support, a lost device), and that must not stop a cutout from working.
pub fn run_inference(spec: &ModelSpec, resized_input: &RgbImage) -> Result<Vec<f32>, AppError> {
    match run_on_session(spec, resized_input) {
        Ok(mask) => Ok(mask),
        Err(error) => {
            let on_gpu = backend_of(&spec.path).map(|b| b != "CPU").unwrap_or(false);
            if !on_gpu {
                return Err(error);
            }
            crate::memstat::log_line(&format!(
                "[gpu] the run failed on the GPU, retrying on the CPU: {error}"
            ));
            mark_gpu_failed(&spec.path);
            forget_model(&spec.path);
            run_on_session(spec, resized_input)
        }
    }
}

fn run_on_session(spec: &ModelSpec, resized_input: &RgbImage) -> Result<Vec<f32>, AppError> {
    let profile = ModelProfile {
        input_size: spec.input_size,
        mean: spec.mean,
        std: spec.std,
        output_needs_minmax_normalize: spec.minmax,
    };

    with_session(&spec.path, |session| {
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
