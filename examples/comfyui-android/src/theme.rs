//! AMOLED synthwave theme: a true-black page carrying two accents.
//!
//! Hot pink ([`PINK`]) is the primary — anything selected, pressed, or active — and aqua ([`AQUA`])
//! is the secondary — hover feedback, links, and live/info markers. Everything else is near-black
//! with cool near-white text, so the two accents stay signals rather than noise. The interaction
//! grammar is: rest = restrained dark surface, hover = aqua edge, press/active/selected = pink.
//! Spacing stays touch-sized rather than desktop density.

use egui::containers::scroll_area::ScrollBarVisibility;
use egui::{Color32, CornerRadius, FontFamily, FontId, Sense, Stroke, TextStyle};

use crate::types::FontSizes;

fn rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

fn rgba(r: u8, g: u8, b: u8, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(r, g, b, a)
}

// AMOLED synthwave palette: a pure-black page carries two accents — hot pink is the primary
// (selection, pressed/active widgets, primary ink, progress) and aqua is the secondary (hover,
// links, live/info markers). Kept to two so each one stays a signal instead of noise.

/// Primary accent — hot pink. The loudest colour in the app; reserved for what's active or chosen.
pub const PINK: Color32 = Color32::from_rgb(255, 61, 139);
/// A lifted pink for ink/text/rings where the base pink reads a touch dim on pure black.
pub const PINK_BRIGHT: Color32 = Color32::from_rgb(255, 110, 168);
/// Secondary accent — aqua/cyan. Hover feedback, hyperlinks, and "live/active" indicators.
pub const AQUA: Color32 = Color32::from_rgb(43, 226, 214);
/// A lifted aqua for text where the base reads dim.
pub const AQUA_BRIGHT: Color32 = Color32::from_rgb(120, 240, 232);

/// Circular floating-action diameter (queue, create menu, lock, undo, inpaint tools).
pub const FAB_SIZE: f32 = 40.0;
/// Vertical/horizontal step between stacked FABs.
pub const FAB_STEP: f32 = FAB_SIZE + 8.0;
/// Inset from a pane edge to the FAB's top-left (`FAB_SIZE` + 10).
pub const FAB_EDGE: f32 = FAB_SIZE + 10.0;

/// Hot-pink icon ink (matches `error_fg` / the primary accent).
pub fn fab_icon() -> Color32 {
    PINK
}

/// Default translucent FAB disc — faint aqua-tinted glass over the AMOLED page.
pub fn fab_bg() -> Color32 {
    rgba(7, 16, 18, 208)
}

/// Selected / open FAB disc (pink-tinted, the primary "active" wash).
pub fn fab_bg_on() -> Color32 {
    rgba(92, 22, 54, 225)
}

/// Queue-busy FAB disc — aqua, the "live" accent.
pub fn fab_bg_ok() -> Color32 {
    rgba(10, 46, 46, 216)
}

/// Cancel FAB disc — deep pink/red.
pub fn fab_bg_danger() -> Color32 {
    rgba(84, 18, 44, 216)
}

/// Circular icon FAB with CENTER_CENTER glyph paint (avoids button-padding left bias on emoji).
pub fn fab(ui: &mut egui::Ui, icon: &str, fill: Color32) -> egui::Response {
    fab_with_sense(ui, icon, fill, Sense::click_and_drag())
}

/// Selectable button that always keeps a frame (egui hides it when unselected + inactive).
pub fn selectable<'a>(selected: bool, atoms: impl egui::IntoAtoms<'a>) -> egui::Button<'a> {
    egui::Button::selectable(selected, atoms).frame_when_inactive(true)
}

