//! Generation parameters and persisted settings shared between the UI and the async engine.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

/// Generation mode: a fresh image from noise, refine an existing image, or a Wan i2v video.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Mode {
    Txt2Img,
    Img2Img,
    Video,
}

/// Where the img2img input image comes from (Android's runtime has no file picker yet).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Img2ImgSource {
    CurrentOutput,
    Url,
    /// A photo picked from the device this session; the bytes live outside `Params`.
    Picked,
}

/// Which loader topology a model needs: one all-in-one checkpoint, or a bare diffusion model
/// paired with separately-loaded text encoder(s) and VAE.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum ModelKind {
    /// `CheckpointLoaderSimple` -> MODEL + CLIP + VAE.
    #[default]
    Checkpoint,
    /// `UNETLoader` + `CLIPLoader`/`DualCLIPLoader` + `VAELoader`.
    Diffusion,
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
    /// Chain through `LoraLoaderModelOnly`, leaving the CLIP untouched.
    #[serde(default)]
    pub model_only: bool,
}

/// Canonical Wan negative prompt (anti-3D prefix + the standard Chinese quality block).
pub const WAN_NEGATIVE: &str = "(((realistic))), ((photograph)), 色调艳丽，过曝，静态，细节模糊不清，字幕，风格，作品，画作，画面，静止，整体发灰，最差质量，低质量，JPEG压缩残留，丑陋的，残缺的，多余的手指，画得不好的手部，画得不好的脸部，畸形的，毁容的，形态畸形的肢体，手指融合，静止不动的画面，杂乱的背景，三条腿，背景人很多，倒着走";

/// Wan 2.2 image-to-video settings, seeded with the user's proven-working defaults.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VideoParams {
    /// `UNETLoader.unet_name` for the high-noise expert.
    pub unet_high: String,
    /// `UNETLoader.unet_name` for the low-noise expert.
    pub unet_low: String,
    /// `UNETLoader.weight_dtype`; empty means `"default"`.
    pub weight_dtype: String,
    /// `CLIPLoader.clip_name`.
    pub clip_name: String,
    /// `CLIPLoader.type`.
    pub clip_type: String,
    /// `CLIPLoader.device`; empty omits the input.
    pub clip_device: String,
    /// `VAELoader.vae_name`.
    pub vae_name: String,
    pub width: u32,
    pub height: u32,
    /// Frame count; Wan requires `length % 4 == 1`.
    pub length: u32,
    /// Model-only LoRAs chained onto the high-noise expert.
    pub loras_high: Vec<ActiveLora>,
    /// Model-only LoRAs chained onto the low-noise expert.
    pub loras_low: Vec<ActiveLora>,
    /// `ModelSamplingSD3.shift`.
    pub shift: f32,
    /// Total sampler steps shared by both experts.
    pub steps: u32,
    /// Step at which the high expert hands off to the low expert.
    pub split_step: u32,
    pub cfg_high: f32,
    pub cfg_low: f32,
    pub sampler: String,
    pub scheduler: String,
    /// Render a text-to-video graph with no start image.
    #[serde(default)]
    pub video_t2v: bool,
    /// Append a `RIFE VFI` frame-interpolation pass when the server has the node.
    pub rife: bool,
    /// `RIFE VFI.ckpt_name`.
    pub rife_ckpt: String,
    /// `RIFE VFI.multiplier`; output frame rate is `16 * rife_multiplier`.
    pub rife_multiplier: u32,
    /// Insert `easy cleanGpuUsed` passthroughs to free VRAM, when the server has the node.
    pub gpu_clean: bool,
}

impl Default for VideoParams {
    fn default() -> Self {
        let model_lora = |file: &str, s: f32| ActiveLora {
            file: file.to_string(),
            strength_model: s,
            strength_clip: s,
            injected: String::new(),
            model_only: true,
        };
        Self {
            unet_high: "Wan/wan2.2_i2v_high_noise_14B_fp8_scaled.safetensors".into(),
            unet_low: "Wan/wan2.2_i2v_low_noise_14B_fp8_scaled.safetensors".into(),
            weight_dtype: "default".into(),
            clip_name: "umt5_xxl_fp8_e4m3fn_scaled.safetensors".into(),
            clip_type: "wan".into(),
            clip_device: "cpu".into(),
            vae_name: "wan_2.1_vae.safetensors".into(),
            width: 560,
            height: 720,
            length: 81,
            loras_high: vec![
                model_lora("Wan/wan2.2_i2v_lightx2v_4steps_lora_v1_high_noise.safetensors", 0.7),
                model_lora("Wan/SmoothMixAnimationStyle_High.safetensors", 0.6),
            ],
            loras_low: vec![
                model_lora("Wan/wan2.2_i2v_lightx2v_4steps_lora_v1_low_noise.safetensors", 1.0),
                model_lora("Wan/SmoothMixAnimation_Low.safetensors", 0.6),
            ],
            shift: 5.0,
            steps: 8,
            split_step: 4,
            cfg_high: 2.5,
            cfg_low: 1.0,
            sampler: "euler".into(),
            scheduler: "simple".into(),
            video_t2v: false,
            rife: true,
            rife_ckpt: "rife49.pth".into(),
            rife_multiplier: 2,
            gpu_clean: true,
        }
    }
}

/// Everything a KSampler txt2img/img2img workflow needs, plus the UI's mode selection.
#[derive(Clone, Serialize, Deserialize)]
pub struct Params {
    pub checkpoint: String,
    /// Which loader topology [`Self::checkpoint`] / [`Self::unet_name`] needs.
    #[serde(default)]
    pub model_kind: ModelKind,
    /// `UNETLoader.unet_name` when `model_kind` is [`ModelKind::Diffusion`].
    #[serde(default)]
    pub unet_name: String,
    /// `UNETLoader.weight_dtype`; empty means `"default"`.
    #[serde(default)]
    pub weight_dtype: String,
    /// Text encoders: one emits `CLIPLoader`, two emit `DualCLIPLoader`.
    #[serde(default)]
    pub clip_names: Vec<String>,
    /// `CLIPLoader.type`; empty means `"stable_diffusion"`.
    #[serde(default)]
    pub clip_type: String,
    /// `CLIPLoader.device`; empty omits the input.
    #[serde(default)]
    pub clip_device: String,
    /// `VAELoader.vae_name`.
    #[serde(default)]
    pub vae_name: String,
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
    /// Route img2img through a SetLatentNoiseMask branch keyed off the input's alpha.
    #[serde(default)]
    pub inpaint_mask: bool,
    pub input_url: String,
    #[serde(default)]
    pub loras: Vec<ActiveLora>,
    /// Ordered enhance chain appended after the base graph's VAE decode.
    #[serde(default)]
    pub apps: Vec<AppStep>,
    /// Wan i2v settings used when `mode` is [`Mode::Video`].
    #[serde(default)]
    pub video: VideoParams,
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
            model_kind: ModelKind::Checkpoint,
            unet_name: String::new(),
            weight_dtype: String::new(),
            clip_names: Vec::new(),
            clip_type: String::new(),
            clip_device: String::new(),
            vae_name: String::new(),
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
            inpaint_mask: false,
            input_url: String::new(),
            loras: Vec::new(),
            apps: Vec::new(),
            video: VideoParams::default(),
        }
    }
}

