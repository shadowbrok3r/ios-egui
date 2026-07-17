//! The "mastertech" color scheme, ported from a desktop egui `Style` dump.
//!
//! egui here has no `serde` feature, so the `Style` can't be deserialized directly (and would be
//! version-fragile if it could). The color values below are transcribed from
//! `mastertech_color_scheme.json`: a near-black theme with purple hovers, magenta actives, cyan
//! warnings and pink errors. Spacing stays touch-sized rather than the source's desktop density.

use egui::{Color32, CornerRadius, Stroke};

fn rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

fn rgba(r: u8, g: u8, b: u8, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(r, g, b, a)
}

/// Apply the theme to a context: dark, near-black, purple/magenta accented.
pub fn apply(ctx: &egui::Context) {
    let text = rgb(232, 232, 232);
    let mut v = egui::Visuals::dark();

    v.override_text_color = Some(text);
    v.panel_fill = rgb(0, 0, 0);
    v.window_fill = rgb(0, 0, 0);
    v.window_stroke = Stroke::new(1.0, rgba(24, 24, 34, 73));
    v.faint_bg_color = rgb(16, 16, 16);
    v.extreme_bg_color = rgb(13, 13, 18);
    v.code_bg_color = rgb(6, 6, 6);
    v.hyperlink_color = rgb(84, 71, 226);
    v.warn_fg_color = rgb(76, 219, 255);
    v.error_fg_color = rgb(255, 73, 137);
    v.selection.bg_fill = rgba(108, 60, 118, 118);
    v.selection.stroke = Stroke::new(1.0, rgba(76, 77, 103, 247));
    v.window_shadow = egui::epaint::Shadow {
        offset: [0, 0],
        blur: 5,
        spread: 7,
        color: rgba(2, 2, 2, 164),
    };
    v.popup_shadow = egui::epaint::Shadow {
        offset: [0, 0],
        blur: 5,
        spread: 5,
        color: rgba(0, 0, 0, 96),
    };
    v.window_corner_radius = CornerRadius::same(4);
    v.menu_corner_radius = CornerRadius::same(6);

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = rgb(6, 6, 6);
    w.noninteractive.weak_bg_fill = rgb(6, 6, 6);
    w.noninteractive.bg_stroke = Stroke::new(1.0, rgba(17, 17, 21, 87));
    w.noninteractive.fg_stroke = Stroke::new(1.0, text);
    w.noninteractive.corner_radius = CornerRadius::same(2);

    w.inactive.bg_fill = rgb(12, 12, 12);
    w.inactive.weak_bg_fill = rgb(0, 0, 0);
    w.inactive.bg_stroke = Stroke::new(0.6, rgba(50, 52, 77, 129));
    w.inactive.fg_stroke = Stroke::new(1.0, text);
    w.inactive.corner_radius = CornerRadius::same(2);

    w.hovered.bg_fill = rgb(7, 7, 7);
    w.hovered.weak_bg_fill = rgb(36, 34, 53);
    w.hovered.bg_stroke = Stroke::new(0.5, rgba(116, 109, 187, 218));
    w.hovered.fg_stroke = Stroke::new(1.5, text);
    w.hovered.corner_radius = CornerRadius::same(3);

    w.active.bg_fill = rgb(0, 0, 0);
    w.active.weak_bg_fill = rgba(118, 26, 60, 118);
    w.active.bg_stroke = Stroke::new(1.0, rgb(11, 11, 11));
    w.active.fg_stroke = Stroke::new(2.0, text);
    w.active.corner_radius = CornerRadius::same(2);

    w.open.bg_fill = rgb(3, 3, 3);
    w.open.weak_bg_fill = rgb(3, 3, 3);
    w.open.bg_stroke = Stroke::new(1.0, rgba(48, 47, 64, 221));
    w.open.corner_radius = CornerRadius::same(2);

    v.striped = true;
    ctx.set_visuals(v);

    ctx.all_styles_mut(|s| {
        // Modest density from the source theme, but touch targets stay usable (the desktop dump's
        // 1px button padding / 18px interact size would be too small to tap reliably).
        s.spacing.item_spacing = egui::vec2(6.0, 6.0);
        s.spacing.button_padding = egui::vec2(8.0, 6.0);
    });
}
