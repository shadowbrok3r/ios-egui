//! Screens and the shared widgets they are built from.

pub mod chips;
pub mod dashboard;
pub mod filters;
pub mod frost;
pub mod inspect;
pub mod requests;
pub mod settings;

use egui::{Color32, CornerRadius, Frame, Margin, Response, RichText, Stroke, Ui, Vec2};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Dashboard,
    Requests,
    Filters,
    Settings,
}

impl Tab {
    pub const ALL: [Tab; 4] = [Tab::Dashboard, Tab::Requests, Tab::Filters, Tab::Settings];

    pub fn label(self) -> &'static str {
        match self {
            Tab::Dashboard => "Dashboard",
            Tab::Requests => "Requests",
            Tab::Filters => "Filters",
            Tab::Settings => "Settings",
        }
    }
}

// ── Palette ──────────────────────────────────────────────────────────────────
//
// One violet hue at three lightnesses, because the neon has to live in the glow and the strokes
// rather than in the text: a bright accent *on* an accent fill is the classic neon-UI mistake and
// lands around 2:1 contrast. `ACCENT` is for text and hairlines on glass, `ACCENT_FILL` is the only
// thing a button is ever filled with, and `ACCENT_GLOW` never touches a glyph.
//
// Translucent constants need `_const`: the plain `from_rgba_unmultiplied` goes through a lookup
// table and is not a const fn.
const fn glass(r: u8, g: u8, b: u8, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied_const(r, g, b, a)
}

/// Backdrop: true black at the bottom so an OLED panel actually switches those pixels off, with
/// just enough violet at the very top for the glass to have something to tint.
pub const BG_TOP: Color32 = Color32::from_rgb(17, 8, 30);
pub const BG_BOTTOM: Color32 = Color32::from_rgb(0, 0, 0);

/// Layered glass, tinted hot pink. Lower alpha than before: on a black backdrop a pane reads as
/// glass at a much lighter touch, and anything heavier just looks like grey plastic.
pub const GLASS: Color32 = glass(255, 130, 220, 14);
pub const GLASS_RAISED: Color32 = glass(255, 130, 220, 26);
/// Recessed: text fields, the body pane, anything the eye should read as "inside". Pure black on
/// an OLED, so the wells genuinely go dark.
pub const WELL: Color32 = glass(0, 0, 0, 140);
pub const HAIRLINE: Color32 = glass(255, 150, 225, 34);
/// Brighter top edge — the catch-light that sells a pane as glass rather than a rectangle.
pub const HAIRLINE_LIT: Color32 = glass(255, 200, 240, 70);

/// Text and hairlines. Violet, and the calmest of the neons — it carries the most text.
pub const ACCENT: Color32 = Color32::from_rgb(198, 164, 255);
/// Button fills only — a pink-leaning violet. White on this is 6.6:1.
pub const ACCENT_FILL: Color32 = Color32::from_rgb(146, 42, 199);
/// Bloom behind a primary control. Hot pink, and never a text colour.
pub const ACCENT_GLOW: Color32 = glass(255, 60, 180, 116);

/// Hot pink — the loud one. Highlights, the second bloom, JSON strings.
pub const PINK: Color32 = Color32::from_rgb(255, 105, 180);
pub const PINK_FILL: Color32 = Color32::from_rgb(190, 24, 93);
/// Aqua green — the cool counterweight. JSON keys, "everything is fine".
pub const AQUA: Color32 = Color32::from_rgb(64, 240, 208);

pub const TEXT: Color32 = Color32::from_rgb(234, 228, 246);
pub const MUTED: Color32 = Color32::from_rgb(168, 160, 196);
pub const ON_ACCENT: Color32 = Color32::from_rgb(255, 255, 255);

/// Aqua rather than leaf green, so "running" belongs to the same galaxy as everything else.
pub const GOOD: Color32 = Color32::from_rgb(52, 245, 197);
pub const WARN: Color32 = Color32::from_rgb(255, 199, 89);
/// Coral, deliberately kept clear of the hot pink so "bad" never reads as "accent".
pub const BAD: Color32 = Color32::from_rgb(255, 107, 107);
/// Destructive fill. Translucent rose glass rather than a solid red slab: Stop is a normal thing
/// to press, and a saturated red button shouted louder than everything around it. Over the dark
/// backdrop this settles near rgb(60,12,32), so white still reads about 13:1.
pub const DANGER_FILL: Color32 = glass(214, 40, 100, 76);

