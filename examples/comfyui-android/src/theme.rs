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
/// Body ink — cool near-white, the default text colour on the black page.
const INK: Color32 = Color32::from_rgb(233, 233, 239);

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
    let text = INK;
    let mut v = egui::Visuals::dark();

    v.override_text_color = Some(text);
    // The page is pure black (AMOLED); windows/menus lift a few points so they read as raised
    // panes, and text wells sink below the page. Separators come from the strokes below.
    v.panel_fill = rgb(0, 0, 0);
    // Menus / dropdowns / modals share window_fill: a cool, faintly teal-tinted glass panel that
    // lifts off the black page, with a visible cool rim so the container itself reads as glass
    // even before you touch an item (the accent hover/press then lights up individual rows).
    // Translucent, because `frost` blurs the page behind these panes and an opaque fill would hide
    // it. The two alphas multiply: this one over the frost's film leaves roughly a third of the
    // blurred backdrop showing, which is glass that light text still reads on.
    v.window_fill = rgba(18, 21, 27, 120);
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

    widget_palette(&mut v.widgets);

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

/// The interaction grammar shared by every framed widget: rest = restrained dark glass, hover =
/// aqua edge, press/active/selected = pink. Applied to the global visuals, and re-applied inside
/// menus — egui's `menu_style` strips the rest-state fill and every accent rim.
fn widget_palette(w: &mut egui::style::Widgets) {
    let text = INK;
    let text_bright = rgb(248, 250, 252);
    // Rounded-but-restrained corners read modern without going bubbly on dense touch rows.
    let radius = CornerRadius::same(5);

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

/// Tap height for a menu row — a framed 40px target rather than egui's 18px text line.
pub const MENU_ROW_H: f32 = 40.0;

/// Give a menu's rows a framed, touch-sized look: egui's `menu_style` strips the rest-state fill
/// and every accent rim and squashes `button_padding` to 2×0, so an entry otherwise reads as bare
/// text on an 18px line. Call once at the top of a popup body; children inherit it.
pub fn menu_row_style(ui: &mut egui::Ui) {
    let s = ui.style_mut();
    s.spacing.button_padding = egui::vec2(10.0, 8.0);
    s.spacing.interact_size.y = MENU_ROW_H;
    // Tighter gaps, since the rows themselves are now more than twice as tall.
    s.spacing.item_spacing.y = 4.0;
    widget_palette(&mut s.visuals.widgets);
}

/// Lay a collapsing section's body out as full-width rows. egui drops the popup's justified layout
/// inside a collapsing body, so without this the options size to their own text while the section
/// header spans the whole menu.
pub fn menu_section_body<R>(ui: &mut egui::Ui, content: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.scope_builder(
        egui::UiBuilder::new().layout(egui::Layout::top_down_justified(egui::Align::Min)),
        content,
    )
    .inner
}

/// A menu / combo-box button whose popup opens *upward* and scrolls.
///
/// `Ui::menu_button` and `egui::ComboBox` only flip their popup above the button when it wouldn't
/// otherwise fit — but egui's screen rect extends under the Android navigation bar, so a short menu
/// "fits" below and ends up covering the nav bar and the system gesture area. Everything in a
/// bottom control bar uses this instead, which always prefers opening upward. The bounded scroll
/// area keeps a long list (e.g. every model) from running off the top of the screen.
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

/// [`up_menu`] with a fixed button size (viewer action icons) and a chosen popup close behavior.
pub fn up_menu_sized<R>(
    ui: &mut egui::Ui,
    label: impl Into<egui::WidgetText>,
    min_size: egui::Vec2,
    close_behavior: egui::PopupCloseBehavior,
    content: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::Response {
    menu_popup(
        ui,
        label,
        Some(min_size),
        egui::RectAlign::TOP_START,
        &[egui::RectAlign::TOP_END, egui::RectAlign::BOTTOM_START],
        close_behavior,
        content,
    )
}

/// Header menu: popup opens below the button, right-aligned so it grows left.
pub fn down_menu<R>(
    ui: &mut egui::Ui,
    label: impl Into<egui::WidgetText>,
    content: impl FnOnce(&mut egui::Ui) -> R,
) {
    let _ = menu_popup(
        ui,
        label,
        None,
        egui::RectAlign::BOTTOM_END,
        &[egui::RectAlign::BOTTOM_START, egui::RectAlign::TOP_END],
        egui::PopupCloseBehavior::CloseOnClick,
        content,
    );
}

/// How tall a menu opened from `anchor` may grow: the room actually available on the side it opens
/// toward, less a small margin. A fixed ceiling used to cut long menus (open workflow tabs, model
/// lists) short on tall screens while leaving them off the screen edge on short ones.
pub fn menu_height_cap(ctx: &egui::Context, anchor: egui::Rect, align: egui::RectAlign) -> f32 {
    let screen = ctx.content_rect();
    // `parent.y()` is the anchor edge the popup hangs off: BOTTOM_* opens downward from the
    // anchor's bottom, TOP_* upward from its top.
    let down = anchor.bottom().max(screen.top());
    let below = screen.bottom() - down;
    let above = anchor.top().min(screen.bottom()) - screen.top();
    let (room, flipped) = match align.parent.y() {
        egui::Align::Max => (below, above),
        egui::Align::Min => (above, below),
        egui::Align::Center => (screen.height(), screen.height()),
    };
    // Frame padding + the gap, so the scroll area itself stops short of the screen edge. The floor
    // is `min`'d rather than clamped: a landscape phone with the keyboard up leaves the content rect
    // under 160pt tall, and `f32::clamp` panics outright on min > max.
    let floor = 160.0_f32.min(screen.height());
    (room.max(flipped) - 24.0).clamp(floor, screen.height().max(floor))
}

/// How wide a popup may grow. egui clips a popup's painting at `content_rect` but never shrinks it,
/// so a row wider than the screen cuts the menu — frame and all — off at the right edge instead of
/// wrapping or scrolling.
pub fn menu_width_cap(ctx: &egui::Context) -> f32 {
    (ctx.content_rect().width() - 24.0).max(160.0)
}

/// How tall a scrollable list *inside* a menu section may grow. The popup's own cap already tracks
/// the room on screen; a nested list used to take a flat 320pt of it, which on a phone showed a
/// handful of albums under a menu with hundreds of points to spare.
pub fn menu_list_height(ctx: &egui::Context) -> f32 {
    (ctx.content_rect().height() * 0.66).clamp(260.0, 720.0)
}

/// A menu button whose popup is bounded to the screen on both axes.
///
/// Width note for callers: egui measures a popup's natural width on a one-off sizing pass the first
/// time it opens, and rows here truncate rather than extend, so a menu built from a list that is
/// still loading keeps the width it was born with. Give such a menu an explicit
/// `ui.set_min_width(..)` — the album pickers do.
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
    // Grow with the screen so a full menu fits without an inner scrollbar.
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
            // egui sizes a popup's Area on a one-off sizing pass, seeded from
            // `spacing.default_area_size` (600×400) — and that becomes the Ui's `max_rect` on every
            // later frame, so the scroll area below could never grow past ~386px no matter what
            // `max_height` said. Claiming the full cap *during the sizing pass only* lets the Area
            // record a tall enough size; the list still shrinks to its content afterwards.
            if ui.is_sizing_pass() {
                ui.set_min_height(cap);
            }
            // `set_max_width` *sets* the width rather than capping it, and `Popup::menu` lays out
            // justified — handed the cap outright, a two-item menu would stretch across the whole
            // phone. Clamped against the Area's own width, this only ever shrinks: the sizing pass
            // measures against the cap (so a long row truncates instead of widening the popup past
            // the screen, where egui clips it), and every later frame keeps the width that
            // measured.
            let width = (width_cap - ui.spacing().menu_margin.sum().x).min(ui.max_rect().width());
            ui.set_max_width(width);
            scroll_vertical()
                .max_height(cap)
                .max_width(width)
                .show(ui, |ui| {
                    // Truncate rather than Extend: rows still never wrap to a second line, but a
                    // long one now ends in an ellipsis instead of pushing the menu off-screen.
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                    content(ui)
                })
                .inner
        });
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `build` inside an upward menu on a phone-sized viewport, settled over several frames
    /// (egui sizes a popup from the previous frame's area state).
    fn in_menu(build: &mut dyn FnMut(&mut egui::Ui)) {
        let ctx = egui::Context::default();
        apply(&ctx);
        let screen = egui::vec2(393.0, 873.0);
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, screen);
        for frame in 0..8 {
            let at = egui::pos2(40.0, screen.y - 20.0);
            let events = if frame == 1 {
                vec![
                    egui::Event::PointerMoved(at),
                    egui::Event::PointerButton {
                        pos: at,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::default(),
                    },
                    egui::Event::PointerButton {
                        pos: at,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: egui::Modifiers::default(),
                    },
                ]
            } else {
                Vec::new()
            };
            let input = egui::RawInput { screen_rect: Some(rect), events, ..Default::default() };
            let _ = ctx.run_ui(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.add_space(screen.y - 40.0);
                    up_menu(ui, "View", |ui| build(ui));
                });
            });
        }
    }

    /// Every row a menu shows — entries at the top level and options inside a section — must span
    /// the same width and carry a full tap target. Without [`menu_row_style`] rows are 18px tall,
    /// and without [`menu_section_body`] a section's options size to their own text (egui drops
    /// justification in a collapsing body) while the header spans the whole menu.
    #[test]
    fn menu_rows_are_full_width_and_tappable() {
        let seen: std::cell::RefCell<Vec<(&'static str, f32, f32)>> =
            std::cell::RefCell::new(Vec::new());
        in_menu(&mut |ui| {
            seen.borrow_mut().clear();
            menu_row_style(ui);
            let rec = |k: &'static str, r: &egui::Response| {
                seen.borrow_mut().push((k, r.rect.width(), r.rect.height()));
            };
            rec("entry", &ui.button("Select"));
            let mut pick = 0usize;
            let header = egui::CollapsingHeader::new("Sort · Newest")
                .id_salt("sort")
                .default_open(true)
                .show(ui, |ui| {
                    menu_section_body(ui, |ui| {
                        rec("option", &selectable_value(ui, &mut pick, 0, "Newest"));
                        rec("long option", &selectable_value(ui, &mut pick, 1, "Oldest first"));
                        scroll_vertical().max_height(200.0).show(ui, |ui| {
                            rec("scrolled option", &selectable_value(ui, &mut pick, 2, "Name"));
                        });
                    });
                });
            rec("section header", &header.header_response);
        });

        let seen = seen.borrow();
        assert_eq!(seen.len(), 5, "every probe row must have been laid out");
        let header_w = seen.iter().find(|(k, ..)| *k == "section header").unwrap().1;
        assert!(header_w > 300.0, "a section header should span the menu, got {header_w:.0}px");
        for (k, w, h) in seen.iter() {
            assert!(*h >= MENU_ROW_H, "{k} row is only {h:.0}px tall");
            // Options sit inside the indent rail and a scrollbar, so they run a little narrower.
            assert!(*w >= header_w - 40.0, "{k} row is {w:.0}px of the header's {header_w:.0}px");
        }
    }

    /// A menu must never grow wider than the screen, and must still hug its content when it is
    /// narrow. egui clips a popup's painting at `content_rect` but never shrinks it, so a row wider
    /// than the phone cut the whole menu — frame, rim and all — off at the right edge; the worst
    /// case is the gallery's View button, which sits hard against the right margin with a model
    /// list of full checkpoint names under it. Capping that with a bare `set_max_width` swings the
    /// other way, because it *sets* the width and the popup lays out justified: both halves are
    /// asserted here.
    #[test]
    fn menu_never_grows_past_the_screen() {
        let screen = egui::vec2(393.0, 873.0);
        let long = "sdxl_a_very_long_checkpoint_filename_v12_pruned.safetensors".repeat(3);
        let (left, right) = menu_row_span(screen, &long);
        println!("long rows span {left:.0}..{right:.0} of a {:.0}px screen", screen.x);
        assert!(right <= screen.x, "a menu row runs to {right:.0}px past the right edge");
        assert!(left >= 0.0, "a menu row starts at {left:.0}px, off the left edge");

        let (left, right) = menu_row_span(screen, "Short");
        println!("short rows span {left:.0}..{right:.0}");
        assert!(right - left < screen.x * 0.6, "a 6-row \"Short\" menu is {:.0}px wide", right - left);
    }

    /// `menu_height_cap` must survive a viewport shorter than its own floor. A landscape phone with
    /// the keyboard up leaves the content rect well under 160pt, and `f32::clamp` panics on
    /// min > max rather than saturating.
    #[test]
    fn menu_height_cap_survives_a_squashed_viewport() {
        let ctx = egui::Context::default();
        for height in [400.0, 160.0, 159.0, 140.0, 40.0, 1.0] {
            let rect =
                egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(873.0, height));
            let _ = ctx.run_ui(
                egui::RawInput { screen_rect: Some(rect), ..Default::default() },
                |ctx| {
                    let anchor = egui::Rect::from_min_size(
                        egui::pos2(40.0, (height - 30.0).max(0.0)),
                        egui::vec2(68.0, 28.0),
                    );
                    let cap = menu_height_cap(ctx, anchor, egui::RectAlign::TOP_START);
                    assert!(cap > 0.0 && cap.is_finite(), "cap {cap} at height {height}");
                    assert!(cap <= height.max(160.0), "cap {cap} exceeds a {height}pt screen");
                },
            );
        }
    }

    /// Open an upward menu from a right-aligned button and return the left/right extent its rows
    /// reached on the settled frame.
    fn menu_row_span(screen: egui::Vec2, label: &str) -> (f32, f32) {
        let ctx = egui::Context::default();
        apply(&ctx);
        let viewport = egui::Rect::from_min_size(egui::Pos2::ZERO, screen);
        let long = label;
        let button = std::cell::Cell::new(egui::Rect::NOTHING);
        let span = std::cell::Cell::new(None::<(f32, f32)>);
        for frame in 0..8 {
            // Frame 0 lays the button out; frame 1 taps where it landed.
            let events = if frame == 1 {
                let at = button.get().center();
                vec![
                    egui::Event::PointerMoved(at),
                    egui::Event::PointerButton {
                        pos: at,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::default(),
                    },
                    egui::Event::PointerButton {
                        pos: at,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: egui::Modifiers::default(),
                    },
                ]
            } else {
                Vec::new()
            };
            let input = egui::RawInput { screen_rect: Some(viewport), events, ..Default::default() };
            let _ = ctx.run_ui(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.add_space(screen.y - 40.0);
                    span.set(None);
                    ui.horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let resp = up_menu_sized(
                                ui,
                                "View",
                                egui::vec2(68.0, 28.0),
                                egui::PopupCloseBehavior::CloseOnClickOutside,
                                |ui| {
                                    menu_row_style(ui);
                                    let mut seen: Option<(f32, f32)> = None;
                                    for row in 0..6 {
                                        let r = ui.button(format!("{row} {long}")).rect;
                                        seen = Some(match seen {
                                            Some((l, x)) => (l.min(r.left()), x.max(r.right())),
                                            None => (r.left(), r.right()),
                                        });
                                    }
                                    span.set(seen);
                                },
                            );
                            button.set(resp.rect);
                        });
                    });
                });
            });
        }

        span.get().expect("the menu never opened")
    }

    /// Drive a `down_menu` with `rows` 32px entries on a `screen`-sized viewport and return how
    /// much of the list is actually visible (the scroll viewport inside the popup).
    fn menu_visible_height(screen: egui::Vec2, rows: usize) -> f32 {
        menu_visible_height_at(screen, rows, false)
    }

    /// `bottom_bar`: put the button at the bottom of the screen and open the menu upward, the way
    /// every control-bar menu (models, gallery filters) does.
    fn menu_visible_height_at(screen: egui::Vec2, rows: usize, bottom_bar: bool) -> f32 {
        let ctx = egui::Context::default();
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, screen);
        let seen = std::cell::Cell::new(0.0f32);
        // Frame 1 clicks the button; egui sizes a popup from the previous frame's area state, so
        // give it a few frames to settle before believing the measurement.
        for frame in 0..5 {
            let events = if frame == 1 {
                let at = if bottom_bar { egui::pos2(40.0, screen.y - 20.0) } else { egui::pos2(40.0, 20.0) };
                vec![
                    egui::Event::PointerMoved(at),
                    egui::Event::PointerButton {
                        pos: at,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::default(),
                    },
                    egui::Event::PointerButton {
                        pos: at,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: egui::Modifiers::default(),
                    },
                ]
            } else {
                Vec::new()
            };
            let input = egui::RawInput { screen_rect: Some(rect), events, ..Default::default() };
            let _ = ctx.run_ui(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    if bottom_bar {
                        ui.add_space(screen.y - 40.0);
                    }
                    let menu: fn(&mut egui::Ui, &str, &mut dyn FnMut(&mut egui::Ui)) =
                        if bottom_bar {
                            |ui, label, content| up_menu(ui, label, content)
                        } else {
                            |ui, label, content| down_menu(ui, label, content)
                        };
                    menu(ui, "Tabs", &mut |ui: &mut egui::Ui| {
                        ui.set_min_width(260.0);
                        // Inside the popup's scroll area, the clip rect IS the visible list.
                        seen.set(ui.clip_rect().height().min(screen.y));
                        for i in 0..rows {
                            ui.add_sized([220.0, 32.0], egui::Button::new(format!("tab {i}")));
                        }
                    });
                });
            });
        }
        seen.get()
    }

    /// A menu must use the room it actually has. The cap used to be a fixed
    /// `content_height - 96`, clamped to 640 — which on a landscape phone (the way the graph
    /// editor is usable at all) left the open-workflow list showing a handful of tabs.
    #[test]
    fn menu_cap_uses_the_room_below() {
        let portrait = egui::vec2(393.0, 873.0);
        let landscape = egui::vec2(873.0, 393.0);

        let tall = menu_visible_height(portrait, 24);
        let short = menu_visible_height(landscape, 24);
        println!("portrait menu {tall:.0}px, landscape {short:.0}px");

        // Portrait: the old ceiling was 640; the room below a top-of-screen button is ~840.
        assert!(tall > 640.0, "portrait menu only got {tall:.0}px of a 873px screen");
        // Landscape: nearly the whole 393px, rather than (393 - 96) minus frame padding.
        assert!(short > 300.0, "landscape menu only got {short:.0}px of a 393px screen");
        // Neither may overflow its screen.
        assert!(tall <= 873.0 && short <= 393.0, "menu overflowed the viewport");

        // …and a short menu must still hug its content rather than claiming the whole cap.
        let few = menu_visible_height(portrait, 3);
        assert!(few < 200.0, "a 3-row menu took {few:.0}px");

        // The bottom control bars open upward (models, gallery filters) — same room rule.
        let upward = menu_visible_height_at(portrait, 24, true);
        println!("upward menu {upward:.0}px");
        assert!(upward > 640.0, "an upward menu only got {upward:.0}px of a 873px screen");
        assert!(upward <= 873.0, "upward menu overflowed the viewport");
    }
}
