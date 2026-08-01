//! Gallery presentation state: how listed items bucket into collapsing headers, and the decoded
//! thumbnail cache behind the tiles.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::types::{ActiveLora, CharacterCard, GalleryGroup, GalleryItem, Params, file_basename};

/// One LoRA referenced by a gallery image's embedded workflow.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LoraMeta {
    pub name: String,
    pub strength_model: f64,
    pub strength_clip: Option<f64>,
    /// Came from `LoraLoaderModelOnly` — the CLIP was left untouched.
    pub model_only: bool,
}

/// Prompt / model summary scraped from an embedded workflow for the viewer header.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ImageMeta {
    pub models: Vec<String>,
    /// `UNETLoader.unet_name` — set when the graph used the diffusion-model topology.
    pub unet: Option<String>,
    /// Standalone text encoders from `CLIPLoader` / `DualCLIPLoader`.
    pub clips: Vec<String>,
    /// The encoder architecture (`stable_diffusion`, `flux`, `qwen_image`, …) — restoring the
    /// encoder files without this rebuilds the graph under the wrong architecture.
    pub clip_type: Option<String>,
    pub vae: Option<String>,
    pub weight_dtype: Option<String>,
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

/// A remix field the diff sheet can toggle, naming which Params slot the meta would overwrite.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RemixField {
    Model,
    Positive,
    Negative,
    Sampler,
    Scheduler,
    Steps,
    Cfg,
    Seed,
    Loras,
}

/// One row of the remix diff sheet: a field whose incoming value differs from the current Params.
#[derive(Clone, Debug)]
pub struct RemixDiffRow {
    pub field: RemixField,
    pub label: &'static str,
    pub current: String,
    pub new: String,
}

/// Convert a gallery image's scraped LoRAs into the Create tab's active-stack shape.
pub fn meta_to_active_loras(loras: &[LoraMeta]) -> Vec<ActiveLora> {
    loras
        .iter()
        .map(|l| ActiveLora {
            file: l.name.clone(),
            strength_model: l.strength_model as f32,
            strength_clip: l.strength_clip.unwrap_or(l.strength_model) as f32,
            injected: String::new(),
            model_only: l.model_only,
        })
        .collect()
}

fn lora_line(loras: &[ActiveLora]) -> String {
    if loras.is_empty() {
        return "none".into();
    }
    loras
        .iter()
        .map(|l| format!("{} ({:.2})", l.file, l.strength_model))
        .collect::<Vec<_>>()
        .join(", ")
}

fn lora_sig(loras: &[ActiveLora]) -> Vec<(String, i32, i32, bool)> {
    loras
        .iter()
        .map(|l| {
            (
                l.file.clone(),
                (l.strength_model * 100.0).round() as i32,
                (l.strength_clip * 100.0).round() as i32,
                l.model_only,
            )
        })
        .collect()
}

/// Every field this image's meta would change vs `params`; unchanged or unset fields are omitted.
pub fn remix_diff_rows(meta: &ImageMeta, params: &Params) -> Vec<RemixDiffRow> {
    let mut rows = Vec::new();
    {
        let mut push = |field, label, current: String, new: String| {
            if current != new {
                rows.push(RemixDiffRow { field, label, current, new });
            }
        };
        if let Some(new) = meta.unet.clone().or_else(|| meta.models.first().cloned()) {
            push(RemixField::Model, "Model", params.model_file().to_string(), new);
        }
        if let Some(p) = meta.positive.clone() {
            push(RemixField::Positive, "Positive", params.positive.clone(), p);
        }
        if let Some(n) = meta.negative.clone() {
            push(RemixField::Negative, "Negative", params.negative.clone(), n);
        }
        if let Some(s) = meta.sampler.clone() {
            push(RemixField::Sampler, "Sampler", params.sampler.clone(), s);
        }
        if let Some(s) = meta.scheduler.clone() {
            push(RemixField::Scheduler, "Scheduler", params.scheduler.clone(), s);
        }
        if let Some(n) = meta.steps {
            push(RemixField::Steps, "Steps", params.steps.to_string(), (n as u32).to_string());
        }
        if let Some(c) = meta.cfg {
            push(RemixField::Cfg, "CFG", format!("{:.2}", params.cfg), format!("{:.2}", c as f32));
        }
        if let Some(s) = meta.seed.filter(|&s| s >= 0) {
            push(RemixField::Seed, "Seed", params.seed.to_string(), (s as u64).to_string());
        }
    }
    let new_loras = meta_to_active_loras(&meta.loras);
    if lora_sig(&new_loras) != lora_sig(&params.loras) {
        rows.push(RemixDiffRow {
            field: RemixField::Loras,
            label: "LoRAs",
            current: lora_line(&params.loras),
            new: lora_line(&new_loras),
        });
    }
    rows
}

/// Pull models / LoRAs / prompts / sampler settings out of API- or UI-format workflow JSON.
#[cfg_attr(target_os = "android", allow(dead_code))]
pub fn parse_workflow_meta(raw: &str) -> ImageMeta {
    parse_workflow_meta_for(raw, None)
}

