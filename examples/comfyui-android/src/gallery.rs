//! Gallery presentation state: how listed items bucket into collapsing headers, and the decoded
//! thumbnail cache behind the tiles.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::types::{GalleryGroup, GalleryItem};

/// One collapsing header's worth of items, as indices into the listing.
pub struct Group {
    /// Stable id for the header's `id_salt` (the label can repeat across groups).
    pub key: String,
    pub label: String,
    pub items: Vec<usize>,
}

/// Bucket a listing into headers, preserving the server's order.
///
/// The server only *orders* rows to match `group`; it sends no bucket keys, so the split happens
/// here. Grouping is by first appearance rather than a sort, so a key the server interleaves stays
/// one group instead of fragmenting.
pub fn group_items(items: &[GalleryItem], group: GalleryGroup) -> Vec<Group> {
    if group == GalleryGroup::None || items.is_empty() {
        return vec![Group {
            key: "all".to_string(),
            label: String::new(),
            items: (0..items.len()).collect(),
        }];
    }
    let mut groups: Vec<Group> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for (i, item) in items.iter().enumerate() {
        let key = match group {
            GalleryGroup::Model => item.model_label(),
            _ => item.subfolder.clone(),
        };
        match index.get(&key) {
            Some(&g) => groups[g].items.push(i),
            None => {
                index.insert(key.clone(), groups.len());
                let label = match group {
                    GalleryGroup::Model => item.model_label(),
                    _ => item.group_label(),
                };
                groups.push(Group { key, label, items: vec![i] });
            }
        }
    }
    groups
}

/// Decoded thumbnails, evicted oldest-first against a memory budget.
///
/// The budget is in bytes rather than a texture count because the column control swings tile size
/// by an order of magnitude: a 320px thumb is ~0.4 MB but a one-column 1024px read is ~4 MB, so a
/// count that is comfortable for the grid would be gigabytes at full width.
pub struct ThumbCache {
    textures: HashMap<String, egui::TextureHandle>,
    /// Insertion order for eviction, alongside each entry's byte cost.
    order: VecDeque<(String, usize)>,
    bytes: usize,
    pending: HashSet<String>,
}

/// Roughly 16 full-width tiles, or ~150 grid tiles.
const BUDGET_BYTES: usize = 64 * 1024 * 1024;

impl Default for ThumbCache {
    fn default() -> Self {
        Self {
            textures: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            pending: HashSet::new(),
        }
    }
}

impl ThumbCache {
    pub fn get(&self, key: &str) -> Option<&egui::TextureHandle> {
        self.textures.get(key)
    }

    /// Claim a fetch for `key`, returning whether the caller should issue the request. Prevents a
    /// tile that stays on screen for many frames from queueing a request per frame.
    pub fn claim(&mut self, key: &str) -> bool {
        !self.textures.contains_key(key) && self.pending.insert(key.to_string())
    }

    /// Drop in-flight claims so failed fetches are retried on the next refresh.
    pub fn reset_pending(&mut self) {
        self.pending.clear();
    }

    pub fn insert(&mut self, key: String, tex: egui::TextureHandle, bytes: usize) {
        self.pending.remove(&key);
        if self.textures.insert(key.clone(), tex).is_none() {
            self.order.push_back((key, bytes));
            self.bytes += bytes;
        }
        while self.bytes > BUDGET_BYTES && self.order.len() > 1 {
            let Some((old, cost)) = self.order.pop_front() else { break };
            self.textures.remove(&old);
            self.bytes = self.bytes.saturating_sub(cost);
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.textures.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(sub: &str, file: &str, models: &[&str]) -> GalleryItem {
        GalleryItem {
            subfolder: sub.into(),
            filename: file.into(),
            size: 0,
            is_video: false,
            has_workflow: false,
            models: models.iter().map(|m| m.to_string()).collect(),
        }
    }

    #[test]
    fn groups_by_folder_preserving_server_order() {
        let items = vec![
            item("u1/a", "1.png", &[]),
            item("u1/b", "2.png", &[]),
            item("u1/a", "3.png", &[]),
        ];
        let groups = group_items(&items, GalleryGroup::Folder);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].label, "a");
        // The interleaved third item rejoins its group rather than starting a new one.
        assert_eq!(groups[0].items, vec![0, 2]);
        assert_eq!(groups[1].items, vec![1]);
    }

    #[test]
    fn groups_by_model_including_multi_model_and_missing() {
        let items = vec![
            item("u1/a", "1.png", &["sdxl.safetensors"]),
            item("u1/a", "2.png", &["sdxl.safetensors", "refiner.safetensors"]),
            item("u1/a", "3.png", &[]),
            item("u1/b", "4.png", &["sdxl.safetensors"]),
        ];
        let groups = group_items(&items, GalleryGroup::Model);
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].label, "sdxl.safetensors");
        // Across folders, same model, one group.
        assert_eq!(groups[0].items, vec![0, 3]);
        // A multi-model image buckets by its combination, matching the server's ordering.
        assert_eq!(groups[1].label, "sdxl.safetensors + refiner.safetensors");
        // Non-PNG / unscraped files carry no models at all and must still land somewhere.
        assert_eq!(groups[2].label, "No model recorded");
    }

    #[test]
    fn no_grouping_yields_one_flat_group() {
        let items = vec![item("u1/a", "1.png", &[]), item("u1/b", "2.png", &[])];
        let groups = group_items(&items, GalleryGroup::None);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].items, vec![0, 1]);
    }

    #[test]
    fn empty_listing_groups_cleanly() {
        assert_eq!(group_items(&[], GalleryGroup::Folder).len(), 1);
        assert!(group_items(&[], GalleryGroup::Folder)[0].items.is_empty());
    }

    /// A tile is fetched once, not once per frame it stays visible.
    #[test]
    fn claim_is_single_shot() {
        let mut c = ThumbCache::default();
        assert!(c.claim("a#320"));
        assert!(!c.claim("a#320"));
        c.reset_pending();
        assert!(c.claim("a#320"));
    }

    #[test]
    fn eviction_is_by_bytes_not_count() {
        let ctx = egui::Context::default();
        let tex = |name: &str| {
            ctx.load_texture(name, egui::ColorImage::filled([1, 1], egui::Color32::RED), egui::TextureOptions::LINEAR)
        };
        let mut c = ThumbCache::default();
        // Ten 4 MB entries fit; a count-based cap would never trigger here.
        for i in 0..10 {
            c.insert(format!("k{i}"), tex("t"), 4 * 1024 * 1024);
        }
        assert_eq!(c.len(), 10);
        // One oversized insert must evict rather than blow the budget.
        c.insert("big".into(), tex("t"), BUDGET_BYTES);
        assert!(c.len() < 11, "expected eviction, kept {}", c.len());
        assert!(c.get("big").is_some(), "the newest entry must survive");
        assert!(c.get("k0").is_none(), "the oldest entry should go first");
    }

    /// Re-inserting a cached key must not double-count its bytes and slowly starve the cache.
    #[test]
    fn reinsert_does_not_leak_budget() {
        let ctx = egui::Context::default();
        let tex = ctx.load_texture("t", egui::ColorImage::filled([1, 1], egui::Color32::RED), egui::TextureOptions::LINEAR);
        let mut c = ThumbCache::default();
        for _ in 0..50 {
            c.insert("same".into(), tex.clone(), 4 * 1024 * 1024);
        }
        assert_eq!(c.len(), 1);
        assert_eq!(c.bytes, 4 * 1024 * 1024);
    }
}
