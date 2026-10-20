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

/// Fits the visible part of the cutout onto a transparent canvas of exactly
/// `cw` x `ch` pixels. The cutout is first trimmed to the bounding box of what
/// is actually visible, then scaled to fit with a small margin, and centred.
fn fit_to_canvas(src: &RgbaImage, cw: u32, ch: u32) -> RgbaImage {
    let (w, h) = src.dimensions();
    let mut canvas = RgbaImage::new(cw, ch);

    // Bounding box of the pixels that are clearly visible.
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (w, h, 0u32, 0u32);
    for (x, y, px) in src.enumerate_pixels() {
        if px[3] > 8 {
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        }
    }
    if min_x > max_x || min_y > max_y {
        return canvas; // nothing visible to place
    }

    let bw = max_x - min_x + 1;
    let bh = max_y - min_y + 1;
    let cropped = image::imageops::crop_imm(src, min_x, min_y, bw, bh).to_image();

    const MARGIN: f32 = 0.06;
    let avail_w = cw as f32 * (1.0 - 2.0 * MARGIN);
    let avail_h = ch as f32 * (1.0 - 2.0 * MARGIN);
    let scale = (avail_w / bw as f32).min(avail_h / bh as f32);
    let nw = ((bw as f32 * scale).round() as u32).clamp(1, cw);
    let nh = ((bh as f32 * scale).round() as u32).clamp(1, ch);

    let resized =
        image::imageops::resize(&cropped, nw, nh, image::imageops::FilterType::CatmullRom);
    let x = (cw - nw) / 2;
    let y = (ch - nh) / 2;
    image::imageops::overlay(&mut canvas, &resized, x as i64, y as i64);
    canvas
}

/// Adds a soft drop shadow underneath the cutout, using the cutout's own
/// alpha as the shape. `strength` (1-100) controls how big, soft and dark it
/// is. The shadow is black, so only the cutout's own colors show through.
fn add_shadow(img: &mut RgbaImage, strength: u8) {
    let (w, h) = img.dimensions();
    let (wu, hu) = (w as usize, h as usize);
    let s = strength.min(100) as f32 / 100.0;
    let longest = w.max(h) as f32;

    let blur = ((0.006 + 0.03 * s) * longest).round().max(1.0) as usize;
    let offset = (blur as f32 * 0.6).round() as usize;
    let opacity = 0.25 + 0.35 * s;

    // The shadow's shape: the cutout's alpha channel moved down a little.
    let mut shadow = vec![0u8; wu * hu];
    {
        let raw = img.as_raw();
        for y in offset..hu {
            let src_row = (y - offset) * wu;
            let dst_row = y * wu;
            for x in 0..wu {
                shadow[dst_row + x] = raw[(src_row + x) * 4 + 3];
            }
        }
    }
    // Two box blurs in a row approximate a smooth gaussian falloff.
    box_blur(&mut shadow, wu, hu, blur);
    box_blur(&mut shadow, wu, hu, blur);

    for (i, px) in img.pixels_mut().enumerate() {
        let sa = shadow[i] as f32 / 255.0 * opacity;
        if sa <= 0.0 {
            continue;
        }
        let a = px[3] as f32 / 255.0;
        let out_a = a + sa * (1.0 - a);
        if out_a <= 0.0 {
            continue;
        }
        let k = a / out_a;
        px[0] = (px[0] as f32 * k).round() as u8;
        px[1] = (px[1] as f32 * k).round() as u8;
        px[2] = (px[2] as f32 * k).round() as u8;
        px[3] = (out_a * 255.0).round().clamp(0.0, 255.0) as u8;
    }
}

/// Every look-changing setting applied to the cutout itself (as opposed to
/// what goes behind it). The frontend sends them as one object, so a new
/// setting only needs adding here, in `shape_cutout`, and in the UI.
#[derive(serde::Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Effects {
    /// Edge hardness, -100 (sharper) to 100 (softer). 0 = unchanged.
    pub edge: i32,
    /// -100 (shrink the cutout) to 100 (grow it). 0 = unchanged.
    pub grow: i32,
    /// Color defringe strength, 0 (off) to 100.
    pub defringe: u8,
    /// Fit the cutout onto a canvas of exactly this size. Missing or zero
    /// means keep the original image size.
    pub canvas_width: Option<u32>,
    pub canvas_height: Option<u32>,
    /// Drop shadow strength, 0 (off) to 100.
    pub shadow: u8,
    /// Trim the empty space around the subject (ignored when a canvas size is set).
    pub auto_crop: bool,
    /// Brightness, contrast and saturation, each -100 to 100. 0 = unchanged.
    pub brightness: i32,
    pub contrast: i32,
    pub saturation: i32,
    /// Mirror the cutout, and turn it by `rotate` quarter turns (0-3, clockwise).
    pub flip_h: bool,
    pub flip_v: bool,
    pub rotate: u8,
    /// Sticker outline width, 0 (off) to 100, and its color as "#rrggbb".
    pub outline: u8,
    pub outline_color: String,
    /// Gradient to put behind the cutout. None = no gradient.
    pub gradient: Option<Gradient>,
    /// Blurred copy of the original photo behind the cutout: strength 0 (off) to 100.
    pub blur_bg: u8,
}

