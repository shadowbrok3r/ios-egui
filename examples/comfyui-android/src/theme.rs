//! The "mastertech" color scheme, ported from a desktop egui `Style` dump.
//!
//! egui here has no `serde` feature, so the `Style` can't be deserialized directly (and would be
//! version-fragile if it could). The color values below are transcribed from
//! `mastertech_color_scheme.json`: a near-black theme with purple hovers, magenta actives, cyan
//! warnings and pink errors. Spacing stays touch-sized rather than the source's desktop density.

use egui::containers::scroll_area::ScrollBarVisibility;
use egui::{Color32, CornerRadius, FontFamily, FontId, Stroke, TextStyle};

use crate::types::FontSizes;

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
    // The page stays near-black; windows/menus sit a step brighter so they read as raised.
    v.panel_fill = rgb(8, 8, 11);
    v.window_fill = rgb(26, 26, 32);
    v.window_stroke = Stroke::new(1.0, rgba(70, 72, 96, 180));
    v.faint_bg_color = rgb(20, 20, 26);
    v.extreme_bg_color = rgb(13, 13, 18);
    v.code_bg_color = rgb(6, 6, 6);
    v.hyperlink_color = rgb(140, 128, 255);
    v.warn_fg_color = rgb(76, 219, 255);
    v.error_fg_color = rgb(255, 73, 137);
    v.selection.bg_fill = rgba(120, 70, 150, 150);
    v.selection.stroke = Stroke::new(1.0, rgba(150, 130, 220, 255));
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

    // Interactive widgets carry a visible border so they stand out against the dark page.
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = rgb(20, 20, 26);
    w.noninteractive.weak_bg_fill = rgb(20, 20, 26);
    w.noninteractive.bg_stroke = Stroke::new(1.0, rgba(66, 68, 90, 160));
    w.noninteractive.fg_stroke = Stroke::new(1.0, text);
    w.noninteractive.corner_radius = CornerRadius::same(3);

    w.inactive.bg_fill = rgb(30, 30, 38);
    w.inactive.weak_bg_fill = rgb(22, 22, 28);
    w.inactive.bg_stroke = Stroke::new(1.0, rgba(104, 108, 148, 210));
    w.inactive.fg_stroke = Stroke::new(1.0, text);
    w.inactive.corner_radius = CornerRadius::same(3);

    w.hovered.bg_fill = rgb(46, 42, 66);
    w.hovered.weak_bg_fill = rgb(46, 42, 66);
    w.hovered.bg_stroke = Stroke::new(1.2, rgba(150, 140, 226, 255));
    w.hovered.fg_stroke = Stroke::new(1.5, rgb(245, 245, 245));
    w.hovered.corner_radius = CornerRadius::same(3);

    w.active.bg_fill = rgb(70, 34, 74);
    w.active.weak_bg_fill = rgba(150, 46, 92, 200);
    w.active.bg_stroke = Stroke::new(1.4, rgba(190, 120, 210, 255));
    w.active.fg_stroke = Stroke::new(2.0, rgb(255, 255, 255));
    w.active.corner_radius = CornerRadius::same(3);

    w.open.bg_fill = rgb(30, 30, 38);
    w.open.weak_bg_fill = rgb(30, 30, 38);
    w.open.bg_stroke = Stroke::new(1.0, rgba(120, 116, 160, 230));
    w.open.corner_radius = CornerRadius::same(3);

    v.striped = true;
    ctx.set_visuals(v);

    ctx.all_styles_mut(|s| {
        // Modest density from the source theme, but touch targets stay usable (the desktop dump's
        // 1px button padding / 18px interact size would be too small to tap reliably).
        s.spacing.item_spacing = egui::vec2(6.0, 6.0);
        s.spacing.button_padding = egui::vec2(8.0, 6.0);
        // Solid (non-floating) bars — wider for touch; visibility is per-ScrollArea below.
        let mut scroll = egui::style::ScrollStyle::solid();
        scroll.bar_width = 14.0;
        scroll.handle_min_length = 28.0;
        scroll.bar_inner_margin = 2.0;
        s.spacing.scroll = scroll;
    });
}

/// Apply persisted font sizes onto egui's text styles.
pub fn apply_fonts(ctx: &egui::Context, fonts: &FontSizes) {
    let styles = [
        (TextStyle::Heading, FontId::new(fonts.heading, FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(fonts.body, FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(fonts.button, FontFamily::Proportional)),
        (TextStyle::Small, FontId::new(fonts.small, FontFamily::Proportional)),
        (TextStyle::Monospace, FontId::new(fonts.monospace, FontFamily::Monospace)),
    ];
    ctx.all_styles_mut(|s| {
        for (style, id) in &styles {
            s.text_styles.insert(style.clone(), id.clone());
        }
    });
}

/// Vertical scroll area; scrollbar only when content overflows.
pub fn scroll_vertical() -> egui::ScrollArea {
    egui::ScrollArea::vertical().scroll_bar_visibility(ScrollBarVisibility::VisibleWhenNeeded)
}

/// Bidirectional scroll area; scrollbars only when content overflows.
pub fn scroll_both() -> egui::ScrollArea {
    egui::ScrollArea::both().scroll_bar_visibility(ScrollBarVisibility::VisibleWhenNeeded)
}

/// Horizontal scroll area; scrollbar only when content overflows.
pub fn scroll_horizontal() -> egui::ScrollArea {
    egui::ScrollArea::horizontal().scroll_bar_visibility(ScrollBarVisibility::VisibleWhenNeeded)
}