impl Params {
    /// Reset creative state (prompts, LoRAs, enhance chain, video, mode, seed) to defaults,
    /// keeping the selected model and its companions.
    pub fn reset_creative(&mut self) {
        let d = Params::default();
        self.positive = d.positive;
        self.negative = d.negative;
        self.lora_triggers = d.lora_triggers;
        self.steps = d.steps;
        self.cfg = d.cfg;
        self.width = d.width;
        self.height = d.height;
        self.batch_size = d.batch_size;
        self.sampler = d.sampler;
        self.scheduler = d.scheduler;
        self.seed = d.seed;
        self.randomize_seed = d.randomize_seed;
        self.denoise = d.denoise;
        self.mode = d.mode;
        self.img2img_source = d.img2img_source;
        self.inpaint_mask = d.inpaint_mask;
        self.input_url = d.input_url;
        self.loras = d.loras;
        self.apps = d.apps;
        self.video = d.video;
    }

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

    /// The selected model's filename, whichever loader it goes through.
    pub fn model_file(&self) -> &str {
        match self.model_kind {
            ModelKind::Checkpoint => &self.checkpoint,
            ModelKind::Diffusion => &self.unet_name,
        }
    }

    /// Text encoders with blanks dropped, capped at the two a `DualCLIPLoader` accepts.
    pub fn active_clips(&self) -> Vec<String> {
        self.clip_names
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .take(2)
            .map(str::to_string)
            .collect()
    }

    /// `UNETLoader.weight_dtype`, defaulted.
    pub fn effective_weight_dtype(&self) -> String {
        match self.weight_dtype.trim() {
            "" => "default".to_string(),
            s => s.to_string(),
        }
    }

    /// `CLIPLoader.type`, defaulted to what the Anima/Qwen recipe uses.
    pub fn effective_clip_type(&self) -> String {
        match self.clip_type.trim() {
            "" => "stable_diffusion".to_string(),
            s => s.to_string(),
        }
    }

    /// Why the diffusion path can't be queued yet, if anything is missing.
    pub fn missing_model_part(&self) -> Option<&'static str> {
        match self.model_kind {
            ModelKind::Checkpoint => {
                self.checkpoint.trim().is_empty().then_some("Pick a checkpoint first")
            }
            ModelKind::Diffusion => {
                if self.unet_name.trim().is_empty() {
                    Some("Pick a diffusion model first")
                } else if self.active_clips().is_empty() {
                    Some("Pick a text encoder for this model")
                } else if self.vae_name.trim().is_empty() {
                    Some("Pick a VAE for this model")
                } else {
                    None
                }
            }
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

/// Keep the first entry for each `file`; Create is a linear stack, not a side-by-side graph.
pub fn dedupe_loras(loras: Vec<ActiveLora>) -> Vec<ActiveLora> {
    let mut seen = HashSet::new();
    loras
        .into_iter()
        .filter(|l| !l.file.is_empty() && seen.insert(l.file.clone()))
        .collect()
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
        let loras = dedupe_loras(loras);
        (!loras.is_empty()).then_some(Self { loras })
    }
}

/// A reusable recurring character: identity tags, its LoRA stack, trigger words, per-character
/// negatives, an optional preferred checkpoint, and an optional face-detailer prompt.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CharacterCard {
    pub name: String,
    /// Danbooru identity tags merged into the positive prompt (`1girl, silver hair, red eyes`).
    #[serde(default)]
    pub identity: String,
    /// LoRA activator tokens merged into `lora_triggers`.
    #[serde(default)]
    pub triggers: String,
    /// Appended to the negative prompt while applied.
    #[serde(default)]
    pub negatives: String,
    /// LoRAs added to the active stack, with strengths.
    #[serde(default)]
    pub loras: Vec<ActiveLora>,
    /// Preferred checkpoint / diffusion-model filename (empty = keep current).
    #[serde(default)]
    pub checkpoint: String,
    /// Switch to [`Self::checkpoint`] on apply; never silent, a per-card opt-in.
    #[serde(default)]
    pub switch_checkpoint: bool,
    /// Face-detailer wildcard prompt, piped into the `face.detailer` app when enabled.
    #[serde(default)]
    pub face_prompt: String,
    /// Gallery item key (`subfolder/filename`) of the card's profile picture; empty = none.
    #[serde(default)]
    pub portrait_key: String,
    /// Server album id collecting this character's matched images; 0 = none yet.
    #[serde(default)]
    pub album_id: i64,
}

/// Exactly what applying a [`CharacterCard`] changed, so removal reverses it token-for-token.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AppliedCharacter {
    pub name: String,
    /// Tokens added to `positive`.
    #[serde(default)]
    pub pos_injected: String,
    /// Tokens added to `lora_triggers` (identity triggers + each added LoRA's catalog triggers).
    #[serde(default)]
    pub trig_injected: String,
    /// Tokens added to `negative`.
    #[serde(default)]
    pub neg_injected: String,
    /// LoRA files added to the stack.
    #[serde(default)]
    pub loras: Vec<String>,
    /// Checkpoint restored on removal when the card switched models.
    #[serde(default)]
    pub prev_checkpoint: String,
    #[serde(default)]
    pub prev_unet: String,
    #[serde(default)]
    pub prev_model_kind: Option<ModelKind>,
    #[serde(default)]
    pub switched_checkpoint: bool,
    /// Face-detailer `face_prompt` restored on removal when the card set it.
    #[serde(default)]
    pub face_touched: bool,
    #[serde(default)]
    pub face_prev: String,
}

/// Append a comma-joined token piece to an accumulator, skipping blanks.
fn push_tokens(dest: &mut String, piece: &str) {
    let piece = piece.trim();
    if piece.is_empty() {
        return;
    }
    if dest.is_empty() {
        *dest = piece.to_string();
    } else {
        dest.push_str(", ");
        dest.push_str(piece);
    }
}

