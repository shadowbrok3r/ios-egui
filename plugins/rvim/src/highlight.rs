//! Hand-rolled Rust lexer producing per-line color spans, cached by buffer version.
//! Block comments and raw strings carry state across lines.

use ratatui::style::Color;

use crate::buffer::TextBuffer;
use crate::theme;

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
        let version = buf.version();
        let fresh = self.cache_key.as_ref().is_some_and(|(n, v)| n == name && *v == version);
        if !fresh {
            self.lines = if name.ends_with(".rs") {
                lex_rust(buf.lines())
            } else {
                vec![Vec::new(); buf.line_count()]
            };
            self.cache_key = Some((name.to_string(), version));
        }
        &self.lines
    }
}

/// Lexer state carried from the end of one line into the next.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Carry {
    Normal,
    /// Inside a block comment at this nesting depth.
    BlockComment(usize),
    /// Inside a raw string closed by `"` plus this many `#`.
    RawString(usize),
    /// Inside a normal string continued by a trailing backslash.
    Str,
}

/// Lex all lines of a Rust source file into color spans (sorted, non-overlapping).
pub fn lex_rust(lines: &[String]) -> Vec<Vec<HlSpan>> {
    let mut out = Vec::with_capacity(lines.len());
    let mut carry = Carry::Normal;
    let mut after_fn = false;
    for line in lines {
        let chars: Vec<char> = line.chars().collect();
        let mut lx = Lexer { chars: &chars, i: 0, spans: Vec::new(), after_fn };
        carry = lx.run(carry);
        after_fn = lx.after_fn;
        out.push(lx.spans);
    }
    out
}

fn is_keyword(w: &str) -> bool {
    matches!(
        w,
        "as" | "async"
            | "await"
            | "break"
            | "const"
            | "continue"
            | "crate"
            | "dyn"
            | "else"
            | "enum"
            | "extern"
            | "fn"
            | "for"
            | "if"
            | "impl"
            | "in"
            | "let"
            | "loop"
            | "match"
            | "mod"
            | "move"
            | "mut"
            | "pub"
            | "ref"
            | "return"
            | "static"
            | "struct"
            | "super"
            | "trait"
            | "type"
            | "unsafe"
            | "use"
            | "where"
            | "while"
    )
}

fn is_primitive(w: &str) -> bool {
    matches!(
        w,
        "bool" | "char"
            | "str"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "u128"
            | "usize"
            | "i8"
            | "i16"
            | "i32"
            | "i64"
            | "i128"
            | "isize"
            | "f32"
            | "f64"
    )
}

fn is_operator(c: char) -> bool {
    matches!(
        c,
        '+' | '-' | '*' | '/' | '%' | '=' | '!' | '<' | '>' | '&' | '|' | '^' | '~' | '?' | '@'
    )
}

struct Lexer<'a> {
    chars: &'a [char],
    i: usize,
    spans: Vec<HlSpan>,
    /// The previous significant token was the `fn` keyword.
    after_fn: bool,
}

