//! rvim: a Neovim-style modal code editor rendered with ratatui inside a WASM plugin.
//!
//! The modal engine lives in [`vim`]; this file owns the plugin boundary: translating
//! egui events into [`vim::Key`]s, touch handling (tap to place the cursor, touchbar for
//! Esc/Ctrl on the iOS soft keyboard, drag to scroll), and session persistence.

mod buffer;
mod explorer;
#[cfg(test)]
mod fuzz_probe;
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
use explorer::ExplorerState;
use fs::Vfs;
use highlight::Highlighter;
use render::TerminalSurface;
use state::{Buffer, EditorState, SplitDir, Window};
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
            let events = std::mem::take(&mut i.events);
            let mut keep = Vec::new();
            for ev in events {
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
                    // Desktop hosts turn Ctrl+V into a paste event before it reaches us.
                    egui::Event::Paste(_) => {
                        self.feed(Key::Ctrl('v'), host);
                        activity = true;
                        consumed = true;
                    }
                    egui::Event::Key { key, pressed: true, modifiers, .. } => {
                        let ctrl = modifiers.ctrl || modifiers.command;
                        let translated = match key {
                            egui::Key::Escape => Some(Key::Esc),
                            egui::Key::Enter => Some(Key::Enter),
                            // Alt+Backspace deletes a word, like Ctrl+w.
                            egui::Key::Backspace if modifiers.alt || ctrl => Some(Key::Ctrl('w')),
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
                } else if let egui::Event::Key { key, modifiers, .. } = ev {
                    // Explicitly tell egui this key is consumed to stop focus traversal/menu navigation
                    i.consume_key(modifiers, key);
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

        // Tap on a which-key hint sends its key.
        if self.layout.whichkey_rows > 0
            && row >= self.layout.whichkey_top
            && row < self.layout.whichkey_top + self.layout.whichkey_rows
        {
            if let Some(hints) = self.vim.key_hints() {
                if let Some(k) = ui::whichkey_action_at(
                    col,
                    row - self.layout.whichkey_top,
                    self.layout.cols,
                    &hints.entries,
                ) {
                    self.feed(k, host);
                    host.haptic(6);
                }
            }
            return;
        }

        if self.st.finder.is_some() {
            return;
        }

        // Tap in the explorer sidebar selects/opens files.
        let sidebar_end = self.layout.explorer_top + self.layout.explorer_rows;
        if self.layout.explorer_w > 0 && col < self.layout.explorer_w {
            if row >= self.layout.explorer_top && row < sidebar_end {
                self.st.explorer_focused = true;
                let action = match self.st.explorer.as_mut() {
                    Some(ex) => ex.handle_tap(&self.vfs, (row - self.layout.explorer_top) as usize),
                    None => return,
                };
                explorer::apply_action(&mut self.st, &mut self.vfs, action);
                host.haptic(6);
            }
            return;
        }

        // Tap inside a window focuses it and places the cursor at that cell.
        let Some(r) = self
            .layout
            .windows
            .iter()
            .find(|r| col >= r.x && col < r.x + r.w && row >= r.y && row < r.y + r.h)
            .copied()
        else {
            return;
        };
        self.st.active_win = r.win;
        self.st.explorer_focused = false;
        let in_insert = self.vim.mode() == Mode::Insert;
        if let Some(buf) = self.st.buf_mut() {
            let line = buf.scroll.0 + (row - r.y) as usize;
            let col = buf.scroll.1 + col.saturating_sub(r.x + r.gutter_w) as usize;
            buf.cursor = buf.text.clamp(Position::new(line, col), in_insert);
            buf.desired_col = buf.cursor.col;
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
                vfs: &self.vfs,
                blink_on,
                ctrl_armed: self.ctrl_armed,
                focused: self.focused,
                paused: time - self.last_input > 0.35,
            },
        );
        self.surface.paint(ui.painter(), rect);

        ui.ctx().request_repaint_after(std::time::Duration::from_millis(120));
    }

    fn save_state(&self) -> Vec<u8> {
        postcard::to_stdvec(&collect_session(&self.st)).unwrap_or_default()
    }

    fn restore_state(&mut self, bytes: &[u8]) {
        let Ok(session) = postcard::from_bytes::<Session>(bytes) else { return };
        apply_session(&mut self.st, session);
    }
}

/// Hot-reload session snapshot: open buffers, window layout, and focus.
#[derive(Serialize, Deserialize)]
struct Session {
    buffers: Vec<BufSession>,
    active: usize,
    /// Buffer index each window shows.
    windows: Vec<usize>,
    active_win: usize,
    split_vertical: bool,
    explorer_visible: bool,
}

#[derive(Serialize, Deserialize)]
struct BufSession {
    name: String,
    text: String,
    cursor: Position,
    modified: bool,
}

