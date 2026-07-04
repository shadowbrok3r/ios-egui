//! The modal engine: vim grammar (count → operator → count → motion/text-object), modes,
//! registers, marks, search, dot-repeat, and ex command execution.

use std::cell::Cell;

use egui_ios_plugin_sdk::HostHandle;

use crate::buffer::{Position, TextBuffer};
use crate::explorer::{self, ExplorerState};
use crate::finder::{FinderAction, FinderState, FinderTarget};
use crate::fs::Vfs;
use crate::help;
use crate::state::{EditorState, RegKind, Register, SplitDir};

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

/// Shape of a visual selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VisualKind {
    Char,
    Line,
    Block,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    Visual { kind: VisualKind },
    Replace,
    /// Ex command line (`:`).
    Command,
    /// Search line (`/` or `?`).
    Search { backwards: bool },
}

/// Viewport scroll requested by a key; ui.rs resolves it against the real viewport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollRequest {
    Center,
    Top,
    Bottom,
    HalfDown,
    HalfUp,
    PageDown,
    PageUp,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Op {
    Delete,
    Change,
    Yank,
    Indent,
    Dedent,
    Lower,
    Upper,
    Comment,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MotionKind {
    Exclusive,
    Inclusive,
    Linewise,
}

/// What the next key is interpreted as.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Await {
    None,
    /// After `"`: a register name.
    Register,
    /// After f/F/t/T: the target char.
    Find(char),
    /// After r: the replacement char.
    Replace,
    /// After m: a mark name.
    Mark,
    /// After backtick: a mark to jump to exactly.
    JumpExact,
    /// After ': a mark whose line to jump to.
    JumpLine,
    /// After g or z: the second key of the chord.
    Prefix(char),
    /// After i/a with an operator (or in visual): a text-object char; true = around.
    Object(bool),
    /// Space leader pending at this menu node.
    Leader(LeaderNode),
    /// After Ctrl+w: a window-command key.
    CtrlW,
    /// After q: the macro register to record into.
    MacroReg,
    /// After @: the macro register to replay.
    MacroPlay,
}

/// Which leader menu the next key selects from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LeaderNode {
    Root,
    Find,
    Split,
}

/// Ceiling on any typed count (vim caps too); keeps count arithmetic and repeat
/// loops from exploding on absurd input like `99999999999999999999p`.
const MAX_COUNT: usize = 1_000_000;

/// Budget for text materialized by one paste beyond a single copy of the register.
const PASTE_MAX_BYTES: usize = 4 << 20;

/// Ceiling on dot-repeat replays of one change.
const MAX_REPEAT: usize = 10_000;

/// Total key budget across one macro replay, nested plays included.
const MAX_MACRO_STEPS: usize = 100_000;

/// Nesting ceiling for macros that invoke macros.
const MAX_REPLAY_DEPTH: usize = 16;

/// Which-key hints for the root leader menu.
const LEADER_ROOT: &[(&str, &str)] = &[
    ("e", "explorer"),
    ("f", "find…"),
    ("o", "other window"),
    ("t", "split…"),
    ("w", "save"),
    ("c", "close buffer"),
    ("q", "close window"),
    ("h", "help"),
];

/// Which-key hints after Space f.
const LEADER_FIND: &[(&str, &str)] = &[("f", "find file"), ("b", "buffers")];

/// Which-key hints after Space t.
const LEADER_SPLIT: &[(&str, &str)] =
    &[("h", "split below"), ("v", "split right"), ("q", "close window")];

/// Which-key hints after g.
const G_MENU: &[(&str, &str)] = &[
    ("g", "first line (gg)"),
    ("e", "prev word end"),
    ("u", "lowercase…"),
    ("U", "uppercase…"),
    ("c", "toggle comment…"),
];

/// Which-key hints after z.
const Z_MENU: &[(&str, &str)] = &[
    ("z", "center cursor line"),
    ("t", "cursor line to top"),
    ("b", "cursor line to bottom"),
];

/// Which-key hints after Ctrl+w.
const CTRLW_MENU: &[(&str, &str)] = &[
    ("s", "split below"),
    ("v", "split right"),
    ("w", "next window"),
    ("h", "prev window"),
    ("l", "next window"),
    ("q", "close window"),
];

/// Which-key hints after " (register select).
const REG_MENU: &[(&str, &str)] = &[
    ("a-z", "named register"),
    ("0", "last yank"),
    ("\"", "unnamed register"),
];

/// Which-key hints after m.
const MARK_SET: &[(&str, &str)] = &[("a-z", "set mark here")];

/// Which-key hints after ` or '.
const MARK_JUMP: &[(&str, &str)] = &[("a-z", "jump to mark")];

/// Which-key hints after i/a in operator-pending or visual mode.
const OBJ_MENU: &[(&str, &str)] = &[
    ("w", "word"),
    ("W", "WORD"),
    ("(", "parens (also b)"),
    ("{", "braces (also B)"),
    ("[", "brackets"),
    ("<", "angle brackets"),
    ("\"", "double quotes"),
    ("'", "single quotes"),
    ("`", "backticks"),
];

/// Which-key hints after q.
const MACRO_REC: &[(&str, &str)] = &[
    ("a-z", "record into register"),
    ("A-Z", "append to register"),
];

/// Which-key hints after @.
const MACRO_PLAY: &[(&str, &str)] = &[
    ("a-z", "replay macro"),
    ("@", "repeat last replay"),
];

/// Which-key hints after f/F/t/T.
const FIND_HINT: &[(&str, &str)] = &[("a-z…", "any character: jump to it on this line")];

/// Which-key hints after r.
const REPLACE_HINT: &[(&str, &str)] = &[("a-z…", "any character: overwrite with it")];

/// Motions shown while an operator is pending; the doubled-key row is prepended per op.
const OP_MOTIONS: &[(&str, &str)] = &[
    ("w e b", "word / end / back"),
    ("i", "inside object…"),
    ("a", "around object…"),
    ("f F t T", "to / till char"),
    ("$ 0 ^", "line end / start"),
    ("G", "last line (gg first)"),
    ("{ }", "paragraph back / fwd"),
    ("%", "matching bracket"),
];

/// Which-key panel content for one pending-key state.
pub struct KeyHints {
    pub title: &'static str,
    pub entries: Vec<(&'static str, &'static str)>,
    /// Menus show instantly; prefix cheatsheets wait for a typing pause.
    pub immediate: bool,
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
    count: usize,
    opcount: usize,
    op: Option<Op>,
    reg: Option<char>,
    awaiting: Await,
    /// Last f/F/t/T as (kind, target char), for ; and , repeats.
    last_find: Option<(char, char)>,
    /// A literal 'j' was just typed in insert mode (jk quick-escape).
    insert_j: bool,
    /// Keys of the in-progress change, for dot-repeat.
    keybuf: Vec<Key>,
    /// Buffer version when `keybuf` started.
    keybuf_version: u64,
    /// The in-progress action must not become the dot-repeat target.
    norepeat: bool,
    last_change: Vec<Key>,
    replaying: bool,
    scroll_req: Cell<Option<ScrollRequest>>,
    /// Cursor when an incremental search started, restored on Esc.
    search_origin: Position,
    /// Pattern before the incremental search started.
    search_prev: String,
    /// Blockwise insert to replicate across its lines when insert mode ends.
    block_insert: Option<BlockInsert>,
    /// Recorded macro key streams by register name.
    macros: std::collections::HashMap<char, Vec<Key>>,
    /// Active recording: target register and keys captured so far.
    recording: Option<(char, Vec<Key>)>,
    /// Register of the last @ replay, for @@.
    last_macro: Option<char>,
    replay_depth: usize,
    replay_steps: usize,
}

/// A pending blockwise insert: where typing started and which lines replicate it.
struct BlockInsert {
    /// Buffer index the insert started in; a focus change cancels replication.
    buf: usize,
    start: Position,
    top: usize,
    bot: usize,
    col: usize,
    /// Pad short lines with spaces out to `col` (block A).
    pad: bool,
    /// Insert at each line's end (block $ A).
    to_eol: bool,
}

/// Block selection rectangle: lines `top..=bot`, char cols `c1..=c2` or to line ends.
struct BlockSpan {
    top: usize,
    bot: usize,
    c1: usize,
    c2: usize,
    to_eol: bool,
}

impl BlockSpan {
    /// Rect cols on a line of `len` chars as `[start, end)`; None when the line ends first.
    fn cols(&self, len: usize) -> Option<(usize, usize)> {
        if len <= self.c1 {
            return None;
        }
        let end = if self.to_eol { len } else { (self.c2 + 1).min(len) };
        (end > self.c1).then_some((self.c1, end))
    }
}

impl VimEngine {
    pub fn new() -> Self {
        VimEngine {
            mode: Mode::Normal,
            cmdline: String::new(),
            cmdline_cursor: 0,
            anchor: Position::default(),
            pending: String::new(),
            count: 0,
            opcount: 0,
            op: None,
            reg: None,
            awaiting: Await::None,
            last_find: None,
            insert_j: false,
            keybuf: Vec::new(),
            keybuf_version: 0,
            norepeat: false,
            last_change: Vec::new(),
            replaying: false,
            scroll_req: Cell::new(None),
            search_origin: Position::default(),
            search_prev: String::new(),
            block_insert: None,
            macros: std::collections::HashMap::new(),
            recording: None,
            last_macro: None,
            replay_depth: 0,
            replay_steps: 0,
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
            Mode::Visual { kind: VisualKind::Char } => "VISUAL",
            Mode::Visual { kind: VisualKind::Line } => "V-LINE",
            Mode::Visual { kind: VisualKind::Block } => "V-BLOCK",
            Mode::Replace => "REPLACE",
            Mode::Command => "COMMAND",
            Mode::Search { .. } => "SEARCH",
        }
    }

    /// Register a macro is being recorded into, for the statusline.
    pub fn recording_reg(&self) -> Option<char> {
        self.recording.as_ref().map(|(c, _)| *c)
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

    /// Visual selection as `(start, end, kind)` in buffer order, when in visual mode.
    pub fn visual_range(&self, cursor: Position) -> Option<(Position, Position, VisualKind)> {
        match self.mode {
            Mode::Visual { kind } => {
                let (a, b) = if self.anchor <= cursor { (self.anchor, cursor) } else { (cursor, self.anchor) };
                Some((a, b, kind))
            }
            _ => None,
        }
    }

    /// Consume the pending viewport scroll request, if a key produced one.
    pub fn take_scroll_request(&self) -> Option<ScrollRequest> {
        self.scroll_req.take()
    }

    /// Which-key panel for whatever key state is pending: leader menus, prefix chords
    /// (g/z/Ctrl+w), pending operators, text objects, registers, marks, and macros.
    pub fn key_hints(&self) -> Option<KeyHints> {
        let menu = |title, entries: &'static [(&'static str, &'static str)], immediate| {
            Some(KeyHints { title, entries: entries.to_vec(), immediate })
        };
        match self.awaiting {
            Await::Leader(LeaderNode::Root) => menu(" space", LEADER_ROOT, true),
            Await::Leader(LeaderNode::Find) => menu(" space f", LEADER_FIND, true),
            Await::Leader(LeaderNode::Split) => menu(" space t", LEADER_SPLIT, true),
            Await::Prefix('g') => menu(" g", G_MENU, false),
            Await::Prefix('z') => menu(" z", Z_MENU, false),
            Await::Prefix(_) => None,
            Await::CtrlW => menu(" ctrl+w", CTRLW_MENU, true),
            Await::Register => menu(" \"", REG_MENU, false),
            Await::Mark => menu(" m", MARK_SET, false),
            Await::JumpExact => menu(" `", MARK_JUMP, false),
            Await::JumpLine => menu(" '", MARK_JUMP, false),
            Await::Object(false) => menu(" i", OBJ_MENU, false),
            Await::Object(true) => menu(" a", OBJ_MENU, false),
            Await::Find(kind) => {
                let title = match kind {
                    'f' => " f",
                    'F' => " F",
                    't' => " t",
                    _ => " T",
                };
                menu(title, FIND_HINT, false)
            }
            Await::Replace => menu(" r", REPLACE_HINT, false),
            Await::MacroReg => menu(" q", MACRO_REC, false),
            Await::MacroPlay => menu(" @", MACRO_PLAY, false),
            Await::None => self.op.map(op_menu),
        }
    }

    fn idle(&self) -> bool {
        self.count == 0
            && self.opcount == 0
            && self.op.is_none()
            && self.reg.is_none()
            && self.awaiting == Await::None
    }

    fn reset(&mut self) {
        self.count = 0;
        self.opcount = 0;
        self.op = None;
        self.reg = None;
        self.awaiting = Await::None;
        self.pending.clear();
    }

    fn pend(&mut self, c: char) {
        self.pending.push(c);
    }

    fn total_count(&self) -> usize {
        self.count.max(1).saturating_mul(self.opcount.max(1)).min(MAX_COUNT)
    }

    /// Explicit count when one was typed, e.g. to distinguish G from 5G.
    fn explicit_count(&self) -> Option<usize> {
        if self.count > 0 || self.opcount > 0 { Some(self.total_count()) } else { None }
    }

    fn set_mode(&mut self, host: &HostHandle, mode: Mode) {
        if self.mode != mode {
            host.haptic(6);
        }
        self.mode = mode;
    }

    fn err(&self, st: &mut EditorState, host: &HostHandle, msg: impl Into<String>) {
        st.error(msg);
        host.haptic(6);
    }

    /// Feed one key through the modal state machine, mutating the editor state.
    pub fn handle_key(&mut self, st: &mut EditorState, vfs: &mut Vfs, host: &HostHandle, key: Key) {
        // Macros capture pressed keys only; replayed keys regenerate at play time.
        if !self.replaying {
            if let Some((_, keys)) = &mut self.recording {
                keys.push(key);
            }
        }
        if st.finder.is_some() {
            let target =
                st.finder.as_ref().map(|f| f.target).unwrap_or(FinderTarget::Files);
            let action =
                st.finder.as_mut().map(|f| f.handle_key(key)).unwrap_or(FinderAction::None);
            match action {
                FinderAction::Close => st.finder = None,
                FinderAction::Open(name) => {
                    match target {
                        FinderTarget::Files => st.open_file(vfs, &name),
                        // Buffer picker: switch windows without touching the vfs.
                        FinderTarget::Buffers => {
                            if let Some(i) = st.buffers.iter().position(|b| b.name == name) {
                                st.set_active(i);
                            }
                        }
                    }
                    st.finder = None;
                }
                FinderAction::None => {}
            }
            return;
        }

        if st.explorer_focused && !matches!(self.mode, Mode::Command | Mode::Search { .. }) {
            let action = match st.explorer.as_mut() {
                Some(ex) => ex.handle_key(vfs, key),
                None => {
                    st.explorer_focused = false;
                    return;
                }
            };
            explorer::apply_action(st, vfs, action);
            return;
        }

        let ver_before = st.buf().map(|b| b.text.version());
        if !self.replaying {
            if matches!(self.mode, Mode::Normal) && self.idle() {
                self.keybuf.clear();
                self.keybuf_version = ver_before.unwrap_or(0);
                self.norepeat = match key {
                    Key::Char('u' | '.' | ':' | '/' | '?' | 'n' | 'N' | '*' | '#' | ' ' | 'q' | '@') => true,
                    Key::Ctrl('v') => false,
                    Key::Ctrl(_) => true,
                    _ => false,
                };
            }
            self.keybuf.push(key);
        }

        match self.mode {
            Mode::Normal => self.normal_key(st, vfs, host, key),
            Mode::Visual { kind } => self.visual_key(st, host, key, kind),
            Mode::Insert => self.insert_key(st, host, key),
            Mode::Replace => self.replace_key(st, host, key),
            Mode::Command => self.command_key(st, vfs, host, key),
            Mode::Search { backwards } => self.search_key(st, host, key, backwards),
        }

        if let Some(buf) = st.buf_mut() {
            let allow_end = matches!(self.mode, Mode::Insert | Mode::Replace);
            buf.cursor = buf.text.clamp(buf.cursor, allow_end);
        }

        if !self.replaying && matches!(self.mode, Mode::Normal) && self.idle() {
            let ver_now = st.buf().map(|b| b.text.version()).unwrap_or(0);
            if !self.norepeat && ver_before.is_some() && ver_now != self.keybuf_version {
                self.last_change = std::mem::take(&mut self.keybuf);
            } else {
                self.keybuf.clear();
            }
        }
    }

    /// Replay the recorded last change `n` times.
    fn repeat_last(&mut self, st: &mut EditorState, vfs: &mut Vfs, host: &HostHandle, n: usize) {
        if self.last_change.is_empty() {
            return;
        }
        let n = n.min(MAX_REPEAT);
        let keys = self.last_change.clone();
        self.replaying = true;
        for _ in 0..n {
            for &k in &keys {
                self.handle_key(st, vfs, host, k);
            }
        }
        self.replaying = false;
    }

    /// Replay macro `reg` `n` times, bounded by nesting depth and a total key budget.
    fn play_macro(
        &mut self,
        st: &mut EditorState,
        vfs: &mut Vfs,
        host: &HostHandle,
        reg: char,
        n: usize,
    ) {
        let keys = match self.macros.get(&reg) {
            Some(keys) if !keys.is_empty() => keys.clone(),
            _ => {
                self.err(st, host, format!("E749: register @{reg} is empty"));
                return;
            }
        };
        if self.replay_depth >= MAX_REPLAY_DEPTH {
            return;
        }
        self.last_macro = Some(reg);
        self.replay_depth += 1;
        let was_replaying = self.replaying;
        self.replaying = true;
        'runs: for _ in 0..n {
            for &k in &keys {
                if self.replay_steps >= MAX_MACRO_STEPS {
                    break 'runs;
                }
                self.replay_steps += 1;
                self.handle_key(st, vfs, host, k);
            }
        }
        self.replaying = was_replaying;
        self.replay_depth -= 1;
        if self.replay_depth == 0 {
            self.replay_steps = 0;
        }
    }

    fn open_finder(&mut self, st: &mut EditorState, vfs: &Vfs, host: &HostHandle) {
        st.finder = Some(FinderState::new(vfs.list()));
        host.haptic(6);
        self.reset();
    }

    fn open_buffer_picker(&mut self, st: &mut EditorState, host: &HostHandle) {
        let names = st.buffers.iter().map(|b| b.name.clone()).collect();
        st.finder = Some(FinderState::buffers(names));
        host.haptic(6);
        self.reset();
    }

    /// Split the focused window, refusing when the resulting windows would be too small.
    fn do_split(&mut self, st: &mut EditorState, host: &HostHandle, dir: SplitDir) {
        if st.buf().is_none() {
            return;
        }
        if st.can_split(dir) {
            st.split(dir);
        } else {
            self.err(st, host, "E36: not enough room");
        }
    }

    /// Show or hide the explorer sidebar; showing it also focuses it.
    fn toggle_explorer(&mut self, st: &mut EditorState, host: &HostHandle) {
        if st.explorer.is_some() {
            st.explorer = None;
            st.explorer_focused = false;
        } else if st.text_dims.0 < 30 {
            self.err(st, host, "E36: not enough room for the explorer");
        } else {
            st.explorer = Some(ExplorerState::new());
            st.explorer_focused = true;
        }
    }

    /// Resolve the key after a pending leader node.
    fn leader_key(
        &mut self,
        st: &mut EditorState,
        vfs: &mut Vfs,
        host: &HostHandle,
        node: LeaderNode,
        key: Key,
    ) {
        use LeaderNode::*;
        match (node, key) {
            (Root, Key::Char('e')) => {
                self.toggle_explorer(st, host);
                self.reset();
            }
            (Root, Key::Char('f')) => self.awaiting = Await::Leader(Find),
            (Root, Key::Char('o')) => {
                if st.explorer.is_some() {
                    st.explorer_focused = !st.explorer_focused;
                } else {
                    st.next_window();
                }
                self.reset();
            }
            (Root, Key::Char('t')) => self.awaiting = Await::Leader(Split),
            (Root, Key::Char('w')) => {
                self.execute_cmd(st, vfs, host, "w");
                self.reset();
            }
            (Root, Key::Char('c')) => {
                self.execute_cmd(st, vfs, host, "bd");
                self.reset();
            }
            (Root, Key::Char('q')) => {
                if st.windows.len() > 1 {
                    st.close_window();
                } else {
                    self.execute_cmd(st, vfs, host, "q");
                }
                self.reset();
            }
            (Root, Key::Char('h')) => {
                self.execute_cmd(st, vfs, host, "help");
                self.reset();
            }
            (Find, Key::Char('f')) => self.open_finder(st, vfs, host),
            (Find, Key::Char('b')) => self.open_buffer_picker(st, host),
            (Split, Key::Char('h')) => {
                self.do_split(st, host, SplitDir::Horizontal);
                self.reset();
            }
            (Split, Key::Char('v')) => {
                self.do_split(st, host, SplitDir::Vertical);
                self.reset();
            }
            (Split, Key::Char('q')) => {
                st.close_window();
                self.reset();
            }
            _ => self.reset(),
        }
    }

    /// Resolve the key after Ctrl+w.
    fn ctrl_w_key(&mut self, st: &mut EditorState, host: &HostHandle, key: Key) {
        match key {
            Key::Char('w') => st.next_window(),
            Key::Char('s') => self.do_split(st, host, SplitDir::Horizontal),
            Key::Char('v') => self.do_split(st, host, SplitDir::Vertical),
            Key::Char('q' | 'c') => {
                st.close_window();
            }
            Key::Char('j' | 'l') | Key::Down | Key::Right => st.next_window(),
            Key::Char('k' | 'h') | Key::Up | Key::Left => st.prev_window(),
            _ => {}
        }
        self.reset();
    }
}

// ---------------------------------------------------------------------------
// Normal mode
// ---------------------------------------------------------------------------

