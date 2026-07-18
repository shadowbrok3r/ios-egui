//! Generation parameters and persisted settings shared between the UI and the async engine.

use serde::{Deserialize, Serialize};

/// Generation mode: a fresh image from noise, or refine an existing image.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Mode {
    Txt2Img,
    Img2Img,
}

/// Where the img2img input image comes from (Android's runtime has no file picker yet).
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Img2ImgSource {
    CurrentOutput,
    Url,
}

/// One LoRA stacked on the Create-tab graph (chained `LoraLoader` after the checkpoint).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ActiveLora {
    /// Exact `lora_name` as ComfyUI knows it (`models/loras` relative path).
    pub file: String,
    pub strength_model: f32,
    pub strength_clip: f32,
    /// Trigger tokens appended to [`Params::lora_triggers`] when this LoRA was added.
    #[serde(default)]
    pub injected: String,
}

/// Everything a KSampler txt2img/img2img workflow needs, plus the UI's mode selection.
#[derive(Clone, Serialize, Deserialize)]
pub struct Params {
    pub checkpoint: String,
    pub positive: String,
    pub negative: String,
    /// LoRA trigger / quality tags kept separate from the subject prompt.
    #[serde(default)]
    pub lora_triggers: String,
    pub steps: u32,
    pub cfg: f32,
    pub width: u32,
    pub height: u32,
    pub batch_size: u32,
    pub sampler: String,
    pub scheduler: String,
    pub seed: u64,
    pub randomize_seed: bool,
    pub denoise: f32,
    pub mode: Mode,
    pub img2img_source: Img2ImgSource,
    pub input_url: String,
    #[serde(default)]
    pub loras: Vec<ActiveLora>,
    /// Ordered enhance chain appended after the base graph's VAE decode.
    #[serde(default)]
    pub apps: Vec<AppStep>,
}

/// One configured app in the Create tab's enhance chain.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AppStep {
    /// [`crate::apps::AppDef::id`].
    pub app: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// The def version this step was configured against.
    #[serde(default)]
    pub version: u32,
    /// Knob overrides, keyed by knob id. Missing entries fall back to the def's default.
    #[serde(default)]
    pub values: std::collections::BTreeMap<String, serde_json::Value>,
}

fn yes() -> bool {
    true
}

impl AppStep {
    /// A step seeded with every knob's default, so the card renders without the def present.
    pub fn new(def: &crate::apps::AppDef) -> Self {
        Self {
            app: def.id.clone(),
            enabled: true,
            version: def.version,
            values: def.knobs.iter().map(|k| (k.id.clone(), k.default.clone())).collect(),
        }
    }

    /// Effective value for `id`: the stored override, else the def's default.
    pub fn value(&self, def: &crate::apps::AppDef, id: &str) -> Option<serde_json::Value> {
        self.values
            .get(id)
            .cloned()
            .or_else(|| def.knob(id).map(|k| k.default.clone()))
    }
}

/// Enhance chain copied from the Create tab for sharing.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AppPack {
    pub apps: Vec<AppStep>,
}

impl AppPack {
    pub const CLIP_TYPE: &'static str = "comfyui_android_apps_v1";

    pub fn to_clipboard_json(&self) -> String {
        serde_json::json!({ "type": Self::CLIP_TYPE, "apps": self.apps }).to_string()
    }

    pub fn from_clipboard_json(raw: &str) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_str(raw.trim()).ok()?;
        if v.get("type").and_then(|t| t.as_str()) != Some(Self::CLIP_TYPE) {
            return None;
        }
        let apps: Vec<AppStep> = serde_json::from_value(v.get("apps")?.clone()).ok()?;
        (!apps.is_empty()).then_some(Self { apps })
    }
}

impl Default for Params {
    fn default() -> Self {
        Self {
            checkpoint: String::new(),
            positive: String::new(),
            negative: "text, watermark, low quality".to_string(),
            lora_triggers: String::new(),
            steps: 20,
            cfg: 7.0,
            width: 1024,
            height: 1024,
            batch_size: 1,
            sampler: "euler".to_string(),
            scheduler: "normal".to_string(),
            seed: 0,
            randomize_seed: true,
            denoise: 0.6,
            mode: Mode::Txt2Img,
            img2img_source: Img2ImgSource::CurrentOutput,
            input_url: String::new(),
            loras: Vec::new(),
            apps: Vec::new(),
        }
    }
}

