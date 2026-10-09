use std::path::Path;

use image::{DynamicImage, ExtendedColorType, GenericImageView, ImageEncoder, RgbImage, RgbaImage};

use crate::AppError;

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

    // Kept in sync with the `SUPPORTED_EXTENSIONS` list in lib.rs (used for
    // folder scans and frontend file-picker filters) — both lists must
    // agree, or a file could pass one check and fail the other.
    if !matches!(
        ext.as_str(),
        "png" | "jpg" | "jpeg" | "webp" | "bmp" | "tiff" | "tif" | "gif"
    ) {
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

/// Resize the source image to a square `size` x `size` RGB buffer for model
/// input, using a high-quality Lanczos3 filter. The source image itself is
/// untouched; this returns a new buffer. `size` is per-model (see
/// `inference::input_size_for`) since the bundled models expect different
/// fixed input resolutions.
pub fn prepare_model_input(img: &DynamicImage, size: u32) -> RgbImage {
    img.resize_exact(size, size, image::imageops::FilterType::Lanczos3)
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

// ---------- Export (format + optional solid background) ----------

/// Output formats Cutout can export. PNG and WebP keep transparency; JPEG
/// never can, so it always needs a solid background behind the cutout.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExportFormat {
    Png,
    Jpg,
    Webp,
}

impl ExportFormat {
    /// Unknown or missing keys fall back to PNG, the original behavior.
    pub fn from_key(key: Option<&str>) -> Self {
        match key.map(|k| k.to_ascii_lowercase()).as_deref() {
            Some("jpg") | Some("jpeg") => ExportFormat::Jpg,
            Some("webp") => ExportFormat::Webp,
            _ => ExportFormat::Png,
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Png => "png",
            ExportFormat::Jpg => "jpg",
            ExportFormat::Webp => "webp",
        }
    }
}

/// Parses "#rrggbb" (the leading # is optional) into RGB bytes.
pub fn parse_hex_color(s: &str) -> Option<[u8; 3]> {
    let hex = s.trim().trim_start_matches('#');
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some([r, g, b])
}

/// Composites an RGBA cutout over a solid color, giving an opaque RGB image.
fn flatten_over(rgba: &RgbaImage, bg: [u8; 3]) -> RgbImage {
    let (w, h) = rgba.dimensions();
    let mut out = RgbImage::new(w, h);
    for (x, y, px) in rgba.enumerate_pixels() {
        let a = px[3] as u32;
        let inv = 255 - a;
        let blend = |f: u8, b: u8| ((f as u32 * a + b as u32 * inv + 127) / 255) as u8;
        out.put_pixel(
            x,
            y,
            image::Rgb([blend(px[0], bg[0]), blend(px[1], bg[1]), blend(px[2], bg[2])]),
        );
    }
    out
}

/// Composites an RGBA cutout over a picture, giving an opaque RGB image.
/// The picture is scaled to cover the whole cutout (cropped from the centre
/// if the proportions differ), like CSS `background-size: cover`.
fn flatten_over_image(rgba: &RgbaImage, bg: &DynamicImage) -> RgbImage {
    let (w, h) = rgba.dimensions();
    let backdrop = bg
        .resize_to_fill(w, h, image::imageops::FilterType::Triangle)
        .to_rgb8();
    let mut out = RgbImage::new(w, h);
    for (x, y, px) in rgba.enumerate_pixels() {
        let a = px[3] as u32;
        let inv = 255 - a;
        let b = backdrop.get_pixel(x, y);
        let blend = |f: u8, b: u8| ((f as u32 * a + b as u32 * inv + 127) / 255) as u8;
        out.put_pixel(
            x,
            y,
            image::Rgb([blend(px[0], b[0]), blend(px[1], b[1]), blend(px[2], b[2])]),
        );
    }
    out
}

// ---------- Edge refinement ----------

/// One pass of a box blur along a line of `len` samples starting at `start`
/// and stepping by `stride`, with edge samples repeated at the borders.
fn blur_line(src: &[u8], dst: &mut [u8], start: usize, stride: usize, len: usize, r: usize) {
    let window = (2 * r + 1) as u32;
    let last = len as isize - 1;
    let at = |i: isize| -> u32 { src[start + i.clamp(0, last) as usize * stride] as u32 };

    let mut sum: u32 = 0;
    for i in -(r as isize)..=(r as isize) {
        sum += at(i);
    }
    for i in 0..len {
        dst[start + i * stride] = ((sum + window / 2) / window) as u8;
        sum = sum + at(i as isize + r as isize + 1) - at(i as isize - r as isize);
    }
}

/// Blurs an 8-bit single-channel buffer (`w` x `h`) horizontally then
/// vertically with a box of radius `r`.
fn box_blur(buf: &mut [u8], w: usize, h: usize, r: usize) {
    let mut tmp = vec![0u8; buf.len()];
    for y in 0..h {
        blur_line(buf, &mut tmp, y * w, 1, w, r);
    }
    for x in 0..w {
        blur_line(&tmp, buf, x, w, h, r);
    }
}

/// Changes how hard or soft the cutout's edge is, by reshaping the alpha
/// channel only (the colors are untouched).
///
/// `edge` runs from -100 to 100. 0 leaves the cutout as it is. Negative
/// values sharpen the edge by steepening the alpha curve around 50%. Positive
/// values soften it with a blur whose radius scales with the image size, so
/// the same slider position looks alike on a phone photo and a 24MP one.
pub fn refine_edges(img: &mut RgbaImage, edge: i32) {
    let edge = edge.clamp(-100, 100);
    if edge == 0 {
        return;
    }

    let (w, h) = img.dimensions();
    let mut alpha: Vec<u8> = img.pixels().map(|p| p[3]).collect();

    if edge < 0 {
        let t = (-edge) as f32 / 100.0;
        let half = 0.5 * (1.0 - 0.9 * t);
        let (lo, hi) = (0.5 - half, 0.5 + half);

        let mut lut = [0u8; 256];
        for (i, slot) in lut.iter_mut().enumerate() {
            let a = i as f32 / 255.0;
            let x = ((a - lo) / (hi - lo)).clamp(0.0, 1.0);
            let steep = x * x * (3.0 - 2.0 * x);
            *slot = ((a * (1.0 - t) + steep * t) * 255.0).round().clamp(0.0, 255.0) as u8;
        }
        for a in alpha.iter_mut() {
            *a = lut[*a as usize];
        }
    } else {
        let longest = w.max(h) as f32;
        let radius = ((edge as f32 / 100.0) * longest * 0.004).round().max(1.0) as usize;
        // Two box blurs in a row approximate a smooth gaussian falloff.
        box_blur(&mut alpha, w as usize, h as usize, radius);
        box_blur(&mut alpha, w as usize, h as usize, radius);
    }

    for (px, a) in img.pixels_mut().zip(alpha) {
        px[3] = a;
    }
}

/// Everything that shapes an exported file besides the cutout itself.
pub struct ExportRender<'a> {
    pub format: ExportFormat,
    /// Solid color behind the cutout. Ignored when `image` is set.
    pub color: Option<[u8; 3]>,
    /// Picture behind the cutout, already decoded.
    pub image: Option<&'a DynamicImage>,
    /// Edge hardness, -100 (sharper) to 100 (softer). 0 = unchanged.
    pub edge: i32,
}

