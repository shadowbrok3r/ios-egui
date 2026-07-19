//! Image output: VAE decode to RGB, PNG encode, and a cheap latent preview.

use crate::error::Result;
use image::{codecs::png::PngEncoder, ExtendedColorType, ImageEncoder};

/// An 8-bit RGB image, row-major HWC.
#[derive(Clone, Debug)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
}

impl Image {
    /// Encode to PNG bytes.
    pub fn to_png(&self) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        PngEncoder::new(&mut buf).write_image(&self.rgb, self.width, self.height, ExtendedColorType::Rgb8)?;
        Ok(buf)
    }
}

/// Map a VAE decoder output `[1, 3, H, W]` in `[-1, 1]` (CHW) to RGB8 (HWC).
pub fn vae_output_to_image(chw: &[f32], width: u32, height: u32) -> Image {
    let (w, h) = (width as usize, height as usize);
    let plane = w * h;
    let mut rgb = vec![0u8; plane * 3];
    for y in 0..h {
        for x in 0..w {
            let p = y * w + x;
            for c in 0..3 {
                let v = chw[c * plane + p];
                rgb[p * 3 + c] = ((v + 1.0) * 0.5).clamp(0.0, 1.0).mul_add(255.0, 0.5) as u8;
            }
        }
    }
    Image { width, height, rgb }
}

/// SD1.5 latent-to-RGB linear factors (ComfyUI), `[channel][rgb]`.
const LATENT_RGB: [[f32; 3]; 4] = [
    [0.3512, 0.2297, 0.3227],
    [0.3250, 0.4974, 0.2350],
    [-0.2829, 0.1762, 0.2721],
    [-0.2120, -0.2616, -0.7177],
];

/// Cheap RGB preview of a `[4, lh, lw]` latent (no VAE), for progress callbacks.
pub fn latent_preview(latent: &[f32], lw: usize, lh: usize) -> Image {
    let plane = lw * lh;
    let mut rgb = vec![0u8; plane * 3];
    for p in 0..plane {
        for c in 0..3 {
            let mut acc = 0.0f32;
            for ch in 0..4 {
                acc += latent[ch * plane + p] * LATENT_RGB[ch][c];
            }
            rgb[p * 3 + c] = (acc * 0.5 + 0.5).clamp(0.0, 1.0).mul_add(255.0, 0.5) as u8;
        }
    }
    Image { width: lw as u32, height: lh as u32, rgb }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vae_output_maps_range_to_bytes() {
        // 1x1 image, CHW = [r=-1, g=0, b=1]; 0 maps to round(0.5*255)=128.
        let img = vae_output_to_image(&[-1.0, 0.0, 1.0], 1, 1);
        assert_eq!(img.rgb, vec![0, 128, 255]);
    }

    #[test]
    fn vae_output_clamps_out_of_range() {
        let img = vae_output_to_image(&[-5.0, 5.0, 0.0], 1, 1);
        assert_eq!(img.rgb, vec![0, 255, 128]);
    }

    #[test]
    fn latent_preview_has_expected_size() {
        let latent = vec![0.0f32; 4 * 8 * 8];
        let img = latent_preview(&latent, 8, 8);
        assert_eq!(img.width, 8);
        assert_eq!(img.height, 8);
        assert_eq!(img.rgb.len(), 8 * 8 * 3);
    }

    #[test]
    fn png_encode_has_signature() {
        let img = Image { width: 1, height: 1, rgb: vec![10, 20, 30] };
        let png = img.to_png().unwrap();
        assert_eq!(&png[0..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
    }
}
