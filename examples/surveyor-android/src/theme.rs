//! Galactic-neon theme: vibrant instrument color on near-black glass.
//!
//! The structural grammar is borrowed from comfyui-android — every floating
//! surface is translucent tinted glass, pane edges are dim WHITE hairlines
//! (real glass has no color at its edge), and the black page is lit from
//! beneath by wide, very-low-alpha light pools so the glass has something to
//! catch. The palette is this app's own: hot pink (live/sweep/pressed), aqua
//! (radar returns + hover), electric violet (CSI/info) on deep-space black.

use egui::{Color32, Stroke};

// ── Palette ──────────────────────────────────────────────────────────────
pub const PINK: Color32 = Color32::from_rgb(255, 64, 170);
pub const PINK_BRIGHT: Color32 = Color32::from_rgb(255, 138, 208);
pub const AQUA: Color32 = Color32::from_rgb(64, 230, 220);
pub const AQUA_BRIGHT: Color32 = Color32::from_rgb(148, 246, 240);
pub const VIOLET: Color32 = Color32::from_rgb(178, 140, 255);
pub const VIOLET_BRIGHT: Color32 = Color32::from_rgb(208, 184, 255);
/// Body text: near-white with a hint of violet cast.
pub const INK: Color32 = Color32::from_rgb(238, 231, 242);
/// Pane edges: dim white hairlines, never colored.
pub const RIM: Color32 = Color32::from_rgba_premultiplied(46, 46, 52, 46);
pub const RIM_BRIGHT: Color32 = Color32::from_rgba_premultiplied(72, 72, 80, 72);

fn glass(r: u8, g: u8, b: u8, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(r, g, b, a)
}

/// Widget rest fill: violet-cast translucent glass. Translucency is
/// load-bearing — an opaque fill punches a matte hole through the frost.
fn fill_rest() -> Color32 {
    glass(26, 18, 40, 165)
}
fn fill_weak() -> Color32 {
    glass(20, 15, 32, 150)
}
fn fill_hover() -> Color32 {
    glass(64, 230, 220, 40)
}
fn fill_active() -> Color32 {
    glass(255, 64, 170, 54)
}

pub fn apply(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();

    // Near-black but not opaque: ambience() paints beneath every panel.
    v.panel_fill = glass(0, 0, 0, 232);
    v.window_fill = glass(16, 12, 26, 120);
    v.extreme_bg_color = Color32::from_rgb(9, 7, 14);
    v.faint_bg_color = Color32::from_rgb(12, 10, 18);
    v.code_bg_color = Color32::from_rgb(7, 5, 11);
    v.window_stroke = Stroke::new(1.0, RIM_BRIGHT);
    v.window_corner_radius = 10.0.into();
    v.menu_corner_radius = 10.0.into();

    v.override_text_color = Some(INK);
    v.hyperlink_color = AQUA;
    v.selection.bg_fill = glass(255, 64, 170, 110);
    v.selection.stroke = Stroke::new(1.2, PINK_BRIGHT);

    v.widgets.noninteractive.bg_fill = Color32::TRANSPARENT;
    v.widgets.noninteractive.weak_bg_fill = Color32::TRANSPARENT;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, RIM);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, INK);

    v.widgets.inactive.bg_fill = fill_rest();
    v.widgets.inactive.weak_bg_fill = fill_weak();
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, RIM_BRIGHT);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, INK);

    v.widgets.hovered.bg_fill = fill_hover();
    v.widgets.hovered.weak_bg_fill = fill_hover();
    v.widgets.hovered.bg_stroke = Stroke::new(1.5, glass(64, 230, 220, 235));
    v.widgets.hovered.fg_stroke = Stroke::new(1.2, AQUA_BRIGHT);

    v.widgets.active.bg_fill = fill_active();
    v.widgets.active.weak_bg_fill = fill_active();
    v.widgets.active.bg_stroke = Stroke::new(1.7, glass(255, 64, 170, 245));
    v.widgets.active.fg_stroke = Stroke::new(1.3, Color32::WHITE);

    v.widgets.open.bg_fill = fill_rest();
    v.widgets.open.weak_bg_fill = fill_rest();
    v.widgets.open.bg_stroke = Stroke::new(1.3, glass(178, 140, 255, 205));
    v.widgets.open.fg_stroke = Stroke::new(1.2, VIOLET);

    ctx.set_visuals(v);

    ctx.all_styles_mut(|s| {
        s.spacing.button_padding = egui::vec2(12.0, 8.0);
        s.spacing.item_spacing = egui::vec2(8.0, 8.0);
        s.spacing.interact_size.y = 40.0;
    });
}

/// Wide, very-low-alpha light pools beneath everything: the black page is lit
/// so the glass has something to catch. egui has no radial gradient — each
/// pool is 16 concentric filled circles at alpha 1, cheap and smooth.
pub fn ambience(ctx: &egui::Context) {
    let rect = ctx.content_rect();
    let min_dim = rect.width().min(rect.height());
    egui::Area::new(egui::Id::new("surveyor-ambience"))
        .order(egui::Order::Background)
        .fixed_pos(rect.min)
        .interactable(false)
        .show(ctx, |ui| {
            let painter = ui.painter();
            let at = |fx: f32, fy: f32| rect.min + egui::vec2(rect.width() * fx, rect.height() * fy);
            light_pool(painter, at(0.10, 0.10), min_dim * 0.46, PINK);
            light_pool(painter, at(0.94, 0.32), min_dim * 0.36, AQUA);
            light_pool(painter, at(0.52, 0.96), min_dim * 0.42, VIOLET);
            ui.allocate_space(egui::Vec2::ZERO);
        });
}

fn light_pool(painter: &egui::Painter, center: egui::Pos2, radius: f32, color: Color32) {
    for i in (1..=16).rev() {
        let r = radius * i as f32 / 16.0;
        painter.circle_filled(center, r, glass(color.r(), color.g(), color.b(), 1));
    }
}

/// Card frame for content sections: violet glass, white rim.
pub fn card() -> egui::Frame {
    egui::Frame::new()
        .fill(glass(20, 15, 34, 140))
        .stroke(Stroke::new(1.0, RIM))
        .corner_radius(10.0)
        .inner_margin(12.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every floating fill must be translucent, or it punches a matte hole
    /// through the frost.
    #[test]
    fn glass_fills_are_translucent() {
        for c in [fill_rest(), fill_weak(), fill_hover(), fill_active()] {
            assert!(c.a() > 0 && c.a() < 255, "{c:?}");
        }
    }

    /// Surfaces carry the violet cast: b strictly dominant, r over g.
    #[test]
    fn surfaces_are_violet_cast() {
        for c in [fill_rest(), fill_weak()] {
            assert!(c.b() > c.r() && c.r() > c.g(), "{c:?}");
        }
    }
}