impl VimEngine {
    fn normal_key(&mut self, st: &mut EditorState, vfs: &mut Vfs, host: &HostHandle, key: Key) {
        // Buffer-less (dashboard): only commands, the finder, and the leader work.
        if st.buf().is_none() {
            match (std::mem::replace(&mut self.awaiting, Await::None), key) {
                (Await::Leader(node), k) => self.leader_key(st, vfs, host, node, k),
                (Await::None, Key::Char(':')) => self.enter_cmdline(host),
                (Await::None, Key::Ctrl('p')) => self.open_finder(st, vfs, host),
                (Await::None, Key::Char(' ')) => self.awaiting = Await::Leader(LeaderNode::Root),
                _ => self.reset(),
            }
            return;
        }

        match std::mem::replace(&mut self.awaiting, Await::None) {
            Await::None => {}
            Await::Register => {
                if let Key::Char(c @ ('a'..='z' | 'A'..='Z' | '0'..='9')) = key {
                    self.reg = Some(c.to_ascii_lowercase());
                    self.pend(c);
                } else {
                    self.reset();
                }
                return;
            }
            Await::Find(kind) => {
                if let Key::Char(c) = key {
                    self.last_find = Some((kind, c));
                    self.do_find(st, host, kind, c);
                } else {
                    self.reset();
                }
                return;
            }
            Await::Replace => {
                if let Key::Char(c) = key {
                    self.do_replace(st, c);
                }
                self.reset();
                return;
            }
            Await::Mark => {
                if let (Key::Char(c @ 'a'..='z'), Some(buf)) = (key, st.buf_mut()) {
                    buf.marks.insert(c, buf.cursor);
                }
                self.reset();
                return;
            }
            Await::JumpExact | Await::JumpLine => return self.jump_mark(st, host, key),
            Await::Prefix('g') => return self.g_key(st, host, key),
            Await::Prefix('z') => {
                match key {
                    Key::Char('z') => self.scroll_req.set(Some(ScrollRequest::Center)),
                    Key::Char('t') => self.scroll_req.set(Some(ScrollRequest::Top)),
                    Key::Char('b') => self.scroll_req.set(Some(ScrollRequest::Bottom)),
                    _ => {}
                }
                self.reset();
                return;
            }
            Await::Prefix(_) => {
                self.reset();
                return;
            }
            Await::Object(around) => {
                if let Key::Char(c) = key {
                    self.do_object(st, host, c, around);
                } else {
                    self.reset();
                }
                return;
            }
            Await::Leader(node) => return self.leader_key(st, vfs, host, node, key),
            Await::CtrlW => return self.ctrl_w_key(st, host, key),
            Await::MacroReg => {
                match key {
                    Key::Char(c @ 'a'..='z') => self.recording = Some((c, Vec::new())),
                    // Uppercase appends to the existing macro.
                    Key::Char(c @ 'A'..='Z') => {
                        let lc = c.to_ascii_lowercase();
                        let keys = self.macros.get(&lc).cloned().unwrap_or_default();
                        self.recording = Some((lc, keys));
                    }
                    _ => {}
                }
                self.reset();
                return;
            }
            Await::MacroPlay => {
                let n = self.total_count();
                let reg = match key {
                    Key::Char('@') => self.last_macro,
                    Key::Char(c @ ('a'..='z' | '0'..='9')) => Some(c),
                    _ => None,
                };
                self.reset();
                if let Some(reg) = reg {
                    self.play_macro(st, vfs, host, reg, n);
                }
                return;
            }
        }

        match key {
            Key::Char(c @ '0'..='9') if !(c == '0' && self.pending_count() == 0) => {
                let d = c as usize - '0' as usize;
                if self.op.is_some() {
                    self.opcount = self.opcount.saturating_mul(10).saturating_add(d).min(MAX_COUNT);
                } else {
                    self.count = self.count.saturating_mul(10).saturating_add(d).min(MAX_COUNT);
                }
                self.pend(c);
            }
            Key::Char('"') => {
                self.awaiting = Await::Register;
                self.pend('"');
            }

            // Operators.
            Key::Char('d') => self.op_key(st, host, Op::Delete, 'd'),
            Key::Char('c') => {
                if self.op == Some(Op::Comment) {
                    self.doubled_op(st, host);
                } else {
                    self.op_key(st, host, Op::Change, 'c');
                }
            }
            Key::Char('y') => self.op_key(st, host, Op::Yank, 'y'),
            Key::Char('>') => self.op_key(st, host, Op::Indent, '>'),
            Key::Char('<') => self.op_key(st, host, Op::Dedent, '<'),
            Key::Char('u') if self.op == Some(Op::Lower) => self.doubled_op(st, host),
            Key::Char('U') if self.op == Some(Op::Upper) => self.doubled_op(st, host),

            // Motions.
            _ if self.motion_key(st, host, key) => {}

            // Simple changes.
            Key::Char('x') | Key::Delete => self.do_x(st, true),
            Key::Char('X') => self.do_x(st, false),
            Key::Char('s') => {
                let n = self.total_count();
                if let Some(buf) = st.buf_mut() {
                    let cur = buf.cursor;
                    let end = Position::new(cur.line, (cur.col + n).min(buf.text.line_len(cur.line)));
                    buf.text.begin_undo_group(cur);
                    let removed = buf.text.delete_range(cur, end);
                    self.store_register(st, removed, RegKind::Char, false);
                    self.enter_insert(st, host);
                }
            }
            Key::Char('S') => {
                self.op = Some(Op::Change);
                self.doubled_op(st, host);
            }
            Key::Char('C') => self.op_to_eol(st, host, Op::Change),
            Key::Char('D') => self.op_to_eol(st, host, Op::Delete),
            Key::Char('Y') => {
                self.op = Some(Op::Yank);
                self.doubled_op(st, host);
            }
            Key::Char('r') => {
                self.awaiting = Await::Replace;
                self.pend('r');
            }
            Key::Char('R') => {
                if let Some(buf) = st.buf_mut() {
                    buf.text.begin_undo_group(buf.cursor);
                }
                self.set_mode(host, Mode::Replace);
                self.reset();
            }
            Key::Char('~') => self.do_tilde(st),
            Key::Char('J') => self.do_join(st),
            Key::Char('p') => self.do_paste(st, true),
            Key::Char('P') => self.do_paste(st, false),
            Key::Char('u') => self.do_undo(st, host),
            Key::Ctrl('r') => self.do_redo(st, host),
            Key::Char('.') => {
                let n = self.total_count();
                self.reset();
                self.repeat_last(st, vfs, host, n);
            }

            // Insert entries.
            Key::Char('i') => {
                if self.op.is_some() {
                    self.awaiting = Await::Object(false);
                    self.pend('i');
                } else {
                    if let Some(buf) = st.buf_mut() {
                        buf.text.begin_undo_group(buf.cursor);
                    }
                    self.enter_insert(st, host);
                }
            }
            Key::Char('a') => {
                if self.op.is_some() {
                    self.awaiting = Await::Object(true);
                    self.pend('a');
                } else if let Some(buf) = st.buf_mut() {
                    buf.text.begin_undo_group(buf.cursor);
                    if buf.text.line_len(buf.cursor.line) > 0 {
                        buf.cursor.col += 1;
                    }
                    self.enter_insert(st, host);
                }
            }
            Key::Char('I') => {
                if let Some(buf) = st.buf_mut() {
                    buf.text.begin_undo_group(buf.cursor);
                    buf.cursor.col = first_nonblank(buf.text.line(buf.cursor.line));
                    self.enter_insert(st, host);
                }
            }
            Key::Char('A') => {
                if let Some(buf) = st.buf_mut() {
                    buf.text.begin_undo_group(buf.cursor);
                    buf.cursor.col = buf.text.line_len(buf.cursor.line);
                    self.enter_insert(st, host);
                }
            }
            Key::Char('o') => self.open_line(st, host, true),
            Key::Char('O') => self.open_line(st, host, false),

            // Visual.
            Key::Char('v') => {
                if let Some(buf) = st.buf() {
                    self.anchor = buf.cursor;
                    self.set_mode(host, Mode::Visual { kind: VisualKind::Char });
                    self.reset();
                }
            }
            Key::Char('V') => {
                if let Some(buf) = st.buf() {
                    self.anchor = buf.cursor;
                    self.set_mode(host, Mode::Visual { kind: VisualKind::Line });
                    self.reset();
                }
            }
            Key::Ctrl('v') => {
                if let Some(buf) = st.buf() {
                    self.anchor = buf.cursor;
                    self.set_mode(host, Mode::Visual { kind: VisualKind::Block });
                    self.reset();
                }
            }

            // Macros.
            Key::Char('q') if !self.replaying => {
                if let Some((name, mut keys)) = self.recording.take() {
                    // Drop the stopping q itself from the recording.
                    keys.pop();
                    self.macros.insert(name, keys);
                    st.info(format!("recorded @{name}"));
                    self.reset();
                } else if self.op.is_some() {
                    self.reset();
                } else {
                    self.awaiting = Await::MacroReg;
                    self.pend('q');
                }
            }
            Key::Char('@') => {
                self.awaiting = Await::MacroPlay;
                self.pend('@');
            }

            // Marks.
            Key::Char('m') => {
                self.awaiting = Await::Mark;
                self.pend('m');
            }
            Key::Char('`') => {
                self.awaiting = Await::JumpExact;
                self.pend('`');
            }
            Key::Char('\'') => {
                self.awaiting = Await::JumpLine;
                self.pend('\'');
            }

            // Command line and search.
            Key::Char(':') => self.enter_cmdline(host),
            Key::Char('/') => self.enter_search(st, host, false),
            Key::Char('?') => self.enter_search(st, host, true),
            Key::Char('n') => self.search_next(st, host, false),
            Key::Char('N') => self.search_next(st, host, true),
            Key::Char('*') => self.search_word(st, host, false),
            Key::Char('#') => self.search_word(st, host, true),

            // Scrolling.
            Key::Ctrl('d') => self.scroll(ScrollRequest::HalfDown),
            Key::Ctrl('u') => self.scroll(ScrollRequest::HalfUp),
            Key::Ctrl('f') | Key::PageDown => self.scroll(ScrollRequest::PageDown),
            Key::Ctrl('b') | Key::PageUp => self.scroll(ScrollRequest::PageUp),

            // Prefixes and the leader.
            Key::Char('g') => {
                self.awaiting = Await::Prefix('g');
                self.pend('g');
            }
            Key::Char('z') => {
                self.awaiting = Await::Prefix('z');
                self.pend('z');
            }
            Key::Char(' ') => self.awaiting = Await::Leader(LeaderNode::Root),
            Key::Ctrl('w') => self.awaiting = Await::CtrlW,
            Key::Ctrl('p') => self.open_finder(st, vfs, host),

            Key::Esc | Key::Ctrl('c') => {
                self.reset();
                st.status = None;
            }
            _ => self.reset(),
        }
    }

    /// Count digits typed so far in the slot the next digit would extend.
    fn pending_count(&self) -> usize {
        if self.op.is_some() { self.opcount } else { self.count }
    }

    fn op_key(&mut self, st: &mut EditorState, host: &HostHandle, op: Op, c: char) {
        match self.op {
            Some(cur) if cur == op => self.doubled_op(st, host),
            Some(_) => self.reset(),
            None => {
                self.op = Some(op);
                self.pend(c);
            }
        }
    }

    /// Doubled operator (dd yy cc >> << guu gUU gcc): acts linewise on count lines.
    fn doubled_op(&mut self, st: &mut EditorState, host: &HostHandle) {
        let n = self.total_count();
        let Some(op) = self.op else { return };
        let Some(first) = st.buf().map(|b| b.cursor.line) else {
            self.reset();
            return;
        };
        let last_line = st.buf().map(|b| b.text.line_count() - 1).unwrap_or(0);
        let last = (first + n - 1).min(last_line);
        self.apply_linewise(st, host, op, first, last);
        self.reset();
    }

    /// C and D: operate from the cursor to the end of the line.
    fn op_to_eol(&mut self, st: &mut EditorState, host: &HostHandle, op: Op) {
        let Some(buf) = st.buf() else { return };
        let cur = buf.cursor;
        let target = Position::new(cur.line, buf.text.line_len(cur.line));
        self.op = Some(op);
        self.apply_op(st, host, cur, target, MotionKind::Exclusive);
        self.reset();
    }

    fn scroll(&mut self, req: ScrollRequest) {
        self.scroll_req.set(Some(req));
        self.reset();
    }

    fn jump_mark(&mut self, st: &mut EditorState, host: &HostHandle, key: Key) {
        let exact = self.pending.ends_with('`');
        let Key::Char(c) = key else {
            self.reset();
            return;
        };
        let Some(buf) = st.buf() else {
            self.reset();
            return;
        };
        let Some(&pos) = buf.marks.get(&c) else {
            self.err(st, host, "E20: Mark not set");
            self.reset();
            return;
        };
        let target = if exact {
            buf.text.clamp(pos, false)
        } else {
            let line = pos.line.min(buf.text.line_count() - 1);
            Position::new(line, first_nonblank(buf.text.line(line)))
        };
        let kind = if exact { MotionKind::Exclusive } else { MotionKind::Linewise };
        self.finish_motion(st, host, target, kind, true);
    }

    fn g_key(&mut self, st: &mut EditorState, host: &HostHandle, key: Key) {
        match key {
            Key::Char('g') => {
                let Some(buf) = st.buf() else {
                    self.reset();
                    return;
                };
                let line = self
                    .explicit_count()
                    .map(|n| n - 1)
                    .unwrap_or(0)
                    .min(buf.text.line_count() - 1);
                let target = Position::new(line, first_nonblank(buf.text.line(line)));
                self.finish_motion(st, host, target, MotionKind::Linewise, true);
            }
            Key::Char('e') | Key::Char('E') => {
                let big = key == Key::Char('E');
                let Some(buf) = st.buf() else {
                    self.reset();
                    return;
                };
                let mut p = buf.cursor;
                for _ in 0..self.total_count() {
                    p = word_back_end(&buf.text, p, big);
                }
                self.finish_motion(st, host, p, MotionKind::Inclusive, true);
            }
            Key::Char('u') => {
                self.op = Some(Op::Lower);
                self.pend('u');
            }
            Key::Char('U') => {
                self.op = Some(Op::Upper);
                self.pend('U');
            }
            Key::Char('c') => {
                self.op = Some(Op::Comment);
                self.pend('c');
            }
            _ => self.reset(),
        }
    }
}

// ---------------------------------------------------------------------------
// Motions
// ---------------------------------------------------------------------------

impl VimEngine {
    /// Handle a pure motion key; returns false when `key` is not a motion.
    fn motion_key(&mut self, st: &mut EditorState, host: &HostHandle, key: Key) -> bool {
        let Some(buf) = st.buf() else { return false };
        let cur = buf.cursor;
        let text = &buf.text;
        let n = self.total_count();
        let last_line = text.line_count() - 1;
        let (target, kind, set_desired) = match key {
            Key::Char('h') | Key::Left => {
                (Position::new(cur.line, cur.col.saturating_sub(n)), MotionKind::Exclusive, true)
            }
            Key::Char('l') | Key::Right => {
                let max = if self.op.is_some() {
                    text.line_len(cur.line)
                } else {
                    text.line_len(cur.line).saturating_sub(1)
                };
                (Position::new(cur.line, (cur.col + n).min(max)), MotionKind::Exclusive, true)
            }
            Key::Char('j') | Key::Down => {
                let line = (cur.line + n).min(last_line);
                (Position::new(line, buf.desired_col), MotionKind::Linewise, false)
            }
            Key::Char('k') | Key::Up => {
                let line = cur.line.saturating_sub(n);
                (Position::new(line, buf.desired_col), MotionKind::Linewise, false)
            }
            Key::Char('0') | Key::Home => (Position::new(cur.line, 0), MotionKind::Exclusive, true),
            Key::Char('^') => {
                (Position::new(cur.line, first_nonblank(text.line(cur.line))), MotionKind::Exclusive, true)
            }
            Key::Char('$') | Key::End => {
                let line = (cur.line + n - 1).min(last_line);
                (Position::new(line, text.line_len(line).saturating_sub(1)), MotionKind::Inclusive, true)
            }
            Key::Char('G') => {
                let line = self.explicit_count().map(|c| c - 1).unwrap_or(last_line).min(last_line);
                (Position::new(line, first_nonblank(text.line(line))), MotionKind::Linewise, true)
            }
            Key::Char(c @ ('w' | 'W')) => {
                let big = c == 'W';
                let mut p = cur;
                for _ in 0..n {
                    p = word_forward(text, p, big);
                }
                // Operators over w stop at the end of the starting line.
                if self.op.is_some() && p.line > cur.line && text.line_len(cur.line) > 0 {
                    if self.op == Some(Op::Change) && !char_is_blank(text, cur) {
                        let mut e = cur;
                        for _ in 0..n {
                            e = word_end(text, e, big);
                        }
                        (e, MotionKind::Inclusive, true)
                    } else {
                        (Position::new(cur.line, text.line_len(cur.line)), MotionKind::Exclusive, true)
                    }
                } else if self.op == Some(Op::Change) && !char_is_blank(text, cur) {
                    let mut e = cur;
                    for _ in 0..n {
                        e = word_end(text, e, big);
                    }
                    (e, MotionKind::Inclusive, true)
                } else {
                    (p, MotionKind::Exclusive, true)
                }
            }
            Key::Char(c @ ('e' | 'E')) => {
                let big = c == 'E';
                let mut p = cur;
                for _ in 0..n {
                    p = word_end(text, p, big);
                }
                (p, MotionKind::Inclusive, true)
            }
            Key::Char(c @ ('b' | 'B')) => {
                let big = c == 'B';
                let mut p = cur;
                for _ in 0..n {
                    p = word_back(text, p, big);
                }
                (p, MotionKind::Exclusive, true)
            }
            Key::Char('{') => (para_back(text, cur.line, n), MotionKind::Exclusive, true),
            Key::Char('}') => (para_forward(text, cur.line, n), MotionKind::Exclusive, true),
            Key::Char('%') => match match_bracket(text, cur) {
                Some(p) => (p, MotionKind::Inclusive, true),
                None => {
                    self.reset();
                    return true;
                }
            },
            Key::Char(c @ ('f' | 'F' | 't' | 'T')) => {
                self.awaiting = Await::Find(c);
                self.pend(c);
                return true;
            }
            Key::Char(';') => {
                if let Some((kind, target)) = self.last_find {
                    self.do_find(st, host, kind, target);
                } else {
                    self.reset();
                }
                return true;
            }
            Key::Char(',') => {
                if let Some((kind, target)) = self.last_find {
                    let flipped = match kind {
                        'f' => 'F',
                        'F' => 'f',
                        't' => 'T',
                        _ => 't',
                    };
                    self.do_find(st, host, flipped, target);
                } else {
                    self.reset();
                }
                return true;
            }
            _ => return false,
        };
        let had_op = self.op.is_some();
        self.finish_motion(st, host, target, kind, set_desired);
        // $ pins the desired column to end-of-line, so j/k (and block selections) track it.
        if matches!(key, Key::Char('$') | Key::End) && !had_op {
            if let Some(buf) = st.buf_mut() {
                buf.desired_col = usize::MAX;
            }
        }
        true
    }

    /// f/F/t/T: jump to (or before) the count'th occurrence of `target` on the line.
    fn do_find(&mut self, st: &mut EditorState, host: &HostHandle, kind: char, target: char) {
        let Some(buf) = st.buf() else {
            self.reset();
            return;
        };
        let cur = buf.cursor;
        let chars: Vec<char> = buf.text.line(cur.line).chars().collect();
        let n = self.total_count();
        let forward = matches!(kind, 'f' | 't');
        let mut col = cur.col;
        for _ in 0..n {
            let found = if forward {
                ((col + 1)..chars.len()).find(|&i| chars[i] == target)
            } else {
                (0..col).rev().find(|&i| chars[i] == target)
            };
            match found {
                Some(i) => col = i,
                None => {
                    self.reset();
                    return;
                }
            }
        }
        let col = match kind {
            't' => col.saturating_sub(1),
            'T' => col + 1,
            _ => col,
        };
        let kind = if forward { MotionKind::Inclusive } else { MotionKind::Exclusive };
        self.finish_motion(st, host, Position::new(cur.line, col), kind, true);
    }

    /// Apply the pending operator over the motion, or just move the cursor.
    fn finish_motion(
        &mut self,
        st: &mut EditorState,
        host: &HostHandle,
        target: Position,
        kind: MotionKind,
        set_desired: bool,
    ) {
        if self.op.is_some() {
            let Some(cur) = st.buf().map(|b| b.cursor) else {
                self.reset();
                return;
            };
            self.apply_op(st, host, cur, target, kind);
        } else if let Some(buf) = st.buf_mut() {
            buf.cursor = buf.text.clamp(target, false);
            if set_desired {
                buf.desired_col = buf.cursor.col;
            }
        }
        if !matches!(self.mode, Mode::Visual { .. }) {
            self.reset();
        } else {
            self.count = 0;
            self.opcount = 0;
            self.pending.clear();
        }
    }
}

// ---------------------------------------------------------------------------
// Operators
// ---------------------------------------------------------------------------

impl VimEngine {
    /// Apply operator `self.op` over the charwise/linewise motion `cur -> target`.
    fn apply_op(
        &mut self,
        st: &mut EditorState,
        host: &HostHandle,
        cur: Position,
        target: Position,
        kind: MotionKind,
    ) {
        let Some(op) = self.op else { return };
        let (a, b) = if cur <= target { (cur, target) } else { (target, cur) };
        if kind == MotionKind::Linewise
            || matches!(op, Op::Indent | Op::Dedent | Op::Comment)
        {
            self.apply_linewise(st, host, op, a.line, b.line);
            return;
        }
        let Some(buf) = st.buf_mut() else { return };
        let end = if kind == MotionKind::Inclusive { advance(&buf.text, b) } else { b };
        match op {
            Op::Delete => {
                let removed = buf.text.delete_range(a, end);
                buf.cursor = buf.text.clamp(a, false);
                buf.desired_col = buf.cursor.col;
                self.store_register(st, removed, RegKind::Char, false);
            }
            Op::Change => {
                buf.text.begin_undo_group(cur);
                let removed = buf.text.delete_range(a, end);
                buf.cursor = buf.text.clamp(a, true);
                self.store_register(st, removed, RegKind::Char, false);
                self.enter_insert(st, host);
            }
            Op::Yank => {
                let text = extract_range(&buf.text, a, end);
                buf.cursor = buf.text.clamp(a, false);
                buf.desired_col = buf.cursor.col;
                self.store_register(st, text, RegKind::Char, true);
            }
            Op::Lower | Op::Upper => {
                map_case_range(&mut buf.text, a, end, op == Op::Upper);
                buf.cursor = buf.text.clamp(a, false);
                buf.desired_col = buf.cursor.col;
            }
            Op::Indent | Op::Dedent | Op::Comment => unreachable!(),
        }
    }