impl Params {
    /// Positive CLIP text: LoRA triggers (if any) then the subject prompt.
    pub fn combined_positive(&self) -> String {
        let triggers = self.lora_triggers.trim().trim_end_matches(',').trim();
        let subject = self.positive.trim();
        match (triggers.is_empty(), subject.is_empty()) {
            (true, _) => subject.to_string(),
            (_, true) => triggers.to_string(),
            _ => format!("{triggers}, {subject}"),
        }
    }
}

/// Sampler / steps / CFG bundle copied from a gallery image for Create paste.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SamplerPack {
    #[serde(default)]
    pub sampler: Option<String>,
    #[serde(default)]
    pub scheduler: Option<String>,
    #[serde(default)]
    pub steps: Option<u32>,
    #[serde(default)]
    pub cfg: Option<f32>,
}

impl SamplerPack {
    pub const CLIP_TYPE: &'static str = "comfyui_android_sampler_v1";

    pub fn is_empty(&self) -> bool {
        self.sampler.is_none()
            && self.scheduler.is_none()
            && self.steps.is_none()
            && self.cfg.is_none()
    }

    pub fn to_clipboard_json(&self) -> String {
        serde_json::json!({
            "type": Self::CLIP_TYPE,
            "sampler": self.sampler,
            "scheduler": self.scheduler,
            "steps": self.steps,
            "cfg": self.cfg,
        })
        .to_string()
    }

    pub fn from_clipboard_json(raw: &str) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_str(raw.trim()).ok()?;
        if v.get("type").and_then(|t| t.as_str()) != Some(Self::CLIP_TYPE) {
            return None;
        }
        let pack = Self {
            sampler: v.get("sampler").and_then(|x| x.as_str()).map(str::to_string),
            scheduler: v.get("scheduler").and_then(|x| x.as_str()).map(str::to_string),
            steps: v.get("steps").and_then(|x| x.as_u64()).map(|n| n as u32),
            cfg: v.get("cfg").and_then(|x| x.as_f64()).map(|n| n as f32),
        };
        (!pack.is_empty()).then_some(pack)
    }
}

/// LoRA stack copied from a gallery image for Create paste.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LoraPack {
    pub loras: Vec<ActiveLora>,
}

impl LoraPack {
    pub const CLIP_TYPE: &'static str = "comfyui_android_loras_v1";

    pub fn to_clipboard_json(&self) -> String {
        serde_json::json!({
            "type": Self::CLIP_TYPE,
            "loras": self.loras,
        })
        .to_string()
    }

    pub fn from_clipboard_json(raw: &str) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_str(raw.trim()).ok()?;
        if v.get("type").and_then(|t| t.as_str()) != Some(Self::CLIP_TYPE) {
            return None;
        }
        let loras: Vec<ActiveLora> = serde_json::from_value(v.get("loras")?.clone()).ok()?;
        (!loras.is_empty()).then_some(Self { loras })
    }
}

/// A named snapshot of Create-tab params, stored on-device.
#[derive(Clone, Serialize, Deserialize)]
pub struct CreatePreset {
    pub name: String,
    pub params: Params,
}

/// Server-published checkpoint catalog (`GET /checkpoint-catalog.json`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CheckpointCatalog {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub checkpoints: Vec<CheckpointEntry>,
}

/// One catalogued checkpoint (LoRA Manager / Civitai sidecar metadata).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CheckpointEntry {
    /// Path relative to `models/<directory>/` (ComfyUI loader name).
    pub file: String,
    /// `checkpoints`, `diffusion_models`, or `unet`.
    #[serde(default)]
    pub directory: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub bases: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub favorite: bool,
    #[serde(default)]
    pub from_civitai: bool,
    #[serde(default)]
    pub base_model: Option<String>,
    #[serde(default)]
    pub base_model_type: Option<String>,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub creator: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub preview: Option<String>,
    #[serde(default)]
    pub nsfw_level: Option<i32>,
    #[serde(default)]
    pub civitai_id: Option<i64>,
    #[serde(default)]
    pub civitai_model_id: Option<i64>,
    #[serde(default)]
    pub download_count: Option<i64>,
    #[serde(default)]
    pub thumbs_up: Option<i64>,
    /// Parsed sampler defaults from description / example metas (omitted when empty).
    #[serde(default)]
    pub recommended: Option<CheckpointRecommended>,
}

