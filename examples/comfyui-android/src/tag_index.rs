//! Persistent on-device auto-tag index: gallery item key -> WD14 tags, with folded tag search,
//! facet counts and a rating (SFW/NSFW) query. Pure and host-testable; the background pump and the
//! gallery UI that drive it live in `app`.

// The query helpers are consumed by the gallery unconditionally; the write path is exercised only
// by the local-npu auto-tag pump (or unit tests), so allow it to be dead without the feature.
#![cfg_attr(not(feature = "local-npu"), allow(dead_code))]

use crate::tags::fold;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Entry schema version; an entry written by an older shape is re-tagged rather than trusted.
pub const SCHEMA_VERSION: u32 = 1;

/// One tag name (raw WD14 form, underscores kept) with its predicted probability.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Scored {
    pub name: String,
    pub prob: f32,
}

/// One image's tag read: general + character tags and the top-1 rating.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TagEntry {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub general: Vec<Scored>,
    #[serde(default)]
    pub character: Vec<Scored>,
    #[serde(default)]
    pub rating: Option<Scored>,
}

impl TagEntry {
    /// General + character tag names in display form (underscores rendered as spaces).
    pub fn display_names(&self) -> Vec<String> {
        self.general.iter().chain(&self.character).map(|t| t.name.replace('_', " ")).collect()
    }

    /// questionable | explicit read as NSFW; general | sensitive (or anything else) as safe.
    fn nsfw(&self) -> bool {
        matches!(self.rating.as_ref().map(|r| r.name.as_str()), Some("questionable") | Some("explicit"))
    }
}

/// Item key -> tags. Persisted as JSON at `{documents}/comfyui/tag_index.json`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TagIndex {
    #[serde(default)]
    entries: HashMap<String, TagEntry>,
}

impl TagIndex {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether `key` carries a current-version entry; stale-version entries report absent so the
    /// pump re-tags them.
    pub fn contains(&self, key: &str) -> bool {
        self.entries.get(key).is_some_and(|e| e.version == SCHEMA_VERSION)
    }

    /// Store `entry` for `key`, stamped at the current schema version.
    pub fn insert(&mut self, key: String, mut entry: TagEntry) {
        entry.version = SCHEMA_VERSION;
        self.entries.insert(key, entry);
    }

    /// Folded substring match of `needle` against `key`'s tag names; false if `key` is unindexed.
    pub fn matches(&self, key: &str, needle: &str) -> bool {
        let Some(e) = self.entries.get(key) else { return false };
        let q = fold(needle);
        e.general.iter().chain(&e.character).any(|t| fold(&t.name).contains(&q))
    }

    /// (folded tag name, count) over `keys`' general + character tags, top `n` by count then name.
    /// Each tag counts once per image.
    pub fn top_tags(&self, keys: &[String], n: usize) -> Vec<(String, usize)> {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for k in keys {
            let Some(e) = self.entries.get(k) else { continue };
            let mut seen: HashSet<String> = HashSet::new();
            for t in e.general.iter().chain(&e.character) {
                let f = fold(&t.name);
                if !f.is_empty() && seen.insert(f.clone()) {
                    *counts.entry(f).or_default() += 1;
                }
            }
        }
        let mut v: Vec<(String, usize)> = counts.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v.truncate(n);
        v
    }

    /// questionable | explicit -> NSFW; general | sensitive (or unknown) -> safe; None if unindexed.
    pub fn is_nsfw(&self, key: &str) -> Option<bool> {
        self.entries.get(key).map(|e| e.nsfw())
    }

