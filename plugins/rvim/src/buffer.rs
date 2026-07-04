//! Line-based text buffer with char-indexed positions and grouped snapshot undo.
//! All columns are char indices, never byte offsets.

use serde::{Deserialize, Serialize};

/// Cap on retained undo snapshots.
const UNDO_CAP: usize = 200;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default, Serialize, Deserialize)]
pub struct Position {
    pub line: usize,
    pub col: usize,
}

impl Position {
    pub fn new(line: usize, col: usize) -> Self {
        Position { line, col }
    }
}

#[derive(Clone)]
struct Snapshot {
    lines: Vec<String>,
    cursor: Position,
}

pub struct TextBuffer {
    lines: Vec<String>,
    version: u64,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// Open undo group: pre-edit snapshot and the version when it was taken.
    group: Option<(Snapshot, u64)>,
}

impl TextBuffer {
    pub fn from_text(text: &str) -> Self {
        let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
        if lines.is_empty() {
            lines.push(String::new());
        }
        TextBuffer { lines, version: 0, undo: Vec::new(), redo: Vec::new(), group: None }
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn line(&self, idx: usize) -> &str {
        self.lines.get(idx).map(String::as_str).unwrap_or("")
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// Char length of line `idx`; 0 for out-of-range lines.
    pub fn line_len(&self, idx: usize) -> usize {
        self.lines.get(idx).map(|l| l.chars().count()).unwrap_or(0)
    }

    pub fn char_at(&self, pos: Position) -> Option<char> {
        self.lines.get(pos.line)?.chars().nth(pos.col)
    }

    /// Clamp `pos` into the buffer. With `allow_end`, col may equal the line length
    /// (insert-mode cursor); otherwise it stops on the last char.
    pub fn clamp(&self, pos: Position, allow_end: bool) -> Position {
        let line = pos.line.min(self.lines.len().saturating_sub(1));
        let len = self.line_len(line);
        let max = if allow_end { len } else { len.saturating_sub(1) };
        Position::new(line, pos.col.min(max))
    }

    fn byte_idx(s: &str, char_idx: usize) -> usize {
        s.char_indices().nth(char_idx).map(|(i, _)| i).unwrap_or(s.len())
    }

    /// Snapshot for undo unless a group already captured one; bumps the version.
    fn touch(&mut self, cursor: Position) {
        if self.group.is_none() {
            self.push_undo(Snapshot { lines: self.lines.clone(), cursor });
        }
        self.version = self.version.wrapping_add(1);
    }

    fn push_undo(&mut self, snap: Snapshot) {
        self.undo.push(snap);
        if self.undo.len() > UNDO_CAP {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    /// Start grouping subsequent edits into one undo step (e.g. an insert-mode session).
    pub fn begin_undo_group(&mut self, cursor: Position) {
        if self.group.is_none() {
            self.group = Some((Snapshot { lines: self.lines.clone(), cursor }, self.version));
        }
    }

    /// Close the open group; records one undo step if anything changed.
    pub fn end_undo_group(&mut self) {
        if let Some((snap, at)) = self.group.take() {
            if self.version != at {
                self.push_undo(snap);
            }
        }
    }

    /// Revert to the previous undo step; returns the cursor recorded with it.
    pub fn undo(&mut self, cursor: Position) -> Option<Position> {
        self.end_undo_group();
        let snap = self.undo.pop()?;
        self.redo.push(Snapshot { lines: std::mem::replace(&mut self.lines, snap.lines), cursor });
        self.version = self.version.wrapping_add(1);
        Some(snap.cursor)
    }

    pub fn redo(&mut self, cursor: Position) -> Option<Position> {
        let snap = self.redo.pop()?;
        self.undo.push(Snapshot { lines: std::mem::replace(&mut self.lines, snap.lines), cursor });
        if self.undo.len() > UNDO_CAP {
            self.undo.remove(0);
        }
        self.version = self.version.wrapping_add(1);
        Some(snap.cursor)
    }

    /// Insert one char (or a line break for '\n') at `pos`; returns the position after it.
    pub fn insert_char(&mut self, pos: Position, c: char) -> Position {
        self.insert_text(pos, &c.to_string())
    }

    /// Insert possibly-multiline text at `pos`; returns the position after the insertion.
    pub fn insert_text(&mut self, pos: Position, text: &str) -> Position {
        let pos = self.clamp(pos, true);
        self.touch(pos);
        let line = &self.lines[pos.line];
        let at = Self::byte_idx(line, pos.col);
        let tail = line[at..].to_string();
        let head = line[..at].to_string();

        let mut segs = text.split('\n');
        let first = segs.next().unwrap_or("");
        let mut cur = head;
        cur.push_str(first);
        let mut end = Position::new(pos.line, pos.col + first.chars().count());
        let mut rest: Vec<String> = Vec::new();
        for seg in segs {
            rest.push(seg.to_string());
        }
        if rest.is_empty() {
            cur.push_str(&tail);
            self.lines[pos.line] = cur;
        } else {
            self.lines[pos.line] = cur;
            let last_idx = rest.len() - 1;
            end = Position::new(pos.line + rest.len(), rest[last_idx].chars().count());
            rest[last_idx].push_str(&tail);
            for (i, seg) in rest.into_iter().enumerate() {
                self.lines.insert(pos.line + 1 + i, seg);
            }
        }
        end
    }

    /// Delete the charwise range `[start, end)` (end exclusive); returns the removed text.
    pub fn delete_range(&mut self, start: Position, end: Position) -> String {
        let start = self.clamp(start, true);
        let end = self.clamp(end, true);
        let (start, end) = if start <= end { (start, end) } else { (end, start) };
        if start == end {
            return String::new();
        }
        self.touch(start);
        if start.line == end.line {
            let line = &mut self.lines[start.line];
            let a = Self::byte_idx(line, start.col);
            let b = Self::byte_idx(line, end.col);
            line.drain(a..b).collect()
        } else {
            let first = &self.lines[start.line];
            let a = Self::byte_idx(first, start.col);
            let mut removed = first[a..].to_string();
            let last = &self.lines[end.line];
            let b = Self::byte_idx(last, end.col);
            let tail = last[b..].to_string();
            for mid in &self.lines[start.line + 1..end.line] {
                removed.push('\n');
                removed.push_str(mid);
            }
            removed.push('\n');
            removed.push_str(&self.lines[end.line][..b]);
            self.lines[start.line].truncate(a);
            let keep = self.lines[start.line].clone() + &tail;
            self.lines[start.line] = keep;
            self.lines.drain(start.line + 1..=end.line);
            removed
        }
    }

    /// Remove whole lines `[first, last]` inclusive; returns them joined with '\n'.
    /// The buffer always keeps at least one (possibly empty) line.
    pub fn delete_lines(&mut self, first: usize, last: usize) -> String {
        let first = first.min(self.lines.len().saturating_sub(1));
        let last = last.min(self.lines.len().saturating_sub(1)).max(first);
        self.touch(Position::new(first, 0));
        let removed: Vec<String> = self.lines.drain(first..=last).collect();
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        removed.join("\n")
    }

    /// Insert whole lines before index `at` (clamped to the line count).
    pub fn insert_lines(&mut self, at: usize, new_lines: Vec<String>) {
        if new_lines.is_empty() {
            return;
        }
        let at = at.min(self.lines.len());
        self.touch(Position::new(at, 0));
        for (i, l) in new_lines.into_iter().enumerate() {
            self.lines.insert(at + i, l);
        }
    }

    /// Join line `idx` with the next; `with_space` collapses leading whitespace to one space.
    /// Returns the col where the lines met.
    pub fn join_line(&mut self, idx: usize, with_space: bool) -> Option<usize> {
        if idx + 1 >= self.lines.len() {
            return None;
        }
        self.touch(Position::new(idx, 0));
        let next = self.lines.remove(idx + 1);
        let line = &mut self.lines[idx];
        let joint = line.chars().count();
        if with_space {
            let trimmed = next.trim_start();
            if !line.is_empty() && !trimmed.is_empty() {
                line.push(' ');
            }
            line.push_str(trimmed);
        } else {
            line.push_str(&next);
        }
        Some(joint)
    }

    /// Replace the char at `pos`; returns false when there is none.
    pub fn replace_char(&mut self, pos: Position, c: char) -> bool {
        let Some(old) = self.char_at(pos) else { return false };
        self.touch(pos);
        let line = &mut self.lines[pos.line];
        let a = Self::byte_idx(line, pos.col);
        line.replace_range(a..a + old.len_utf8(), &c.to_string());
        true
    }

    /// Replace an entire line's content.
    pub fn set_line(&mut self, idx: usize, text: String) {
        if idx < self.lines.len() {
            self.touch(Position::new(idx, 0));
            self.lines[idx] = text;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_delete_roundtrip() {
        let mut b = TextBuffer::from_text("hello\nworld");
        let end = b.insert_text(Position::new(0, 5), ", brave\nnew");
        assert_eq!(b.text(), "hello, brave\nnew\nworld");
        assert_eq!(end, Position::new(1, 3));
        let removed = b.delete_range(Position::new(0, 5), end);
        assert_eq!(removed, ", brave\nnew");
        assert_eq!(b.text(), "hello\nworld");
    }

    #[test]
    fn unicode_char_indexing() {
        let mut b = TextBuffer::from_text("héllo");
        b.insert_char(Position::new(0, 2), 'x');
        assert_eq!(b.text(), "héxllo");
        assert_eq!(b.delete_range(Position::new(0, 1), Position::new(0, 3)), "éx");
    }

    #[test]
    fn grouped_undo_is_one_step() {
        let mut b = TextBuffer::from_text("ab");
        b.begin_undo_group(Position::new(0, 0));
        b.insert_char(Position::new(0, 2), 'c');
        b.insert_char(Position::new(0, 3), 'd');
        b.end_undo_group();
        assert_eq!(b.text(), "abcd");
        let cur = b.undo(Position::new(0, 3)).unwrap();
        assert_eq!(b.text(), "ab");
        assert_eq!(cur, Position::new(0, 0));
        b.redo(cur);
        assert_eq!(b.text(), "abcd");
    }

    #[test]
    fn delete_lines_keeps_one_line() {
        let mut b = TextBuffer::from_text("a\nb");
        assert_eq!(b.delete_lines(0, 1), "a\nb");
        assert_eq!(b.line_count(), 1);
        assert_eq!(b.text(), "");
    }

    #[test]
    fn join_collapses_indent() {
        let mut b = TextBuffer::from_text("fn main() {\n    body();");
        let joint = b.join_line(0, true).unwrap();
        assert_eq!(b.text(), "fn main() { body();");
        assert_eq!(joint, 11);
    }
}
