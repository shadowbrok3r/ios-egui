//! Galactic-glass theme: an AMOLED-black page, violet glass surfaces, two neon accents.
//!
//! The grammar is comfyui-android's, ported the same way surveyor and plugins ported it. Hot pink
//! is the primary *interaction* signal (selected / pressed / active), aqua is the secondary
//! (hover, links, live markers), and violet is not a signal at all — it is the ambient light every
//! surface is tinted by, which is why it can be everywhere without competing with the other two.
//!
//! Pane edges are dim WHITE hairlines. A hued outline is what stops a surface reading as glass:
//! real glass has no colour at its edge, only light catching the bevel.
//!
//! Surfaces are translucent and the page is lit from beneath by [`ambience`], so panes read as lit
//! glass rather than flat dark plastic. This app is on the Glow backend, so [`crate::frost`] adds
//! the real backdrop blur behind the bottom chrome on top of the simulated glass.
//!
//! Glyphs stay ASCII plus mainstream emoji — no icon font is loaded into this Context, so anything
//! outside egui's default coverage renders as a tofu box.

use egui_mobile::egui;

use egui::containers::scroll_area::ScrollBarVisibility;
use egui::{Color32, CornerRadius, Stroke};

/// Primary accent — hot pink. Reserved for what is active or chosen.
pub const PINK: Color32 = Color32::from_rgb(255, 61, 139);
/// A lifted pink for ink and rings, where the base pink reads dim on pure black.
pub const PINK_BRIGHT: Color32 = Color32::from_rgb(255, 110, 168);
/// Secondary accent — aqua. Hover feedback, links, and live/ready markers.
pub const AQUA: Color32 = Color32::from_rgb(43, 226, 214);
/// A lifted aqua for text.
pub const AQUA_BRIGHT: Color32 = Color32::from_rgb(120, 240, 232);
/// Third colour — violet. Ambient light, not an interaction state.
pub const VIOLET: Color32 = Color32::from_rgb(163, 140, 255);
/// Body ink — cool near-white.
pub const INK: Color32 = Color32::from_rgb(233, 233, 239);
/// Dimmed ink for secondary lines (readouts, hints).
pub const INK_DIM: Color32 = Color32::from_rgb(150, 148, 162);
/// A pane edge: a dim white hairline, never a coloured one.
pub const RIM: Color32 = Color32::from_rgba_premultiplied(46, 46, 52, 46);
/// A brighter hairline for surfaces meant to sit closest to the eye.
pub const RIM_BRIGHT: Color32 = Color32::from_rgba_premultiplied(72, 72, 80, 72);

fn glass(r: u8, g: u8, b: u8, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(r, g, b, a)
}

/// Widget rest fill: violet-cast translucent glass, raised to compensate for alpha over black.
fn fill_rest() -> Color32 {
    glass(31, 28, 47, 165)
}
fn fill_weak() -> Color32 {
    glass(25, 23, 38, 150)
}
fn fill_hover() -> Color32 {
    glass(43, 226, 214, 42)
}
fn fill_active() -> Color32 {
    glass(255, 61, 139, 54)
}

