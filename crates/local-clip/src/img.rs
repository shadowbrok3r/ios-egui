//! CLIP preprocessing: RGB bytes to the visual tower's `1x3x224x224` input.
//!
//! Contract (OpenAI CLIP): resize the shortest side to 224 (bilinear),
//! centre-crop 224x224, RGB, scale to `0..1`, normalize with the CLIP means/stds,
//! layout NCHW (channel-planar) float32.

use crate::error::Result;
use image::{imageops::FilterType, RgbImage};

/// The visual tower's fixed square input edge.
pub const INPUT_SIZE: u32 = 224;

/// Per-channel means CLIP normalizes with (RGB order).
pub const CLIP_MEAN: [f32; 3] = [0.48145466, 0.4578275, 0.40821073];
/// Per-channel standard deviations CLIP normalizes with (RGB order).
pub const CLIP_STD: [f32; 3] = [0.26862954, 0.26130258, 0.27577711];

/// Resized `w x h` so the shortest side is `size`, aspect preserved (both dims at least `size`).
pub fn fit_shortest(w: u32, h: u32, size: u32) -> (u32, u32) {
    if w == 0 || h == 0 {
        return (size, size);
    }
    let scale = (size as f32 / w as f32).max(size as f32 / h as f32);
    let nw = ((w as f32 * scale).round() as u32).max(size);
    let nh = ((h as f32 * scale).round() as u32).max(size);
    (nw, nh)
}

/// Top-left offset of a centred `size x size` crop inside a `nw x nh` image.
pub fn center_crop_offset(nw: u32, nh: u32, size: u32) -> (u32, u32) {
    (nw.saturating_sub(size) / 2, nh.saturating_sub(size) / 2)
}

/// A `size x size` RGB (HWC) buffer to CLIP-normalized NCHW float32.
pub fn normalize_nchw(rgb: &[u8], size: u32) -> Vec<f32> {
    let n = (size * size) as usize;
    let mut out = vec![0.0f32; n * 3];
    for i in 0..n {
        for c in 0..3 {
            let v = rgb[i * 3 + c] as f32 / 255.0;
            out[c * n + i] = (v - CLIP_MEAN[c]) / CLIP_STD[c];
        }
    }
    out
}

/// RGB bytes to the visual tower input at `size`: resize shortest side, centre-crop, normalize NCHW.
pub fn preprocess(rgb: &[u8], w: u32, h: u32, size: u32) -> Vec<f32> {
    let (nw, nh) = fit_shortest(w, h, size);
    let src = RgbImage::from_raw(w, h, rgb.to_vec()).unwrap_or_else(|| RgbImage::new(1, 1));
    let resized =
        if (nw, nh) == (w, h) { src } else { image::imageops::resize(&src, nw, nh, FilterType::Triangle) };
    let (ox, oy) = center_crop_offset(nw, nh, size);
    let cropped = image::imageops::crop_imm(&resized, ox, oy, size, size).to_image();
    normalize_nchw(cropped.as_raw(), size)
}

/// Decode encoded image bytes (png/jpeg/webp) to `(rgb, width, height)`, dropping any alpha.
pub fn decode_rgb(bytes: &[u8]) -> Result<(Vec<u8>, u32, u32)> {
    let img = image::load_from_memory(bytes)?.to_rgb8();
    let (w, h) = (img.width(), img.height());
    Ok((img.into_raw(), w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_shortest_scales_the_short_side_to_size() {
        assert_eq!(fit_shortest(224, 224, 224), (224, 224));
        // Wide image: height is the short side -> 224, width scaled up.
        assert_eq!(fit_shortest(400, 200, 224), (448, 224));
        // Tall image: width is the short side -> 224.
        assert_eq!(fit_shortest(200, 400, 224), (224, 448));
        // Degenerate size falls back to a full square.
        assert_eq!(fit_shortest(0, 10, 224), (224, 224));
    }

    #[test]
    fn center_crop_offset_centres_the_box() {
        assert_eq!(center_crop_offset(448, 224, 224), (112, 0));
        assert_eq!(center_crop_offset(224, 448, 224), (0, 112));
        assert_eq!(center_crop_offset(224, 224, 224), (0, 0));
    }

    #[test]
    fn normalize_scales_and_lays_out_nchw() {
        // 2x2 HWC: red, green, blue, black.
        let rgb = [255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0];
        let out = normalize_nchw(&rgb, 2);
        assert_eq!(out.len(), 2 * 2 * 3);
        let n = 4;
        let hi = |c: usize| (1.0 - CLIP_MEAN[c]) / CLIP_STD[c];
        let lo = |c: usize| (0.0 - CLIP_MEAN[c]) / CLIP_STD[c];
        // R plane (indices 0..n): red pixel 0 is high, green pixel 1 is low.
        assert!((out[0] - hi(0)).abs() < 1e-5);
        assert!((out[1] - lo(0)).abs() < 1e-5);
        // G plane starts at n: green pixel 1 is high.
        assert!((out[n + 1] - hi(1)).abs() < 1e-5);
        // B plane starts at 2n: blue pixel 2 is high.
        assert!((out[2 * n + 2] - hi(2)).abs() < 1e-5);
    }

    #[test]
    fn preprocess_center_crops_and_emits_nchw() {
        // 4x2 HWC, left half red, right half blue; short side is already 2, crop keeps cols 1..3.
        let mut rgb = Vec::new();
        for _y in 0..2 {
            for x in 0..4 {
                rgb.extend_from_slice(if x < 2 { &[255, 0, 0] } else { &[0, 0, 255] });
            }
        }
        let out = preprocess(&rgb, 4, 2, 2);
        assert_eq!(out.len(), 2 * 2 * 3);
        let n = 4;
        let hi = |c: usize| (1.0 - CLIP_MEAN[c]) / CLIP_STD[c];
        let lo = |c: usize| (0.0 - CLIP_MEAN[c]) / CLIP_STD[c];
        // Cropped cols are original 1 (red) and 2 (blue): R plane pixel 0 high, pixel 1 low.
        assert!((out[0] - hi(0)).abs() < 1e-4);
        assert!((out[1] - lo(0)).abs() < 1e-4);
        // B plane: pixel 1 (blue) is high.
        assert!((out[2 * n + 1] - hi(2)).abs() < 1e-4);
    }
}