impl Params {
    /// Inject a character's identity tags, trigger words, negatives, and LoRA stack, recording what
    /// changed so [`Self::remove_character`] reverses it exactly. `lora_words(file)` yields the
    /// catalog `(triggers, negatives)` for each newly added LoRA.
    pub fn apply_character(
        &mut self,
        card: &CharacterCard,
        lora_words: impl Fn(&str) -> (String, String),
    ) -> AppliedCharacter {
        let mut applied = AppliedCharacter { name: card.name.clone(), ..Default::default() };
        applied.pos_injected = merge_triggers(&mut self.positive, &card.identity, &self.lora_triggers);
        let mut trig = merge_triggers(&mut self.lora_triggers, &card.triggers, &self.positive);
        let mut neg = merge_triggers(&mut self.negative, &card.negatives, "");
        for lora in &card.loras {
            if self.loras.iter().any(|l| l.file == lora.file) {
                continue;
            }
            let (t, n) = lora_words(&lora.file);
            let inj = merge_triggers(&mut self.lora_triggers, &t, &self.positive);
            push_tokens(&mut trig, &inj);
            let neg_inj = merge_triggers(&mut self.negative, &n, "");
            push_tokens(&mut neg, &neg_inj);
            self.loras.push(ActiveLora {
                file: lora.file.clone(),
                strength_model: lora.strength_model,
                strength_clip: lora.strength_clip,
                injected: String::new(),
                model_only: lora.model_only,
            });
            applied.loras.push(lora.file.clone());
        }
        applied.trig_injected = trig;
        applied.neg_injected = neg;
        applied
    }

    /// Reverse [`Self::apply_character`]'s prompt/LoRA edits. Checkpoint and face-detailer
    /// restoration are the caller's, since those touch app state beyond `Params`.
    pub fn remove_character(&mut self, applied: &AppliedCharacter) {
        strip_injected(&mut self.positive, &applied.pos_injected);
        strip_injected(&mut self.lora_triggers, &applied.trig_injected);
        strip_injected(&mut self.negative, &applied.neg_injected);
        let drop: HashSet<&str> = applied.loras.iter().map(String::as_str).collect();
        self.loras.retain(|l| !drop.contains(l.file.as_str()));
    }
}

/// A [`CharacterCard`] copied to the clipboard for sharing / import.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CharacterPack {
    pub card: CharacterCard,
}

impl CharacterPack {
    pub const CLIP_TYPE: &'static str = "comfyui_android_character_v1";

    pub fn to_clipboard_json(&self) -> String {
        serde_json::json!({ "type": Self::CLIP_TYPE, "card": self.card }).to_string()
    }

    pub fn from_clipboard_json(raw: &str) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_str(raw.trim()).ok()?;
        if v.get("type").and_then(|t| t.as_str()) != Some(Self::CLIP_TYPE) {
            return None;
        }
        let card: CharacterCard = serde_json::from_value(v.get("card")?.clone()).ok()?;
        (!card.name.trim().is_empty()).then_some(Self { card })
    }
}

/// A named snapshot of Create-tab params, stored on-device.
#[derive(Clone, Serialize, Deserialize)]
pub struct CreatePreset {
    pub name: String,
    pub params: Params,
}

/// One recorded Create-tab prompt pair for the history scrubber.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptHist {
    #[serde(default)]
    pub positive: String,
    #[serde(default)]
    pub negative: String,
}

/// Newest-last cap on [`Settings::prompt_history`].
pub const PROMPT_HISTORY_CAP: usize = 60;

/// Append `entry` as the newest history item, skipping an exact repeat of the current newest
/// and evicting the oldest past [`PROMPT_HISTORY_CAP`].
pub fn push_prompt_hist(hist: &mut Vec<PromptHist>, entry: PromptHist) {
    if hist.last() == Some(&entry) {
        return;
    }
    hist.push(entry);
    let overflow = hist.len().saturating_sub(PROMPT_HISTORY_CAP);
    if overflow > 0 {
        hist.drain(..overflow);
    }
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
    /// Companion models for diffusion-model entries, when the catalog knows them.
    #[serde(default)]
    pub clip_names: Option<Vec<String>>,
    #[serde(default)]
    pub clip_type: Option<String>,
    #[serde(default)]
    pub vae: Option<String>,
    #[serde(default)]
    pub weight_dtype: Option<String>,
}

impl CheckpointRecommended {
    /// Short inline hint: steps, CFG, size (and sampler only if nothing else).
    pub fn short_hint(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(v) = self.steps {
            parts.push(format!("steps {v}"));
        } else if let (Some(a), Some(b)) = (self.steps_min, self.steps_max) {
            parts.push(format!("steps {a}–{b}"));
        }
        if let Some(v) = self.cfg {
            parts.push(format!("CFG {v}"));
        } else if let (Some(a), Some(b)) = (self.cfg_min, self.cfg_max) {
            parts.push(format!("CFG {a}–{b}"));
        }
        if let (Some(w), Some(h)) = (self.width, self.height) {
            parts.push(format!("{w}×{h}"));
        }
        if parts.is_empty() {
            if let Some(s) = self.sampler.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
                parts.push(s.to_string());
            }
        }
        (!parts.is_empty()).then(|| parts.join(" · "))
    }
}

impl CheckpointEntry {
    pub fn display_name(&self) -> &str {
        if self.name.trim().is_empty() { &self.file } else { &self.name }
    }

    /// Loader topology implied by `directory`, or `None` when the catalog didn't say.
    pub fn model_kind(&self) -> Option<ModelKind> {
        match self.directory.trim().to_ascii_lowercase().as_str() {
            "diffusion_models" | "diffusion_model" | "unet" | "unets" => Some(ModelKind::Diffusion),
            "checkpoints" | "checkpoint" => Some(ModelKind::Checkpoint),
            _ => None,
        }
    }

    /// Label for a version row under a shared display name.
    pub fn version_label(&self) -> String {
        if let Some(v) = self.version.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
            return v.to_string();
        }
        file_basename(&self.file).to_string()
    }

    /// Model-family bucket for the Create list.
    /// Prefers Civitai `base_model` (Pony, Illustrious, Anima, …) over coarse `bases` tags (sdxl).
    pub fn family_label(&self) -> String {
        if let Some(b) = self.base_model.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
            return pretty_model_family(b);
        }
        if let Some(b) = self.base_model_type.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
            return pretty_model_family(b);
        }
        if let Some(b) = self.bases.iter().map(|s| s.trim()).find(|s| !s.is_empty()) {
            return pretty_model_family(b);
        }
        "Other".into()
    }
}

/// How checkpoint rows are ordered within Favorites / each family group.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum CheckpointSort {
    #[default]
    Name,
    Recent,
}

impl CheckpointSort {
    pub fn label(self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Recent => "Recent",
        }
    }
}

/// Which on-device pipeline the Local NPU path runs (feature `local-npu`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum LocalBackend {
    /// SD1.5 at 512²: CLIP on CPU, UNet + VAE on HTP, from a `qnn/`-style dir with `unet.bin`.
    #[default]
    Sd15,
    /// Anima DiT at 1024²: a pack dir carrying the `ANIMA` marker.
    Anima,
}

#[cfg_attr(not(feature = "local-npu"), allow(dead_code))]
impl LocalBackend {
    pub fn label(self) -> &'static str {
        match self {
            Self::Sd15 => "SD1.5",
            Self::Anima => "Anima",
        }
    }
}