    /// Apply an operator to whole lines `[first, last]`.
    fn apply_linewise(
        &mut self,
        st: &mut EditorState,
        host: &HostHandle,
        op: Op,
        first: usize,
        last: usize,
    ) {
        let Some(buf) = st.buf_mut() else { return };
        let last_line = buf.text.line_count() - 1;
        let (first, last) = (first.min(last_line), last.min(last_line).max(first));
        match op {
            Op::Delete => {
                let removed = buf.text.delete_lines(first, last);
                let line = first.min(buf.text.line_count() - 1);
                buf.cursor = Position::new(line, first_nonblank(buf.text.line(line)));
                buf.desired_col = buf.cursor.col;
                self.store_register(st, removed, RegKind::Line, false);
            }
            Op::Yank => {
                let text = extract_lines(&buf.text, first, last);
                buf.cursor = buf.text.clamp(Position::new(first, buf.cursor.col), false);
                self.store_register(st, text, RegKind::Line, true);
            }
            Op::Change => {
                let indent: String =
                    buf.text.line(first).chars().take_while(|c| c.is_whitespace()).collect();
                buf.text.begin_undo_group(buf.cursor);
                let removed = buf.text.delete_lines(first, last);
                let at = first.min(buf.text.line_count());
                let ilen = indent.chars().count();
                if buf.text.line_count() == 1 && buf.text.line_len(0) == 0 && first == 0 {
                    buf.text.set_line(0, indent);
                } else {
                    buf.text.insert_lines(at, vec![indent]);
                }
                buf.cursor = Position::new(at.min(buf.text.line_count() - 1), ilen);
                self.store_register(st, removed, RegKind::Line, false);
                self.enter_insert(st, host);
            }
            Op::Indent | Op::Dedent => {
                buf.text.begin_undo_group(buf.cursor);
                for i in first..=last {
                    let line = buf.text.line(i).to_string();
                    if op == Op::Indent {
                        if !line.is_empty() {
                            buf.text.set_line(i, format!("    {line}"));
                        }
                    } else {
                        let strip = line.chars().take(4).take_while(|&c| c == ' ').count();
                        if strip > 0 {
                            buf.text.set_line(i, line.chars().skip(strip).collect());
                        }
                    }
                }
                buf.text.end_undo_group();
                buf.cursor =
                    buf.text.clamp(Position::new(first, first_nonblank(buf.text.line(first))), false);
                buf.desired_col = buf.cursor.col;
            }
            Op::Comment => {
                buf.text.begin_undo_group(buf.cursor);
                toggle_comment(&mut buf.text, first, last);
                buf.text.end_undo_group();
                buf.cursor =
                    buf.text.clamp(Position::new(first, first_nonblank(buf.text.line(first))), false);
                buf.desired_col = buf.cursor.col;
            }
            Op::Lower | Op::Upper => {
                buf.text.begin_undo_group(buf.cursor);
                for i in first..=last {
                    let line = buf.text.line(i).to_string();
                    let mapped: String = if op == Op::Upper {
                        line.to_uppercase()
                    } else {
                        line.to_lowercase()
                    };
                    if mapped != line {
                        buf.text.set_line(i, mapped);
                    }
                }
                buf.text.end_undo_group();
                buf.cursor =
                    buf.text.clamp(Position::new(first, first_nonblank(buf.text.line(first))), false);
                buf.desired_col = buf.cursor.col;
            }
        }
    }

    /// Write `text` to the unnamed register, the named one if prefixed, and 0 for yanks.
    fn store_register(&self, st: &mut EditorState, text: String, kind: RegKind, is_yank: bool) {
        if text.is_empty() && kind == RegKind::Char {
            return;
        }
        let r = Register { text, kind };
        if let Some(name) = self.reg {
            st.registers.insert(name, r.clone());
        }
        if is_yank && self.reg.is_none() {
            st.registers.insert('0', r.clone());
        }
        st.registers.insert('"', r);
    }
}

// ---------------------------------------------------------------------------
// Simple normal-mode changes
// ---------------------------------------------------------------------------

impl VimEngine {
    /// x / X: delete count chars after (or before) the cursor on the line.
    fn do_x(&mut self, st: &mut EditorState, forward: bool) {
        let n = self.total_count();
        if let Some(buf) = st.buf_mut() {
            let cur = buf.cursor;
            let len = buf.text.line_len(cur.line);
            let (a, b) = if forward {
                (cur, Position::new(cur.line, (cur.col + n).min(len)))
            } else {
                (Position::new(cur.line, cur.col.saturating_sub(n)), cur)
            };
            if a != b {
                let removed = buf.text.delete_range(a, b);
                buf.cursor = buf.text.clamp(a, false);
                buf.desired_col = buf.cursor.col;
                self.store_register(st, removed, RegKind::Char, false);
            }
        }
        self.reset();
    }

    /// ~: toggle case under the cursor and advance, count times.
    fn do_tilde(&mut self, st: &mut EditorState) {
        let n = self.total_count();
        if let Some(buf) = st.buf_mut() {
            buf.text.begin_undo_group(buf.cursor);
            for _ in 0..n {
                let Some(c) = buf.text.char_at(buf.cursor) else { break };
                let flipped = if c.is_lowercase() {
                    c.to_uppercase().next().unwrap_or(c)
                } else {
                    c.to_lowercase().next().unwrap_or(c)
                };
                buf.text.replace_char(buf.cursor, flipped);
                if buf.cursor.col + 1 < buf.text.line_len(buf.cursor.line) {
                    buf.cursor.col += 1;
                } else {
                    break;
                }
            }
            buf.text.end_undo_group();
            buf.desired_col = buf.cursor.col;
        }
        self.reset();
    }

    /// J: join count.max(2)-1 lines with vim space semantics.
    fn do_join(&mut self, st: &mut EditorState) {
        let joins = self.total_count().max(2) - 1;
        if let Some(buf) = st.buf_mut() {
            buf.text.begin_undo_group(buf.cursor);
            let mut joint = None;
            for _ in 0..joins {
                match buf.text.join_line(buf.cursor.line, true) {
                    Some(j) => joint = joint.or(Some(j)),
                    None => break,
                }
            }
            buf.text.end_undo_group();
            if let Some(j) = joint {
                buf.cursor = buf.text.clamp(Position::new(buf.cursor.line, j), false);
                buf.desired_col = buf.cursor.col;
            }
        }
        self.reset();
    }

    /// r{char}: replace count chars under the cursor.
    fn do_replace(&mut self, st: &mut EditorState, c: char) {
        let n = self.total_count();
        if let Some(buf) = st.buf_mut() {
            let cur = buf.cursor;
            if cur.col + n <= buf.text.line_len(cur.line) {
                buf.text.begin_undo_group(cur);
                for i in 0..n {
                    buf.text.replace_char(Position::new(cur.line, cur.col + i), c);
                }
                buf.text.end_undo_group();
                buf.cursor = buf.text.clamp(Position::new(cur.line, cur.col + n - 1), false);
                buf.desired_col = buf.cursor.col;
            }
        }
    }

    /// p / P: put the register after or before the cursor, count times.
    fn do_paste(&mut self, st: &mut EditorState, after: bool) {
        let name = self.reg.unwrap_or('"');
        let Some(r) = st.registers.get(&name).cloned() else {
            self.reset();
            return;
        };
        if r.text.is_empty() {
            self.reset();
            return;
        }
        // One copy always goes in; further repeats fit within the paste budget.
        let n = self
            .total_count()
            .min((PASTE_MAX_BYTES / (r.text.len() + 1)).max(1));
        if let Some(buf) = st.buf_mut() {
            buf.text.begin_undo_group(buf.cursor);
            match r.kind {
                RegKind::Line => {
                    let at = if after { buf.cursor.line + 1 } else { buf.cursor.line };
                    let mut lines: Vec<String> = Vec::new();
                    for _ in 0..n {
                        lines.extend(r.text.split('\n').map(str::to_string));
                    }
                    buf.text.insert_lines(at, lines);
                    let line = at.min(buf.text.line_count() - 1);
                    buf.cursor = Position::new(line, first_nonblank(buf.text.line(line)));
                }
                RegKind::Char => {
                    let mut at = buf.cursor;
                    if after && buf.text.line_len(at.line) > 0 {
                        at.col += 1;
                    }
                    let text = r.text.repeat(n);
                    let end = buf.text.insert_text(at, &text);
                    buf.cursor = buf.text.clamp(step_back(&buf.text, end), false);
                }
                // Block: each fragment lands on its own line at the paste column,
                // padding short lines with spaces and appending lines past the end.
                RegKind::Block => {
                    let base = buf.cursor.line;
                    let col = if after && buf.text.line_len(base) > 0 {
                        buf.cursor.col + 1
                    } else {
                        buf.cursor.col
                    };
                    for (k, frag) in r.text.split('\n').enumerate() {
                        if frag.is_empty() {
                            continue;
                        }
                        let l = base + k;
                        while l >= buf.text.line_count() {
                            let at = buf.text.line_count();
                            buf.text.insert_lines(at, vec![String::new()]);
                        }
                        let len = buf.text.line_len(l);
                        if len < col {
                            let text = buf.text.line(l).to_string();
                            buf.text.set_line(l, text + &" ".repeat(col - len));
                        }
                        buf.text.insert_text(Position::new(l, col), &frag.repeat(n));
                    }
                    buf.cursor = buf.text.clamp(Position::new(base, col), false);
                }
            }
            buf.text.end_undo_group();
            buf.desired_col = buf.cursor.col;
        }
        self.reset();
    }

    fn do_undo(&mut self, st: &mut EditorState, host: &HostHandle) {
        let n = self.total_count();
        if let Some(buf) = st.buf_mut() {
            let mut done = 0;
            for _ in 0..n {
                let Some(pos) = buf.text.undo(buf.cursor) else { break };
                buf.cursor = buf.text.clamp(pos, false);
                done += 1;
            }
            if done == 0 {
                self.err(st, host, "Already at oldest change");
            } else {
                st.info("undo");
            }
        }
        self.reset();
    }

    fn do_redo(&mut self, st: &mut EditorState, host: &HostHandle) {
        let n = self.total_count();
        if let Some(buf) = st.buf_mut() {
            let mut done = 0;
            for _ in 0..n {
                let Some(pos) = buf.text.redo(buf.cursor) else { break };
                buf.cursor = buf.text.clamp(pos, false);
                done += 1;
            }
            if done == 0 {
                self.err(st, host, "Already at newest change");
            } else {
                st.info("redo");
            }
        }
        self.reset();
    }

    /// o / O: open a line below or above with the current line's indent.
    fn open_line(&mut self, st: &mut EditorState, host: &HostHandle, below: bool) {
        if let Some(buf) = st.buf_mut() {
            let indent: String =
                buf.text.line(buf.cursor.line).chars().take_while(|c| c.is_whitespace()).collect();
            let ilen = indent.chars().count();
            buf.text.begin_undo_group(buf.cursor);
            let at = if below { buf.cursor.line + 1 } else { buf.cursor.line };
            buf.text.insert_lines(at, vec![indent]);
            buf.cursor = Position::new(at, ilen);
            self.enter_insert(st, host);
        }
    }

    /// Switch to insert mode; the caller must already have opened an undo group.
    fn enter_insert(&mut self, st: &mut EditorState, host: &HostHandle) {
        if let Some(buf) = st.buf_mut() {
            buf.desired_col = buf.cursor.col;
        }
        self.insert_j = false;
        self.set_mode(host, Mode::Insert);
        self.reset();
    }

    /// Leave insert/replace mode: close the undo group and step the cursor back.
    fn leave_insert(&mut self, st: &mut EditorState, host: &HostHandle) {
        self.finish_block_insert(st);
        if let Some(buf) = st.buf_mut() {
            buf.text.end_undo_group();
            buf.cursor.col = buf.cursor.col.saturating_sub(1);
            buf.cursor = buf.text.clamp(buf.cursor, false);
            buf.desired_col = buf.cursor.col;
        }
        self.insert_j = false;
        self.set_mode(host, Mode::Normal);
        self.reset();
    }

    /// Replicate a completed blockwise insert onto the block's other lines. Runs inside
    /// the still-open undo group, so the whole block edit undoes as one step. Skipped if
    /// focus moved to another buffer or the typing spilled onto another line.
    fn finish_block_insert(&mut self, st: &mut EditorState) {
        let Some(bi) = self.block_insert.take() else { return };
        if st.active() != bi.buf {
            return;
        }
        let Some(buf) = st.buf_mut() else { return };
        if buf.cursor.line != bi.start.line || buf.cursor.col < bi.start.col {
            return;
        }
        let line: Vec<char> = buf.text.line(bi.start.line).chars().collect();
        let s = bi.start.col.min(line.len());
        let e = buf.cursor.col.min(line.len());
        if e <= s {
            return;
        }
        let typed: String = line[s..e].iter().collect();
        let last = buf.text.line_count() - 1;
        for l in bi.top..=bi.bot.min(last) {
            if l == bi.start.line {
                continue;
            }
            let len = buf.text.line_len(l);
            let col = if bi.to_eol {
                len
            } else if len < bi.col {
                if !bi.pad {
                    continue;
                }
                let text = buf.text.line(l).to_string();
                buf.text.set_line(l, text + &" ".repeat(bi.col - len));
                bi.col
            } else {
                bi.col
            };
            buf.text.insert_text(Position::new(l, col), &typed);
        }
    }
}

// ---------------------------------------------------------------------------
// Text objects
// ---------------------------------------------------------------------------

impl VimEngine {
    /// Resolve a text object and apply the pending operator or extend the selection.
    fn do_object(&mut self, st: &mut EditorState, host: &HostHandle, obj: char, around: bool) {
        let Some(buf) = st.buf() else {
            self.reset();
            return;
        };
        let Some((a, b)) = object_range(&buf.text, buf.cursor, obj, around) else {
            self.reset();
            return;
        };
        if let Mode::Visual { .. } = self.mode {
            self.anchor = a;
            if let Some(buf) = st.buf_mut() {
                buf.cursor = buf.text.clamp(step_back(&buf.text, b), false);
            }
            self.count = 0;
            self.opcount = 0;
            self.pending.clear();
            return;
        }
        let Some(op) = self.op else {
            self.reset();
            return;
        };
        if matches!(op, Op::Indent | Op::Dedent | Op::Comment) {
            let last = step_back(&buf.text, b).line;
            self.apply_linewise(st, host, op, a.line, last);
        } else {
            let Some(buf) = st.buf_mut() else { return };
            match op {
                Op::Delete => {
                    let removed = buf.text.delete_range(a, b);
                    buf.cursor = buf.text.clamp(a, false);
                    buf.desired_col = buf.cursor.col;
                    self.store_register(st, removed, RegKind::Char, false);
                }
                Op::Change => {
                    buf.text.begin_undo_group(buf.cursor);
                    let removed = buf.text.delete_range(a, b);
                    buf.cursor = buf.text.clamp(a, true);
                    self.store_register(st, removed, RegKind::Char, false);
                    self.enter_insert(st, host);
                    return;
                }
                Op::Yank => {
                    let text = extract_range(&buf.text, a, b);
                    buf.cursor = buf.text.clamp(a, false);
                    self.store_register(st, text, RegKind::Char, true);
                }
                Op::Lower | Op::Upper => {
                    map_case_range(&mut buf.text, a, b, op == Op::Upper);
                    buf.cursor = buf.text.clamp(a, false);
                }
                _ => {}
            }
        }
        self.reset();
    }
}

/// Char range `[start, end)` covered by text object `obj` at `cur`.
fn object_range(text: &TextBuffer, cur: Position, obj: char, around: bool) -> Option<(Position, Position)> {
    match obj {
        'w' | 'W' => word_object(text, cur, obj == 'W', around),
        '"' | '\'' | '`' => quote_object(text, cur, obj, around),
        '(' | ')' | 'b' => pair_object(text, cur, '(', ')', around),
        '{' | '}' | 'B' => pair_object(text, cur, '{', '}', around),
        '[' | ']' => pair_object(text, cur, '[', ']', around),
        '<' | '>' => pair_object(text, cur, '<', '>', around),
        _ => None,
    }
}

fn word_object(text: &TextBuffer, cur: Position, big: bool, around: bool) -> Option<(Position, Position)> {
    let chars: Vec<char> = text.line(cur.line).chars().collect();
    if chars.is_empty() {
        return None;
    }
    let col = cur.col.min(chars.len() - 1);
    let cls = char_class(chars[col], big);
    let mut s = col;
    while s > 0 && char_class(chars[s - 1], big) == cls {
        s -= 1;
    }
    let mut e = col;
    while e + 1 < chars.len() && char_class(chars[e + 1], big) == cls {
        e += 1;
    }
    if around {
        if cls == 0 {
            // On whitespace: include the following word.
            if e + 1 < chars.len() {
                let ncls = char_class(chars[e + 1], big);
                while e + 1 < chars.len() && char_class(chars[e + 1], big) == ncls {
                    e += 1;
                }
            }
        } else {
            let mut e2 = e;
            while e2 + 1 < chars.len() && char_class(chars[e2 + 1], big) == 0 {
                e2 += 1;
            }
            if e2 > e {
                e = e2;
            } else {
                while s > 0 && char_class(chars[s - 1], big) == 0 {
                    s -= 1;
                }
            }
        }
    }
    Some((Position::new(cur.line, s), Position::new(cur.line, e + 1)))
}

fn quote_object(text: &TextBuffer, cur: Position, q: char, around: bool) -> Option<(Position, Position)> {
    let chars: Vec<char> = text.line(cur.line).chars().collect();
    let idxs: Vec<usize> = chars.iter().enumerate().filter(|&(_, &c)| c == q).map(|(i, _)| i).collect();
    let pair = idxs.chunks(2).find(|p| p.len() == 2 && cur.col <= p[1])?;
    let (a, b) = (pair[0], pair[1]);
    if !around {
        return Some((Position::new(cur.line, a + 1), Position::new(cur.line, b)));
    }
    let mut s = a;
    let mut e = b + 1;
    let mut e2 = e;
    while e2 < chars.len() && chars[e2] == ' ' {
        e2 += 1;
    }
    if e2 > e {
        e = e2;
    } else {
        while s > 0 && chars[s - 1] == ' ' {
            s -= 1;
        }
    }
    Some((Position::new(cur.line, s), Position::new(cur.line, e)))
}

fn pair_object(
    text: &TextBuffer,
    cur: Position,
    open: char,
    close: char,
    around: bool,
) -> Option<(Position, Position)> {
    let (o, c) = enclosing_pair(text, cur, open, close)?;
    if around {
        Some((o, advance(text, c)))
    } else {
        let start = advance(text, o);
        if start >= c { None } else { Some((start, c)) }
    }
}

/// Innermost `open`..`close` pair enclosing (or under) the cursor.
fn enclosing_pair(text: &TextBuffer, cur: Position, open: char, close: char) -> Option<(Position, Position)> {
    let here = text.char_at(cur);
    if here == Some(open) {
        return Some((cur, match_forward(text, cur, open, close)?));
    }
    if here == Some(close) {
        return Some((match_backward(text, cur, open, close)?, cur));
    }
    let o = match_backward(text, cur, open, close)?;
    let c = match_forward(text, o, open, close)?;
    if c < cur { None } else { Some((o, c)) }
}

/// Scan budget for bracket matching, in chars.
const MATCH_BUDGET: usize = 200_000;

/// Closing partner of the `open` at `from`, scanning forward.
fn match_forward(text: &TextBuffer, from: Position, open: char, close: char) -> Option<Position> {
    let mut depth = 1usize;
    let mut budget = MATCH_BUDGET;
    let mut line = from.line;
    let mut skip = from.col + 1;
    while line < text.line_count() {
        for (i, ch) in text.line(line).chars().enumerate().skip(skip) {
            budget = budget.checked_sub(1)?;
            if ch == open {
                depth += 1;
            } else if ch == close {
                depth -= 1;
                if depth == 0 {
                    return Some(Position::new(line, i));
                }
            }
        }
        line += 1;
        skip = 0;
    }
    None
}

/// Unmatched `open` before `from`, scanning backward.
fn match_backward(text: &TextBuffer, from: Position, open: char, close: char) -> Option<Position> {
    let mut depth = 0usize;
    let mut budget = MATCH_BUDGET;
    let mut line = from.line;
    loop {
        let upto = if line == from.line { from.col } else { text.line_len(line) };
        let chars: Vec<char> = text.line(line).chars().take(upto).collect();
        for i in (0..chars.len()).rev() {
            budget = budget.checked_sub(1)?;
            if chars[i] == close {
                depth += 1;
            } else if chars[i] == open {
                if depth == 0 {
                    return Some(Position::new(line, i));
                }
                depth -= 1;
            }
        }
        if line == 0 {
            return None;
        }
        line -= 1;
    }
}

/// `%`: partner of the first bracket at or after the cursor on its line.
fn match_bracket(text: &TextBuffer, cur: Position) -> Option<Position> {
    let chars: Vec<char> = text.line(cur.line).chars().collect();
    let (col, ch) = chars
        .iter()
        .enumerate()
        .skip(cur.col)
        .find(|&(_, &c)| matches!(c, '(' | ')' | '[' | ']' | '{' | '}'))
        .map(|(i, &c)| (i, c))?;
    let from = Position::new(cur.line, col);
    match ch {
        '(' => match_forward(text, from, '(', ')'),
        '[' => match_forward(text, from, '[', ']'),
        '{' => match_forward(text, from, '{', '}'),
        ')' => match_backward(text, from, '(', ')'),
        ']' => match_backward(text, from, '[', ']'),
        _ => match_backward(text, from, '{', '}'),
    }
}

// ---------------------------------------------------------------------------
// Buffer helpers (pure)
// ---------------------------------------------------------------------------

/// Word class: 0 whitespace, 1 word chars (or any non-blank when `big`), 2 punctuation.
fn char_class(c: char, big: bool) -> u8 {
    if c.is_whitespace() {
        0
    } else if big || c == '_' || c.is_alphanumeric() {
        1
    } else {
        2
    }
}

