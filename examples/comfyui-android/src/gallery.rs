//! Gallery presentation state: how listed items bucket into collapsing headers, and the decoded
//! thumbnail cache behind the tiles.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::types::{GalleryGroup, GalleryItem};

/// One LoRA referenced by a gallery image's embedded workflow.
#[derive(Clone, Debug, Default)]
pub struct LoraMeta {
    pub name: String,
    pub strength_model: f64,
    pub strength_clip: Option<f64>,
}

/// Prompt / model summary scraped from an embedded workflow for the viewer header.
#[derive(Clone, Debug, Default)]
pub struct ImageMeta {
    pub models: Vec<String>,
    pub loras: Vec<LoraMeta>,
    pub positive: Option<String>,
    pub negative: Option<String>,
    pub sampler: Option<String>,
    pub scheduler: Option<String>,
    pub steps: Option<u64>,
    pub cfg: Option<f64>,
    pub seed: Option<i64>,
}

impl ImageMeta {
    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
            && self.loras.is_empty()
            && self.positive.is_none()
            && self.negative.is_none()
            && self.sampler.is_none()
    }
}

/// Pull models / LoRAs / prompts / sampler settings out of API- or UI-format workflow JSON.
#[cfg_attr(target_os = "android", allow(dead_code))]
pub fn parse_workflow_meta(raw: &str) -> ImageMeta {
    parse_workflow_meta_for(raw, None)
}

/// Like [`parse_workflow_meta`], but when `filename` is set, prefer the SaveImage column that
/// produced that file (multi-checkpoint / LoRA-matrix benches).
pub fn parse_workflow_meta_for(raw: &str, filename: Option<&str>) -> ImageMeta {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return ImageMeta::default();
    };
    let value = unwrap_workflow_root(value);
    if value.get("nodes").is_some() {
        parse_ui_meta(&value, filename)
    } else if value.as_object().is_some() {
        parse_api_meta(&value, filename)
    } else {
        ImageMeta::default()
    }
}

/// Prefer the API `prompt` graph (class_type + slots), else UI `workflow`, else the value itself.
fn unwrap_workflow_root(value: Value) -> Value {
    let looks_api = |v: &Value| -> bool {
        v.as_object().is_some_and(|o| o.values().any(|n| n.get("class_type").is_some()))
    };
    if let Some(p) = value.get("prompt").filter(|p| looks_api(p)) {
        return p.clone();
    }
    if let Some(w) = value.get("workflow") {
        return w.clone();
    }
    // Some gallery endpoints wrap again: `{ "data": { "prompt": … } }`.
    if let Some(inner) = value.get("data").cloned() {
        return unwrap_workflow_root(inner);
    }
    value
}

fn parse_api_meta(root: &Value, filename: Option<&str>) -> ImageMeta {
    let Some(nodes) = root.as_object() else {
        return ImageMeta::default();
    };
    let keep = filename.and_then(|f| api_save_subgraph(nodes, f));
    fill_api_meta(nodes, keep.as_ref())
}

fn parse_ui_meta(root: &Value, filename: Option<&str>) -> ImageMeta {
    let Some(nodes_arr) = root.get("nodes").and_then(Value::as_array) else {
        return ImageMeta::default();
    };
    let links = root.get("links").and_then(Value::as_array).cloned().unwrap_or_default();
    let by_id: HashMap<u64, &Value> = nodes_arr
        .iter()
        .filter_map(|n| Some((n.get("id")?.as_u64()?, n)))
        .collect();
    let link_src: HashMap<u64, u64> = links
        .iter()
        .filter_map(|l| {
            let a = l.as_array()?;
            Some((a.first()?.as_u64()?, a.get(1)?.as_u64()?))
        })
        .collect();

    let keep = filename.and_then(|f| ui_save_subgraph(&by_id, &link_src, f));
    fill_ui_meta(&by_id, &link_src, keep.as_ref())
}