/// Snapshot the editor state for `save_state`.
fn collect_session(st: &EditorState) -> Session {
    Session {
        buffers: st
            .buffers
            .iter()
            .map(|b| BufSession {
                name: b.name.clone(),
                text: b.text.text(),
                cursor: b.cursor,
                modified: b.modified(),
            })
            .collect(),
        active: st.active(),
        windows: st.windows.iter().map(|w| w.buffer).collect(),
        active_win: st.active_win,
        split_vertical: st.split_dir == SplitDir::Vertical,
        explorer_visible: st.explorer.is_some(),
    }
}

/// Rebuild the editor state from a decoded session, clamping every index.
fn apply_session(st: &mut EditorState, session: Session) {
    st.buffers = session
        .buffers
        .into_iter()
        .map(|b| {
            let mut buf = Buffer::new(&b.name, &b.text);
            buf.cursor = buf.text.clamp(b.cursor, false);
            buf.saved_version = if b.modified { u64::MAX } else { 0 };
            buf
        })
        .collect();
    let last = st.buffers.len().saturating_sub(1);
    st.windows = session
        .windows
        .into_iter()
        .map(|b| Window { buffer: b.min(last) })
        .collect();
    if st.windows.is_empty() || st.buffers.is_empty() {
        st.windows = vec![Window { buffer: 0 }];
    }
    st.active_win = session.active_win.min(st.windows.len() - 1);
    st.split_dir = if session.split_vertical { SplitDir::Vertical } else { SplitDir::Horizontal };
    st.explorer = session.explorer_visible.then(ExplorerState::new);
    st.explorer_focused = false;
    if st.windows.len() == 1 {
        st.set_active(session.active.min(last));
    }
}

plugin!(Rvim::new);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_roundtrip_restores_windows_and_explorer() {
        let mut st = EditorState::new();
        st.buffers.push(Buffer::new("a.rs", "alpha"));
        st.buffers.push(Buffer::new("b.rs", "bravo"));
        st.buffers[1].text.insert_char(Position::default(), 'x');
        st.buffers[1].cursor = Position::new(0, 3);
        st.split(SplitDir::Vertical);
        st.windows[0].buffer = 1;
        st.active_win = 1;
        st.explorer = Some(ExplorerState::new());

        let bytes = postcard::to_stdvec(&collect_session(&st)).unwrap();
        let mut re = EditorState::new();
        apply_session(&mut re, postcard::from_bytes(&bytes).unwrap());

        assert_eq!(re.buffers.len(), 2);
        assert_eq!(re.buffers[1].name, "b.rs");
        assert_eq!(re.buffers[1].text.text(), "xbravo");
        assert!(re.buffers[1].modified());
        assert!(!re.buffers[0].modified());
        assert_eq!(re.buffers[1].cursor, Position::new(0, 3));
        assert_eq!(re.windows.len(), 2);
        assert_eq!(re.windows[0].buffer, 1);
        assert_eq!(re.windows[1].buffer, 0);
        assert_eq!(re.active_win, 1);
        assert_eq!(re.split_dir, SplitDir::Vertical);
        assert!(re.explorer.is_some());
        assert!(!re.explorer_focused);
    }

    #[test]
    fn apply_session_clamps_bad_indices() {
        let session = Session {
            buffers: vec![BufSession {
                name: "a.rs".into(),
                text: "x".into(),
                cursor: Position::new(99, 99),
                modified: false,
            }],
            active: 42,
            windows: vec![7, 0, 9],
            active_win: 12,
            split_vertical: false,
            explorer_visible: false,
        };
        let mut st = EditorState::new();
        apply_session(&mut st, session);
        assert_eq!(st.buffers[0].cursor, Position::new(0, 0));
        assert!(st.windows.iter().all(|w| w.buffer == 0));
        assert_eq!(st.active_win, 2);
        assert!(st.explorer.is_none());
    }

    #[test]
    fn apply_session_survives_empty_everything() {
        let session = Session {
            buffers: Vec::new(),
            active: 0,
            windows: Vec::new(),
            active_win: 0,
            split_vertical: true,
            explorer_visible: true,
        };
        let mut st = EditorState::new();
        apply_session(&mut st, session);
        assert!(st.buffers.is_empty());
        assert_eq!(st.windows.len(), 1);
        assert_eq!(st.active_win, 0);
        assert_eq!(st.split_dir, SplitDir::Vertical);
        assert!(st.explorer.is_some());
    }

    #[test]
    fn stale_session_bytes_fail_decode_silently() {
        // Phase-1 layout lacked the window fields; decoding must simply fail.
        assert!(postcard::from_bytes::<Session>(&[1, 0]).is_err());
        assert!(postcard::from_bytes::<Session>(&[]).is_err());
    }
}