/// The widget states, shared between [`apply`] and [`menu_row_style`] — egui's
/// `menu_style` strips the rest fills and accent rims, so popups re-apply this.
fn widget_palette(w: &mut egui::style::Widgets) {
    let radius = CornerRadius::same(5);

    w.noninteractive.bg_fill = glass(18, 16, 28, 132);
    w.noninteractive.weak_bg_fill = glass(14, 12, 22, 120);
    w.noninteractive.bg_stroke = Stroke::new(1.0, RIM);
    w.noninteractive.fg_stroke = Stroke::new(1.0, INK);
    w.noninteractive.corner_radius = radius;

    // Translucent on purpose: these fills sit inside the panes the frost blurs, and an opaque
    // fill punches a matte hole straight through the glass it stands on.
    w.inactive.bg_fill = fill_rest();
    w.inactive.weak_bg_fill = fill_weak();
    w.inactive.bg_stroke = Stroke::new(1.0, RIM_BRIGHT);
    w.inactive.fg_stroke = Stroke::new(1.0, INK);
    w.inactive.corner_radius = radius;

    w.hovered.bg_fill = fill_hover();
    w.hovered.weak_bg_fill = fill_hover();
    w.hovered.bg_stroke = Stroke::new(1.5, glass(43, 226, 214, 240));
    w.hovered.fg_stroke = Stroke::new(1.5, AQUA_BRIGHT);
    w.hovered.corner_radius = radius;

    w.active.bg_fill = fill_active();
    w.active.weak_bg_fill = fill_active();
    w.active.bg_stroke = Stroke::new(1.7, glass(255, 61, 139, 245));
    w.active.fg_stroke = Stroke::new(2.0, Color32::WHITE);
    w.active.corner_radius = radius;

    w.open.bg_fill = fill_rest();
    w.open.weak_bg_fill = fill_weak();
    w.open.bg_stroke = Stroke::new(1.3, glass(43, 226, 214, 205));
    w.open.fg_stroke = Stroke::new(1.0, INK);
    w.open.corner_radius = radius;
}

pub fn apply(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();

    // Not fully opaque: `ambience` paints below every panel and a solid page would hide it.
    v.panel_fill = glass(0, 0, 0, 232);
    v.window_fill = glass(19, 17, 30, 232);
    v.extreme_bg_color = Color32::from_rgb(8, 7, 13);
    v.faint_bg_color = Color32::from_rgb(11, 10, 16);
    v.code_bg_color = Color32::from_rgb(6, 5, 10);
    v.window_stroke = Stroke::new(1.2, RIM_BRIGHT);
    v.window_corner_radius = CornerRadius::same(8);
    v.menu_corner_radius = CornerRadius::same(8);
    v.window_shadow =
        egui::epaint::Shadow { offset: [0, 2], blur: 12, spread: 2, color: glass(0, 0, 0, 200) };
    v.popup_shadow =
        egui::epaint::Shadow { offset: [0, 2], blur: 10, spread: 1, color: glass(0, 0, 0, 170) };

    v.override_text_color = Some(INK);
    v.hyperlink_color = AQUA;
    v.warn_fg_color = AQUA_BRIGHT;
    v.error_fg_color = PINK;
    v.selection.bg_fill = glass(255, 61, 139, 140);
    v.selection.stroke = Stroke::new(1.4, PINK_BRIGHT);

    widget_palette(&mut v.widgets);

    v.striped = true;
    v.collapsing_header_frame = true;
    v.indent_has_left_vline = true;
    ctx.set_visuals(v);

    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = egui::vec2(6.0, 6.0);
        s.spacing.button_padding = egui::vec2(10.0, 7.0);
        s.spacing.interact_size.y = 34.0;
        let mut scroll = egui::style::ScrollStyle::solid();
        scroll.bar_width = 14.0;
        scroll.handle_min_length = 28.0;
        scroll.bar_inner_margin = 2.0;
        s.spacing.scroll = scroll;
    });
}

/// Wide, very-low-alpha light pools beneath everything, so the glass has something to catch.
///
/// egui has no radial gradient; each pool is 16 concentric filled circles at alpha 1, which
/// composites to a smooth falloff. The radii keep black between the pools on purpose — light
/// everywhere is just a tinted page.
pub fn ambience(ctx: &egui::Context) {
    let rect = ctx.content_rect();
    let d = rect.width().min(rect.height()).max(1.0);
    egui::Area::new(egui::Id::new("ringdesigner-ambience"))
        .order(egui::Order::Background)
        .fixed_pos(rect.min)
        .movable(false)
        .interactable(false)
        .show(ctx, |ui| {
            ui.set_clip_rect(rect);
            let painter = ui.painter();
            for (fx, fy, fr, color) in [
                (0.12, 0.14, 0.46, VIOLET),
                (0.94, 0.38, 0.38, AQUA),
                (0.46, 0.97, 0.42, PINK),
            ] {
                light_pool(painter, rect.lerp_inside(egui::vec2(fx, fy)), d * fr, color);
            }
            ui.allocate_space(egui::Vec2::ZERO);
        });
}

