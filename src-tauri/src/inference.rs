use std::path::Path;
use std::sync::OnceLock;

use image::RgbImage;
use ort::ep::CPU;
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::Tensor;

use crate::image_ops::MODEL_INPUT_SIZE;
use crate::AppError;

// ImageNet normalization constants used by BiRefNet's preprocessing.
const MEAN: [f32; 3] = [0.485, 0.456, 0.406];
const STD: [f32; 3] = [0.229, 0.224, 0.225];

/// The ONNX Runtime session is expensive to construct (loads + optimizes a
/// 200+MB graph), so it is built once on first use and reused for every
/// subsequent image. Wrapped in a Mutex because `Session::run` takes `&mut
/// self` in `ort` 2.x.
static SESSION: OnceLock<std::sync::Mutex<Session>> = OnceLock::new();

fn get_session(model_path: &Path) -> Result<&'static std::sync::Mutex<Session>, AppError> {
    if let Some(s) = SESSION.get() {
        return Ok(s);
    }

    if !model_path.exists() {
        return Err(AppError::ModelMissing(model_path.display().to_string()));
    }

    let intra_threads = inference_thread_count();

    let session = Session::builder()
        .map_err(|e| AppError::ModelLoadFailed(e.to_string()))?
        // Explicitly configure the CPU execution provider with its memory
        // arena DISABLED. By default, ONNX Runtime's CPU provider uses an
        // arena allocator: it requests large chunks of memory from the OS
        // and sub-allocates from them for speed, but does not return those
        // chunks to the OS between inference calls. When a call needs a
        // differently-shaped internal buffer than what's already cached in
        // the arena, it can request an entirely new large chunk on top of
        // what it already holds, growing peak memory in a way that doesn't
        // correlate with the current image's size. On a machine already
        // close to its RAM ceiling, that OS-level allocation can itself
        // become very slow (competing for physical pages, potentially
        // triggering page-file thrashing) — a plausible, code-grounded
        // explanation for a sudden multi-hundred-second stall on an
        // otherwise small image. Disabling the arena trades a small,
        // measurable amount of steady-state speed (ONNX Runtime allocates
        // more directly instead of from a pre-grown pool) for allocator
        // behavior that's flat and predictable across varying inputs,
        // instead of occasionally spiking.
        .with_execution_providers([CPU::default()
            .with_arena_allocator(false)
            .build()])
        .map_err(|e| AppError::ModelLoadFailed(e.to_string()))?
        .with_optimization_level(GraphOptimizationLevel::Level3)
        .map_err(|e| AppError::ModelLoadFailed(e.to_string()))?
        // Intra-op threads: how many threads ONNX Runtime uses to parallelize
        // math *within* a single op (e.g. one big matrix multiply). Capped
        // below the machine's total thread count (see `inference_thread_count`)
        // so at least one thread stays free for Windows/the UI/everything
        // else — using every available thread here is what pegs a low-core
        // CPU (e.g. a 4-thread Ryzen 3) at 100% and makes the whole system
        // stop responding to input during inference.
        .with_intra_threads(intra_threads)
        .map_err(|e| AppError::ModelLoadFailed(e.to_string()))?
        // Inter-op threads: how many *additional* threads ONNX Runtime uses
        // to run independent parts of the graph in parallel. Left at its
        // default, this adds threads on top of the intra-op ones above,
        // oversubscribing a low-core CPU even further. BiRefNet's graph
        // doesn't have meaningfully parallel independent branches to exploit
        // here, so this is pinned to 1 (i.e. effectively disabled) rather
        // than left to ONNX Runtime's own default.
        .with_inter_threads(1)
        .map_err(|e| AppError::ModelLoadFailed(e.to_string()))?
        // Idle inference threads busy-wait (spin) by default, burning CPU
        // cycles instead of yielding, in case more work arrives momentarily.
        // That's a reasonable trade on a server with cores to spare; on a
        // fully-loaded low-core desktop CPU it's extra contention against
        // the UI thread. Disabling spinning tells idle threads to actually
        // sleep/yield instead of busy-polling.
        .with_intra_op_spinning(false)
        .map_err(|e| AppError::ModelLoadFailed(e.to_string()))?
        .commit_from_file(model_path)
        .map_err(|e| AppError::ModelLoadFailed(e.to_string()))?;

    let _ = SESSION.set(std::sync::Mutex::new(session));
    Ok(SESSION.get().expect("session was just set"))
}

