//! Image preprocessing on the CPU, as Qwen's image processor (`Qwen2VLImageProcessor`, the
//! torchvision backend transformers uses for Qwen3.5/3.8) does it:
//!
//! 1. decode (PNG, JPEG, WebP, GIF first frame, BMP) and drop any alpha channel (`convert("RGB")`);
//! 2. `smart_resize`: both sides to multiples of `patch × merge` (32 px), the area within
//!    `[min_pixels, max_pixels]`, the aspect ratio kept as closely as possible;
//! 3. bicubic resampling as Pillow does it (and torchvision's antialiased uint8 path, which matches
//!    Pillow): Keys a = -0.5, the kernel widened by the scale when shrinking, horizontal then
//!    vertical pass in 22-bit fixed point with 8-bit rounding between them;
//! 4. `(x - 255·mean) / (255·std)` in f32 (the processor's fused rescale and normalize);
//! 5. the frame repeated to the temporal patch (2) and cut into patches flattened as
//!    `channel × temporal × row × column`, emitted in merge-window order.

use super::VisionConfig;
use half::bf16;

/// `smart_resize(height, width, factor, min_pixels, max_pixels)` -> (height, width).
pub fn smart_resize(height: u32, width: u32, factor: u32, min_pixels: u64, max_pixels: u64) -> Result<(u32, u32), String> {
    if height == 0 || width == 0 {
        return Err("the image is empty".into());
    }
    let (h, w, f) = (f64::from(height), f64::from(width), f64::from(factor));
    if h.max(w) / h.min(w) > 200.0 {
        return Err(format!("the image's aspect ratio {}:{} exceeds 200", height.max(width), height.min(width)));
    }
    // Python's round() rounds halves to even.
    let mut h_bar = (h / f).round_ties_even() * f;
    let mut w_bar = (w / f).round_ties_even() * f;
    if h_bar * w_bar > max_pixels as f64 {
        let beta = ((h * w) / max_pixels as f64).sqrt();
        h_bar = f.max((h / beta / f).floor() * f);
        w_bar = f.max((w / beta / f).floor() * f);
    } else if h_bar * w_bar < min_pixels as f64 {
        let beta = (min_pixels as f64 / (h * w)).sqrt();
        h_bar = (h * beta / f).ceil() * f;
        w_bar = (w * beta / f).ceil() * f;
    }
    Ok((h_bar as u32, w_bar as u32))
}

/// The merged token grid (rows, columns) of an image of `width × height` pixels.
pub fn merged_grid(config: &VisionConfig, width: u32, height: u32, max_pixels: u64) -> Result<(u32, u32), String> {
    let factor = config.factor();
    let (h, w) = smart_resize(height, width, factor, config.min_pixels, max_pixels.max(u64::from(factor) * u64::from(factor)))?;
    Ok((h / factor, w / factor))
}

/// One image ready for the encoder.
#[derive(Debug, Clone, PartialEq)]
pub struct Patches {
    /// `grid.0 × grid.1` patches of `patch_dim` values, in merge-window order.
    pub values: Vec<bf16>,
    /// Patch grid (rows, columns).
    pub grid: (u32, u32),
    /// Merged token grid (rows, columns).
    pub merged: (u32, u32),
}

/// Decode an image (format from its bytes) to 8-bit RGB.
pub fn decode(bytes: &[u8]) -> Result<image::RgbImage, String> {
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| format!("cannot read the image: {error}"))?;
    let format = reader.format().ok_or("unrecognized image format (PNG, JPEG, WebP, GIF or BMP expected)")?;
    let image = reader.decode().map_err(|error| format!("cannot decode the {format:?} image: {error}"))?;
    Ok(image.to_rgb8())
}

/// Width and height from the image header (no full decode).
pub fn dimensions(bytes: &[u8]) -> Result<(u32, u32), String> {
    image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| format!("cannot read the image: {error}"))?
        .into_dimensions()
        .map_err(|error| format!("cannot read the image size: {error}"))
}

/// Pillow's bicubic kernel (a = -0.5).
fn bicubic(x: f64) -> f64 {
    const A: f64 = -0.5;
    let x = x.abs();
    if x < 1.0 {
        ((A + 2.0) * x - (A + 3.0)) * x * x + 1.0
    } else if x < 2.0 {
        (((x - 5.0) * x + 8.0) * x - 4.0) * A
    } else {
        0.0
    }
}

const PRECISION_BITS: u32 = 32 - 8 - 2;

