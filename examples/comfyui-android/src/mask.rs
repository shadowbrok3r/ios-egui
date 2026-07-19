//! Inpaint mask canvas: feathered UV-space brush strokes baked into an image alpha channel. Pure.

use std::io::Cursor;

/// A grayscale mask, 0 (unmasked) .. 255 (fully masked), row-major `w`x`h`.
pub struct MaskCanvas {
    pub w: u32,
    pub h: u32,
    pub buf: Vec<u8>,
}

impl MaskCanvas {
    pub fn new(w: u32, h: u32) -> Self {
        Self { w, h, buf: vec![0u8; w as usize * h as usize] }
    }

    pub fn is_empty(&self) -> bool {
        self.buf.iter().all(|&v| v == 0)
    }

    /// Stamp feathered discs from `from` to `to` (uv 0..1) at ~radius/2 spacing.
    /// Paint max-blends toward `intensity`*255; erase min-blends toward 0. `soft` is the feather
    /// fraction; `intensity` (0..1) scales coverage for pressure-sensitive strokes.
    pub fn stroke(
        &mut self,
        from: (f32, f32),
        to: (f32, f32),
        radius_uv: f32,
        soft: f32,
        intensity: f32,
        erase: bool,
    ) {
        let r = radius_uv.max(0.0);
        if r <= 0.0 || self.w == 0 || self.h == 0 {
            return;
        }
        let (dx, dy) = (to.0 - from.0, to.1 - from.1);
        let len = (dx * dx + dy * dy).sqrt();
        let step = (r * 0.5).max(1e-4);
        let count = (len / step).ceil().max(1.0) as u32;
        for i in 0..=count {
            let t = i as f32 / count as f32;
            self.stamp(from.0 + dx * t, from.1 + dy * t, r, soft, intensity, erase);
        }
    }

    /// One feathered disc centered at (cx, cy) in uv space, coverage scaled by `intensity`.
    fn stamp(&mut self, cx: f32, cy: f32, r: f32, soft: f32, intensity: f32, erase: bool) {
        let (w, h) = (self.w as f32, self.h as f32);
        let k = intensity.clamp(0.0, 1.0);
        let inner = r * (1.0 - soft.clamp(0.0, 1.0));
        let min_x = (((cx - r) * w).floor() as i64).clamp(0, self.w as i64 - 1) as u32;
        let max_x = (((cx + r) * w).ceil() as i64).clamp(0, self.w as i64 - 1) as u32;
        let min_y = (((cy - r) * h).floor() as i64).clamp(0, self.h as i64 - 1) as u32;
        let max_y = (((cy + r) * h).ceil() as i64).clamp(0, self.h as i64 - 1) as u32;
        for py in min_y..=max_y {
            let v = (py as f32 + 0.5) / h;
            for px in min_x..=max_x {
                let u = (px as f32 + 0.5) / w;
                let d = ((u - cx).powi(2) + (v - cy).powi(2)).sqrt();
                if d > r {
                    continue;
                }
                let cov = if d <= inner || r <= inner {
                    1.0
                } else {
                    1.0 - (d - inner) / (r - inner)
                };
                let a = cov * k;
                let idx = py as usize * self.w as usize + px as usize;
                if erase {
                    self.buf[idx] = self.buf[idx].min(((1.0 - a) * 255.0).round() as u8);
                } else {
                    self.buf[idx] = self.buf[idx].max((a * 255.0).round() as u8);
                }
            }
        }
    }

    /// Bilinearly sampled mask intensity at uv (u, v).
    fn sample_bilinear(&self, u: f32, v: f32) -> u8 {
        if self.w == 0 || self.h == 0 {
            return 0;
        }
        let fx = (u * self.w as f32 - 0.5).clamp(0.0, (self.w - 1) as f32);
        let fy = (v * self.h as f32 - 0.5).clamp(0.0, (self.h - 1) as f32);
        let (x0, y0) = (fx.floor() as u32, fy.floor() as u32);
        let (x1, y1) = ((x0 + 1).min(self.w - 1), (y0 + 1).min(self.h - 1));
        let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
        let g = |x: u32, y: u32| self.buf[y as usize * self.w as usize + x as usize] as f32;
        let top = g(x0, y0) * (1.0 - tx) + g(x1, y0) * tx;
        let bot = g(x0, y1) * (1.0 - tx) + g(x1, y1) * tx;
        (top * (1.0 - ty) + bot * ty).round().clamp(0.0, 255.0) as u8
    }
}

