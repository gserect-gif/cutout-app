use std::path::Path;

use image::{DynamicImage, GenericImageView, RgbImage, RgbaImage};

use crate::AppError;

/// The model's fixed square input resolution. BiRefNet-family models are
/// trained at 1024x1024; we resize (with aspect-preserving letterboxing is
/// unnecessary since BiRefNet was trained on direct/stretched resizing) the
/// full-resolution source down for inference only, then upscale the
/// predicted mask back to the original dimensions before compositing.
pub const MODEL_INPUT_SIZE: u32 = 1024;

pub struct LoadedImage {
    pub image: DynamicImage,
    pub width: u32,
    pub height: u32,
}

/// Load an image from disk, validating the extension and decoding it.
/// Returns the original, full-resolution `DynamicImage` untouched — no
/// resizing happens here. Resizing for inference happens later, separately,
/// so the original pixels are always preserved for compositing/export.
pub fn load_image(path: &str) -> Result<LoadedImage, AppError> {
    let p = Path::new(path);

    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default();

    if !matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "webp") {
        return Err(AppError::UnsupportedFormat(ext));
    }

    if !p.exists() {
        return Err(AppError::FileNotFound(path.to_string()));
    }

    let bytes = std::fs::read(p).map_err(|e| AppError::Io(e.to_string()))?;

    let image = image::load_from_memory(&bytes)
        .map_err(|e| AppError::DecodeFailed(format!("{ext} image: {e}")))?;

    let (width, height) = image.dimensions();

    if width == 0 || height == 0 {
        return Err(AppError::DecodeFailed("image has zero dimensions".into()));
    }

    Ok(LoadedImage { image, width, height })
}

/// Downscale (or upscale) the source image to a square MODEL_INPUT_SIZE x
/// MODEL_INPUT_SIZE RGB buffer for model input, using a high-quality Lanczos3
/// filter. The source image itself is untouched; this returns a new buffer.
pub fn prepare_model_input(img: &DynamicImage) -> RgbImage {
    img.resize_exact(
        MODEL_INPUT_SIZE,
        MODEL_INPUT_SIZE,
        image::imageops::FilterType::Lanczos3,
    )
    .to_rgb8()
}

/// Resize a single-channel mask (values 0.0-1.0, row-major, mask_size x
/// mask_size) up/down to the original image's exact dimensions using
/// bilinear interpolation, which is appropriate for smooth alpha edges.
pub fn resize_mask_to(
    mask: &[f32],
    mask_size: u32,
    target_width: u32,
    target_height: u32,
) -> Vec<f32> {
    if mask_size == target_width && mask_size == target_height {
        return mask.to_vec();
    }

    let mut out = vec![0f32; (target_width * target_height) as usize];
    let scale_x = mask_size as f32 / target_width as f32;
    let scale_y = mask_size as f32 / target_height as f32;

    for y in 0..target_height {
        let src_y = ((y as f32 + 0.5) * scale_y - 0.5).clamp(0.0, (mask_size - 1) as f32);
        let y0 = src_y.floor() as u32;
        let y1 = (y0 + 1).min(mask_size - 1);
        let fy = src_y - y0 as f32;

        for x in 0..target_width {
            let src_x = ((x as f32 + 0.5) * scale_x - 0.5).clamp(0.0, (mask_size - 1) as f32);
            let x0 = src_x.floor() as u32;
            let x1 = (x0 + 1).min(mask_size - 1);
            let fx = src_x - x0 as f32;

            let v00 = mask[(y0 * mask_size + x0) as usize];
            let v10 = mask[(y0 * mask_size + x1) as usize];
            let v01 = mask[(y1 * mask_size + x0) as usize];
            let v11 = mask[(y1 * mask_size + x1) as usize];

            let top = v00 * (1.0 - fx) + v10 * fx;
            let bottom = v01 * (1.0 - fx) + v11 * fx;
            let value = top * (1.0 - fy) + bottom * fy;

            out[(y * target_width + x) as usize] = value;
        }
    }

    out
}

/// Apply a per-pixel alpha mask (0.0-1.0, row-major, same dimensions as
/// `base`) to the original full-resolution image, producing an RGBA image
/// with real transparency. `base` must be exactly `width` x `height`.
///
/// Raw model masks are soft (values gradually fade from 0 to 1 across an
/// edge). Left as-is, low but non-zero alpha values near edges let a sliver
/// of the original background color show through as faint "crumbs" once
/// composited over a new background. To counter that, values are pushed
/// toward the extremes with a smoothstep-like curve, sharpening the cutout
/// edge while keeping a thin anti-aliased transition instead of a hard,
/// jagged one.
pub fn apply_alpha_mask(base: &DynamicImage, mask: &[f32], width: u32, height: u32) -> RgbaImage {
    let rgb = base.to_rgba8();
    let mut out = RgbaImage::new(width, height);

    // Values below LOW snap to fully transparent; above HIGH snap to fully
    // opaque. Between them, alpha is remapped with a smoothstep curve so the
    // transition band that remains is a clean, narrow gradient rather than a
    // wide muddy fade.
    const LOW: f32 = 0.06;
    const HIGH: f32 = 0.94;

    for y in 0..height {
        for x in 0..width {
            let idx = (y * width + x) as usize;
            let raw = mask.get(idx).copied().unwrap_or(0.0);

            let shaped = if raw <= LOW {
                0.0
            } else if raw >= HIGH {
                1.0
            } else {
                let t = (raw - LOW) / (HIGH - LOW);
                t * t * (3.0 - 2.0 * t) // smoothstep
            };

            let alpha = (shaped * 255.0).round().clamp(0.0, 255.0) as u8;

            let px = rgb.get_pixel(x, y);
            out.put_pixel(x, y, image::Rgba([px[0], px[1], px[2], alpha]));
        }
    }

    out
}
