//! Shared in-app log. Engine workers and the UI push lines; the Logs tab renders them, and every
//! line mirrors to logcat under target "comfyui" (`adb logcat -s comfyui`).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub const MAX_LINES: usize = 2000;
/// logcat drops messages past ~4KB; longer lines are chunked with a [i/n] prefix.
const LOGCAT_CHUNK: usize = 3800;

#[derive(Clone, Copy, PartialEq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

#[derive(Clone)]
pub struct Line {
    pub seq: u64,
    /// Seconds since app launch.
    pub secs: f32,
    pub level: Level,
    pub text: String,
}

struct Inner {
    start: Instant,
    next_seq: u64,
    lines: VecDeque<Line>,
}

#[derive(Clone)]
pub struct Logger(Arc<Mutex<Inner>>);

impl Logger {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(Inner {
            start: Instant::now(),
            next_seq: 0,
            lines: VecDeque::new(),
        })))
    }

    pub fn info(&self, text: impl Into<String>) {
        self.push(Level::Info, text.into());
    }

    pub fn warn(&self, text: impl Into<String>) {
        self.push(Level::Warn, text.into());
    }

    pub fn error(&self, text: impl Into<String>) {
        self.push(Level::Error, text.into());
    }

    fn push(&self, level: Level, text: String) {
        mirror_to_logcat(level, &text);
        let Ok(mut g) = self.0.lock() else { return };
        let line = Line {
            seq: g.next_seq,
            secs: g.start.elapsed().as_secs_f32(),
            level,
            text,
        };
        g.next_seq += 1;
        g.lines.push_back(line);
        while g.lines.len() > MAX_LINES {
            g.lines.pop_front();
        }
    }

    /// Lines newer than `cursor`, advancing it.
    pub fn take_new(&self, cursor: &mut u64) -> Vec<Line> {
        let Ok(g) = self.0.lock() else { return Vec::new() };
        let new = g.lines.iter().filter(|l| l.seq >= *cursor).cloned().collect();
        *cursor = g.next_seq;
        new
    }

    pub fn clear(&self) {
        if let Ok(mut g) = self.0.lock() {
            g.lines.clear();
        }
    }
}

fn mirror_to_logcat(level: Level, text: &str) {
    let lvl = match level {
        Level::Info => log::Level::Info,
        Level::Warn => log::Level::Warn,
        Level::Error => log::Level::Error,
    };
    if text.len() <= LOGCAT_CHUNK {
        log::log!(target: "comfyui", lvl, "{text}");
        return;
    }
    let total = text.len().div_ceil(LOGCAT_CHUNK);
    let (mut i, mut part) = (0, 1);
    while i < text.len() {
        let mut end = (i + LOGCAT_CHUNK).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        log::log!(target: "comfyui", lvl, "[{part}/{total}] {}", &text[i..end]);
        i = end;
        part += 1;
    }
}
