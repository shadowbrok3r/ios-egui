//! The alpha library and the design library.
//!
//! Every procedural pattern in `AlphaLibrary::builtin()` — 28 of them today — as a thumbnail grid,
//! a tap to put one on the band, and the controls that decide how it sits there.
//!
//! Each cell also carries what the sand will hold: the alpha's finest stroke or gap, measured by
//! granulometry and scaled to the cell the current repeat count would lay down. A pattern finer
//! than the detail floor is rimmed amber, so the refusal arrives before the pattern is applied
//! rather than as a DFM finding afterwards.
//!
//! Thumbnails are uploaded once and cached by name. The grid draws them at a few dozen points, so
//! a full-resolution texture per entry would waste both a large transient and the VRAM it lands in
//! — `Alpha::thumbnail_rgba8` downscales before the upload.

use std::collections::HashMap;

use egui_mobile::egui;
use ringdesign_core::alpha::AlphaLibrary;

/// Longest edge of an uploaded preview texture.
const THUMB_TEXTURE_EDGE: usize = 128;
/// Drawn size of a grid cell, in points. Comfortably over the 44 dp touch minimum.
const THUMB_PT: f32 = 76.0;

#[derive(Default)]
pub struct Thumbs {
    cache: HashMap<String, egui::TextureHandle>,
    /// Granulometry per alpha, in texels: `(finest ink, finest gap)`.
    ///
    /// `Alpha::min_feature_px` caches by content, but its key hashes every
    /// texel — a full grid of 256x256 alphas would pay that on every frame.
    /// Cached by name here, on the same lifecycle as the thumbnail.
    finest: HashMap<String, Option<(f64, f64)>>,
}

impl Thumbs {
    /// Finest ink and gap of `name` in texels, measured once.
    ///
    /// The alpha is measured unshaped: this reads a pattern before it is a
    /// layer, so there is no contrast, bias or invert to apply yet.
    /// `dfm::findings_in` measures the shaped mask once the layer exists.
    pub fn finest_px(&mut self, lib: &AlphaLibrary, name: &str) -> Option<(f64, f64)> {
        if let Some(hit) = self.finest.get(name) {
            return *hit;
        }
        let got = lib.get(name).and_then(|a| a.min_feature_px());
        self.finest.insert(name.to_string(), got);
        got
    }

    pub fn get(
        &mut self,
        ctx: &egui::Context,
        lib: &AlphaLibrary,
        name: &str,
    ) -> Option<egui::TextureId> {
        if let Some(t) = self.cache.get(name) {
            return Some(t.id());
        }
        let alpha = lib.get(name)?;
        let (w, h, bytes) = alpha.thumbnail_rgba8(THUMB_TEXTURE_EDGE);
        if w == 0 || h == 0 {
            return None;
        }
        let image = egui::ColorImage::from_rgba_unmultiplied([w, h], &bytes);
        let handle = ctx.load_texture(format!("alpha:{name}"), image, egui::TextureOptions::LINEAR);
        let id = handle.id();
        self.cache.insert(name.to_string(), handle);
        Some(id)
    }

    /// Drop a cached preview, so a regenerated or repainted alpha shows its new self.
    pub fn forget(&mut self, name: &str) {
        self.cache.remove(name);
        self.finest.remove(name);
    }

    pub fn clear(&mut self) {
        self.cache.clear();
        self.finest.clear();
    }
}

/// The cell a picked pattern would be laid down on, and the floor it is judged against.
///
/// Carries what turns texels into millimetres of metal, so the grid can say what
/// the sand will hold before a pattern is applied rather than after.
#[derive(Clone, Copy)]
pub struct CellScale {
    pub cell_w_mm: f64,
    pub cell_h_mm: f64,
    pub floor_mm: f64,
}