/// A replayable stroke record; a mask is the deterministic rasterization of an ordered list.
pub struct StrokeRec {
    pub from: (f32, f32),
    pub to: (f32, f32),
    pub radius_uv: f32,
    pub soft: f32,
    pub intensity: f32,
    pub erase: bool,
}

/// Rasterize an ordered stroke list into a fresh canvas (undo = rasterize minus the last stroke).
pub fn rasterize(w: u32, h: u32, strokes: &[StrokeRec]) -> MaskCanvas {
    let mut canvas = MaskCanvas::new(w, h);
    for s in strokes {
        canvas.stroke(s.from, s.to, s.radius_uv, s.soft, s.intensity, s.erase);
    }
    canvas
}

/// A brush shaped by pointer pressure: `radius_uv` and mask `intensity` (0..1).
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Brush {
    pub radius_uv: f32,
    pub intensity: f32,
}

/// Shape `base_radius_uv` by pointer `force` (0..1, from `egui::Event::Touch`). No force keeps the
/// full radius and intensity; light contact paints thinner and fainter down to a fixed floor.
pub fn pressure_brush(base_radius_uv: f32, force: Option<f32>) -> Brush {
    match force {
        Some(f) => {
            let p = f.clamp(0.0, 1.0);
            Brush { radius_uv: base_radius_uv * (0.4 + 0.6 * p), intensity: 0.35 + 0.65 * p }
        }
        None => Brush { radius_uv: base_radius_uv, intensity: 1.0 },
    }
}

/// Pointer tool as seen by the paint surface, surfaced from the android-activity motion-event
/// side channel (`getToolType`). `Unknown` covers pre-first-event and non-touch pointers.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PointerKind {
    Finger,
    Stylus,
    Eraser,
    Palm,
    Mouse,
    Unknown,
}

/// Whether a paint stroke from `kind` is accepted under `stylus_only`. Rejects finger and palm
/// contacts; stylus, eraser, mouse, and unknown are accepted.
pub fn accept_paint(kind: PointerKind, stylus_only: bool) -> bool {
    if !stylus_only {
        return true;
    }
    !matches!(kind, PointerKind::Finger | PointerKind::Palm)
}