/// Like [`parse_workflow_meta`] but from an already-parsed JSON value (e.g. a `/queue` entry's
/// embedded graph), skipping the string round-trip.
pub fn parse_workflow_meta_value(value: &Value) -> ImageMeta {
    let value = unwrap_workflow_root(value.clone());
    if value.get("nodes").is_some() {
        parse_ui_meta(&value, None)
    } else if value.as_object().is_some() {
        parse_api_meta(&value, None)
    } else {
        ImageMeta::default()
    }
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
                    meta.unet.get_or_insert_with(|| n.clone());
                    push_unique(&mut meta.models, n);
                }
                if let Some(d) = str_in(&inputs, "weight_dtype") {
                    meta.weight_dtype.get_or_insert(d);
                }
            }
            "CLIPLoader" | "DualCLIPLoader" => {
                for key in ["clip_name", "clip_name1", "clip_name2"] {
                    if let Some(n) = str_in(&inputs, key) {
                        push_unique(&mut meta.clips, n);
                    }
                }
                if let Some(t) = str_in(&inputs, "type") {
                    meta.clip_type.get_or_insert(t);
                }
            }
            "VAELoader" => {
                if let Some(n) = str_in(&inputs, "vae_name") {
                    meta.vae.get_or_insert(n);
                }
            }
            "LoraLoader" | "LoraLoaderModelOnly" => {
                if let Some(name) = str_in(&inputs, "lora_name") {
                    meta.loras.push(LoraMeta {
                        name,
                        strength_model: num_in(&inputs, "strength_model").unwrap_or(1.0),
                        strength_clip: num_in(&inputs, "strength_clip"),
                        model_only: class == "LoraLoaderModelOnly",
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
                            model_only: false,
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

    // Node id order, not hash order: a two-expert Wan graph has two UNETLoaders, and whichever is
    // visited first claims `unet` and heads `models` — a scrape that changed run to run otherwise.
    for id in sorted_ids(by_id) {
        let node = by_id[&id];
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
            // UNETLoader widgets: [unet_name, weight_dtype].
            "UNETLoader" => {
                if let Some(n) = widget_str(&widgets, 0) {
                    meta.unet.get_or_insert_with(|| n.clone());
                    push_unique(&mut meta.models, n);
                }
                if let Some(d) = widget_str(&widgets, 1) {
                    meta.weight_dtype.get_or_insert(d);
                }
            }
            // CLIPLoader widgets: [clip_name, type, device].
            // DualCLIPLoader widgets: [clip_name1, clip_name2, type, device].
            "CLIPLoader" | "DualCLIPLoader" => {
                let names = if class == "DualCLIPLoader" { 2 } else { 1 };
                for slot in 0..names {
                    if let Some(n) = widget_str(&widgets, slot) {
                        push_unique(&mut meta.clips, n);
                    }
                }
                if let Some(t) = widget_str(&widgets, names) {
                    meta.clip_type.get_or_insert(t);
                }
            }
            "VAELoader" => {
                if let Some(n) = widget_str(&widgets, 0) {
                    meta.vae.get_or_insert(n);
                }
            }
            "LoraLoader" | "LoraLoaderModelOnly" => {
                let model_only = class == "LoraLoaderModelOnly";
                if let Some(name) = widget_str(&widgets, 0) {
                    meta.loras.push(LoraMeta {
                        name,
                        strength_model: widget_num(&widgets, 1).unwrap_or(1.0),
                        // Model-only has no clip widget; slot 2 would be another node's value.
                        strength_clip: (!model_only).then(|| widget_num(&widgets, 2)).flatten(),
                        model_only,
                    });
                }
            }
            // KSampler widgets: [seed, control, steps, cfg, sampler_name, scheduler, denoise].
            // KSamplerAdvanced leads with `add_noise`, so every widget after it sits one slot later.
            "KSampler" | "KSamplerAdvanced" => {
                if best_sampler.as_ref().map(|(p, _)| *p).unwrap_or(0) < 2 {
                    best_sampler = Some((2, id));
                    let seed = usize::from(class == "KSamplerAdvanced");
                    meta.seed = widget_num(&widgets, seed).map(|n| n as i64);
                    meta.steps = widget_num(&widgets, seed + 2).map(|n| n as u64);
                    meta.cfg = widget_num(&widgets, seed + 3);
                    meta.sampler = widget_str(&widgets, seed + 4);
                    meta.scheduler = widget_str(&widgets, seed + 5);
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
                        model_only: false,
                    });
                }
            }
        }
    }

    if meta.positive.is_none() || meta.negative.is_none() {
        let mut vals = Vec::new();
        for id in sorted_ids(by_id) {
            let node = by_id[&id];
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

fn sorted_ids(by_id: &HashMap<u64, &Value>) -> Vec<u64> {
    let mut ids: Vec<u64> = by_id.keys().copied().collect();
    ids.sort_unstable();
    ids
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

/// Durable gallery tree (same root as model packs). Full images live in `gallery_full/`.
pub const DURABLE_GALLERY_ROOT: &str = "/storage/emulated/0/ComfyUI";

/// Keeps Android's media scanner from indexing a directory tree into the phone's Photos app.
const NOMEDIA: &str = ".nomedia";

/// Soft cap on the full-image cache; oldest mtime files evict first.
const FULL_CACHE_BUDGET: u64 = 32 * 1024 * 1024 * 1024;

/// File/byte counts for a full-image cache root.
#[derive(Clone, Copy, Debug, Default)]
pub struct FullCacheStats {
    pub files: usize,
    pub bytes: u64,
}

/// Prefer durable `/sdcard/ComfyUI/gallery_full`; fall back to `{documents}/gallery_full`.
pub fn resolve_full_cache_root(documents: Option<&str>) -> Option<String> {
    let durable = format!("{DURABLE_GALLERY_ROOT}/gallery_full");
    if probe_writable_dir(&durable) {
        return Some(durable);
    }
    let docs = documents?;
    let fallback = format!("{docs}/gallery_full");
    probe_writable_dir(&fallback).then_some(fallback)
}

fn probe_writable_dir(dir: &str) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let probe = Path::new(dir).join(".write_test");
    if std::fs::write(&probe, b"").is_err() {
        return false;
    }
    let _ = std::fs::remove_file(&probe);
    true
}

fn full_cache_path(dir: &Path, key: &str) -> PathBuf {
    // Keep one file per image; flatten nested subfolders.
    let safe: String = key
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' })
        .collect();
    dir.join(safe)
}

fn full_cache_key_path(dir: &Path, key: &str) -> PathBuf {
    let mut os = full_cache_path(dir, key).into_os_string();
    os.push(".key");
    PathBuf::from(os)
}

fn is_full_cache_meta(name: &std::ffi::OsStr) -> bool {
    let s = name.to_string_lossy();
    s == ".write_test" || s == NOMEDIA || s.ends_with(".key")
}

/// What a delete's tombstone should do with a listing row carrying its name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TombstoneVerdict {
    /// The deleted image itself, still in the server index — keep it out of the grid.
    Hide,
    /// A different image under the recycled save number — retire the tombstone and show it.
    Recycled,
    /// Nothing identifiable left to hide.
    Expired,
}

/// Rule the gallery applies to a listed row a delete tombstoned.
///
/// `deleted` is the row's byte size at delete time (0 when the listing never said), `listed` the
/// size the server reports now, `age` seconds since the delete, `settle` how long a sizeless
/// tombstone may hide. Sizes decide it when both are known: ComfyUI numbers a save
/// `max(existing) + 1` over the output folder and comfy-gate's soft delete moves the file out of
/// that folder, so the next render can be handed the deleted image's exact filename.
pub fn tombstone_verdict(deleted: u64, listed: u64, age: f64, settle: f64) -> TombstoneVerdict {
    if deleted > 0 && listed > 0 && deleted != listed {
        return TombstoneVerdict::Recycled;
    }
    if deleted > 0 || age < settle {
        TombstoneVerdict::Hide
    } else {
        TombstoneVerdict::Expired
    }
}

/// Write `.nomedia` into `dir`; true only when it was newly created (nothing there before).
pub fn ensure_nomedia(dir: &str) -> bool {
    let path = Path::new(dir).join(NOMEDIA);
    if path.exists() {
        return false;
    }
    std::fs::create_dir_all(dir).is_ok() && std::fs::write(&path, b"").is_ok()
}

/// Drop a cached image and its key sidecar, so a deleted server image stops occupying the phone.
pub fn forget_full_cache(root: &str, key: &str) {
    let dir = Path::new(root);
    let _ = std::fs::remove_file(full_cache_path(dir, key));
    let _ = std::fs::remove_file(full_cache_key_path(dir, key));
}

/// Persist the original gallery key next to a cached image (for CLIP indexing without a listing).
pub fn ensure_full_cache_key(root: &str, key: &str) {
    let dir = Path::new(root);
    if !full_cache_has(root, key) {
        return;
    }
    let path = full_cache_key_path(dir, key);
    if path.is_file() {
        return;
    }
    let _ = std::fs::write(path, key.as_bytes());
}

/// True when `root` holds a non-empty file for gallery key `subfolder/filename`.
pub fn full_cache_has(root: &str, key: &str) -> bool {
    full_cache_len(root, key).is_some()
}

/// Byte length of the cached image for `key`, or `None` when absent or empty. Compared against the
/// listing's size to spot a cache entry whose filename now names different bytes.
pub fn full_cache_len(root: &str, key: &str) -> Option<u64> {
    let path = full_cache_path(Path::new(root), key);
    let meta = path.metadata().ok().filter(|m| m.is_file())?;
    (meta.len() > 0).then_some(meta.len())
}

/// Read a previously cached full image, or `None` on miss / IO error.
/// `root` is the `gallery_full` directory itself.
pub fn read_full_cache(root: &str, key: &str) -> Option<Vec<u8>> {
    let path = full_cache_path(Path::new(root), key);
    let bytes = std::fs::read(path).ok().filter(|b| !b.is_empty())?;
    ensure_full_cache_key(root, key);
    Some(bytes)
}

/// Persist a full image and evict oldest files when the cache exceeds the budget.
pub fn write_full_cache(root: &str, key: &str, bytes: &[u8]) {
    let dir = Path::new(root);
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let path = full_cache_path(dir, key);
    if std::fs::write(&path, bytes).is_err() {
        return;
    }
    let _ = std::fs::write(full_cache_key_path(dir, key), key.as_bytes());
    evict_full_cache(dir);
}

/// Original gallery keys recorded beside cached images (via `.key` sidecars).
pub fn full_cache_keys(root: &str) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut keys = Vec::new();
    for ent in rd.flatten() {
        let path = ent.path();
        let name = ent.file_name();
        let name_s = name.to_string_lossy();
        if !name_s.ends_with(".key") || !path.is_file() {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else { continue };
        let key = raw.trim();
        if key.is_empty() {
            continue;
        }
        // Image file is the sidecar stem (strip trailing ".key").
        let img = Path::new(root).join(name_s.trim_end_matches(".key"));
        if img.is_file() && img.metadata().map(|m| m.len() > 0).unwrap_or(false) {
            keys.push(key.to_string());
        }
    }
    keys
}

/// Count files and total bytes under the full-image cache root.
pub fn full_cache_stats(root: &str) -> FullCacheStats {
    let Ok(rd) = std::fs::read_dir(root) else {
        return FullCacheStats::default();
    };
    let mut stats = FullCacheStats::default();
    for ent in rd.flatten() {
        let Ok(meta) = ent.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let name = ent.file_name();
        if is_full_cache_meta(&name) {
            continue;
        }
        stats.files += 1;
        stats.bytes = stats.bytes.saturating_add(meta.len());
    }
    stats
}

/// How many of `keys` are present as non-empty files under `root`.
pub fn full_cache_hits(root: &str, keys: impl IntoIterator<Item = impl AsRef<str>>) -> usize {
    keys.into_iter().filter(|k| full_cache_has(root, k.as_ref())).count()
}

/// Delete every file in the full-image cache directory.
pub fn clear_full_cache(root: &str) -> Result<usize, String> {
    let Ok(rd) = std::fs::read_dir(root) else {
        return Ok(0);
    };
    let mut n = 0usize;
    for ent in rd.flatten() {
        let path = ent.path();
        // `.nomedia` stays: removing it re-publishes the tree to Photos on the next scan.
        if ent.file_name() == NOMEDIA {
            continue;
        }
        if path.is_file() && std::fs::remove_file(&path).is_ok() {
            n += 1;
        }
    }
    Ok(n)
}

fn evict_full_cache(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();
    let mut total = 0u64;
    for ent in rd.flatten() {
        let Ok(meta) = ent.metadata() else { continue };
        if !meta.is_file() || is_full_cache_meta(&ent.file_name()) {
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
            let mut key_os = path.as_os_str().to_os_string();
            key_os.push(".key");
            let _ = std::fs::remove_file(PathBuf::from(key_os));
        }
    }
}

/// One collapsing header's worth of items, as indices into the listing.
#[derive(Clone)]
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
pub fn group_items(items: &[GalleryItem], group: GalleryGroup, characters: &[CharacterCard]) -> Vec<Group> {
    let all: Vec<usize> = (0..items.len()).collect();
    group_selected(items, &all, group, characters)
}

/// [`group_items`] over a subset: `sel` holds indices into `items` (e.g. after a media filter),
/// and the returned groups carry those same original indices.
pub fn group_selected(
    items: &[GalleryItem],
    sel: &[usize],
    group: GalleryGroup,
    characters: &[CharacterCard],
) -> Vec<Group> {
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
            GalleryGroup::Date => item.date_label(),
            GalleryGroup::Character => character_label(item, characters),
            GalleryGroup::Folder | GalleryGroup::None => item.subfolder.clone(),
        };
        match index.get(&key) {
            Some(&g) => groups[g].items.push(i),
            None => {
                index.insert(key.clone(), groups.len());
                let label = match group {
                    GalleryGroup::Model => item.model_label(),
                    GalleryGroup::Date => item.date_label(),
                    GalleryGroup::Character => key.clone(),
                    GalleryGroup::Folder | GalleryGroup::None => item.group_label(),
                };
                groups.push(Group { key, label, items: vec![i] });
            }
        }
    }
    groups
}

