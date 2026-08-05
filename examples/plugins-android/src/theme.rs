//! Galactic-glass theme: an AMOLED-black page, violet glass surfaces, two neon accents.
//!
//! The grammar is comfyui-android's, ported the same way surveyor-android ported it. Hot pink is
//! the primary *interaction* signal (selected / pressed / active), aqua is the secondary (hover,
//! links, live markers), and violet is not a signal at all — it is the ambient light every surface
//! is tinted by, which is why it can be everywhere without competing with the other two.
//!
//! Pane edges are dim WHITE hairlines. A hued outline is what stops a surface reading as glass:
//! real glass has no colour at its edge, only light catching the bevel.
//!
//! Surfaces are translucent and the page is lit from beneath by [`ambience`], so panes read as lit
//! glass rather than flat dark plastic. Unlike comfyui/surveyor there is no backdrop blur here:
//! the blur is a Glow grab-pass and plugin viewports paint through a wgpu callback, so this app is
//! locked to wgpu. The translucent fills over the light pools carry the look on their own.
//!
//! Glyphs stay ASCII plus mainstream emoji — no font is loaded into this Context, so anything
//! outside egui's default coverage renders as a tofu box.

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
/// Dimmed ink for secondary lines (ids, versions, hints).
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

pub fn apply(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    let radius = CornerRadius::same(5);

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

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = glass(18, 16, 28, 132);
    w.noninteractive.weak_bg_fill = glass(14, 12, 22, 120);
    w.noninteractive.bg_stroke = Stroke::new(1.0, RIM);
    w.noninteractive.fg_stroke = Stroke::new(1.0, INK);
    w.noninteractive.corner_radius = radius;

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
/// composites to a smooth falloff at a fixed 48 circles regardless of screen size. The radii keep
/// black between the pools on purpose — light everywhere is just a tinted page.
pub fn ambience(ctx: &egui::Context) {
    let rect = ctx.content_rect();
    let d = rect.width().min(rect.height()).max(1.0);
    egui::Area::new(egui::Id::new("plugins-ambience"))
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

/// Frame for a bar pinned to a screen edge: a touch more opaque than a card, no corner rounding.
pub fn bar() -> egui::Frame {
    egui::Frame::new()
        .fill(glass(14, 12, 24, 190))
        .inner_margin(egui::Margin::symmetric(8, 6))
}

/// The page itself: `panel_fill` over the light pools, with a margin narrower than egui's default
/// so a plugin viewport keeps nearly the full width.
pub fn page(style: &egui::Style) -> egui::Frame {
    egui::Frame::central_panel(style).inner_margin(6)
}

/// Selectable button that keeps its frame when unselected — egui drops it, which reads as a plain
/// label rather than a control.
pub fn selectable<'a>(selected: bool, atoms: impl egui::IntoAtoms<'a>) -> egui::Button<'a> {
    egui::Button::selectable(selected, atoms).frame_when_inactive(true)
}

/// [`egui::Ui::selectable_label`] at a fixed size, with a persistent frame plus a neon pink rim
/// when selected — egui's `interact_selectable` leaves `bg_stroke` off for the selected state, so
/// the pink edge a pressed button would show is painted here on top of the selection fill.
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

/// A status dot, wrapped in a fixed allocation to vertically centre it against the row's buttons.
pub fn status_dot(ui: &mut egui::Ui, color: Color32) {
    let h = ui.spacing().interact_size.y;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, h), egui::Sense::hover());
    let halo = glass(color.r(), color.g(), color.b(), 90);
    ui.painter().circle_filled(rect.center(), 4.0, color);
    ui.painter().circle_stroke(rect.center(), 6.0, Stroke::new(1.5, halo));
}

/// Vertical scroll area; scrollbar only when content overflows.
pub fn scroll_vertical() -> egui::ScrollArea {
    egui::ScrollArea::vertical().scroll_bar_visibility(ScrollBarVisibility::VisibleWhenNeeded)
}

/// Horizontal scroll for the nav strip; no bar, since one under the tabs would eat tap height.
pub fn scroll_horizontal() -> egui::ScrollArea {
    egui::ScrollArea::horizontal().scroll_bar_visibility(ScrollBarVisibility::AlwaysHidden)
}