impl Effects {
    pub fn canvas(&self) -> Option<(u32, u32)> {
        match (self.canvas_width, self.canvas_height) {
            (Some(w), Some(h)) if w > 0 && h > 0 => Some((w, h)),
            _ => None,
        }
    }

    /// True when nothing here changes the cutout.
    pub fn is_default(&self) -> bool {
        self.edge == 0
            && self.grow == 0
            && self.defringe == 0
            && self.canvas().is_none()
            && self.shadow == 0
            && !self.auto_crop
            && self.brightness == 0
            && self.contrast == 0
            && self.saturation == 0
            && !self.flip_h
            && !self.flip_v
            && self.rotate % 4 == 0
            && self.outline == 0
            && self.gradient.is_none()
            && self.blur_bg == 0
    }
}

/// Moves the cutout's edge inward (shrink, negative) or outward (grow,
/// positive) by reshaping the alpha channel only. The distance scales with
/// the image size, like the Edges slider. Done with one box blur and a
/// threshold, which is fast even on large photos.
pub fn adjust_edge_position(img: &mut RgbaImage, amount: i32) {
    let amount = amount.clamp(-100, 100);
    if amount == 0 {
        return;
    }

    let (w, h) = img.dimensions();
    let (wu, hu) = (w as usize, h as usize);
    let longest = w.max(h) as f32;
    let dist = ((amount.abs() as f32 / 100.0) * longest * 0.004).round().max(1.0);
    let radius = (dist * 2.0) as usize;
    let window = (2 * radius + 1) as f32;

    let mut alpha: Vec<u8> = img.pixels().map(|p| p[3]).collect();
    box_blur(&mut alpha, wu, hu, radius);

    // Across a straight edge the blurred alpha is 0.5 on the old edge and
    // changes by 1/window per pixel. So the new edge sits where it crosses
    // 0.5 -/+ dist/window, and a 1-pixel margin each side keeps it smooth.
    let shift = dist / window;
    let threshold = if amount > 0 { 0.5 - shift } else { 0.5 + shift };
    let half = 1.0 / window;
    let (lo, hi) = (threshold - half, threshold + half);

    let mut lut = [0u8; 256];
    for (i, slot) in lut.iter_mut().enumerate() {
        let x = ((i as f32 / 255.0 - lo) / (hi - lo)).clamp(0.0, 1.0);
        *slot = (x * x * (3.0 - 2.0 * x) * 255.0).round().clamp(0.0, 255.0) as u8;
    }

    for (px, b) in img.pixels_mut().zip(alpha) {
        let new_a = lut[b as usize];
        // Growing never makes anything more transparent, shrinking never less.
        px[3] = if amount > 0 { px[3].max(new_a) } else { px[3].min(new_a) };
    }
}

