//! A single-line editor with a char-indexed cursor and command history — the line-editing a
//! terminal does itself (a plugin can't rely on `egui::TextEdit` for a custom cell grid).

pub struct LineEditor {
    chars: Vec<char>,
    /// Cursor position as a char index in `0..=chars.len()`.
    cursor: usize,
    history: Vec<String>,
    /// `Some(i)` while browsing history; `None` when editing a live line.
    browse: Option<usize>,
    /// The live line stashed while browsing history, restored on ArrowDown past the newest.
    stash: Vec<char>,
}

impl LineEditor {
    pub fn new() -> Self {
        LineEditor { chars: Vec::new(), cursor: 0, history: Vec::new(), browse: None, stash: Vec::new() }
    }

    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn chars(&self) -> &[char] {
        &self.chars
    }

    pub fn insert(&mut self, c: char) {
        self.chars.insert(self.cursor, c);
        self.cursor += 1;
    }

    #[allow(dead_code)]
    pub fn insert_str(&mut self, s: &str) {
        for c in s.chars() {
            self.insert(c);
        }
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
        }
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        if self.cursor < self.chars.len() {
            self.cursor += 1;
        }
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.chars.len();
    }

    /// Delete from the cursor to the start of the line (Ctrl+U).
    pub fn kill_to_start(&mut self) {
        self.chars.drain(0..self.cursor);
        self.cursor = 0;
    }

    pub fn clear(&mut self) {
        self.chars.clear();
        self.cursor = 0;
        self.browse = None;
    }

    /// Take the current line, push it to history, and reset for the next line.
    pub fn take(&mut self) -> String {
        let line: String = self.chars.iter().collect();
        if !line.trim().is_empty() && self.history.last().map(String::as_str) != Some(line.as_str()) {
            self.history.push(line.clone());
        }
        self.clear();
        line
    }

    pub fn history(&self) -> &[String] {
        &self.history
    }

    /// Recall the previous history entry (ArrowUp).
    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.browse {
            None => {
                self.stash = self.chars.clone();
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.browse = Some(next);
        self.load(self.history[next].clone());
    }

    /// Recall the next history entry, or the stashed live line past the newest (ArrowDown).
    pub fn history_next(&mut self) {
        match self.browse {
            None => {}
            Some(i) if i + 1 < self.history.len() => {
                self.browse = Some(i + 1);
                self.load(self.history[i + 1].clone());
            }
            Some(_) => {
                self.browse = None;
                let stash = std::mem::take(&mut self.stash);
                self.chars = stash;
                self.cursor = self.chars.len();
            }
        }
    }

    fn load(&mut self, s: String) {
        self.chars = s.chars().collect();
        self.cursor = self.chars.len();
    }
}
