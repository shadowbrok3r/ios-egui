//! Shared editor state: open buffers, registers, search, options, and status messages.

use std::collections::HashMap;

use crate::buffer::{Position, TextBuffer};
use crate::explorer::ExplorerState;
use crate::finder::FinderState;
use crate::fs::Vfs;

/// Shape of a register's capture; determines paste geometry.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum RegKind {
    #[default]
    Char,
    Line,
    Block,
}

/// Yanked or deleted text plus the shape it was captured with.
#[derive(Clone, Default)]
pub struct Register {
    pub text: String,
    pub kind: RegKind,
}

#[derive(Clone, Default)]
pub struct SearchState {
    pub pattern: String,
    pub backwards: bool,
    /// Match highlighting is suppressed until the next search (`:noh`).
    pub suppressed: bool,
}

#[derive(Clone)]
pub struct Options {
    pub number: bool,
    pub relativenumber: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options { number: true, relativenumber: true }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MsgKind {
    Info,
    Error,
}

#[derive(Clone)]
pub struct StatusMsg {
    pub text: String,
    pub kind: MsgKind,
}

/// One open file: its text plus per-buffer view state.
pub struct Buffer {
    pub name: String,
    pub text: TextBuffer,
    pub cursor: Position,
    /// Top visible line and leftmost visible column.
    pub scroll: (usize, usize),
    /// Buffer version at the last save; differing from `text.version()` means modified.
    pub saved_version: u64,
    /// Column j/k aim for when passing through shorter lines.
    pub desired_col: usize,
    pub marks: HashMap<char, Position>,
}

impl Buffer {
    pub fn new(name: &str, text: &str) -> Self {
        Buffer {
            name: name.to_string(),
            text: TextBuffer::from_text(text),
            cursor: Position::default(),
            scroll: (0, 0),
            saved_version: 0,
            desired_col: 0,
            marks: HashMap::new(),
        }
    }

    pub fn modified(&self) -> bool {
        self.text.version() != self.saved_version
    }
}

/// One viewport into a buffer; splits create more of these.
pub struct Window {
    pub buffer: usize,
}

/// Axis every window splits along: Horizontal = stacked, Vertical = side by side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitDir {
    Horizontal,
    Vertical,
}

/// Minimum window width in columns for a split to be allowed.
pub const MIN_WIN_COLS: u16 = 10;
/// Minimum window height in rows for a split to be allowed.
pub const MIN_WIN_ROWS: u16 = 2;

pub struct EditorState {
    pub buffers: Vec<Buffer>,
    pub windows: Vec<Window>,
    pub active_win: usize,
    pub split_dir: SplitDir,
    /// Window-area size (cols, rows) from the last draw, for split-size checks.
    pub text_dims: (u16, u16),
    /// File explorer sidebar; `Some` = visible.
    pub explorer: Option<ExplorerState>,
    /// Keys route to the explorer instead of the editor.
    pub explorer_focused: bool,
    pub registers: HashMap<char, Register>,
    pub search: SearchState,
    pub options: Options,
    pub status: Option<StatusMsg>,
    pub finder: Option<FinderState>,
}

impl EditorState {
    pub fn new() -> Self {
        EditorState {
            buffers: Vec::new(),
            windows: vec![Window { buffer: 0 }],
            active_win: 0,
            split_dir: SplitDir::Horizontal,
            text_dims: (80, 22),
            explorer: None,
            explorer_focused: false,
            registers: HashMap::new(),
            search: SearchState::default(),
            options: Options::default(),
            status: None,
            finder: None,
        }
    }

    /// Buffer index the focused window shows.
    pub fn active(&self) -> usize {
        self.windows.get(self.active_win).map(|w| w.buffer).unwrap_or(0)
    }

    /// Point the focused window at buffer `idx` (clamped).
    pub fn set_active(&mut self, idx: usize) {
        let idx = idx.min(self.buffers.len().saturating_sub(1));
        if let Some(w) = self.windows.get_mut(self.active_win) {
            w.buffer = idx;
        }
    }