/// Decode `src_image` (PNG/JPEG), set each pixel's alpha to 255 minus the sampled mask, re-encode PNG.
pub fn bake_alpha(src_image: &[u8], mask: &MaskCanvas) -> Result<Vec<u8>, String> {
    let img = image::load_from_memory(src_image).map_err(|e| e.to_string())?;
    let mut rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    for y in 0..h {
        let v = (y as f32 + 0.5) / h as f32;
        for x in 0..w {
            let u = (x as f32 + 0.5) / w as f32;
            let m = mask.sample_bilinear(u, v);
            rgba.get_pixel_mut(x, y)[3] = 255u8 - m;
        }
    }
    let mut out = Vec::new();
    image::DynamicImage::ImageRgba8(rgba)
        .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(color: [u8; 3], w: u32, h: u32, fmt: image::ImageFormat) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(w, h, image::Rgb(color));
        let mut out = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut Cursor::new(&mut out), fmt)
            .unwrap();
        out
    }

    #[test]
    fn stroke_covers_center_and_erase_clears() {
        let mut c = MaskCanvas::new(16, 16);
        c.stroke((0.1, 0.5), (0.9, 0.5), 0.15, 0.3, 1.0, false);
        assert!(!c.is_empty());
        // A pixel on the stroke line is painted; a corner far away is not.
        assert!(c.buf[8 * 16 + 8] > 0);
        assert_eq!(c.buf[0], 0);
        // Erasing the same path clears the painted pixels.
        c.stroke((0.1, 0.5), (0.9, 0.5), 0.2, 0.0, 1.0, true);
        assert_eq!(c.buf[8 * 16 + 8], 0);
    }

    #[test]
    fn rasterize_is_deterministic() {
        let strokes = vec![
            StrokeRec { from: (0.2, 0.2), to: (0.8, 0.8), radius_uv: 0.2, soft: 0.4, intensity: 1.0, erase: false },
            StrokeRec { from: (0.8, 0.2), to: (0.2, 0.8), radius_uv: 0.1, soft: 0.2, intensity: 1.0, erase: false },
        ];
        let a = rasterize(24, 24, &strokes);
        let b = rasterize(24, 24, &strokes);
        assert_eq!(a.buf, b.buf);
        // Dropping the last stroke changes the result (undo semantics).
        let c = rasterize(24, 24, &strokes[..1]);
        assert_ne!(a.buf, c.buf);
    }

    #[test]
    fn intensity_scales_painted_value() {
        let c = 8 * 16 + 8;
        let mut full = MaskCanvas::new(16, 16);
        full.stroke((0.5, 0.5), (0.5, 0.5), 0.3, 0.0, 1.0, false);
        let mut half = MaskCanvas::new(16, 16);
        half.stroke((0.5, 0.5), (0.5, 0.5), 0.3, 0.0, 0.5, false);
        assert_eq!(full.buf[c], 255);
        // Half intensity halves the covered center value (soft 0 → coverage 1 * 0.5).
        assert_eq!(half.buf[c], 128);
    }

    #[test]
    fn pressure_brush_scales_with_force() {
        let base = 0.1;
        let none = pressure_brush(base, None);
        assert_eq!(none.radius_uv, base);
        assert_eq!(none.intensity, 1.0);
        let light = pressure_brush(base, Some(0.0));
        let hard = pressure_brush(base, Some(1.0));
        // Harder press → larger radius and higher intensity, bounded by base / full.
        assert!(light.radius_uv < hard.radius_uv);
        assert!(light.intensity < hard.intensity);
        assert!(light.radius_uv > 0.0 && hard.radius_uv <= base + 1e-6);
        assert!(hard.intensity <= 1.0 + 1e-6 && light.intensity >= 0.0);
        // Out-of-range force clamps to the full-press brush.
        assert_eq!(pressure_brush(base, Some(2.0)), hard);
    }

    #[test]
    fn stylus_only_gates_finger_and_palm() {
        use PointerKind::*;
        // Off: everything paints.
        assert!(accept_paint(Finger, false));
        assert!(accept_paint(Palm, false));
        // On: finger and palm rejected; stylus/eraser/mouse/unknown accepted.
        assert!(!accept_paint(Finger, true));
        assert!(!accept_paint(Palm, true));
        assert!(accept_paint(Stylus, true));
        assert!(accept_paint(Eraser, true));
        assert!(accept_paint(Mouse, true));
        assert!(accept_paint(Unknown, true));
    }

    #[test]
    fn bake_alpha_inverts_mask_over_png_and_jpeg() {
        // PNG in: left half masked -> left alpha 0, right alpha 255; RGB preserved.
        let png = encode([200, 40, 40], 8, 8, image::ImageFormat::Png);
        let mut mask = MaskCanvas::new(8, 8);
        mask.stroke((0.0, 0.5), (0.25, 0.5), 0.5, 0.0, 1.0, false);
        let baked = bake_alpha(&png, &mask).unwrap();
        assert_eq!(&baked[..4], &[0x89, 0x50, 0x4E, 0x47]);
        let out = image::load_from_memory(&baked).unwrap().to_rgba8();
        let left = out.get_pixel(0, 4);
        let right = out.get_pixel(7, 4);
        assert_eq!(left[3], 0);
        assert_eq!(right[3], 255);
        assert_eq!([left[0], left[1], left[2]], [200, 40, 40]);

        // JPEG in -> PNG out; empty mask leaves alpha opaque and color roughly preserved.
        let jpg = encode([30, 120, 200], 16, 16, image::ImageFormat::Jpeg);
        let baked = bake_alpha(&jpg, &MaskCanvas::new(16, 16)).unwrap();
        assert_eq!(&baked[..4], &[0x89, 0x50, 0x4E, 0x47]);
        let out = image::load_from_memory(&baked).unwrap().to_rgba8();
        let p = out.get_pixel(8, 8);
        assert_eq!(p[3], 255);
        assert!((p[0] as i32 - 30).abs() < 24 && (p[2] as i32 - 200).abs() < 24);
    }
}
