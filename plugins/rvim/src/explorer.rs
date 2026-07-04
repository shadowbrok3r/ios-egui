//! Neo-tree-style file explorer sidebar: lists vfs files, keyboard- and tap-navigable.

use crate::fs::Vfs;
use crate::state::EditorState;
use crate::vim::Key;

/// What the explorer wants the caller to do after a key or tap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExplorerAction {
    /// Keep the explorer focused.
    None,
    /// Open this file in the focused window and move focus to the editor.
    Open(String),
    /// Hide the explorer entirely.
    Close,
    /// Keep the explorer visible but move focus back to the editor.
    FocusEditor,
    /// Delete this file, unless a modified buffer holds it.
    Delete(String),
}

pub struct ExplorerState {
    pub selected: usize,
    /// First visible row when the list overflows the sidebar.
    pub offset: usize,
    /// `Some` while typing a new file name after `a`.
    pub new_name: Option<String>,
    /// A `d` was pressed; the next `d` deletes the selected file.
    pub pending_delete: bool,
}

impl ExplorerState {
    pub fn new() -> Self {
        ExplorerState { selected: 0, offset: 0, new_name: None, pending_delete: false }
    }

    fn clamp(&mut self, len: usize) {
        self.selected = self.selected.min(len.saturating_sub(1));
        self.offset = self.offset.min(self.selected);
    }

    /// Route a key into the explorer (j/k move, Enter/l open, a add, dd delete,
    /// Esc back to editor).
    pub fn handle_key(&mut self, vfs: &mut Vfs, key: Key) -> ExplorerAction {
        let len = vfs.len();
        self.clamp(len);

        if let Some(name) = self.new_name.as_mut() {
            match key {
                Key::Char(c) => name.push(c),
                Key::Backspace => {
                    name.pop();
                }
                Key::Enter => {
                    let name = self.new_name.take().unwrap_or_default();
                    if !name.is_empty() {
                        vfs.write(&name, "");
                        self.selected =
                            vfs.names().position(|n| n == name).unwrap_or(self.selected);
                        return ExplorerAction::Open(name);
                    }
                }
                Key::Esc | Key::Ctrl('c') => self.new_name = None,
                _ => {}
            }
            return ExplorerAction::None;
        }

        if self.pending_delete {
            self.pending_delete = false;
            if key == Key::Char('d') {
                if let Some(name) = vfs.name_at(self.selected) {
                    return ExplorerAction::Delete(name.to_string());
                }
            }
            return ExplorerAction::None;
        }

        match key {
            Key::Char('j') | Key::Down => self.selected = (self.selected + 1).min(len.saturating_sub(1)),
            Key::Char('k') | Key::Up => self.selected = self.selected.saturating_sub(1),
            Key::Char('g') => self.selected = 0,
            Key::Char('G') => self.selected = len.saturating_sub(1),
            Key::Enter | Key::Char('l') | Key::Right => {
                if let Some(name) = vfs.name_at(self.selected) {
                    return ExplorerAction::Open(name.to_string());
                }
            }
            Key::Char('h') | Key::Left | Key::Esc | Key::Ctrl('c') => {
                return ExplorerAction::FocusEditor;
            }
            Key::Char('a') => self.new_name = Some(String::new()),
            Key::Char('d') => self.pending_delete = true,
            Key::Char('q') => return ExplorerAction::Close,
            _ => {}
        }
        self.offset = self.offset.min(self.selected);
        ExplorerAction::None
    }

    /// A tap on sidebar row `row` selects (and on the selected row opens) that file.
    pub fn handle_tap(&mut self, vfs: &Vfs, row: usize) -> ExplorerAction {
        self.clamp(vfs.len());
        // Row 0 is the header; file rows start below it.
        let Some(idx) = row.checked_sub(1).map(|r| r + self.offset) else {
            return ExplorerAction::None;
        };
        let Some(name) = vfs.name_at(idx) else { return ExplorerAction::None };
        if idx == self.selected {
            ExplorerAction::Open(name.to_string())
        } else {
            self.selected = idx;
            ExplorerAction::None
        }
    }
}