fn char_is_blank(text: &TextBuffer, p: Position) -> bool {
    text.char_at(p).map(|c| c.is_whitespace()).unwrap_or(true)
}

/// Char col of the first non-blank char of `line` (0 for blank lines).
fn first_nonblank(line: &str) -> usize {
    line.chars().position(|c| !c.is_whitespace()).unwrap_or(0)
}

/// The position one char after `p`, stepping across line breaks.
fn advance(text: &TextBuffer, p: Position) -> Position {
    if p.col < text.line_len(p.line) {
        Position::new(p.line, p.col + 1)
    } else if p.line + 1 < text.line_count() {
        Position::new(p.line + 1, 0)
    } else {
        Position::new(p.line, text.line_len(p.line))
    }
}

/// The position one char before `p`, stepping across line breaks.
fn step_back(text: &TextBuffer, p: Position) -> Position {
    if p.col > 0 {
        Position::new(p.line, p.col - 1)
    } else if p.line > 0 {
        Position::new(p.line - 1, text.line_len(p.line - 1))
    } else {
        p
    }
}

/// w / W: start of the next word; empty lines count as words.
fn word_forward(text: &TextBuffer, p: Position, big: bool) -> Position {
    let mut line = p.line;
    let mut col = p.col;
    let len = text.line_len(line);
    if col < len {
        let cls = char_class(text.char_at(Position::new(line, col)).unwrap_or(' '), big);
        if cls != 0 {
            while col < text.line_len(line)
                && char_class(text.char_at(Position::new(line, col)).unwrap_or(' '), big) == cls
            {
                col += 1;
            }
        }
    }
    loop {
        if col >= text.line_len(line) {
            if line + 1 >= text.line_count() {
                return Position::new(line, text.line_len(line));
            }
            line += 1;
            col = 0;
            if text.line_len(line) == 0 && line != p.line {
                return Position::new(line, 0);
            }
            continue;
        }
        let c = text.char_at(Position::new(line, col)).unwrap_or(' ');
        if char_class(c, big) == 0 {
            col += 1;
        } else {
            return Position::new(line, col);
        }
    }
}

/// e / E: end of the current or next word.
fn word_end(text: &TextBuffer, p: Position, big: bool) -> Position {
    let mut pos = advance(text, p);
    // Skip whitespace (and empty lines).
    loop {
        if pos.line >= text.line_count() {
            let l = text.line_count() - 1;
            return Position::new(l, text.line_len(l).saturating_sub(1));
        }
        if pos.col >= text.line_len(pos.line) {
            if pos.line + 1 >= text.line_count() {
                return Position::new(pos.line, text.line_len(pos.line).saturating_sub(1));
            }
            pos = Position::new(pos.line + 1, 0);
            continue;
        }
        let c = text.char_at(pos).unwrap_or(' ');
        if char_class(c, big) == 0 {
            pos = advance(text, pos);
        } else {
            break;
        }
    }
    let cls = char_class(text.char_at(pos).unwrap_or(' '), big);
    while pos.col + 1 < text.line_len(pos.line)
        && char_class(text.char_at(Position::new(pos.line, pos.col + 1)).unwrap_or(' '), big) == cls
    {
        pos.col += 1;
    }
    pos
}

/// b / B: start of the current or previous word.
fn word_back(text: &TextBuffer, p: Position, big: bool) -> Position {
    let mut pos = p;
    if pos == Position::new(0, 0) {
        return pos;
    }
    pos = step_back(text, pos);
    // Skip whitespace backwards (col == line_len means the virtual newline).
    loop {
        if pos == Position::new(0, 0) {
            break;
        }
        if pos.col >= text.line_len(pos.line) {
            if text.line_len(pos.line) == 0 {
                return pos;
            }
            pos = step_back(text, pos);
            continue;
        }
        let c = text.char_at(pos).unwrap_or(' ');
        if char_class(c, big) == 0 {
            pos = step_back(text, pos);
        } else {
            break;
        }
    }
    let Some(c) = text.char_at(pos) else { return pos };
    let cls = char_class(c, big);
    if cls == 0 {
        return pos;
    }
    while pos.col > 0 {
        let prev = text.char_at(Position::new(pos.line, pos.col - 1)).unwrap_or(' ');
        if char_class(prev, big) == cls {
            pos.col -= 1;
        } else {
            break;
        }
    }
    pos
}

/// ge / gE: end of the previous word.
fn word_back_end(text: &TextBuffer, p: Position, big: bool) -> Position {
    let mut pos = p;
    if pos == Position::new(0, 0) {
        return pos;
    }
    // Step back to the start of the current word run first.
    if let Some(c) = text.char_at(pos) {
        let cls = char_class(c, big);
        if cls != 0 {
            while pos.col > 0 {
                let prev = text.char_at(Position::new(pos.line, pos.col - 1));
                if prev.map(|c| char_class(c, big)) == Some(cls) {
                    pos.col -= 1;
                } else {
                    break;
                }
            }
        }
    }
    if pos == Position::new(0, 0) {
        return pos;
    }
    pos = step_back(text, pos);
    loop {
        if pos == Position::new(0, 0) {
            return pos;
        }
        if pos.col >= text.line_len(pos.line) {
            pos = step_back(text, pos);
            continue;
        }
        let c = text.char_at(pos).unwrap_or(' ');
        if char_class(c, big) == 0 {
            pos = step_back(text, pos);
        } else {
            return pos;
        }
    }
}

/// }: line of the next empty line (or the end of the buffer).
fn para_forward(text: &TextBuffer, from_line: usize, n: usize) -> Position {
    let lc = text.line_count();
    let mut l = from_line;
    for _ in 0..n {
        l += 1;
        while l < lc && text.line_len(l) != 0 {
            l += 1;
        }
        if l >= lc {
            return Position::new(lc - 1, text.line_len(lc - 1));
        }
    }
    Position::new(l, 0)
}

/// {: line of the previous empty line (or the start of the buffer).
fn para_back(text: &TextBuffer, from_line: usize, n: usize) -> Position {
    let mut l = from_line;
    for _ in 0..n {
        if l == 0 {
            return Position::new(0, 0);
        }
        l -= 1;
        while l > 0 && text.line_len(l) != 0 {
            l -= 1;
        }
        if l == 0 && text.line_len(0) != 0 {
            return Position::new(0, 0);
        }
    }
    Position::new(l, 0)
}

/// Text of the charwise range `[start, end)` without deleting it.
fn extract_range(text: &TextBuffer, start: Position, end: Position) -> String {
    let (start, end) = if start <= end { (start, end) } else { (end, start) };
    if start.line == end.line {
        return text
            .line(start.line)
            .chars()
            .skip(start.col)
            .take(end.col.saturating_sub(start.col))
            .collect();
    }
    let mut out: String = text.line(start.line).chars().skip(start.col).collect();
    for l in start.line + 1..end.line.min(text.line_count()) {
        out.push('\n');
        out.push_str(text.line(l));
    }
    out.push('\n');
    out.extend(text.line(end.line).chars().take(end.col));
    out
}

/// Lines `[first, last]` joined with newlines.
fn extract_lines(text: &TextBuffer, first: usize, last: usize) -> String {
    let last = last.min(text.line_count() - 1);
    text.lines()[first.min(last)..=last].join("\n")
}

/// Upper/lowercase every char in the charwise range `[start, end)`.
fn map_case_range(text: &mut TextBuffer, start: Position, end: Position, upper: bool) {
    text.begin_undo_group(start);
    let last = end.line.min(text.line_count() - 1);
    for l in start.line..=last {
        let line = text.line(l).to_string();
        let from = if l == start.line { start.col } else { 0 };
        let to = if l == end.line { end.col } else { line.chars().count() };
        let mut out = String::with_capacity(line.len());
        for (i, c) in line.chars().enumerate() {
            if i >= from && i < to {
                if upper {
                    out.extend(c.to_uppercase());
                } else {
                    out.extend(c.to_lowercase());
                }
            } else {
                out.push(c);
            }
        }
        if out != line {
            text.set_line(l, out);
        }
    }
    text.end_undo_group();
}

/// Toggle `// ` line comments over `[first, last]` (blank lines are left alone).
fn toggle_comment(text: &mut TextBuffer, first: usize, last: usize) {
    let last = last.min(text.line_count() - 1);
    let all_commented = (first..=last)
        .map(|i| text.line(i))
        .filter(|l| !l.trim().is_empty())
        .all(|l| l.trim_start().starts_with("//"));
    let any_content = (first..=last).any(|i| !text.line(i).trim().is_empty());
    if !any_content {
        return;
    }
    for i in first..=last {
        let line = text.line(i).to_string();
        if line.trim().is_empty() {
            continue;
        }
        let indent = first_nonblank(&line);
        let (head, tail): (String, String) =
            (line.chars().take(indent).collect(), line.chars().skip(indent).collect());
        if all_commented {
            let stripped = tail.strip_prefix("// ").or_else(|| tail.strip_prefix("//")).unwrap_or(&tail);
            text.set_line(i, format!("{head}{stripped}"));
        } else {
            text.set_line(i, format!("{head}// {tail}"));
        }
    }
}

// ---------------------------------------------------------------------------
// Visual mode
// ---------------------------------------------------------------------------

impl VimEngine {
    fn visual_key(&mut self, st: &mut EditorState, host: &HostHandle, key: Key, kind: VisualKind) {
        match std::mem::replace(&mut self.awaiting, Await::None) {
            Await::None => {}
            Await::Find(kind) => {
                if let Key::Char(c) = key {
                    self.last_find = Some((kind, c));
                    self.do_find(st, host, kind, c);
                }
                return;
            }
            Await::Object(around) => {
                if let Key::Char(c) = key {
                    self.do_object(st, host, c, around);
                }
                return;
            }
            Await::Register => {
                if let Key::Char(c @ ('a'..='z' | 'A'..='Z' | '0'..='9')) = key {
                    self.reg = Some(c.to_ascii_lowercase());
                }
                return;
            }
            Await::Replace => {
                if let Key::Char(c) = key {
                    self.visual_replace(st, host, c);
                }
                return;
            }
            Await::Prefix('g') => {
                match key {
                    Key::Char('g') => {
                        if let Some(buf) = st.buf_mut() {
                            buf.cursor = buf.text.clamp(Position::new(0, 0), false);
                            buf.desired_col = 0;
                        }
                    }
                    Key::Char('u') => self.visual_op(st, host, Op::Lower, kind),
                    Key::Char('U') => self.visual_op(st, host, Op::Upper, kind),
                    Key::Char('c') => self.visual_op(st, host, Op::Comment, kind),
                    Key::Char('e') => {
                        if let Some(buf) = st.buf_mut() {
                            let p = word_back_end(&buf.text, buf.cursor, false);
                            buf.cursor = buf.text.clamp(p, false);
                        }
                    }
                    _ => {}
                }
                self.pending.clear();
                return;
            }
            _ => {
                self.reset();
                return;
            }
        }

        match key {
            Key::Char(c @ '0'..='9') if !(c == '0' && self.count == 0) => {
                self.count =
                    self.count.saturating_mul(10).saturating_add(c as usize - '0' as usize).min(MAX_COUNT);
                self.pend(c);
            }
            Key::Esc | Key::Ctrl('c') => {
                self.set_mode(host, Mode::Normal);
                self.reset();
            }
            Key::Char('v') => {
                if kind == VisualKind::Char {
                    self.set_mode(host, Mode::Normal);
                } else {
                    self.set_mode(host, Mode::Visual { kind: VisualKind::Char });
                }
                self.reset();
            }
            Key::Char('V') => {
                if kind == VisualKind::Line {
                    self.set_mode(host, Mode::Normal);
                } else {
                    self.set_mode(host, Mode::Visual { kind: VisualKind::Line });
                }
                self.reset();
            }
            Key::Ctrl('v') => {
                if kind == VisualKind::Block {
                    self.set_mode(host, Mode::Normal);
                } else {
                    self.set_mode(host, Mode::Visual { kind: VisualKind::Block });
                }
                self.reset();
            }
            Key::Char('o') => {
                if let Some(buf) = st.buf_mut() {
                    std::mem::swap(&mut self.anchor, &mut buf.cursor);
                    buf.desired_col = buf.cursor.col;
                }
            }
            Key::Char('O') => {
                // Block: swap the horizontal corners; otherwise same as o.
                if let Some(buf) = st.buf_mut() {
                    if kind == VisualKind::Block {
                        std::mem::swap(&mut self.anchor.col, &mut buf.cursor.col);
                    } else {
                        std::mem::swap(&mut self.anchor, &mut buf.cursor);
                    }
                    buf.desired_col = buf.cursor.col;
                }
            }
            Key::Char('I') if kind == VisualKind::Block => {
                self.block_insert_enter(st, host, false);
            }
            Key::Char('A') if kind == VisualKind::Block => {
                self.block_insert_enter(st, host, true);
            }
            Key::Char('D') if kind == VisualKind::Block => {
                self.block_to_eol(st, host, Op::Delete);
            }
            Key::Char('C') if kind == VisualKind::Block => {
                self.block_to_eol(st, host, Op::Change);
            }
            Key::Char('"') => self.awaiting = Await::Register,
            Key::Char('d') | Key::Char('x') | Key::Delete => {
                self.visual_op(st, host, Op::Delete, kind)
            }
            Key::Char('c') | Key::Char('s') => self.visual_op(st, host, Op::Change, kind),
            Key::Char('y') => self.visual_op(st, host, Op::Yank, kind),
            Key::Char('>') => self.visual_op(st, host, Op::Indent, kind),
            Key::Char('<') => self.visual_op(st, host, Op::Dedent, kind),
            Key::Char('~') => {
                let upper_any = self.selection_has_lower(st);
                self.visual_op(st, host, if upper_any { Op::Upper } else { Op::Lower }, kind)
            }
            Key::Char('u') => self.visual_op(st, host, Op::Lower, kind),
            Key::Char('U') => self.visual_op(st, host, Op::Upper, kind),
            Key::Char('J') => {
                let Some(buf) = st.buf() else { return };
                let (a, b) = ordered(self.anchor, buf.cursor);
                let joins = (b.line - a.line).max(1);
                if let Some(buf) = st.buf_mut() {
                    buf.cursor = Position::new(a.line, 0);
                }
                self.count = joins + 1;
                self.set_mode(host, Mode::Normal);
                self.do_join(st);
            }
            Key::Char('r') => self.awaiting = Await::Replace,
            Key::Char('i') => self.awaiting = Await::Object(false),
            Key::Char('a') => self.awaiting = Await::Object(true),
            Key::Char('g') => self.awaiting = Await::Prefix('g'),
            Key::Char(':') => self.enter_cmdline(host),
            _ if self.motion_key(st, host, key) => {}
            _ => {}
        }
    }

    /// Apply `op` to the visual selection and drop back to normal mode.
    fn visual_op(&mut self, st: &mut EditorState, host: &HostHandle, op: Op, kind: VisualKind) {
        let Some(buf) = st.buf() else { return };
        let (a, b) = ordered(self.anchor, buf.cursor);
        let span = self.block_span(buf);
        self.op = Some(op);
        self.mode = Mode::Normal;
        host.haptic(6);
        match kind {
            VisualKind::Line => self.apply_linewise(st, host, op, a.line, b.line),
            // Indent-family ops act on whole lines even from a block selection.
            VisualKind::Block if matches!(op, Op::Indent | Op::Dedent | Op::Comment) => {
                self.apply_linewise(st, host, op, a.line, b.line)
            }
            VisualKind::Block => self.apply_block(st, host, op, span),
            VisualKind::Char => self.apply_op(st, host, a, b, MotionKind::Inclusive),
        }
        if !matches!(self.mode, Mode::Insert) {
            self.reset();
        }
        self.op = None;
    }

    /// Rectangle covered by the block selection between the anchor and the cursor.
    fn block_span(&self, buf: &crate::state::Buffer) -> BlockSpan {
        let (a, b) = ordered(self.anchor, buf.cursor);
        let (c1, c2) = if self.anchor.col <= buf.cursor.col {
            (self.anchor.col, buf.cursor.col)
        } else {
            (buf.cursor.col, self.anchor.col)
        };
        BlockSpan { top: a.line, bot: b.line, c1, c2, to_eol: buf.desired_col == usize::MAX }
    }

    /// Apply a charwise operator over the block rectangle, one line at a time.
    fn apply_block(&mut self, st: &mut EditorState, host: &HostHandle, op: Op, span: BlockSpan) {
        let active = st.active();
        let Some(buf) = st.buf_mut() else { return };
        let bot = span.bot.min(buf.text.line_count() - 1);
        let top = span.top.min(bot);
        match op {
            Op::Delete | Op::Change | Op::Yank => {
                if op != Op::Yank {
                    buf.text.begin_undo_group(buf.cursor);
                }
                let mut frags: Vec<String> = Vec::with_capacity(bot - top + 1);
                for l in top..=bot {
                    match span.cols(buf.text.line_len(l)) {
                        Some((s, e)) => {
                            let (a, b) = (Position::new(l, s), Position::new(l, e));
                            frags.push(if op == Op::Yank {
                                extract_range(&buf.text, a, b)
                            } else {
                                buf.text.delete_range(a, b)
                            });
                        }
                        None => frags.push(String::new()),
                    }
                }
                let cursor = buf.text.clamp(Position::new(top, span.c1), op == Op::Change);
                buf.cursor = cursor;
                buf.desired_col = cursor.col;
                if op == Op::Delete {
                    buf.text.end_undo_group();
                }
                self.store_register(st, frags.join("\n"), RegKind::Block, op == Op::Yank);
                if op == Op::Change {
                    // Undo group stays open through the insert; Esc replicates the typed
                    // text onto the block's other lines.
                    self.block_insert = Some(BlockInsert {
                        buf: active,
                        start: cursor,
                        top,
                        bot,
                        col: span.c1,
                        pad: false,
                        to_eol: false,
                    });
                    self.enter_insert(st, host);
                }
            }
            Op::Lower | Op::Upper => {
                buf.text.begin_undo_group(buf.cursor);
                for l in top..=bot {
                    if let Some((s, e)) = span.cols(buf.text.line_len(l)) {
                        map_case_range(
                            &mut buf.text,
                            Position::new(l, s),
                            Position::new(l, e),
                            op == Op::Upper,
                        );
                    }
                }
                buf.text.end_undo_group();
                buf.cursor = buf.text.clamp(Position::new(top, span.c1), false);
                buf.desired_col = buf.cursor.col;
            }
            Op::Indent | Op::Dedent | Op::Comment => unreachable!(),
        }
    }

    /// Block I / A: park the cursor on the top line and arm cross-line replication.
    fn block_insert_enter(&mut self, st: &mut EditorState, host: &HostHandle, append: bool) {
        let Some(buf) = st.buf() else { return };
        let span = self.block_span(buf);
        let active = st.active();
        let Some(buf) = st.buf_mut() else { return };
        let last = buf.text.line_count() - 1;
        let (top, bot) = (span.top.min(last), span.bot.min(last));
        buf.text.begin_undo_group(buf.cursor);
        let col = if append {
            if span.to_eol {
                buf.text.line_len(top)
            } else {
                let target = span.c2 + 1;
                let len = buf.text.line_len(top);
                if len < target {
                    // Pad so typing starts exactly after the block edge.
                    let line = buf.text.line(top).to_string();
                    buf.text.set_line(top, line + &" ".repeat(target - len));
                }
                target
            }
        } else {
            span.c1.min(buf.text.line_len(top))
        };
        buf.cursor = Position::new(top, col);
        self.block_insert = Some(BlockInsert {
            buf: active,
            start: Position::new(top, col),
            top,
            bot,
            col: if append { span.c2 + 1 } else { span.c1 },
            pad: append,
            to_eol: append && span.to_eol,
        });
        self.enter_insert(st, host);
    }

    /// Block D / C: the rect extends to end-of-line on every spanned line.
    fn block_to_eol(&mut self, st: &mut EditorState, host: &HostHandle, op: Op) {
        let Some(buf) = st.buf() else { return };
        let mut span = self.block_span(buf);
        span.to_eol = true;
        self.op = Some(op);
        self.mode = Mode::Normal;
        host.haptic(6);
        self.apply_block(st, host, op, span);
        if !matches!(self.mode, Mode::Insert) {
            self.reset();
        }
        self.op = None;
    }

    /// r{char} in visual: overwrite every selected char.
    fn visual_replace(&mut self, st: &mut EditorState, host: &HostHandle, c: char) {
        let Some(buf) = st.buf() else { return };
        let kind = match self.mode {
            Mode::Visual { kind } => kind,
            _ => VisualKind::Char,
        };
        let span = self.block_span(buf);
        let (a, b) = ordered(self.anchor, buf.cursor);
        if let Some(buf) = st.buf_mut() {
            buf.text.begin_undo_group(a);
            let last = b.line.min(buf.text.line_count() - 1);
            for l in a.line..=last {
                let len = buf.text.line_len(l);
                let (from, to) = match kind {
                    VisualKind::Line => (0, len),
                    VisualKind::Char => (
                        if l == a.line { a.col } else { 0 },
                        if l == b.line { (b.col + 1).min(len) } else { len },
                    ),
                    VisualKind::Block => match span.cols(len) {
                        Some(cols) => cols,
                        None => continue,
                    },
                };
                for col in from..to {
                    buf.text.replace_char(Position::new(l, col), c);
                }
            }
            buf.text.end_undo_group();
            let home = if kind == VisualKind::Block { Position::new(a.line, span.c1) } else { a };
            buf.cursor = buf.text.clamp(home, false);
        }
        self.set_mode(host, Mode::Normal);
        self.reset();
    }

    fn selection_has_lower(&self, st: &EditorState) -> bool {
        let Some(buf) = st.buf() else { return false };
        let (a, b) = ordered(self.anchor, buf.cursor);
        let text = extract_range(&buf.text, a, advance(&buf.text, b));
        text.chars().any(|c| c.is_lowercase())
    }
}

fn ordered(a: Position, b: Position) -> (Position, Position) {
    if a <= b { (a, b) } else { (b, a) }
}