impl CellScale {
    /// Finest feature of an alpha in millimetres on this cell, and which of ink
    /// or gaps runs finest.
    ///
    /// The same law as `dfm::tiling_finest_mm`: the mask is fitted to the cell by
    /// whichever axis binds, so a wide cell on a short face still measures short.
    fn finest_mm(&self, alpha_w: usize, alpha_h: usize, px: (f64, f64)) -> Option<(f64, &'static str)> {
        let scale = (self.cell_w_mm / alpha_w.max(1) as f64).min(self.cell_h_mm / alpha_h.max(1) as f64);
        if !(scale.is_finite() && scale > 0.0) {
            return None;
        }
        let (ink, gap) = (px.0 * scale, px.1 * scale);
        Some(if ink <= gap { (ink, "strokes") } else { (gap, "gaps") })
    }
}

/// What the grid was asked to do.
pub enum Pick {
    None,
    /// Put this alpha on the band.
    Use(String),
    /// Show it larger.
    Preview(String),
}

/// A scrolling grid of every alpha in the library.
pub fn grid(
    ui: &mut egui::Ui,
    lib: &AlphaLibrary,
    thumbs: &mut Thumbs,
    selected: Option<&str>,
    filter: &str,
    scale: Option<CellScale>,
    // When set, show only these names in this order — the result of a
    // similarity query rather than the whole library.
    only: Option<&[String]>,
) -> Pick {
    let mut pick = Pick::None;
    let names: Vec<String> = match only {
        Some(order) => order.to_vec(),
        None => lib.names(),
    }
    .into_iter()
    .filter(|n| filter.is_empty() || n.to_lowercase().contains(&filter.to_lowercase()))
    .collect();

    if names.is_empty() {
        ui.label(egui::RichText::new("nothing matches").weak());
        return pick;
    }

    let avail = ui.available_width();
    let spacing = ui.spacing().item_spacing.x;
    let cols = (((avail + spacing) / (THUMB_PT + spacing)).floor() as usize).max(1);

    egui::ScrollArea::vertical().show(ui, |ui| {
        egui::Grid::new("alpha_grid").num_columns(cols).spacing([spacing, spacing]).show(
            ui,
            |ui| {
                for (i, name) in names.iter().enumerate() {
                    let is_sel = selected == Some(name.as_str());
                    let resp = cell(ui, lib, thumbs, name, is_sel, scale);
                    if resp.clicked() {
                        pick = Pick::Use(name.clone());
                    }
                    if resp.long_touched() {
                        pick = Pick::Preview(name.clone());
                    }
                    if (i + 1) % cols == 0 {
                        ui.end_row();
                    }
                }
            },
        );
    });
    pick
}

fn cell(
    ui: &mut egui::Ui,
    lib: &AlphaLibrary,
    thumbs: &mut Thumbs,
    name: &str,
    selected: bool,
    scale: Option<CellScale>,
) -> egui::Response {
    let size = egui::vec2(THUMB_PT, THUMB_PT + 27.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    // Measured only for cells actually on screen: the grid scrolls, and the
    // first measurement of an alpha walks its whole distance field.
    let sand = scale.and_then(|s| {
        let a = lib.get(name)?;
        let px = thumbs.finest_px(lib, name)?;
        let (finest, what) = s.finest_mm(a.width, a.height, px)?;
        Some((finest, what, finest < s.floor_mm))
    });
    let painter = ui.painter_at(rect);
    let img_rect =
        egui::Rect::from_min_size(rect.min, egui::vec2(THUMB_PT, THUMB_PT)).shrink(2.0);

    painter.rect_filled(img_rect, 3.0, egui::Color32::from_rgb(26, 27, 31));
    if let Some(id) = thumbs.get(ui.ctx(), lib, name) {
        painter.image(
            id,
            img_rect,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );
    }
    // A pattern the sand cannot hold gets the warning rim, so the grid reads
    // at a glance rather than one tap at a time.
    let border = if selected {
        egui::Stroke::new(2.0, egui::Color32::from_rgb(203, 166, 247))
    } else if sand.is_some_and(|(_, _, under)| under) {
        egui::Stroke::new(1.5, MUSH)
    } else if resp.hovered() {
        egui::Stroke::new(1.0, egui::Color32::from_rgb(120, 124, 136))
    } else {
        egui::Stroke::new(1.0, egui::Color32::from_rgb(58, 60, 68))
    };
    painter.rect_stroke(img_rect, 3.0, border, egui::StrokeKind::Inside);

    painter.text(
        egui::pos2(rect.center().x, rect.max.y - 13.0),
        egui::Align2::CENTER_BOTTOM,
        elide(name, 12),
        egui::FontId::proportional(10.0),
        egui::Color32::from_rgb(170, 173, 182),
    );
    if let Some((finest, what, under)) = sand {
        painter.text(
            egui::pos2(rect.center().x, rect.max.y - 1.0),
            egui::Align2::CENTER_BOTTOM,
            format!("{finest:.2} mm {what}"),
            egui::FontId::proportional(9.0),
            if under { MUSH } else { egui::Color32::from_rgb(130, 133, 142) },
        );
    }
    resp
}

/// Amber for a feature finer than the sand's detail floor — the theme's warning.
const MUSH: egui::Color32 = egui::Color32::from_rgb(220, 170, 90);

fn elide(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let keep: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{keep}…")
    }
}