impl Lexer<'_> {
    fn len(&self) -> usize {
        self.chars.len()
    }

    fn at(&self, k: usize) -> Option<char> {
        self.chars.get(self.i + k).copied()
    }

    /// Append `[start, end)` with `color`, merging into a contiguous same-color span.
    fn push(&mut self, start: usize, end: usize, color: Color) {
        if end <= start {
            return;
        }
        if let Some(last) = self.spans.last_mut() {
            if last.color == color && last.start + last.len == start {
                last.len = end - last.start;
                return;
            }
        }
        self.spans.push(HlSpan { start, len: end - start, color });
    }

    /// Lex one line starting in `carry`; returns the state at end of line.
    fn run(&mut self, carry: Carry) -> Carry {
        match carry {
            Carry::BlockComment(depth) => {
                if let Some(next) = self.block_comment(depth) {
                    return next;
                }
            }
            Carry::RawString(hashes) => {
                if let Some(next) = self.raw_string(hashes) {
                    return next;
                }
            }
            Carry::Str => {
                if let Some(next) = self.string_body() {
                    return next;
                }
            }
            Carry::Normal => {}
        }
        while self.i < self.len() {
            let c = self.chars[self.i];
            if c.is_whitespace() {
                self.i += 1;
                continue;
            }
            if c == '/' && self.at(1) == Some('/') {
                self.push(self.i, self.len(), theme::SYN_COMMENT);
                self.i = self.len();
                break;
            }
            if c == '/' && self.at(1) == Some('*') {
                self.push(self.i, self.i + 2, theme::SYN_COMMENT);
                self.i += 2;
                if let Some(next) = self.block_comment(1) {
                    return next;
                }
                continue;
            }
            if c == '"' {
                self.after_fn = false;
                self.push(self.i, self.i + 1, theme::SYN_STRING);
                self.i += 1;
                if let Some(next) = self.string_body() {
                    return next;
                }
                continue;
            }
            if c == '\'' {
                self.after_fn = false;
                self.quote();
                continue;
            }
            if c == '#' && (self.at(1) == Some('[') || (self.at(1) == Some('!') && self.at(2) == Some('['))) {
                self.after_fn = false;
                self.attribute();
                continue;
            }
            if c.is_ascii_digit() {
                self.after_fn = false;
                self.number();
                continue;
            }
            if c == '_' || c.is_alphabetic() {
                if let Some(ended) = self.try_prefixed_literal() {
                    self.after_fn = false;
                    if let Some(next) = ended {
                        return next;
                    }
                    continue;
                }
                self.ident();
                continue;
            }
            if is_operator(c) {
                self.after_fn = false;
                let start = self.i;
                while self.i < self.len() {
                    let ch = self.chars[self.i];
                    if !is_operator(ch) {
                        break;
                    }
                    if ch == '/' && matches!(self.at(1), Some('/') | Some('*')) {
                        break;
                    }
                    self.i += 1;
                }
                self.push(start, self.i, theme::SYN_OPERATOR);
                continue;
            }
            self.after_fn = false;
            self.i += 1;
        }
        Carry::Normal
    }

    /// Scan inside a block comment at `depth`; Some means the line ended inside it.
    fn block_comment(&mut self, mut depth: usize) -> Option<Carry> {
        let start = self.i;
        while self.i < self.len() {
            if self.at(0) == Some('/') && self.at(1) == Some('*') {
                depth += 1;
                self.i += 2;
            } else if self.at(0) == Some('*') && self.at(1) == Some('/') {
                depth = depth.saturating_sub(1);
                self.i += 2;
                if depth == 0 {
                    self.push(start, self.i, theme::SYN_COMMENT);
                    return None;
                }
            } else {
                self.i += 1;
            }
        }
        self.push(start, self.i, theme::SYN_COMMENT);
        Some(Carry::BlockComment(depth))
    }

    /// Scan inside a raw string until `"` + `hashes` `#`; Some means still open at EOL.
    fn raw_string(&mut self, hashes: usize) -> Option<Carry> {
        let start = self.i;
        while self.i < self.len() {
            if self.at(0) == Some('"') && (1..=hashes).all(|k| self.at(k) == Some('#')) {
                self.i += 1 + hashes;
                self.push(start, self.i, theme::SYN_STRING);
                return None;
            }
            self.i += 1;
        }
        self.push(start, self.i, theme::SYN_STRING);
        Some(Carry::RawString(hashes))
    }

    /// Scan inside a normal string; Some means a trailing backslash continues it.
    fn string_body(&mut self) -> Option<Carry> {
        let start = self.i;
        while self.i < self.len() {
            match self.chars[self.i] {
                '\\' => {
                    if self.i + 1 >= self.len() {
                        self.i += 1;
                        self.push(start, self.i, theme::SYN_STRING);
                        return Some(Carry::Str);
                    }
                    self.i += 2;
                }
                '"' => {
                    self.i += 1;
                    self.push(start, self.i, theme::SYN_STRING);
                    return None;
                }
                _ => self.i += 1,
            }
        }
        self.push(start, self.i, theme::SYN_STRING);
        None
    }

    /// A `'`: char literal (`'x'`, `'\n'`) vs lifetime (`'a`, `'static`).
    fn quote(&mut self) {
        let start = self.i;
        let n1 = self.at(1);
        if n1 == Some('\\') {
            let mut j = (self.i + 3).min(self.len());
            while j < self.len() && self.chars[j] != '\'' {
                j += 1;
            }
            if j < self.len() {
                j += 1;
            }
            self.push(start, j, theme::SYN_STRING);
            self.i = j;
            return;
        }
        if n1.is_some() && n1 != Some('\'') && self.at(2) == Some('\'') {
            self.push(start, self.i + 3, theme::SYN_STRING);
            self.i += 3;
            return;
        }
        if n1.is_some_and(|ch| ch == '_' || ch.is_alphabetic()) {
            let mut j = self.i + 2;
            while j < self.len() && (self.chars[j] == '_' || self.chars[j].is_alphanumeric()) {
                j += 1;
            }
            self.push(start, j, theme::SYN_LIFETIME);
            self.i = j;
            return;
        }
        self.i += 1;
    }

    /// `#[…]` / `#![…]` to the matching `]`, skipping over string literals inside.
    fn attribute(&mut self) {
        let start = self.i;
        let mut j = self.i + 1;
        if self.chars.get(j) == Some(&'!') {
            j += 1;
        }
        let mut depth = 0usize;
        while j < self.len() {
            match self.chars[j] {
                '[' => {
                    depth += 1;
                    j += 1;
                }
                ']' => {
                    depth = depth.saturating_sub(1);
                    j += 1;
                    if depth == 0 {
                        break;
                    }
                }
                '"' => {
                    j += 1;
                    while j < self.len() {
                        match self.chars[j] {
                            '\\' => j += 2,
                            '"' => {
                                j += 1;
                                break;
                            }
                            _ => j += 1,
                        }
                    }
                }
                _ => j += 1,
            }
        }
        let j = j.min(self.len());
        self.push(start, j, theme::SYN_ATTRIBUTE);
        self.i = j;
    }

    /// Numeric literal: radix prefixes, `_` separators, float dot, signed exponent, suffix.
    fn number(&mut self) {
        let start = self.i;
        let radix_prefix = self.chars[start] == '0'
            && matches!(self.at(1), Some('x' | 'X' | 'b' | 'B' | 'o' | 'O'));
        self.consume_alnum();
        if !radix_prefix {
            if self.at(0) == Some('.') && self.at(1).is_some_and(|ch| ch.is_ascii_digit()) {
                self.i += 1;
                self.consume_alnum();
            }
            if matches!(self.chars.get(self.i.wrapping_sub(1)), Some('e') | Some('E'))
                && matches!(self.at(0), Some('+') | Some('-'))
                && self.at(1).is_some_and(|ch| ch.is_ascii_digit())
            {
                self.i += 1;
                self.consume_alnum();
            }
        }
        self.push(start, self.i, theme::SYN_NUMBER);
    }

    fn consume_alnum(&mut self) {
        while self.i < self.len() {
            let ch = self.chars[self.i];
            if ch == '_' || ch.is_alphanumeric() {
                self.i += 1;
            } else {
                break;
            }
        }
    }

    /// `r"…"`, `r#"…"#`, `b"…"`, `br#"…"#`, `b'…'`; None when it is a plain identifier.
    fn try_prefixed_literal(&mut self) -> Option<Option<Carry>> {
        let start = self.i;
        let (plen, raw) = match self.chars[self.i] {
            'r' => (1, true),
            'b' => match self.at(1) {
                Some('r') => (2, true),
                Some('"') => (1, false),
                Some('\'') => {
                    self.push(start, start + 1, theme::SYN_STRING);
                    self.i += 1;
                    self.quote();
                    return Some(None);
                }
                _ => return None,
            },
            _ => return None,
        };
        if raw {
            let mut j = self.i + plen;
            let mut hashes = 0;
            while self.chars.get(j) == Some(&'#') {
                hashes += 1;
                j += 1;
            }
            if self.chars.get(j) != Some(&'"') {
                return None;
            }
            self.push(start, j + 1, theme::SYN_STRING);
            self.i = j + 1;
            return Some(self.raw_string(hashes));
        }
        self.push(start, start + plen + 1, theme::SYN_STRING);
        self.i = start + plen + 1;
        Some(self.string_body())
    }

    /// Identifier: keyword / self / bool / primitive / macro / function / type.
    fn ident(&mut self) {
        let start = self.i;
        self.consume_alnum();
        let word: String = self.chars[start..self.i].iter().collect();
        let next = self.at(0);
        let color = if word == "self" || word == "Self" {
            Some(theme::SYN_SELF)
        } else if word == "true" || word == "false" {
            Some(theme::SYN_NUMBER)
        } else if is_keyword(&word) {
            Some(theme::SYN_KEYWORD)
        } else if is_primitive(&word) {
            Some(theme::SYN_TYPE)
        } else if next == Some('!') && self.at(1) != Some('=') {
            self.i += 1;
            Some(theme::SYN_MACRO)
        } else if next == Some('(') || self.after_fn {
            Some(theme::SYN_FUNCTION)
        } else if word.chars().next().is_some_and(char::is_uppercase) {
            Some(theme::SYN_TYPE)
        } else {
            None
        };
        self.after_fn = word == "fn";
        if let Some(color) = color {
            self.push(start, self.i, color);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::{Position, TextBuffer};

    fn lex(src: &str) -> Vec<Vec<HlSpan>> {
        let lines: Vec<String> = src.split('\n').map(str::to_string).collect();
        lex_rust(&lines)
    }

    fn color_at(line: &[HlSpan], col: usize) -> Option<Color> {
        line.iter().find(|s| col >= s.start && col < s.start + s.len).map(|s| s.color)
    }

    fn span_at(line: &[HlSpan], col: usize) -> Option<HlSpan> {
        line.iter().find(|s| col >= s.start && col < s.start + s.len).copied()
    }

    fn assert_invariants(lines: &[Vec<HlSpan>]) {
        for spans in lines {
            for w in spans.windows(2) {
                assert!(w[0].start + w[0].len <= w[1].start, "overlap/unsorted: {w:?}");
            }
            for s in spans {
                assert!(s.len > 0, "empty span");
            }
        }
    }

    #[test]
    fn nested_block_comments_span_lines() {
        let l = lex("let a = 1; /* outer /* inner\nstill comment */ still outer\ndone */ let b = 2;");
        assert_invariants(&l);
        assert_eq!(color_at(&l[0], 0), Some(theme::SYN_KEYWORD));
        assert_eq!(color_at(&l[0], 8), Some(theme::SYN_NUMBER));
        assert_eq!(span_at(&l[0], 11), Some(HlSpan { start: 11, len: 17, color: theme::SYN_COMMENT }));
        assert_eq!(l[1], vec![HlSpan { start: 0, len: 28, color: theme::SYN_COMMENT }]);
        assert_eq!(span_at(&l[2], 0), Some(HlSpan { start: 0, len: 7, color: theme::SYN_COMMENT }));
        assert_eq!(color_at(&l[2], 7), None);
        assert_eq!(span_at(&l[2], 8), Some(HlSpan { start: 8, len: 3, color: theme::SYN_KEYWORD }));
        assert_eq!(color_at(&l[2], 16), Some(theme::SYN_NUMBER));
    }

    #[test]
    fn raw_string_with_hashes_contains_quote() {
        let l = lex(r##"let s = r#"say "hi" now"#;"##);
        assert_invariants(&l);
        assert_eq!(span_at(&l[0], 8), Some(HlSpan { start: 8, len: 17, color: theme::SYN_STRING }));
        assert_eq!(color_at(&l[0], 15), Some(theme::SYN_STRING));
        assert_eq!(color_at(&l[0], 25), None);
    }

    #[test]
    fn raw_string_multiline() {
        let l = lex("let s = r##\"one\n\"# not end\nend\"##;");
        assert_invariants(&l);
        assert_eq!(span_at(&l[0], 8), Some(HlSpan { start: 8, len: 7, color: theme::SYN_STRING }));
        assert_eq!(l[1], vec![HlSpan { start: 0, len: 10, color: theme::SYN_STRING }]);
        assert_eq!(span_at(&l[2], 0), Some(HlSpan { start: 0, len: 6, color: theme::SYN_STRING }));
        assert_eq!(color_at(&l[2], 6), None);
    }

    #[test]
    fn lifetime_vs_char_literal() {
        let l = lex("fn f<'a>(x: &'a str) -> char { 'x' }");
        assert_invariants(&l);
        assert_eq!(span_at(&l[0], 5), Some(HlSpan { start: 5, len: 2, color: theme::SYN_LIFETIME }));
        assert_eq!(span_at(&l[0], 13), Some(HlSpan { start: 13, len: 2, color: theme::SYN_LIFETIME }));
        assert_eq!(span_at(&l[0], 31), Some(HlSpan { start: 31, len: 3, color: theme::SYN_STRING }));
        assert_eq!(span_at(&l[0], 3), Some(HlSpan { start: 3, len: 1, color: theme::SYN_FUNCTION }));
        assert_eq!(span_at(&l[0], 16), Some(HlSpan { start: 16, len: 3, color: theme::SYN_TYPE }));
        assert_eq!(span_at(&l[0], 24), Some(HlSpan { start: 24, len: 4, color: theme::SYN_TYPE }));
        assert_eq!(span_at(&l[0], 21), Some(HlSpan { start: 21, len: 2, color: theme::SYN_OPERATOR }));
    }

    #[test]
    fn char_literal_escapes() {
        let l = lex("let c = '\\n';\nlet q = '\\'';\nlet u = '\\u{263A}';");
        assert_invariants(&l);
        assert_eq!(span_at(&l[0], 8), Some(HlSpan { start: 8, len: 4, color: theme::SYN_STRING }));
        assert_eq!(color_at(&l[0], 12), None);
        assert_eq!(span_at(&l[1], 8), Some(HlSpan { start: 8, len: 4, color: theme::SYN_STRING }));
        assert_eq!(span_at(&l[2], 8), Some(HlSpan { start: 8, len: 10, color: theme::SYN_STRING }));
    }

    #[test]
    fn byte_char_and_byte_strings() {
        let l = lex("let a = b\"raw\"; let c = br#\"x\"#;\nlet z = b'z';");
        assert_invariants(&l);
        assert_eq!(span_at(&l[0], 8), Some(HlSpan { start: 8, len: 6, color: theme::SYN_STRING }));
        assert_eq!(color_at(&l[0], 14), None);
        assert_eq!(span_at(&l[0], 16), Some(HlSpan { start: 16, len: 3, color: theme::SYN_KEYWORD }));
        assert_eq!(span_at(&l[0], 24), Some(HlSpan { start: 24, len: 7, color: theme::SYN_STRING }));
        assert_eq!(span_at(&l[1], 8), Some(HlSpan { start: 8, len: 4, color: theme::SYN_STRING }));
    }

    #[test]
    fn attribute_spans() {
        let l = lex("#[derive(Debug, Clone)]\n#![allow(dead_code)]\n#[doc = \"a ] b\"] let x = 1;");
        assert_invariants(&l);
        assert_eq!(l[0], vec![HlSpan { start: 0, len: 23, color: theme::SYN_ATTRIBUTE }]);
        assert_eq!(l[1], vec![HlSpan { start: 0, len: 20, color: theme::SYN_ATTRIBUTE }]);
        assert_eq!(span_at(&l[2], 11), Some(HlSpan { start: 0, len: 16, color: theme::SYN_ATTRIBUTE }));
        assert_eq!(span_at(&l[2], 17), Some(HlSpan { start: 17, len: 3, color: theme::SYN_KEYWORD }));
        assert_eq!(color_at(&l[2], 25), Some(theme::SYN_NUMBER));
    }

    #[test]
    fn numbers_with_radix_underscores_suffixes() {
        let l = lex("let x = 0xFF_u8 + 0b1010_1111 + 0o77 + 1_000 + 3.14f64 + 2e-10;");
        assert_invariants(&l);
        assert_eq!(span_at(&l[0], 8), Some(HlSpan { start: 8, len: 7, color: theme::SYN_NUMBER }));
        assert_eq!(span_at(&l[0], 16), Some(HlSpan { start: 16, len: 1, color: theme::SYN_OPERATOR }));
        assert_eq!(span_at(&l[0], 18), Some(HlSpan { start: 18, len: 11, color: theme::SYN_NUMBER }));
        assert_eq!(span_at(&l[0], 32), Some(HlSpan { start: 32, len: 4, color: theme::SYN_NUMBER }));
        assert_eq!(span_at(&l[0], 39), Some(HlSpan { start: 39, len: 5, color: theme::SYN_NUMBER }));
        assert_eq!(span_at(&l[0], 47), Some(HlSpan { start: 47, len: 7, color: theme::SYN_NUMBER }));
        assert_eq!(span_at(&l[0], 57), Some(HlSpan { start: 57, len: 5, color: theme::SYN_NUMBER }));
        assert_eq!(color_at(&l[0], 62), None);
    }

    #[test]
    fn float_dot_does_not_eat_method_or_range() {
        let l = lex("let a = 1.max(2); let r = 0..10;");
        assert_invariants(&l);
        assert_eq!(span_at(&l[0], 8), Some(HlSpan { start: 8, len: 1, color: theme::SYN_NUMBER }));
        assert_eq!(color_at(&l[0], 9), None);
        assert_eq!(span_at(&l[0], 10), Some(HlSpan { start: 10, len: 3, color: theme::SYN_FUNCTION }));
        assert_eq!(span_at(&l[0], 26), Some(HlSpan { start: 26, len: 1, color: theme::SYN_NUMBER }));
        assert_eq!(color_at(&l[0], 27), None);
        assert_eq!(span_at(&l[0], 29), Some(HlSpan { start: 29, len: 2, color: theme::SYN_NUMBER }));
    }

    #[test]
    fn realistic_fn_snippet() {
        let src = "/// Doc line.\n\
                   #[derive(Debug)]\n\
                   pub struct Point { x: f64 }\n\
                   \n\
                   impl Point {\n\
                   \x20   pub fn norm(&self) -> f64 {\n\
                   \x20       let v = vec![self.x, 2.0_f32];\n\
                   \x20       v.len() as f64 // done\n\
                   \x20   }\n\
                   }";
        let l = lex(src);
        assert_invariants(&l);
        assert_eq!(l[0], vec![HlSpan { start: 0, len: 13, color: theme::SYN_COMMENT }]);
        assert_eq!(l[1], vec![HlSpan { start: 0, len: 16, color: theme::SYN_ATTRIBUTE }]);
        assert_eq!(span_at(&l[2], 0), Some(HlSpan { start: 0, len: 3, color: theme::SYN_KEYWORD }));
        assert_eq!(span_at(&l[2], 4), Some(HlSpan { start: 4, len: 6, color: theme::SYN_KEYWORD }));
        assert_eq!(span_at(&l[2], 11), Some(HlSpan { start: 11, len: 5, color: theme::SYN_TYPE }));
        assert_eq!(span_at(&l[2], 22), Some(HlSpan { start: 22, len: 3, color: theme::SYN_TYPE }));
        assert!(l[3].is_empty());
        assert_eq!(span_at(&l[4], 0), Some(HlSpan { start: 0, len: 4, color: theme::SYN_KEYWORD }));
        assert_eq!(span_at(&l[4], 5), Some(HlSpan { start: 5, len: 5, color: theme::SYN_TYPE }));
        assert_eq!(span_at(&l[5], 4), Some(HlSpan { start: 4, len: 3, color: theme::SYN_KEYWORD }));
        assert_eq!(span_at(&l[5], 8), Some(HlSpan { start: 8, len: 2, color: theme::SYN_KEYWORD }));
        assert_eq!(span_at(&l[5], 11), Some(HlSpan { start: 11, len: 4, color: theme::SYN_FUNCTION }));
        assert_eq!(span_at(&l[5], 16), Some(HlSpan { start: 16, len: 1, color: theme::SYN_OPERATOR }));
        assert_eq!(span_at(&l[5], 17), Some(HlSpan { start: 17, len: 4, color: theme::SYN_SELF }));
        assert_eq!(span_at(&l[5], 23), Some(HlSpan { start: 23, len: 2, color: theme::SYN_OPERATOR }));
        assert_eq!(span_at(&l[5], 26), Some(HlSpan { start: 26, len: 3, color: theme::SYN_TYPE }));
        assert_eq!(span_at(&l[6], 8), Some(HlSpan { start: 8, len: 3, color: theme::SYN_KEYWORD }));
        assert_eq!(span_at(&l[6], 16), Some(HlSpan { start: 16, len: 4, color: theme::SYN_MACRO }));
        assert_eq!(span_at(&l[6], 21), Some(HlSpan { start: 21, len: 4, color: theme::SYN_SELF }));
        assert_eq!(span_at(&l[6], 29), Some(HlSpan { start: 29, len: 7, color: theme::SYN_NUMBER }));
        assert_eq!(span_at(&l[7], 10), Some(HlSpan { start: 10, len: 3, color: theme::SYN_FUNCTION }));
        assert_eq!(span_at(&l[7], 16), Some(HlSpan { start: 16, len: 2, color: theme::SYN_KEYWORD }));
        assert_eq!(span_at(&l[7], 19), Some(HlSpan { start: 19, len: 3, color: theme::SYN_TYPE }));
        assert_eq!(span_at(&l[7], 23), Some(HlSpan { start: 23, len: 7, color: theme::SYN_COMMENT }));
    }

    #[test]
    fn unicode_idents_char_indexed() {
        let l = lex("let é = \"ü\";\nlet 中文 = 42;");
        assert_invariants(&l);
        assert_eq!(
            l[0],
            vec![
                HlSpan { start: 0, len: 3, color: theme::SYN_KEYWORD },
                HlSpan { start: 6, len: 1, color: theme::SYN_OPERATOR },
                HlSpan { start: 8, len: 3, color: theme::SYN_STRING },
            ]
        );
        assert_eq!(color_at(&l[1], 4), None);
        assert_eq!(span_at(&l[1], 9), Some(HlSpan { start: 9, len: 2, color: theme::SYN_NUMBER }));
    }

    #[test]
    fn string_backslash_continuation() {
        let l = lex("let s = \"abc\\\ndef\";\nlet n = 5;");
        assert_invariants(&l);
        assert_eq!(span_at(&l[0], 8), Some(HlSpan { start: 8, len: 5, color: theme::SYN_STRING }));
        assert_eq!(span_at(&l[1], 0), Some(HlSpan { start: 0, len: 4, color: theme::SYN_STRING }));
        assert_eq!(color_at(&l[1], 4), None);
        assert_eq!(color_at(&l[2], 8), Some(theme::SYN_NUMBER));
    }

    #[test]
    fn unterminated_string_ends_at_eol() {
        let l = lex("let s = \"abc\nlet x = 1;");
        assert_invariants(&l);
        assert_eq!(span_at(&l[0], 8), Some(HlSpan { start: 8, len: 4, color: theme::SYN_STRING }));
        assert_eq!(span_at(&l[1], 0), Some(HlSpan { start: 0, len: 3, color: theme::SYN_KEYWORD }));
    }

    #[test]
    fn escaped_backslash_at_eol_does_not_continue() {
        let l = lex("let s = \"ab\\\\\nlet x = 1;");
        assert_invariants(&l);
        assert_eq!(span_at(&l[1], 0), Some(HlSpan { start: 0, len: 3, color: theme::SYN_KEYWORD }));
    }

    #[test]
    fn macro_includes_bang_but_not_neq() {
        let l = lex("println!(\"hi {}\", name);\nif x != y { assert!(true); }");
        assert_invariants(&l);
        assert_eq!(span_at(&l[0], 0), Some(HlSpan { start: 0, len: 8, color: theme::SYN_MACRO }));
        assert_eq!(span_at(&l[0], 9), Some(HlSpan { start: 9, len: 7, color: theme::SYN_STRING }));
        assert_eq!(color_at(&l[1], 3), None);
        assert_eq!(span_at(&l[1], 5), Some(HlSpan { start: 5, len: 2, color: theme::SYN_OPERATOR }));
        assert_eq!(span_at(&l[1], 12), Some(HlSpan { start: 12, len: 7, color: theme::SYN_MACRO }));
        assert_eq!(span_at(&l[1], 20), Some(HlSpan { start: 20, len: 4, color: theme::SYN_NUMBER }));
    }

    #[test]
    fn keywords_self_and_bools() {
        let l = lex("match self { Self::A => true, _ => false }");
        assert_invariants(&l);
        assert_eq!(span_at(&l[0], 0), Some(HlSpan { start: 0, len: 5, color: theme::SYN_KEYWORD }));
        assert_eq!(span_at(&l[0], 6), Some(HlSpan { start: 6, len: 4, color: theme::SYN_SELF }));
        assert_eq!(span_at(&l[0], 13), Some(HlSpan { start: 13, len: 4, color: theme::SYN_SELF }));
        assert_eq!(span_at(&l[0], 19), Some(HlSpan { start: 19, len: 1, color: theme::SYN_TYPE }));
        assert_eq!(span_at(&l[0], 24), Some(HlSpan { start: 24, len: 4, color: theme::SYN_NUMBER }));
        assert_eq!(color_at(&l[0], 30), None);
        assert_eq!(span_at(&l[0], 35), Some(HlSpan { start: 35, len: 5, color: theme::SYN_NUMBER }));
    }

    #[test]
    fn doc_comments() {
        let l = lex("//! module docs\n/// item docs\n// plain");
        assert_invariants(&l);
        assert_eq!(l[0], vec![HlSpan { start: 0, len: 15, color: theme::SYN_COMMENT }]);
        assert_eq!(l[1], vec![HlSpan { start: 0, len: 13, color: theme::SYN_COMMENT }]);
        assert_eq!(l[2], vec![HlSpan { start: 0, len: 8, color: theme::SYN_COMMENT }]);
    }

    #[test]
    fn types_paths_and_calls() {
        let l = lex("let v: Vec<String> = Vec::new();");
        assert_invariants(&l);
        assert_eq!(span_at(&l[0], 7), Some(HlSpan { start: 7, len: 3, color: theme::SYN_TYPE }));
        assert_eq!(span_at(&l[0], 11), Some(HlSpan { start: 11, len: 6, color: theme::SYN_TYPE }));
        assert_eq!(span_at(&l[0], 21), Some(HlSpan { start: 21, len: 3, color: theme::SYN_TYPE }));
        assert_eq!(span_at(&l[0], 26), Some(HlSpan { start: 26, len: 3, color: theme::SYN_FUNCTION }));
    }

    #[test]
    fn fn_name_with_generics_is_function() {
        let l = lex("fn foo<T>(x: T) -> T {}");
        assert_invariants(&l);
        assert_eq!(span_at(&l[0], 3), Some(HlSpan { start: 3, len: 3, color: theme::SYN_FUNCTION }));
        assert_eq!(span_at(&l[0], 7), Some(HlSpan { start: 7, len: 1, color: theme::SYN_TYPE }));
        assert_eq!(span_at(&l[0], 13), Some(HlSpan { start: 13, len: 1, color: theme::SYN_TYPE }));
    }

    #[test]
    fn operator_run_stops_before_comment() {
        let l = lex("let y = 1 +// c");
        assert_invariants(&l);
        assert_eq!(span_at(&l[0], 10), Some(HlSpan { start: 10, len: 1, color: theme::SYN_OPERATOR }));
        assert_eq!(span_at(&l[0], 11), Some(HlSpan { start: 11, len: 4, color: theme::SYN_COMMENT }));
    }

    #[test]
    fn raw_identifier_is_not_a_string() {
        let l = lex("let r#type = 1;");
        assert_invariants(&l);
        assert!(!l[0].iter().any(|s| s.color == theme::SYN_STRING));
        assert_eq!(color_at(&l[0], 14), None);
    }

    #[test]
    fn empty_line_inside_block_comment() {
        let l = lex("/* a\n\nb */ let x = 1;");
        assert_invariants(&l);
        assert!(l[1].is_empty());
        assert_eq!(span_at(&l[2], 0), Some(HlSpan { start: 0, len: 4, color: theme::SYN_COMMENT }));
        assert_eq!(span_at(&l[2], 5), Some(HlSpan { start: 5, len: 3, color: theme::SYN_KEYWORD }));
    }

    #[test]
    fn highlighter_cache_and_extensions() {
        let mut hl = Highlighter::new();
        let rs = TextBuffer::from_text("let x = 1;");
        assert!(!hl.spans("main.rs", &rs)[0].is_empty());
        let md = TextBuffer::from_text("# heading\ntext");
        let spans = hl.spans("notes.md", &md);
        assert_eq!(spans.len(), 2);
        assert!(spans.iter().all(Vec::is_empty));
        let mut buf = TextBuffer::from_text("fn");
        assert_eq!(hl.spans("a.rs", &buf)[0][0].color, theme::SYN_KEYWORD);
        buf.insert_text(Position::new(0, 2), " main() {}");
        let relexed = hl.spans("a.rs", &buf);
        assert!(relexed[0].iter().any(|s| s.color == theme::SYN_FUNCTION));
    }

    #[test]
    fn thousand_lines_smoke() {
        let lines: Vec<String> = (0..1000)
            .map(|i| format!("fn f{i}(x: u32) -> u32 {{ x + {i} }} // c{i} \"s\" 'a' r#\"raw\"#"))
            .collect();
        let out = lex_rust(&lines);
        assert_eq!(out.len(), 1000);
        assert_invariants(&out);
    }
}