/// Removes color halos along the cutout edge. Pixels that are only partly
/// visible often still carry the old background's color; this replaces their
/// color with the color of the solid subject pixels next to them. Only colors
/// change, never transparency. `strength` (0-100) is how far to move toward
/// the corrected color.
pub fn defringe(img: &mut RgbaImage, strength: u8) {
    if strength == 0 {
        return;
    }
    const SOLID: u8 = 245;

    let (w, h) = img.dimensions();
    let (wu, hu) = (w as usize, h as usize);
    let t = strength.min(100) as f32 / 100.0;

    // Pixels that are (nearly) fully visible are the trusted colors. The
    // partly visible ones in between are what gets corrected.
    let mut known: Vec<bool> = img.pixels().map(|p| p[3] >= SOLID).collect();
    let targets: Vec<usize> = img
        .pixels()
        .enumerate()
        .filter(|(_, p)| p[3] > 0 && p[3] < SOLID)
        .map(|(i, _)| i)
        .collect();
    if targets.is_empty() {
        return;
    }

    let data: &mut [u8] = &mut **img;
    let original: Vec<[u8; 3]> = targets
        .iter()
        .map(|&i| [data[i * 4], data[i * 4 + 1], data[i * 4 + 2]])
        .collect();
    let mut reached = vec![false; targets.len()];
    let mut remaining: Vec<usize> = (0..targets.len()).collect();

    // Colors spread outward from the solid area one pixel ring at a time,
    // through the partly visible band only. Only that band is visited, so
    // this stays fast on large images.
    let longest = w.max(h) as f32;
    let max_passes = ((0.012 * longest).round() as usize).clamp(3, 60);

    for _ in 0..max_passes {
        let mut newly: Vec<(usize, [u8; 3])> = Vec::new();
        let mut still: Vec<usize> = Vec::new();

        for &ti in &remaining {
            let idx = targets[ti];
            let (x, y) = ((idx % wu) as i32, (idx / wu) as i32);
            let (mut r, mut g, mut b, mut n) = (0u32, 0u32, 0u32, 0u32);
            for dy in -1i32..=1 {
                for dx in -1i32..=1 {
                    if dx == 0 && dy == 0 {
                        continue;
                    }
                    let (nx, ny) = (x + dx, y + dy);
                    if nx < 0 || ny < 0 || nx >= wu as i32 || ny >= hu as i32 {
                        continue;
                    }
                    let ni = ny as usize * wu + nx as usize;
                    if known[ni] {
                        r += data[ni * 4] as u32;
                        g += data[ni * 4 + 1] as u32;
                        b += data[ni * 4 + 2] as u32;
                        n += 1;
                    }
                }
            }
            if n > 0 {
                newly.push((ti, [(r / n) as u8, (g / n) as u8, (b / n) as u8]));
            } else {
                still.push(ti);
            }
        }

        if newly.is_empty() {
            break;
        }
        // Applied after the whole ring is measured, so each ring only
        // learns from the one before it.
        for (ti, c) in newly {
            let idx = targets[ti];
            data[idx * 4] = c[0];
            data[idx * 4 + 1] = c[1];
            data[idx * 4 + 2] = c[2];
            known[idx] = true;
            reached[ti] = true;
        }
        remaining = still;
        if remaining.is_empty() {
            break;
        }
    }

    // Blend between the old and the corrected color by the chosen strength.
    for (ti, &idx) in targets.iter().enumerate() {
        if !reached[ti] {
            continue;
        }
        for c in 0..3 {
            let corrected = data[idx * 4 + c] as f32;
            let old = original[ti][c] as f32;
            data[idx * 4 + c] = (old * (1.0 - t) + corrected * t).round().clamp(0.0, 255.0) as u8;
        }
    }
}

/// Trims the image to the box around everything that is visible.
fn crop_to_subject(src: RgbaImage) -> RgbaImage {
    let (w, h) = src.dimensions();
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (w, h, 0u32, 0u32);
    for (x, y, px) in src.enumerate_pixels() {
        if px[3] > 8 {
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        }
    }
    if min_x > max_x || min_y > max_y {
        return src; // nothing visible, leave it alone
    }
    image::imageops::crop_imm(&src, min_x, min_y, max_x - min_x + 1, max_y - min_y + 1).to_image()
}

/// A two-color gradient used as the background. `angle` follows CSS:
/// 0 points up, 90 points right.
#[derive(serde::Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Gradient {
    pub from: String,
    pub to: String,
    pub angle: f32,
}

/// Mirrors and turns the cutout.
fn orient_rgba(mut img: RgbaImage, fx: &Effects) -> RgbaImage {
    if fx.flip_h {
        img = image::imageops::flip_horizontal(&img);
    }
    if fx.flip_v {
        img = image::imageops::flip_vertical(&img);
    }
    match fx.rotate % 4 {
        1 => image::imageops::rotate90(&img),
        2 => image::imageops::rotate180(&img),
        3 => image::imageops::rotate270(&img),
        _ => img,
    }
}

/// Same as `orient_rgba`, for the photo used as a blurred background.
fn orient_dynamic(mut img: DynamicImage, fx: &Effects) -> DynamicImage {
    if fx.flip_h {
        img = img.fliph();
    }
    if fx.flip_v {
        img = img.flipv();
    }
    match fx.rotate % 4 {
        1 => img.rotate90(),
        2 => img.rotate180(),
        3 => img.rotate270(),
        _ => img,
    }
}

