//! Hand-rolled Rust lexer producing per-line color spans, cached by buffer version.
//! Block comments and raw strings carry state across lines.

use ratatui::style::Color;

use crate::buffer::TextBuffer;

/// A run of chars `[start, start+len)` (char indices) in one line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HlSpan {
    pub start: usize,
    pub len: usize,
    pub color: Color,
}

pub struct Highlighter {
    cache_key: Option<(String, u64)>,
    lines: Vec<Vec<HlSpan>>,
}

impl Highlighter {
    pub fn new() -> Self {
        Highlighter { cache_key: None, lines: Vec::new() }
    }

    /// Spans per line for `buf`, relexed only when (name, version) changed.
    /// Non-`.rs` names get no highlighting (empty span lists).
    pub fn spans(&mut self, name: &str, buf: &TextBuffer) -> &[Vec<HlSpan>] {
        let key = (name.to_string(), buf.version());
        if self.cache_key.as_ref() != Some(&key) {
            self.lines = if name.ends_with(".rs") {
                lex_rust(buf.lines())
            } else {
                vec![Vec::new(); buf.line_count()]
            };
            self.cache_key = Some(key);
        }
        &self.lines
    }
}

/// Lex all lines of a Rust source file into color spans.
pub fn lex_rust(lines: &[String]) -> Vec<Vec<HlSpan>> {
    // STUB: implemented by the highlight module owner.
    lines.iter().map(|_| Vec::new()).collect()
}