/// Node ids reachable walking inputs backward from the SaveImage matching `filename`.
fn api_save_subgraph(
    nodes: &serde_json::Map<String, Value>,
    filename: &str,
) -> Option<HashSet<String>> {
    let start = nodes.iter().find_map(|(id, n)| {
        let class = n.get("class_type").and_then(Value::as_str)?;
        if class != "SaveImage" && class != "SaveImageWebsocket" {
            return None;
        }
        let prefix = str_in(n.get("inputs")?, "filename_prefix")?;
        save_prefix_matches(&prefix, filename).then_some(id.clone())
    })?;
    let mut keep = HashSet::new();
    let mut stack = vec![start];
    while let Some(id) = stack.pop() {
        if !keep.insert(id.clone()) {
            continue;
        }
        let Some(node) = nodes.get(&id) else { continue };
        let Some(inputs) = node.get("inputs").and_then(Value::as_object) else {
            continue;
        };
        for v in inputs.values() {
            if let Some(src) = link_node_id(v) {
                stack.push(src);
            }
        }
    }
    Some(keep)
}

fn ui_save_subgraph(
    by_id: &HashMap<u64, &Value>,
    link_src: &HashMap<u64, u64>,
    filename: &str,
) -> Option<HashSet<u64>> {
    let start = by_id.iter().find_map(|(&id, n)| {
        let class = n.get("type").and_then(Value::as_str)?;
        if class != "SaveImage" {
            return None;
        }
        let widgets = n.get("widgets_values")?;
        let prefix = widget_str(widgets, 0)?;
        save_prefix_matches(&prefix, filename).then_some(id)
    })?;
    let mut keep = HashSet::new();
    let mut stack = vec![start];
    while let Some(id) = stack.pop() {
        if !keep.insert(id) {
            continue;
        }
        let Some(node) = by_id.get(&id) else { continue };
        for inp in node.get("inputs").and_then(Value::as_array).into_iter().flatten() {
            if let Some(lid) = inp.get("link").and_then(Value::as_u64)
                && let Some(&src) = link_src.get(&lid)
            {
                stack.push(src);
            }
        }
    }
    Some(keep)
}

fn save_prefix_matches(prefix: &str, filename: &str) -> bool {
    let stem = filename
        .rsplit_once('.')
        .map(|(s, _)| s)
        .unwrap_or(filename);
    let leaf = prefix.replace('\\', "/");
    let leaf = leaf.rsplit('/').next().unwrap_or(leaf.as_str());
    !leaf.is_empty() && (stem.starts_with(leaf) || stem.contains(leaf))
}