/// Brightness, contrast and saturation of the subject's colors, each -100 to
/// 100. Transparency is untouched.
pub fn adjust_colors(img: &mut RgbaImage, brightness: i32, contrast: i32, saturation: i32) {
    if brightness == 0 && contrast == 0 && saturation == 0 {
        return;
    }
    let offset = brightness.clamp(-100, 100) as f32 * 2.55;
    let factor = 1.0 + contrast.clamp(-100, 100) as f32 / 100.0;
    let sat = 1.0 + saturation.clamp(-100, 100) as f32 / 100.0;

    let mut lut = [0u8; 256];
    for (i, slot) in lut.iter_mut().enumerate() {
        *slot = ((i as f32 - 128.0) * factor + 128.0 + offset)
            .round()
            .clamp(0.0, 255.0) as u8;
    }

    for px in img.pixels_mut() {
        if px[3] == 0 {
            continue;
        }
        let (r, g, b) = (px[0] as f32, px[1] as f32, px[2] as f32);
        let gray = 0.299 * r + 0.587 * g + 0.114 * b;
        let mix = |v: f32| (gray + (v - gray) * sat).round().clamp(0.0, 255.0) as usize;
        px[0] = lut[mix(r)];
        px[1] = lut[mix(g)];
        px[2] = lut[mix(b)];
    }
}

/// Grows an alpha channel outward by about `dist` pixels with a round shape.
/// Two box blurs approximate a gaussian, and the new edge is where that
/// blurred edge crosses the level a gaussian of this size has `dist` pixels
/// outside a straight edge.
fn dilate_alpha(alpha: &mut [u8], w: usize, h: usize, dist: f32) {
    let sigma = dist.max(1.0);
    let radius = (((sigma * 2.45 - 1.0) / 2.0).round().max(1.0)) as usize;
    box_blur(alpha, w, h, radius);
    box_blur(alpha, w, h, radius);

    let threshold = 0.1587f32;
    let half = (0.242 / sigma).clamp(0.02, 0.2);
    let (lo, hi) = (threshold - half, threshold + half);

    let mut lut = [0u8; 256];
    for (i, slot) in lut.iter_mut().enumerate() {
        let x = ((i as f32 / 255.0 - lo) / (hi - lo)).clamp(0.0, 1.0);
        *slot = (x * x * (3.0 - 2.0 * x) * 255.0).round().clamp(0.0, 255.0) as u8;
    }
    for a in alpha.iter_mut() {
        *a = lut[*a as usize];
    }
}

/// Adds a solid-color sticker outline around the cutout, behind it.
/// `strength` (1-100) is the outline width, relative to the image size.
fn add_outline(img: &mut RgbaImage, strength: u8, color: [u8; 3]) {
    if strength == 0 {
        return;
    }
    let (w, h) = img.dimensions();
    let longest = w.max(h) as f32;
    let dist = ((strength.min(100) as f32 / 100.0) * longest * 0.02)
        .round()
        .max(1.0);

    let mut grown: Vec<u8> = img.pixels().map(|p| p[3]).collect();
    dilate_alpha(&mut grown, w as usize, h as usize, dist);

    for (px, d) in img.pixels_mut().zip(grown) {
        let a = px[3] as f32 / 255.0;
        let d = (d as f32 / 255.0).max(a);
        // The subject sits on top of the outline layer.
        let out_a = a + d * (1.0 - a);
        if out_a <= 0.0 {
            continue;
        }
        for c in 0..3 {
            let subject = px[c] as f32;
            let base = color[c] as f32;
            px[c] = ((subject * a + base * d * (1.0 - a)) / out_a)
                .round()
                .clamp(0.0, 255.0) as u8;
        }
        px[3] = (out_a * 255.0).round().clamp(0.0, 255.0) as u8;
    }
}

/// Draws a two-color gradient of the given size, matching CSS `linear-gradient`.
fn make_gradient(w: u32, h: u32, g: &Gradient) -> RgbImage {
    let a = parse_hex_color(&g.from).unwrap_or([255, 255, 255]);
    let b = parse_hex_color(&g.to).unwrap_or([0, 0, 0]);
    let rad = g.angle.to_radians();
    let (dx, dy) = (rad.sin(), -rad.cos());
    let len = (w as f32 * dx.abs() + h as f32 * dy.abs()).max(1.0);
    let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);

    let mut out = RgbImage::new(w, h);
    for (x, y, px) in out.enumerate_pixels_mut() {
        let t = (((x as f32 + 0.5 - cx) * dx + (y as f32 + 0.5 - cy) * dy) / len + 0.5)
            .clamp(0.0, 1.0);
        let mix = |i: usize| (a[i] as f32 + (b[i] as f32 - a[i] as f32) * t).round() as u8;
        *px = image::Rgb([mix(0), mix(1), mix(2)]);
    }
    out
}