/// Cap on persisted MRU checkpoint filenames.
pub const CHECKPOINT_RECENT_MAX: usize = 40;

/// Human label for a base-model tag (`sdxl` → `SDXL`, `sd15` → `SD 1.5`).
pub fn pretty_model_family(raw: &str) -> String {
    let t = raw.trim();
    if t.is_empty() {
        return "Other".into();
    }
    let key = t.to_ascii_lowercase().replace([' ', '_', '-', '.'], "");
    match key.as_str() {
        "sd15" | "stablediffusion15" => "SD 1.5".into(),
        "sd20" | "sd2" => "SD 2.0".into(),
        "sd21" => "SD 2.1".into(),
        "sdxl" | "sdxl10" | "stablediffusionxl" => "SDXL".into(),
        "sdxlturbo" => "SDXL Turbo".into(),
        "sd3" | "sd30" | "stablediffusion3" => "SD 3".into(),
        "sd35" | "sd35large" | "sd35medium" => "SD 3.5".into(),
        "pony" | "ponydiffusion" | "ponyxl" => "Pony".into(),
        "illustrious" | "illustriousxl" => "Illustrious".into(),
        "noobai" | "noobaixl" => "NoobAI".into(),
        "flux" | "flux1" | "fluxdev" | "flux1dev" | "flux1d" => "Flux".into(),
        "fluxschnell" | "flux1schnell" | "flux1s" => "Flux Schnell".into(),
        "auraflow" => "AuraFlow".into(),
        "hunyuan" | "hunyuandit" | "hunyuanvideo" => "Hunyuan".into(),
        "cascade" | "stablecascade" => "Cascade".into(),
        "pixart" | "pixarta" | "pixartsigma" | "pixarte" => "PixArt".into(),
        "qwen" | "qwenimage" => "Qwen".into(),
        "anima" => "Anima".into(),
        "svd" | "stablevideodiffusion" => "SVD".into(),
        "wan" | "wanvideo" | "wan21" => "Wan".into(),
        "lumina" | "lumina2" => "Lumina".into(),
        "chroma" => "Chroma".into(),
        "hidream" => "HiDream".into(),
        other => {
            // Title-case unknown tags; keep short acronyms uppercase.
            if other.len() <= 5 && other.chars().all(|c| c.is_ascii_alphanumeric()) {
                other.to_ascii_uppercase()
            } else {
                let mut out = String::new();
                for (i, part) in t.split(|c: char| c == ' ' || c == '_' || c == '-').enumerate() {
                    if part.is_empty() {
                        continue;
                    }
                    if i > 0 {
                        out.push(' ');
                    }
                    let mut chars = part.chars();
                    if let Some(first) = chars.next() {
                        out.extend(first.to_uppercase());
                        out.push_str(&chars.as_str().to_ascii_lowercase());
                    }
                }
                if out.is_empty() { "Other".into() } else { out }
            }
        }
    }
}

/// Family bucket for an installed checkpoint (catalog metadata, else `"Other"`).
pub fn checkpoint_family(entry: Option<&CheckpointEntry>) -> String {
    entry.map(|e| e.family_label()).unwrap_or_else(|| "Other".into())
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

    /// Short inline hint for active LoRA cards.
    pub fn strength_hint(&self) -> String {
        let mut parts = Vec::new();
        if (self.strength_clip - self.strength_model).abs() < 0.005 {
            parts.push(format!("strength {:.2}", self.strength_model));
        } else {
            parts.push(format!(
                "model {:.2} · CLIP {:.2}",
                self.strength_model, self.strength_clip
            ));
        }
        match (self.strength_model_min, self.strength_model_max) {
            (Some(a), Some(b)) => parts.push(format!("{a:.2}–{b:.2}")),
            (Some(a), None) => parts.push(format!("min {a:.2}")),
            (None, Some(b)) => parts.push(format!("max {b:.2}")),
            _ => {}
        }
        parts.join(" · ")
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

/// Pull known LoRA trigger tokens out of `positive` into `lora_triggers`.
///
/// `known` is `(lora_index, trigger)` from the catalog for the active stack. Matching is
/// case-insensitive on comma-separated tokens; catalog spelling is kept in `lora_triggers`.
/// Returns per-lora joined triggers that were moved (for [`ActiveLora::injected`]).
pub fn extract_triggers_from_positive(
    positive: &mut String,
    lora_triggers: &mut String,
    known: &[(usize, String)],
) -> Vec<(usize, String)> {
    if known.is_empty() || positive.trim().is_empty() {
        return Vec::new();
    }
    let mut kept = Vec::new();
    let mut moved: Vec<String> = Vec::new();
    let mut by_lora: std::collections::BTreeMap<usize, Vec<String>> =
        std::collections::BTreeMap::new();
    for part in split_triggers(positive) {
        if let Some((idx, canon)) = known
            .iter()
            .find(|(_, t)| t.eq_ignore_ascii_case(&part))
        {
            if !trigger_present(&[&moved.join(", "), lora_triggers.as_str()], canon) {
                moved.push(canon.clone());
                by_lora.entry(*idx).or_default().push(canon.clone());
            }
        } else {
            kept.push(part);
        }
    }
    if moved.is_empty() {
        return Vec::new();
    }
    *positive = kept.join(", ");
    let piece = moved.join(", ");
    merge_triggers(lora_triggers, &piece, "");
    by_lora
        .into_iter()
        .map(|(idx, toks)| (idx, toks.join(", ")))
        .collect()
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

/// Keep the character-defining tags from a scraped prompt, dropping quality / meta boilerplate.
/// Used by "Save as character" to seed a card's identity block from a gallery image.
pub fn character_tags_from_prompt(prompt: &str) -> String {
    split_triggers(prompt)
        .into_iter()
        .filter(|t| !is_quality_tag(t))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A generic quality / rating / meta tag rather than a character-identity tag.
fn is_quality_tag(tag: &str) -> bool {
    let t = tag.trim().to_ascii_lowercase();
    if t.is_empty() {
        return true;
    }
    if t.starts_with("score_") || t.starts_with("rating:") || t.starts_with("source_") {
        return true;
    }
    const DROP: &[&str] = &[
        "masterpiece", "best quality", "high quality", "normal quality", "low quality",
        "worst quality", "amazing quality", "great quality", "good quality", "very aesthetic",
        "aesthetic", "absurdres", "highres", "high resolution", "lowres", "ultra-detailed",
        "ultra detailed", "extremely detailed", "highly detailed", "detailed", "intricate details",
        "8k", "4k", "2k", "uhd", "hdr", "raw photo", "sharp focus", "depth of field", "bokeh",
        "cinematic lighting", "professional lighting", "studio lighting", "dramatic lighting",
        "official art", "game cg", "illustration", "artist name", "signature", "watermark", "text",
        "logo", "username", "web address", "dated", "newest", "oldest", "sfw", "nsfw",
        "photorealistic", "realistic", "render", "unreal engine",
    ];
    DROP.contains(&t.as_str())
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
    Score,
}

impl GallerySort {
    pub fn param(self) -> &'static str {
        match self {
            Self::Newest => "new",
            Self::Oldest => "old",
            Self::Largest => "large",
            Self::Smallest => "small",
            Self::Name => "name",
            // Aesthetic score is client-side data; list newest and reorder locally.
            Self::Score => "new",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Newest => "Newest",
            Self::Oldest => "Oldest",
            Self::Largest => "Largest",
            Self::Smallest => "Smallest",
            Self::Name => "Name",
            Self::Score => "Score",
        }
    }

    pub const ALL: &'static [Self] =
        &[Self::Newest, Self::Oldest, Self::Largest, Self::Smallest, Self::Name, Self::Score];
}

/// What the gallery's collapsing headers bucket by. The server only orders rows to match; the
/// header text is derived client-side.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GalleryGroup {
    None,
    Folder,
    Model,
    Date,
    Character,
}

impl GalleryGroup {
    pub fn param(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Folder => "folder",
            Self::Model => "model",
            Self::Date => "date",
            // No server-side ordering exists for characters; the split is entirely client-side.
            Self::Character => "none",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::None => "No grouping",
            Self::Folder => "Folder",
            Self::Model => "Model",
            Self::Date => "Date",
            Self::Character => "Character",
        }
    }

    pub const ALL: &'static [Self] =
        &[Self::Folder, Self::Model, Self::Date, Self::Character, Self::None];
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

/// Rating filter for the gallery, applied client-side over the local auto-tag index. Unindexed
/// items (rating unknown) count as Safe, so a fresh library isn't emptied by the filter.
#[derive(Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum RatingFilter {
    #[default]
    All,
    Safe,
    Nsfw,
}

impl RatingFilter {
    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All ratings",
            Self::Safe => "Safe only",
            Self::Nsfw => "NSFW only",
        }
    }

    /// Whether an item passes; `nsfw` is `None` when the item is unindexed (counted as Safe).
    pub fn matches(self, nsfw: Option<bool>) -> bool {
        match self {
            Self::All => true,
            Self::Safe => nsfw != Some(true),
            Self::Nsfw => nsfw == Some(true),
        }
    }

    pub const ALL: &'static [Self] = &[Self::All, Self::Safe, Self::Nsfw];
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
    /// Safe / NSFW / all, filtered client-side over the auto-tag index.
    #[serde(default)]
    pub rating: RatingFilter,
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
            rating: RatingFilter::All,
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

