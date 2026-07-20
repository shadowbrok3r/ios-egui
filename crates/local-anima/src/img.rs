//! VAE decoder output to RGB8 and PNG.

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

    /// Write a PNG to `path`.
    pub fn save_png(&self, path: impl AsRef<std::path::Path>) -> Result<()> {
        std::fs::write(path, self.to_png()?)?;
        Ok(())
    }
}

/// Map a decoder output `[1, 3, H, W]` in `[-1, 1]` (CHW) to RGB8 (HWC).
pub fn vae_output_to_image(chw: &[f32], width: u32, height: u32) -> Image {
    let (w, h) = (width as usize, height as usize);
    let plane = w * h;
    let mut rgb = vec![0u8; plane * 3];
    for p in 0..plane {
        for c in 0..3 {
            let v = chw.get(c * plane + p).copied().unwrap_or(0.0);
            rgb[p * 3 + c] = ((v + 1.0) * 0.5).clamp(0.0, 1.0).mul_add(255.0, 0.5) as u8;
        }
    }
    Image { width, height, rgb }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_range_to_bytes() {
        let img = vae_output_to_image(&[-1.0, 0.0, 1.0], 1, 1);
        assert_eq!(img.rgb, vec![0, 128, 255]);
    }

    #[test]
    fn clamps_out_of_range() {
        assert_eq!(vae_output_to_image(&[-5.0, 5.0, 0.0], 1, 1).rgb, vec![0, 255, 128]);
    }

    #[test]
    fn deinterleaves_chw_planes_to_hwc() {
        // 2x1 image: R plane [-1, 1], G plane [1, -1], B plane [0, 0].
        let chw = [-1.0, 1.0, 1.0, -1.0, 0.0, 0.0];
        let img = vae_output_to_image(&chw, 2, 1);
        assert_eq!(img.rgb, vec![0, 255, 128, 255, 0, 128]);
    }

    #[test]
    fn output_buffer_is_three_bytes_per_pixel() {
        let img = vae_output_to_image(&vec![0.0f32; 3 * 16 * 8], 16, 8);
        assert_eq!(img.rgb.len(), 16 * 8 * 3);
        assert_eq!((img.width, img.height), (16, 8));
    }

    #[test]
    fn png_encode_has_signature() {
        let png = Image { width: 1, height: 1, rgb: vec![10, 20, 30] }.to_png().unwrap();
        assert_eq!(&png[0..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
    }
}
