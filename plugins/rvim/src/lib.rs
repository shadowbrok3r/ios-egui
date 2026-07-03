//! rvim: a Neovim-style modal code editor rendered with ratatui inside a WASM plugin.
//!
//! The modal engine lives in [`vim`]; this file owns the plugin boundary: translating
//! egui events into [`vim::Key`]s, touch handling (tap to place the cursor, touchbar for
//! Esc/Ctrl on the iOS soft keyboard, drag to scroll), and session persistence.

mod buffer;
mod finder;
mod fs;
mod help;
mod highlight;
mod render;
mod state;
mod theme;
mod ui;
mod vim;

use egui_ios_plugin_sdk::{CreateConfig, HostHandle, PluginApp, egui, plugin};
use serde::{Deserialize, Serialize};

use buffer::Position;
use fs::Vfs;
use highlight::Highlighter;
use render::TerminalSurface;
use state::{Buffer, EditorState};
use ui::{DrawCtx, LayoutInfo, TouchAction};
use vim::{Key, Mode, VimEngine};

struct Rvim {
    surface: TerminalSurface,
    st: EditorState,
    vim: VimEngine,
    hl: Highlighter,
    vfs: Vfs,
    focused: bool,
    /// egui time of the last keystroke, for cursor-blink reset.
    last_input: f64,
    /// Ctrl armed from the touchbar; applies to the next key.
    ctrl_armed: bool,
    /// Grid layout of the previous frame, for mapping taps to buffer positions.
    layout: LayoutInfo,
    /// Sub-cell drag remainders so slow drags accumulate instead of rounding away.
    scroll_accum: egui::Vec2,
    /// Pointer position on the previous frame while a drag is in progress.
    last_pointer: Option<egui::Pos2>,
}

impl Rvim {
    fn new(_cfg: &CreateConfig) -> Self {
        Rvim {
            surface: TerminalSurface::new(),
            st: EditorState::new(),
            vim: VimEngine::new(),
            hl: Highlighter::new(),
            vfs: Vfs::load(),
            focused: false,
            last_input: 0.0,
            ctrl_armed: false,
            layout: LayoutInfo::default(),
            scroll_accum: egui::Vec2::ZERO,
            last_pointer: None,
        }
    }

    fn feed(&mut self, key: Key, host: &HostHandle) {
        let key = if self.ctrl_armed {
            self.ctrl_armed = false;
            match key {
                Key::Char(c) => Key::Ctrl(c.to_ascii_lowercase()),
                other => other,
            }
        } else {
            key
        };
        self.vim.handle_key(&mut self.st, &mut self.vfs, host, key);
    }

    /// Translate this frame's egui events into vim keys. Returns true on any key.
    fn handle_keys(&mut self, ui: &egui::Ui, host: &HostHandle) -> bool {
        let mut activity = false;
        ui.input_mut(|i| {
            let mut keep = Vec::new();
            for ev in i.events.drain(..) {
                let mut consumed = false;
                match &ev {
                    egui::Event::Text(text) => {
                        for c in text.chars() {
                            if c != '\n' && c != '\r' && !c.is_control() {
                                self.feed(Key::Char(c), host);
                                activity = true;
                                consumed = true;
                            }
                        }
                    }
                    egui::Event::Key { key, pressed: true, modifiers, .. } => {
                        let ctrl = modifiers.ctrl || modifiers.command;
                        let translated = match key {
                            egui::Key::Escape => Some(Key::Esc),
                            egui::Key::Enter => Some(Key::Enter),
                            egui::Key::Backspace => Some(Key::Backspace),
                            egui::Key::Delete => Some(Key::Delete),
                            egui::Key::Tab => Some(Key::Tab),
                            egui::Key::ArrowUp => Some(Key::Up),
                            egui::Key::ArrowDown => Some(Key::Down),
                            egui::Key::ArrowLeft => Some(Key::Left),
                            egui::Key::ArrowRight => Some(Key::Right),
                            egui::Key::Home => Some(Key::Home),
                            egui::Key::End => Some(Key::End),
                            egui::Key::PageUp => Some(Key::PageUp),
                            egui::Key::PageDown => Some(Key::PageDown),
                            egui::Key::OpenBracket if ctrl => Some(Key::Esc),
                            egui::Key::Period if ctrl => Some(Key::Esc), // Cmd+. is iOS escape
                            k if ctrl => letter(*k).map(Key::Ctrl),
                            _ => None,
                        };
                        if let Some(k) = translated {
                            self.feed(k, host);
                            activity = true;
                            // Only consume the key if we are focused, so we don't steal global keys unless focused
                            if self.focused || k == Key::Esc {
                                consumed = true;
                            }
                        }
                    }
                    _ => {}
                }
                if !consumed {
                    keep.push(ev);
                }
            }
            i.events = keep;
        });
        
        if activity {
            self.last_input = ui.input(|i| i.time);
        }
        activity
    }