/// Recommended sampler settings for a checkpoint (`CheckpointEntry.recommended`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CheckpointRecommended {
    #[serde(default)]
    pub cfg: Option<f32>,
    #[serde(default)]
    pub cfg_min: Option<f32>,
    #[serde(default)]
    pub cfg_max: Option<f32>,
    #[serde(default)]
    pub steps: Option<u32>,
    #[serde(default)]
    pub steps_min: Option<u32>,
    #[serde(default)]
    pub steps_max: Option<u32>,
    #[serde(default)]
    pub sampler: Option<String>,
    #[serde(default)]
    pub scheduler: Option<String>,
    #[serde(default)]
    pub clip_skip: Option<u32>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
}

impl CheckpointEntry {
    pub fn display_name(&self) -> &str {
        if self.name.trim().is_empty() { &self.file } else { &self.name }
    }

    /// Label for a version row under a shared display name.
    pub fn version_label(&self) -> String {
        if let Some(v) = self.version.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
            return v.to_string();
        }
        file_basename(&self.file).to_string()
    }
}

impl CheckpointCatalog {
    pub fn entry(&self, file: &str) -> Option<&CheckpointEntry> {
        let base = file_basename(file);
        self.checkpoints
            .iter()
            .find(|e| e.file == file || file_basename(&e.file) == base)
    }

    pub fn bases_for(&self, checkpoint: &str) -> Vec<String> {
        self.entry(checkpoint).map(|e| e.bases.clone()).unwrap_or_default()
    }
}

/// Server-published LoRA catalog (`GET /comfyui-android/lora-catalog.json`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LoraCatalog {
    #[serde(default)]
    pub version: u32,
    /// Checkpoint filename (or basename) → base-model tags, e.g. `["sdxl"]`.
    #[serde(default)]
    pub checkpoints: std::collections::BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub loras: Vec<LoraEntry>,
}

/// One catalogued LoRA with recommended strengths and trigger words.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LoraEntry {
    /// Exact ComfyUI `lora_name` (path under `models/loras`).
    pub file: String,
    #[serde(default)]
    pub name: String,
    /// Base families this LoRA supports (`sdxl`, `flux`, `sd15`, `pony`, …).
    #[serde(default)]
    pub bases: Vec<String>,
    /// Optional explicit checkpoint filenames/basenames this LoRA is allowed with.
    #[serde(default)]
    pub checkpoints: Vec<String>,
    #[serde(default = "default_lora_strength")]
    pub strength_model: f32,
    #[serde(default = "default_lora_strength")]
    pub strength_clip: f32,
    #[serde(default)]
    pub strength_model_min: Option<f32>,
    #[serde(default)]
    pub strength_model_max: Option<f32>,
    /// Where `strength_*` was resolved (`usage_tips`, `description_range`, …).
    #[serde(default = "default_strength_source")]
    pub strength_source: String,
    /// Joined with `, ` and prepended to the positive prompt when the LoRA is added.
    #[serde(default)]
    pub trigger_words: Vec<String>,
    /// Optionally appended to the negative prompt when the LoRA is added.
    #[serde(default)]
    pub negative_words: Vec<String>,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

fn default_lora_strength() -> f32 {
    1.0
}

fn default_strength_source() -> String {
    "default".into()
}

impl LoraEntry {
    pub fn display_name(&self) -> &str {
        if self.name.trim().is_empty() { &self.file } else { &self.name }
    }