/// Pillow's `precompute_coeffs` and `normalize_coeffs_8bpc`: per output pixel the first input
/// pixel and the fixed-point weights.
fn coefficients(in_size: u32, out_size: u32) -> Vec<(usize, Vec<i64>)> {
    let scale = f64::from(in_size) / f64::from(out_size);
    let filter_scale = scale.max(1.0);
    let support = 2.0 * filter_scale;
    (0..out_size)
        .map(|xx| {
            let center = (f64::from(xx) + 0.5) * scale;
            let inverse = 1.0 / filter_scale;
            let xmin = ((center - support + 0.5) as i64).max(0);
            let xmax = ((center + support + 0.5) as i64).min(i64::from(in_size));
            let weights: Vec<f64> = (xmin..xmax).map(|x| bicubic((x as f64 - center + 0.5) * inverse)).collect();
            let total: f64 = weights.iter().sum();
            let fixed = weights
                .iter()
                .map(|weight| {
                    let weight = if total != 0.0 { weight / total } else { *weight };
                    let scaled = weight * f64::from(1u32 << PRECISION_BITS);
                    if weight < 0.0 { (-0.5 + scaled) as i64 } else { (0.5 + scaled) as i64 }
                })
                .collect();
            (xmin as usize, fixed)
        })
        .collect()
}

fn clip8(value: i64) -> u8 {
    (value >> PRECISION_BITS).clamp(0, 255) as u8
}

/// Bicubic resize of an RGB image, as `PIL.Image.resize(..., BICUBIC)`.
pub fn resize_bicubic(image: &image::RgbImage, width: u32, height: u32) -> image::RgbImage {
    let horizontal = if image.width() == width {
        image.clone()
    } else {
        let coefficients = coefficients(image.width(), width);
        image::RgbImage::from_fn(width, image.height(), |x, y| {
            let (start, weights) = &coefficients[x as usize];
            let mut sums = [1i64 << (PRECISION_BITS - 1); 3];
            for (offset, weight) in weights.iter().enumerate() {
                let pixel = image.get_pixel((start + offset) as u32, y).0;
                for channel in 0..3 {
                    sums[channel] += i64::from(pixel[channel]) * weight;
                }
            }
            image::Rgb(sums.map(clip8))
        })
    };
    if horizontal.height() == height {
        return horizontal;
    }
    let coefficients = coefficients(horizontal.height(), height);
    image::RgbImage::from_fn(width, height, |x, y| {
        let (start, weights) = &coefficients[y as usize];
        let mut sums = [1i64 << (PRECISION_BITS - 1); 3];
        for (offset, weight) in weights.iter().enumerate() {
            let pixel = horizontal.get_pixel(x, (start + offset) as u32).0;
            for channel in 0..3 {
                sums[channel] += i64::from(pixel[channel]) * weight;
            }
        }
        image::Rgb(sums.map(clip8))
    })
}

/// Resized, normalized pixels (`channel × height × width`, f32) of an RGB image.
pub fn normalized(image: &image::RgbImage, config: &VisionConfig, max_pixels: u64) -> Result<(Vec<f32>, u32, u32), String> {
    let factor = config.factor();
    let (height, width) = smart_resize(image.height(), image.width(), factor, config.min_pixels, max_pixels.max(u64::from(factor) * u64::from(factor)))?;
    let resized = resize_bicubic(image, width, height);
    let plane = (width * height) as usize;
    let mut out = vec![0f32; 3 * plane];
    for (index, pixel) in resized.pixels().enumerate() {
        for channel in 0..3 {
            let mean = config.image_mean[channel] * 255.0;
            let std = config.image_std[channel] * 255.0;
            out[channel * plane + index] = (f32::from(pixel.0[channel]) - mean) / std;
        }
    }
    Ok((out, height, width))
}

/// Patch values of normalized pixels (`channel × height × width`) in merge-window order, f32
/// (the processor's `pixel_values`), and the patch grid.
pub fn patch_values(pixels: &[f32], height: u32, width: u32, config: &VisionConfig) -> (Vec<f32>, (u32, u32)) {
    let patch = config.patch_size;
    let (rows, columns) = (height / patch, width / patch);
    let temporal = config.temporal_patch_size as usize;
    let channels = config.in_channels as usize;
    let p = patch as usize;
    let mut values = Vec::with_capacity((rows * columns) as usize * channels * temporal * p * p);
    for (row, column) in uzu_engine::engine::vision::window_order((rows, columns), config.spatial_merge_size) {
        for channel in 0..channels {
            for _ in 0..temporal {
                for y in 0..p {
                    let line = channel * (height * width) as usize + (row as usize * p + y) * width as usize + column as usize * p;
                    values.extend_from_slice(&pixels[line..line + p]);
                }
            }
        }
    }
    (values, (rows, columns))
}

/// Patches of normalized pixels (`channel × height × width`), in merge-window order, as the
/// encoder takes them (bf16).
pub fn patchify(pixels: &[f32], height: u32, width: u32, config: &VisionConfig) -> Patches {
    let (values, grid) = patch_values(pixels, height, width, config);
    let merge = config.spatial_merge_size;
    Patches { values: values.into_iter().map(bf16::from_f32).collect(), grid, merged: (grid.0 / merge, grid.1 / merge) }
}

/// Decode, resize, normalize and patchify an image file's bytes.
pub fn preprocess(bytes: &[u8], config: &VisionConfig, max_pixels: u64) -> Result<Patches, String> {
    let image = decode(bytes)?;
    let (pixels, height, width) = normalized(&image, config, max_pixels)?;
    Ok(patchify(&pixels, height, width, config))
}
