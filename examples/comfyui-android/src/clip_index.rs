//! Persistent CLIP embedding index: gallery item key -> L2-normalized embedding + optional
//! aesthetic score. Binary format (JSON would be ~40MB at 10k images): magic "CIDX", u32 version,
//! then per entry u32 key_len, key utf8, u32 dim, dim f32 LE, f32 score (NaN = none).

use std::collections::HashMap;

const MAGIC: &[u8; 4] = b"CIDX";
pub const SCHEMA_VERSION: u32 = 1;

pub struct ClipEntry {
    pub key: String,
    pub emb: Vec<f32>,
    pub score: Option<f32>,
}

#[derive(Default)]
pub struct ClipIndex {
    entries: Vec<ClipEntry>,
    by_key: HashMap<String, usize>,
}

impl ClipIndex {
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn contains(&self, key: &str) -> bool {
        self.by_key.contains_key(key)
    }

    pub fn score(&self, key: &str) -> Option<f32> {
        self.by_key.get(key).and_then(|&i| self.entries[i].score)
    }

    /// Insert or replace one entry.
    pub fn insert(&mut self, key: String, emb: Vec<f32>, score: Option<f32>) {
        match self.by_key.get(&key) {
            Some(&i) => {
                self.entries[i].emb = emb;
                self.entries[i].score = score;
            }
            None => {
                self.by_key.insert(key.clone(), self.entries.len());
                self.entries.push(ClipEntry { key, emb, score });
            }
        }
    }

    /// Keys most similar to `key` by dot product (embeddings are L2-normalized), best first.
    pub fn top_similar(&self, key: &str, n: usize) -> Vec<(String, f32)> {
        let Some(&at) = self.by_key.get(key) else { return Vec::new() };
        let probe = &self.entries[at].emb;
        let mut scored: Vec<(String, f32)> = self
            .entries
            .iter()
            .enumerate()
            .filter(|&(i, e)| i != at && e.emb.len() == probe.len())
            .map(|(_, e)| (e.key.clone(), e.emb.iter().zip(probe).map(|(a, b)| a * b).sum()))
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        scored.truncate(n);
        scored
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16 + self.entries.len() * 2100);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&SCHEMA_VERSION.to_le_bytes());
        for e in &self.entries {
            out.extend_from_slice(&(e.key.len() as u32).to_le_bytes());
            out.extend_from_slice(e.key.as_bytes());
            out.extend_from_slice(&(e.emb.len() as u32).to_le_bytes());
            for &v in &e.emb {
                out.extend_from_slice(&v.to_le_bytes());
            }
            out.extend_from_slice(&e.score.unwrap_or(f32::NAN).to_le_bytes());
        }
        out
    }

    /// Parse; wrong magic/version or a truncated tail yields what parsed so far.
    pub fn from_bytes(bytes: &[u8]) -> Self {
        let mut idx = Self::default();
        if bytes.len() < 8 || &bytes[0..4] != MAGIC {
            return idx;
        }
        if u32::from_le_bytes(bytes[4..8].try_into().unwrap()) != SCHEMA_VERSION {
            return idx;
        }
        let mut at = 8usize;
        let take = |at: &mut usize, n: usize| -> Option<&[u8]> {
            let s = bytes.get(*at..*at + n)?;
            *at += n;
            Some(s)
        };
        while at < bytes.len() {
            let Some(kl) = take(&mut at, 4) else { break };
            let kl = u32::from_le_bytes(kl.try_into().unwrap()) as usize;
            let Some(key) = take(&mut at, kl).and_then(|b| std::str::from_utf8(b).ok()) else {
                break;
            };
            let key = key.to_string();
            let Some(dim) = take(&mut at, 4) else { break };
            let dim = u32::from_le_bytes(dim.try_into().unwrap()) as usize;
            let Some(raw) = take(&mut at, dim * 4) else { break };
            let emb: Vec<f32> =
                raw.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect();
            let Some(s) = take(&mut at, 4) else { break };
            let s = f32::from_le_bytes(s.try_into().unwrap());
            idx.insert(key, emb, (!s.is_nan()).then_some(s));
        }
        idx
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(dir: (f32, f32)) -> Vec<f32> {
        let n = (dir.0 * dir.0 + dir.1 * dir.1).sqrt();
        vec![dir.0 / n, dir.1 / n]
    }

    #[test]
    fn round_trip_preserves_entries_and_scores() {
        let mut idx = ClipIndex::default();
        idx.insert("a".into(), unit((1.0, 0.0)), Some(5.5));
        idx.insert("b".into(), unit((0.0, 1.0)), None);
        let back = ClipIndex::from_bytes(&idx.to_bytes());
        assert_eq!(back.len(), 2);
        assert_eq!(back.score("a"), Some(5.5));
        assert_eq!(back.score("b"), None);
        assert!(back.contains("b"));
    }

    #[test]
    fn top_similar_orders_by_dot_and_skips_self() {
        let mut idx = ClipIndex::default();
        idx.insert("probe".into(), unit((1.0, 0.0)), None);
        idx.insert("near".into(), unit((0.9, 0.1)), None);
        idx.insert("far".into(), unit((0.0, 1.0)), None);
        let got = idx.top_similar("probe", 5);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, "near");
        assert_eq!(got[1].0, "far");
        assert!(got[0].1 > got[1].1);
        assert!(idx.top_similar("missing", 5).is_empty());
    }

    #[test]
    fn insert_replaces_in_place() {
        let mut idx = ClipIndex::default();
        idx.insert("a".into(), unit((1.0, 0.0)), Some(1.0));
        idx.insert("a".into(), unit((0.0, 1.0)), Some(2.0));
        assert_eq!(idx.len(), 1);
        assert_eq!(idx.score("a"), Some(2.0));
    }

    #[test]
    fn junk_and_truncation_degrade_to_parsed_prefix() {
        assert_eq!(ClipIndex::from_bytes(b"nope").len(), 0);
        let mut idx = ClipIndex::default();
        idx.insert("a".into(), unit((1.0, 0.0)), Some(1.0));
        idx.insert("b".into(), unit((0.0, 1.0)), None);
        let mut bytes = idx.to_bytes();
        bytes.truncate(bytes.len() - 3);
        let back = ClipIndex::from_bytes(&bytes);
        assert_eq!(back.len(), 1);
        assert!(back.contains("a"));
    }
}
