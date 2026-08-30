//! Finding one alpha among hundreds.
//!
//! `MAX_LIBRARY_ENTRIES` is 1024 and the Alphas grid is a flat scroll with a
//! substring filter. Generating variants makes that worse fast, so the cheapest
//! thing that keeps the library usable is knowing which entries are near-copies
//! of each other and being able to ask for "more like this one".
//!
//! The ranking is pure and tested here; only the embedding itself needs the
//! NPU, and that sits behind `local-npu`. Without the feature — or without a
//! pack — the cache stays empty and every entry point degrades to the substring
//! filter the app already had.

use std::collections::HashMap;

/// Cosine distance below which two entries are the same picture for our
/// purposes.
///
/// CLIP puts genuinely different textures well below 0.9; this is deliberately
/// strict, because a false "you already have this" is worse than a miss — it
/// would talk someone out of an alpha they wanted.
pub const NEAR_DUPLICATE: f32 = 0.95;

/// L2-normalized embeddings by library entry name.
///
/// Keyed by name *and* stamped with the content hash the embedding was taken
/// from, so regenerating the builtins at a new size invalidates rather than
/// silently comparing an old vector to a new picture.
#[derive(Default)]
pub struct Embeddings {
    by_name: HashMap<String, (u64, Vec<f32>)>,
}

impl Embeddings {
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    pub fn insert(&mut self, name: &str, content: u64, emb: Vec<f32>) {
        self.by_name.insert(name.to_string(), (content, emb));
    }

    /// The stored embedding, if it was taken from this same content.
    pub fn get(&self, name: &str, content: u64) -> Option<&[f32]> {
        self.by_name.get(name).filter(|(c, _)| *c == content).map(|(_, e)| e.as_slice())
    }

    pub fn forget(&mut self, name: &str) {
        self.by_name.remove(name);
    }

    pub fn clear(&mut self) {
        self.by_name.clear();
    }

    /// Every entry ranked against `query`, most similar first.
    ///
    /// Excludes `skip` so "more like this" does not put the thing itself at the
    /// top of its own results.
    pub fn rank(&self, query: &[f32], skip: Option<&str>) -> Vec<(String, f32)> {
        let mut out: Vec<(String, f32)> = self
            .by_name
            .iter()
            .filter(|(n, _)| skip != Some(n.as_str()))
            .map(|(n, (_, e))| (n.clone(), cosine(query, e)))
            .collect();
        out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        out
    }

    /// The nearest existing entry to `query`, when it is near enough to call a
    /// duplicate.
    pub fn near_duplicate(&self, query: &[f32], skip: Option<&str>) -> Option<(String, f32)> {
        self.rank(query, skip).into_iter().next().filter(|(_, s)| *s >= NEAR_DUPLICATE)
    }
}

/// Cosine of two L2-normalized vectors.
///
/// Guards the length mismatch rather than panicking: a pack swapped for one
/// with a different embedding width would otherwise take the app down on the
/// first comparison.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// A cheap content hash for an alpha, so a regenerated one re-embeds.
pub fn content_hash(a: &ringdesign_core::alpha::Alpha) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::hash::DefaultHasher::new();
    (a.width, a.height).hash(&mut h);
    // Every 37th texel: enough to catch a regenerated or repainted alpha
    // without walking a quarter-million floats on every library change.
    for v in a.data.iter().step_by(37) {
        v.to_bits().hash(&mut h);
    }
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(v: &[f32]) -> Vec<f32> {
        let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
        v.iter().map(|x| x / n).collect()
    }

    #[test]
    fn cosine_is_one_for_a_vector_with_itself() {
        let a = unit(&[1.0, 2.0, 3.0]);
        assert!((cosine(&a, &a) - 1.0).abs() < 1e-6);
    }

    /// A pack swapped for one with a different embedding width must not take
    /// the app down on the first comparison.
    #[test]
    fn a_length_mismatch_scores_zero_rather_than_panicking() {
        assert_eq!(cosine(&[1.0, 0.0], &[1.0, 0.0, 0.0]), 0.0);
        assert_eq!(cosine(&[], &[]), 0.0);
    }

    #[test]
    fn ranking_puts_the_most_similar_first_and_can_skip_itself() {
        let mut e = Embeddings::default();
        e.insert("same", 1, unit(&[1.0, 0.0, 0.0]));
        e.insert("near", 1, unit(&[0.9, 0.1, 0.0]));
        e.insert("far", 1, unit(&[0.0, 0.0, 1.0]));
        let q = unit(&[1.0, 0.0, 0.0]);

        let all = e.rank(&q, None);
        assert_eq!(all[0].0, "same");
        assert_eq!(all[2].0, "far");

        let others = e.rank(&q, Some("same"));
        assert_eq!(others[0].0, "near", "the query itself is excluded");
        assert_eq!(others.len(), 2);
    }

    #[test]
    fn a_near_duplicate_is_only_reported_above_the_threshold() {
        let mut e = Embeddings::default();
        e.insert("twin", 1, unit(&[1.0, 0.02, 0.0]));
        let q = unit(&[1.0, 0.0, 0.0]);
        assert_eq!(e.near_duplicate(&q, None).map(|(n, _)| n), Some("twin".into()));

        let mut f = Embeddings::default();
        f.insert("cousin", 1, unit(&[0.6, 0.8, 0.0]));
        assert!(f.near_duplicate(&q, None).is_none(), "0.6 is not a duplicate");
    }

    /// Regenerating the builtins at a new size changes the picture; comparing a
    /// stale vector to it would report confident nonsense.
    #[test]
    fn a_changed_content_hash_invalidates_the_stored_embedding() {
        let mut e = Embeddings::default();
        e.insert("rope", 111, unit(&[1.0, 0.0]));
        assert!(e.get("rope", 111).is_some());
        assert!(e.get("rope", 222).is_none(), "a regenerated alpha must re-embed");
    }

    #[test]
    fn the_content_hash_moves_with_the_picture_and_the_size() {
        use ringdesign_core::alpha::Alpha;
        let a = Alpha::new("a", 8, 8, vec![0.5; 64]);
        let same = Alpha::new("a", 8, 8, vec![0.5; 64]);
        let mut other = vec![0.5; 64];
        other[0] = 0.9;
        let changed = Alpha::new("a", 8, 8, other);
        let resized = Alpha::new("a", 16, 4, vec![0.5; 64]);

        assert_eq!(content_hash(&a), content_hash(&same));
        assert_ne!(content_hash(&a), content_hash(&changed));
        assert_ne!(content_hash(&a), content_hash(&resized), "same texels, different shape");
    }

    #[test]
    fn forgetting_and_clearing_do_what_they_say() {
        let mut e = Embeddings::default();
        e.insert("a", 1, vec![1.0]);
        e.insert("b", 1, vec![1.0]);
        assert_eq!(e.len(), 2);
        e.forget("a");
        assert_eq!(e.len(), 1);
        e.clear();
        assert!(e.is_empty());
    }
}