    /// A tap at `pos` either presses a touchbar key or places the cursor.
    fn handle_tap(&mut self, pos: egui::Pos2, rect: egui::Rect, host: &HostHandle) {
        self.focused = true;
        let cell = self.surface.cell;
        let col = ((pos.x - rect.min.x) / cell.x.max(1.0)) as i64;
        let row = ((pos.y - rect.min.y) / cell.y.max(1.0)) as i64;
        if col < 0 || row < 0 {
            return;
        }
        let (col, row) = (col as u16, row as u16);

        if row == self.layout.touchbar_row {
            match ui::touchbar_action_at(col, self.layout.cols) {
                Some(TouchAction::Key(k)) => {
                    self.feed(k, host);
                    host.haptic(6);
                }
                Some(TouchAction::StickyCtrl) => {
                    self.ctrl_armed = !self.ctrl_armed;
                    host.haptic(6);
                }
                Some(TouchAction::ToggleKeyboard) => {
                    self.focused = false;
                    host.haptic(6);
                }
                None => {}
            }
            return;
        }

        // Tap inside the text area places the cursor at that cell.
        let text_end = self.layout.text_top + self.layout.text_rows;
        if row >= self.layout.text_top && row < text_end && self.st.finder.is_none() {
            let in_insert = self.vim.mode() == Mode::Insert;
            if let Some(buf) = self.st.buf_mut() {
                let line = buf.scroll.0 + (row - self.layout.text_top) as usize;
                let col = (buf.scroll.1 + col.saturating_sub(self.layout.gutter_w) as usize)
                    .saturating_sub(0);
                buf.cursor = buf.text.clamp(Position::new(line, col), in_insert);
                buf.desired_col = buf.cursor.col;
            }
        }
    }

    /// Grab-scroll: pointer drags and the scroll wheel move the view, char-cell at a time.
    fn handle_scroll(&mut self, ui: &egui::Ui) {
        let cell = self.surface.cell;
        let (down, pos, wheel) = ui.input(|i| {
            (i.pointer.primary_down(), i.pointer.interact_pos(), i.smooth_scroll_delta)
        });
        let mut drag = egui::Vec2::ZERO;
        match (down, pos) {
            (true, Some(p)) => {
                if let Some(prev) = self.last_pointer {
                    drag = p - prev;
                }
                self.last_pointer = Some(p);
            }
            _ => self.last_pointer = None,
        }
        self.scroll_accum += drag + wheel;
        let rows = (self.scroll_accum.y / cell.y.max(1.0)).trunc() as i64;
        let cols = (self.scroll_accum.x / cell.x.max(1.0)).trunc() as i64;
        if rows == 0 && cols == 0 {
            return;
        }
        self.scroll_accum.y -= rows as f32 * cell.y;
        self.scroll_accum.x -= cols as f32 * cell.x;
        if let Some(buf) = self.st.buf_mut() {
            // Dragging down reveals earlier lines; content follows the finger.
            let max_top = buf.text.line_count().saturating_sub(1);
            let top = (buf.scroll.0 as i64 - rows).clamp(0, max_top as i64) as usize;
            let left = (buf.scroll.1 as i64 - cols).max(0) as usize;
            buf.scroll = (top, left);
        }
    }
}

/// Lowercase letter for an egui letter key, for Ctrl-chord translation.
fn letter(key: egui::Key) -> Option<char> {
    use egui::Key as K;
    Some(match key {
        K::A => 'a', K::B => 'b', K::C => 'c', K::D => 'd', K::E => 'e', K::F => 'f',
        K::G => 'g', K::H => 'h', K::I => 'i', K::J => 'j', K::K => 'k', K::L => 'l',
        K::M => 'm', K::N => 'n', K::O => 'o', K::P => 'p', K::Q => 'q', K::R => 'r',
        K::S => 's', K::T => 't', K::U => 'u', K::V => 'v', K::W => 'w', K::X => 'x',
        K::Y => 'y', K::Z => 'z',
        _ => return None,
    })
}

impl PluginApp for Rvim {
    fn update(&mut self, ui: &mut egui::Ui, host: &HostHandle) {
        let avail = ui.available_size();
        let (rect, resp) = ui.allocate_exact_size(avail, egui::Sense::click_and_drag());

        self.surface.fit(ui, avail);
        if resp.clicked() {
            if let Some(pos) = resp.interact_pointer_pos() {
                self.handle_tap(pos, rect, host);
            }
        }
        host.request_keyboard(self.focused);

        self.handle_keys(ui, host);
        self.handle_scroll(ui);

        let time = ui.input(|i| i.time);
        let blink_on = if !self.focused {
            false
        } else if time - self.last_input < 0.5 {
            true
        } else {
            (time * 1.2) as i64 % 2 == 0
        };

        self.layout = ui::draw(
            self.surface.terminal_mut(),
            DrawCtx {
                st: &mut self.st,
                vim: &self.vim,
                hl: &mut self.hl,
                blink_on,
                ctrl_armed: self.ctrl_armed,
                focused: self.focused,
            },
        );
        self.surface.paint(ui.painter(), rect);

        ui.ctx().request_repaint_after(std::time::Duration::from_millis(120));
    }

    fn save_state(&self) -> Vec<u8> {
        let session = Session {
            buffers: self
                .st
                .buffers
                .iter()
                .map(|b| BufSession {
                    name: b.name.clone(),
                    text: b.text.text(),
                    cursor: b.cursor,
                    modified: b.modified(),
                })
                .collect(),
            active: self.st.active,
        };
        postcard::to_stdvec(&session).unwrap_or_default()
    }

    fn restore_state(&mut self, bytes: &[u8]) {
        let Ok(session) = postcard::from_bytes::<Session>(bytes) else { return };
        self.st.buffers = session
            .buffers
            .into_iter()
            .map(|b| {
                let mut buf = Buffer::new(&b.name, &b.text);
                buf.cursor = buf.text.clamp(b.cursor, false);
                buf.saved_version = if b.modified { u64::MAX } else { 0 };
                buf
            })
            .collect();
        self.st.active = session.active.min(self.st.buffers.len().saturating_sub(1));
    }
}

/// Hot-reload session snapshot: open buffers and which one is focused.
#[derive(Serialize, Deserialize)]
struct Session {
    buffers: Vec<BufSession>,
    active: usize,
}

#[derive(Serialize, Deserialize)]
struct BufSession {
    name: String,
    text: String,
    cursor: Position,
    modified: bool,
}

plugin!(Rvim::new);