/// Persisted to `<documents>/comfyui_settings.json` and mirrored under the app external files dir.
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
    /// Legacy gallery search box text; ignored on load (session-only in the app).
    #[serde(default)]
    pub gallery_q: String,
    /// Main gallery search box runs CLIP semantic search where the local pack supports it.
    #[serde(default = "default_true")]
    pub gallery_semantic: bool,
    /// How many gallery rows to fetch per page / Load more (20..=500).
    #[serde(default = "default_gallery_page")]
    pub gallery_page: u64,
    /// Auto-follow: pan/zoom the graph to whichever node is currently executing.
    #[serde(default)]
    pub auto_follow: bool,
    /// Auto-arrange the canvas when a loaded workflow is first shown.
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
    /// On-device recurring-character cards.
    #[serde(default)]
    pub characters: Vec<CharacterCard>,
    /// The currently applied character's undo bookkeeping (so removal survives a restart).
    #[serde(default)]
    pub active_character: Option<AppliedCharacter>,
    /// Create Checkpoints list sort (name vs most recently used).
    #[serde(default)]
    pub checkpoint_sort: CheckpointSort,
    /// Locally pinned favorite checkpoint filenames (in addition to catalog `favorite`).
    #[serde(default)]
    pub checkpoint_favorites: Vec<String>,
    /// Most-recently-used checkpoint filenames (newest first).
    #[serde(default)]
    pub checkpoint_recent: Vec<String>,
    /// Ask before deleting gallery images (viewer or multi-select).
    #[serde(default = "default_true")]
    pub confirm_gallery_delete: bool,
    /// Create Main: text-encoder/VAE and img2img source block is expanded.
    #[serde(default = "default_true")]
    pub create_setup_open: bool,
    /// Create Main: companions & image source block is expanded (same block, persisted separately).
    #[serde(default = "default_true")]
    pub create_companions_open: bool,
    /// Route Create Queue through on-device HTP (feature `local-npu`); ignores remote ComfyUI.
    #[serde(default)]
    pub local_npu: bool,
    /// Background-tag the whole server gallery when idle (feature `local-npu`).
    #[serde(default)]
    pub auto_tag: bool,
    /// Background-download full gallery images to the on-device cache while idle.
    #[serde(default = "default_true")]
    pub cache_prefetch: bool,
    /// Which on-device pipeline `local_npu` runs; absent in older settings, so SD1.5 by default.
    #[serde(default)]
    pub local_backend: LocalBackend,
    /// Selected pack subdir under the app external files dir (empty = first pack of `local_backend`).
    #[serde(default)]
    pub local_pack: String,
    /// Route Create generation to the server even while the Local NPU stack is on (Server model pick).
    #[serde(default)]
    pub local_use_server: bool,
    /// Container-side path of ComfyUI's output dir, used to build VHS_LoadVideoPath finish paths.
    #[serde(default = "default_server_output_root")]
    pub server_output_root: String,
    /// Recorded Create-tab prompt pairs for the history scrubber (newest last, capped).
    #[serde(default)]
    pub prompt_history: Vec<PromptHist>,
    /// Per-character denied gallery keys, keyed by card name, so a denied match never resurfaces.
    #[serde(default)]
    pub character_denied: std::collections::BTreeMap<String, Vec<String>>,
    /// Per-character pending match suggestions awaiting review, keyed by card name (capped).
    #[serde(default)]
    pub character_suggestions: std::collections::BTreeMap<String, Vec<String>>,
    /// Per-character accepted gallery keys; every approval sharpens the match centroid.
    #[serde(default)]
    pub character_approved: std::collections::BTreeMap<String, Vec<String>>,
}

pub fn default_server_output_root() -> String {
    "/data/output/".into()
}