/// Character bucket for an item: the first card whose LoRA stack names a model the item's graph
/// referenced (case-insensitive basename match against `item.models`), else "No character".
///
/// The list API returns no prompt text per item, so identity-tag matching is not possible here;
/// LoRA-name matching is the robust signal available before opening a viewer.
fn character_label(item: &GalleryItem, characters: &[CharacterCard]) -> String {
    characters
        .iter()
        .find(|c| item_matches_character(item, c))
        .map(|c| c.name.clone())
        .unwrap_or_else(|| "No character".to_string())
}

/// Whether `item`'s graph referenced any of `card`'s LoRAs (case-insensitive basename match against
/// `item.models`). The signal the Character grouping and centroid-seeding both rely on.
pub fn item_matches_character(item: &GalleryItem, card: &CharacterCard) -> bool {
    card.loras.iter().any(|l| {
        let base = file_basename(&l.file);
        !base.is_empty() && item.models.iter().any(|m| file_basename(m).eq_ignore_ascii_case(base))
    })
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
    /// The frame each entry was last read on, so eviction can tell what is currently on screen.
    /// Read through a shared borrow (every grid reads tiles behind `&self`), hence the `RefCell`.
    seen: RefCell<HashMap<String, u64>>,
    /// Bumped once per frame by [`Self::begin_frame`]; 0 when the host never calls it, which
    /// leaves every entry looking current and degrades cleanly to insertion-order eviction.
    frame: u64,
}