    pub fn buf(&self) -> Option<&Buffer> {
        self.buffers.get(self.active())
    }

    pub fn buf_mut(&mut self) -> Option<&mut Buffer> {
        let idx = self.active();
        self.buffers.get_mut(idx)
    }

    /// Open a second (or further) window on the current buffer along `dir`.
    pub fn split(&mut self, dir: SplitDir) {
        let buffer = self.active();
        self.split_dir = dir;
        self.windows.insert(self.active_win + 1, Window { buffer });
        self.active_win += 1;
    }

    /// Close the focused window; refuses (returning false) on the last one.
    pub fn close_window(&mut self) -> bool {
        if self.windows.len() <= 1 {
            return false;
        }
        self.windows.remove(self.active_win);
        if self.active_win >= self.windows.len() {
            self.active_win = self.windows.len() - 1;
        }
        true
    }

    /// Cycle focus to the next window.
    pub fn next_window(&mut self) {
        if !self.windows.is_empty() {
            self.active_win = (self.active_win + 1) % self.windows.len();
        }
    }

    /// Cycle focus to the previous window.
    pub fn prev_window(&mut self) {
        if !self.windows.is_empty() {
            self.active_win = (self.active_win + self.windows.len() - 1) % self.windows.len();
        }
    }

    /// Collapse to just the focused window.
    pub fn only(&mut self) {
        if self.active_win < self.windows.len() {
            let w = self.windows.swap_remove(self.active_win);
            self.windows = vec![w];
        } else {
            self.windows.truncate(1);
        }
        self.active_win = 0;
    }

    /// Whether one more window fits along `dir` within `text_dims`.
    pub fn can_split(&self, dir: SplitDir) -> bool {
        let n = self.windows.len() + 1;
        let (cols, rows) = self.text_dims;
        let (total, min) = match dir {
            SplitDir::Horizontal => (rows as usize, MIN_WIN_ROWS as usize),
            SplitDir::Vertical => (cols as usize, MIN_WIN_COLS as usize),
        };
        total.saturating_sub(n - 1) / n >= min
    }

    pub fn info(&mut self, text: impl Into<String>) {
        self.status = Some(StatusMsg { text: text.into(), kind: MsgKind::Info });
    }

    pub fn error(&mut self, text: impl Into<String>) {
        self.status = Some(StatusMsg { text: text.into(), kind: MsgKind::Error });
    }

    /// Open `name` from the vfs (creating an empty buffer for new files) and focus it.
    pub fn open_file(&mut self, vfs: &Vfs, name: &str) {
        if let Some(i) = self.buffers.iter().position(|b| b.name == name) {
            self.set_active(i);
            return;
        }
        let text = vfs.read(name).unwrap_or_default();
        self.buffers.push(Buffer::new(name, &text));
        self.set_active(self.buffers.len() - 1);
    }

    /// Write the active buffer to the vfs; returns (name, byte count) on success.
    pub fn save_active(&mut self, vfs: &mut Vfs) -> Option<(String, usize)> {
        let idx = self.active();
        let buf = self.buffers.get_mut(idx)?;
        let text = buf.text.text();
        let bytes = text.len();
        vfs.write(&buf.name, &text);
        buf.saved_version = buf.text.version();
        Some((buf.name.clone(), bytes))
    }

    /// Close buffer `idx`; refuses (with an error message) when modified unless `force`.
    /// Windows showing it fall back to the nearest remaining buffer.
    pub fn close_buffer(&mut self, idx: usize, force: bool) -> bool {
        let Some(buf) = self.buffers.get(idx) else { return false };
        if buf.modified() && !force {
            self.error(format!("E89: {} has unsaved changes (add ! to override)", buf.name));
            return false;
        }
        self.buffers.remove(idx);
        let last = self.buffers.len().saturating_sub(1);
        for w in &mut self.windows {
            if w.buffer > idx {
                w.buffer -= 1;
            } else if w.buffer == idx {
                w.buffer = w.buffer.min(last);
            }
        }
        if self.buffers.is_empty() {
            self.windows = vec![Window { buffer: 0 }];
            self.active_win = 0;
        }
        true
    }