/// Comfortable thumb target; egui's default button height is built for a mouse.
pub const TOUCH_HEIGHT: f32 = 46.0;
pub const RADIUS: u8 = 14;

/// Block `target` and everything under it, keeping the Filters screen's text buffer in step.
///
/// That resync is load-bearing: the custom-rules TextEdit overwrites the config from its buffer on
/// `lost_focus`, so a rule added without it would be silently deleted the next time that field was
/// touched.
pub fn apply_block(app: &mut crate::app::PrivaxyApp, target: &str, host: &egui_mobile::Host) {
    let Some(loaded) = app.loaded.as_mut() else {
        return;
    };

    match loaded.block(target) {
        Some(rule) => {
            app.notice = Some(if loaded.proxy.is_some() {
                format!("Blocking {rule} — the engine rebuilds in a moment.")
            } else {
                format!("Added {rule}. It takes effect when the proxy starts.")
            });
            host.haptic(egui_mobile::Haptic::Success);
        }
        None => {
            app.notice = Some(format!("{target} is already blocked."));
            host.haptic(egui_mobile::Haptic::Light);
        }
    }
}

/// Removes a previously added `||host^` rule and tells the user which one went away.
pub fn apply_unblock(app: &mut crate::app::PrivaxyApp, target: &str, host: &egui_mobile::Host) {
    let Some(loaded) = app.loaded.as_mut() else {
        return;
    };
    let bare = target.split(':').next().unwrap_or_default().trim();
    let rule = format!("||{bare}^");
    if loaded.unblock(&rule) {
        app.notice = Some(format!("Unblocked {bare}."));
        host.haptic(egui_mobile::Haptic::Success);
    }
}

/// One Android back press this frame, taken so no other layer reacts to the same press.
///
/// Back arrives as an ordinary key: winit maps `AKEYCODE_BACK` to `NamedKey::BrowserBack` and
/// already reports it to the framework as handled, so an unconsumed press is a no-op rather than
/// an exit. Escape rides along for a desktop/keyboard build.
pub fn back_pressed(ctx: &egui::Context) -> bool {
    ctx.input_mut(|input| {
        input.consume_key(egui::Modifiers::NONE, egui::Key::BrowserBack)
            || input.consume_key(egui::Modifiers::NONE, egui::Key::Escape)
    })
}

/// Ask for `POST_NOTIFICATIONS`, without which the ongoing service notification — and its Stop
/// button — is never drawn, even though the foreground service itself runs.
pub fn request_notifications(host: &egui_mobile::Host) {
    #[cfg(target_os = "android")]
    {
        use egui_mobile::HostExt;
        host.request_notification_permission();
    }
    #[cfg(not(target_os = "android"))]
    let _ = host;
}

/// Open Android's Security settings, where a CA certificate has to be installed from.
///
/// The programmatic route (`KeyChain.createInstallIntent`) is refused for CA certificates on
/// Android 11 and later — the installer answers "Can't install CA certificates" — so the file plus
/// this deep link is the only path that works on a current device.
pub fn open_security_settings(host: &egui_mobile::Host) {
    #[cfg(target_os = "android")]
    {
        use egui_mobile::HostExt;
        host.open_settings("android.settings.SECURITY_SETTINGS");
    }
    #[cfg(not(target_os = "android"))]
    let _ = host;
}

