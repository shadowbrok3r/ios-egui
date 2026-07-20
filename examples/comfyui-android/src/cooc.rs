//! Personal danbooru tag co-occurrence model. Learns which tags the user queues together and
//! scores next-tag / re-rank candidates from that. Pure: no egui/android deps.

use crate::tags::fold;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

/// Distinct-tag cap; lowest-total tags are evicted past it.
const MAX_TAGS: usize = 4000;
/// Per-tag neighbor cap; lowest-count neighbors are evicted past it.
const MAX_NEIGHBORS: usize = 200;
/// Global-frequency smoothing in the PMI-flavored score denominator.
const SMOOTHING: f32 = 1.0;

/// One tag's global prompt count plus its co-occurrence counts against other tags.
#[derive(Default, Serialize, Deserialize)]
struct TagStats {
    #[serde(default)]
    total: u32,
    #[serde(default)]
    neighbors: HashMap<String, u32>,
}

/// Per-tag (folded key) neighbor counts and totals learned from queued prompts.
#[derive(Default, Serialize, Deserialize)]
pub struct CoocModel {
    #[serde(default)]
    tags: HashMap<String, TagStats>,
}

impl CoocModel {
    /// Record one prompt's tags: bump each tag's total and every unordered pair (both directions).
    /// Folds and dedupes keys, skips single-tag prompts. Returns whether anything was recorded.
    pub fn observe(&mut self, tags: &[String]) -> bool {
        let mut uniq: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for t in tags {
            let f = fold(t);
            if !f.is_empty() && seen.insert(f.clone()) {
                uniq.push(f);
            }
        }
        if uniq.len() < 2 {
            return false;
        }
        for t in &uniq {
            self.tags.entry(t.clone()).or_default().total += 1;
        }
        for i in 0..uniq.len() {
            for j in 0..uniq.len() {
                if i != j {
                    let st = self.tags.get_mut(&uniq[i]).unwrap();
                    *st.neighbors.entry(uniq[j].clone()).or_default() += 1;
                }
            }
        }
        self.enforce_caps();
        true
    }

    /// PMI-flavored score: `cand`'s co-occurrence mass `raw` damped by its global frequency, so a
    /// specific tag can outrank a ubiquitous one.
    fn score(&self, cand: &str, raw: u32) -> f32 {
        if raw == 0 {
            return 0.0;
        }
        let total = self.tags.get(cand).map(|s| s.total).unwrap_or(0);
        raw as f32 / (total as f32 + SMOOTHING)
    }

    /// Sum of `cand`'s co-occurrence with each present tag.
    fn raw_cooc(&self, present: &HashSet<String>, cand: &str) -> u32 {
        present
            .iter()
            .filter_map(|p| self.tags.get(p))
            .filter_map(|st| st.neighbors.get(cand))
            .sum()
    }

    /// Top next-tag candidates for the tags already present, scored by [`score`], excluding present.
    pub fn suggest(&self, present: &[String], limit: usize) -> Vec<(String, f32)> {
        if limit == 0 {
            return Vec::new();
        }
        let present: HashSet<String> =
            present.iter().map(|t| fold(t)).filter(|s| !s.is_empty()).collect();
        let mut raw: HashMap<&str, u32> = HashMap::new();
        for p in &present {
            if let Some(st) = self.tags.get(p) {
                for (nb, c) in &st.neighbors {
                    if !present.contains(nb) {
                        *raw.entry(nb.as_str()).or_default() += c;
                    }
                }
            }
        }
        let mut scored: Vec<(String, f32)> = raw
            .into_iter()
            .map(|(cand, r)| (cand.to_string(), self.score(cand, r)))
            .collect();
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal).then_with(|| a.0.cmp(&b.0))
        });
        scored.truncate(limit);
        scored
    }

    /// Blend weight for prefix autocomplete: `candidate`'s fit against the present set (0 if absent).
    pub fn rerank_boost(&self, present: &[String], candidate: &str) -> f32 {
        let cand = fold(candidate);
        if cand.is_empty() {
            return 0.0;
        }
        let present: HashSet<String> =
            present.iter().map(|t| fold(t)).filter(|s| !s.is_empty()).collect();
        if present.contains(&cand) {
            return 0.0;
        }
        let raw = self.raw_cooc(&present, &cand);
        self.score(&cand, raw)
    }

    /// Trim neighbor lists past the cap, then evict the least-seen tags past the distinct-tag cap.
    fn enforce_caps(&mut self) {
        for st in self.tags.values_mut() {
            if st.neighbors.len() > MAX_NEIGHBORS {
                let mut v: Vec<(String, u32)> = std::mem::take(&mut st.neighbors).into_iter().collect();
                v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                v.truncate(MAX_NEIGHBORS);
                st.neighbors = v.into_iter().collect();
            }
        }
        if self.tags.len() > MAX_TAGS {
            let mut totals: Vec<(String, u32)> =
                self.tags.iter().map(|(k, v)| (k.clone(), v.total)).collect();
            totals.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
            let excess = self.tags.len() - MAX_TAGS;
            let doomed: HashSet<String> = totals.into_iter().take(excess).map(|(k, _)| k).collect();
            self.tags.retain(|k, _| !doomed.contains(k));
            for st in self.tags.values_mut() {
                st.neighbors.retain(|k, _| !doomed.contains(k));
            }
        }
    }
}

