//! Shared editor state: open buffers, registers, search, options, and status messages.

use std::collections::HashMap;

use crate::buffer::{Position, TextBuffer};
use crate::finder::FinderState;
use crate::fs::Vfs;

/// Yanked or deleted text plus whether it came from a linewise operation.
#[derive(Clone, Default)]
pub struct Register {
    pub text: String,
    pub linewise: bool,
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

pub struct EditorState {
    pub buffers: Vec<Buffer>,
    pub active: usize,
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
            active: 0,
            registers: HashMap::new(),
            search: SearchState::default(),
            options: Options::default(),
            status: None,
            finder: None,
        }
    }

    pub fn buf(&self) -> Option<&Buffer> {
        self.buffers.get(self.active)
    }

    pub fn buf_mut(&mut self) -> Option<&mut Buffer> {
        self.buffers.get_mut(self.active)
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
            self.active = i;
            return;
        }
        let text = vfs.read(name).unwrap_or_default();
        self.buffers.push(Buffer::new(name, &text));
        self.active = self.buffers.len() - 1;
    }

    /// Write the active buffer to the vfs; returns (name, byte count) on success.
    pub fn save_active(&mut self, vfs: &mut Vfs) -> Option<(String, usize)> {
        let buf = self.buffers.get_mut(self.active)?;
        let text = buf.text.text();
        let bytes = text.len();
        vfs.write(&buf.name, &text);
        buf.saved_version = buf.text.version();
        Some((buf.name.clone(), bytes))
    }

    /// Close buffer `idx`; refuses (with an error message) when modified unless `force`.
    pub fn close_buffer(&mut self, idx: usize, force: bool) -> bool {
        let Some(buf) = self.buffers.get(idx) else { return false };
        if buf.modified() && !force {
            self.error(format!("E89: {} has unsaved changes (add ! to override)", buf.name));
            return false;
        }
        self.buffers.remove(idx);
        if self.active >= idx && self.active > 0 {
            self.active -= 1;
        }
        true
    }

    pub fn next_buffer(&mut self) {
        if !self.buffers.is_empty() {
            self.active = (self.active + 1) % self.buffers.len();
        }
    }

    pub fn prev_buffer(&mut self) {
        if !self.buffers.is_empty() {
            self.active = (self.active + self.buffers.len() - 1) % self.buffers.len();
        }
    }
}