    pub fn next_buffer(&mut self) {
        if !self.buffers.is_empty() {
            self.set_active((self.active() + 1) % self.buffers.len());
        }
    }

    pub fn prev_buffer(&mut self) {
        if !self.buffers.is_empty() {
            self.set_active((self.active() + self.buffers.len() - 1) % self.buffers.len());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st_with(names: &[&str]) -> EditorState {
        let mut st = EditorState::new();
        for n in names {
            st.buffers.push(Buffer::new(n, "text"));
        }
        st
    }

    #[test]
    fn split_inserts_window_on_same_buffer_and_focuses_it() {
        let mut st = st_with(&["a.rs", "b.rs"]);
        st.set_active(1);
        st.split(SplitDir::Vertical);
        assert_eq!(st.windows.len(), 2);
        assert_eq!(st.active_win, 1);
        assert_eq!(st.split_dir, SplitDir::Vertical);
        assert_eq!(st.windows[0].buffer, 1);
        assert_eq!(st.windows[1].buffer, 1);
    }

    #[test]
    fn close_window_refuses_last_and_clamps_focus() {
        let mut st = st_with(&["a.rs"]);
        assert!(!st.close_window());
        st.split(SplitDir::Horizontal);
        st.split(SplitDir::Horizontal);
        st.active_win = 2;
        assert!(st.close_window());
        assert_eq!(st.active_win, 1);
        assert!(st.close_window());
        assert_eq!(st.windows.len(), 1);
        assert!(!st.close_window());
    }

    #[test]
    fn next_prev_window_cycle() {
        let mut st = st_with(&["a.rs"]);
        st.split(SplitDir::Horizontal);
        st.split(SplitDir::Horizontal);
        st.active_win = 0;
        st.next_window();
        assert_eq!(st.active_win, 1);
        st.prev_window();
        st.prev_window();
        assert_eq!(st.active_win, 2);
        st.next_window();
        assert_eq!(st.active_win, 0);
    }

    #[test]
    fn only_keeps_focused_window() {
        let mut st = st_with(&["a.rs", "b.rs"]);
        st.split(SplitDir::Horizontal);
        st.set_active(1);
        st.only();
        assert_eq!(st.windows.len(), 1);
        assert_eq!(st.active_win, 0);
        assert_eq!(st.active(), 1);
    }

    #[test]
    fn close_buffer_remaps_window_buffer_indices() {
        let mut st = st_with(&["a.rs", "b.rs", "c.rs"]);
        st.split(SplitDir::Horizontal);
        st.windows[0].buffer = 0;
        st.windows[1].buffer = 2;
        assert!(st.close_buffer(1, false));
        assert_eq!(st.windows[0].buffer, 0);
        assert_eq!(st.windows[1].buffer, 1);
        assert!(st.close_buffer(1, false));
        assert_eq!(st.windows[1].buffer, 0);
    }

    #[test]
    fn close_last_buffer_resets_windows() {
        let mut st = st_with(&["a.rs"]);
        st.split(SplitDir::Vertical);
        assert!(st.close_buffer(0, false));
        assert!(st.buffers.is_empty());
        assert_eq!(st.windows.len(), 1);
        assert_eq!(st.active_win, 0);
    }

    #[test]
    fn can_split_respects_minimum_sizes() {
        let mut st = st_with(&["a.rs"]);
        st.text_dims = (80, 22);
        assert!(st.can_split(SplitDir::Horizontal));
        assert!(st.can_split(SplitDir::Vertical));
        st.text_dims = (20, 5);
        assert!(st.can_split(SplitDir::Horizontal));
        assert!(!st.can_split(SplitDir::Vertical));
        st.text_dims = (21, 5);
        assert!(st.can_split(SplitDir::Vertical));
        st.split(SplitDir::Horizontal);
        assert!(!st.can_split(SplitDir::Horizontal));
        st.text_dims = (10, 3);
        st.windows.truncate(1);
        assert!(!st.can_split(SplitDir::Horizontal));
        assert!(!st.can_split(SplitDir::Vertical));
    }
}