fn light_pool(painter: &egui::Painter, center: egui::Pos2, radius: f32, color: Color32) {
    const RINGS: usize = 16;
    let fill = glass(color.r(), color.g(), color.b(), 1);
    for i in 0..RINGS {
        painter.circle_filled(center, radius * (1.0 - i as f32 / RINGS as f32), fill);
    }
}

/// Card frame for a content section: violet glass under a white rim.
pub fn card() -> egui::Frame {
    egui::Frame::new()
        .fill(glass(20, 18, 34, 140))
        .stroke(Stroke::new(1.0, RIM))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(12)
}

/// Frame for a bar pinned to a screen edge. Kept transparent where the frost
/// runs — the grab-pass tint IS the bar — with just the tap-height margin.
pub fn bar() -> egui::Frame {
    egui::Frame::new().inner_margin(egui::Margin::symmetric(8, 6))
}

/// Selectable button that keeps its frame when unselected — egui drops it, which reads as a plain
/// label rather than a control.
pub fn selectable<'a>(selected: bool, atoms: impl egui::IntoAtoms<'a>) -> egui::Button<'a> {
    egui::Button::selectable(selected, atoms).frame_when_inactive(true)
}

/// [`egui::Ui::selectable_label`] at a fixed size, with a persistent frame plus a neon pink rim
/// when selected — egui's `interact_selectable` leaves `bg_stroke` off for the selected state.
pub fn selectable_label<'a>(
    ui: &mut egui::Ui,
    selected: bool,
    size: impl Into<egui::Vec2>,
    text: impl egui::IntoAtoms<'a>,
) -> egui::Response {
    let resp = ui.add_sized(size, selectable(selected, text));
    if selected {
        ui.painter().rect_stroke(
            resp.rect,
            CornerRadius::same(5),
            Stroke::new(1.6, PINK),
            egui::StrokeKind::Inside,
        );
    }
    resp
}

/// Vertical scroll area; scrollbar only when content overflows.
pub fn scroll_vertical() -> egui::ScrollArea {
    egui::ScrollArea::vertical().scroll_bar_visibility(ScrollBarVisibility::VisibleWhenNeeded)
}

// --- Menus -------------------------------------------------------------------

/// Tap height for a menu row — a framed 40px target rather than egui's 18px text line.
pub const MENU_ROW_H: f32 = 40.0;

/// Give a menu's rows a framed, touch-sized look: egui's `menu_style` strips the rest-state fill
/// and every accent rim and squashes `button_padding` to 2×0, so an entry otherwise reads as bare
/// text on an 18px line. Call once at the top of a popup body; children inherit it.
pub fn menu_row_style(ui: &mut egui::Ui) {
    let s = ui.style_mut();
    s.spacing.button_padding = egui::vec2(10.0, 8.0);
    s.spacing.interact_size.y = MENU_ROW_H;
    s.spacing.item_spacing.y = 4.0;
    widget_palette(&mut s.visuals.widgets);
}

/// A menu / combo-box button whose popup opens *upward* and scrolls.
///
/// `Ui::menu_button` only flips its popup above the button when it wouldn't otherwise fit — but
/// egui's screen rect extends under the Android navigation bar, so a short menu "fits" below and
/// ends up covering the nav bar and the system gesture area. Everything in a bottom control bar
/// uses this instead, which always prefers opening upward.
pub fn up_menu<R>(
    ui: &mut egui::Ui,
    label: impl Into<egui::WidgetText>,
    content: impl FnOnce(&mut egui::Ui) -> R,
) {
    let _ = menu_popup(
        ui,
        label,
        None,
        egui::RectAlign::TOP_START,
        &[egui::RectAlign::TOP_END, egui::RectAlign::BOTTOM_START],
        egui::PopupCloseBehavior::CloseOnClick,
        content,
    );
}