    pub fn trigger_text(&self) -> String {
        self.trigger_words
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub fn negative_text(&self) -> String {
        self.negative_words
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Model/CLIP strengths for Add, clamped to an optional recommended range.
    pub fn add_strengths(&self) -> (f32, f32) {
        let mut sm = self.strength_model;
        let mut sc = self.strength_clip;
        if let Some(lo) = self.strength_model_min {
            sm = sm.max(lo);
            sc = sc.max(lo);
        }
        if let Some(hi) = self.strength_model_max {
            sm = sm.min(hi);
            sc = sc.min(hi);
        }
        (sm, sc)
    }

    /// Compatible when listed for this checkpoint, sharing a base tag, or unrestricted.
    pub fn matches_checkpoint(&self, checkpoint: &str, model_bases: &[String]) -> bool {
        let ckpt = file_basename(checkpoint);
        if self.checkpoints.iter().any(|c| file_basename(c) == ckpt || c == checkpoint) {
            return true;
        }
        if self.bases.is_empty() && self.checkpoints.is_empty() {
            return true;
        }
        if model_bases.is_empty() {
            return false;
        }
        self.bases.iter().any(|b| {
            model_bases.iter().any(|m| m.eq_ignore_ascii_case(b.trim()))
        })
    }
}

impl LoraCatalog {
    pub fn bases_for_checkpoint(&self, checkpoint: &str) -> Vec<String> {
        let ckpt = file_basename(checkpoint);
        if let Some(bases) = self.checkpoints.get(checkpoint) {
            return bases.clone();
        }
        self.checkpoints
            .iter()
            .find(|(k, _)| file_basename(k) == ckpt)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }

    pub fn entry(&self, file: &str) -> Option<&LoraEntry> {
        let base = file_basename(file);
        self.loras
            .iter()
            .find(|e| e.file == file || file_basename(&e.file) == base)
    }
}

pub fn file_basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// Split a comma-separated trigger list into trimmed tokens.
pub fn split_triggers(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

fn trigger_present(haystacks: &[&str], trigger: &str) -> bool {
    let needle = trigger.trim().to_lowercase();
    if needle.is_empty() {
        return true;
    }
    for hay in haystacks {
        for part in split_triggers(hay) {
            if part.eq_ignore_ascii_case(&needle) {
                return true;
            }
        }
    }
    false
}

/// Append only the trigger tokens not already present in `dest` / `also_check`.
/// Returns the comma-joined tokens that were actually added (for later removal).
pub fn merge_triggers(dest: &mut String, triggers: &str, also_check: &str) -> String {
    let mut added = Vec::new();
    for t in split_triggers(triggers) {
        if trigger_present(&[dest.as_str(), also_check], &t) {
            continue;
        }
        added.push(t);
    }
    if added.is_empty() {
        return String::new();
    }
    let piece = added.join(", ");
    if dest.trim().is_empty() {
        *dest = piece.clone();
    } else {
        dest.push_str(", ");
        dest.push_str(&piece);
    }
    piece
}

/// Remove previously injected trigger tokens from a comma-separated field.
pub fn strip_injected(dest: &mut String, injected: &str) {
    let remove: std::collections::HashSet<String> = split_triggers(injected)
        .into_iter()
        .map(|t| t.to_lowercase())
        .collect();
    if remove.is_empty() {
        return;
    }
    let kept: Vec<String> = split_triggers(dest)
        .into_iter()
        .filter(|t| !remove.contains(&t.to_lowercase()))
        .collect();
    *dest = kept.join(", ");
}

/// Append negative words once (comma-separated) if not already present.
pub fn append_negatives(negative: &mut String, words: &str) {
    let words = words.trim();
    if words.is_empty() || negative.to_lowercase().contains(&words.to_lowercase()) {
        return;
    }
    if negative.trim().is_empty() {
        *negative = words.to_string();
    } else {
        negative.push_str(", ");
        negative.push_str(words);
    }
}

/// How the gallery orders results. Mirrors comfy-gate's `sort` values; the server silently falls
/// back to `new` for anything it doesn't know, and offers no sort-by-model.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GallerySort {
    Newest,
    Oldest,
    Largest,
    Smallest,
    Name,
}

impl GallerySort {
    pub fn param(self) -> &'static str {
        match self {
            Self::Newest => "new",
            Self::Oldest => "old",
            Self::Largest => "large",
            Self::Smallest => "small",
            Self::Name => "name",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Newest => "Newest",
            Self::Oldest => "Oldest",
            Self::Largest => "Largest",
            Self::Smallest => "Smallest",
            Self::Name => "Name",
        }
    }

    pub const ALL: &'static [Self] =
        &[Self::Newest, Self::Oldest, Self::Largest, Self::Smallest, Self::Name];
}

/// What the gallery's collapsing headers bucket by. The server only orders rows to match; the
/// header text is derived client-side.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GalleryGroup {
    None,
    Folder,
    Model,
    Date,
}

