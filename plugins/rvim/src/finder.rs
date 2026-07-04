//! Fuzzy file finder: subsequence matcher with scoring plus the picker overlay state.

use crate::vim::Key;

/// What the finder wants the caller to do after a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinderAction {
    /// Keep the finder open.
    None,
    /// Close without opening anything.
    Close,
    /// Close and open this file.
    Open(String),
}

/// What the finder picks over: vfs files or open buffers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FinderTarget {
    Files,
    Buffers,
}

pub struct FinderState {
    pub query: String,
    /// Filtered file names, best match first, with matched char indices for highlighting.
    pub results: Vec<(String, Vec<usize>)>,
    pub selected: usize,
    pub target: FinderTarget,
    all: Vec<String>,
}

impl FinderState {
    pub fn new(files: Vec<String>) -> Self {
        let mut f = FinderState {
            query: String::new(),
            results: Vec::new(),
            selected: 0,
            target: FinderTarget::Files,
            all: files,
        };
        f.refilter();
        f
    }

    /// Picker over the open buffer names.
    pub fn buffers(names: Vec<String>) -> Self {
        let mut f = Self::new(names);
        f.target = FinderTarget::Buffers;
        f
    }

    /// Route a key into the finder (typing filters, arrows move, Enter opens, Esc closes).
    pub fn handle_key(&mut self, key: Key) -> FinderAction {
        match key {
            Key::Esc | Key::Ctrl('c') => FinderAction::Close,
            Key::Enter => {
                if let Some((name, _)) = self.results.get(self.selected) {
                    FinderAction::Open(name.clone())
                } else if self.target == FinderTarget::Buffers {
                    FinderAction::Close
                } else if !self.query.is_empty() {
                    // Create-on-open: no match, the query becomes a new file name.
                    FinderAction::Open(self.query.clone())
                } else {
                    FinderAction::None
                }
            }
            Key::Char(c) => {
                self.query.push(c);
                self.refilter();
                FinderAction::None
            }
            Key::Backspace => {
                self.query.pop();
                self.refilter();
                FinderAction::None
            }
            Key::Up | Key::Ctrl('p') => {
                self.selected = self.selected.saturating_sub(1);
                FinderAction::None
            }
            Key::Down | Key::Ctrl('n') | Key::Tab => {
                if self.selected + 1 < self.results.len() {
                    self.selected += 1;
                }
                FinderAction::None
            }
            _ => FinderAction::None,
        }
    }

    /// Rescore every file against the query, sort best-first, clamp the selection.
    fn refilter(&mut self) {
        let mut scored: Vec<(i32, String, Vec<usize>)> = self
            .all
            .iter()
            .filter_map(|n| fuzzy_match(&self.query, n).map(|(s, idx)| (s, n.clone(), idx)))
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        self.results = scored.into_iter().map(|(_, n, idx)| (n, idx)).collect();
        if self.selected >= self.results.len() {
            self.selected = self.results.len().saturating_sub(1);
        }
    }
}

const START_BONUS: i32 = 10;
const BOUNDARY_BONUS: i32 = 8;
const CAMEL_BONUS: i32 = 6;
const CONSECUTIVE_BONUS: i32 = 5;
const GAP_PENALTY: i32 = 1;
const NEG_INF: i32 = i32::MIN / 2;

/// Case-folded char equality.
fn chars_eq(a: char, b: char) -> bool {
    a == b || a.to_lowercase().eq(b.to_lowercase())
}

/// Positional bonus for matching candidate char `j`.
fn match_bonus(cand: &[char], j: usize) -> i32 {
    if j == 0 {
        return START_BONUS;
    }
    let prev = cand[j - 1];
    if matches!(prev, '/' | '.' | '_' | '-' | ' ') {
        return BOUNDARY_BONUS;
    }
    if prev.is_lowercase() && cand[j].is_uppercase() {
        return CAMEL_BONUS;
    }
    0
}