/// Stable re-rank of prefix matches by (co-oc boost desc, existing count desc).
pub fn blend_rank(matches: &mut [(String, u32, u8)], boost: impl Fn(&str) -> f32) {
    matches.sort_by(|a, b| {
        boost(&b.0)
            .partial_cmp(&boost(&a.0))
            .unwrap_or(Ordering::Equal)
            .then_with(|| b.1.cmp(&a.1))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn observe_skips_single_and_folds_keys() {
        let mut m = CoocModel::default();
        assert!(!m.observe(&s(&["solo"])));
        assert!(!m.observe(&s(&["1girl", "1girl"])));
        // Folding: "Long_Hair" and "long hair" collapse to one key.
        assert!(m.observe(&s(&["1girl", "Long_Hair", "long hair"])));
        let out = m.suggest(&s(&["1girl"]), 8);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "long hair");
    }

    #[test]
    fn suggest_ranks_related_and_excludes_present() {
        let mut m = CoocModel::default();
        for _ in 0..10 {
            m.observe(&s(&["a", "b"]));
        }
        // C is globally frequent but never appears with A.
        for _ in 0..50 {
            m.observe(&s(&["c", "d"]));
        }
        let out = m.suggest(&s(&["a"]), 8);
        let names: Vec<&str> = out.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["b"], "B ranks in, ubiquitous-but-unrelated C stays out");

        // Present tags never appear as candidates.
        m.observe(&s(&["a", "b", "e"]));
        let out = m.suggest(&s(&["a", "b"]), 8);
        let names: Vec<&str> = out.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"e"));
        assert!(!names.contains(&"a") && !names.contains(&"b"));
    }

    #[test]
    fn specific_tag_beats_ubiquitous_by_pmi() {
        let mut m = CoocModel::default();
        // "ubiq" is everywhere and co-occurs with "a" a lot in absolute terms.
        for i in 0..20 {
            m.observe(&s(&["ubiq", &format!("z{i}")]));
        }
        for _ in 0..8 {
            m.observe(&s(&["a", "ubiq"]));
        }
        // "spec" is rarer and appears mostly with "a".
        for _ in 0..5 {
            m.observe(&s(&["a", "spec"]));
        }
        let out = m.suggest(&s(&["a"]), 8);
        let names: Vec<&str> = out.iter().map(|(n, _)| n.as_str()).collect();
        let spec = names.iter().position(|n| *n == "spec").unwrap();
        let ubiq = names.iter().position(|n| *n == "ubiq").unwrap();
        assert!(spec < ubiq, "specific tag outranks the ubiquitous one: {names:?}");
    }

    #[test]
    fn neighbor_cap_evicts_weakest() {
        let mut m = CoocModel::default();
        for i in 0..(MAX_NEIGHBORS + 50) {
            m.observe(&s(&["anchor", &format!("n{i}")]));
        }
        let n = m.tags.get("anchor").unwrap().neighbors.len();
        assert_eq!(n, MAX_NEIGHBORS);
    }

    #[test]
    fn tag_cap_evicts_lowest_total() {
        let mut m = CoocModel::default();
        // Two hot tags survive on high total.
        for _ in 0..5 {
            m.observe(&s(&["keep_a", "keep_b"]));
        }
        // Flood past the cap with total-1 filler pairs.
        let pairs = (MAX_TAGS / 2) + 200;
        for i in 0..pairs {
            m.observe(&s(&[&format!("fa{i}"), &format!("fb{i}")]));
        }
        assert!(m.tags.len() <= MAX_TAGS);
        assert!(m.tags.contains_key("keep a"));
        assert!(m.tags.contains_key("keep b"));
    }

    #[test]
    fn serde_round_trip_preserves_ranking() {
        let mut m = CoocModel::default();
        for _ in 0..7 {
            m.observe(&s(&["a", "b"]));
        }
        m.observe(&s(&["a", "c"]));
        let json = serde_json::to_string(&m).unwrap();
        let back: CoocModel = serde_json::from_str(&json).unwrap();
        assert_eq!(m.suggest(&s(&["a"]), 8), back.suggest(&s(&["a"]), 8));
        let out = back.suggest(&s(&["a"]), 8);
        let names: Vec<&str> = out.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["b", "c"]);
    }

    #[test]
    fn blend_rank_zero_data_preserves_order() {
        // No co-oc data: boost is 0 for all, so the incoming count-desc order is untouched.
        let mut matches = vec![
            ("solo".to_string(), 900u32, 0u8),
            ("1girl".to_string(), 800u32, 0u8),
            ("smile".to_string(), 700u32, 0u8),
        ];
        let before = matches.clone();
        blend_rank(&mut matches, |_| 0.0);
        assert_eq!(matches, before);
    }

    #[test]
    fn blend_rank_floats_boosted_up() {
        let mut matches = vec![
            ("solo".to_string(), 900u32, 0u8),
            ("thighhighs".to_string(), 100u32, 0u8),
        ];
        // "thighhighs" fits the present prompt; it jumps ahead of the more common "solo".
        blend_rank(&mut matches, |n| if n == "thighhighs" { 1.0 } else { 0.0 });
        assert_eq!(matches[0].0, "thighhighs");
    }
}