/// Roughly 16 full-width tiles, or ~150 grid tiles.
const BUDGET_BYTES: usize = 64 * 1024 * 1024;

/// Box-downscale raw RGBA so its long side is at most `max`, returning `(w, h, rgba)` unchanged
/// when it already fits.
///
/// Exists because not every producer decodes through `decode_thumb`: Android's
/// `ContentResolver.loadThumbnail` takes its size as a HINT and serves from fixed buckets, so a
/// request for 256 can come back at 512 or 1024. One unclamped producer is all it takes to put
/// multi-megabyte entries in a cache sized for 0.4 MB tiles, which is what made the pickers evict
/// and re-fetch the tiles they were drawing.
pub fn downscale_rgba(w: u32, h: u32, rgba: &[u8], max: u32) -> (u32, u32, Vec<u8>) {
    let long = w.max(h);
    if long <= max || w == 0 || h == 0 || rgba.len() < (w as usize * h as usize * 4) {
        return (w, h, rgba.to_vec());
    }
    // Integer box factor: cheap, and the result is a preview at a few hundred pixels.
    let factor = long.div_ceil(max).max(2) as usize;
    let (nw, nh) = ((w as usize / factor).max(1), (h as usize / factor).max(1));
    let mut out = Vec::with_capacity(nw * nh * 4);
    for y in 0..nh {
        for x in 0..nw {
            let (mut acc, mut n) = ([0u32; 4], 0u32);
            for dy in 0..factor {
                let sy = y * factor + dy;
                if sy >= h as usize {
                    break;
                }
                for dx in 0..factor {
                    let sx = x * factor + dx;
                    if sx >= w as usize {
                        break;
                    }
                    let i = (sy * w as usize + sx) * 4;
                    for c in 0..4 {
                        acc[c] += rgba[i + c] as u32;
                    }
                    n += 1;
                }
            }
            let n = n.max(1);
            for c in 0..4 {
                out.push((acc[c] / n) as u8);
            }
        }
    }
    (nw as u32, nh as u32, out)
}

/// Cap on remembered reads. Far more than any screenful, so an on-screen entry is always covered;
/// the bound just stops a long session accumulating keys nothing looks at any more.
const SEEN_MAX: usize = 1024;

impl Default for ThumbCache {
    fn default() -> Self {
        Self {
            textures: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            pending: HashSet::new(),
            seen: RefCell::new(HashMap::new()),
            frame: 0,
        }
    }
}

impl ThumbCache {
    pub fn get(&self, key: &str) -> Option<&egui::TextureHandle> {
        let tex = self.textures.get(key);
        if tex.is_some() {
            self.touch(key);
        }
        tex
    }

    /// Start a frame. Everything read on this frame or the one before counts as on screen for
    /// [`Self::insert`]'s eviction pass; older reads stop protecting an entry.
    ///
    /// The previous frame has to count too, because the message drain that lands decoded
    /// thumbnails runs BEFORE this frame's draw — at that moment the only record of what is on
    /// screen is last frame's reads.
    pub fn begin_frame(&mut self) {
        self.frame = self.frame.wrapping_add(1);
    }

    /// Stamp `key` as drawn on the current frame. Bounded, because over a long session the map
    /// would otherwise fill with keys nothing looks at any more — but pruned BY AGE, never
    /// wholesale: `touch` is reached from `get` mid-draw and from `insert` one statement before
    /// the eviction pass, so a `clear()` here would strip the on-screen marks from tiles already
    /// drawn this frame and the very next pass would evict them.
    fn touch(&self, key: &str) {
        let mut seen = self.seen.borrow_mut();
        if seen.len() >= SEEN_MAX && !seen.contains_key(key) {
            let frame = self.frame;
            seen.retain(|_, f| frame.wrapping_sub(*f) <= 1);
            // Nothing aged out (every mark is current) — drop them all rather than grow unbounded.
            if seen.len() >= SEEN_MAX {
                seen.clear();
            }
        }
        seen.insert(key.to_string(), self.frame);
    }