fn fill_api_meta(
    nodes: &serde_json::Map<String, Value>,
    keep: Option<&HashSet<String>>,
) -> ImageMeta {
    let mut meta = ImageMeta::default();
    // Prefer KSampler (2) over FaceDetailer (1) when both feed the same SaveImage.
    let mut best_sampler: Option<(u8, String)> = None;

    for (id, node) in nodes {
        if keep.is_some_and(|k| !k.contains(id)) {
            continue;
        }
        let class = node.get("class_type").and_then(Value::as_str).unwrap_or("");
        let inputs = node.get("inputs").cloned().unwrap_or(Value::Null);
        match class {
            "CheckpointLoaderSimple" | "CheckpointLoader" => {
                if let Some(n) = str_in(&inputs, "ckpt_name") {
                    push_unique(&mut meta.models, n);
                }
            }
            "UNETLoader" => {
                if let Some(n) = str_in(&inputs, "unet_name") {
                    push_unique(&mut meta.models, n);
                }
            }
            "LoraLoader" | "LoraLoaderModelOnly" => {
                if let Some(name) = str_in(&inputs, "lora_name") {
                    meta.loras.push(LoraMeta {
                        name,
                        strength_model: num_in(&inputs, "strength_model").unwrap_or(1.0),
                        strength_clip: num_in(&inputs, "strength_clip"),
                    });
                }
            }
            "KSampler" | "KSamplerAdvanced" | "SamplerCustom" | "SamplerCustomAdvanced" => {
                if best_sampler.as_ref().map(|(p, _)| *p).unwrap_or(0) < 2 {
                    best_sampler = Some((2, id.clone()));
                    meta.sampler = str_in(&inputs, "sampler_name");
                    meta.scheduler = str_in(&inputs, "scheduler");
                    meta.steps = num_in(&inputs, "steps").map(|n| n as u64);
                    meta.cfg = num_in(&inputs, "cfg");
                    meta.seed = num_in(&inputs, "seed")
                        .or_else(|| num_in(&inputs, "noise_seed"))
                        .map(|n| n as i64);
                    meta.positive = api_resolve_text(nodes, &inputs, "positive", 0);
                    meta.negative = api_resolve_text(nodes, &inputs, "negative", 0);
                }
            }
            "FaceDetailer" => {
                if best_sampler.is_none() {
                    best_sampler = Some((1, id.clone()));
                    meta.sampler = str_in(&inputs, "sampler_name");
                    meta.scheduler = str_in(&inputs, "scheduler");
                    meta.steps = num_in(&inputs, "steps").map(|n| n as u64);
                    meta.cfg = num_in(&inputs, "cfg");
                    meta.seed = num_in(&inputs, "seed")
                        .or_else(|| num_in(&inputs, "noise_seed"))
                        .map(|n| n as i64);
                    meta.positive = api_resolve_text(nodes, &inputs, "positive", 0);
                    meta.negative = api_resolve_text(nodes, &inputs, "negative", 0);
                }
            }
            _ => {
                if class.to_ascii_lowercase().contains("lora") {
                    if let Some(name) =
                        str_in(&inputs, "lora_name").or_else(|| str_in(&inputs, "lora"))
                    {
                        meta.loras.push(LoraMeta {
                            name,
                            strength_model: num_in(&inputs, "strength_model")
                                .or_else(|| num_in(&inputs, "strength"))
                                .unwrap_or(1.0),
                            strength_clip: num_in(&inputs, "strength_clip"),
                        });
                    }
                }
            }
        }
    }

    if meta.positive.is_none() || meta.negative.is_none() {
        // Fallback: longest resolved CLIP texts in scope.
        let mut vals = Vec::new();
        for (id, node) in nodes {
            if keep.is_some_and(|k| !k.contains(id)) {
                continue;
            }
            let class = node.get("class_type").and_then(Value::as_str).unwrap_or("");
            if matches!(
                class,
                "CLIPTextEncode" | "CLIPTextEncodeSDXL" | "CLIPTextEncodeFlux"
            ) {
                if let Some(t) = api_node_text(nodes, id, 0).filter(|s| !s.trim().is_empty()) {
                    vals.push(t);
                }
            }
        }
        vals.sort_by_key(|t| std::cmp::Reverse(t.len()));
        if meta.positive.is_none() {
            meta.positive = vals.first().cloned();
        }
        if meta.negative.is_none() {
            meta.negative = vals.get(1).cloned();
        }
    }
    meta
}