/// Installs the palette. Called from `update`, because Android never invokes `EguiApp::theme` —
/// `egui-android` has no `.theme(..)` call, so everything here was dead until now.
///
/// Written into *both* theme slots and then pinned to dark: `set_visuals` only touches the
/// currently active theme, so a device reporting light would otherwise keep stock egui styling.
pub fn apply_theme(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();

    // Panels stay transparent so the backdrop gradient shows through them.
    visuals.panel_fill = Color32::TRANSPARENT;
    visuals.window_fill = GLASS_RAISED;
    visuals.window_stroke = Stroke::new(1.0, HAIRLINE);
    visuals.extreme_bg_color = WELL;
    visuals.faint_bg_color = GLASS;
    visuals.override_text_color = Some(TEXT);
    visuals.hyperlink_color = ACCENT;
    visuals.warn_fg_color = WARN;
    visuals.error_fg_color = BAD;
    visuals.selection.bg_fill = ACCENT_FILL;
    visuals.selection.stroke = Stroke::new(1.0, ON_ACCENT);

    // Buttons paint `weak_bg_fill`, never `bg_fill` — setting only the latter (as this app did)
    // leaves every button at egui's default grey. `bg_fill` still drives checkboxes and sliders.
    for (state, weak, strong, stroke) in [
        (0usize, GLASS, GLASS_RAISED, HAIRLINE),
        (1, GLASS_RAISED, GLASS_RAISED, ACCENT),
        (2, ACCENT_FILL, ACCENT_FILL, ACCENT),
    ] {
        let widget = match state {
            0 => &mut visuals.widgets.inactive,
            1 => &mut visuals.widgets.hovered,
            _ => &mut visuals.widgets.active,
        };
        widget.weak_bg_fill = weak;
        widget.bg_fill = strong;
        widget.bg_stroke = Stroke::new(1.0, stroke);
        widget.corner_radius = CornerRadius::same(10);
        widget.fg_stroke = Stroke::new(1.0, if state == 2 { ON_ACCENT } else { TEXT });
    }
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, HAIRLINE);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
    visuals.widgets.open.weak_bg_fill = GLASS_RAISED;
    visuals.widgets.open.bg_stroke = Stroke::new(1.0, ACCENT);

    ctx.set_visuals_of(egui::Theme::Dark, visuals.clone());
    ctx.set_visuals_of(egui::Theme::Light, visuals);
    ctx.set_theme(egui::ThemePreference::Dark);

    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = Vec2::new(8.0, 8.0);
        style.spacing.button_padding = Vec2::new(12.0, 9.0);
        style.spacing.interact_size.y = 30.0;
        // The default floating bar is a 10pt invisible strip; a finger needs to see and hit it.
        style.spacing.scroll.bar_width = 8.0;
        style.spacing.scroll.floating = false;
        // The handle takes `fg_stroke.color` while this is true — which is the near-white text
        // colour, i.e. a glaring bar down the edge. False switches it to `bg_fill`, so it picks up
        // the glass tint and only turns violet while it is being dragged.
        style.spacing.scroll.foreground_color = false;
        style.spacing.scroll.dormant_handle_opacity = 1.0;
        style.spacing.scroll.active_handle_opacity = 1.0;
        style.spacing.scroll.interact_handle_opacity = 1.0;
        style.spacing.scroll.dormant_background_opacity = 0.0;
        style.spacing.scroll.active_background_opacity = 0.4;
        // Keep the handle off the pane's edge and the text off the handle — at the defaults the
        // last characters of a line sit under the bar and read as clipped.
        style.spacing.scroll.bar_inner_margin = 8.0;
        style.spacing.scroll.bar_outer_margin = 3.0;
    });
}

/// The one gradient in the app, on the background layer under every panel.
///
/// This is what the translucent surfaces are translucent *against* — without it the glass has
/// nothing to tint and the whole thing reads as flat grey.
pub fn paint_backdrop(ctx: &egui::Context) {
    // The whole viewport, not `content_rect`: the status bar and the gesture strip sit outside the
    // safe area, and stopping the gradient at the insets leaves black bands top and bottom.
    let rect = ctx.input(|input| input.viewport_rect());
    let painter = ctx.layer_painter(egui::LayerId::background());
    painter.add(egui::epaint::Shape::gradient_rect(
        rect,
        egui::epaint::Direction::TopDown,
        [BG_TOP, BG_BOTTOM],
    ));
    // Two nebula blooms rather than one: a magenta light source up top and a cooler aqua wash low
    // down, so the field shifts hue as it falls instead of being a flat violet ramp.
    for (centre, size, colour) in [
        (
            egui::pos2(rect.left() + rect.width() * 0.26, rect.top() + 40.0),
            egui::vec2(rect.width() * 0.9, rect.width() * 0.5),
            glass(255, 45, 170, 64),
        ),
        (
            egui::pos2(rect.right() - rect.width() * 0.18, rect.top() + rect.height() * 0.34),
            egui::vec2(rect.width() * 0.7, rect.width() * 0.5),
            glass(150, 60, 255, 30),
        ),
    ] {
        painter.add(
            egui::epaint::RectShape::filled(
                egui::Rect::from_center_size(centre, size),
                CornerRadius::same(255),
                colour,
            )
            .with_blur_width(rect.width() * 0.42),
        );
    }
}