/// A blurred copy of the original photo, `w` x `h` pixels, to use as the
/// background. It is turned the same way as the cutout. When the cutout keeps
/// the photo's own size it lines up with the subject exactly; with a size
/// preset or auto-crop it simply fills the new frame.
fn make_blur_backdrop(orig: &DynamicImage, fx: &Effects, w: u32, h: u32) -> RgbImage {
    use image::imageops::FilterType;

    // Work on a small copy: a blur hides detail anyway, and it is far faster.
    let small = if orig.width().max(orig.height()) > 1600 {
        orig.thumbnail(1600, 1600)
    } else {
        orig.clone()
    };
    let small = orient_dynamic(small, fx);

    let k = (1200.0 / w.max(h) as f32).min(1.0);
    let ww = ((w as f32 * k).round() as u32).max(1);
    let wh = ((h as f32 * k).round() as u32).max(1);
    let work = small.resize_to_fill(ww, wh, FilterType::Triangle).to_rgb8();
    let (wu, hu) = (ww as usize, wh as usize);

    let strength = fx.blur_bg.min(100) as f32 / 100.0;
    let radius = (strength * ww.max(wh) as f32 * 0.03).round().max(1.0) as usize;

    let mut planes = [vec![0u8; wu * hu], vec![0u8; wu * hu], vec![0u8; wu * hu]];
    for (i, p) in work.pixels().enumerate() {
        for c in 0..3 {
            planes[c][i] = p[c];
        }
    }
    for plane in planes.iter_mut() {
        box_blur(plane, wu, hu, radius);
        box_blur(plane, wu, hu, radius);
    }

    let mut blurred = RgbImage::new(ww, wh);
    for (i, p) in blurred.pixels_mut().enumerate() {
        *p = image::Rgb([planes[0][i], planes[1][i], planes[2][i]]);
    }

    if (ww, wh) == (w, h) {
        blurred
    } else {
        image::imageops::resize(&blurred, w, h, FilterType::Triangle)
    }
}