/// Writes the cutout PNG at `result_path` to `dest`, applying the edge
/// setting and an optional solid color or picture behind it.
///
/// A plain transparent PNG with no edge change is just the file already on
/// disk, so that case is a copy (the original, fastest behavior). Everything
/// else decodes the result, adjusts it, and encodes it in memory first so a
/// failed encode never leaves a half-written file behind.
pub fn export_cutout(
    result_path: &str,
    dest: &Path,
    render: &ExportRender,
) -> Result<(), AppError> {
    let format = render.format;

    if format == ExportFormat::Png
        && render.color.is_none()
        && render.image.is_none()
        && render.edge == 0
    {
        std::fs::copy(result_path, dest).map_err(|e| AppError::Io(e.to_string()))?;
        return Ok(());
    }

    let mut rgba = image::open(result_path)
        .map_err(|e| AppError::DecodeFailed(format!("result image: {e}")))?
        .to_rgba8();
    refine_edges(&mut rgba, render.edge);
    let (w, h) = rgba.dimensions();

    // JPEG can't hold transparency, so with nothing chosen it gets white.
    let flat: Option<RgbImage> = if let Some(bg) = render.image {
        Some(flatten_over_image(&rgba, bg))
    } else if let Some(c) = render.color {
        Some(flatten_over(&rgba, c))
    } else if format == ExportFormat::Jpg {
        Some(flatten_over(&rgba, [255, 255, 255]))
    } else {
        None
    };

    let mut buf: Vec<u8> = Vec::new();
    let encoded = match flat {
        Some(flat) => match format {
            ExportFormat::Png => image::codecs::png::PngEncoder::new(&mut buf).write_image(
                flat.as_raw(),
                w,
                h,
                ExtendedColorType::Rgb8,
            ),
            ExportFormat::Jpg => image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 95)
                .write_image(flat.as_raw(), w, h, ExtendedColorType::Rgb8),
            ExportFormat::Webp => image::codecs::webp::WebPEncoder::new_lossless(&mut buf)
                .write_image(flat.as_raw(), w, h, ExtendedColorType::Rgb8),
        },
        // Transparent output. Plain PNG goes through here only when the edge
        // was changed, and WebP always does.
        None => match format {
            ExportFormat::Webp => image::codecs::webp::WebPEncoder::new_lossless(&mut buf)
                .write_image(rgba.as_raw(), w, h, ExtendedColorType::Rgba8),
            _ => image::codecs::png::PngEncoder::new(&mut buf).write_image(
                rgba.as_raw(),
                w,
                h,
                ExtendedColorType::Rgba8,
            ),
        },
    };
    encoded.map_err(|e| AppError::EncodeFailed(e.to_string()))?;

    std::fs::write(dest, &buf).map_err(|e| AppError::Io(e.to_string()))?;
    Ok(())
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
