//! A floating key toolbar for keys an iOS soft keyboard lacks (Esc, Tab, Ctrl-*, arrows, …).
//!
//! When the keyboard is up it floats as rows just above it (the host already shrinks the
//! viewport by the keyboard height, so the bottom edge sits on the keyboard); when the keyboard
//! is down it becomes a vertical strip on the right. Either way it returns the *content* rect
//! left for the terminal, so the ratatui grid is fitted around it and never covered. The button
//! set is priority-ordered and truncated to what fits, so a wide screen (iPad / landscape) shows
//! more keys than a narrow one (iPhone portrait).

use egui_ios_plugin_sdk::egui;

/// What a key does. `Bytes` are sent verbatim to the PTY (and interpreted by the password
/// prompt); `Submit`/`Cancel`/`Disconnect` are semantic so each screen can react.
#[derive(Clone, Copy)]
pub enum Act {
    Bytes(&'static [u8]),
    Submit,
    Cancel,
    Disconnect,
}

struct ToolKey {
    label: &'static str,
    act: Act,
}

/// Priority-ordered; the most useful keys survive truncation on a narrow screen.
static KEYS: &[ToolKey] = &[
    ToolKey { label: "Esc", act: Act::Cancel },
    ToolKey { label: "Enter", act: Act::Submit },
    ToolKey { label: "Tab", act: Act::Bytes(b"\t") },
    ToolKey { label: "^C", act: Act::Bytes(b"\x03") },
    ToolKey { label: "↑", act: Act::Bytes(b"\x1b[A") },
    ToolKey { label: "↓", act: Act::Bytes(b"\x1b[B") },
    ToolKey { label: "←", act: Act::Bytes(b"\x1b[D") },
    ToolKey { label: "→", act: Act::Bytes(b"\x1b[C") },
    ToolKey { label: "^D", act: Act::Bytes(b"\x04") },
    ToolKey { label: "⏻", act: Act::Disconnect },
    ToolKey { label: "^Z", act: Act::Bytes(b"\x1a") },
    ToolKey { label: "^L", act: Act::Bytes(b"\x0c") },
    ToolKey { label: "^R", act: Act::Bytes(b"\x12") },
    ToolKey { label: "Home", act: Act::Bytes(b"\x1b[H") },
    ToolKey { label: "End", act: Act::Bytes(b"\x1b[F") },
    ToolKey { label: "PgUp", act: Act::Bytes(b"\x1b[5~") },
    ToolKey { label: "PgDn", act: Act::Bytes(b"\x1b[6~") },
    ToolKey { label: "|", act: Act::Bytes(b"|") },
    ToolKey { label: "/", act: Act::Bytes(b"/") },
    ToolKey { label: "~", act: Act::Bytes(b"~") },
];

const GAP: f32 = 4.0;
const BTN_H: f32 = 30.0;
const BTN_W: f32 = 54.0;
/// Cap the horizontal toolbar to this many rows so it can't eat the screen on a narrow device.
const MAX_ROWS: usize = 2;

pub struct ToolbarResult {
    /// Rect left for the terminal after the toolbar takes its space.
    pub content: egui::Rect,
    /// Key actions pressed this frame.
    pub actions: Vec<Act>,
    /// The hide/show handle was tapped this frame.
    pub toggle_hidden: bool,
}

fn panel_fill() -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(18, 18, 26, 235)
}

/// Lay the toolbar into `area`; `kb_up` chooses the docked-horizontal vs vertical-right layout,
/// `hidden` collapses it to a small show handle.
pub fn render(ui: &mut egui::Ui, area: egui::Rect, kb_up: bool, hidden: bool) -> ToolbarResult {
    if hidden {
        // A small handle to bring the toolbar back; the terminal keeps the whole area.
        let handle = egui::Rect::from_min_size(
            egui::pos2(area.max.x - 38.0, area.max.y - BTN_H - GAP),
            egui::vec2(34.0, BTN_H),
        );
        let toggle = ui.put(handle, egui::Button::new("⌨").small()).clicked();
        return ToolbarResult { content: area, actions: Vec::new(), toggle_hidden: toggle };
    }
    if kb_up {
        render_horizontal(ui, area)
    } else {
        render_vertical(ui, area)
    }
}

fn render_horizontal(ui: &mut egui::Ui, area: egui::Rect) -> ToolbarResult {
    let per_row = (((area.width() + GAP) / (BTN_W + GAP)).floor() as usize).max(1);
    let capacity = per_row * MAX_ROWS;
    // Reserve one slot for the hide button.
    let show_n = KEYS.len().min(capacity.saturating_sub(1));
    let items = show_n + 1;
    let rows = items.div_ceil(per_row).max(1);
    let tb_h = rows as f32 * (BTN_H + GAP) + GAP;
    let tb_top = area.max.y - tb_h;
    let tb_rect = egui::Rect::from_min_max(egui::pos2(area.min.x, tb_top), area.max);
    ui.painter().rect_filled(tb_rect, 6.0, panel_fill());

    let mut actions = Vec::new();
    let mut toggle = false;
    for i in 0..items {
        let (r, c) = (i / per_row, i % per_row);
        let pos = egui::pos2(
            area.min.x + GAP + c as f32 * (BTN_W + GAP),
            tb_top + GAP + r as f32 * (BTN_H + GAP),
        );
        let rect = egui::Rect::from_min_size(pos, egui::vec2(BTN_W, BTN_H));
        if i < show_n {
            if ui.put(rect, egui::Button::new(KEYS[i].label)).clicked() {
                actions.push(KEYS[i].act);
            }
        } else if ui.put(rect, egui::Button::new("▾").small()).on_hover_text("Hide").clicked() {
            toggle = true;
        }
    }
    let content = egui::Rect::from_min_max(area.min, egui::pos2(area.max.x, tb_top - GAP));
    ToolbarResult { content, actions, toggle_hidden: toggle }
}

fn render_vertical(ui: &mut egui::Ui, area: egui::Rect) -> ToolbarResult {
    let per_col = (((area.height() + GAP) / (BTN_H + GAP)).floor() as usize).max(1);
    let show_n = KEYS.len().min(per_col.saturating_sub(1));
    let items = show_n + 1;
    let tb_w = BTN_W + 2.0 * GAP;
    let tb_left = area.max.x - tb_w;
    let col_h = items as f32 * (BTN_H + GAP) + GAP;
    // Center the strip vertically so it reads as a floating column.
    let top = (area.min.y + (area.height() - col_h) * 0.5).max(area.min.y);
    let tb_rect = egui::Rect::from_min_max(egui::pos2(tb_left, top), egui::pos2(area.max.x, top + col_h));
    ui.painter().rect_filled(tb_rect, 6.0, panel_fill());

    let mut actions = Vec::new();
    let mut toggle = false;
    for i in 0..items {
        let pos = egui::pos2(tb_left + GAP, top + GAP + i as f32 * (BTN_H + GAP));
        let rect = egui::Rect::from_min_size(pos, egui::vec2(BTN_W, BTN_H));
        if i < show_n {
            if ui.put(rect, egui::Button::new(KEYS[i].label)).clicked() {
                actions.push(KEYS[i].act);
            }
        } else if ui.put(rect, egui::Button::new("▸").small()).on_hover_text("Hide").clicked() {
            toggle = true;
        }
    }
    let content = egui::Rect::from_min_max(area.min, egui::pos2(tb_left - GAP, area.max.y));
    ToolbarResult { content, actions, toggle_hidden: toggle }
}