/// A pane of glass — the app's one grouping primitive.
pub fn card<R>(ui: &mut Ui, add_contents: impl FnOnce(&mut Ui) -> R) -> R {
    glass_frame(GLASS)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let inner = add_contents(ui);
            catch_light(ui);
            inner
        })
        .inner
}

pub fn glass_frame(fill: Color32) -> Frame {
    Frame::new()
        .fill(fill)
        .stroke(Stroke::new(1.0, HAIRLINE))
        .corner_radius(CornerRadius::same(RADIUS))
        .inner_margin(Margin::same(14))
}

/// Brightens the pane's top edge. A uniform hairline reads as a border; a lit top edge reads as a
/// surface catching light from above, which is most of what makes flat translucency look like glass.
fn catch_light(ui: &Ui) {
    let rect = ui.min_rect().expand(14.0);
    let inset = f32::from(RADIUS) * 0.7;
    ui.painter().line_segment(
        [
            egui::pos2(rect.left() + inset, rect.top()),
            egui::pos2(rect.right() - inset, rect.top()),
        ],
        Stroke::new(1.0, HAIRLINE_LIT),
    );
}

pub fn section_title(ui: &mut Ui, text: &str) {
    ui.label(
        RichText::new(text.to_uppercase())
            .size(11.0)
            .color(MUTED)
            .strong(),
    );
}

/// A full-width primary control, with a bloom behind it.
///
/// The glow is a blurred rect written into a slot reserved *before* the button, because a painter
/// call after the fact would land on top of it. `Frame`'s own shadow is no use here: its margin
/// maths excludes the shadow, so the halo would be clipped by the enclosing scroll area.
pub fn big_button(ui: &mut Ui, text: &str, fill: Color32) -> Response {
    let slot = ui.painter().add(egui::Shape::Noop);
    let response = ui.add_sized(
        [ui.available_width(), TOUCH_HEIGHT],
        egui::Button::new(
            RichText::new(text)
                .size(16.0)
                .strong()
                .color(ON_ACCENT),
        )
        .fill(fill)
        .corner_radius(CornerRadius::same(RADIUS)),
    );

    let glow = if fill == ACCENT_FILL {
        ACCENT_GLOW
    } else {
        fill.gamma_multiply(0.45)
    };
    ui.painter().set(
        slot,
        egui::epaint::RectShape::filled(
            response.rect.shrink(2.0),
            CornerRadius::same(RADIUS),
            glow,
        )
        // Clamped to the shorter side by the tessellator, so a 46pt-tall button tops out here.
        .with_blur_width(18.0),
    );
    response
}

/// Label and value on one line, value pushed to the right edge.
///
/// Both sides truncate. Text in a `horizontal` does not wrap, so a long value would otherwise
/// widen the page past the screen and push everything to its right out of reach.
pub fn detail_row(ui: &mut Ui, label: &str, value: RichText) {
    ui.horizontal(|ui| {
        ui.add(egui::Label::new(RichText::new(label).color(MUTED)).truncate());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add(egui::Label::new(value).truncate());
        });
    });
}

pub fn stat(ui: &mut Ui, label: &str, value: u64, color: Color32) {
    ui.vertical(|ui| {
        ui.label(RichText::new(format_count(value)).size(22.0).color(color).strong());
        ui.label(RichText::new(label).size(11.0).color(MUTED));
    });
}

/// Thousands separators, then k/M once the number stops fitting a phone column.
pub fn format_count(value: u64) -> String {
    if value >= 1_000_000 {
        return format!("{:.1}M", value as f64 / 1_000_000.0);
    }
    if value >= 100_000 {
        return format!("{:.0}k", value as f64 / 1_000.0);
    }

    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(character);
    }
    out
}

/// Byte counts at the size a request log produces them.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Keeps long URLs from forcing horizontal scroll, trimming the middle where the noise is.
pub fn elide(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let head: String = text.chars().take(max * 2 / 3).collect();
    let tail: String = text
        .chars()
        .skip(text.chars().count().saturating_sub(max / 4))
        .collect();
    format!("{head}…{tail}")
}