impl GalleryGroup {
    pub fn param(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Folder => "folder",
            Self::Model => "model",
            Self::Date => "date",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::None => "No grouping",
            Self::Folder => "Folder",
            Self::Model => "Model",
            Self::Date => "Date",
        }
    }

    pub const ALL: &'static [Self] = &[Self::Folder, Self::Model, Self::Date, Self::None];
}

/// Media-type filter for the gallery listing. Applied client-side (the listing API has no media
/// param), so a non-All value triggers the same load-the-whole-set paging as grouping does.
#[derive(Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum GalleryMedia {
    #[default]
    All,
    Images,
    Videos,
}

impl GalleryMedia {
    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All media",
            Self::Images => "Images",
            Self::Videos => "Videos",
        }
    }

    /// Whether `is_video` passes this filter.
    pub fn matches(self, is_video: bool) -> bool {
        match self {
            Self::All => true,
            Self::Images => !is_video,
            Self::Videos => is_video,
        }
    }

    pub const ALL: &'static [Self] = &[Self::All, Self::Images, Self::Videos];
}

/// The gallery's query + layout state, persisted so the view survives restarts.
#[derive(Clone, Serialize, Deserialize)]
pub struct GalleryView {
    /// Exact model name from `/gallery/api/facets`; empty = all models.
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub album: Option<i64>,
    /// Images / videos / everything, filtered client-side.
    #[serde(default)]
    pub media: GalleryMedia,
    pub sort: GallerySort,
    pub group: GalleryGroup,
    /// Tiles per row, 1..=3. At 1 the tiles show near-full-resolution images.
    pub columns: usize,
    /// Whether folder/model collapsing headers start expanded.
    #[serde(default = "default_true")]
    pub groups_open: bool,
}

fn default_true() -> bool {
    true
}

fn default_gallery_page() -> u64 {
    60
}

impl Default for GalleryView {
    fn default() -> Self {
        Self {
            model: String::new(),
            album: None,
            media: GalleryMedia::All,
            sort: GallerySort::Newest,
            group: GalleryGroup::Folder,
            columns: 3,
            groups_open: true,
        }
    }
}

impl GalleryView {
    /// Thumbnail edge to request for the current column count. One column is a full-width read, so
    /// it asks for comfy-gate's largest thumb (1024, its clamp ceiling) rather than the original —
    /// on a ~1080px-wide phone that is visually full-scale at a fraction of the bytes.
    pub fn thumb_size(&self) -> u32 {
        match self.columns {
            1 => 1024,
            2 => 512,
            _ => 320,
        }
    }
}

/// Per-style font sizes (points), applied to egui's `TextStyle` map.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FontSizes {
    pub heading: f32,
    pub body: f32,
    pub button: f32,
    pub small: f32,
    pub monospace: f32,
}

impl Default for FontSizes {
    fn default() -> Self {
        Self {
            heading: 18.0,
            body: 14.5,
            button: 14.5,
            small: 11.0,
            monospace: 12.5,
        }
    }
}

impl FontSizes {
    pub fn clamp(&mut self) {
        self.heading = self.heading.clamp(12.0, 36.0);
        self.body = self.body.clamp(10.0, 28.0);
        self.button = self.button.clamp(10.0, 28.0);
        self.small = self.small.clamp(8.0, 20.0);
        self.monospace = self.monospace.clamp(9.0, 24.0);
    }
}