    /// Was `key` drawn recently enough that evicting it would just make the grid re-fetch it?
    fn on_screen(&self, key: &str) -> bool {
        self.seen.borrow().get(key).is_some_and(|f| self.frame.wrapping_sub(*f) <= 1)
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

    /// Drop every decoded size of one gallery key, and any in-flight claim on them.
    pub fn forget(&mut self, key: &str) {
        let prefix = format!("{key}#");
        let mut freed = 0usize;
        self.pending.retain(|k| !k.starts_with(&prefix));
        self.seen.borrow_mut().retain(|k, _| !k.starts_with(&prefix));
        self.order.retain(|(k, cost)| {
            let stale = k.starts_with(&prefix);
            if stale {
                freed += *cost;
            }
            !stale
        });
        self.bytes = self.bytes.saturating_sub(freed);
        self.textures.retain(|k, _| !k.starts_with(&prefix));
    }

    /// Drop one entry by its exact key. [`Self::forget`] matches the `{item}#{size}` grammar by
    /// prefix, which can never reach a namespaced key like `input#name.png` — and an input file
    /// CAN be overwritten in place (uploads pass `overwrite = true`), so its preview needs a way
    /// to be invalidated or the node keeps painting the previous picture.
    pub fn forget_exact(&mut self, key: &str) {
        self.pending.remove(key);
        self.seen.borrow_mut().remove(key);
        if self.textures.remove(key).is_some()
            && let Some(pos) = self.order.iter().position(|(k, _)| k == key)
        {
            let cost = self.order.remove(pos).map(|(_, c)| c).unwrap_or(0);
            self.bytes = self.bytes.saturating_sub(cost);
        }
    }

    pub fn insert(&mut self, key: String, tex: egui::TextureHandle, bytes: usize) {
        self.pending.remove(&key);
        // A tile is decoded because something is drawing it, so it counts as read.
        self.touch(&key);
        if self.textures.insert(key.clone(), tex).is_none() {
            self.order.push_back((key, bytes));
            self.bytes += bytes;
        } else if let Some(slot) = self.order.iter_mut().find(|(k, _)| *k == key) {
            // Replacing a live entry: the two decodes can differ in size (a thumb's disk-cache
            // and network paths can both land after `reset_pending` clears the claim), and
            // keeping the first cost leaves the budget wrong for the entry's whole lifetime.
            self.bytes = self.bytes.saturating_sub(slot.1) + bytes;
            slot.1 = bytes;
        }
        // Second chance: insertion order alone throws out the tile the user is looking at the
        // moment the cache fills, and the grid re-fetches it on the very next frame — a picker
        // scrolled over a big library sat there reloading the same thumbnails forever. An entry
        // that is on screen goes to the back instead.
        //
        // The reprieve must NOT consume the on-screen mark. A message drain lands up to a dozen
        // decoded thumbnails back to back, each running a full pass, and no draw happens in
        // between to re-mark anything — an earlier version cleared the marks as it went, so only
        // the first insert of the frame was protected and the other eleven evicted straight
        // through the visible tiles. Sparing is tracked per pass instead, which is also what
        // bounds the loop: an entry can be requeued at most once, so after at most `order.len()`
        // requeues something is evicted or the budget is met.
        let mut spared: HashSet<String> = HashSet::new();
        while self.bytes > BUDGET_BYTES && self.order.len() > 1 {
            let Some((old, cost)) = self.order.pop_front() else { break };
            if self.on_screen(&old) && spared.insert(old.clone()) {
                self.order.push_back((old, cost));
                continue;
            }
            self.seen.borrow_mut().remove(&old);
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

    /// A three-loader Anima graph must round-trip every part the Create tab needs to rebuild it —
    /// dropping `type` would resurrect the encoders under the wrong architecture.
    #[test]
    fn api_format_captures_the_whole_diffusion_loader_set() {
        let meta = parse_workflow_meta(
            r#"{
            "4": {"class_type": "CLIPLoader", "inputs": {"clip_name": "qwen_3_06b_base.safetensors", "type": "stable_diffusion", "device": "default"}},
            "5": {"class_type": "VAELoader", "inputs": {"vae_name": "qwen_image_vae.safetensors"}},
            "6": {"class_type": "UNETLoader", "inputs": {"unet_name": "Anima/novaAnimeAM_v30.safetensors", "weight_dtype": "default"}},
            "7": {"class_type": "LoraLoaderModelOnly", "inputs": {"lora_name": "Anima/MatureFemaleSliderAnima.safetensors", "strength_model": 0.7, "model": ["6", 0]}}
        }"#,
        );
        assert_eq!(meta.unet.as_deref(), Some("Anima/novaAnimeAM_v30.safetensors"));
        assert_eq!(meta.clips, vec!["qwen_3_06b_base.safetensors"]);
        assert_eq!(meta.clip_type.as_deref(), Some("stable_diffusion"));
        assert_eq!(meta.vae.as_deref(), Some("qwen_image_vae.safetensors"));
        assert_eq!(meta.weight_dtype.as_deref(), Some("default"));
        assert!(meta.loras[0].model_only, "LoraLoaderModelOnly must not restore as a CLIP LoRA");
    }

    /// DualCLIPLoader's `type` sits at widget slot 2, after both names.
    #[test]
    fn ui_format_reads_dual_clip_widgets_positionally() {
        let meta = parse_workflow_meta(
            r#"{"nodes": [
            {"id": 1, "type": "UNETLoader", "widgets_values": ["flux1-dev.safetensors", "fp8_e4m3fn"]},
            {"id": 2, "type": "DualCLIPLoader", "widgets_values": ["clip_l.safetensors", "t5xxl.safetensors", "flux", "default"]},
            {"id": 3, "type": "VAELoader", "widgets_values": ["ae.safetensors"]}
        ], "links": []}"#,
        );
        assert_eq!(meta.unet.as_deref(), Some("flux1-dev.safetensors"));
        assert_eq!(meta.weight_dtype.as_deref(), Some("fp8_e4m3fn"));
        assert_eq!(meta.clips, vec!["clip_l.safetensors", "t5xxl.safetensors"]);
        assert_eq!(meta.clip_type.as_deref(), Some("flux"));
        assert_eq!(meta.vae.as_deref(), Some("ae.safetensors"));
    }