/// Picks how many threads ONNX Runtime is allowed to use for inference math.
/// Leaves at least one logical thread free for the OS, the UI thread, and
/// everything else running on the machine, so a background-removal run
/// doesn't starve the rest of the system — this is the direct fix for
/// inference freezing the whole desktop on low-core-count CPUs (e.g. a
/// 4-thread Ryzen 3 3200G, where using all 4 threads for inference leaves
/// nothing for Windows to schedule anything else with).
fn inference_thread_count() -> usize {
    let total = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);

    // Leave 1 thread free on small machines; leave 2 free on larger ones so
    // there's still comfortable headroom for the rest of the system. Always
    // use at least 1 thread for inference itself.
    let reserved = if total <= 4 { 1 } else { 2 };
    total.saturating_sub(reserved).max(1)
}

/// Convert a resized RGB image into a flat NCHW float32 buffer (plus its
/// shape) that BiRefNet expects: pixel values scaled to [0,1], then
/// normalized per-channel with ImageNet mean/std. Returned as
/// `(shape, data)` so it can be handed directly to `Tensor::from_array`
/// without depending on `ndarray` ourselves (avoids pulling in a second,
/// possibly-mismatched `ndarray` version alongside the one `ort` uses
/// internally).
fn to_input_tensor(img: &RgbImage) -> ([usize; 4], Vec<f32>) {
    let (w, h) = (img.width() as usize, img.height() as usize);
    let mut data = vec![0f32; 3 * h * w];

    // NCHW layout: channel-major, then row, then column.
    for y in 0..h {
        for x in 0..w {
            let px = img.get_pixel(x as u32, y as u32);
            for c in 0..3 {
                let normalized = (px[c] as f32 / 255.0 - MEAN[c]) / STD[c];
                data[c * h * w + y * w + x] = normalized;
            }
        }
    }

    ([1, 3, h, w], data)
}

/// Run BiRefNet inference on a pre-resized (MODEL_INPUT_SIZE x
/// MODEL_INPUT_SIZE) RGB image and return a row-major, single-channel mask
/// (values 0.0-1.0, same MODEL_INPUT_SIZE x MODEL_INPUT_SIZE resolution) that
/// still needs to be resized back to the original image's dimensions before
/// use as an alpha channel.
pub fn run_inference(model_path: &Path, resized_input: &RgbImage) -> Result<Vec<f32>, AppError> {
    let session_lock = get_session(model_path)?;
    let mut session = session_lock
        .lock()
        .map_err(|_| AppError::InferenceFailed("model session lock poisoned".into()))?;

    let (shape, data) = to_input_tensor(resized_input);
    let input_tensor = Tensor::from_array((shape, data))
        .map_err(|e| AppError::InferenceFailed(format!("failed to build input tensor: {e}")))?;

    // Read both input and output names up front, as owned Strings, before
    // calling `run()`. `run()` needs a mutable borrow of `session`, so no
    // borrow from `.inputs()`/`.outputs()` (which borrow immutably) can
    // still be alive when we call it.
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

    // Expected output shape: [1, 1, H, W] (a single-channel logit/probability
    // map). BiRefNet's exported ONNX graph already applies sigmoid, so values
    // should already be in [0,1] — but we apply sigmoid defensively in case a
    // raw-logit variant of the model is swapped in later.
    let expected_len = (MODEL_INPUT_SIZE * MODEL_INPUT_SIZE) as usize;
    if data.len() != expected_len {
        return Err(AppError::InferenceFailed(format!(
            "unexpected output size: got {} values (shape {:?}), expected {}",
            data.len(),
            shape,
            expected_len
        )));
    }

    let looks_pre_sigmoid = data.iter().any(|v| *v < -0.001 || *v > 1.001);

    let mask: Vec<f32> = if looks_pre_sigmoid {
        data.iter().map(|v| sigmoid(*v)).collect()
    } else {
        data.to_vec()
    };

    Ok(mask)
}

fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}
