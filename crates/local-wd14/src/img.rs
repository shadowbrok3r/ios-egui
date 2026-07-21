//! WD14 preprocessing: RGBA bytes to the classifier's `1x448x448x3` input.
//!
//! Contract (WD taggers are cv2-trained): composite over WHITE, aspect-fit and
//! centre-pad to a square with WHITE borders, channel order RGB->BGR, float32
//! in `0..255` (no `0..1` normalization), layout NHWC.

use crate::error::Result;
use image::{imageops::FilterType, RgbImage};

/// The tagger's fixed square input edge.
pub const INPUT_SIZE: u32 = 448;

/// Fitted size and centring offset placing a `w x h` image inside a `size` square, aspect preserved.
pub fn fit_dims(w: u32, h: u32, size: u32) -> (u32, u32, u32, u32) {
    if w == 0 || h == 0 {
        return (size, size, 0, 0);
    }
    let scale = (size as f32 / w as f32).min(size as f32 / h as f32);
    let nw = ((w as f32 * scale).round() as u32).clamp(1, size);
    let nh = ((h as f32 * scale).round() as u32).clamp(1, size);
    ((nw), nh, (size - nw) / 2, (size - nh) / 2)
}

/// Alpha-composite `rgba` over a white background into packed RGB (row-major HWC).
pub fn composite_over_white(rgba: &[u8], w: u32, h: u32) -> Vec<u8> {
    let n = (w * h) as usize;
    let mut rgb = vec![255u8; n * 3];
    for i in 0..n.min(rgba.len() / 4) {
        let a = rgba[i * 4 + 3] as u16;
        for c in 0..3 {
            let fg = rgba[i * 4 + c] as u16;
            // fg*a + 255*(255-a), rounded, over 255.
            rgb[i * 3 + c] = ((fg * a + 255 * (255 - a) + 127) / 255) as u8;
        }
    }
    rgb
}

/// A `size x size` RGB (HWC) buffer to BGR NHWC float32 in `0..255`.
pub fn rgb_to_bgr_nhwc(rgb: &[u8], size: u32) -> Vec<f32> {
    let n = (size * size) as usize;
    let mut out = vec![0.0f32; n * 3];
    for i in 0..n {
        out[i * 3] = rgb[i * 3 + 2] as f32;
        out[i * 3 + 1] = rgb[i * 3 + 1] as f32;
        out[i * 3 + 2] = rgb[i * 3] as f32;
    }
    out
}

/// RGBA bytes to the classifier input at `size`: composite, aspect-fit onto a white square, BGR f32.
pub fn preprocess(rgba: &[u8], w: u32, h: u32, size: u32) -> Vec<f32> {
    let rgb = composite_over_white(rgba, w, h);
    let (nw, nh, ox, oy) = fit_dims(w, h, size);
    let src = RgbImage::from_raw(w, h, rgb).unwrap_or_else(|| RgbImage::new(1, 1));
    let resized = image::imageops::resize(&src, nw, nh, FilterType::Triangle);
    let mut canvas = RgbImage::from_pixel(size, size, image::Rgb([255, 255, 255]));
    image::imageops::overlay(&mut canvas, &resized, ox as i64, oy as i64);
    rgb_to_bgr_nhwc(canvas.as_raw(), size)
}

/// Decode encoded image bytes (png/jpeg/webp) to `(rgba, width, height)`.
pub fn decode_rgba(bytes: &[u8]) -> Result<(Vec<u8>, u32, u32)> {
    let img = image::load_from_memory(bytes)?.to_rgba8();
    let (w, h) = (img.width(), img.height());
    Ok((img.into_raw(), w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_preserves_aspect_and_centres() {
        // Square fills the canvas.
        assert_eq!(fit_dims(100, 100, 448), (448, 448, 0, 0));
        // Wide image is width-limited, centred vertically.
        let (nw, nh, ox, oy) = fit_dims(200, 100, 448);
        assert_eq!((nw, nh, ox), (448, 224, 0));
        assert_eq!(oy, 112);
        // Tall image is height-limited, centred horizontally.
        let (nw, nh, ox, oy) = fit_dims(100, 200, 448);
        assert_eq!((nw, nh, oy), (224, 448, 0));
        assert_eq!(ox, 112);
        // 2x1 into a 2 square: one image row, one white row.
        assert_eq!(fit_dims(2, 1, 2), (2, 1, 0, 0));
    }

    #[test]
    fn composite_blends_alpha_over_white() {
        // Opaque red stays red; fully transparent becomes white; half-alpha black -> mid grey.
        let rgba = [255, 0, 0, 255, 0, 0, 0, 0, 0, 0, 0, 128];
        let rgb = composite_over_white(&rgba, 3, 1);
        assert_eq!(&rgb[0..3], &[255, 0, 0]);
        assert_eq!(&rgb[3..6], &[255, 255, 255]);
        // 0*128 + 255*127 = 32385, +127 /255 = 127.
        assert_eq!(&rgb[6..9], &[127, 127, 127]);
    }

    #[test]
    fn channel_order_is_bgr_and_range_is_0_255() {
        // 2x2 RGB: R, G, B, white.
        let rgb = [255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255];
        let out = rgb_to_bgr_nhwc(&rgb, 2);
        assert_eq!(&out[0..3], &[0.0, 0.0, 255.0]); // R pixel -> BGR
        assert_eq!(&out[3..6], &[0.0, 255.0, 0.0]); // G pixel
        assert_eq!(&out[6..9], &[255.0, 0.0, 0.0]); // B pixel
        assert_eq!(&out[9..12], &[255.0, 255.0, 255.0]); // white
    }

    #[test]
    fn preprocess_pads_and_emits_full_buffer() {
        // A solid-red 4x2 image: content resized to fill width, padded white above/below.
        let rgba: Vec<u8> = std::iter::repeat_n([255u8, 0, 0, 255], 8).flatten().collect();
        let out = preprocess(&rgba, 4, 2, 4);
        assert_eq!(out.len(), 4 * 4 * 3);
        // Top-left is white padding (BGR 255,255,255); a middle row is red (BGR 0,0,255).
        assert_eq!(&out[0..3], &[255.0, 255.0, 255.0]);
        let mid = (4 * 1) * 3; // row 1, col 0
        assert_eq!(&out[mid..mid + 3], &[0.0, 0.0, 255.0]);
    }
}
