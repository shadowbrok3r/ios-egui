//! Temporary review probe: randomized key/render fuzzing. NOT for commit.

use egui_ios_plugin_sdk::HostHandle;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use crate::buffer::Position;
use crate::finder::FinderState;
use crate::fs::Vfs;
use crate::highlight::Highlighter;
use crate::state::{Buffer, EditorState, SplitDir};
use crate::ui::{self, DrawCtx};
use crate::vim::{Key, VimEngine};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
        xs[(self.next() as usize) % xs.len()]
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() as usize) % n.max(1)
    }
}

const CHARS: &[char] = &[
    'a', 'b', 'x', 'j', 'k', 'h', 'l', 'w', 'b', 'e', 'd', 'c', 'y', 'p', 'P', 'u', 'r', 's',
    'S', 'C', 'D', 'x', 'X', 'o', 'O', 'i', 'a', 'A', 'I', 'v', 'V', 'g', 'G', 'z', 'f', 'F',
    't', 'T', ';', ',', '0', '1', '2', '9', '$', '^', '{', '}', '%', '~', 'J', 'n', 'N', '*',
    '#', 'm', '`', '\'', '"', '.', ':', '/', '?', ' ', '(', ')', '[', ']', '<', '>', '中', 'é',
    '🎉', 'ß', '\u{200d}', 'q', 'w', 'W', 'E', 'B', '@',
];

fn random_key(r: &mut Rng) -> Key {
    match r.below(12) {
        0 => Key::Esc,
        1 => Key::Enter,
        2 => Key::Backspace,
        3 => Key::Delete,
        4 => Key::Tab,
        5 => Key::Up,
        6 => Key::Down,
        7 => Key::Left,
        8 => Key::Right,
        9 => Key::Home,
        10 => Key::Ctrl(r.pick(&['d', 'u', 'f', 'b', 'w', 'r', 'p', 'c', 'v'])),
        _ => Key::Char(r.pick(CHARS)),
    }
}

const TEXTS: &[&str] = &[
    "",
    "\n\n\n",
    "héllo wörld\n中文字テスト🎉\nfn main() { let x = \"str\"; }\n    indented\n",
    "a",
    "中",
    "one two three\nfour five\n\nsix\n(bracket [nested] {deep})\n\"quoted 'str'\"",
];

#[test]
fn fuzz_engine_and_render_never_panic() {
    for seed in 0..24u64 {
        let mut r = Rng(seed.wrapping_mul(0x9E3779B97F4A7C15).wrapping_add(7));
        let mut st = EditorState::new();
        let mut vfs = Vfs::load();
        for (i, name) in ["main.rs", "中文ファイル.md", "🎉.rs", "héllo.rs"].iter().enumerate() {
            vfs.write(name, TEXTS[i % TEXTS.len()]);
        }
        st.buffers.push(Buffer::new("main.rs", TEXTS[(seed as usize) % TEXTS.len()]));
        st.buffers.push(Buffer::new("🎉.rs", "中文 emoji 🎉 line\nsecond"));
        let mut vim = VimEngine::new();
        let mut hl = Highlighter::new();
        let host = HostHandle;
        let sizes = [(20u16, 6u16), (30, 8), (41, 12), (80, 24), (200, 10), (400, 300)];

        for step in 0..4000 {
            let key = random_key(&mut r);
            vim.handle_key(&mut st, &mut vfs, &host, key);

            if step % 7 == 0 {
                // Hostile external state: wild scroll (drag), buffer/window churn.
                if let Some(b) = st.buf_mut() {
                    b.scroll = (r.below(10_000), r.below(10_000));
                }
                if r.below(20) == 0 {
                    st.split(if r.below(2) == 0 { SplitDir::Horizontal } else { SplitDir::Vertical });
                }
                if r.below(25) == 0 {
                    st.close_buffer(r.below(st.buffers.len().max(1)), true);
                }
                if r.below(30) == 0 {
                    st.finder = Some(FinderState::new(vfs.list()));
                }
                if r.below(30) == 0 {
                    // External file removal while explorer/finder state persists.
                    let names = vfs.list();
                    if !names.is_empty() {
                        let n = names[r.below(names.len())].clone();
                        vfs.remove(&n);
                    }
                }
                if r.below(15) == 0 {
                    st.active_win = r.below(st.windows.len().max(1));
                }
                // Simulate a tap landing anywhere: raw cursor set then clamp path via draw.
                if let Some(b) = st.buf_mut() {
                    if r.below(10) == 0 {
                        b.cursor = Position::new(r.below(5000), r.below(5000));
                        b.cursor = b.text.clamp(b.cursor, false);
                    }
                }
            }

            if step % 13 == 0 {
                let (w, h) = sizes[r.below(sizes.len())];
                let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
                let _ = ui::draw(
                    &mut term,
                    DrawCtx {
                        st: &mut st,
                        vim: &vim,
                        hl: &mut hl,
                        vfs: &vfs,
                        blink_on: true,
                        ctrl_armed: false,
                        focused: true,
                        paused: step % 2 == 0,
                    },
                );
            }
        }
    }
}

