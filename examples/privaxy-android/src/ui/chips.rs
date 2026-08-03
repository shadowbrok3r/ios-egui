//! A list of hosts or rules edited as chips rather than as lines of text.
//!
//! A multiline `TextEdit` is the wrong shape for this on a phone: adding one host means summoning
//! the keyboard, finding the end of the text, and typing a newline in the right place, and removing
//! one means selecting a line by dragging over 11pt monospace. A chip carries its own remove
//! button, so the common edit is one tap and the keyboard only appears to add.

use crate::ui;
use egui_mobile::{Haptic, Host, egui};

/// What a chip row did this frame.
pub enum Action {
    None,
    /// The entry was added; the caller persists and pushes it to the proxy.
    Added(String),
    /// The entry was removed.
    Removed(String),
}

/// Draws `entries` as chips above an add field, and returns the single edit made this frame.
///
/// `draft` is the add field's buffer, owned by the caller so it survives across frames.
pub fn editor(
    ui: &mut egui::Ui,
    id_salt: &str,
    entries: &[String],
    draft: &mut String,
    hint: &str,
    empty: &str,
    host: &Host,
) -> Action {
    let mut action = Action::None;

    if entries.is_empty() {
        ui.label(egui::RichText::new(empty).size(12.0).color(ui::MUTED));
    } else {
        // horizontal_wrapped so a long list flows onto as many rows as it needs instead of
        // running off the screen edge.
        ui.horizontal_wrapped(|ui| {
            // Padding is what gives a chip its tappable height; set once for the whole row.
            ui.spacing_mut().button_padding = egui::vec2(11.0, 8.0);
            for entry in entries {
                if chip(ui, entry).clicked() {
                    action = Action::Removed(entry.clone());
                    host.haptic(Haptic::Light);
                }
            }
        });
    }

    ui.add_space(8.0);

    let mut submitted = false;
    ui.horizontal(|ui| {
        // Add button first at a fixed width, so the field takes exactly what is left.
        let button = 74.0;
        let field = (ui.available_width() - button - ui.spacing().item_spacing.x).max(80.0);
        let response = ui.add_sized(
            [field, ui::TOUCH_HEIGHT],
            egui::TextEdit::singleline(draft)
                .hint_text(hint)
                .id_salt(id_salt),
        );
        submitted = response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
        submitted |= ui
            .add_sized([button, ui::TOUCH_HEIGHT], egui::Button::new("Add"))
            .clicked();
    });

    if submitted {
        let entry = draft.trim().to_owned();
        if !entry.is_empty() && !entries.iter().any(|existing| existing == &entry) {
            draft.clear();
            host.haptic(Haptic::Success);
            action = Action::Added(entry);
        }
    }

    action
}

/// One chip: its text plus a trailing cross that removes it. Sizes itself to its text.
fn chip(ui: &mut egui::Ui, text: &str) -> egui::Response {
    let label = format!("{}  \u{2716}", ui::elide(text, 26));
    ui.add(
        egui::Button::new(egui::RichText::new(label).size(12.0).color(ui::TEXT))
            .fill(ui::GLASS_RAISED)
            .stroke(egui::Stroke::new(1.0, ui::HAIRLINE))
            .corner_radius(egui::CornerRadius::same(ui::RADIUS)),
    )
}
