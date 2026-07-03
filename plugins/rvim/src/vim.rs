//! The modal engine: vim grammar (count → operator → count → motion/text-object), modes,
//! registers, marks, search, dot-repeat, and ex command execution.

use egui_ios_plugin_sdk::HostHandle;

use crate::buffer::Position;
use crate::fs::Vfs;
use crate::state::EditorState;

/// A decoded input key. Printable chars arrive as `Char`; Ctrl-chords as `Ctrl` with the
/// lowercase letter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Ctrl(char),
    Esc,
    Enter,
    Backspace,
    Delete,
    Tab,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    Visual { linewise: bool },
    Replace,
    /// Ex command line (`:`).
    Command,
    /// Search line (`/` or `?`).
    Search { backwards: bool },
}

pub struct VimEngine {
    mode: Mode,
    /// Command/search line content and char cursor while in Command/Search mode.
    cmdline: String,
    cmdline_cursor: usize,
    /// Visual-mode anchor position.
    anchor: Position,
    /// Keys collected toward the current normal-mode action, for showcmd.
    pending: String,
}

impl VimEngine {
    pub fn new() -> Self {
        VimEngine {
            mode: Mode::Normal,
            cmdline: String::new(),
            cmdline_cursor: 0,
            anchor: Position::default(),
            pending: String::new(),
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Statusline chip label for the current mode.
    pub fn mode_label(&self) -> &'static str {
        match self.mode {
            Mode::Normal => "NORMAL",
            Mode::Insert => "INSERT",
            Mode::Visual { linewise: false } => "VISUAL",
            Mode::Visual { linewise: true } => "V-LINE",
            Mode::Replace => "REPLACE",
            Mode::Command => "COMMAND",
            Mode::Search { .. } => "SEARCH",
        }
    }

    /// `(prefix, text, char cursor)` of the active command/search line, when in one.
    pub fn cmdline(&self) -> Option<(char, &str, usize)> {
        match self.mode {
            Mode::Command => Some((':', &self.cmdline, self.cmdline_cursor)),
            Mode::Search { backwards } => {
                Some((if backwards { '?' } else { '/' }, &self.cmdline, self.cmdline_cursor))
            }
            _ => None,
        }
    }

    /// In-progress normal-mode keys (count/operator/register) for the showcmd area.
    pub fn pending_display(&self) -> &str {
        &self.pending
    }

    /// Visual selection as `(start, end, linewise)` in buffer order, when in visual mode.
    pub fn visual_range(&self, cursor: Position) -> Option<(Position, Position, bool)> {
        match self.mode {
            Mode::Visual { linewise } => {
                let (a, b) = if self.anchor <= cursor { (self.anchor, cursor) } else { (cursor, self.anchor) };
                Some((a, b, linewise))
            }
            _ => None,
        }
    }

    /// Feed one key through the modal state machine, mutating the editor state.
    pub fn handle_key(&mut self, st: &mut EditorState, vfs: &mut Vfs, host: &HostHandle, key: Key) {
        // STUB: implemented by the vim module owner.
        let _ = (st, vfs, host, key);
    }
}