/// Regenerate every procedural pattern at `size`, replacing the library's copies.
///
/// The builtins ship at 256. A tile that will be repeated twenty times round the ring never needs
/// more, but one drawn across the whole band does — and a coarse alpha stretched over 67 mm is
/// visibly stepped.
pub fn regenerate_builtins(lib: &mut AlphaLibrary, size: usize) {
    use ringdesign_core::alpha::Procedural;
    for p in Procedural::ALL {
        // `generate` already names the alpha after the pattern, so this replaces in place.
        lib.insert(p.generate(size));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_name_is_left_alone() {
        assert_eq!(elide("Rope", 12), "Rope");
    }

    #[test]
    fn a_long_name_is_cut_with_an_ellipsis() {
        let out = elide("Basketweave Extended", 12);
        assert_eq!(out.chars().count(), 12);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn eliding_counts_characters_not_bytes() {
        // A byte-based cut would split these and panic.
        let out = elide("ééééééééééééééé", 5);
        assert_eq!(out.chars().count(), 5);
    }

    #[test]
    fn regenerating_replaces_every_builtin_at_the_new_size() {
        use ringdesign_core::alpha::Procedural;
        let mut lib = AlphaLibrary::builtin();
        regenerate_builtins(&mut lib, 64);
        for p in Procedural::ALL {
            let a = lib.get(p.label()).expect("still present");
            assert_eq!((a.width, a.height), (64, 64), "{}", p.label());
        }
    }

    /// Pinned against `Procedural::ALL` rather than a literal, which is how the
    /// doc drifted to "16" while the core grew to 28.
    #[test]
    fn the_grid_shows_every_procedural() {
        use ringdesign_core::alpha::Procedural;
        let lib = AlphaLibrary::builtin();
        assert!(
            lib.len() >= Procedural::ALL.len(),
            "expected at least the {} procedurals, got {}",
            Procedural::ALL.len(),
            lib.len()
        );
    }

    /// The cell law is `dfm::tiling_finest_mm`'s: the mask fits by whichever
    /// axis binds, so a wide cell on a short face still measures short.
    #[test]
    fn the_binding_axis_decides_the_millimetres() {
        let wide = CellScale { cell_w_mm: 20.0, cell_h_mm: 2.0, floor_mm: 0.35 };
        let (finest, what) = wide.finest_mm(100, 100, (10.0, 40.0)).expect("measurable");
        // Height binds: 2.0 / 100 per texel, so 10 texels of ink is 0.2 mm.
        assert!((finest - 0.2).abs() < 1e-9, "{finest}");
        assert_eq!(what, "strokes");
        assert!(finest < wide.floor_mm, "and that is under the floor");
    }

    #[test]
    fn gaps_win_when_they_run_finer_than_the_ink() {
        let s = CellScale { cell_w_mm: 10.0, cell_h_mm: 10.0, floor_mm: 0.35 };
        let (_, what) = s.finest_mm(100, 100, (40.0, 10.0)).expect("measurable");
        assert_eq!(what, "gaps");
    }

    #[test]
    fn a_degenerate_cell_measures_nothing_rather_than_panicking() {
        let s = CellScale { cell_w_mm: 0.0, cell_h_mm: 0.0, floor_mm: 0.35 };
        assert!(s.finest_mm(100, 100, (10.0, 10.0)).is_none());
    }
}