/// Persisted to `<documents>/comfyui_settings.json` so the server + last params survive reinstalls.
#[derive(Clone, Serialize, Deserialize)]
pub struct Settings {
    pub server_url: String,
    #[serde(default)]
    pub api_key: String,
    /// The signed-in account, remembered only to label the Settings tab.
    #[serde(default)]
    pub username: String,
    /// `cg_session` cookie token from a `POST /login`, sent alongside any API key.
    #[serde(default)]
    pub session: String,
    pub params: Params,
    #[serde(default)]
    pub gallery: GalleryView,
    /// Gallery search box text.
    #[serde(default)]
    pub gallery_q: String,
    /// How many gallery rows to fetch per page / Load more (20..=500).
    #[serde(default = "default_gallery_page")]
    pub gallery_page: u64,
    /// Auto-follow: pan/zoom the graph to whichever node is currently executing.
    #[serde(default)]
    pub auto_follow: bool,
    /// Auto-arrange the canvas when a workflow is loaded.
    #[serde(default = "default_true")]
    pub auto_arrange: bool,
    #[serde(default)]
    pub fonts: FontSizes,
    /// Name of the last opened graph workflow.
    #[serde(default)]
    pub workflow_name: String,
    /// UI-format JSON of the last opened graph, restored after reconnect.
    #[serde(default)]
    pub workflow_json: Option<String>,
    /// On-device Create-tab presets (prompts, sampler, LoRAs, …).
    #[serde(default)]
    pub presets: Vec<CreatePreset>,
    /// Name of the last-applied Create preset (empty = none / custom).
    #[serde(default)]
    pub selected_preset: String,
}

/// One album from `GET /gallery/api/albums`. Albums are per-account (namespaced by the credential),
/// and `count` is the live count of members still present in the gallery index.
#[derive(Clone, Debug, Deserialize)]
pub struct Album {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub count: i64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct AlbumList {
    pub albums: Vec<Album>,
}

/// One distinct model name across the account's gallery, with how many images used it.
#[derive(Clone, Debug, Deserialize)]
pub struct ModelFacet {
    pub name: String,
    #[serde(default)]
    pub count: i64,
}

/// `GET /gallery/api/facets` — the source of the model filter's options.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Facets {
    #[serde(default)]
    pub models: Vec<ModelFacet>,
}

/// One image in the server's `/gallery/api/list` response.
#[derive(Clone, Debug, Deserialize)]
pub struct GalleryItem {
    pub subfolder: String,
    pub filename: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub is_video: bool,
    #[serde(default)]
    pub has_workflow: bool,
    #[serde(default)]
    pub models: Vec<String>,
    /// Unix mtime seconds when the gallery API provides it.
    #[serde(default)]
    pub mtime: Option<f64>,
}

impl GalleryItem {
    /// Cache key `subfolder/filename`, matching the engine's thumb/full message keys.
    pub fn key(&self) -> String {
        format!("{}/{}", self.subfolder, self.filename)
    }

    /// Thumbnail cache key. The size rides in the key because changing the column count re-requests
    /// the same image at a new edge, and the two decodes must not collide.
    pub fn thumb_key(&self, size: u32) -> String {
        format!("{}/{}#{size}", self.subfolder, self.filename)
    }

    /// Header label when grouping by model: every model the image's graph referenced.
    pub fn model_label(&self) -> String {
        if self.models.is_empty() {
            return "No model recorded".to_string();
        }
        self.models.join(" + ")
    }

    /// Group header label: the subfolder without its first path segment.
    ///
    /// Every subfolder comfy-gate reports is namespace-prefixed (`<ns>` or `<ns>/sub/dir`), and the
    /// namespace is an opaque account id — so a subfolder with nothing after it is the account's
    /// output root, not a folder anyone named.
    pub fn group_label(&self) -> String {
        let s = self.subfolder.trim_matches('/');
        match s.split_once('/') {
            Some((_, rest)) if !rest.is_empty() => rest.to_string(),
            _ => "Output".to_string(),
        }
    }

    /// Group header when grouping by date: `YYYY-MM-DD` from mtime, path, or filename.
    pub fn date_label(&self) -> String {
        if let Some(secs) = self.mtime.filter(|s| s.is_finite() && *s > 0.0) {
            return unix_ymd(secs as i64);
        }
        extract_ymd(&self.subfolder)
            .or_else(|| extract_ymd(&self.filename))
            .unwrap_or_else(|| "Unknown date".into())
    }
}