/// Composites an RGBA cutout over a backdrop of the same size.
fn composite_over(rgba: &RgbaImage, backdrop: &RgbImage) -> RgbImage {
    let (w, h) = rgba.dimensions();
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

/// The cutout over a blurred copy of the original photo (opaque result).
pub fn flatten_over_blur(rgba: &RgbaImage, original: &DynamicImage, fx: &Effects) -> RgbImage {
    let (w, h) = rgba.dimensions();
    composite_over(rgba, &make_blur_backdrop(original, fx, w, h))
}

/// Applies every look-changing setting to a cutout, in the one order used by
/// previews, exports and copy-to-clipboard so they always agree: flip and
/// rotate, edge position (shrink/grow), edge softness, color defringe, color
/// adjust, fitting onto a canvas size, the sticker outline, the drop shadow,
/// and last the auto-crop.
pub fn shape_cutout(rgba: RgbaImage, fx: &Effects) -> RgbaImage {
    let mut rgba = orient_rgba(rgba, fx);
    adjust_edge_position(&mut rgba, fx.grow);
    refine_edges(&mut rgba, fx.edge);
    defringe(&mut rgba, fx.defringe);
    adjust_colors(&mut rgba, fx.brightness, fx.contrast, fx.saturation);
    let canvas = fx.canvas();
    if let Some((cw, ch)) = canvas {
        rgba = fit_to_canvas(&rgba, cw, ch);
    }
    if fx.outline > 0 {
        let color = parse_hex_color(&fx.outline_color).unwrap_or([255, 255, 255]);
        add_outline(&mut rgba, fx.outline, color);
    }
    if fx.shadow > 0 {
        add_shadow(&mut rgba, fx.shadow);
    }
    // Cropping comes last so the shadow is kept inside the cropped image.
    if canvas.is_none() && fx.auto_crop {
        rgba = crop_to_subject(rgba);
    }
    rgba
}

// ---------- Touch-up brush ----------

/// One brush stroke painted in the touch-up editor, in image pixel coordinates.
#[derive(serde::Deserialize)]
pub struct BrushStroke {
    /// true = erase (make transparent), false = restore (make visible again).
    pub erase: bool,
    /// Brush radius in image pixels.
    pub radius: f32,
    pub points: Vec<[f32; 2]>,
}

/// Paints one round, soft-edged dab of the brush onto the alpha channel.
fn stamp_brush(img: &mut RgbaImage, cx: f32, cy: f32, r: f32, erase: bool) {
    let (w, h) = img.dimensions();
    let x0 = ((cx - r).floor() as i32).max(0);
    let x1 = ((cx + r).ceil() as i32).min(w as i32 - 1);
    let y0 = ((cy - r).floor() as i32).max(0);
    let y1 = ((cy + r).ceil() as i32).min(h as i32 - 1);
    if x0 > x1 || y0 > y1 {
        return;
    }

    let inner = r * 0.8;
    for y in y0..=y1 {
        for x in x0..=x1 {
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            let d = (dx * dx + dy * dy).sqrt();
            if d >= r {
                continue;
            }
            let coverage = if d <= inner { 1.0 } else { (r - d) / (r - inner) };
            let px = img.get_pixel_mut(x as u32, y as u32);
            let a = px[3] as f32;
            let new_a = if erase {
                a * (1.0 - coverage)
            } else {
                a + (255.0 - a) * coverage
            };
            px[3] = new_a.round().clamp(0.0, 255.0) as u8;
        }
    }
}

/// Paints brush strokes onto the cutout's alpha channel. The colors are left
/// alone on purpose: the result file keeps the original photo's colors even
/// where it is transparent, so "restore" only has to bring the alpha back.
pub fn apply_brush_strokes(img: &mut RgbaImage, strokes: &[BrushStroke]) {
    for stroke in strokes {
        let r = stroke.radius.max(1.0);
        let step = (r * 0.3).max(1.0);

        match stroke.points.len() {
            0 => {}
            1 => stamp_brush(img, stroke.points[0][0], stroke.points[0][1], r, stroke.erase),
            _ => {
                for pair in stroke.points.windows(2) {
                    let (ax, ay) = (pair[0][0], pair[0][1]);
                    let (bx, by) = (pair[1][0], pair[1][1]);
                    let dist = ((bx - ax).powi(2) + (by - ay).powi(2)).sqrt();
                    let n = (dist / step).ceil().max(1.0) as u32;
                    for i in 0..=n {
                        let t = i as f32 / n as f32;
                        stamp_brush(img, ax + (bx - ax) * t, ay + (by - ay) * t, r, stroke.erase);
                    }
                }
            }
        }
    }
}

/// Everything that shapes an exported file besides the cutout itself.
pub struct ExportRender<'a> {
    pub format: ExportFormat,
    /// Solid color behind the cutout. Ignored when `image` is set.
    pub color: Option<[u8; 3]>,
    /// Picture behind the cutout, already decoded.
    pub image: Option<&'a DynamicImage>,
    /// The look-changing settings applied to the cutout itself.
    pub fx: Effects,
    /// The original photo, when a blurred copy of it is the background.
    pub blur_source: Option<&'a DynamicImage>,
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
        && render.fx.is_default()
    {
        std::fs::copy(result_path, dest).map_err(|e| AppError::Io(e.to_string()))?;
        return Ok(());
    }

    let buf = encode_cutout(result_path, render)?;
    std::fs::write(dest, &buf).map_err(|e| AppError::Io(e.to_string()))?;
    Ok(())
}

/// Renders the cutout with all the chosen settings and returns the encoded
/// file bytes (nothing is written to disk). Used for exports and for
/// copying to the clipboard.
pub fn encode_cutout(result_path: &str, render: &ExportRender) -> Result<Vec<u8>, AppError> {
    let format = render.format;

    let source = image::open(result_path)
        .map_err(|e| AppError::DecodeFailed(format!("result image: {e}")))?
        .to_rgba8();
    let rgba = shape_cutout(source, &render.fx);
    let (w, h) = rgba.dimensions();

    // JPEG can't hold transparency, so with nothing chosen it gets white.
    let flat: Option<RgbImage> = if let Some(bg) = render.image {
        Some(flatten_over_image(&rgba, bg))
    } else if let Some(g) = &render.fx.gradient {
        Some(composite_over(&rgba, &make_gradient(w, h, g)))
    } else if let (true, Some(src)) = (render.fx.blur_bg > 0, render.blur_source) {
        Some(flatten_over_blur(&rgba, src, &render.fx))
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
    Ok(buf)
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