/// Score `query` against `candidate`; `None` when it is not a subsequence.
/// Higher is better; returns the matched char indices for highlighting.
pub fn fuzzy_match(query: &str, candidate: &str) -> Option<(i32, Vec<usize>)> {
    if query.is_empty() {
        return Some((0, Vec::new()));
    }
    let q: Vec<char> = query.chars().collect();
    let cand: Vec<char> = candidate.chars().collect();
    let (m, n) = (q.len(), cand.len());
    if m > n {
        return None;
    }
    let bonus: Vec<i32> = (0..n).map(|j| match_bonus(&cand, j)).collect();

    // score[i][j]: best score with query char i matched at candidate char j.
    let mut score = vec![vec![NEG_INF; n]; m];
    let mut parent = vec![vec![usize::MAX; n]; m];

    for j in 0..n {
        if chars_eq(q[0], cand[j]) {
            score[0][j] = bonus[j];
        }
    }
    for i in 1..m {
        // Running max over gapped predecessors k <= j-2, decayed by GAP_PENALTY per step.
        let mut gap_best = NEG_INF;
        let mut gap_k = usize::MAX;
        for j in i..n {
            let adj = score[i - 1][j - 1];
            let mut val = NEG_INF;
            let mut par = usize::MAX;
            if adj > NEG_INF {
                val = adj + CONSECUTIVE_BONUS;
                par = j - 1;
            }
            if gap_best > NEG_INF && gap_best > val {
                val = gap_best;
                par = gap_k;
            }
            if val > NEG_INF && chars_eq(q[i], cand[j]) {
                score[i][j] = val + bonus[j];
                parent[i][j] = par;
            }
            // Fold k = j-1 into the gapped window for the next j.
            let entering = if adj > NEG_INF { adj - GAP_PENALTY } else { NEG_INF };
            let carried = if gap_best > NEG_INF { gap_best - GAP_PENALTY } else { NEG_INF };
            if entering >= carried {
                gap_best = entering;
                gap_k = j - 1;
            } else {
                gap_best = carried;
            }
        }
    }

    let mut best = NEG_INF;
    let mut best_j = usize::MAX;
    for j in 0..n {
        if score[m - 1][j] > best {
            best = score[m - 1][j];
            best_j = j;
        }
    }
    if best <= NEG_INF {
        return None;
    }
    let mut indices = vec![0usize; m];
    let mut j = best_j;
    for i in (0..m).rev() {
        indices[i] = j;
        j = parent[i][j];
    }
    Some((best, indices))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score(query: &str, cand: &str) -> i32 {
        fuzzy_match(query, cand).expect("expected a match").0
    }

    fn indices(query: &str, cand: &str) -> Vec<usize> {
        fuzzy_match(query, cand).expect("expected a match").1
    }

    #[test]
    fn empty_query_matches_everything() {
        assert_eq!(fuzzy_match("", "main.rs"), Some((0, Vec::new())));
        assert_eq!(fuzzy_match("", ""), Some((0, Vec::new())));
    }

    #[test]
    fn non_subsequence_is_none() {
        assert!(fuzzy_match("xyz", "main.rs").is_none());
        assert!(fuzzy_match("sm", "m.s").is_none());
        assert!(fuzzy_match("aa", "a").is_none());
    }

    #[test]
    fn query_longer_than_candidate_is_none() {
        assert!(fuzzy_match("main.rs!", "main.rs").is_none());
    }

    #[test]
    fn mrs_prefers_main_rs_over_mars_txt() {
        assert!(score("mrs", "main.rs") > score("mrs", "mars.txt"));
    }

    #[test]
    fn matched_indices_are_char_positions() {
        assert_eq!(indices("mrs", "main.rs"), vec![0, 5, 6]);
        assert_eq!(indices("main", "main.rs"), vec![0, 1, 2, 3]);
    }

    #[test]
    fn start_of_name_bonus() {
        assert!(score("ma", "main.rs") > score("ma", "omar.rs"));
    }

    #[test]
    fn boundary_bonus_after_separator() {
        assert!(score("s", "x/s.rs") > score("s", "xs.rs"));
        assert_eq!(indices("s", "x/s.rs"), vec![2]);
        assert!(score("r", "a_r.txt") > score("r", "ar.txt"));
        assert!(score("r", "a-r.txt") > score("r", "ar.txt"));
        assert!(score("r", "a.r") > score("r", "ar"));
    }

    #[test]
    fn extension_boundary_beats_buried_match() {
        // 'r' after '.' outscores an 'r' buried mid-word.
        assert_eq!(indices("rs", "main.rs"), vec![5, 6]);
    }

    #[test]
    fn camel_case_bonus() {
        assert!(score("b", "fooBar.rs") > score("b", "foobar.rs"));
        assert_eq!(indices("b", "fooBar.rs"), vec![3]);
    }

    #[test]
    fn consecutive_beats_gapped() {
        assert!(score("mn", "mn.rs") > score("mn", "moon.rs"));
    }

    #[test]
    fn wider_gap_scores_lower() {
        assert!(score("mn", "man.rs") > score("mn", "maan.rs"));
    }

    #[test]
    fn case_insensitive_matching() {
        assert_eq!(indices("MAIN", "main.rs"), vec![0, 1, 2, 3]);
        assert_eq!(indices("readme", "README.md"), vec![0, 1, 2, 3, 4, 5]);
        assert_eq!(score("main", "main.rs"), score("MAIN", "main.rs"));
    }

    #[test]
    fn unicode_indices() {
        assert_eq!(indices("é", "café.rs"), vec![3]);
        assert_eq!(indices("中", "你好中国.txt"), vec![2]);
        assert_eq!(indices("é中", "aé中b"), vec![1, 2]);
    }

    fn finder() -> FinderState {
        FinderState::new(vec![
            "lib.rs".to_string(),
            "main.rs".to_string(),
            "notes.md".to_string(),
        ])
    }

    fn names(f: &FinderState) -> Vec<&str> {
        f.results.iter().map(|(n, _)| n.as_str()).collect()
    }

    #[test]
    fn empty_query_lists_all_sorted() {
        let f = finder();
        assert_eq!(names(&f), vec!["lib.rs", "main.rs", "notes.md"]);
        assert!(f.results.iter().all(|(_, idx)| idx.is_empty()));
        assert_eq!(f.selected, 0);
    }

    #[test]
    fn typing_filters_and_backspace_restores() {
        let mut f = finder();
        assert!(matches!(f.handle_key(Key::Char('l')), FinderAction::None));
        assert!(matches!(f.handle_key(Key::Char('i')), FinderAction::None));
        assert_eq!(f.query, "li");
        assert_eq!(names(&f), vec!["lib.rs"]);
        f.handle_key(Key::Backspace);
        assert_eq!(f.query, "l");
        assert_eq!(names(&f), vec!["lib.rs"]);
        f.handle_key(Key::Backspace);
        assert_eq!(f.query, "");
        assert_eq!(names(&f).len(), 3);
        f.handle_key(Key::Backspace);
        assert_eq!(f.query, "");
    }

    #[test]
    fn selection_moves_and_clamps() {
        let mut f = finder();
        f.handle_key(Key::Up);
        assert_eq!(f.selected, 0);
        f.handle_key(Key::Down);
        assert_eq!(f.selected, 1);
        f.handle_key(Key::Tab);
        assert_eq!(f.selected, 2);
        f.handle_key(Key::Ctrl('n'));
        assert_eq!(f.selected, 2);
        f.handle_key(Key::Ctrl('p'));
        assert_eq!(f.selected, 1);
        f.handle_key(Key::Up);
        assert_eq!(f.selected, 0);
    }

    #[test]
    fn selection_clamps_when_results_shrink() {
        let mut f = finder();
        f.handle_key(Key::Down);
        f.handle_key(Key::Down);
        assert_eq!(f.selected, 2);
        f.handle_key(Key::Char('m'));
        // "m" matches main.rs and notes.md but not lib.rs.
        assert_eq!(names(&f).len(), 2);
        assert_eq!(f.selected, 1);
    }

    #[test]
    fn enter_opens_selected() {
        let mut f = finder();
        f.handle_key(Key::Down);
        let picked = names(&f)[1].to_string();
        assert_eq!(f.handle_key(Key::Enter), FinderAction::Open(picked));
    }

    #[test]
    fn enter_with_no_results_creates_from_query() {
        let mut f = finder();
        for c in "zzz".chars() {
            f.handle_key(Key::Char(c));
        }
        assert!(f.results.is_empty());
        assert_eq!(f.handle_key(Key::Enter), FinderAction::Open("zzz".to_string()));
    }

    #[test]
    fn enter_with_nothing_stays_open() {
        let mut f = FinderState::new(Vec::new());
        assert_eq!(f.handle_key(Key::Enter), FinderAction::None);
    }

    #[test]
    fn buffers_target_never_creates_on_open() {
        let mut f = FinderState::buffers(vec!["main.rs".to_string()]);
        assert_eq!(f.target, FinderTarget::Buffers);
        for c in "zzz".chars() {
            f.handle_key(Key::Char(c));
        }
        assert!(f.results.is_empty());
        assert_eq!(f.handle_key(Key::Enter), FinderAction::Close);
        let mut f = FinderState::buffers(Vec::new());
        assert_eq!(f.handle_key(Key::Enter), FinderAction::Close);
    }

    #[test]
    fn esc_and_ctrl_c_close() {
        let mut f = finder();
        assert_eq!(f.handle_key(Key::Esc), FinderAction::Close);
        assert_eq!(f.handle_key(Key::Ctrl('c')), FinderAction::Close);
    }

    #[test]
    fn unhandled_keys_are_ignored() {
        let mut f = finder();
        assert_eq!(f.handle_key(Key::Left), FinderAction::None);
        assert_eq!(f.handle_key(Key::Home), FinderAction::None);
        assert_eq!(f.query, "");
        assert_eq!(names(&f).len(), 3);
    }

    #[test]
    fn results_sorted_by_score_then_name() {
        let mut f = FinderState::new(vec![
            "mars.txt".to_string(),
            "main.rs".to_string(),
        ]);
        for c in "mrs".chars() {
            f.handle_key(Key::Char(c));
        }
        assert_eq!(names(&f), vec!["main.rs", "mars.txt"]);
    }
}
