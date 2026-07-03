//! Fuzzy file finder: subsequence matcher with scoring plus the picker overlay state.

use crate::vim::Key;

/// What the finder wants the caller to do after a key.
pub enum FinderAction {
    /// Keep the finder open.
    None,
    /// Close without opening anything.
    Close,
    /// Close and open this file.
    Open(String),
}

pub struct FinderState {
    pub query: String,
    /// Filtered file names, best match first, with matched char indices for highlighting.
    pub results: Vec<(String, Vec<usize>)>,
    pub selected: usize,
    all: Vec<String>,
}

impl FinderState {
    pub fn new(files: Vec<String>) -> Self {
        let mut f = FinderState { query: String::new(), results: Vec::new(), selected: 0, all: files };
        f.refilter();
        f
    }

    /// Route a key into the finder (typing filters, arrows move, Enter opens, Esc closes).
    pub fn handle_key(&mut self, key: Key) -> FinderAction {
        // STUB: implemented by the finder module owner.
        let _ = key;
        FinderAction::Close
    }

    fn refilter(&mut self) {
        // STUB: implemented by the finder module owner.
        self.results = self.all.iter().map(|n| (n.clone(), Vec::new())).collect();
    }
}

/// Score `query` against `candidate`; `None` when it is not a subsequence.
/// Higher is better; returns the matched char indices for highlighting.
pub fn fuzzy_match(query: &str, candidate: &str) -> Option<(i32, Vec<usize>)> {
    // STUB: implemented by the finder module owner.
    let _ = (query, candidate);
    None
}