/// Create generation routes to the local NPU only when the stack is on and a local model is the
/// chosen one; picking "Server model" (`use_server_model`) keeps the NPU features but sends the
/// job to the server.
pub fn routes_local_generation(local_npu: bool, use_server_model: bool) -> bool {
    local_npu && !use_server_model
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

    /// Settings written before the Anima backend existed still load, as SD1.5.
    #[test]
    fn settings_without_local_backend_load_as_sd15() {
        let params = serde_json::to_value(Params::default()).unwrap();
        let json = serde_json::json!({"server_url": "http://x", "params": params, "local_npu": true});
        let s: Settings = serde_json::from_value(json).unwrap();
        assert!(s.local_npu);
        assert_eq!(s.local_backend, LocalBackend::Sd15);
        assert!(s.local_pack.is_empty());
    }

    /// Older settings (no `local_use_server`) with the NPU on still route locally.
    #[test]
    fn settings_without_use_server_still_route_local() {
        let params = serde_json::to_value(Params::default()).unwrap();
        let json = serde_json::json!({"server_url": "http://x", "params": params, "local_npu": true});
        let s: Settings = serde_json::from_value(json).unwrap();
        assert!(!s.local_use_server);
        assert!(routes_local_generation(s.local_npu, s.local_use_server));
    }

    #[test]
    fn create_routes_local_only_when_npu_on_and_a_local_model_chosen() {
        assert!(routes_local_generation(true, false));
        // Server model picked: NPU on but generation goes to the server.
        assert!(!routes_local_generation(true, true));
        // NPU off: always server.
        assert!(!routes_local_generation(false, false));
        assert!(!routes_local_generation(false, true));
    }

    #[test]
    fn settings_round_trip_the_server_model_pick() {
        let params = serde_json::to_value(Params::default()).unwrap();
        let json = serde_json::json!({
            "server_url": "", "params": params, "local_npu": true, "local_use_server": true,
        });
        let s: Settings = serde_json::from_value(json).unwrap();
        assert!(s.local_use_server);
        let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert!(back.local_use_server);
    }

    /// Settings written before the finish-pass output root existed default it to `/data/output/`.
    #[test]
    fn settings_without_output_root_default_to_data_output() {
        let params = serde_json::to_value(Params::default()).unwrap();
        let json = serde_json::json!({"server_url": "http://x", "params": params});
        let s: Settings = serde_json::from_value(json).unwrap();
        assert_eq!(s.server_output_root, "/data/output/");
        let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back.server_output_root, "/data/output/");
    }

    #[test]
    fn settings_round_trip_the_anima_backend() {
        let params = serde_json::to_value(Params::default()).unwrap();
        let json = serde_json::json!({
            "server_url": "", "params": params, "local_npu": true,
            "local_backend": "Anima", "local_pack": "anima_nova",
        });
        let s: Settings = serde_json::from_value(json).unwrap();
        assert_eq!(s.local_backend, LocalBackend::Anima);
        assert_eq!(s.local_pack, "anima_nova");
        let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back.local_backend, LocalBackend::Anima);
        assert_eq!(back.local_pack, "anima_nova");
    }

    fn ph(p: &str, n: &str) -> PromptHist {
        PromptHist { positive: p.into(), negative: n.into() }
    }

    #[test]
    fn push_prompt_hist_skips_repeat_of_newest() {
        let mut h = vec![ph("a", "x")];
        push_prompt_hist(&mut h, ph("a", "x"));
        assert_eq!(h, vec![ph("a", "x")]);
        // A differing negative is not a repeat.
        push_prompt_hist(&mut h, ph("a", "y"));
        assert_eq!(h.len(), 2);
        // Repeating an older-but-not-newest entry still appends.
        push_prompt_hist(&mut h, ph("a", "x"));
        assert_eq!(h, vec![ph("a", "x"), ph("a", "y"), ph("a", "x")]);
    }

    #[test]
    fn push_prompt_hist_keeps_newest_last() {
        let mut h = Vec::new();
        for i in 0..3 {
            push_prompt_hist(&mut h, ph(&i.to_string(), ""));
        }
        assert_eq!(h.last(), Some(&ph("2", "")));
        assert_eq!(h.first(), Some(&ph("0", "")));
    }

    #[test]
    fn push_prompt_hist_evicts_oldest_past_cap() {
        let mut h = Vec::new();
        for i in 0..(PROMPT_HISTORY_CAP + 10) {
            push_prompt_hist(&mut h, ph(&i.to_string(), ""));
        }
        assert_eq!(h.len(), PROMPT_HISTORY_CAP);
        assert_eq!(h.first(), Some(&ph("10", "")));
        assert_eq!(h.last(), Some(&ph(&(PROMPT_HISTORY_CAP + 9).to_string(), "")));
    }

    #[test]
    fn settings_without_prompt_history_default_empty() {
        let params = serde_json::to_value(Params::default()).unwrap();
        let json = serde_json::json!({"server_url": "http://x", "params": params});
        let s: Settings = serde_json::from_value(json).unwrap();
        assert!(s.prompt_history.is_empty());
    }

    #[test]
    fn dedupe_loras_keeps_first_of_each_file() {
        let pack = vec![
            ActiveLora {
                file: "a.safetensors".into(),
                strength_model: 0.5,
                strength_clip: 0.5,
                injected: String::new(),
                model_only: false,
            },
            ActiveLora {
                file: "b.safetensors".into(),
                strength_model: 1.0,
                strength_clip: 1.0,
                injected: String::new(),
                model_only: true,
            },
            ActiveLora {
                file: "a.safetensors".into(),
                strength_model: 0.9,
                strength_clip: 0.9,
                injected: String::new(),
                model_only: false,
            },
        ];
        let out = dedupe_loras(pack);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].file, "a.safetensors");
        assert!((out[0].strength_model - 0.5).abs() < 1e-6);
        assert_eq!(out[1].file, "b.safetensors");
    }

    /// Presets and settings written before the diffusion-model fields existed must still load —
    /// a failed `Settings` parse silently discards the server URL, key and every saved preset.
    #[test]
    fn params_without_the_diffusion_fields_still_load() {
        let old = r#"{
            "checkpoint": "sdxl.safetensors",
            "positive": "a cat",
            "negative": "blurry",
            "steps": 20, "cfg": 7.0, "width": 1024, "height": 1024, "batch_size": 1,
            "sampler": "euler", "scheduler": "normal", "seed": 0, "randomize_seed": true,
            "denoise": 0.6, "mode": "Txt2Img", "img2img_source": "CurrentOutput",
            "input_url": "",
            "loras": [{"file": "x.safetensors", "strength_model": 1.0, "strength_clip": 1.0}]
        }"#;
        let p: Params = serde_json::from_str(old).expect("old params must still deserialize");
        assert_eq!(p.model_kind, ModelKind::Checkpoint);
        assert_eq!(p.model_file(), "sdxl.safetensors");
        assert!(p.clip_names.is_empty() && p.vae_name.is_empty());
        assert!(!p.loras[0].model_only);
        // Old JSON without the flag defaults inpaint off.
        assert!(!p.inpaint_mask);
        // Unchanged behavior for existing presets: nothing blocks the queue.
        assert_eq!(p.missing_model_part(), None);
    }

    #[test]
    fn params_without_video_field_default_to_the_proven_wan_settings() {
        let old = r#"{
            "checkpoint": "sdxl.safetensors", "positive": "a cat", "negative": "blurry",
            "steps": 20, "cfg": 7.0, "width": 1024, "height": 1024, "batch_size": 1,
            "sampler": "euler", "scheduler": "normal", "seed": 0, "randomize_seed": true,
            "denoise": 0.6, "mode": "Txt2Img", "img2img_source": "CurrentOutput", "input_url": ""
        }"#;
        let p: Params = serde_json::from_str(old).expect("old params must still deserialize");
        assert_eq!(p.video.length, 81);
        assert_eq!(p.video.steps, 8);
        assert_eq!(p.video.split_step, 4);
        assert_eq!(p.video.loras_high.len(), 2);
        assert_eq!(p.video.loras_low.len(), 2);
        assert!(p.video.loras_high.iter().all(|l| l.model_only));
        assert!((p.video.cfg_high - 2.5).abs() < 1e-6);
        assert!(p.video.rife && p.video.gpu_clean && !p.video.video_t2v);
        // Round-trips.
        let json = serde_json::to_string(&p).unwrap();
        let back: Params = serde_json::from_str(&json).unwrap();
        assert_eq!(back.video.clip_type, "wan");
        assert_eq!(back.video.rife_multiplier, 2);
    }

    #[test]
    fn params_round_trip_with_video_mode() {
        let p = Params { mode: Mode::Video, ..Default::default() };
        let json = serde_json::to_string(&p).unwrap();
        let back: Params = serde_json::from_str(&json).unwrap();
        assert_eq!(back.mode, Mode::Video);
    }

    #[test]
    fn params_round_trip_with_picked_img2img_source() {
        let p = Params {
            img2img_source: Img2ImgSource::Picked,
            mode: Mode::Img2Img,
            inpaint_mask: true,
            ..Default::default()
        };
        let json = serde_json::to_string(&p).expect("serialize");
        let back: Params = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.img2img_source, Img2ImgSource::Picked);
        assert_eq!(back.mode, Mode::Img2Img);
        assert!(back.inpaint_mask);
    }

    #[test]
    fn a_diffusion_model_needs_its_encoder_and_vae_before_queueing() {
        let mut p = Params {
            model_kind: ModelKind::Diffusion,
            unet_name: "Anima/novaAnimeAM_v30.safetensors".into(),
            ..Default::default()
        };
        assert_eq!(p.missing_model_part(), Some("Pick a text encoder for this model"));
        p.clip_names = vec!["qwen_3_06b_base.safetensors".into()];
        assert_eq!(p.missing_model_part(), Some("Pick a VAE for this model"));
        p.vae_name = "qwen_image_vae.safetensors".into();
        assert_eq!(p.missing_model_part(), None);
        // Blank entries never count as a chosen encoder.
        p.clip_names = vec!["  ".into()];
        assert_eq!(p.missing_model_part(), Some("Pick a text encoder for this model"));
    }

    #[test]
    fn catalog_directory_picks_the_loader() {
        let entry = |dir: &str| CheckpointEntry {
            file: "m.safetensors".into(),
            directory: dir.into(),
            name: String::new(),
            bases: Vec::new(),
            tags: Vec::new(),
            notes: String::new(),
            favorite: false,
            from_civitai: false,
            base_model: None,
            base_model_type: None,
            sha256: None,
            size: None,
            creator: None,
            version: None,
            description: None,
            preview: None,
            nsfw_level: None,
            civitai_id: None,
            civitai_model_id: None,
            download_count: None,
            thumbs_up: None,
            recommended: None,
        };
        assert_eq!(entry("diffusion_models").model_kind(), Some(ModelKind::Diffusion));
        assert_eq!(entry("unet").model_kind(), Some(ModelKind::Diffusion));
        assert_eq!(entry("checkpoints").model_kind(), Some(ModelKind::Checkpoint));
        // Unknown / absent directory defers to the caller's list-membership fallback.
        assert_eq!(entry("").model_kind(), None);
    }

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

    fn card_lora(file: &str, s: f32) -> ActiveLora {
        ActiveLora {
            file: file.into(),
            strength_model: s,
            strength_clip: s,
            injected: String::new(),
            model_only: false,
        }
    }

    #[test]
    fn character_pack_round_trips_through_the_clipboard() {
        let card = CharacterCard {
            name: "Mia".into(),
            identity: "1girl, silver hair, red eyes, twin braids".into(),
            triggers: "miachar".into(),
            negatives: "bad anatomy".into(),
            loras: vec![card_lora("mia_v2.safetensors", 0.8)],
            checkpoint: "novaAnime.safetensors".into(),
            switch_checkpoint: true,
            face_prompt: "close-up of Mia's face".into(),
            portrait_key: "user_x/Mia/portrait.png".into(),
            album_id: 7,
        };
        let json = CharacterPack { card: card.clone() }.to_clipboard_json();
        let back = CharacterPack::from_clipboard_json(&json).expect("valid pack");
        assert_eq!(back.card, card);
        // Foreign / malformed payloads are rejected.
        assert!(CharacterPack::from_clipboard_json(&LoraPack::default().to_clipboard_json()).is_none());
        assert!(CharacterPack::from_clipboard_json("not json").is_none());
        // A nameless card is not a shareable pack.
        let nameless = CharacterPack { card: CharacterCard::default() };
        assert!(CharacterPack::from_clipboard_json(&nameless.to_clipboard_json()).is_none());
    }

    /// Cards written before the profile / album fields existed still load, with empty defaults.
    #[test]
    fn character_card_without_portrait_or_album_loads_with_defaults() {
        let old = r#"{"name": "Mia", "identity": "1girl, silver hair"}"#;
        let card: CharacterCard = serde_json::from_str(old).expect("old card must deserialize");
        assert_eq!(card.name, "Mia");
        assert!(card.portrait_key.is_empty());
        assert_eq!(card.album_id, 0);
        // The new fields round-trip once set.
        let card = CharacterCard { portrait_key: "u/p.png".into(), album_id: 3, ..card };
        let back: CharacterCard =
            serde_json::from_str(&serde_json::to_string(&card).unwrap()).unwrap();
        assert_eq!(back.portrait_key, "u/p.png");
        assert_eq!(back.album_id, 3);
    }

    /// Settings written before the character denied / suggestions maps existed still load empty.
    #[test]
    fn settings_without_character_maps_load_empty() {
        let params = serde_json::to_value(Params::default()).unwrap();
        let json = serde_json::json!({"server_url": "http://x", "params": params});
        let s: Settings = serde_json::from_value(json).unwrap();
        assert!(s.character_denied.is_empty());
        assert!(s.character_suggestions.is_empty());
        let mut s = s;
        s.character_denied.insert("Mia".into(), vec!["u/a.png".into()]);
        s.character_suggestions.insert("Mia".into(), vec!["u/b.png".into()]);
        let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back.character_denied["Mia"], vec!["u/a.png".to_string()]);
        assert_eq!(back.character_suggestions["Mia"], vec!["u/b.png".to_string()]);
    }

    #[test]
    fn applying_then_removing_a_character_restores_params_exactly() {
        let mut p = Params {
            positive: "a girl, outdoors".into(),
            negative: "text, watermark, low quality".into(),
            lora_triggers: "masterpiece".into(),
            loras: vec![card_lora("base.safetensors", 1.0)],
            ..Default::default()
        };
        let before = serde_json::to_string(&p).unwrap();
        let card = CharacterCard {
            name: "Mia".into(),
            identity: "silver hair, red eyes, twin braids".into(),
            triggers: "miachar".into(),
            negatives: "bad anatomy".into(),
            loras: vec![card_lora("mia.safetensors", 0.8)],
            ..Default::default()
        };
        let applied = p.apply_character(&card, |f| {
            if f == "mia.safetensors" {
                ("mia trigger".into(), "extra fingers".into())
            } else {
                (String::new(), String::new())
            }
        });
        assert!(p.positive.contains("silver hair") && p.positive.contains("twin braids"));
        assert!(p.lora_triggers.contains("miachar") && p.lora_triggers.contains("mia trigger"));
        assert!(p.negative.contains("bad anatomy") && p.negative.contains("extra fingers"));
        assert_eq!(p.loras.len(), 2);
        p.remove_character(&applied);
        assert_eq!(serde_json::to_string(&p).unwrap(), before, "remove must restore Params exactly");
    }

    #[test]
    fn reset_creative_clears_creative_state_but_keeps_the_model() {
        let mut p = Params {
            checkpoint: "novaAnime.safetensors".into(),
            positive: "a girl, outdoors".into(),
            lora_triggers: "masterpiece".into(),
            loras: vec![card_lora("base.safetensors", 1.0)],
            apps: vec![AppStep {
                app: "face.detailer".into(),
                enabled: true,
                version: 0,
                values: Default::default(),
            }],
            mode: Mode::Img2Img,
            randomize_seed: false,
            steps: 40,
            ..Default::default()
        };
        p.video.length = 33;
        p.reset_creative();
        assert!(p.positive.is_empty());
        assert!(p.lora_triggers.is_empty());
        assert!(p.loras.is_empty());
        assert!(p.apps.is_empty());
        assert_eq!(p.mode, Mode::Txt2Img);
        assert!(p.randomize_seed);
        assert_eq!(p.steps, Params::default().steps);
        assert_eq!(p.video.length, VideoParams::default().length);
        // The selected model survives the reset.
        assert_eq!(p.checkpoint, "novaAnime.safetensors");
    }

    #[test]
    fn resetting_after_applying_a_character_matches_a_fresh_default() {
        let mut p = Params { checkpoint: "novaAnime.safetensors".into(), ..Default::default() };
        let card = CharacterCard {
            name: "Mia".into(),
            identity: "silver hair, red eyes".into(),
            triggers: "miachar".into(),
            negatives: "bad anatomy".into(),
            loras: vec![card_lora("mia.safetensors", 0.8)],
            ..Default::default()
        };
        let _ = p.apply_character(&card, |_| ("mia trigger".into(), "extra fingers".into()));
        p.reset_creative();
        let fresh = Params { checkpoint: "novaAnime.safetensors".into(), ..Default::default() };
        assert_eq!(
            serde_json::to_string(&p).unwrap(),
            serde_json::to_string(&fresh).unwrap(),
            "reset must leave no trace of the applied character"
        );
    }

    #[test]
    fn applying_a_character_does_not_duplicate_present_tokens_or_loras() {
        let mut p = Params {
            positive: "silver hair, a girl".into(),
            lora_triggers: String::new(),
            loras: vec![card_lora("mia.safetensors", 1.0)],
            ..Default::default()
        };
        let before = serde_json::to_string(&p).unwrap();
        let card = CharacterCard {
            name: "Mia".into(),
            identity: "silver hair, red eyes".into(),
            loras: vec![card_lora("mia.safetensors", 0.5)],
            ..Default::default()
        };
        let applied = p.apply_character(&card, |_| (String::new(), String::new()));
        // "silver hair" was already there; only "red eyes" is added, and the pre-existing LoRA is
        // left untouched (its strength is not overwritten, and it is not in the undo set).
        assert_eq!(p.positive, "silver hair, a girl, red eyes");
        assert_eq!(p.loras.len(), 1);
        assert!((p.loras[0].strength_model - 1.0).abs() < 1e-6);
        assert!(applied.loras.is_empty());
        p.remove_character(&applied);
        assert_eq!(serde_json::to_string(&p).unwrap(), before);
    }

    #[test]
    fn character_tags_drop_quality_boilerplate() {
        let prompt =
            "masterpiece, best quality, 1girl, silver hair, red eyes, absurdres, score_9, watermark";
        assert_eq!(character_tags_from_prompt(prompt), "1girl, silver hair, red eyes");
    }

    #[test]
    fn extract_triggers_moves_catalog_tags_out_of_positive() {
        let mut positive = "styletag, a cat sitting, OtherTag, indoors".to_string();
        let mut triggers = String::new();
        let known = vec![
            (0usize, "StyleTag".into()),
            (1usize, "OtherTag".into()),
            (1usize, "missing".into()),
        ];
        let moved = extract_triggers_from_positive(&mut positive, &mut triggers, &known);
        assert_eq!(positive, "a cat sitting, indoors");
        assert_eq!(triggers, "StyleTag, OtherTag");
        assert_eq!(moved, vec![(0, "StyleTag".into()), (1, "OtherTag".into())]);
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

    #[test]
    fn pretty_model_family_normalizes_common_tags() {
        assert_eq!(pretty_model_family("sdxl"), "SDXL");
        assert_eq!(pretty_model_family("SD 1.5"), "SD 1.5");
        assert_eq!(pretty_model_family("flux-dev"), "Flux");
        assert_eq!(pretty_model_family("Pony"), "Pony");
        assert_eq!(pretty_model_family("Illustrious"), "Illustrious");
        assert_eq!(pretty_model_family("Anima"), "Anima");
        assert_eq!(pretty_model_family(""), "Other");
    }

    #[test]
    fn checkpoint_family_prefers_base_model_over_bases() {
        let e = CheckpointEntry {
            file: "a.safetensors".into(),
            directory: "checkpoints".into(),
            name: "A".into(),
            bases: vec!["sdxl".into()],
            tags: vec![],
            notes: String::new(),
            favorite: false,
            from_civitai: false,
            base_model: Some("Pony".into()),
            base_model_type: None,
            sha256: None,
            size: None,
            creator: None,
            version: None,
            description: None,
            preview: None,
            nsfw_level: None,
            civitai_id: None,
            civitai_model_id: None,
            download_count: None,
            thumbs_up: None,
            recommended: None,
        };
        assert_eq!(e.family_label(), "Pony");
        assert_eq!(checkpoint_family(None), "Other");
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