fn fill_ui_meta(
    by_id: &HashMap<u64, &Value>,
    link_src: &HashMap<u64, u64>,
    keep: Option<&HashSet<u64>>,
) -> ImageMeta {
    let mut meta = ImageMeta::default();
    let mut best_sampler: Option<(u8, u64)> = None;

    for (&id, node) in by_id {
        if keep.is_some_and(|k| !k.contains(&id)) {
            continue;
        }
        let class = node
            .get("type")
            .or_else(|| node.get("class_type"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let widgets = node.get("widgets_values").cloned().unwrap_or(Value::Null);
        match class {
            "CheckpointLoaderSimple" | "CheckpointLoader" => {
                if let Some(n) = widget_str(&widgets, 0) {
                    push_unique(&mut meta.models, n);
                }
            }
            "UNETLoader" => {
                if let Some(n) = widget_str(&widgets, 0) {
                    push_unique(&mut meta.models, n);
                }
            }
            "LoraLoader" | "LoraLoaderModelOnly" => {
                if let Some(name) = widget_str(&widgets, 0) {
                    meta.loras.push(LoraMeta {
                        name,
                        strength_model: widget_num(&widgets, 1).unwrap_or(1.0),
                        strength_clip: widget_num(&widgets, 2),
                    });
                }
            }
            "KSampler" | "KSamplerAdvanced" => {
                if best_sampler.as_ref().map(|(p, _)| *p).unwrap_or(0) < 2 {
                    best_sampler = Some((2, id));
                    meta.seed = widget_num(&widgets, 0).map(|n| n as i64);
                    meta.steps = widget_num(&widgets, 2).map(|n| n as u64);
                    meta.cfg = widget_num(&widgets, 3);
                    meta.sampler = widget_str(&widgets, 4);
                    meta.scheduler = widget_str(&widgets, 5);
                    meta.positive = ui_input_text(by_id, link_src, id, "positive", 0);
                    meta.negative = ui_input_text(by_id, link_src, id, "negative", 0);
                }
            }
            "FaceDetailer" => {
                if best_sampler.is_none() {
                    best_sampler = Some((1, id));
                    meta.seed = widget_num(&widgets, 0).map(|n| n as i64);
                    meta.steps = widget_num(&widgets, 2).map(|n| n as u64);
                    meta.cfg = widget_num(&widgets, 3);
                    meta.sampler = widget_str(&widgets, 4);
                    meta.scheduler = widget_str(&widgets, 5);
                    meta.positive = ui_input_text(by_id, link_src, id, "positive", 0);
                    meta.negative = ui_input_text(by_id, link_src, id, "negative", 0);
                }
            }
            _ => {
                if class.to_ascii_lowercase().contains("lora")
                    && let Some(name) = widget_str(&widgets, 0)
                {
                    meta.loras.push(LoraMeta {
                        name,
                        strength_model: widget_num(&widgets, 1).unwrap_or(1.0),
                        strength_clip: widget_num(&widgets, 2),
                    });
                }
            }
        }
    }

    if meta.positive.is_none() || meta.negative.is_none() {
        let mut vals = Vec::new();
        for (&id, node) in by_id {
            if keep.is_some_and(|k| !k.contains(&id)) {
                continue;
            }
            let class = node.get("type").and_then(Value::as_str).unwrap_or("");
            if matches!(
                class,
                "CLIPTextEncode" | "CLIPTextEncodeSDXL" | "CLIPTextEncodeFlux"
            ) {
                if let Some(t) = ui_node_text(by_id, link_src, id, 0).filter(|s| !s.trim().is_empty())
                {
                    vals.push(t);
                }
            }
        }
        vals.sort_by_key(|t| std::cmp::Reverse(t.len()));
        if meta.positive.is_none() {
            meta.positive = vals.first().cloned();
        }
        if meta.negative.is_none() {
            meta.negative = vals.get(1).cloned();
        }
    }
    meta
}

fn link_node_id(v: &Value) -> Option<String> {
    match v {
        Value::Array(a) => a.first().and_then(Value::as_str).map(str::to_string)
            .or_else(|| a.first().and_then(Value::as_u64).map(|n| n.to_string())),
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

fn api_resolve_text(
    nodes: &serde_json::Map<String, Value>,
    inputs: &Value,
    key: &str,
    depth: u8,
) -> Option<String> {
    let slot = inputs.get(key)?;
    let id = link_node_id(slot)?;
    api_node_text(nodes, &id, depth)
}

fn api_node_text(nodes: &serde_json::Map<String, Value>, id: &str, depth: u8) -> Option<String> {
    if depth > 24 {
        return None;
    }
    let node = nodes.get(id)?;
    let class = node.get("class_type").and_then(Value::as_str).unwrap_or("");
    let inputs = node.get("inputs").cloned().unwrap_or(Value::Null);
    match class {
        "CLIPTextEncode" | "CLIPTextEncodeSDXL" | "CLIPTextEncodeFlux" => {
            if let Some(t) = str_in(&inputs, "text").filter(|s| !s.is_empty()) {
                return Some(t);
            }
            // Text linked from StringConcatenate / Primitive.
            api_resolve_text(nodes, &inputs, "text", depth + 1)
        }
        "StringConcatenate" | "ConcatString" | "Text Concatenate" => {
            let a = str_in(&inputs, "string_a")
                .or_else(|| api_resolve_text(nodes, &inputs, "string_a", depth + 1))
                .unwrap_or_default();
            let b = str_in(&inputs, "string_b")
                .or_else(|| api_resolve_text(nodes, &inputs, "string_b", depth + 1))
                .unwrap_or_default();
            Some(format!("{a}{b}"))
        }
        "Reroute" => {
            let next = inputs.as_object()?.values().next()?;
            api_node_text(nodes, &link_node_id(next)?, depth + 1)
        }
        _ if class.contains("Primitive") || class.contains("String") || class.contains("Text") => {
            str_in(&inputs, "value")
                .or_else(|| str_in(&inputs, "string"))
                .or_else(|| str_in(&inputs, "text"))
        }
        _ => None,
    }
}

fn ui_input_text(
    by_id: &HashMap<u64, &Value>,
    link_src: &HashMap<u64, u64>,
    node_id: u64,
    input: &str,
    depth: u8,
) -> Option<String> {
    let node = *by_id.get(&node_id)?;
    let lid = node
        .get("inputs")
        .and_then(Value::as_array)?
        .iter()
        .find(|i| i.get("name").and_then(Value::as_str) == Some(input))?
        .get("link")
        .and_then(Value::as_u64)?;
    let src = *link_src.get(&lid)?;
    ui_node_text(by_id, link_src, src, depth)
}

fn ui_node_text(
    by_id: &HashMap<u64, &Value>,
    link_src: &HashMap<u64, u64>,
    id: u64,
    depth: u8,
) -> Option<String> {
    if depth > 24 {
        return None;
    }
    let node = *by_id.get(&id)?;
    let class = node.get("type").and_then(Value::as_str).unwrap_or("");
    let widgets = node.get("widgets_values").cloned().unwrap_or(Value::Null);
    match class {
        "CLIPTextEncode" | "CLIPTextEncodeSDXL" | "CLIPTextEncodeFlux" => {
            if let Some(t) = widget_str(&widgets, 0).filter(|s| !s.is_empty()) {
                return Some(t);
            }
            ui_input_text(by_id, link_src, id, "text", depth + 1)
        }
        "StringConcatenate" | "ConcatString" | "Text Concatenate" => {
            let a = ui_input_text(by_id, link_src, id, "string_a", depth + 1)
                .or_else(|| widget_str(&widgets, 0))
                .unwrap_or_default();
            let b = ui_input_text(by_id, link_src, id, "string_b", depth + 1)
                .or_else(|| widget_str(&widgets, 1))
                .unwrap_or_default();
            Some(format!("{a}{b}"))
        }
        "PrimitiveStringMultiline" | "PrimitiveString" | "StringLiteral" | "Text" => {
            widget_str(&widgets, 0)
        }
        "Reroute" => {
            let lid = node
                .get("inputs")
                .and_then(Value::as_array)?
                .first()?
                .get("link")
                .and_then(Value::as_u64)?;
            ui_node_text(by_id, link_src, *link_src.get(&lid)?, depth + 1)
        }
        _ => widget_str(&widgets, 0).filter(|s| !s.is_empty()),
    }
}

fn str_in(inputs: &Value, key: &str) -> Option<String> {
    inputs.get(key).and_then(Value::as_str).map(str::to_string)
}

fn num_in(inputs: &Value, key: &str) -> Option<f64> {
    inputs.get(key).and_then(|v| v.as_f64().or_else(|| v.as_i64().map(|n| n as f64)))
}

fn widget_str(widgets: &Value, idx: usize) -> Option<String> {
    widgets.as_array()?.get(idx)?.as_str().map(str::to_string)
}

fn widget_num(widgets: &Value, idx: usize) -> Option<f64> {
    let v = widgets.as_array()?.get(idx)?;
    v.as_f64().or_else(|| v.as_i64().map(|n| n as f64))
}

fn push_unique(list: &mut Vec<String>, value: String) {
    if !list.iter().any(|x| x == &value) {
        list.push(value);
    }
}

/// On-disk cache for full-resolution gallery images under `{documents}/gallery_full/`.
const FULL_CACHE_BUDGET: u64 = 256 * 1024 * 1024;

pub fn full_cache_dir(documents: &str) -> PathBuf {
    Path::new(documents).join("gallery_full")
}

fn full_cache_path(dir: &Path, key: &str) -> PathBuf {
    // Keep one file per image; flatten nested subfolders.
    let safe: String = key
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' })
        .collect();
    dir.join(safe)
}

/// Read a previously cached full image, or `None` on miss / IO error.
pub fn read_full_cache(documents: &str, key: &str) -> Option<Vec<u8>> {
    let path = full_cache_path(&full_cache_dir(documents), key);
    std::fs::read(path).ok().filter(|b| !b.is_empty())
}

/// Persist a full image and evict oldest files when the cache exceeds the budget.
pub fn write_full_cache(documents: &str, key: &str, bytes: &[u8]) {
    let dir = full_cache_dir(documents);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = full_cache_path(&dir, key);
    if std::fs::write(&path, bytes).is_err() {
        return;
    }
    evict_full_cache(&dir);
}

fn evict_full_cache(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();
    let mut total = 0u64;
    for ent in rd.flatten() {
        let Ok(meta) = ent.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let len = meta.len();
        let mtime = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        total += len;
        files.push((ent.path(), len, mtime));
    }
    if total <= FULL_CACHE_BUDGET {
        return;
    }
    files.sort_by_key(|(_, _, t)| *t);
    for (path, len, _) in files {
        if total <= FULL_CACHE_BUDGET {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(len);
        }
    }
}

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
// The UI always goes through `group_selected` now; this stays as the host-test entry point.
#[cfg_attr(target_os = "android", allow(dead_code))]
pub fn group_items(items: &[GalleryItem], group: GalleryGroup) -> Vec<Group> {
    let all: Vec<usize> = (0..items.len()).collect();
    group_selected(items, &all, group)
}

/// [`group_items`] over a subset: `sel` holds indices into `items` (e.g. after a media filter),
/// and the returned groups carry those same original indices.
pub fn group_selected(items: &[GalleryItem], sel: &[usize], group: GalleryGroup) -> Vec<Group> {
    if group == GalleryGroup::None || sel.is_empty() {
        return vec![Group {
            key: "all".to_string(),
            label: String::new(),
            items: sel.to_vec(),
        }];
    }
    let mut groups: Vec<Group> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for &i in sel {
        let item = &items[i];
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
    fn parse_api_workflow_meta_extracts_prompts_loras_model() {
        let raw = r#"{
            "1": {"class_type": "CheckpointLoaderSimple", "inputs": {"ckpt_name": "sdxl.safetensors"}},
            "2": {"class_type": "CLIPTextEncode", "inputs": {"text": "a cat", "clip": ["1", 1]}},
            "3": {"class_type": "CLIPTextEncode", "inputs": {"text": "blurry", "clip": ["1", 1]}},
            "4": {"class_type": "LoraLoader", "inputs": {
                "lora_name": "style.safetensors", "strength_model": 0.8, "strength_clip": 0.7,
                "model": ["1", 0], "clip": ["1", 1]
            }},
            "5": {"class_type": "KSampler", "inputs": {
                "seed": 42, "steps": 20, "cfg": 7.0, "sampler_name": "euler", "scheduler": "normal",
                "positive": ["2", 0], "negative": ["3", 0], "model": ["4", 0], "latent_image": ["1", 0]
            }}
        }"#;
        let m = parse_workflow_meta(raw);
        assert_eq!(m.models, vec!["sdxl.safetensors"]);
        assert_eq!(m.loras.len(), 1);
        assert_eq!(m.loras[0].name, "style.safetensors");
        assert!((m.loras[0].strength_model - 0.8).abs() < 1e-6);
        assert_eq!(m.positive.as_deref(), Some("a cat"));
        assert_eq!(m.negative.as_deref(), Some("blurry"));
        assert_eq!(m.sampler.as_deref(), Some("euler"));
        assert_eq!(m.steps, Some(20));
        assert_eq!(m.seed, Some(42));
    }

    #[test]
    fn parse_ui_concat_and_save_filename_scope() {
        // Minimal multi-column UI workflow: shared subject + per-column prefix via StringConcatenate.
        let raw = r#"{
            "nodes": [
                {"id": 2, "type": "PrimitiveStringMultiline", "widgets_values": ["a cat sitting"]},
                {"id": 3, "type": "PrimitiveStringMultiline", "widgets_values": ["blurry"]},
                {"id": 100, "type": "CheckpointLoaderSimple", "widgets_values": ["model_a.safetensors"],
                 "inputs": [], "outputs": [{"name": "MODEL", "links": [10]}, {"name": "CLIP", "links": [11]}, {"name": "VAE", "links": []}]},
                {"id": 102, "type": "StringConcatenate", "widgets_values": ["masterpiece, ", "", ""],
                 "inputs": [{"name": "string_b", "link": 2}], "outputs": [{"name": "STRING", "links": [4]}]},
                {"id": 103, "type": "CLIPTextEncode", "widgets_values": [""],
                 "inputs": [{"name": "text", "link": 4}, {"name": "clip", "link": 11}],
                 "outputs": [{"name": "CONDITIONING", "links": [5]}]},
                {"id": 104, "type": "CLIPTextEncode", "widgets_values": [""],
                 "inputs": [{"name": "text", "link": 3}, {"name": "clip", "link": 11}],
                 "outputs": [{"name": "CONDITIONING", "links": [6]}]},
                {"id": 106, "type": "KSampler",
                 "widgets_values": [42, "fixed", 20, 5.0, "euler", "normal", 1.0],
                 "inputs": [
                    {"name": "model", "link": 10},
                    {"name": "positive", "link": 5},
                    {"name": "negative", "link": 6},
                    {"name": "latent_image", "link": null}
                 ],
                 "outputs": [{"name": "LATENT", "links": [7]}]},
                {"id": 107, "type": "VAEDecode",
                 "inputs": [{"name": "samples", "link": 7}, {"name": "vae", "link": null}],
                 "outputs": [{"name": "IMAGE", "links": [8]}]},
                {"id": 110, "type": "SaveImage", "widgets_values": ["Bench/01_model_a_face"],
                 "inputs": [{"name": "images", "link": 8}]},
                {"id": 200, "type": "CheckpointLoaderSimple", "widgets_values": ["model_b.safetensors"],
                 "inputs": [], "outputs": [{"name": "MODEL", "links": []}, {"name": "CLIP", "links": []}, {"name": "VAE", "links": []}]},
                {"id": 210, "type": "SaveImage", "widgets_values": ["Bench/02_model_b_face"],
                 "inputs": [{"name": "images", "link": null}]}
            ],
            "links": [
                [2, 2, 0, 102, 0, "STRING"],
                [3, 3, 0, 104, 0, "STRING"],
                [4, 102, 0, 103, 0, "STRING"],
                [5, 103, 0, 106, 0, "CONDITIONING"],
                [6, 104, 0, 106, 1, "CONDITIONING"],
                [7, 106, 0, 107, 0, "LATENT"],
                [8, 107, 0, 110, 0, "IMAGE"],
                [10, 100, 0, 106, 0, "MODEL"],
                [11, 100, 1, 103, 1, "CLIP"]
            ]
        }"#;
        let m = parse_workflow_meta_for(raw, Some("01_model_a_face_00001_.png"));
        assert_eq!(m.models, vec!["model_a.safetensors"]);
        assert!(m.models.iter().all(|x| x != "model_b.safetensors"));
        assert_eq!(
            m.positive.as_deref(),
            Some("masterpiece, a cat sitting")
        );
        assert_eq!(m.negative.as_deref(), Some("blurry"));
        assert_eq!(m.sampler.as_deref(), Some("euler"));
        assert_eq!(m.steps, Some(20));
    }

    #[test]
    fn parse_unwraps_comfy_prompt_wrapper() {
        let raw = r#"{
            "prompt": {
                "1": {"class_type": "CheckpointLoaderSimple", "inputs": {"ckpt_name": "a.safetensors"}},
                "2": {"class_type": "CLIPTextEncode", "inputs": {"text": "hello", "clip": ["1", 1]}},
                "3": {"class_type": "KSampler", "inputs": {
                    "positive": ["2", 0], "negative": ["2", 0],
                    "sampler_name": "euler", "scheduler": "normal", "steps": 8, "cfg": 1.0, "seed": 1
                }}
            }
        }"#;
        let m = parse_workflow_meta(raw);
        assert_eq!(m.models, vec!["a.safetensors"]);
        assert_eq!(m.positive.as_deref(), Some("hello"));
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