/// Operator-pending which-key panel: the doubled-key row for `op` plus shared motions.
fn op_menu(op: Op) -> KeyHints {
    let (title, doubled) = match op {
        Op::Delete => (" d", ("d", "whole line (dd)")),
        Op::Change => (" c", ("c", "whole line (cc)")),
        Op::Yank => (" y", ("y", "whole line (yy)")),
        Op::Indent => (" >", (">", "this line (>>)")),
        Op::Dedent => (" <", ("<", "this line (<<)")),
        Op::Lower => (" gu", ("u", "whole line (guu)")),
        Op::Upper => (" gU", ("U", "whole line (gUU)")),
        Op::Comment => (" gc", ("c", "this line (gcc)")),
    };
    let mut entries = vec![doubled];
    entries.extend_from_slice(OP_MOTIONS);
    KeyHints { title, entries, immediate: false }
}

// ---------------------------------------------------------------------------
// Insert / replace modes
// ---------------------------------------------------------------------------

impl VimEngine {
    fn insert_key(&mut self, st: &mut EditorState, host: &HostHandle, key: Key) {
        match key {
            Key::Esc | Key::Ctrl('c') => self.leave_insert(st, host),
            Key::Char('k') if self.insert_j => {
                if let Some(buf) = st.buf_mut() {
                    let cur = buf.cursor;
                    if cur.col > 0 {
                        buf.text.delete_range(Position::new(cur.line, cur.col - 1), cur);
                        buf.cursor.col -= 1;
                    }
                }
                self.leave_insert(st, host);
            }
            Key::Char(c) => {
                self.insert_j = c == 'j';
                if let Some(buf) = st.buf_mut() {
                    let cur = buf.cursor;
                    if c == '}' && cur.col >= 4 {
                        let blank_prefix =
                            buf.text.line(cur.line).chars().take(cur.col).all(|ch| ch == ' ');
                        if blank_prefix {
                            buf.text.delete_range(Position::new(cur.line, cur.col - 4), cur);
                            buf.cursor.col -= 4;
                        }
                    }
                    buf.cursor = buf.text.insert_char(buf.cursor, c);
                    buf.desired_col = buf.cursor.col;
                }
            }
            Key::Enter => {
                self.insert_j = false;
                if let Some(buf) = st.buf_mut() {
                    let cur = buf.cursor;
                    let before: String = buf.text.line(cur.line).chars().take(cur.col).collect();
                    let indent: String = before.chars().take_while(|c| c.is_whitespace()).collect();
                    let extra = match before.trim_end().chars().last() {
                        Some('{' | '(' | '[') => "    ",
                        _ => "",
                    };
                    buf.cursor = buf.text.insert_text(cur, &format!("\n{indent}{extra}"));
                    buf.desired_col = buf.cursor.col;
                }
            }
            Key::Backspace => {
                self.insert_j = false;
                if let Some(buf) = st.buf_mut() {
                    let cur = buf.cursor;
                    if cur.col > 0 {
                        buf.text.delete_range(Position::new(cur.line, cur.col - 1), cur);
                        buf.cursor.col -= 1;
                    } else if cur.line > 0 {
                        let plen = buf.text.line_len(cur.line - 1);
                        buf.text.delete_range(Position::new(cur.line - 1, plen), cur);
                        buf.cursor = Position::new(cur.line - 1, plen);
                    }
                    buf.desired_col = buf.cursor.col;
                }
            }
            // Ctrl+w deletes the word before the cursor, stopping at line start.
            Key::Ctrl('w') => {
                self.insert_j = false;
                if let Some(buf) = st.buf_mut() {
                    let cur = buf.cursor;
                    let mut target = word_back(&buf.text, cur, false);
                    if target.line != cur.line {
                        target = Position::new(cur.line, 0);
                    }
                    if target < cur {
                        buf.text.delete_range(target, cur);
                        buf.cursor = buf.text.clamp(target, true);
                        buf.desired_col = buf.cursor.col;
                    }
                }
            }
            // Ctrl+u deletes from the line start to the cursor.
            Key::Ctrl('u') => {
                self.insert_j = false;
                if let Some(buf) = st.buf_mut() {
                    let cur = buf.cursor;
                    if cur.col > 0 {
                        buf.text.delete_range(Position::new(cur.line, 0), cur);
                        buf.cursor = Position::new(cur.line, 0);
                        buf.desired_col = 0;
                    }
                }
            }
            Key::Delete => {
                self.insert_j = false;
                if let Some(buf) = st.buf_mut() {
                    let cur = buf.cursor;
                    let end = advance(&buf.text, cur);
                    if end != cur {
                        buf.text.delete_range(cur, end);
                    }
                }
            }
            Key::Tab => {
                self.insert_j = false;
                if let Some(buf) = st.buf_mut() {
                    buf.cursor = buf.text.insert_text(buf.cursor, "    ");
                    buf.desired_col = buf.cursor.col;
                }
            }
            _ => {
                self.insert_j = false;
                self.move_editing_cursor(st, key);
            }
        }
    }

    fn replace_key(&mut self, st: &mut EditorState, host: &HostHandle, key: Key) {
        match key {
            Key::Esc | Key::Ctrl('c') => self.leave_insert(st, host),
            Key::Char(c) => {
                if let Some(buf) = st.buf_mut() {
                    if !buf.text.replace_char(buf.cursor, c) {
                        buf.cursor = buf.text.insert_char(buf.cursor, c);
                    } else {
                        buf.cursor.col += 1;
                    }
                    buf.desired_col = buf.cursor.col;
                }
            }
            Key::Enter => {
                if let Some(buf) = st.buf_mut() {
                    buf.cursor = buf.text.insert_char(buf.cursor, '\n');
                    buf.desired_col = buf.cursor.col;
                }
            }
            Key::Backspace => {
                if let Some(buf) = st.buf_mut() {
                    buf.cursor.col = buf.cursor.col.saturating_sub(1);
                    buf.desired_col = buf.cursor.col;
                }
            }
            _ => self.move_editing_cursor(st, key),
        }
    }