/// [`Ui::selectable_label`] with a persistent frame, plus a neon pink rim when selected — egui's
/// `interact_selectable` leaves `bg_stroke` off for the selected state, so the pink edge (the same
/// signal a pressed button shows) is painted here on top of the selection fill.
pub fn selectable_label<'a>(
    ui: &mut egui::Ui,
    selected: bool,
    text: impl egui::IntoAtoms<'a>,
) -> egui::Response {
    let resp = ui.add(selectable(selected, text));
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

/// [`Ui::selectable_value`] with a persistent frame.
pub fn selectable_value<'a, Value: PartialEq>(
    ui: &mut egui::Ui,
    current_value: &mut Value,
    selected_value: Value,
    text: impl egui::IntoAtoms<'a>,
) -> egui::Response {
    let mut response = selectable_label(ui, *current_value == selected_value, text);
    if response.clicked() && *current_value != selected_value {
        *current_value = selected_value;
        response.mark_changed();
    }
    response
}

fn fab_with_sense(
    ui: &mut egui::Ui,
    icon: &str,
    fill: Color32,
    sense: Sense,
) -> egui::Response {
    let size = egui::vec2(FAB_SIZE, FAB_SIZE);
    let (rect, resp) = ui.allocate_exact_size(size, sense);
    let enabled = ui.is_enabled();
    let mut fill = fill;
    if enabled {
        if resp.is_pointer_button_down_on() {
            fill = fab_bg_on();
        } else if resp.hovered() {
            fill = rgba(10, 40, 42, 224);
        }
    } else {
        fill = Color32::from_rgba_unmultiplied(fill.r(), fill.g(), fill.b(), fill.a() / 2);
    }
    let center = rect.center();
    let r = FAB_SIZE * 0.5;
    ui.painter().circle_filled(center, r, fill);
    // Neon aqua rim — the FAB's glass edge (pairs with the pink icon for the synthwave read).
    ui.painter().circle_stroke(center, r, Stroke::new(1.0, rgba(43, 226, 214, 140)));
    let ink = if enabled { fab_icon() } else { rgba(255, 61, 139, 110) };
    let icon_pt = if icon.chars().count() > 1 { 15.0 } else { 17.0 };
    ui.painter().text(
        center,
        egui::Align2::CENTER_CENTER,
        icon,
        FontId::new(icon_pt, FontFamily::Proportional),
        ink,
    );
    resp
}

/// Subtle dark fill tinting a tag chip/suggestion by Danbooru category, or `None` for unknown.
/// 0 general, 1 artist, 3 copyright, 4 character, 5 meta; light text stays readable on each.
pub fn tag_category_fill(cat: u8) -> Option<Color32> {
    match cat {
        0 => Some(rgb(28, 40, 60)),
        1 => Some(rgb(60, 32, 36)),
        3 => Some(rgb(52, 32, 58)),
        4 => Some(rgb(28, 52, 38)),
        5 => Some(rgb(58, 48, 30)),
        _ => None,
    }
}

