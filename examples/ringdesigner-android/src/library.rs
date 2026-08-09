//! The alpha library and the design library.
//!
//! The 16 procedural patterns have always existed in `AlphaLibrary::builtin()`; until now there was
//! no way to see or use them on the phone. This is that: a thumbnail grid, a tap to put one on the
//! band, and the controls that decide how it sits there.
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
}

impl Thumbs {
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
    }

    pub fn clear(&mut self) {
        self.cache.clear();
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
) -> Pick {
    let mut pick = Pick::None;
    let names: Vec<String> = lib
        .names()
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
                    let resp = cell(ui, lib, thumbs, name, is_sel);
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
) -> egui::Response {
    let size = egui::vec2(THUMB_PT, THUMB_PT + 16.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    if !ui.is_rect_visible(rect) {
        return resp;
    }
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
    let border = if selected {
        egui::Stroke::new(2.0, egui::Color32::from_rgb(203, 166, 247))
    } else if resp.hovered() {
        egui::Stroke::new(1.0, egui::Color32::from_rgb(120, 124, 136))
    } else {
        egui::Stroke::new(1.0, egui::Color32::from_rgb(58, 60, 68))
    };
    painter.rect_stroke(img_rect, 3.0, border, egui::StrokeKind::Inside);

    painter.text(
        egui::pos2(rect.center().x, rect.max.y - 2.0),
        egui::Align2::CENTER_BOTTOM,
        elide(name, 12),
        egui::FontId::proportional(10.0),
        egui::Color32::from_rgb(170, 173, 182),
    );
    resp
}

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

    #[test]
    fn the_builtin_library_is_not_empty() {
        let lib = AlphaLibrary::builtin();
        assert!(lib.len() >= 16, "expected the 16 procedurals, got {}", lib.len());
    }
}