/// Apply an explorer action to the editor state.
pub fn apply_action(st: &mut EditorState, vfs: &mut Vfs, action: ExplorerAction) {
    match action {
        ExplorerAction::None => {}
        ExplorerAction::Open(name) => {
            st.open_file(vfs, &name);
            st.explorer_focused = false;
        }
        ExplorerAction::Close => {
            st.explorer = None;
            st.explorer_focused = false;
        }
        ExplorerAction::FocusEditor => st.explorer_focused = false,
        ExplorerAction::Delete(name) => {
            if let Some(i) = st.buffers.iter().position(|b| b.name == name) {
                if st.buffers[i].modified() {
                    st.error(format!("E89: {name} has unsaved changes"));
                    return;
                }
                st.close_buffer(i, false);
            }
            vfs.remove(&name);
            st.info(format!("removed {name}"));
            if let Some(ex) = st.explorer.as_mut() {
                ex.clamp(vfs.len());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::Position;
    use crate::state::Buffer;

    fn vfs(names: &[&str]) -> Vfs {
        let mut v = Vfs::load();
        for n in names {
            v.write(n, "x");
        }
        v
    }

    #[test]
    fn navigation_clamps_and_wraps_nothing() {
        let mut v = vfs(&["a.rs", "b.rs", "c.md"]);
        let mut ex = ExplorerState::new();
        ex.handle_key(&mut v, Key::Char('k'));
        assert_eq!(ex.selected, 0);
        ex.handle_key(&mut v, Key::Char('j'));
        ex.handle_key(&mut v, Key::Down);
        assert_eq!(ex.selected, 2);
        ex.handle_key(&mut v, Key::Char('j'));
        assert_eq!(ex.selected, 2);
        ex.handle_key(&mut v, Key::Char('g'));
        assert_eq!(ex.selected, 0);
        ex.handle_key(&mut v, Key::Char('G'));
        assert_eq!(ex.selected, 2);
        ex.handle_key(&mut v, Key::Up);
        assert_eq!(ex.selected, 1);
    }

    #[test]
    fn enter_and_l_open_selected() {
        let mut v = vfs(&["a.rs", "b.rs"]);
        let mut ex = ExplorerState::new();
        ex.handle_key(&mut v, Key::Char('j'));
        assert_eq!(ex.handle_key(&mut v, Key::Enter), ExplorerAction::Open("b.rs".into()));
        assert_eq!(ex.handle_key(&mut v, Key::Char('l')), ExplorerAction::Open("b.rs".into()));
    }

    #[test]
    fn h_esc_focus_editor_and_q_closes() {
        let mut v = vfs(&["a.rs"]);
        let mut ex = ExplorerState::new();
        assert_eq!(ex.handle_key(&mut v, Key::Char('h')), ExplorerAction::FocusEditor);
        assert_eq!(ex.handle_key(&mut v, Key::Esc), ExplorerAction::FocusEditor);
        assert_eq!(ex.handle_key(&mut v, Key::Char('q')), ExplorerAction::Close);
    }

    #[test]
    fn add_flow_creates_and_opens() {
        let mut v = vfs(&["z.rs"]);
        let mut ex = ExplorerState::new();
        ex.handle_key(&mut v, Key::Char('a'));
        assert_eq!(ex.new_name.as_deref(), Some(""));
        for c in "necw".chars() {
            ex.handle_key(&mut v, Key::Char(c));
        }
        ex.handle_key(&mut v, Key::Backspace);
        ex.handle_key(&mut v, Key::Char('.'));
        ex.handle_key(&mut v, Key::Char('r'));
        ex.handle_key(&mut v, Key::Char('s'));
        assert_eq!(ex.new_name.as_deref(), Some("nec.rs"));
        assert_eq!(ex.handle_key(&mut v, Key::Enter), ExplorerAction::Open("nec.rs".into()));
        assert!(v.exists("nec.rs"));
        assert_eq!(v.name_at(ex.selected), Some("nec.rs"));
        assert!(ex.new_name.is_none());
    }

    #[test]
    fn add_flow_esc_cancels_and_empty_enter_does_nothing() {
        let mut v = vfs(&["a.rs"]);
        let mut ex = ExplorerState::new();
        ex.handle_key(&mut v, Key::Char('a'));
        assert_eq!(ex.handle_key(&mut v, Key::Enter), ExplorerAction::None);
        assert!(ex.new_name.is_none());
        ex.handle_key(&mut v, Key::Char('a'));
        ex.handle_key(&mut v, Key::Char('x'));
        ex.handle_key(&mut v, Key::Esc);
        assert!(ex.new_name.is_none());
        assert!(!v.exists("x"));
        assert_eq!(v.len(), 1);
    }

    #[test]
    fn dd_deletes_and_other_keys_disarm() {
        let mut v = vfs(&["a.rs", "b.rs"]);
        let mut ex = ExplorerState::new();
        ex.handle_key(&mut v, Key::Char('d'));
        assert!(ex.pending_delete);
        assert_eq!(ex.handle_key(&mut v, Key::Char('j')), ExplorerAction::None);
        assert!(!ex.pending_delete);
        assert_eq!(ex.selected, 0, "disarming key is not replayed");
        ex.handle_key(&mut v, Key::Char('d'));
        assert_eq!(ex.handle_key(&mut v, Key::Char('d')), ExplorerAction::Delete("a.rs".into()));
        assert!(!ex.pending_delete);
    }

    #[test]
    fn tap_selects_then_opens() {
        let v = vfs(&["a.rs", "b.rs", "c.rs"]);
        let mut ex = ExplorerState::new();
        assert_eq!(ex.handle_tap(&v, 0), ExplorerAction::None);
        assert_eq!(ex.handle_tap(&v, 2), ExplorerAction::None);
        assert_eq!(ex.selected, 1);
        assert_eq!(ex.handle_tap(&v, 2), ExplorerAction::Open("b.rs".into()));
        assert_eq!(ex.handle_tap(&v, 9), ExplorerAction::None);
    }

    #[test]
    fn tap_respects_offset() {
        let v = vfs(&["a.rs", "b.rs", "c.rs", "d.rs"]);
        let mut ex = ExplorerState::new();
        ex.selected = 2;
        ex.offset = 2;
        assert_eq!(ex.handle_tap(&v, 1), ExplorerAction::Open("c.rs".into()));
    }

    #[test]
    fn delete_refuses_modified_buffer_and_closes_clean_one() {
        let mut st = EditorState::new();
        let mut v = vfs(&["a.rs", "b.rs"]);
        st.explorer = Some(ExplorerState::new());
        st.buffers.push(Buffer::new("a.rs", "x"));
        st.buffers[0].text.insert_char(Position::default(), 'y');
        apply_action(&mut st, &mut v, ExplorerAction::Delete("a.rs".into()));
        assert!(v.exists("a.rs"));
        assert_eq!(st.buffers.len(), 1);
        assert!(st.status.as_ref().is_some_and(|m| m.text.contains("unsaved")));

        st.buffers[0].saved_version = st.buffers[0].text.version();
        apply_action(&mut st, &mut v, ExplorerAction::Delete("a.rs".into()));
        assert!(!v.exists("a.rs"));
        assert!(st.buffers.is_empty());

        apply_action(&mut st, &mut v, ExplorerAction::Delete("b.rs".into()));
        assert!(!v.exists("b.rs"));
    }

    #[test]
    fn unicode_names_navigate_tap_and_delete() {
        let mut v = vfs(&["héllo wörld.rs", "中文ファイル.md", "🎉.rs"]);
        let mut ex = ExplorerState::new();
        ex.handle_key(&mut v, Key::Char('G'));
        let last = v.name_at(ex.selected).unwrap().to_string();
        assert_eq!(ex.handle_key(&mut v, Key::Enter), ExplorerAction::Open(last));
        assert_eq!(ex.handle_tap(&v, 1), ExplorerAction::None);
        assert_eq!(ex.selected, 0);
        let first = v.name_at(0).unwrap().to_string();
        assert_eq!(ex.handle_tap(&v, 1), ExplorerAction::Open(first.clone()));
        ex.handle_key(&mut v, Key::Char('d'));
        assert_eq!(ex.handle_key(&mut v, Key::Char('d')), ExplorerAction::Delete(first));
    }

    #[test]
    fn empty_vfs_keys_never_panic() {
        let mut v = Vfs::load();
        let mut ex = ExplorerState::new();
        for key in [
            Key::Char('j'),
            Key::Char('k'),
            Key::Char('g'),
            Key::Char('G'),
            Key::Enter,
            Key::Char('l'),
            Key::Char('d'),
            Key::Char('d'),
            Key::Down,
            Key::Up,
        ] {
            let a = ex.handle_key(&mut v, key);
            assert!(matches!(a, ExplorerAction::None | ExplorerAction::FocusEditor));
        }
        assert_eq!(ex.handle_tap(&v, 0), ExplorerAction::None);
        assert_eq!(ex.handle_tap(&v, 5), ExplorerAction::None);
        assert_eq!(ex.selected, 0);
    }

    #[test]
    fn apply_action_open_close_focus() {
        let mut st = EditorState::new();
        let mut v = vfs(&["a.rs"]);
        st.explorer = Some(ExplorerState::new());
        st.explorer_focused = true;
        apply_action(&mut st, &mut v, ExplorerAction::Open("a.rs".into()));
        assert_eq!(st.buf().unwrap().name, "a.rs");
        assert!(!st.explorer_focused);
        assert!(st.explorer.is_some());
        st.explorer_focused = true;
        apply_action(&mut st, &mut v, ExplorerAction::FocusEditor);
        assert!(!st.explorer_focused);
        assert!(st.explorer.is_some());
        st.explorer_focused = true;
        apply_action(&mut st, &mut v, ExplorerAction::Close);
        assert!(st.explorer.is_none());
        assert!(!st.explorer_focused);
    }
}