#[test]
fn probe_session_hostile_bytes() {
    // Random bytes into restore path must never panic.
    let mut r = Rng(42);
    for _ in 0..2000 {
        let len = r.below(64);
        let bytes: Vec<u8> = (0..len).map(|_| r.next() as u8).collect();
        let _ = postcard::from_bytes::<crate::Session>(&bytes).map(|s| {
            let mut st = EditorState::new();
            crate::apply_session(&mut st, s);
            st
        });
    }
}

#[test]
fn probe_cmdline_fuzz() {
    let cmds = [
        ":%s/中/x/g", ":1,999999s/e/é/g", ":s/a//g", ":999999", ":$", ":.,$s/x/🎉/",
        ":0", ":e 中.rs", ":b 99", ":b", ":bd!", ":bd", ":sp", ":vs", ":only", ":close",
        ":wq", ":enew", ":set rnu", ":rm main.rs", ":w 新.rs", ":help", ":q!", ":q",
        ":5,2s/l/L/g", ":%s/\u{200d}/z/g", ":s/é/中中中/g",
    ];
    let mut r = Rng(7);
    for round in 0..200 {
        let mut st = EditorState::new();
        let mut vfs = Vfs::load();
        vfs.write("main.rs", TEXTS[round % TEXTS.len()]);
        st.buffers.push(Buffer::new("main.rs", TEXTS[(round + 1) % TEXTS.len()]));
        let mut vim = VimEngine::new();
        let host = HostHandle;
        for _ in 0..30 {
            let cmd = cmds[r.below(cmds.len())];
            for ch in cmd.chars() {
                vim.handle_key(&mut st, &mut vfs, &host, Key::Char(ch));
            }
            vim.handle_key(&mut st, &mut vfs, &host, Key::Enter);
            // A couple of normal keys between commands.
            for _ in 0..r.below(6) {
                vim.handle_key(&mut st, &mut vfs, &host, random_key(&mut r));
            }
        }
    }
}

#[test]
fn probe_visual_stale_anchor_across_tap_switch() {
    // Tap path: switch active window/buffer while visual mode persists.
    let host = HostHandle;
    let mut st = EditorState::new();
    let mut vfs = Vfs::load();
    let long: String = (0..500).map(|i| format!("line {i} 中文\n")).collect();
    st.buffers.push(Buffer::new("big.rs", &long));
    st.buffers.push(Buffer::new("small.rs", "ab"));
    let mut vim = VimEngine::new();
    for op in ['d', 'y', 'c', '>', '<', 'r', 'J', '~', 'U'] {
        let mut vim2 = VimEngine::new();
        st.buffers[0] = Buffer::new("big.rs", &long);
        st.buffers[1] = Buffer::new("small.rs", "ab");
        st.set_active(0);
        if let Some(b) = st.buf_mut() {
            b.cursor = Position::new(450, 5);
        }
        vim2.handle_key(&mut st, &mut vfs, &host, Key::Char('V'));
        vim2.handle_key(&mut st, &mut vfs, &host, Key::Char('j'));
        // Simulate lib.rs handle_tap: focus another window/buffer, cursor placed there.
        st.set_active(1);
        if let Some(b) = st.buf_mut() {
            b.cursor = b.text.clamp(Position::new(0, 1), false);
        }
        vim2.handle_key(&mut st, &mut vfs, &host, Key::Char(op));
        if op == 'r' {
            vim2.handle_key(&mut st, &mut vfs, &host, Key::Char('z'));
        }
        vim2.handle_key(&mut st, &mut vfs, &host, Key::Esc);
    }
    let _ = &mut vim;
}

#[test]
fn probe_explorer_finder_churn() {
    use crate::explorer::ExplorerState;
    let host = HostHandle;
    let mut r = Rng(99);
    for seed in 0..50 {
        let mut st = EditorState::new();
        let mut vfs = Vfs::load();
        for i in 0..8 {
            vfs.write(&format!("f{i}中.rs"), "x");
        }
        st.buffers.push(Buffer::new("f0中.rs", "x"));
        st.explorer = Some(ExplorerState::new());
        st.explorer_focused = true;
        let mut vim = VimEngine::new();
        let mut hl = Highlighter::new();
        for step in 0..400 {
            vim.handle_key(&mut st, &mut vfs, &host, random_key(&mut r));
            if step % 5 == 0 {
                let names = vfs.list();
                if !names.is_empty() && r.below(3) == 0 {
                    vfs.remove(&names[r.below(names.len())].clone());
                }
            }
            if step % 11 == 0 {
                let mut term = Terminal::new(TestBackend::new(45, 12)).unwrap();
                let _ = ui::draw(&mut term, DrawCtx {
                    st: &mut st, vim: &vim, hl: &mut hl, vfs: &vfs,
                    blink_on: true, ctrl_armed: false, focused: true, paused: step % 3 == 0,
                });
            }
        }
        let _ = seed;
    }
}

#[test]
fn probe_huge_count_overflow() {
    let host = HostHandle;
    for keys in ["yy99999999999999999999p"] {
        let mut st = EditorState::new();
        let mut vfs = Vfs::load();
        st.buffers.push(Buffer::new("main.rs", "line one\nline two\nline three"));
        let mut vim = VimEngine::new();
        for c in keys.chars() {
            vim.handle_key(&mut st, &mut vfs, &host, Key::Char(c));
        }
        let b = st.buf().unwrap();
        assert!(b.cursor.line < 3, "{keys}: cursor line {}", b.cursor.line);
    }
}