    /// A WAN video's UI graph: `KSamplerAdvanced` puts `add_noise` ahead of the seed, so reading it
    /// on KSampler's slots hands `steps` to `cfg` and `sampler_name` to `scheduler`.
    #[test]
    fn ui_format_reads_ksampler_advanced_one_slot_over() {
        let meta = parse_workflow_meta(
            r#"{"nodes": [
            {"id": 1, "type": "UNETLoader", "widgets_values": ["Wan/wan2.2_i2v_high_noise_14B.safetensors", "default"]},
            {"id": 2, "type": "UNETLoader", "widgets_values": ["Wan/wan2.2_i2v_low_noise_14B.safetensors", "default"]},
            {"id": 3, "type": "CLIPLoader", "widgets_values": ["umt5_xxl_fp8.safetensors", "wan", "cpu"]},
            {"id": 4, "type": "KSamplerAdvanced",
             "widgets_values": ["enable", 12345, "fixed", 8, 2.5, "euler", "simple", 0, 4, "enable"]}
        ], "links": []}"#,
        );
        assert_eq!(meta.seed, Some(12345), "noise_seed sits after add_noise");
        assert_eq!(meta.steps, Some(8));
        assert_eq!(meta.cfg, Some(2.5));
        assert_eq!(meta.sampler.as_deref(), Some("euler"));
        assert_eq!(meta.scheduler.as_deref(), Some("simple"));
        assert_eq!(meta.clip_type.as_deref(), Some("wan"));
        assert_eq!(meta.models.len(), 2, "both Wan experts are models");
    }

    /// Plain KSampler must keep reading its own slots.
    #[test]
    fn ui_format_still_reads_plain_ksampler_slots() {
        let meta = parse_workflow_meta(
            r#"{"nodes": [
            {"id": 1, "type": "KSampler", "widgets_values": [777, "randomize", 20, 5.0, "dpmpp_2m", "karras", 1.0]}
        ], "links": []}"#,
        );
        assert_eq!(meta.seed, Some(777));
        assert_eq!(meta.steps, Some(20));
        assert_eq!(meta.cfg, Some(5.0));
        assert_eq!(meta.sampler.as_deref(), Some("dpmpp_2m"));
        assert_eq!(meta.scheduler.as_deref(), Some("karras"));
    }

    /// The symptom this exists for: delete the newest image, render again, and ComfyUI writes the
    /// filename just freed. Hiding by name alone would swallow that brand-new image.
    #[test]
    fn a_recycled_save_number_retires_the_tombstone() {
        use TombstoneVerdict::*;
        // Same name, same bytes: the delete has not reached the index yet.
        assert_eq!(tombstone_verdict(1_048_576, 1_048_576, 5.0, 120.0), Hide);
        // Same name, different bytes: a new image took the number.
        assert_eq!(tombstone_verdict(1_048_576, 990_000, 5.0, 120.0), Recycled);
        // Sizes stay decisive well past the settle window — that is what makes the long TTL safe.
        assert_eq!(tombstone_verdict(1_048_576, 990_000, 86_400.0, 120.0), Recycled);
        assert_eq!(tombstone_verdict(1_048_576, 1_048_576, 86_400.0, 120.0), Hide);
    }

    /// Without a recorded size there is nothing to tell the two apart, so the hide is brief.
    #[test]
    fn a_sizeless_tombstone_only_hides_while_the_delete_settles() {
        use TombstoneVerdict::*;
        assert_eq!(tombstone_verdict(0, 1_048_576, 5.0, 120.0), Hide);
        assert_eq!(tombstone_verdict(0, 1_048_576, 300.0, 120.0), Expired);
        // A listing that omits size falls back to the same window.
        assert_eq!(tombstone_verdict(1_048_576, 0, 5.0, 120.0), Hide);
    }

    fn temp_dir(name: &str) -> String {
        let dir = std::env::temp_dir().join(format!("comfyui-gallery-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().into_owned()
    }

    /// The marker is written once; a second call must report "already there" so the app doesn't
    /// re-trigger a media rescan on every launch.
    #[test]
    fn nomedia_is_written_once() {
        let dir = temp_dir("nomedia");
        assert!(ensure_nomedia(&dir));
        assert!(Path::new(&dir).join(".nomedia").is_file());
        assert!(!ensure_nomedia(&dir));
    }

    /// A cleared cache keeps its `.nomedia`: losing it re-publishes the tree to the phone's Photos.
    #[test]
    fn clearing_the_cache_keeps_the_nomedia_marker() {
        let dir = temp_dir("clear");
        ensure_nomedia(&dir);
        write_full_cache(&dir, "u1/a/1.png", b"png");
        assert_eq!(clear_full_cache(&dir).unwrap(), 2, "image + key sidecar");
        assert!(Path::new(&dir).join(".nomedia").is_file());
        assert_eq!(full_cache_stats(&dir).files, 0);
    }

    /// Deleting an image server-side must free its on-device copy, sidecar included.
    #[test]
    fn forgetting_a_key_drops_the_image_and_its_sidecar() {
        let dir = temp_dir("forget");
        write_full_cache(&dir, "u1/a/1.png", b"png");
        write_full_cache(&dir, "u1/a/2.png", b"png");
        forget_full_cache(&dir, "u1/a/1.png");
        assert!(!full_cache_has(&dir, "u1/a/1.png"));
        assert!(full_cache_keys(&dir).iter().all(|k| k != "u1/a/1.png"));
        assert!(full_cache_has(&dir, "u1/a/2.png"), "siblings survive");
    }

    fn item(sub: &str, file: &str, models: &[&str]) -> GalleryItem {
        GalleryItem {
            subfolder: sub.into(),
            filename: file.into(),
            size: 0,
            is_video: false,
            has_workflow: false,
            models: models.iter().map(|m| m.to_string()).collect(),
            mtime: None,
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
        let groups = group_items(&items, GalleryGroup::Folder, &[]);
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
        let groups = group_items(&items, GalleryGroup::Model, &[]);
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
        let groups = group_items(&items, GalleryGroup::None, &[]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].items, vec![0, 1]);
    }

    #[test]
    fn groups_by_character_via_lora_name_match() {
        let card = |name: &str, lora: &str| CharacterCard {
            name: name.into(),
            loras: vec![crate::types::ActiveLora {
                file: lora.into(),
                strength_model: 1.0,
                strength_clip: 1.0,
                injected: String::new(),
                model_only: false,
            }],
            ..Default::default()
        };
        let chars = vec![card("Mia", "loras/mia_v2.safetensors"), card("Rin", "rin.safetensors")];
        let items = vec![
            item("u1/a", "1.png", &["sdxl.safetensors", "mia_v2.safetensors"]),
            item("u1/a", "2.png", &["sdxl.safetensors"]),
            item("u1/a", "3.png", &["RIN.safetensors"]),
            item("u1/a", "4.png", &["mia_v2.safetensors"]),
        ];
        let groups = group_items(&items, GalleryGroup::Character, &chars);
        assert_eq!(groups[0].label, "Mia");
        // Matched by basename, case-insensitively; interleaved Mia items share one group.
        assert_eq!(groups[0].items, vec![0, 3]);
        assert_eq!(groups[1].label, "No character");
        assert_eq!(groups[1].items, vec![1]);
        assert_eq!(groups[2].label, "Rin");
        assert_eq!(groups[2].items, vec![2]);
    }

    #[test]
    fn groups_by_date_from_subfolder_path() {
        let items = vec![
            item("u1/out/2026-07-16", "a.png", &[]),
            item("u1/out/2026-07-15", "b.png", &[]),
            item("u1/out/2026-07-16", "c.png", &[]),
            item("u1/out", "plain.png", &[]),
        ];
        let groups = group_items(&items, GalleryGroup::Date, &[]);
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].label, "2026-07-16");
        assert_eq!(groups[0].items, vec![0, 2]);
        assert_eq!(groups[1].label, "2026-07-15");
        assert_eq!(groups[2].label, "Unknown date");
    }

    #[test]
    fn empty_listing_groups_cleanly() {
        assert_eq!(group_items(&[], GalleryGroup::Folder, &[]).len(), 1);
        assert!(group_items(&[], GalleryGroup::Folder, &[])[0].items.is_empty());
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

    /// The symptom this exists for: scroll a picker over a library bigger than the budget and the
    /// oldest-first rule evicts the tiles still on screen, which the grid then re-fetches on the
    /// next frame — thumbnails reloading under the finger. A tile that was read survives; one that
    /// wasn't is what pays for the new entry.
    #[test]
    fn a_visible_tile_outlives_one_that_scrolled_away() {
        let ctx = egui::Context::default();
        let tex = |name: &str| {
            ctx.load_texture(
                name,
                egui::ColorImage::filled([1, 1], egui::Color32::RED),
                egui::TextureOptions::LINEAR,
            )
        };
        let mut c = ThumbCache::default();
        // Two entries fit; the third pushes past the budget and one has to go.
        let third = BUDGET_BYTES / 3 + 1;
        c.insert("gone#320".into(), tex("gone"), third);
        c.insert("onscreen#320".into(), tex("onscreen"), third);
        // A frame passes with only the second one still on screen.
        assert!(c.get("onscreen#320").is_some());
        c.insert("new#320".into(), tex("new"), third);
        assert!(c.get("onscreen#320").is_some(), "a tile being drawn must not be evicted");
        assert!(c.get("gone#320").is_none(), "the tile that scrolled away is the one that pays");
        assert!(c.get("new#320").is_some());
    }

    /// The bug the frame-stamped mark exists for: a message drain lands many decoded thumbnails
    /// back to back with NO draw in between, so an eviction pass that consumed the on-screen mark
    /// protected only the first insert of the frame and the rest evicted straight through the
    /// visible tiles. Draw once, then insert repeatedly — the drawn tiles must all survive.
    #[test]
    fn a_drawn_tile_survives_a_whole_drain_of_inserts() {
        let ctx = egui::Context::default();
        let tex = |name: &str| {
            ctx.load_texture(
                name,
                egui::ColorImage::filled([1, 1], egui::Color32::RED),
                egui::TextureOptions::LINEAR,
            )
        };
        let mut c = ThumbCache::default();
        let tile = BUDGET_BYTES / 16; // 16 tiles fill the budget exactly
        // Twelve tiles from earlier browsing, scrolled away several frames ago.
        c.begin_frame();
        for i in 0..12 {
            c.insert(format!("stale{i}"), tex("stale"), tile);
        }
        for _ in 0..4 {
            c.begin_frame();
        }
        // Four tiles the picker is showing now, read on this frame's draw.
        for i in 0..4 {
            c.insert(format!("live{i}"), tex("live"), tile);
        }
        for i in 0..4 {
            assert!(c.get(&format!("live{i}")).is_some());
        }
        // The cache is now exactly at budget. Next frame's drain lands eight more thumbnails back
        // to back, with no draw in between to re-mark anything.
        c.begin_frame();
        for i in 0..8 {
            c.insert(format!("new{i}"), tex("new"), tile);
        }
        for i in 0..4 {
            assert!(
                c.get(&format!("live{i}")).is_some(),
                "live{i} was evicted mid-drain — the grid would re-fetch it on the next frame"
            );
        }
        assert!(c.get("stale0").is_none(), "the scrolled-away tiles are what should pay");
        assert!(c.bytes <= BUDGET_BYTES, "the budget still has to hold");
    }

    /// A producer that ignores the size it was asked for must not be able to put a huge entry in a
    /// cache sized for small ones — Android's `loadThumbnail` size is a hint, not a contract.
    #[test]
    fn an_oversized_source_is_boxed_down_to_the_cap() {
        let (w, h) = (1024u32, 768u32);
        let src = vec![200u8; (w * h * 4) as usize];
        let (nw, nh, out) = downscale_rgba(w, h, &src, 256);
        assert!(nw.max(nh) <= 256, "got {nw}x{nh}");
        assert_eq!(out.len(), (nw * nh * 4) as usize);
        assert!(out.iter().all(|&b| b == 200), "a flat image must stay flat through the box filter");
        // Already small enough: handed back untouched, no allocation-shaped surprises.
        let small = vec![7u8; 4 * 4 * 4];
        assert_eq!(downscale_rgba(4, 4, &small, 256), (4, 4, small.clone()));
        // Degenerate inputs must not panic or index out of bounds.
        assert_eq!(downscale_rgba(0, 0, &[], 256), (0, 0, Vec::new()));
        assert_eq!(downscale_rgba(9999, 9999, &small, 256), (9999, 9999, small));
    }

    /// A mark must not protect an entry forever: a tile scrolled away frames ago is exactly what
    /// eviction should take.
    #[test]
    fn a_tile_stops_being_protected_once_it_leaves_the_screen() {
        let ctx = egui::Context::default();
        let tex = |name: &str| {
            ctx.load_texture(
                name,
                egui::ColorImage::filled([1, 1], egui::Color32::RED),
                egui::TextureOptions::LINEAR,
            )
        };
        let mut c = ThumbCache::default();
        let third = BUDGET_BYTES / 3 + 1;
        c.begin_frame();
        c.insert("stale".into(), tex("stale"), third);
        assert!(c.get("stale").is_some());
        // Several frames pass without it being drawn.
        for _ in 0..4 {
            c.begin_frame();
        }
        c.insert("a".into(), tex("a"), third);
        c.insert("b".into(), tex("b"), third);
        assert!(c.get("stale").is_none(), "an entry nothing has drawn for frames must be evictable");
    }

    /// Every entry read and still over budget: the pass must not spin re-queueing them forever.
    #[test]
    fn eviction_terminates_when_everything_is_on_screen() {
        let ctx = egui::Context::default();
        let tex = |name: &str| {
            ctx.load_texture(
                name,
                egui::ColorImage::filled([1, 1], egui::Color32::RED),
                egui::TextureOptions::LINEAR,
            )
        };
        let mut c = ThumbCache::default();
        let third = BUDGET_BYTES / 3 + 1;
        c.insert("a#320".into(), tex("a"), third);
        c.insert("b#320".into(), tex("b"), third);
        assert!(c.get("a#320").is_some());
        assert!(c.get("b#320").is_some());
        c.insert("c#320".into(), tex("c"), third);
        assert_eq!(c.len(), 2, "the budget still has to be enforced");
        assert!(c.get("c#320").is_some(), "the tile just decoded is not the one to throw away");
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

    /// A recycled filename names different pixels: every decoded size of that key must go, and
    /// its claim with them, or the grid keeps painting the deleted image.
    #[test]
    fn forget_drops_every_size_of_one_key() {
        let ctx = egui::Context::default();
        let tex = ctx.load_texture("t", egui::ColorImage::filled([1, 1], egui::Color32::RED), egui::TextureOptions::LINEAR);
        let mut c = ThumbCache::default();
        c.insert("u1/a/1.png#320".into(), tex.clone(), 1024);
        c.insert("u1/a/1.png#1024".into(), tex.clone(), 4096);
        c.insert("u1/a/10.png#320".into(), tex.clone(), 1024);
        c.claim("u1/a/1.png#512");
        c.forget("u1/a/1.png");
        assert!(c.get("u1/a/1.png#320").is_none());
        assert!(c.get("u1/a/1.png#1024").is_none());
        // A key the stale one merely prefixes must survive.
        assert!(c.get("u1/a/10.png#320").is_some());
        assert_eq!(c.bytes, 1024, "freed bytes must return to the budget");
        assert!(c.claim("u1/a/1.png#512"), "the dropped claim is re-issuable");
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

    fn base_params() -> Params {
        Params {
            checkpoint: "current.safetensors".into(),
            positive: "old prompt".into(),
            negative: "old neg".into(),
            steps: 20,
            cfg: 7.0,
            sampler: "euler".into(),
            scheduler: "normal".into(),
            seed: 100,
            ..Default::default()
        }
    }

    #[test]
    fn remix_diff_lists_only_changed_fields() {
        let meta = ImageMeta {
            models: vec!["current.safetensors".into()],
            positive: Some("new prompt".into()),
            negative: Some("old neg".into()),
            sampler: Some("dpmpp_2m".into()),
            scheduler: Some("normal".into()),
            steps: Some(30),
            cfg: Some(7.0),
            seed: Some(555),
            ..Default::default()
        };
        let rows = remix_diff_rows(&meta, &base_params());
        let fields: Vec<_> = rows.iter().map(|r| r.field).collect();
        // Model, negative, scheduler, cfg all match the current params and must be dropped.
        assert!(!fields.contains(&RemixField::Model));
        assert!(!fields.contains(&RemixField::Negative));
        assert!(!fields.contains(&RemixField::Scheduler));
        assert!(!fields.contains(&RemixField::Cfg));
        assert!(fields.contains(&RemixField::Positive));
        assert!(fields.contains(&RemixField::Sampler));
        assert!(fields.contains(&RemixField::Steps));
        assert!(fields.contains(&RemixField::Seed));
        let seed = rows.iter().find(|r| r.field == RemixField::Seed).unwrap();
        assert_eq!(seed.current, "100");
        assert_eq!(seed.new, "555");
    }

    #[test]
    fn remix_diff_model_prefers_unet_topology() {
        let meta = ImageMeta {
            unet: Some("flux1-dev.safetensors".into()),
            models: vec!["ignored.safetensors".into()],
            ..Default::default()
        };
        let rows = remix_diff_rows(&meta, &base_params());
        let model = rows.iter().find(|r| r.field == RemixField::Model).unwrap();
        assert_eq!(model.current, "current.safetensors");
        assert_eq!(model.new, "flux1-dev.safetensors");
    }

    #[test]
    fn remix_diff_flags_a_changed_lora_stack() {
        let meta = ImageMeta {
            loras: vec![LoraMeta {
                name: "style.safetensors".into(),
                strength_model: 0.8,
                strength_clip: Some(0.7),
                model_only: false,
            }],
            ..Default::default()
        };
        let rows = remix_diff_rows(&meta, &base_params());
        let lora = rows.iter().find(|r| r.field == RemixField::Loras).unwrap();
        assert_eq!(lora.current, "none");
        assert!(lora.new.contains("style.safetensors"));
    }

    #[test]
    fn remix_diff_hides_matching_lora_stack() {
        let meta = ImageMeta {
            loras: vec![LoraMeta {
                name: "style.safetensors".into(),
                strength_model: 0.8,
                strength_clip: Some(0.8),
                model_only: false,
            }],
            ..Default::default()
        };
        let mut params = base_params();
        params.loras = meta_to_active_loras(&meta.loras);
        let rows = remix_diff_rows(&meta, &params);
        assert!(rows.iter().all(|r| r.field != RemixField::Loras));
    }

    #[test]
    fn remix_diff_ignores_a_negative_seed() {
        let meta = ImageMeta { seed: Some(-1), ..Default::default() };
        let rows = remix_diff_rows(&meta, &base_params());
        assert!(rows.iter().all(|r| r.field != RemixField::Seed));
    }
}