/// Apply the theme: a true-black AMOLED page with hot-pink primary and aqua secondary accents.
pub fn apply(ctx: &egui::Context) {
    let text = rgb(233, 233, 239);
    let text_bright = rgb(248, 250, 252);
    // Rounded-but-restrained corners read modern without going bubbly on dense touch rows.
    let radius = CornerRadius::same(5);
    let mut v = egui::Visuals::dark();

    v.override_text_color = Some(text);
    // The page is pure black (AMOLED); windows/menus lift a few points so they read as raised
    // panes, and text wells sink below the page. Separators come from the strokes below.
    v.panel_fill = rgb(0, 0, 0);
    // Menus / dropdowns / modals share window_fill: a cool, faintly teal-tinted glass panel that
    // lifts off the black page, with a visible cool rim so the container itself reads as glass
    // even before you touch an item (the accent hover/press then lights up individual rows).
    v.window_fill = rgb(18, 21, 27);
    v.window_stroke = Stroke::new(1.2, rgba(72, 146, 156, 190));
    v.faint_bg_color = rgb(10, 10, 13); // striped-row alternate — barely there on black
    v.extreme_bg_color = rgb(7, 7, 10); // TextEdit / deep wells sink below the page
    v.code_bg_color = rgb(5, 5, 7);
    v.hyperlink_color = AQUA;
    v.warn_fg_color = AQUA_BRIGHT;
    v.error_fg_color = PINK;
    // Primary accent: everything selected/active/highlighted is pink. A saturated-but-still-glassy
    // fill so a selected chip reads as hot pink rather than a matte maroon, paired with the neon
    // rim painted in `selectable_label`. egui also uses this fill for progress bars.
    v.selection.bg_fill = rgba(255, 61, 139, 140);
    v.selection.stroke = Stroke::new(1.4, rgba(255, 110, 168, 255));
    v.window_shadow = egui::epaint::Shadow {
        offset: [0, 2],
        blur: 12,
        spread: 2,
        color: rgba(0, 0, 0, 200),
    };
    v.popup_shadow = egui::epaint::Shadow {
        offset: [0, 2],
        blur: 10,
        spread: 1,
        color: rgba(0, 0, 0, 170),
    };
    v.window_corner_radius = CornerRadius::same(8);
    v.menu_corner_radius = CornerRadius::same(8);

    let w = &mut v.widgets;
    // Non-interactive frames/labels/separators AND the indent rail beside collapsing bodies: a
    // faintly cool line so an open section's body reads as a bounded, subtly-tinted region.
    w.noninteractive.bg_fill = rgb(11, 11, 14);
    w.noninteractive.weak_bg_fill = rgb(8, 8, 11);
    w.noninteractive.bg_stroke = Stroke::new(1.0, rgba(58, 84, 96, 165));
    w.noninteractive.fg_stroke = Stroke::new(1.0, text);
    w.noninteractive.corner_radius = radius;

    // At rest — buttons and (framed) collapsing headers: a restrained dark-glass panel just above
    // the page (nudged a touch lighter than the first pass) with a faint cool rim, so the neon
    // accents on hover/press carry the interaction.
    w.inactive.bg_fill = rgb(22, 22, 27);
    w.inactive.weak_bg_fill = rgb(18, 18, 22);
    w.inactive.bg_stroke = Stroke::new(1.0, rgba(66, 68, 88, 155));
    w.inactive.fg_stroke = Stroke::new(1.0, text);
    w.inactive.corner_radius = radius;

    // Hover — aqua tinted glass: a translucent aqua fill over the black page under a neon aqua rim.
    w.hovered.bg_fill = rgba(43, 226, 214, 42);
    w.hovered.weak_bg_fill = rgba(43, 226, 214, 42);
    w.hovered.bg_stroke = Stroke::new(1.5, rgba(43, 226, 214, 240));
    w.hovered.fg_stroke = Stroke::new(1.5, text_bright);
    w.hovered.corner_radius = radius;

    // Active / pressed — pink tinted glass: a translucent pink fill under a vivid neon pink rim.
    w.active.bg_fill = rgba(255, 61, 139, 54);
    w.active.weak_bg_fill = rgba(255, 61, 139, 54);
    w.active.bg_stroke = Stroke::new(1.7, rgba(255, 61, 139, 245));
    w.active.fg_stroke = Stroke::new(2.0, Color32::WHITE);
    w.active.corner_radius = radius;

    // Open (expanded combo / menu source): the rest panel under a bright aqua "open" rim.
    w.open.bg_fill = rgb(22, 22, 27);
    w.open.weak_bg_fill = rgb(18, 18, 22);
    w.open.bg_stroke = Stroke::new(1.3, rgba(43, 226, 214, 205));
    w.open.fg_stroke = Stroke::new(1.0, text);
    w.open.corner_radius = radius;

    v.striped = true;
    // Full-width framed collapsing headers read as tappable section buttons.
    v.collapsing_header_frame = true;
    // A left rail down every indented region — chiefly collapsing-header bodies — so an open
    // section's contents read as a bounded, faintly-tinted group rather than floating on black.
    v.indent_has_left_vline = true;
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