    /// General + character tag names (display form) for `key`.
    pub fn display_names(&self, key: &str) -> Vec<String> {
        self.entries.get(key).map(|e| e.display_names()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scored(name: &str, prob: f32) -> Scored {
        Scored { name: name.into(), prob }
    }

    fn entry(general: &[&str], character: &[&str], rating: Option<&str>) -> TagEntry {
        TagEntry {
            version: SCHEMA_VERSION,
            general: general.iter().map(|n| scored(n, 0.9)).collect(),
            character: character.iter().map(|n| scored(n, 0.9)).collect(),
            rating: rating.map(|r| scored(r, 0.8)),
        }
    }

    #[test]
    fn insert_stamps_version_and_contains_is_version_gated() {
        let mut idx = TagIndex::default();
        idx.insert("a/1.png".into(), TagEntry::default());
        assert!(idx.contains("a/1.png"));
        assert!(!idx.contains("missing"));
        assert!(!idx.is_empty());
    }

    #[test]
    fn matches_folds_case_and_underscores() {
        let mut idx = TagIndex::default();
        idx.insert("a/1.png".into(), entry(&["long_hair", "1girl"], &["hakurei_reimu"], None));
        assert!(idx.matches("a/1.png", "Long Hair"));
        assert!(idx.matches("a/1.png", "hair"));
        assert!(idx.matches("a/1.png", "reimu"));
        assert!(!idx.matches("a/1.png", "blonde"));
        // Unindexed keys never match.
        assert!(!idx.matches("b/2.png", "hair"));
    }

    #[test]
    fn top_tags_counts_once_per_image_and_orders() {
        let mut idx = TagIndex::default();
        idx.insert("a/1.png".into(), entry(&["1girl", "smile", "long_hair"], &[], None));
        idx.insert("a/2.png".into(), entry(&["1girl", "smile"], &[], None));
        idx.insert("a/3.png".into(), entry(&["1girl"], &["hakurei_reimu"], None));
        let keys: Vec<String> = vec!["a/1.png".into(), "a/2.png".into(), "a/3.png".into()];
        let top = idx.top_tags(&keys, 3);
        assert_eq!(top[0], ("1girl".to_string(), 3));
        assert_eq!(top[1], ("smile".to_string(), 2));
        // Third slot ties at 1: alphabetical order picks "hakurei reimu".
        assert_eq!(top[2].1, 1);
        assert_eq!(top.len(), 3);
    }

    #[test]
    fn rating_maps_to_nsfw() {
        let mut idx = TagIndex::default();
        idx.insert("q".into(), entry(&["1girl"], &[], Some("questionable")));
        idx.insert("e".into(), entry(&["1girl"], &[], Some("explicit")));
        idx.insert("g".into(), entry(&["1girl"], &[], Some("general")));
        idx.insert("s".into(), entry(&["1girl"], &[], Some("sensitive")));
        idx.insert("n".into(), entry(&["1girl"], &[], None));
        assert_eq!(idx.is_nsfw("q"), Some(true));
        assert_eq!(idx.is_nsfw("e"), Some(true));
        assert_eq!(idx.is_nsfw("g"), Some(false));
        assert_eq!(idx.is_nsfw("s"), Some(false));
        // Indexed but no rating counts as safe; truly unindexed is None.
        assert_eq!(idx.is_nsfw("n"), Some(false));
        assert_eq!(idx.is_nsfw("missing"), None);
    }

    #[test]
    fn serde_round_trip_and_unknown_version_tolerance() {
        let mut idx = TagIndex::default();
        idx.insert("a/1.png".into(), entry(&["long_hair", "1girl"], &["reimu"], Some("general")));
        idx.insert("a/2.png".into(), entry(&["solo"], &[], Some("explicit")));
        let json = serde_json::to_string(&idx).unwrap();
        let back: TagIndex = serde_json::from_str(&json).unwrap();
        assert!(back.contains("a/1.png") && back.contains("a/2.png"));
        assert!(back.matches("a/1.png", "long hair"));
        assert_eq!(back.is_nsfw("a/2.png"), Some(true));

        // A future/unknown per-entry version and an entry missing the field both parse; the query
        // helpers keep working and the mismatched-version entry reports as needing a re-tag.
        let mixed = r#"{"entries":{
            "future":{"version":999,"general":[{"name":"1girl","prob":0.9}],"rating":{"name":"questionable","prob":0.7}},
            "legacy":{"general":[{"name":"solo","prob":0.5}]}
        }}"#;
        let idx: TagIndex = serde_json::from_str(mixed).unwrap();
        assert!(!idx.is_empty());
        assert!(idx.matches("future", "1girl"));
        assert_eq!(idx.is_nsfw("future"), Some(true));
        assert_eq!(idx.is_nsfw("legacy"), Some(false));
        // Neither is the current schema version, so both are treated as absent for pump purposes.
        assert!(!idx.contains("future"));
        assert!(!idx.contains("legacy"));
    }
}