/// How tall a menu opened from `anchor` may grow: the room actually available on the side it opens
/// toward, less a small margin.
pub fn menu_height_cap(ctx: &egui::Context, anchor: egui::Rect, align: egui::RectAlign) -> f32 {
    let screen = ctx.content_rect();
    let down = anchor.bottom().max(screen.top());
    let below = screen.bottom() - down;
    let above = anchor.top().min(screen.bottom()) - screen.top();
    let (room, flipped) = match align.parent.y() {
        egui::Align::Max => (below, above),
        egui::Align::Min => (above, below),
        egui::Align::Center => (screen.height(), screen.height()),
    };
    // The floor is `min`'d rather than clamped: a landscape phone with the keyboard up leaves the
    // content rect under 160pt tall, and `f32::clamp` panics outright on min > max.
    let floor = 160.0_f32.min(screen.height());
    (room.max(flipped) - 24.0).clamp(floor, screen.height().max(floor))
}

/// How wide a popup may grow. egui clips a popup's painting at `content_rect` but never shrinks
/// it, so a row wider than the screen cuts the menu off at the right edge instead of wrapping.
pub fn menu_width_cap(ctx: &egui::Context) -> f32 {
    (ctx.content_rect().width() - 24.0).max(160.0)
}

/// A menu button whose popup is bounded to the screen on both axes.
pub fn menu_popup<R>(
    ui: &mut egui::Ui,
    label: impl Into<egui::WidgetText>,
    min_size: Option<egui::Vec2>,
    align: egui::RectAlign,
    alternatives: &'static [egui::RectAlign],
    close_behavior: egui::PopupCloseBehavior,
    content: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::Response {
    use egui::containers::menu::MenuConfig;
    let response = if let Some(size) = min_size {
        ui.add_sized(size, egui::Button::new(label.into()))
    } else {
        ui.add(egui::Button::new(label.into()))
    };
    let config = MenuConfig::default().close_behavior(close_behavior);
    let cap = menu_height_cap(ui.ctx(), response.rect, align);
    let width_cap = menu_width_cap(ui.ctx());
    egui::Popup::menu(&response)
        .align(align)
        .align_alternatives(alternatives)
        .gap(4.0)
        .close_behavior(config.close_behavior)
        .style(config.style.clone())
        .info(
            egui::UiStackInfo::new(egui::UiKind::Menu)
                .with_tag_value(MenuConfig::MENU_CONFIG_TAG, config),
        )
        .show(|ui| {
            // egui sizes a popup's Area on a one-off sizing pass seeded from
            // `spacing.default_area_size`, and that becomes the Ui's `max_rect` forever after.
            // Claiming the full cap during the sizing pass only lets the Area record a tall
            // enough size; the list still shrinks to its content afterwards.
            if ui.is_sizing_pass() {
                ui.set_min_height(cap);
            }
            // Clamped against the Area's own width so this only ever shrinks — handed the cap
            // outright, a two-item menu would stretch across the whole phone.
            let width = (width_cap - ui.spacing().menu_margin.sum().x).min(ui.max_rect().width());
            ui.set_max_width(width);
            scroll_vertical()
                .max_height(cap)
                .max_width(width)
                .show(ui, |ui| {
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                    menu_row_style(ui);
                    content(ui)
                })
                .inner
        });
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An opaque fill inside a frosted pane punches a matte hole through the glass.
    #[test]
    fn floating_surfaces_stay_translucent_so_the_blur_reads_through() {
        for fill in [fill_rest(), fill_weak(), fill_hover(), fill_active(), card().fill] {
            assert!(fill.a() < 255, "{fill:?} is opaque");
        }
    }

    /// Surfaces carry the violet ambient cast: blue over red over green.
    #[test]
    fn surfaces_carry_the_violet_cast() {
        for fill in [fill_rest(), fill_weak(), card().fill] {
            assert!(fill.b() > fill.r() && fill.r() > fill.g(), "{fill:?} lost the cast");
        }
    }
}