    /// Arrow/Home/End movement shared by insert and replace modes.
    fn move_editing_cursor(&mut self, st: &mut EditorState, key: Key) {
        if let Some(buf) = st.buf_mut() {
            let cur = buf.cursor;
            let target = match key {
                Key::Left => Position::new(cur.line, cur.col.saturating_sub(1)),
                Key::Right => Position::new(cur.line, cur.col + 1),
                Key::Up => Position::new(cur.line.saturating_sub(1), buf.desired_col),
                Key::Down => Position::new(cur.line + 1, buf.desired_col),
                Key::Home => Position::new(cur.line, 0),
                Key::End => Position::new(cur.line, buf.text.line_len(cur.line)),
                _ => return,
            };
            buf.cursor = buf.text.clamp(target, true);
            if !matches!(key, Key::Up | Key::Down) {
                buf.desired_col = buf.cursor.col;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Command line
// ---------------------------------------------------------------------------

impl VimEngine {
    fn enter_cmdline(&mut self, host: &HostHandle) {
        self.cmdline.clear();
        self.cmdline_cursor = 0;
        self.set_mode(host, Mode::Command);
        self.reset();
    }

    fn cmdline_byte(&self, char_idx: usize) -> usize {
        self.cmdline
            .char_indices()
            .nth(char_idx)
            .map(|(i, _)| i)
            .unwrap_or(self.cmdline.len())
    }

    /// Shared cmdline editing for command and search modes; returns true when handled.
    fn edit_cmdline(&mut self, key: Key) -> bool {
        match key {
            Key::Char(c) => {
                let at = self.cmdline_byte(self.cmdline_cursor);
                self.cmdline.insert(at, c);
                self.cmdline_cursor += 1;
            }
            Key::Backspace => {
                if self.cmdline_cursor > 0 {
                    let at = self.cmdline_byte(self.cmdline_cursor - 1);
                    self.cmdline.remove(at);
                    self.cmdline_cursor -= 1;
                } else {
                    return false;
                }
            }
            Key::Delete => {
                if self.cmdline_cursor < self.cmdline.chars().count() {
                    let at = self.cmdline_byte(self.cmdline_cursor);
                    self.cmdline.remove(at);
                }
            }
            Key::Left => self.cmdline_cursor = self.cmdline_cursor.saturating_sub(1),
            Key::Right => {
                self.cmdline_cursor = (self.cmdline_cursor + 1).min(self.cmdline.chars().count())
            }
            Key::Home => self.cmdline_cursor = 0,
            Key::End => self.cmdline_cursor = self.cmdline.chars().count(),
            _ => return false,
        }
        true
    }

    fn command_key(&mut self, st: &mut EditorState, vfs: &mut Vfs, host: &HostHandle, key: Key) {
        match key {
            Key::Esc | Key::Ctrl('c') => {
                self.cmdline.clear();
                self.cmdline_cursor = 0;
                self.set_mode(host, Mode::Normal);
            }
            Key::Enter => {
                let line = std::mem::take(&mut self.cmdline);
                self.cmdline_cursor = 0;
                self.set_mode(host, Mode::Normal);
                self.execute_cmd(st, vfs, host, &line);
            }
            Key::Backspace if self.cmdline.is_empty() => {
                self.set_mode(host, Mode::Normal);
            }
            _ => {
                let _ = self.edit_cmdline(key);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

impl VimEngine {
    fn enter_search(&mut self, st: &mut EditorState, host: &HostHandle, backwards: bool) {
        let Some(buf) = st.buf() else { return };
        self.search_origin = buf.cursor;
        self.search_prev = st.search.pattern.clone();
        self.cmdline.clear();
        self.cmdline_cursor = 0;
        self.set_mode(host, Mode::Search { backwards });
        self.reset();
    }

    fn search_key(&mut self, st: &mut EditorState, host: &HostHandle, key: Key, backwards: bool) {
        match key {
            Key::Esc | Key::Ctrl('c') => {
                st.search.pattern = std::mem::take(&mut self.search_prev);
                if let Some(buf) = st.buf_mut() {
                    buf.cursor = buf.text.clamp(self.search_origin, false);
                }
                self.cmdline.clear();
                self.cmdline_cursor = 0;
                self.set_mode(host, Mode::Normal);
            }
            Key::Enter => {
                let typed = std::mem::take(&mut self.cmdline);
                self.cmdline_cursor = 0;
                self.set_mode(host, Mode::Normal);
                let pat = if typed.is_empty() { self.search_prev.clone() } else { typed };
                if pat.is_empty() {
                    self.err(st, host, "E35: No previous search pattern");
                    return;
                }
                st.search.pattern = pat.clone();
                st.search.backwards = backwards;
                st.search.suppressed = false;
                self.jump_to_match(st, host, self.search_origin, &pat, backwards, true);
            }
            Key::Backspace if self.cmdline.is_empty() => {
                st.search.pattern = std::mem::take(&mut self.search_prev);
                if let Some(buf) = st.buf_mut() {
                    buf.cursor = buf.text.clamp(self.search_origin, false);
                }
                self.set_mode(host, Mode::Normal);
            }
            _ => {
                if self.edit_cmdline(key) {
                    st.search.pattern = self.cmdline.clone();
                    st.search.suppressed = false;
                    if let Some(buf) = st.buf_mut() {
                        let hit = find_pattern(&buf.text, self.search_origin, &self.cmdline, backwards);
                        buf.cursor = match hit {
                            Some((pos, _)) => buf.text.clamp(pos, false),
                            None => buf.text.clamp(self.search_origin, false),
                        };
                    }
                }
            }
        }
    }

    /// Move to the next match from `from`; reports wrap or E486.
    fn jump_to_match(
        &mut self,
        st: &mut EditorState,
        host: &HostHandle,
        from: Position,
        pat: &str,
        backwards: bool,
        announce: bool,
    ) -> bool {
        let Some(buf) = st.buf_mut() else { return false };
        match find_pattern(&buf.text, from, pat, backwards) {
            Some((pos, wrapped)) => {
                buf.cursor = buf.text.clamp(pos, false);
                buf.desired_col = buf.cursor.col;
                if wrapped {
                    st.info(if backwards {
                        "search hit TOP, continuing at BOTTOM"
                    } else {
                        "search hit BOTTOM, continuing at TOP"
                    });
                } else if announce {
                    st.status = None;
                }
                true
            }
            None => {
                self.err(st, host, format!("E486: Pattern not found: {pat}"));
                false
            }
        }
    }

    fn search_next(&mut self, st: &mut EditorState, host: &HostHandle, reverse: bool) {
        let n = self.total_count();
        self.reset();
        let pat = st.search.pattern.clone();
        if pat.is_empty() {
            self.err(st, host, "E35: No previous search pattern");
            return;
        }
        st.search.suppressed = false;
        let backwards = st.search.backwards ^ reverse;
        for _ in 0..n {
            let Some(from) = st.buf().map(|b| b.cursor) else { return };
            if !self.jump_to_match(st, host, from, &pat, backwards, false) {
                return;
            }
        }
    }

    /// * / #: search for the word under (or after) the cursor.
    fn search_word(&mut self, st: &mut EditorState, host: &HostHandle, backwards: bool) {
        self.reset();
        let Some(buf) = st.buf() else { return };
        let chars: Vec<char> = buf.text.line(buf.cursor.line).chars().collect();
        let start = chars
            .iter()
            .enumerate()
            .skip(buf.cursor.col.min(chars.len()))
            .find(|&(_, &c)| char_class(c, false) == 1)
            .map(|(i, _)| i);
        let Some(mut s) = start else {
            self.err(st, host, "E348: No string under cursor");
            return;
        };
        while s > 0 && char_class(chars[s - 1], false) == 1 {
            s -= 1;
        }
        let mut e = s;
        while e + 1 < chars.len() && char_class(chars[e + 1], false) == 1 {
            e += 1;
        }
        let word: String = chars[s..=e].iter().collect();
        st.search.pattern = word.clone();
        st.search.backwards = backwards;
        st.search.suppressed = false;
        let from = st.buf().map(|b| b.cursor).unwrap_or_default();
        self.jump_to_match(st, host, from, &word, backwards, false);
    }
}

/// All char cols where `pat` occurs in `line`.
fn matches_in_line(line: &str, pat: &[char]) -> Vec<usize> {
    let chars: Vec<char> = line.chars().collect();
    if pat.is_empty() || pat.len() > chars.len() {
        return Vec::new();
    }
    (0..=chars.len() - pat.len()).filter(|&i| chars[i..i + pat.len()] == *pat).collect()
}

/// Next literal match strictly after (or before) `from`, wrapping; true = wrapped.
fn find_pattern(text: &TextBuffer, from: Position, pat: &str, backwards: bool) -> Option<(Position, bool)> {
    let p: Vec<char> = pat.chars().collect();
    if p.is_empty() {
        return None;
    }
    let lc = text.line_count();
    if !backwards {
        if let Some(c) =
            matches_in_line(text.line(from.line), &p).into_iter().find(|&c| c > from.col)
        {
            return Some((Position::new(from.line, c), false));
        }
        for l in from.line + 1..lc {
            if let Some(&c) = matches_in_line(text.line(l), &p).first() {
                return Some((Position::new(l, c), false));
            }
        }
        for l in 0..=from.line {
            for c in matches_in_line(text.line(l), &p) {
                if l < from.line || c <= from.col {
                    return Some((Position::new(l, c), true));
                }
            }
        }
    } else {
        if let Some(c) = matches_in_line(text.line(from.line), &p)
            .into_iter()
            .rev()
            .find(|&c| c < from.col)
        {
            return Some((Position::new(from.line, c), false));
        }
        for l in (0..from.line).rev() {
            if let Some(&c) = matches_in_line(text.line(l), &p).last() {
                return Some((Position::new(l, c), false));
            }
        }
        for l in (from.line..lc).rev() {
            for c in matches_in_line(text.line(l), &p).into_iter().rev() {
                if l > from.line || c >= from.col {
                    return Some((Position::new(l, c), true));
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Ex commands
// ---------------------------------------------------------------------------

/// Parse a line address (`N`, `.`, `$`) at `chars[*i]` into a 0-based line.
fn parse_addr(chars: &[char], i: &mut usize, cur: usize, last: usize) -> Option<usize> {
    match chars.get(*i) {
        Some('.') => {
            *i += 1;
            Some(cur)
        }
        Some('$') => {
            *i += 1;
            Some(last)
        }
        Some(c) if c.is_ascii_digit() => {
            let mut n = 0usize;
            while let Some(d) = chars.get(*i).and_then(|c| c.to_digit(10)) {
                n = n.saturating_mul(10).saturating_add(d as usize);
                *i += 1;
            }
            Some(n.saturating_sub(1))
        }
        _ => None,
    }
}

impl VimEngine {
    fn execute_cmd(&mut self, st: &mut EditorState, vfs: &mut Vfs, host: &HostHandle, input: &str) {
        let s = input.trim();
        if s.is_empty() {
            return;
        }
        let chars: Vec<char> = s.chars().collect();
        let (cur, last) =
            st.buf().map(|b| (b.cursor.line, b.text.line_count() - 1)).unwrap_or((0, 0));
        let mut i = 0usize;
        let mut range: Option<(usize, usize)> = None;
        if chars[0] == '%' {
            range = Some((0, last));
            i = 1;
        } else if let Some(a) = parse_addr(&chars, &mut i, cur, last) {
            let mut b = a;
            if chars.get(i) == Some(&',') {
                i += 1;
                match parse_addr(&chars, &mut i, cur, last) {
                    Some(x) => b = x,
                    None => {
                        self.err(st, host, format!("E492: not an editor command: {s}"));
                        return;
                    }
                }
            }
            range = Some((a, b));
        }
        let rest: String = chars[i..].iter().collect();
        if rest.is_empty() {
            if let (Some((_, b)), Some(buf)) = (range, st.buf_mut()) {
                let line = b.min(buf.text.line_count() - 1);
                buf.cursor = Position::new(line, first_nonblank(buf.text.line(line)));
                buf.desired_col = buf.cursor.col;
            }
            return;
        }
        if let Some(body) = rest.strip_prefix("s/") {
            let (first, last_l) = range.unwrap_or((cur, cur));
            self.substitute(st, host, first, last_l, body);
            return;
        }
        if range.is_some() {
            self.err(st, host, format!("E492: not an editor command: {s}"));
            return;
        }

        let name: String = rest.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
        let after = &rest[name.len()..];
        let bang = after.starts_with('!');
        let arg = after[usize::from(bang)..].trim().to_string();
        match (name.as_str(), bang) {
            ("w", false) | ("write", false) => {
                if !arg.is_empty() {
                    if let Some(buf) = st.buf() {
                        let text = buf.text.text();
                        vfs.write(&arg, &text);
                        st.info(format!("\"{arg}\" {}B written", text.len()));
                    }
                } else {
                    match st.save_active(vfs) {
                        Some((name, bytes)) => st.info(format!("\"{name}\" {bytes}B written")),
                        None => self.err(st, host, "E32: No file name"),
                    }
                }
            }
            ("wa", false) | ("wall", false) => {
                let mut n = 0;
                for buf in &mut st.buffers {
                    vfs.write(&buf.name, &buf.text.text());
                    buf.saved_version = buf.text.version();
                    n += 1;
                }
                st.info(format!("{n} buffer(s) written"));
            }
            ("q", b) | ("quit", b) => {
                if st.windows.len() > 1 {
                    st.close_window();
                } else if !st.buffers.is_empty() {
                    st.close_buffer(st.active(), b);
                }
            }
            ("sp", false) | ("split", false) => self.do_split(st, host, SplitDir::Horizontal),
            ("vs", false) | ("vsplit", false) => self.do_split(st, host, SplitDir::Vertical),
            ("close", false) | ("clo", false) => {
                if !st.close_window() {
                    self.err(st, host, "E444: Cannot close last window");
                }
            }
            ("only", false) | ("on", false) => st.only(),
            ("wq", _) | ("x", _) | ("xit", _) => {
                if st.save_active(vfs).is_some() {
                    st.close_buffer(st.active(), true);
                } else {
                    self.err(st, host, "E32: No file name");
                }
            }
            ("e", b) | ("edit", b) => {
                if !arg.is_empty() {
                    st.open_file(vfs, &arg);
                } else if b {
                    // :e! — reload the active buffer from the vfs.
                    if let Some(buf) = st.buf_mut() {
                        let name = buf.name.clone();
                        if let Some(text) = vfs.read(&name) {
                            *buf = crate::state::Buffer::new(&name, &text);
                            st.info(format!("\"{name}\" reloaded"));
                        } else {
                            self.err(st, host, format!("E484: Can't open file {name}"));
                        }
                    }
                } else {
                    self.err(st, host, "E32: No file name");
                }
            }
            ("enew", _) => {
                st.buffers.push(crate::state::Buffer::new("[No Name]", ""));
                st.set_active(st.buffers.len() - 1);
            }
            ("ls", false) | ("buffers", false) | ("files", false) => {
                let list: Vec<String> = st
                    .buffers
                    .iter()
                    .enumerate()
                    .map(|(i, b)| {
                        format!(
                            "{}{} \"{}\"{}",
                            i + 1,
                            if i == st.active() { "%" } else { " " },
                            b.name,
                            if b.modified() { " [+]" } else { "" }
                        )
                    })
                    .collect();
                if list.is_empty() {
                    st.info("no buffers");
                } else {
                    st.info(list.join("  "));
                }
            }
            ("b", false) | ("buffer", false) | ("bu", false) => {
                if let Ok(n) = arg.parse::<usize>() {
                    if n >= 1 && n <= st.buffers.len() {
                        st.set_active(n - 1);
                    } else {
                        self.err(st, host, format!("E86: Buffer {n} does not exist"));
                    }
                } else if let Some(i) = st.buffers.iter().position(|b| b.name.contains(&arg)) {
                    if arg.is_empty() {
                        self.err(st, host, "E471: Argument required");
                    } else {
                        st.set_active(i);
                    }
                } else {
                    self.err(st, host, format!("E94: No matching buffer for {arg}"));
                }
            }
            ("bn", false) | ("bnext", false) | ("n", false) | ("next", false) => st.next_buffer(),
            ("bp", false) | ("bprev", false) | ("bprevious", false) => st.prev_buffer(),
            ("bd", b) | ("bdelete", b) => {
                if !st.buffers.is_empty() {
                    st.close_buffer(st.active(), b);
                }
            }
            ("noh", false) | ("nohl", false) | ("nohlsearch", false) => {
                st.search.suppressed = true;
            }
            ("set", false) => match arg.as_str() {
                "nu" | "number" => st.options.number = true,
                "nonu" | "nonumber" => st.options.number = false,
                "rnu" | "relativenumber" => st.options.relativenumber = true,
                "nornu" | "norelativenumber" => st.options.relativenumber = false,
                _ => self.err(st, host, format!("E518: Unknown option: {arg}")),
            },
            ("help", false) | ("h", false) => {
                if let Some(i) = st.buffers.iter().position(|b| b.name == help::HELP_BUFFER) {
                    st.set_active(i);
                } else {
                    st.buffers.push(crate::state::Buffer::new(help::HELP_BUFFER, help::HELP_TEXT));
                    st.set_active(st.buffers.len() - 1);
                }
            }
            ("reg", false) | ("registers", false) => {
                let mut names: Vec<char> = st.registers.keys().copied().collect();
                names.sort();
                let dump: Vec<String> = names
                    .iter()
                    .map(|n| {
                        let r = &st.registers[n];
                        let mut text = r.text.replace('\n', "^J");
                        if text.chars().count() > 24 {
                            text = text.chars().take(24).collect::<String>() + "…";
                        }
                        format!("\"{n} {text}")
                    })
                    .collect();
                if dump.is_empty() {
                    st.info("no registers");
                } else {
                    st.info(dump.join("  "));
                }
            }
            ("rm", false) => {
                if arg.is_empty() {
                    self.err(st, host, "E471: Argument required");
                } else if vfs.remove(&arg) {
                    st.info(format!("removed {arg}"));
                } else {
                    self.err(st, host, format!("E484: Can't open file {arg}"));
                }
            }
            _ => self.err(st, host, format!("E492: not an editor command: {s}")),
        }
    }

    /// `:[range]s/pat/rep/[g]` with a literal pattern.
    fn substitute(&mut self, st: &mut EditorState, host: &HostHandle, first: usize, last: usize, body: &str) {
        let mut it = body.splitn(3, '/');
        let pat = it.next().unwrap_or("").to_string();
        let rep = it.next().unwrap_or("").to_string();
        let flags = it.next().unwrap_or("");
        if pat.is_empty() {
            self.err(st, host, "E35: No previous search pattern");
            return;
        }
        let global = flags.contains('g');
        let Some(buf) = st.buf_mut() else { return };
        let last = last.min(buf.text.line_count() - 1);
        let first = first.min(last);
        let mut subs = 0usize;
        let mut lines_hit = 0usize;
        let mut last_line = first;
        buf.text.begin_undo_group(buf.cursor);
        for l in first..=last {
            let line = buf.text.line(l).to_string();
            let (new, n) = if global {
                let n = line.matches(pat.as_str()).count();
                (line.replace(pat.as_str(), &rep), n)
            } else if line.contains(pat.as_str()) {
                (line.replacen(pat.as_str(), &rep, 1), 1)
            } else {
                (line, 0)
            };
            if n > 0 {
                buf.text.set_line(l, new);
                subs += n;
                lines_hit += 1;
                last_line = l;
            }
        }
        buf.text.end_undo_group();
        if subs == 0 {
            self.err(st, host, format!("E486: Pattern not found: {pat}"));
        } else {
            buf.cursor =
                buf.text.clamp(Position::new(last_line, first_nonblank(buf.text.line(last_line))), false);
            buf.desired_col = buf.cursor.col;
            st.info(format!("{subs} substitution(s) on {lines_hit} line(s)"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Buffer;

    struct Env {
        vim: VimEngine,
        st: EditorState,
        vfs: Vfs,
    }

    fn env(text: &str) -> Env {
        let mut st = EditorState::new();
        st.buffers.push(Buffer::new("main.rs", text));
        Env { vim: VimEngine::new(), st, vfs: Vfs::load() }
    }

    fn empty_env() -> Env {
        Env { vim: VimEngine::new(), st: EditorState::new(), vfs: Vfs::load() }
    }

    impl Env {
        fn key(&mut self, k: Key) {
            self.vim.handle_key(&mut self.st, &mut self.vfs, &HostHandle, k);
        }

        /// Feed chars as keys; \u{1b}=Esc, \n=Enter, \u{8}=Backspace, \t=Tab.
        fn feed(&mut self, keys: &str) {
            for c in keys.chars() {
                let k = match c {
                    '\u{1b}' => Key::Esc,
                    '\n' => Key::Enter,
                    '\u{8}' => Key::Backspace,
                    '\t' => Key::Tab,
                    _ => Key::Char(c),
                };
                self.key(k);
            }
        }

        fn text(&self) -> String {
            self.st.buf().map(|b| b.text.text()).unwrap_or_default()
        }

        fn cursor(&self) -> (usize, usize) {
            self.st.buf().map(|b| (b.cursor.line, b.cursor.col)).unwrap_or((0, 0))
        }

        fn set_cursor(&mut self, line: usize, col: usize) {
            if let Some(b) = self.st.buf_mut() {
                b.cursor = Position::new(line, col);
                b.desired_col = col;
            }
        }

        fn reg(&self, name: char) -> Option<&Register> {
            self.st.registers.get(&name)
        }

        fn status(&self) -> String {
            self.st.status.as_ref().map(|m| m.text.clone()).unwrap_or_default()
        }
    }

    // -- word operators ----------------------------------------------------

    #[test]
    fn dw_deletes_word_and_trailing_space() {
        let mut e = env("one two three");
        e.feed("dw");
        assert_eq!(e.text(), "two three");
        assert_eq!(e.cursor(), (0, 0));
    }

    #[test]
    fn dw_mid_word_deletes_to_next_word() {
        let mut e = env("foobar baz");
        e.set_cursor(0, 3);
        e.feed("dw");
        assert_eq!(e.text(), "foobaz");
    }

    #[test]
    fn dw_stops_at_end_of_line() {
        let mut e = env("one two\nnext");
        e.set_cursor(0, 4);
        e.feed("dw");
        assert_eq!(e.text(), "one \nnext");
    }

    #[test]
    fn dw_on_empty_line_joins() {
        let mut e = env("\nabc");
        e.feed("dw");
        assert_eq!(e.text(), "abc");
    }

    #[test]
    fn de_deletes_to_word_end_inclusive() {
        let mut e = env("hello world");
        e.feed("de");
        assert_eq!(e.text(), " world");
    }

    #[test]
    fn db_deletes_back_to_word_start() {
        let mut e = env("hello world");
        e.set_cursor(0, 6);
        e.feed("db");
        assert_eq!(e.text(), "world");
        assert_eq!(e.cursor(), (0, 0));
    }

    #[test]
    fn d2w_and_3dw_counts() {
        let mut e = env("one two three four");
        e.feed("d2w");
        assert_eq!(e.text(), "three four");
        let mut e = env("one two three four");
        e.feed("3dw");
        assert_eq!(e.text(), "four");
    }

    #[test]
    fn cw_acts_like_ce_on_a_word() {
        let mut e = env("hello world");
        e.feed("cwbye\u{1b}");
        assert_eq!(e.text(), "bye world");
        assert_eq!(e.vim.mode(), Mode::Normal);
    }

    #[test]
    fn w_b_e_movement_and_punctuation() {
        let mut e = env("foo.bar baz");
        e.feed("w");
        assert_eq!(e.cursor(), (0, 3));
        e.feed("w");
        assert_eq!(e.cursor(), (0, 4));
        e.feed("w");
        assert_eq!(e.cursor(), (0, 8));
        e.feed("b");
        assert_eq!(e.cursor(), (0, 4));
        e.feed("e");
        assert_eq!(e.cursor(), (0, 6));
        e.feed("W");
        assert_eq!(e.cursor(), (0, 8));
    }

    #[test]
    fn ge_moves_to_previous_word_end() {
        let mut e = env("one two");
        e.set_cursor(0, 5);
        e.feed("ge");
        assert_eq!(e.cursor(), (0, 2));
    }

    #[test]
    fn w_lands_on_empty_lines() {
        let mut e = env("\n\nabc");
        e.feed("w");
        assert_eq!(e.cursor(), (1, 0));
        e.feed("w");
        assert_eq!(e.cursor(), (2, 0));
        e.feed("$");
        assert_eq!(e.cursor(), (2, 2));
        e.set_cursor(0, 0);
        e.feed("$");
        assert_eq!(e.cursor(), (0, 0));
        e.feed("^");
        assert_eq!(e.cursor(), (0, 0));
        e.feed("0");
        assert_eq!(e.cursor(), (0, 0));
    }

    // -- text objects -------------------------------------------------------

    #[test]
    fn ciw_replaces_inner_word() {
        let mut e = env("foo bar baz");
        e.set_cursor(0, 5);
        e.feed("ciwqux\u{1b}");
        assert_eq!(e.text(), "foo qux baz");
    }

    #[test]
    fn daw_takes_trailing_space() {
        let mut e = env("foo bar baz");
        e.set_cursor(0, 5);
        e.feed("daw");
        assert_eq!(e.text(), "foo baz");
    }

    #[test]
    fn di_paren_nested_and_on_delimiter() {
        let mut e = env("a(b(c)d)e");
        e.set_cursor(0, 4);
        e.feed("di(");
        assert_eq!(e.text(), "a(b()d)e");
        let mut e = env("a(b(c)d)e");
        e.set_cursor(0, 1);
        e.feed("di(");
        assert_eq!(e.text(), "a()e");
        let mut e = env("a(b(c)d)e");
        e.set_cursor(0, 7);
        e.feed("dib");
        assert_eq!(e.text(), "a()e");
    }

    #[test]
    fn da_quote_takes_quotes_and_space() {
        let mut e = env("say \"hi\" now");
        e.set_cursor(0, 5);
        e.feed("da\"");
        assert_eq!(e.text(), "say now");
        let mut e = env("say \"hi\" now");
        e.set_cursor(0, 5);
        e.feed("di\"");
        assert_eq!(e.text(), "say \"\" now");
    }

    #[test]
    fn quote_object_before_first_quote_uses_forward_pair() {
        let mut e = env("x = \"val\"");
        e.feed("ci\"new\u{1b}");
        assert_eq!(e.text(), "x = \"new\"");
    }

    #[test]
    fn ca_brace_multiline() {
        let mut e = env("fn f() {\n    body();\n}");
        e.set_cursor(1, 4);
        e.feed("ca{x\u{1b}");
        assert_eq!(e.text(), "fn f() x");
    }

    #[test]
    fn yi_bracket_and_angle() {
        let mut e = env("v[a, b] <T>");
        e.set_cursor(0, 3);
        e.feed("yi[");
        assert_eq!(e.reg('"').unwrap().text, "a, b");
        e.set_cursor(0, 9);
        e.feed("di<");
        assert_eq!(e.text(), "v[a, b] <>");
    }

    // -- lines: dd, J, indent, comments --------------------------------------

    #[test]
    fn dd_on_last_line() {
        let mut e = env("a\nb");
        e.set_cursor(1, 0);
        e.feed("dd");
        assert_eq!(e.text(), "a");
        assert_eq!(e.cursor(), (0, 0));
        e.feed("dd");
        assert_eq!(e.text(), "");
        assert_eq!(e.st.buf().unwrap().text.line_count(), 1);
    }

    #[test]
    fn count_3dd_deletes_three_lines() {
        let mut e = env("1\n2\n3\n4");
        e.feed("3dd");
        assert_eq!(e.text(), "4");
        assert_eq!(e.reg('"').unwrap().kind, RegKind::Line);
        assert_eq!(e.reg('"').unwrap().text, "1\n2\n3");
    }

    #[test]
    fn dj_is_linewise() {
        let mut e = env("1\n2\n3");
        e.feed("dj");
        assert_eq!(e.text(), "3");
    }

    #[test]
    fn join_collapses_indent_with_space() {
        let mut e = env("foo\n    bar\nbaz");
        e.feed("J");
        assert_eq!(e.text(), "foo bar\nbaz");
        assert_eq!(e.cursor(), (0, 3));
        let mut e = env("a\nb\nc\nd");
        e.feed("3J");
        assert_eq!(e.text(), "a b c\nd");
    }

    #[test]
    fn indent_and_dedent() {
        let mut e = env("fn x\nlet y");
        e.feed("2>>");
        assert_eq!(e.text(), "    fn x\n    let y");
        e.feed("<<");
        assert_eq!(e.text(), "fn x\n    let y");
        e.set_cursor(0, 0);
        e.feed(">j");
        assert_eq!(e.text(), "    fn x\n        let y");
    }

    #[test]
    fn gcc_toggles_line_comment() {
        let mut e = env("fn main() {\n    x();\n}");
        e.feed("gcc");
        assert_eq!(e.text(), "// fn main() {\n    x();\n}");
        e.feed("gcc");
        assert_eq!(e.text(), "fn main() {\n    x();\n}");
    }

    #[test]
    fn gc_visual_comments_block() {
        let mut e = env("a();\n    b();");
        e.feed("Vjgc");
        assert_eq!(e.text(), "// a();\n    // b();");
        assert_eq!(e.vim.mode(), Mode::Normal);
        e.feed("Vjgc");
        assert_eq!(e.text(), "a();\n    b();");
    }

    // -- x, s, r, ~, case ----------------------------------------------------

    #[test]
    fn x_at_line_end_clamps_cursor() {
        let mut e = env("abc");
        e.feed("$x");
        assert_eq!(e.text(), "ab");
        assert_eq!(e.cursor(), (0, 1));
        e.feed("xx");
        assert_eq!(e.text(), "");
        e.feed("x");
        assert_eq!(e.text(), "");
    }

    #[test]
    fn count_x_and_big_x() {
        let mut e = env("abcdef");
        e.feed("3x");
        assert_eq!(e.text(), "def");
        e.feed("$X");
        assert_eq!(e.text(), "df");
    }

    #[test]
    fn s_substitutes_char_and_inserts() {
        let mut e = env("abc");
        e.feed("sx\u{1b}");
        assert_eq!(e.text(), "xbc");
    }

    #[test]
    fn r_replaces_count_chars() {
        let mut e = env("abcd");
        e.feed("rx");
        assert_eq!(e.text(), "xbcd");
        e.feed("3ry");
        assert_eq!(e.text(), "yyyd");
        assert_eq!(e.cursor(), (0, 2));
        e.feed("$9rz");
        assert_eq!(e.text(), "yyyd");
    }

    #[test]
    fn replace_mode_overtypes_and_appends() {
        let mut e = env("abcd");
        e.feed("Rxy\u{1b}");
        assert_eq!(e.text(), "xycd");
        assert_eq!(e.cursor(), (0, 1));
        let mut e = env("ab");
        e.feed("Rwxyz\u{1b}");
        assert_eq!(e.text(), "wxyz");
    }

    #[test]
    fn tilde_toggles_case_and_advances() {
        let mut e = env("aBc");
        e.feed("3~");
        assert_eq!(e.text(), "AbC");
        assert_eq!(e.cursor(), (0, 2));
    }

    #[test]
    fn gu_gU_operators() {
        let mut e = env("abc def");
        e.feed("gUe");
        assert_eq!(e.text(), "ABC def");
        e.feed("guu");
        assert_eq!(e.text(), "abc def");
        let mut e = env("MiXeD");
        e.feed("veu");
        assert_eq!(e.text(), "mixed");
    }

    #[test]
    fn c_and_d_to_eol() {
        let mut e = env("hello world");
        e.set_cursor(0, 5);
        e.feed("D");
        assert_eq!(e.text(), "hello");
        let mut e = env("hello world");
        e.set_cursor(0, 6);
        e.feed("C!\u{1b}");
        assert_eq!(e.text(), "hello !");
    }

    // -- registers and paste --------------------------------------------------

    #[test]
    fn paste_charwise_after_and_before() {
        let mut e = env("ab");
        e.feed("x");
        assert_eq!(e.reg('"').unwrap().text, "a");
        e.feed("p");
        assert_eq!(e.text(), "ba");
        assert_eq!(e.cursor(), (0, 1));
        e.feed("P");
        assert_eq!(e.text(), "baa");
    }

    #[test]
    fn paste_linewise_below_and_above() {
        let mut e = env("one\ntwo");
        e.feed("yyp");
        assert_eq!(e.text(), "one\none\ntwo");
        assert_eq!(e.cursor(), (1, 0));
        e.feed("2P");
        assert_eq!(e.text(), "one\none\none\none\ntwo");
    }

    #[test]
    fn count_paste_charwise() {
        let mut e = env("xy");
        e.feed("ylp");
        assert_eq!(e.text(), "xxy");
        let mut e = env("xy");
        e.feed("yl");
        assert_eq!(e.reg('"').unwrap().text, "x");
        e.feed("3p");
        assert_eq!(e.text(), "xxxxy");
    }

    #[test]
    fn named_register_and_yank_register() {
        let mut e = env("one\ntwo");
        e.feed("\"ayy");
        assert_eq!(e.reg('a').unwrap().text, "one");
        // A named yank fills the unnamed register but skips register 0.
        assert!(e.reg('0').is_none());
        e.feed("yy");
        assert_eq!(e.reg('0').unwrap().text, "one");
        // A delete goes to the unnamed register and leaves register 0 alone.
        e.feed("dd");
        assert_eq!(e.reg('"').unwrap().text, "one");
        assert_eq!(e.reg('0').unwrap().text, "one");
        e.feed("\"ap");
        assert_eq!(e.text(), "two\none");
    }

    #[test]
    fn yank_word_then_paste() {
        let mut e = env("foo bar");
        e.feed("yw");
        assert_eq!(e.reg('"').unwrap().text, "foo ");
        assert_eq!(e.reg('"').unwrap().kind, RegKind::Char);
        e.feed("$p");
        assert_eq!(e.text(), "foo barfoo ");
    }

    // -- undo, redo, dot ------------------------------------------------------

    #[test]
    fn insert_session_is_one_undo_step() {
        let mut e = env("ab");
        e.feed("ihello \u{1b}");
        assert_eq!(e.text(), "hello ab");
        e.feed("u");
        assert_eq!(e.text(), "ab");
        e.key(Key::Ctrl('r'));
        assert_eq!(e.text(), "hello ab");
    }

    #[test]
    fn o_open_and_typed_text_is_one_undo() {
        let mut e = env("  foo");
        e.feed("obar\u{1b}");
        assert_eq!(e.text(), "  foo\n  bar");
        e.feed("u");
        assert_eq!(e.text(), "  foo");
    }

    #[test]
    fn discrete_changes_undo_separately() {
        let mut e = env("abcd");
        e.feed("xx");
        assert_eq!(e.text(), "cd");
        e.feed("u");
        assert_eq!(e.text(), "bcd");
        e.feed("u");
        assert_eq!(e.text(), "abcd");
        e.feed("u");
        assert!(e.status().contains("oldest"));
    }

    #[test]
    fn dot_repeats_dw() {
        let mut e = env("one two three");
        e.feed("dw");
        assert_eq!(e.text(), "two three");
        e.feed(".");
        assert_eq!(e.text(), "three");
    }

    #[test]
    fn dot_repeats_insert_session() {
        let mut e = env("x");
        e.feed("iab\u{1b}");
        assert_eq!(e.text(), "abx");
        e.feed(".");
        assert_eq!(e.text(), "aabbx");
    }

    #[test]
    fn dot_repeats_x_r_and_indent() {
        let mut e = env("abcdef");
        e.feed("x.");
        assert_eq!(e.text(), "cdef");
        // r leaves the cursor on the replaced char, so . re-replaces in place.
        e.feed("ry.");
        assert_eq!(e.text(), "ydef");
        let mut e = env("a\nb");
        e.feed(">>j.");
        assert_eq!(e.text(), "    a\n    b");
    }

    #[test]
    fn motions_do_not_become_dot_target() {
        let mut e = env("one two");
        e.feed("x");
        e.feed("wjklhG");
        e.feed(".");
        assert_eq!(e.text(), "e two");
    }

    // -- f/t and ; , -----------------------------------------------------------

    #[test]
    fn find_char_and_semicolon_repeat() {
        let mut e = env("abcabcabc");
        e.feed("fb");
        assert_eq!(e.cursor(), (0, 1));
        e.feed(";");
        assert_eq!(e.cursor(), (0, 4));
        e.feed(";");
        assert_eq!(e.cursor(), (0, 7));
        e.feed(",");
        assert_eq!(e.cursor(), (0, 4));
        e.feed("Fa");
        assert_eq!(e.cursor(), (0, 3));
    }

    #[test]
    fn t_and_dt_motion() {
        let mut e = env("abc");
        e.feed("tc");
        assert_eq!(e.cursor(), (0, 1));
        let mut e = env("say: done");
        e.feed("dt:");
        assert_eq!(e.text(), ": done");
        let mut e = env("abc");
        e.feed("dfc");
        assert_eq!(e.text(), "");
    }

    #[test]
    fn count_find() {
        let mut e = env("a.b.c.d");
        e.feed("2f.");
        assert_eq!(e.cursor(), (0, 3));
        e.feed("f.");
        assert_eq!(e.cursor(), (0, 5));
    }

    // -- search -----------------------------------------------------------------

    #[test]
    fn search_commits_and_n_wraps_with_message() {
        let mut e = env("foo\nbar\nfoo");
        e.feed("/foo\n");
        assert_eq!(e.cursor(), (2, 0));
        assert_eq!(e.st.search.pattern, "foo");
        e.feed("n");
        assert_eq!(e.cursor(), (0, 0));
        assert!(e.status().contains("BOTTOM"), "{}", e.status());
        e.feed("N");
        assert_eq!(e.cursor(), (2, 0));
        assert!(e.status().contains("TOP"), "{}", e.status());
    }

    #[test]
    fn incremental_search_moves_and_esc_restores() {
        let mut e = env("hello");
        e.feed("/ll");
        assert_eq!(e.cursor(), (0, 2));
        assert_eq!(e.st.search.pattern, "ll");
        e.key(Key::Esc);
        assert_eq!(e.cursor(), (0, 0));
        assert_eq!(e.st.search.pattern, "");
        assert_eq!(e.vim.mode(), Mode::Normal);
    }

    #[test]
    fn search_not_found_reports_e486() {
        let mut e = env("hello");
        e.feed("/zz\n");
        assert!(e.status().contains("E486"));
        assert_eq!(e.cursor(), (0, 0));
    }

    #[test]
    fn backwards_search_and_star() {
        let mut e = env("foo bar foo bar");
        e.set_cursor(0, 14);
        e.feed("?foo\n");
        assert_eq!(e.cursor(), (0, 8));
        e.feed("n");
        assert_eq!(e.cursor(), (0, 0));
        let mut e = env("foo bar foo");
        e.feed("*");
        assert_eq!(e.st.search.pattern, "foo");
        assert_eq!(e.cursor(), (0, 8));
        e.feed("#");
        assert_eq!(e.cursor(), (0, 0));
    }

    #[test]
    fn noh_suppresses_until_next_search() {
        let mut e = env("aaa");
        e.feed("/a\n");
        assert!(!e.st.search.suppressed);
        e.feed(":noh\n");
        assert!(e.st.search.suppressed);
        e.feed("n");
        assert!(!e.st.search.suppressed);
    }

    // -- ex commands ---------------------------------------------------------

    #[test]
    fn substitute_current_line_and_global() {
        let mut e = env("a a\nb a a");
        e.feed(":s/a/x/\n");
        assert_eq!(e.text(), "x a\nb a a");
        e.set_cursor(1, 0);
        e.feed(":s/a/x/g\n");
        assert_eq!(e.text(), "x a\nb x x");
    }

    #[test]
    fn substitute_percent_range_and_counts() {
        let mut e = env("a a\na a\na a");
        e.feed(":%s/a/b/g\n");
        assert_eq!(e.text(), "b b\nb b\nb b");
        assert!(e.status().contains("6 substitution(s) on 3 line(s)"), "{}", e.status());
    }

    #[test]
    fn substitute_numeric_and_dollar_ranges() {
        let mut e = env("a\na\na\na");
        e.feed(":2,3s/a/b/\n");
        assert_eq!(e.text(), "a\nb\nb\na");
        e.feed(":.,$s/a/c/\n");
        assert_eq!(e.text(), "a\nb\nb\nc".replace("b\nb\nc", "b\nb\nc"));
        assert_eq!(e.text(), "a\nb\nb\nc");
        e.feed(":1s/a/z/\n");
        assert_eq!(e.text(), "z\nb\nb\nc");
    }

    #[test]
    fn substitute_is_one_undo_and_reports_miss() {
        let mut e = env("a a a");
        e.feed(":s/a/b/g\n");
        assert_eq!(e.text(), "b b b");
        e.feed("u");
        assert_eq!(e.text(), "a a a");
        e.feed(":s/zz/b/\n");
        assert!(e.status().contains("E486"));
    }

    #[test]
    fn line_jump_and_dollar() {
        let mut e = env("1\n2\n3\n4\n5");
        e.feed(":4\n");
        assert_eq!(e.cursor(), (3, 0));
        e.feed(":$\n");
        assert_eq!(e.cursor(), (4, 0));
        e.feed(":1\n");
        assert_eq!(e.cursor(), (0, 0));
    }

    #[test]
    fn gg_G_and_counted_G() {
        let mut e = env("1\n2\n3\n4\n5\n6\n7\n8\n9\n10");
        e.feed("G");
        assert_eq!(e.cursor(), (9, 0));
        e.feed("gg");
        assert_eq!(e.cursor(), (0, 0));
        e.feed("5G");
        assert_eq!(e.cursor(), (4, 0));
        e.feed("3gg");
        assert_eq!(e.cursor(), (2, 0));
        e.feed("dG");
        assert_eq!(e.text(), "1\n2");
    }

    #[test]
    fn unknown_command_reports_e492() {
        let mut e = env("x");
        e.feed(":frobnicate\n");
        assert!(e.status().contains("E492"), "{}", e.status());
        assert!(e.status().contains("frobnicate"));
    }

    #[test]
    fn set_number_options() {
        let mut e = env("x");
        e.feed(":set nonu\n");
        assert!(!e.st.options.number);
        e.feed(":set nu\n");
        assert!(e.st.options.number);
        e.feed(":set nornu\n");
        assert!(!e.st.options.relativenumber);
        e.feed(":set rnu\n");
        assert!(e.st.options.relativenumber);
        e.feed(":set bogus\n");
        assert!(e.status().contains("E518"));
    }

    #[test]
    fn write_quit_and_buffer_commands() {
        let mut e = env("hello");
        e.feed(":w\n");
        assert_eq!(e.vfs.read("main.rs").as_deref(), Some("hello"));
        assert!(!e.st.buf().unwrap().modified());
        e.feed(":e other.rs\n");
        assert_eq!(e.st.buffers.len(), 2);
        assert_eq!(e.st.buf().unwrap().name, "other.rs");
        e.feed(":bp\n");
        assert_eq!(e.st.buf().unwrap().name, "main.rs");
        e.feed(":bn\n");
        assert_eq!(e.st.buf().unwrap().name, "other.rs");
        e.feed(":b 1\n");
        assert_eq!(e.st.buf().unwrap().name, "main.rs");
        e.feed(":b other\n");
        assert_eq!(e.st.buf().unwrap().name, "other.rs");
        e.feed(":bd\n");
        assert_eq!(e.st.buffers.len(), 1);
    }

    #[test]
    fn quit_refuses_modified_without_bang() {
        let mut e = env("x");
        e.feed("ia\u{1b}");
        e.feed(":q\n");
        assert_eq!(e.st.buffers.len(), 1);
        assert!(e.status().contains("E89"));
        e.feed(":q!\n");
        assert!(e.st.buffers.is_empty());
    }

    #[test]
    fn wq_writes_and_closes() {
        let mut e = env("data");
        e.feed("ix\u{1b}:wq\n");
        assert!(e.st.buffers.is_empty());
        assert_eq!(e.vfs.read("main.rs").as_deref(), Some("xdata"));
    }

    #[test]
    fn edit_bang_reloads_from_vfs() {
        let mut e = env("orig");
        e.feed(":w\n");
        e.feed("dd");
        assert_eq!(e.text(), "");
        e.feed(":e!\n");
        assert_eq!(e.text(), "orig");
        assert!(!e.st.buf().unwrap().modified());
    }

    #[test]
    fn w_with_name_and_rm() {
        let mut e = env("abc");
        e.feed(":w copy.rs\n");
        assert_eq!(e.vfs.read("copy.rs").as_deref(), Some("abc"));
        e.feed(":rm copy.rs\n");
        assert!(e.vfs.read("copy.rs").is_none());
        e.feed(":rm copy.rs\n");
        assert!(e.status().contains("E484"));
    }

    #[test]
    fn help_opens_single_help_buffer() {
        let mut e = env("x");
        e.feed(":help\n");
        assert_eq!(e.st.buf().unwrap().name, help::HELP_BUFFER);
        assert!(e.text().contains("cheatsheet"));
        e.feed(":bp\n:help\n");
        assert_eq!(e.st.buffers.len(), 2);
        assert_eq!(e.st.buf().unwrap().name, help::HELP_BUFFER);
    }

    #[test]
    fn cmdline_editing_keys() {
        let mut e = env("x");
        e.feed(":wx");
        e.key(Key::Left);
        e.key(Key::Backspace);
        assert_eq!(e.vim.cmdline(), Some((':', "x", 0usize)));
        e.key(Key::End);
        e.key(Key::Char('y'));
        assert_eq!(e.vim.cmdline(), Some((':', "xy", 2usize)));
        e.key(Key::Home);
        e.key(Key::Delete);
        assert_eq!(e.vim.cmdline(), Some((':', "y", 0usize)));
        e.key(Key::Esc);
        assert_eq!(e.vim.mode(), Mode::Normal);
        e.feed(":");
        e.key(Key::Backspace);
        assert_eq!(e.vim.mode(), Mode::Normal);
    }

    // -- visual mode ----------------------------------------------------------

    #[test]
    fn visual_charwise_delete_and_yank() {
        let mut e = env("hello world");
        e.feed("ved");
        assert_eq!(e.text(), " world");
        assert_eq!(e.vim.mode(), Mode::Normal);
        let mut e = env("hello world");
        e.feed("vey");
        assert_eq!(e.reg('"').unwrap().text, "hello");
        assert_eq!(e.reg('"').unwrap().kind, RegKind::Char);
        assert_eq!(e.cursor(), (0, 0));
    }

    #[test]
    fn visual_linewise_delete_and_paste() {
        let mut e = env("one\ntwo\nthree");
        e.feed("Vjd");
        assert_eq!(e.text(), "three");
        assert_eq!(e.reg('"').unwrap().kind, RegKind::Line);
        e.feed("Vy");
        e.feed("p");
        assert_eq!(e.text(), "three\nthree");
    }

    #[test]
    fn visual_o_swaps_ends_and_esc_cancels() {
        let mut e = env("abcdef");
        e.set_cursor(0, 3);
        e.feed("vll");
        assert_eq!(e.vim.visual_range(e.st.buf().unwrap().cursor).unwrap().0, Position::new(0, 3));
        e.feed("o");
        assert_eq!(e.cursor(), (0, 3));
        e.feed("\u{1b}");
        assert_eq!(e.vim.mode(), Mode::Normal);
        assert!(e.vim.visual_range(Position::default()).is_none());
    }

    #[test]
    fn visual_object_and_change() {
        let mut e = env("say (big words) now");
        e.set_cursor(0, 7);
        e.feed("vi(d");
        assert_eq!(e.text(), "say () now");
        let mut e = env("hello world");
        e.set_cursor(0, 7);
        e.feed("viwcbye\u{1b}");
        assert_eq!(e.text(), "hello bye");
    }

    #[test]
    fn visual_switch_char_to_line() {
        let mut e = env("aaa\nbbb");
        e.feed("v");
        assert_eq!(e.vim.mode(), Mode::Visual { kind: VisualKind::Char });
        e.feed("V");
        assert_eq!(e.vim.mode(), Mode::Visual { kind: VisualKind::Line });
        e.feed("jd");
        assert_eq!(e.text(), "");
    }

    #[test]
    fn visual_replace_char() {
        let mut e = env("abc");
        e.feed("vllrx");
        assert_eq!(e.text(), "xxx");
        assert_eq!(e.vim.mode(), Mode::Normal);
    }

    #[test]
    fn visual_indent() {
        let mut e = env("a\nb");
        e.feed("Vj>");
        assert_eq!(e.text(), "    a\n    b");
    }

    // -- marks -----------------------------------------------------------------

    #[test]
    fn marks_backtick_exact_and_quote_line() {
        let mut e = env("one\n  two\nthree");
        e.set_cursor(1, 4);
        e.feed("ma");
        e.feed("gg");
        assert_eq!(e.cursor(), (0, 0));
        e.feed("`a");
        assert_eq!(e.cursor(), (1, 4));
        e.feed("gg'a");
        assert_eq!(e.cursor(), (1, 2));
        e.feed("`z");
        assert!(e.status().contains("E20"));
    }

    #[test]
    fn delete_to_mark() {
        let mut e = env("abcdef");
        e.set_cursor(0, 4);
        e.feed("mq0");
        e.feed("d`q");
        assert_eq!(e.text(), "ef");
    }

    // -- insert mode details ------------------------------------------------------

    #[test]
    fn jk_quick_escape_leaves_clean_buffer() {
        let mut e = env("");
        e.feed("iabjk");
        assert_eq!(e.text(), "ab");
        assert_eq!(e.vim.mode(), Mode::Normal);
        let mut e = env("");
        e.feed("ijx\u{1b}");
        assert_eq!(e.text(), "jx");
        let mut e = env("");
        e.feed("ijjk");
        assert_eq!(e.text(), "j");
    }

    #[test]
    fn enter_keeps_indent_and_adds_after_brace() {
        let mut e = env("    foo");
        e.feed("A\nbar\u{1b}");
        assert_eq!(e.text(), "    foo\n    bar");
        let mut e = env("fn f() {");
        e.feed("A\nx\u{1b}");
        assert_eq!(e.text(), "fn f() {\n    x");
    }

    #[test]
    fn closing_brace_dedents_blank_line() {
        let mut e = env("fn f() {");
        e.feed("A\n}\u{1b}");
        assert_eq!(e.text(), "fn f() {\n}");
    }

    #[test]
    fn backspace_joins_lines_and_tab_indents() {
        let mut e = env("ab\ncd");
        e.set_cursor(1, 0);
        e.feed("i\u{8}\u{1b}");
        assert_eq!(e.text(), "abcd");
        assert_eq!(e.cursor(), (0, 1));
        let mut e = env("x");
        e.feed("i\t\u{1b}");
        assert_eq!(e.text(), "    x");
    }

    #[test]
    fn insert_entries_a_A_I_o_O() {
        let mut e = env("  ab");
        e.feed("a1\u{1b}");
        assert_eq!(e.text(), " 1 ab");
        let mut e = env("  ab");
        e.feed("A!\u{1b}");
        assert_eq!(e.text(), "  ab!");
        let mut e = env("  ab");
        e.feed("I>\u{1b}");
        assert_eq!(e.text(), "  >ab");
        let mut e = env("  ab");
        e.feed("ox\u{1b}");
        assert_eq!(e.text(), "  ab\n  x");
        let mut e = env("  ab");
        e.feed("Oy\u{1b}");
        assert_eq!(e.text(), "  y\n  ab");
    }

    #[test]
    fn insert_arrows_move_without_typing() {
        let mut e = env("abc");
        e.feed("i");
        e.key(Key::End);
        e.feed("!");
        assert_eq!(e.text(), "abc!");
        e.key(Key::Home);
        e.feed("?");
        e.key(Key::Esc);
        assert_eq!(e.text(), "?abc!");
    }

    #[test]
    fn cc_keeps_indentation() {
        let mut e = env("    foo();\nbar");
        e.feed("ccnew\u{1b}");
        assert_eq!(e.text(), "    new\nbar");
        let mut e = env("only");
        e.feed("Sx\u{1b}");
        assert_eq!(e.text(), "x");
    }

    // -- unicode ---------------------------------------------------------------

    #[test]
    fn unicode_word_and_char_ops() {
        let mut e = env("héllo wörld");
        e.feed("dw");
        assert_eq!(e.text(), "wörld");
        e.feed("x");
        assert_eq!(e.text(), "örld");
        let mut e = env("中文 abc");
        e.feed("ciwé\u{1b}");
        assert_eq!(e.text(), "é abc");
        e.feed("fa");
        assert_eq!(e.cursor(), (0, 2));
    }

    #[test]
    fn unicode_search_substitute_and_objects() {
        let mut e = env("let é = \"ü中\";");
        e.feed("/ü\n");
        assert_eq!(e.cursor(), (0, 9));
        e.feed("di\"");
        assert_eq!(e.text(), "let é = \"\";");
        e.feed(":s/é/e/\n");
        assert_eq!(e.text(), "let e = \"\";");
        let mut e = env("é中");
        e.feed("~~");
        assert_eq!(e.text(), "É中");
    }

    #[test]
    fn unicode_insert_and_visual() {
        let mut e = env("ab");
        e.feed("i中é\u{1b}");
        assert_eq!(e.text(), "中éab");
        // Esc leaves the cursor on é, so vl selects é and a.
        e.feed("vly");
        assert_eq!(e.reg('"').unwrap().text, "éa");
    }

    // -- brackets and paragraphs -------------------------------------------------

    #[test]
    fn percent_jumps_and_deletes() {
        let mut e = env("a(b)c");
        e.feed("%");
        assert_eq!(e.cursor(), (0, 3));
        e.feed("%");
        assert_eq!(e.cursor(), (0, 1));
        e.feed("d%");
        assert_eq!(e.text(), "ac");
        let mut e = env("{\n  x\n}");
        e.feed("%");
        assert_eq!(e.cursor(), (2, 0));
    }

    #[test]
    fn paragraph_motions() {
        let mut e = env("a\n\nb\nb2\n\nc");
        e.feed("}");
        assert_eq!(e.cursor(), (1, 0));
        e.feed("}");
        assert_eq!(e.cursor(), (4, 0));
        e.feed("}");
        assert_eq!(e.cursor(), (5, 0));
        e.feed("{{");
        assert_eq!(e.cursor(), (1, 0));
        let mut e = env("a\n\nb");
        e.feed("d}");
        assert_eq!(e.text(), "\nb");
    }

    // -- scrolling requests -------------------------------------------------------

    #[test]
    fn scroll_requests_are_queued_and_consumed() {
        let mut e = env("line");
        e.feed("zz");
        assert_eq!(e.vim.take_scroll_request(), Some(ScrollRequest::Center));
        assert_eq!(e.vim.take_scroll_request(), None);
        e.feed("zt");
        assert_eq!(e.vim.take_scroll_request(), Some(ScrollRequest::Top));
        e.feed("zb");
        assert_eq!(e.vim.take_scroll_request(), Some(ScrollRequest::Bottom));
        e.key(Key::Ctrl('d'));
        assert_eq!(e.vim.take_scroll_request(), Some(ScrollRequest::HalfDown));
        e.key(Key::Ctrl('u'));
        assert_eq!(e.vim.take_scroll_request(), Some(ScrollRequest::HalfUp));
        e.key(Key::Ctrl('f'));
        assert_eq!(e.vim.take_scroll_request(), Some(ScrollRequest::PageDown));
        e.key(Key::PageUp);
        assert_eq!(e.vim.take_scroll_request(), Some(ScrollRequest::PageUp));
    }

    // -- finder routing --------------------------------------------------------

    #[test]
    fn ctrl_p_opens_finder_and_esc_closes() {
        let mut e = env("x");
        e.key(Key::Ctrl('p'));
        assert!(e.st.finder.is_some());
        e.key(Key::Esc);
        assert!(e.st.finder.is_none());
        assert_eq!(e.vim.mode(), Mode::Normal);
    }

    #[test]
    fn space_ff_leader_opens_finder() {
        let mut e = env("x");
        e.feed(" ff");
        assert!(e.st.finder.is_some());
        e.key(Key::Esc);
        let mut e = env("abc");
        e.feed(" fx");
        assert!(e.st.finder.is_none());
        assert_eq!(e.text(), "abc");
        e.feed(" x");
        assert_eq!(e.text(), "abc");
    }

    #[test]
    fn finder_open_creates_and_focuses_file() {
        let mut e = env("x");
        e.vfs.write("notes.md", "hello notes");
        e.key(Key::Ctrl('p'));
        e.feed("notes");
        e.key(Key::Enter);
        assert!(e.st.finder.is_none());
        assert_eq!(e.st.buf().unwrap().name, "notes.md");
        assert_eq!(e.text(), "hello notes");
        // Create-on-open for a query with no match.
        e.key(Key::Ctrl('p'));
        e.feed("fresh.rs");
        e.key(Key::Enter);
        assert_eq!(e.st.buf().unwrap().name, "fresh.rs");
        assert_eq!(e.text(), "");
    }

    #[test]
    fn finder_swallows_all_keys() {
        let mut e = env("abc");
        e.key(Key::Ctrl('p'));
        e.feed("dd");
        assert_eq!(e.text(), "abc");
        assert_eq!(e.st.finder.as_ref().unwrap().query, "dd");
        e.key(Key::Ctrl('c'));
        assert!(e.st.finder.is_none());
    }

    // -- dashboard (no buffers) ---------------------------------------------------

    #[test]
    fn dashboard_ignores_motions_and_allows_commands() {
        let mut e = empty_env();
        e.feed("jkhlwbx$dGu.");
        assert!(e.st.buffers.is_empty());
        e.feed(":enew\n");
        assert_eq!(e.st.buffers.len(), 1);
        e.feed(":q\n");
        assert!(e.st.buffers.is_empty());
        e.feed(":help\n");
        assert_eq!(e.st.buf().unwrap().name, help::HELP_BUFFER);
        e.feed(":q!\n");
        e.feed(":e new.rs\n");
        assert_eq!(e.st.buf().unwrap().name, "new.rs");
    }

    #[test]
    fn dashboard_finder_shortcuts_work() {
        let mut e = empty_env();
        e.feed(" ff");
        assert!(e.st.finder.is_some());
        e.key(Key::Esc);
        e.key(Key::Ctrl('p'));
        assert!(e.st.finder.is_some());
    }

    // -- misc discipline ------------------------------------------------------------

    #[test]
    fn desired_col_survives_short_lines() {
        let mut e = env("longline\nab\nlongline");
        e.feed("$");
        assert_eq!(e.cursor(), (0, 7));
        e.feed("j");
        assert_eq!(e.cursor(), (1, 1));
        e.feed("j");
        assert_eq!(e.cursor(), (2, 7));
    }

    #[test]
    fn pending_display_shows_and_clears() {
        let mut e = env("abc");
        e.feed("2d");
        assert_eq!(e.vim.pending_display(), "2d");
        e.feed("d");
        assert_eq!(e.vim.pending_display(), "");
        let mut e = env("abc");
        e.feed("\"a3");
        assert_eq!(e.vim.pending_display(), "\"a3");
        e.key(Key::Esc);
        assert_eq!(e.vim.pending_display(), "");
    }

    #[test]
    fn cursor_never_past_line_end_in_normal() {
        let mut e = env("abc\nd");
        e.feed("$j");
        let (l, c) = e.cursor();
        assert_eq!(l, 1);
        assert_eq!(c, 0);
        e.feed("A!!\u{1b}");
        assert_eq!(e.cursor(), (1, 2));
    }

    #[test]
    fn home_end_pageup_delete_key_aliases() {
        let mut e = env("hello world\nsecond");
        e.key(Key::End);
        assert_eq!(e.cursor(), (0, 10));
        e.key(Key::Home);
        assert_eq!(e.cursor(), (0, 0));
        e.key(Key::Delete);
        assert_eq!(e.text(), "ello world\nsecond");
    }

    #[test]
    fn registers_command_lists_contents() {
        let mut e = env("word here");
        e.feed("yw:reg\n");
        assert!(e.status().contains("\"0 word"), "{}", e.status());
    }

    #[test]
    fn visual_tilde_and_join() {
        let mut e = env("abc def");
        e.feed("ve~");
        assert_eq!(e.text(), "ABC def");
        let mut e = env("a\nb\nc");
        e.feed("VjJ");
        assert_eq!(e.text(), "a b\nc");
    }
    // -- leader, windows, explorer routing -----------------------------------------

    #[test]
    fn leader_hints_per_node_and_cancel() {
        let mut e = env("x");
        assert!(e.vim.key_hints().is_none());
        e.feed(" ");
        let h = e.vim.key_hints().unwrap();
        assert_eq!(h.title, " space");
        assert!(h.entries.iter().any(|&(k, l)| k == "e" && l == "explorer"));
        assert!(h.entries.iter().any(|&(k, _)| k == "q"));
        e.feed("f");
        let h = e.vim.key_hints().unwrap();
        assert_eq!(h.title, " space f");
        assert!(h.entries.iter().any(|&(k, l)| k == "b" && l == "buffers"));
        e.key(Key::Esc);
        assert!(e.vim.key_hints().is_none());
        e.feed(" t");
        let h = e.vim.key_hints().unwrap();
        assert_eq!(h.title, " space t");
        assert_eq!(h.entries.len(), 3);
        e.feed("z");
        assert!(e.vim.key_hints().is_none());
        assert_eq!(e.text(), "x", "unmapped leader key cancels silently");
    }

    #[test]
    fn leader_e_toggles_explorer_with_focus() {
        let mut e = env("x");
        e.feed(" e");
        assert!(e.st.explorer.is_some());
        assert!(e.st.explorer_focused);
        // Keys now route to the explorer; q closes it.
        e.feed("q");
        assert!(e.st.explorer.is_none());
        assert!(!e.st.explorer_focused);
    }

    #[test]
    fn leader_e_refuses_when_too_narrow() {
        let mut e = env("x");
        e.st.text_dims = (25, 10);
        e.feed(" e");
        assert!(e.st.explorer.is_none());
        assert!(e.status().contains("not enough room"));
    }

    #[test]
    fn leader_o_toggles_explorer_focus_or_cycles_windows() {
        let mut e = env("x");
        e.feed(" e");
        assert!(e.st.explorer_focused);
        // Explorer consumes keys while focused; h returns focus to the editor.
        e.feed("h");
        assert!(!e.st.explorer_focused);
        e.feed(" o");
        assert!(e.st.explorer_focused);
        let mut e = env("x");
        e.feed(" tv");
        assert_eq!(e.st.windows.len(), 2);
        assert_eq!(e.st.active_win, 1);
        e.feed(" o");
        assert_eq!(e.st.active_win, 0);
    }

    #[test]
    fn explorer_keys_open_files_through_routing() {
        let mut e = env("x");
        e.vfs.write("aaa.rs", "AAA");
        e.feed(" e");
        assert!(e.st.explorer_focused);
        e.key(Key::Enter);
        assert_eq!(e.st.buf().unwrap().name, "aaa.rs");
        assert_eq!(e.text(), "AAA");
        assert!(!e.st.explorer_focused);
        assert!(e.st.explorer.is_some());
    }

    #[test]
    fn explorer_dd_delete_closes_clean_buffer() {
        let mut e = env("x");
        e.vfs.write("aaa.rs", "AAA");
        e.st.open_file(&e.vfs, "aaa.rs");
        e.feed(" e");
        e.feed("dd");
        assert!(!e.vfs.exists("aaa.rs"));
        assert!(e.st.buffers.iter().all(|b| b.name != "aaa.rs"));
    }

    #[test]
    fn explorer_dd_refuses_modified_buffer() {
        let mut e = env("x");
        e.vfs.write("aaa.rs", "AAA");
        e.st.open_file(&e.vfs, "aaa.rs");
        e.feed("ix\u{1b}");
        e.feed(" e");
        e.feed("dd");
        assert!(e.vfs.exists("aaa.rs"));
        assert!(e.status().contains("unsaved"));
    }

    #[test]
    fn leader_splits_and_close() {
        let mut e = env("x");
        e.feed(" th");
        assert_eq!(e.st.windows.len(), 2);
        assert_eq!(e.st.split_dir, SplitDir::Horizontal);
        e.feed(" tq");
        assert_eq!(e.st.windows.len(), 1);
        e.feed(" tv");
        assert_eq!(e.st.split_dir, SplitDir::Vertical);
        assert_eq!(e.st.windows.len(), 2);
        e.feed(" q");
        assert_eq!(e.st.windows.len(), 1, "space q closes the window first");
        assert_eq!(e.st.buffers.len(), 1);
        e.feed(" q");
        assert!(e.st.buffers.is_empty(), "space q on the last window closes the buffer");
    }

    #[test]
    fn leader_split_refused_when_too_small() {
        let mut e = env("x");
        e.st.text_dims = (15, 3);
        e.feed(" tv");
        assert_eq!(e.st.windows.len(), 1);
        assert!(e.status().contains("E36"));
        e.feed(" th");
        assert_eq!(e.st.windows.len(), 1);
        assert!(e.status().contains("E36"));
    }

    #[test]
    fn leader_w_c_h() {
        let mut e = env("hello");
        e.feed(" w");
        assert!(e.status().contains("written"));
        assert!(e.vfs.exists("main.rs"));
        e.feed(" h");
        assert_eq!(e.st.buf().unwrap().name, help::HELP_BUFFER);
        e.feed(" c");
        assert_eq!(e.st.buf().unwrap().name, "main.rs");
        assert_eq!(e.st.buffers.len(), 1);
        e.feed(".");
        assert_eq!(e.st.buffers.len(), 1, "leader chords are not dot-repeatable");
    }

    #[test]
    fn leader_fb_opens_buffer_picker_without_vfs_reads() {
        let mut e = env("unsaved content");
        e.st.buffers.push(Buffer::new("other.rs", "OTHER"));
        e.feed(" fb");
        let f = e.st.finder.as_ref().unwrap();
        assert_eq!(f.target, FinderTarget::Buffers);
        assert_eq!(f.results.len(), 2);
        e.feed("other");
        e.key(Key::Enter);
        assert!(e.st.finder.is_none());
        assert_eq!(e.st.buf().unwrap().name, "other.rs");
        assert_eq!(e.text(), "OTHER", "buffer content untouched by the vfs");
        assert_eq!(e.st.buffers.len(), 2, "no new buffer created");
        assert!(!e.vfs.exists("other.rs"), "picker never touched the vfs");
    }

    #[test]
    fn buffer_picker_enter_with_no_match_closes() {
        let mut e = env("x");
        e.feed(" fb");
        e.feed("nomatch");
        e.key(Key::Enter);
        assert!(e.st.finder.is_none());
        assert_eq!(e.st.buffers.len(), 1);
        assert_eq!(e.st.buf().unwrap().name, "main.rs");
    }

    #[test]
    fn ctrl_w_chords() {
        let mut e = env("x");
        e.key(Key::Ctrl('w'));
        e.feed("s");
        assert_eq!(e.st.windows.len(), 2);
        assert_eq!(e.st.split_dir, SplitDir::Horizontal);
        e.key(Key::Ctrl('w'));
        e.feed("w");
        assert_eq!(e.st.active_win, 0);
        e.key(Key::Ctrl('w'));
        e.feed("j");
        assert_eq!(e.st.active_win, 1);
        e.key(Key::Ctrl('w'));
        e.feed("k");
        assert_eq!(e.st.active_win, 0);
        e.key(Key::Ctrl('w'));
        e.feed("q");
        assert_eq!(e.st.windows.len(), 1);
        e.key(Key::Ctrl('w'));
        e.feed("v");
        assert_eq!(e.st.split_dir, SplitDir::Vertical);
        e.key(Key::Ctrl('w'));
        e.feed("c");
        assert_eq!(e.st.windows.len(), 1);
        e.key(Key::Ctrl('w'));
        e.feed("x");
        assert_eq!(e.text(), "x", "unmapped ctrl-w key is dropped");
    }

    #[test]
    fn split_ex_commands() {
        let mut e = env("x");
        e.feed(":sp\n");
        assert_eq!(e.st.windows.len(), 2);
        e.feed(":close\n");
        assert_eq!(e.st.windows.len(), 1);
        e.feed(":close\n");
        assert!(e.status().contains("E444"));
        e.feed(":vsplit\n");
        e.feed(":split\n");
        assert_eq!(e.st.windows.len(), 3);
        e.feed(":only\n");
        assert_eq!(e.st.windows.len(), 1);
        e.feed(":vs\n");
        assert_eq!(e.st.windows.len(), 2);
        e.feed(":q\n");
        assert_eq!(e.st.windows.len(), 1);
        assert_eq!(e.st.buffers.len(), 1, ":q closes the window, not the buffer");
        e.feed(":q\n");
        assert!(e.st.buffers.is_empty());
    }

    #[test]
    fn q_bang_still_forces_on_last_window() {
        let mut e = env("x");
        e.feed("ichanged\u{1b}");
        e.feed(":q\n");
        assert_eq!(e.st.buffers.len(), 1);
        assert!(e.status().contains("E89"));
        e.feed(":q!\n");
        assert!(e.st.buffers.is_empty());
    }

    #[test]
    fn dashboard_leader_chords_work() {
        let mut e = empty_env();
        e.feed(" fb");
        assert!(e.st.finder.is_some());
        assert_eq!(e.st.finder.as_ref().unwrap().target, FinderTarget::Buffers);
        e.key(Key::Esc);
        e.feed(" h");
        assert_eq!(e.st.buf().unwrap().name, help::HELP_BUFFER);
        e.feed(":bd\n");
        e.feed(" e");
        assert!(e.st.explorer.is_some());
        e.feed("q");
        assert!(e.st.explorer.is_none());
    }

    #[test]
    fn explorer_routing_skips_cmdline_and_finder() {
        let mut e = env("x");
        e.feed(" e");
        assert!(e.st.explorer_focused);
        // The finder takes precedence over explorer routing.
        e.st.finder = Some(FinderState::new(vec!["main.rs".into()]));
        e.feed("j");
        assert_eq!(e.st.finder.as_ref().unwrap().query, "j");
        e.key(Key::Esc);
        assert!(e.st.finder.is_none());
        assert!(e.st.explorer_focused, "esc went to the finder, not the explorer");
    }

    #[test]
    fn visual_linewise_gc_steps() {
        let mut e = env("a();\n    b();");
        e.feed("V");
        assert_eq!(e.vim.mode(), Mode::Visual { kind: VisualKind::Line }, "V enters V-LINE");
        e.feed("j");
        assert_eq!(e.cursor(), (1, 0), "j moves down in visual");
        e.feed("g");
        e.feed("c");
        assert_eq!(e.text(), "// a();\n    // b();");
    }

    // -- visual block ------------------------------------------------------------------

    #[test]
    fn block_delete_removes_rectangle() {
        let mut e = env("abcd\nefgh\nijkl");
        e.feed("l");
        e.key(Key::Ctrl('v'));
        assert_eq!(e.vim.mode(), Mode::Visual { kind: VisualKind::Block });
        e.feed("jjl");
        e.feed("d");
        assert_eq!(e.text(), "ad\neh\nil");
        assert_eq!(e.cursor(), (0, 1));
        let r = e.reg('"').unwrap();
        assert_eq!(r.kind, RegKind::Block);
        assert_eq!(r.text, "bc\nfg\njk");
    }

    #[test]
    fn block_delete_skips_short_lines() {
        let mut e = env("abcd\ne\nijkl");
        e.set_cursor(0, 1);
        e.key(Key::Ctrl('v'));
        e.feed("jjld");
        assert_eq!(e.text(), "ad\ne\nil");
        assert_eq!(e.reg('"').unwrap().text, "bc\n\njk");
    }

    #[test]
    fn block_yank_then_paste_is_rectangular() {
        let mut e = env("abcd\nefgh\nijkl");
        e.feed("l");
        e.key(Key::Ctrl('v'));
        e.feed("jly");
        assert_eq!(e.text(), "abcd\nefgh\nijkl", "yank leaves text untouched");
        assert_eq!(e.cursor(), (0, 1), "cursor jumps to the block's top-left");
        assert_eq!(e.reg('"').unwrap().kind, RegKind::Block);
        e.feed("$p");
        assert_eq!(e.text(), "abcdbc\nefghfg\nijkl");
    }

    #[test]
    fn block_paste_pads_and_extends_lines() {
        let mut e = env("12\n34");
        e.key(Key::Ctrl('v'));
        e.feed("jly");
        e.feed("j$p");
        assert_eq!(e.text(), "12\n3412\n  34", "short and missing lines pad to the paste col");
    }

    #[test]
    fn block_insert_replicates_on_all_lines() {
        let mut e = env("one\ntwo\nthree");
        e.key(Key::Ctrl('v'));
        e.feed("jjI");
        e.feed("# ");
        e.key(Key::Esc);
        assert_eq!(e.text(), "# one\n# two\n# three");
        e.feed("u");
        assert_eq!(e.text(), "one\ntwo\nthree", "the whole block insert is one undo step");
    }

    #[test]
    fn block_insert_skips_lines_shorter_than_left_edge() {
        let mut e = env("alpha\nx\ngamma");
        e.set_cursor(0, 2);
        e.key(Key::Ctrl('v'));
        e.feed("jjI");
        e.feed("_");
        e.key(Key::Esc);
        assert_eq!(e.text(), "al_pha\nx\nga_mma");
    }

    #[test]
    fn block_append_pads_short_lines() {
        let mut e = env("aa\nb\ncccc");
        e.key(Key::Ctrl('v'));
        e.feed("jjlA");
        e.feed("!");
        e.key(Key::Esc);
        assert_eq!(e.text(), "aa!\nb !\ncc!cc");
    }

    #[test]
    fn block_dollar_append_lands_at_each_eol() {
        let mut e = env("short\nlonger line\nmid");
        e.key(Key::Ctrl('v'));
        e.feed("jj$A");
        e.feed(";");
        e.key(Key::Esc);
        assert_eq!(e.text(), "short;\nlonger line;\nmid;");
    }

    #[test]
    fn block_change_replicates_replacement() {
        let mut e = env("foo_a\nfoo_b\nfoo_c");
        e.key(Key::Ctrl('v'));
        e.feed("jjllc");
        e.feed("bar");
        e.key(Key::Esc);
        assert_eq!(e.text(), "bar_a\nbar_b\nbar_c");
        e.feed("u");
        assert_eq!(e.text(), "foo_a\nfoo_b\nfoo_c");
    }

    #[test]
    fn block_replace_fills_rectangle() {
        let mut e = env("abcd\nefgh\nijkl");
        e.feed("l");
        e.key(Key::Ctrl('v'));
        e.feed("jjlrx");
        assert_eq!(e.text(), "axxd\nexxh\nixxl");
        assert_eq!(e.cursor(), (0, 1));
    }

    #[test]
    fn block_d_and_c_run_to_line_ends() {
        let mut e = env("aa11\nbb22\ncc33");
        e.set_cursor(0, 2);
        e.key(Key::Ctrl('v'));
        e.feed("jjD");
        assert_eq!(e.text(), "aa\nbb\ncc");

        let mut e = env("xx11\nyy22");
        e.set_cursor(0, 2);
        e.key(Key::Ctrl('v'));
        e.feed("jC");
        e.feed("Z");
        e.key(Key::Esc);
        assert_eq!(e.text(), "xxZ\nyyZ");
    }

    #[test]
    fn block_corner_swap_and_case_ops() {
        let mut e = env("abcd\nefgh");
        e.feed("l");
        e.key(Key::Ctrl('v'));
        e.feed("jl");
        e.feed("O");
        assert_eq!(e.cursor(), (1, 1), "O swaps the horizontal corners");
        e.feed("U");
        assert_eq!(e.text(), "aBCd\neFGh");
    }

    #[test]
    fn dollar_motion_sticks_to_line_ends() {
        let mut e = env("long line\nab\nlonger line!");
        e.feed("$");
        assert_eq!(e.cursor(), (0, 8));
        e.feed("j");
        assert_eq!(e.cursor(), (1, 1), "j after $ tracks the shorter line's end");
        e.feed("j");
        assert_eq!(e.cursor(), (2, 11), "and springs back out on longer lines");
        e.feed("h");
        e.feed("j");
        assert_eq!(e.cursor(), (2, 10), "h breaks the $ pin");
    }

    #[test]
    fn escape_leaves_block_mode() {
        let mut e = env("abc\ndef");
        e.key(Key::Ctrl('v'));
        e.feed("j");
        e.key(Key::Esc);
        assert_eq!(e.vim.mode(), Mode::Normal);
        e.key(Key::Ctrl('v'));
        e.key(Key::Ctrl('v'));
        assert_eq!(e.vim.mode(), Mode::Normal, "Ctrl+v toggles back out");
    }

    // -- macros ------------------------------------------------------------------------

    #[test]
    fn macro_records_and_replays() {
        let mut e = env("one\ntwo\nthree");
        e.feed("qaA;");
        e.key(Key::Esc);
        e.feed("jq");
        assert_eq!(e.text(), "one;\ntwo\nthree");
        assert_eq!(e.vim.recording_reg(), None);
        assert!(e.status().contains("recorded @a"), "status: {}", e.status());
        e.feed("@a");
        assert_eq!(e.text(), "one;\ntwo;\nthree", "@a replays the append+move");
        e.feed("@@");
        assert_eq!(e.text(), "one;\ntwo;\nthree;", "@@ repeats the last macro");
    }

    #[test]
    fn macro_takes_a_count() {
        let mut e = env("a\nb\nc\nd");
        e.feed("qqxjq");
        assert_eq!(e.text(), "\nb\nc\nd");
        e.feed("3@q");
        assert_eq!(e.text(), "\n\n\n");
    }

    #[test]
    fn macro_uppercase_appends() {
        let mut e = env("abcdef");
        e.feed("qaxq");
        e.feed("qAxq");
        e.feed("@a");
        assert_eq!(e.text(), "ef", "two x recorded across sessions, both replayed");
    }

    #[test]
    fn recording_indicator_tracks_q() {
        let mut e = env("x");
        e.feed("qb");
        assert_eq!(e.vim.recording_reg(), Some('b'));
        e.feed("q");
        assert_eq!(e.vim.recording_reg(), None);
        assert_eq!(e.vim.mode(), Mode::Normal);
    }

    #[test]
    fn empty_or_unknown_macro_reports_error() {
        let mut e = env("x");
        e.feed("@z");
        assert!(e.status().contains("E749"), "status: {}", e.status());
    }

    #[test]
    fn recursive_macro_is_bounded() {
        let mut e = env(&"y\n".repeat(64));
        // @a deletes a char then calls itself: must stop at the budget, not hang.
        e.feed("qaxj@aq");
        e.feed("@a");
        assert_eq!(e.vim.mode(), Mode::Normal);
    }

    #[test]
    fn macro_survives_insert_and_visual_keys() {
        let mut e = env("abc abc\nabc abc");
        e.feed("qwviwc123");
        e.key(Key::Esc);
        e.feed("q");
        assert_eq!(e.text(), "123 abc\nabc abc");
        e.feed("j0@w");
        assert_eq!(e.text(), "123 abc\n123 abc");
    }

    // -- which-key ---------------------------------------------------------------------

    #[test]
    fn key_hints_cover_every_pending_state() {
        // (keys to press, expected panel title, shows without a pause)
        let cases: &[(&[Key], &str, bool)] = &[
            (&[Key::Char(' ')], " space", true),
            (&[Key::Char(' '), Key::Char('f')], " space f", true),
            (&[Key::Char(' '), Key::Char('t')], " space t", true),
            (&[Key::Char('g')], " g", false),
            (&[Key::Char('z')], " z", false),
            (&[Key::Ctrl('w')], " ctrl+w", true),
            (&[Key::Char('"')], " \"", false),
            (&[Key::Char('m')], " m", false),
            (&[Key::Char('`')], " `", false),
            (&[Key::Char('\'')], " '", false),
            (&[Key::Char('f')], " f", false),
            (&[Key::Char('T')], " T", false),
            (&[Key::Char('r')], " r", false),
            (&[Key::Char('q')], " q", false),
            (&[Key::Char('@')], " @", false),
            (&[Key::Char('d')], " d", false),
            (&[Key::Char('c')], " c", false),
            (&[Key::Char('y')], " y", false),
            (&[Key::Char('>')], " >", false),
            (&[Key::Char('g'), Key::Char('u')], " gu", false),
            (&[Key::Char('g'), Key::Char('c')], " gc", false),
            (&[Key::Char('d'), Key::Char('i')], " i", false),
            (&[Key::Char('d'), Key::Char('a')], " a", false),
        ];
        for (keys, title, immediate) in cases {
            let mut e = env("word (one) \"two\"");
            for &k in *keys {
                e.key(k);
            }
            let h = e.vim.key_hints().unwrap_or_else(|| panic!("no hints for {title:?}"));
            assert_eq!(h.title, *title);
            assert_eq!(h.immediate, *immediate, "immediate flag for {title:?}");
            assert!(!h.entries.is_empty(), "{title:?} panel has entries");
        }
        // No pending state — no panel.
        let e = env("x");
        assert!(e.vim.key_hints().is_none());
        // Visual-mode object pending shows the panel too.
        let mut e = env("word");
        e.feed("vi");
        assert_eq!(e.vim.key_hints().unwrap().title, " i");
    }

    #[test]
    fn key_hints_menu_entries_match_real_bindings() {
        // Every root-leader hint key actually does something (state visibly changes).
        let mut e = env("text");
        e.feed(" e");
        assert!(e.st.explorer.is_some(), "space e opens the explorer");
        assert!(e.st.explorer_focused, "opening the explorer focuses it");
        e.key(Key::Esc);
        assert!(!e.st.explorer_focused, "esc returns to the editor");
        e.feed(" o");
        assert!(e.st.explorer_focused, "space o refocuses the explorer");
        e.feed("q");
        assert!(e.st.explorer.is_none(), "q hides the explorer");
        e.feed(" th");
        assert_eq!(e.st.windows.len(), 2, "space t h splits below");
        assert_eq!(e.st.split_dir, SplitDir::Horizontal);
        e.feed(" tv");
        assert_eq!(e.st.windows.len(), 3, "space t v splits right");
        assert_eq!(e.st.split_dir, SplitDir::Vertical);
        e.feed(" tq");
        assert_eq!(e.st.windows.len(), 2, "space t q closes the window");
        e.feed(" c");
        assert!(e.st.buffers.is_empty(), "space c closes the buffer");
    }

    #[test]
    fn huge_count_paste_is_capped() {
        let mut e = env("line one\nline two");
        e.feed("yy99999999999999999999p");
        let lines = e.st.buf().unwrap().text.line_count();
        assert!(lines <= 2 + PASTE_MAX_BYTES / 8, "paste stays within budget: {lines}");
        assert!(lines > 2, "one copy still pastes");
    }
}