/// First `YYYY-MM-DD` substring in `s`, if any.
fn extract_ymd(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut i = 0;
    while i + 10 <= b.len() {
        if b[i].is_ascii_digit()
            && b[i + 1].is_ascii_digit()
            && b[i + 2].is_ascii_digit()
            && b[i + 3].is_ascii_digit()
            && b[i + 4] == b'-'
            && b[i + 5].is_ascii_digit()
            && b[i + 6].is_ascii_digit()
            && b[i + 7] == b'-'
            && b[i + 8].is_ascii_digit()
            && b[i + 9].is_ascii_digit()
        {
            return Some(s[i..i + 10].to_string());
        }
        i += 1;
    }
    None
}

/// UTC calendar date for a unix timestamp (`YYYY-MM-DD`).
fn unix_ymd(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_label_strips_user_root() {
        let item = |sub: &str| GalleryItem {
            subfolder: sub.into(),
            filename: "a.png".into(),
            size: 0,
            is_video: false,
            has_workflow: false,
            models: Vec::new(),
            mtime: None,
        };
        assert_eq!(item("user_abc/Character/2026-07-16").group_label(), "Character/2026-07-16");
        // A bare namespace is the account's output root — never show the raw account id.
        assert_eq!(item("shadowbroker_531d823e-4a3b-46c8-9550-2e8f").group_label(), "Output");
        assert_eq!(item("").group_label(), "Output");
        assert_eq!(item("user_abc/Character/2026-07-16").date_label(), "2026-07-16");
        assert_eq!(
            GalleryItem {
                subfolder: "u1".into(),
                filename: "shot_2026-01-02_x.png".into(),
                size: 0,
                is_video: false,
                has_workflow: false,
                models: Vec::new(),
                mtime: None,
            }
            .date_label(),
            "2026-01-02"
        );
    }

    #[test]
    fn inject_and_strip_triggers() {
        let mut triggers = String::new();
        let inj = merge_triggers(&mut triggers, "foo style, bar", "a cat");
        assert_eq!(inj, "foo style, bar");
        assert_eq!(triggers, "foo style, bar");
        // Already present — not re-added.
        let again = merge_triggers(&mut triggers, "foo style, baz", "a cat");
        assert_eq!(again, "baz");
        assert_eq!(triggers, "foo style, bar, baz");
        strip_injected(&mut triggers, "foo style, baz");
        assert_eq!(triggers, "bar");
        assert_eq!(
            Params {
                lora_triggers: "masterpiece, ".into(),
                positive: "a cat".into(),
                ..Default::default()
            }
            .combined_positive(),
            "masterpiece, a cat"
        );
    }

    #[test]
    fn lora_matches_by_base_and_checkpoint() {
        let entry = LoraEntry {
            file: "style.safetensors".into(),
            name: "Style".into(),
            bases: vec!["sdxl".into()],
            checkpoints: vec![],
            strength_model: 0.8,
            strength_clip: 0.8,
            strength_model_min: None,
            strength_model_max: None,
            strength_source: "default".into(),
            trigger_words: vec!["style".into()],
            negative_words: vec![],
            notes: String::new(),
            tags: vec![],
        };
        assert!(entry.matches_checkpoint(
            "models/juggernautXL.safetensors",
            &["sdxl".into()],
        ));
        assert!(!entry.matches_checkpoint("flux1-dev.safetensors", &["flux".into()]));
        assert!(!entry.matches_checkpoint("unknown.safetensors", &[]));
    }
}

/// `/gallery/api/list` response page.
#[derive(Clone, Debug, Deserialize)]
pub struct GalleryPage {
    pub total: u64,
    #[serde(default)]
    pub offset: u64,
    pub items: Vec<GalleryItem>,
}

/// Sampler names shown before a server reports its real list (KSampler defaults on a stock ComfyUI).
pub const FALLBACK_SAMPLERS: &[&str] = &[
    "euler",
    "euler_ancestral",
    "heun",
    "dpm_2",
    "dpm_2_ancestral",
    "lms",
    "dpmpp_2s_ancestral",
    "dpmpp_2m",
    "dpmpp_2m_sde",
    "dpmpp_3m_sde",
    "dpmpp_sde",
    "ddim",
    "uni_pc",
    "lcm",
];

/// Scheduler names shown before a server reports its real list.
pub const FALLBACK_SCHEDULERS: &[&str] = &[
    "normal",
    "karras",
    "exponential",
    "sgm_uniform",
    "simple",
    "ddim_uniform",
    "beta",
];

pub fn fallback_vec(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}
