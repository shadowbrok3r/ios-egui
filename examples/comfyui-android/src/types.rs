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
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize)]
pub enum ModelKind {
    /// `CheckpointLoaderSimple` -> MODEL + CLIP + VAE.
    #[default]
    Checkpoint,
    /// `UNETLoader` + `CLIPLoader`/`DualCLIPLoader` + `VAELoader`.
    Diffusion,
    /// `CheckpointLoaderSimple` for the MODEL only, with `CLIPLoader` + `VAELoader` alongside.
    ///
    /// A bare diffusion model (Anima, Flux, Qwen-Image) that sits in `models/checkpoints` rather
    /// than `models/diffusion_models`. ComfyUI lists it under `CheckpointLoaderSimple.ckpt_name`
    /// and nowhere else, so [`Self::Diffusion`]'s `UNETLoader` cannot reach the file — but the
    /// checkpoint loader reads its weights fine and simply returns no CLIP and no VAE. Wiring the
    /// encoder and VAE separately is then the only topology that runs.
    CheckpointDiffusion,
}

/// Hand-written so an unrecognised variant degrades to the all-in-one checkpoint instead of
/// failing the whole settings file — a derived unit enum rejects an unknown string outright, which
/// would take the server URL, presets and characters down with it.
impl<'de> Deserialize<'de> for ModelKind {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(match String::deserialize(d)?.as_str() {
            "Diffusion" => Self::Diffusion,
            "CheckpointDiffusion" => Self::CheckpointDiffusion,
            _ => Self::Checkpoint,
        })
    }
}

impl ModelKind {
    /// Picker order: plain checkpoint, the split-companion checkpoint, then the bare UNET.
    pub const ALL: [ModelKind; 3] =
        [ModelKind::Checkpoint, ModelKind::CheckpointDiffusion, ModelKind::Diffusion];

    pub fn label(self) -> &'static str {
        match self {
            Self::Checkpoint => "Checkpoint",
            Self::CheckpointDiffusion => "Checkpoint + separate CLIP/VAE",
            Self::Diffusion => "Diffusion model",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Self::Checkpoint => "One file carrying MODEL + CLIP + VAE.",
            Self::CheckpointDiffusion => {
                "A bare diffusion model filed under models/checkpoints: the file loads through \
                 CheckpointLoaderSimple, and the text encoder and VAE load separately."
            }
            Self::Diffusion => {
                "A bare diffusion model under models/diffusion_models or models/unet, loaded by \
                 UNETLoader with a separate text encoder and VAE."
            }
        }
    }

    /// Reads the file from the `CheckpointLoaderSimple` list rather than the `UNETLoader` one.
    pub fn is_checkpoint_file(self) -> bool {
        matches!(self, Self::Checkpoint | Self::CheckpointDiffusion)
    }

    /// Needs a separately-loaded text encoder and VAE.
    pub fn needs_companions(self) -> bool {
        matches!(self, Self::Diffusion | Self::CheckpointDiffusion)
    }
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

impl ActiveLora {
    /// Model and CLIP currently hold the same number, to slider precision.
    ///
    /// This is what makes the strength lock default to *on* without storing a flag per slot: two
    /// equal strengths are a linked pair, and the only way they can differ is a workflow the user
    /// pasted/remixed in or an edit made with the lock deliberately off. Either way, differing
    /// numbers are the user's, and silently collapsing them onto one value would change the render.
    pub fn strengths_linked(&self) -> bool {
        (self.strength_model - self.strength_clip).abs() < 0.005
    }
}

/// Canonical Wan negative prompt (anti-3D prefix + the standard Chinese quality block).
pub const WAN_NEGATIVE: &str = "(((realistic))), ((photograph)), 色调艳丽，过曝，静态，细节模糊不清，字幕，风格，作品，画作，画面，静止，整体发灰，最差质量，低质量，JPEG压缩残留，丑陋的，残缺的，多余的手指，画得不好的手部，画得不好的脸部，畸形的，毁容的，形态畸形的肢体，手指融合，静止不动的画面，杂乱的背景，三条腿，背景人很多，倒着走";

/// Wan 2.2 image-to-video settings, seeded with the user's proven-working defaults.
///
/// Container-level `serde(default)`: a settings file written before any one of these fields existed
/// must still load. A failed `Settings` parse blocks autosave outright, taking every saved preset,
/// character and credential with it.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
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
    /// Trigger words for the Wan LoRAs, prepended to the video positive. Separate from the image
    /// `lora_triggers` so a Wan trigger never leaks into an image-mode prompt.
    #[serde(default)]
    pub lora_triggers: String,
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
            lora_triggers: String::new(),
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
    /// `CLIPSetLastLayer` depth: 0 = off, n>=1 emits `stop_at_clip_layer = -n` (checkpoints only —
    /// Pony/Illustrious conventionally run 2; diffusion-model encoders are not CLIP towers).
    #[serde(default)]
    pub clip_skip: u32,
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
    /// Submit the positive verbatim: prepends the gate's `raw:` marker so its queue-time prompt
    /// expander leaves the text alone (set after accepting a streamed `/api/expand` rewrite).
    #[serde(default)]
    pub raw_prompt: bool,
}

/// Create Main mode picker: image vs Wan video, with t2v/i2v split for video.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GenMode {
    Txt2Img,
    Img2Img,
    Txt2Video,
    Img2Video,
}

impl GenMode {
    pub const ALL: [GenMode; 4] =
        [GenMode::Txt2Img, GenMode::Img2Img, GenMode::Txt2Video, GenMode::Img2Video];

    pub fn label(self) -> &'static str {
        match self {
            GenMode::Txt2Img => "Text to Image",
            GenMode::Img2Img => "Image to Image",
            GenMode::Txt2Video => "Text to Video",
            GenMode::Img2Video => "Image to Video",
        }
    }

    pub fn is_video(self) -> bool {
        matches!(self, GenMode::Txt2Video | GenMode::Img2Video)
    }

    pub fn is_image(self) -> bool {
        matches!(self, GenMode::Txt2Img | GenMode::Img2Img)
    }
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
            clip_skip: 0,
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
            raw_prompt: false,
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
        self.clip_skip = d.clip_skip;
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
        self.raw_prompt = d.raw_prompt;
    }

    /// The LoRA-trigger field feeding the current mode: the Wan stacks' own field in Video,
    /// the image field otherwise.
    pub fn active_lora_triggers(&self) -> &str {
        if self.mode == Mode::Video { &self.video.lora_triggers } else { &self.lora_triggers }
    }

    /// Positive CLIP text: LoRA triggers (if any) then the subject prompt.
    pub fn combined_positive(&self) -> String {
        let triggers = self.active_lora_triggers().trim().trim_end_matches(',').trim();
        let subject = self.positive.trim();
        match (triggers.is_empty(), subject.is_empty()) {
            (true, _) => subject.to_string(),
            (_, true) => triggers.to_string(),
            _ => format!("{triggers}, {subject}"),
        }
    }

    /// Positive CLIP text for a comfy-gate submission: [`Self::combined_positive`], prefixed with
    /// the `raw:` marker when the user opted out of the gate's queue-time expander. Only video
    /// prompts are ever expanded server-side, so the marker is added for video alone; the gate
    /// strips it and queues the text verbatim.
    pub fn server_positive(&self) -> String {
        let base = self.combined_positive();
        if self.raw_prompt && self.mode == Mode::Video && !base.is_empty() {
            format!("raw:{base}")
        } else {
            base
        }
    }

    /// The selected model's filename, whichever loader it goes through.
    pub fn model_file(&self) -> &str {
        if self.model_kind.is_checkpoint_file() { &self.checkpoint } else { &self.unet_name }
    }

    /// Create Main mode picker value derived from [`Self::mode`] + [`VideoParams::video_t2v`].
    pub fn gen_mode(&self) -> GenMode {
        match self.mode {
            Mode::Txt2Img => GenMode::Txt2Img,
            Mode::Img2Img => GenMode::Img2Img,
            Mode::Video if self.video.video_t2v => GenMode::Txt2Video,
            Mode::Video => GenMode::Img2Video,
        }
    }

    /// Write Create Main mode bits (checkpoint / Wan UNET picks are the caller's job).
    pub fn set_gen_mode(&mut self, mode: GenMode) {
        match mode {
            GenMode::Txt2Img => self.mode = Mode::Txt2Img,
            GenMode::Img2Img => self.mode = Mode::Img2Img,
            GenMode::Txt2Video => {
                self.mode = Mode::Video;
                self.video.video_t2v = true;
            }
            GenMode::Img2Video => {
                self.mode = Mode::Video;
                self.video.video_t2v = false;
            }
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
        if self.model_file().trim().is_empty() {
            return Some(match self.model_kind {
                ModelKind::Diffusion => "Pick a diffusion model first",
                _ => "Pick a checkpoint first",
            });
        }
        if self.model_kind.needs_companions() {
            if self.active_clips().is_empty() {
                return Some("Pick a text encoder for this model");
            }
            if self.vae_name.trim().is_empty() {
                return Some("Pick a VAE for this model");
            }
        }
        None
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

/// What a [`CharacterLook`] contributes. `Look` is the original combined outfit/pose/scene overlay
/// applied through the character system; the others are single-axis presets picked from the Create
/// Main comboboxes (global or grouped by character).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LookKind {
    #[default]
    Look,
    Outfit,
    Pose,
    CameraAngle,
    Environment,
}

impl LookKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Look => "Look",
            Self::Outfit => "Outfit",
            Self::Pose => "Pose",
            Self::CameraAngle => "Camera angle",
            Self::Environment => "Environment",
        }
    }

    pub fn plural(self) -> &'static str {
        match self {
            Self::Look => "Looks",
            Self::Outfit => "Outfits",
            Self::Pose => "Poses",
            Self::CameraAngle => "Camera angles",
            Self::Environment => "Environments",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Self::Look => "school uniform, hand on hip",
            Self::Outfit => "school uniform, thigh-highs",
            Self::Pose => "hand on hip, looking back",
            Self::CameraAngle => "low angle, from below, wide shot",
            Self::Environment => "rainy neon street at night",
        }
    }

    /// The single-axis kinds surfaced as Create-Main comboboxes (not the combined `Look`).
    pub const MAIN: &'static [Self] = &[Self::Outfit, Self::Pose, Self::CameraAngle, Self::Environment];
}

/// A swappable "look" for a character: a named prompt fragment (outfit, accessories, pose, scene)
/// with an optional photo. The character's `identity` is the fixed person; a look layers on top at
/// apply time and can be swapped without touching the identity.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CharacterLook {
    pub name: String,
    /// Situational tags appended after the identity (clothing, accessories, pose, scene).
    #[serde(default)]
    pub prompt: String,
    /// Gallery item key (`subfolder/filename`) of this look's photo; empty = none.
    #[serde(default)]
    pub portrait_key: String,
    /// Which axis this look feeds. Defaults to the combined `Look` for cards from before categories.
    #[serde(default)]
    pub kind: LookKind,
}

/// The current Create-Main combobox selection for one single-axis [`LookKind`], with the undo record
/// so swapping or clearing reverses its appended tokens exactly.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AppliedMainLook {
    pub kind: LookKind,
    /// The selected look's name (what the combobox shows).
    pub name: String,
    /// Originating character's name, or empty for a global look — the combobox grouping key.
    #[serde(default)]
    pub origin: String,
    /// Tokens appended to `positive`, stripped on removal.
    #[serde(default)]
    pub injected: String,
}

/// Baked-in single-axis look presets (danbooru-style tags), always available in the Create-Main
/// comboboxes on top of any the user creates. Names must stay unique within a kind.
pub fn builtin_looks() -> Vec<CharacterLook> {
    const DATA: &[(LookKind, &str, &str)] = &[
        // ---- Outfits ----
        (LookKind::Outfit, "School uniform", "school uniform, pleated skirt, necktie"),
        (LookKind::Outfit, "Sailor uniform", "serafuku, sailor collar, pleated skirt"),
        (LookKind::Outfit, "Gym clothes", "gym uniform, buruma, white shirt"),
        (LookKind::Outfit, "Business suit", "business suit, blazer, pencil skirt, necktie"),
        (LookKind::Outfit, "Office lady", "office lady, white blouse, pencil skirt, pantyhose"),
        (LookKind::Outfit, "Casual", "casual, t-shirt, jeans"),
        (LookKind::Outfit, "Hoodie & jeans", "hoodie, denim, jeans, sneakers"),
        (LookKind::Outfit, "Overalls", "overalls, denim, t-shirt"),
        (LookKind::Outfit, "Summer dress", "sundress, sun hat, sandals"),
        (LookKind::Outfit, "Evening gown", "evening gown, elegant dress, jewelry"),
        (LookKind::Outfit, "Cocktail dress", "cocktail dress, bare shoulders, high heels"),
        (LookKind::Outfit, "Wedding dress", "wedding dress, veil, bridal gown"),
        (LookKind::Outfit, "Kimono", "kimono, obi, floral print"),
        (LookKind::Outfit, "Yukata", "yukata, obi, summer festival"),
        (LookKind::Outfit, "Hanbok", "hanbok, korean clothes"),
        (LookKind::Outfit, "China dress", "china dress, side slit, mandarin collar"),
        (LookKind::Outfit, "Maid outfit", "maid, maid apron, frilled dress, maid headdress"),
        (LookKind::Outfit, "Nurse", "nurse, nurse cap, white dress"),
        (LookKind::Outfit, "Miko", "miko, red hakama, white kimono, ribbon-trimmed sleeves"),
        (LookKind::Outfit, "Gothic lolita", "gothic lolita, frilled dress, ribbon, bonnet"),
        (LookKind::Outfit, "Goth", "goth fashion, black dress, choker, dark makeup"),
        (LookKind::Outfit, "Punk", "punk, studded belt, fishnets, ripped clothes"),
        (LookKind::Outfit, "Sportswear", "sportswear, track jacket, athletic shorts"),
        (LookKind::Outfit, "Tracksuit", "tracksuit, hoodie, athletic wear"),
        (LookKind::Outfit, "Cozy sweater", "sweater, turtleneck sweater, skirt, thighhighs"),
        (LookKind::Outfit, "Winter coat", "winter coat, scarf, gloves, earmuffs"),
        (LookKind::Outfit, "Leather jacket", "leather jacket, ripped jeans, boots"),
        (LookKind::Outfit, "Swimsuit", "one-piece swimsuit"),
        (LookKind::Outfit, "Bikini", "bikini, swimsuit"),
        (LookKind::Outfit, "Lingerie", "lingerie, lace, garter belt"),
        (LookKind::Outfit, "Pajamas", "pajamas, sleepwear"),
        (LookKind::Outfit, "Military uniform", "military uniform, epaulettes, peaked cap"),
        (LookKind::Outfit, "Police uniform", "police, police uniform, cap"),
        (LookKind::Outfit, "Witch", "witch, witch hat, robe"),
        (LookKind::Outfit, "Fantasy armor", "armor, breastplate, pauldrons, fantasy"),
        (LookKind::Outfit, "Knight", "plate armor, knight, gauntlets"),
        (LookKind::Outfit, "Cyberpunk", "cyberpunk, techwear, neon trim, bodysuit"),
        (LookKind::Outfit, "Bodysuit", "bodysuit, skin tight, zipper"),
        (LookKind::Outfit, "Steampunk", "steampunk, corset, goggles, gears"),
        (LookKind::Outfit, "Idol costume", "idol, frilled stage costume, gloves"),
        (LookKind::Outfit, "Bunny suit", "playboy bunny, rabbit ears, detached collar, pantyhose"),
        (LookKind::Outfit, "Cheerleader", "cheerleader, crop top, pleated skirt, pom poms"),
        (LookKind::Outfit, "Cowgirl", "cowboy hat, western, denim, boots"),
        // ---- Poses ----
        (LookKind::Pose, "Standing", "standing"),
        (LookKind::Pose, "Contrapposto", "standing, contrapposto"),
        (LookKind::Pose, "Hand on hip", "hand on hip, standing"),
        (LookKind::Pose, "Arms crossed", "crossed arms, standing"),
        (LookKind::Pose, "Hands behind back", "arms behind back, standing"),
        (LookKind::Pose, "Hands behind head", "arms behind head"),
        (LookKind::Pose, "Hands in pockets", "hands in pockets, casual"),
        (LookKind::Pose, "Sitting", "sitting"),
        (LookKind::Pose, "Sitting on floor", "sitting on floor, hugging own legs"),
        (LookKind::Pose, "Cross-legged", "sitting, crossed legs"),
        (LookKind::Pose, "Seiza", "seiza, kneeling"),
        (LookKind::Pose, "Kneeling", "kneeling"),
        (LookKind::Pose, "Crouching", "squatting, crouching"),
        (LookKind::Pose, "Lying on back", "lying, on back"),
        (LookKind::Pose, "Lying on stomach", "lying, on stomach, feet up"),
        (LookKind::Pose, "Lying on side", "lying, on side, head rest"),
        (LookKind::Pose, "Walking", "walking"),
        (LookKind::Pose, "Running", "running, dynamic pose"),
        (LookKind::Pose, "Jumping", "jumping, mid-air"),
        (LookKind::Pose, "Dancing", "dancing, dynamic pose"),
        (LookKind::Pose, "Twirling", "twirling, dress flip, dynamic pose"),
        (LookKind::Pose, "Stretching", "stretching, arms up"),
        (LookKind::Pose, "Leaning forward", "leaning forward, hands on own knees"),
        (LookKind::Pose, "Leaning back", "leaning back, relaxed"),
        (LookKind::Pose, "Looking back", "looking back, from behind"),
        (LookKind::Pose, "Over the shoulder", "looking over shoulder"),
        (LookKind::Pose, "Hand on cheek", "hand on own cheek, head tilt"),
        (LookKind::Pose, "Hugging self", "hugging own body"),
        (LookKind::Pose, "Waving", "waving, one arm up"),
        (LookKind::Pose, "Peace sign", "peace sign, v, smile"),
        (LookKind::Pose, "Pointing", "pointing at viewer"),
        (LookKind::Pose, "Reaching out", "reaching towards viewer, outstretched arm"),
        (LookKind::Pose, "Blowing a kiss", "blowing kiss, hand to own mouth"),
        (LookKind::Pose, "Arms up", "arms up, cheering"),
        (LookKind::Pose, "Holding skirt", "skirt hold, curtsy"),
        (LookKind::Pose, "Salute", "salute"),
        // ---- Camera angles ----
        (LookKind::CameraAngle, "Eye level", "eye level, straight-on"),
        (LookKind::CameraAngle, "From below", "from below, low angle"),
        (LookKind::CameraAngle, "From above", "from above, high angle"),
        (LookKind::CameraAngle, "Bird's-eye view", "from above, bird's-eye view"),
        (LookKind::CameraAngle, "Worm's-eye view", "from below, worm's-eye view"),
        (LookKind::CameraAngle, "Dutch angle", "dutch angle, tilted"),
        (LookKind::CameraAngle, "Close-up", "close-up, face focus"),
        (LookKind::CameraAngle, "Extreme close-up", "extreme close-up, eye focus"),
        (LookKind::CameraAngle, "Portrait", "portrait, upper body"),
        (LookKind::CameraAngle, "Upper body", "upper body"),
        (LookKind::CameraAngle, "Cowboy shot", "cowboy shot"),
        (LookKind::CameraAngle, "Full body", "full body"),
        (LookKind::CameraAngle, "Wide shot", "wide shot, scenery"),
        (LookKind::CameraAngle, "POV", "pov"),
        (LookKind::CameraAngle, "Over-the-shoulder", "over the shoulder, from behind"),
        (LookKind::CameraAngle, "Profile", "from side, profile"),
        (LookKind::CameraAngle, "From behind", "from behind, back view"),
        (LookKind::CameraAngle, "Front view", "front view, facing viewer"),
        (LookKind::CameraAngle, "Three-quarter view", "three-quarter view"),
        (LookKind::CameraAngle, "Selfie", "selfie, arm at own side extended"),
        (LookKind::CameraAngle, "Fisheye", "fisheye, wide-angle lens"),
        (LookKind::CameraAngle, "Bokeh", "depth of field, bokeh, blurry background"),
        (LookKind::CameraAngle, "Foreshortening", "foreshortening, dynamic angle"),
        // ---- Environments ----
        (LookKind::Environment, "Neon city night", "neon lights, city street, night, rain, reflections"),
        (LookKind::Environment, "Cyberpunk alley", "cyberpunk, alley, neon signs, rain, night"),
        (LookKind::Environment, "Cherry blossoms", "cherry blossoms, falling petals, spring, park"),
        (LookKind::Environment, "Beach sunset", "beach, ocean, sunset, palm trees"),
        (LookKind::Environment, "Snowy mountain", "snow, mountains, winter, overcast"),
        (LookKind::Environment, "Forest", "forest, trees, dappled sunlight, nature"),
        (LookKind::Environment, "Bamboo forest", "bamboo forest, green, serene"),
        (LookKind::Environment, "Foggy forest", "fog, mist, forest, eerie atmosphere"),
        (LookKind::Environment, "Flower field", "flower field, meadow, blue sky"),
        (LookKind::Environment, "Autumn park", "autumn leaves, maple, park, orange tones"),
        (LookKind::Environment, "Sunset field", "grassland, sunset, golden hour, warm light"),
        (LookKind::Environment, "Starry night", "starry sky, night, milky way, outdoors"),
        (LookKind::Environment, "Rainy street", "rain, wet street, umbrella, reflections"),
        (LookKind::Environment, "Snowfall city", "snowing, city, winter, street lights"),
        (LookKind::Environment, "City rooftop", "rooftop, city skyline, evening"),
        (LookKind::Environment, "Rooftop garden", "rooftop garden, plants, city view"),
        (LookKind::Environment, "Cozy bedroom", "bedroom, bed, indoors, warm lighting"),
        (LookKind::Environment, "Classroom", "classroom, desks, window light, school"),
        (LookKind::Environment, "Library", "library, bookshelves, indoors"),
        (LookKind::Environment, "Coffee shop", "cafe, coffee shop, indoors, cozy"),
        (LookKind::Environment, "Night market", "night market, food stalls, lanterns, crowd"),
        (LookKind::Environment, "Japanese room", "tatami, japanese-style room, shoji, indoors"),
        (LookKind::Environment, "Shrine", "shrine, torii, stone path"),
        (LookKind::Environment, "Onsen", "onsen, hot spring, steam, rocks"),
        (LookKind::Environment, "Cathedral", "cathedral, stained glass, gothic architecture"),
        (LookKind::Environment, "Ancient ruins", "ancient ruins, overgrown, moss, stone"),
        (LookKind::Environment, "Medieval castle", "castle, medieval, stone walls, banners"),
        (LookKind::Environment, "Futuristic city", "futuristic city, skyscrapers, sci-fi, holograms"),
        (LookKind::Environment, "Spaceship interior", "spaceship interior, sci-fi, control panels"),
        (LookKind::Environment, "Outer space", "outer space, stars, nebula, galaxy"),
        (LookKind::Environment, "Underwater", "underwater, bubbles, sunlight rays, ocean"),
        (LookKind::Environment, "Desert", "desert, sand dunes, clear sky"),
        (LookKind::Environment, "Waterfall", "waterfall, river, rocks, mist"),
        (LookKind::Environment, "Fantasy landscape", "fantasy landscape, floating islands, epic scale"),
        (LookKind::Environment, "Amusement park", "amusement park, ferris wheel, festive"),
        (LookKind::Environment, "Train interior", "train interior, seats, window, city passing by"),
    ];
    DATA.iter()
        .map(|&(kind, name, prompt)| CharacterLook {
            name: name.to_string(),
            prompt: prompt.to_string(),
            portrait_key: String::new(),
            kind,
        })
        .collect()
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
    /// Swappable looks (outfit / accessories / pose / scene) layered on the identity at apply time.
    /// Empty for cards from before looks existed — they simply apply identity-only.
    #[serde(default)]
    pub looks: Vec<CharacterLook>,
    /// Gallery item key (`subfolder/filename`) of the card's profile picture; empty = none.
    #[serde(default)]
    pub portrait_key: String,
    /// Server album id collecting this character's matched images; 0 = none yet.
    #[serde(default)]
    pub album_id: i64,
}

/// Bookkeeping for the currently-applied character. Applying a character is a clean reset (clear
/// prompts / LoRAs, apply the model's quality tags, layer the character on), so removal restores
/// [`Self::prev`] verbatim rather than reversing token-by-token. The token fields below are legacy,
/// still produced/consumed by [`Params::apply_character`]/[`Params::remove_character`] and their
/// tests, but the app no longer uses them.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct AppliedCharacter {
    pub name: String,
    /// Full Create params snapshotted just before this character was applied, restored on removal.
    /// `None` for a character applied by an older build (its Remove just clears the marker).
    #[serde(default)]
    pub prev: Option<Params>,
    /// Which look (by name) is currently applied, if any — so the swap UI can highlight it and a
    /// re-apply can preserve it across an edit.
    #[serde(default)]
    pub look: Option<String>,
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

    /// Append a single-axis look's tokens to the positive, returning what was injected so
    /// [`Self::remove_main_look`] can strip it exactly.
    pub fn apply_main_look(&mut self, prompt: &str) -> String {
        merge_triggers(&mut self.positive, prompt, &self.lora_triggers)
    }

    /// Strip a previously applied single-axis look's tokens from the positive.
    pub fn remove_main_look(&mut self, injected: &str) {
        strip_injected(&mut self.positive, injected);
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

/// A user's corrections to one model's defaults, layered over the catalog's parsed `recommended`
/// block. Every field is `None` until the user sets it, so an override only replaces what it names
/// and the rest keeps tracking the server catalog.
///
/// The catalog's numbers are scraped out of Civitai descriptions and example metadata, so they are
/// routinely wrong for a given model; this is the local last word on them.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelOverride {
    /// Forces the loader topology when the folder the file sits in implies the wrong one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_kind: Option<ModelKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steps: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cfg: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampler: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduler: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clip_skip: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clip_names: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clip_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vae: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight_dtype: Option<String>,
}

impl ModelOverride {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// `rec` with every field the user set replaced by theirs.
    pub fn merged(&self, rec: &CheckpointRecommended) -> CheckpointRecommended {
        let mut out = rec.clone();
        if self.steps.is_some() {
            out.steps = self.steps;
        }
        if self.cfg.is_some() {
            out.cfg = self.cfg;
        }
        if self.sampler.is_some() {
            out.sampler = self.sampler.clone();
        }
        if self.scheduler.is_some() {
            out.scheduler = self.scheduler.clone();
        }
        if self.clip_skip.is_some() {
            out.clip_skip = self.clip_skip;
        }
        if self.width.is_some() {
            out.width = self.width;
        }
        if self.height.is_some() {
            out.height = self.height;
        }
        if self.clip_names.is_some() {
            out.clip_names = self.clip_names.clone();
        }
        if self.clip_type.is_some() {
            out.clip_type = self.clip_type.clone();
        }
        if self.vae.is_some() {
            out.vae = self.vae.clone();
        }
        if self.weight_dtype.is_some() {
            out.weight_dtype = self.weight_dtype.clone();
        }
        out
    }

    /// What the user pinned, for the model row's "Yours" line.
    pub fn short_hint(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(k) = self.model_kind {
            parts.push(k.label().to_string());
        }
        if let Some(v) = self.steps {
            parts.push(format!("steps {v}"));
        }
        if let Some(v) = self.cfg {
            parts.push(format!("CFG {v}"));
        }
        if let Some(s) = self.sampler.as_ref().filter(|s| !s.trim().is_empty()) {
            parts.push(s.clone());
        }
        if let Some(s) = self.scheduler.as_ref().filter(|s| !s.trim().is_empty()) {
            parts.push(s.clone());
        }
        if let Some(v) = self.clip_skip {
            parts.push(format!("clip skip {v}"));
        }
        if let (Some(w), Some(h)) = (self.width, self.height) {
            parts.push(format!("{w}×{h}"));
        }
        if let Some(v) = self.clip_names.as_ref().filter(|v| !v.is_empty()) {
            parts.push(format!("CLIP {}", v.iter().map(|s| file_basename(s)).collect::<Vec<_>>().join(" + ")));
        }
        if let Some(s) = self.clip_type.as_ref().filter(|s| !s.trim().is_empty()) {
            parts.push(format!("type {s}"));
        }
        if let Some(s) = self.vae.as_ref().filter(|s| !s.trim().is_empty()) {
            parts.push(format!("VAE {}", file_basename(s)));
        }
        if let Some(s) = self.weight_dtype.as_ref().filter(|s| !s.trim().is_empty()) {
            parts.push(format!("dtype {s}"));
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

/// Which engine rewrites the Create positive prompt. Two exist and they are not interchangeable —
/// comfy-gate's `POST /api/expand` is a server LLM that writes in the checkpoint family's dialect,
/// the on-device pack is a CPU Qwen model that works with no server at all (feature `local-npu`) —
/// so the choice is explicit and the Create button always says which one it will use.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum RewriteEngine {
    /// comfy-gate when it's reachable and has the endpoint, else the on-device pack.
    #[default]
    Auto,
    /// Always comfy-gate `POST /api/expand`.
    Server,
    /// Always the on-device rewrite pack.
    Device,
}

impl RewriteEngine {
    pub const ALL: [RewriteEngine; 3] = [Self::Auto, Self::Server, Self::Device];

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Server => "comfy-gate",
            Self::Device => "On device",
        }
    }

    /// The engine that will actually run, or `None` when the chosen one can't. Only `Auto` ever
    /// switches engines: an explicit pick that isn't available reports unavailable rather than
    /// quietly running the other one, so what the user chose is what the button offers.
    pub fn resolve(self, server_ok: bool, device_ok: bool) -> Option<RewriteEngine> {
        match self {
            Self::Auto if server_ok => Some(Self::Server),
            Self::Auto if device_ok => Some(Self::Device),
            Self::Auto => None,
            Self::Server => server_ok.then_some(Self::Server),
            Self::Device => device_ok.then_some(Self::Device),
        }
    }
}

/// How far `POST /api/variations` may drift from the prompt it was given.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum VariationStrength {
    Subtle,
    #[default]
    Moderate,
    Wild,
}

impl VariationStrength {
    pub const ALL: [VariationStrength; 3] = [Self::Subtle, Self::Moderate, Self::Wild];

    /// The wire value; the server takes these three and nothing else.
    pub fn key(self) -> &'static str {
        match self {
            Self::Subtle => "subtle",
            Self::Moderate => "moderate",
            Self::Wild => "wild",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Subtle => "Subtle",
            Self::Moderate => "Moderate",
            Self::Wild => "Wild",
        }
    }
}

/// How many alternatives to ask `/api/variations` for; the server clamps to 1..=6.
pub const VARIATION_COUNT_RANGE: std::ops::RangeInclusive<u32> = 1..=6;

pub fn default_variation_count() -> u32 {
    3
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

/// Whether `model_file` belongs to a family that only ever ships as a bare diffusion model, so a
/// copy of it sitting in `models/checkpoints` still needs its text encoder and VAE loaded
/// separately ([`ModelKind::CheckpointDiffusion`]) rather than through a `CheckpointLoaderSimple`
/// whose CLIP and VAE outputs are null.
///
/// Deliberately narrow: Flux, Qwen-Image and SD3 all have real all-in-one checkpoint releases, so
/// only Anima is listed, and a catalogued family settles it outright. Guessing from the path is a
/// last resort for an uncatalogued download, and even then a whole `anima` token is not enough on
/// its own — AnimaPencil XL is an SDXL merge, so an SDXL-family marker anywhere in the path vetoes
/// the guess. Whole tokens throughout, since `animagine` and `animatediff` are ordinary
/// checkpoints.
pub fn is_clipless_family(model_file: &str, family: &str) -> bool {
    let fam = family.trim();
    if !fam.is_empty() && !fam.eq_ignore_ascii_case("Other") {
        return fam.eq_ignore_ascii_case("Anima");
    }
    let tokens: Vec<&str> = model_file
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();
    let has = |want: &str| tokens.iter().any(|t| t.eq_ignore_ascii_case(want));
    has("anima")
        && !["xl", "sdxl", "sd15", "pony", "illustrious", "noobai", "flux", "wan"]
            .iter()
            .any(|m| has(m))
}

/// Family bucket for an installed checkpoint (catalog metadata, else `"Other"`).
pub fn checkpoint_family(entry: Option<&CheckpointEntry>) -> String {
    entry.map(|e| e.family_label()).unwrap_or_else(|| "Other".into())
}

/// Map a catalog family label ([`checkpoint_family`] / [`pretty_model_family`]) onto a comfy-gate
/// `POST /api/expand` dialect key, so the rewrite is written in the tag dialect that family wants.
/// `None` for families the gate has no dialect for (or `"Other"`, i.e. no catalog entry) — the
/// caller then sends the loader filename instead and lets the gate classify it.
///
/// Catalog metadata beats the filename when we have it: `family_label` comes from Civitai's
/// `base_model`, so a checkpoint filed under a non-obvious name still lands on the right dialect.
pub fn expand_dialect_key(family: &str) -> Option<&'static str> {
    let key = family.to_ascii_lowercase().replace([' ', '_', '-', '.'], "");
    Some(match key.as_str() {
        // NoobAI is Illustrious-derived and shares its danbooru dialect (as `lint::FAMILY_QUALITY`
        // already groups them).
        "illustrious" | "illustriousxl" | "noobai" | "noobaixl" => "illustrious",
        "pony" | "ponydiffusion" | "ponyxl" => "pony",
        "anima" => "anima",
        "fluxkontext" | "flux1kontext" => "flux-kontext",
        "flux" | "flux1" | "fluxdev" | "flux1dev" | "flux1d" | "fluxschnell" | "flux1schnell"
        | "flux1s" => "flux",
        "hunyuan" | "hunyuandit" | "hunyuanvideo" => "hunyuan",
        "sdxl" | "sdxl10" | "sdxlturbo" | "stablediffusionxl" => "sdxl",
        "sd15" | "stablediffusion15" => "sd15",
        "ltxv" | "ltx" => "ltxv",
        "zimage" => "zimage",
        _ => return None,
    })
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

/// Step for the LoRA strength ◀ / ▶ buttons, and the granularity the slider track grows by.
pub const LORA_STRENGTH_STEP: f32 = 0.5;

/// ComfyUI's own `LoraLoader` bound. Wide enough to be "any number" in practice, but it keeps a
/// held-down bump button from running off to infinity.
pub const LORA_STRENGTH_LIMIT: f32 = 100.0;

/// Track bounds for a LoRA the catalog knows no range for.
pub const LORA_STRENGTH_FALLBACK: (f32, f32) = (-2.0, 2.0);

/// Nudge a strength by `delta`, snapped to two decimals (so 0.5 steps off a catalogued 0.85 stay
/// reversible) and held inside ComfyUI's own limits.
pub fn bump_lora_strength(value: f32, delta: f32) -> f32 {
    let v = ((value + delta) * 100.0).round() / 100.0;
    v.clamp(-LORA_STRENGTH_LIMIT, LORA_STRENGTH_LIMIT)
}

/// Slider track for a LoRA strength: `base` (the catalogued range, or [`LORA_STRENGTH_FALLBACK`]),
/// widened by one [`LORA_STRENGTH_STEP`] at each end and then far enough to contain `values`.
///
/// A catalogued range is only ever a *hint*. Civitai's "recommended" numbers are routinely narrower
/// than what a LoRA actually tolerates — the example images under the same LoRA often run well past
/// them — so a hard cap there makes good settings unreachable. The recommendation decides where the
/// track spends its precision; it never decides what is allowed. The ◀ / ▶ buttons and the typed
/// value box are free to leave the track, and the track regrows on the next frame to follow.
pub fn lora_strength_range(
    base: Option<(f32, f32)>,
    values: &[f32],
) -> std::ops::RangeInclusive<f32> {
    let (mut lo, mut hi) = base.unwrap_or(LORA_STRENGTH_FALLBACK);
    if hi < lo {
        std::mem::swap(&mut lo, &mut hi);
    }
    lo -= LORA_STRENGTH_STEP;
    hi += LORA_STRENGTH_STEP;
    for v in values {
        let v = v.clamp(-LORA_STRENGTH_LIMIT, LORA_STRENGTH_LIMIT);
        if v < lo {
            lo = (v / LORA_STRENGTH_STEP).floor() * LORA_STRENGTH_STEP;
        }
        if v > hi {
            hi = (v / LORA_STRENGTH_STEP).ceil() * LORA_STRENGTH_STEP;
        }
    }
    lo..=hi.max(lo + LORA_STRENGTH_STEP)
}

/// A user's corrections to one LoRA's catalogued defaults, layered over its [`LoraEntry`] the same
/// way [`ModelOverride`] layers over [`CheckpointRecommended`]. Also stands alone for a LoRA the
/// catalog has never heard of.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LoraOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strength_model: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strength_clip: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strength_model_min: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strength_model_max: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_words: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub negative_words: Option<Vec<String>>,
    /// Chain through `LoraLoaderModelOnly`, leaving the CLIP untouched, whenever this LoRA is added.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_only: Option<bool>,
}

impl LoraOverride {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// `entry` with every field the user set replaced by theirs.
    pub fn merged(&self, entry: &LoraEntry) -> LoraEntry {
        let mut out = entry.clone();
        if let Some(v) = self.strength_model {
            out.strength_model = v;
        }
        if let Some(v) = self.strength_clip {
            out.strength_clip = v;
        }
        if self.strength_model_min.is_some() {
            out.strength_model_min = self.strength_model_min;
        }
        if self.strength_model_max.is_some() {
            out.strength_model_max = self.strength_model_max;
        }
        if let Some(v) = self.trigger_words.clone() {
            out.trigger_words = v;
        }
        if let Some(v) = self.negative_words.clone() {
            out.negative_words = v;
        }
        // A pinned strength is a decision, not a suggestion — widen the catalogued window to
        // contain it so [`LoraEntry::add_strengths`]'s clamp can't pull it back on the next Add.
        // The window is only ever a precision hint (see [`lora_strength_range`]).
        if self.strength_model.is_some() || self.strength_clip.is_some() {
            for v in [out.strength_model, out.strength_clip] {
                if let Some(lo) = out.strength_model_min.as_mut()
                    && v < *lo
                {
                    *lo = v;
                }
                if let Some(hi) = out.strength_model_max.as_mut()
                    && v > *hi
                {
                    *hi = v;
                }
            }
        }
        if self.strength_model.is_some() || self.strength_model_min.is_some() {
            out.strength_source = "yours".into();
        }
        out
    }

    /// What the user pinned, for the LoRA card's "Yours" line.
    pub fn short_hint(&self) -> Option<String> {
        let mut parts = Vec::new();
        match (self.strength_model, self.strength_clip) {
            (Some(m), Some(c)) if (m - c).abs() >= 0.005 => {
                parts.push(format!("model {m:.2} · CLIP {c:.2}"))
            }
            (Some(m), _) => parts.push(format!("strength {m:.2}")),
            (None, Some(c)) => parts.push(format!("CLIP {c:.2}")),
            (None, None) => {}
        }
        match (self.strength_model_min, self.strength_model_max) {
            (Some(a), Some(b)) => parts.push(format!("{a:.2}–{b:.2}")),
            (Some(a), None) => parts.push(format!("min {a:.2}")),
            (None, Some(b)) => parts.push(format!("max {b:.2}")),
            _ => {}
        }
        if self.trigger_words.is_some() {
            parts.push("triggers".into());
        }
        if self.negative_words.is_some() {
            parts.push("negatives".into());
        }
        match self.model_only {
            Some(true) => parts.push("model only".into()),
            Some(false) => parts.push("CLIP chain kept".into()),
            None => {}
        }
        (!parts.is_empty()).then(|| parts.join(" · "))
    }
}

impl LoraEntry {
    /// A catalog-shaped entry for a LoRA the catalog has no row for, so an override still has a
    /// base to merge onto.
    pub fn bare(file: &str) -> Self {
        Self {
            file: file.to_string(),
            name: String::new(),
            bases: Vec::new(),
            checkpoints: Vec::new(),
            strength_model: default_lora_strength(),
            strength_clip: default_lora_strength(),
            strength_model_min: None,
            strength_model_max: None,
            strength_source: default_strength_source(),
            trigger_words: Vec::new(),
            negative_words: Vec::new(),
            notes: String::new(),
            tags: Vec::new(),
        }
    }

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

    /// Catalogued strength window, with [`LORA_STRENGTH_FALLBACK`] filling in whichever end the
    /// catalog left blank. Shapes the slider track only — see [`lora_strength_range`].
    pub fn strength_range(&self) -> (f32, f32) {
        let lo = self.strength_model_min.unwrap_or(LORA_STRENGTH_FALLBACK.0);
        let hi = self.strength_model_max.unwrap_or(LORA_STRENGTH_FALLBACK.1.max(lo));
        if hi < lo { (hi, lo) } else { (lo, hi) }
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

/// Wan generation parsed from a model/LoRA path: "wan2.2"/"wan22"/"wan_2-1" → `(2, 2)`/`(2, 1)`.
/// A high/low-noise expert marker implies 2.2 (the two-expert split only exists there).
pub fn wan_version(path: &str) -> Option<(u8, u8)> {
    let lower = path.to_ascii_lowercase();
    let b = lower.as_bytes();
    let mut from = 0;
    while let Some(pos) = lower[from..].find("wan").map(|p| p + from) {
        from = pos + 3;
        if pos > 0 && b[pos - 1].is_ascii_alphanumeric() {
            continue;
        }
        let mut i = from;
        while i < b.len() && matches!(b[i], b' ' | b'_' | b'.' | b'-' | b'v') {
            i += 1;
        }
        let Some(&major) = b.get(i).filter(|c| c.is_ascii_digit()) else { continue };
        // No Wan 1.x generation exists — a leading `1` is a parameter count (14B, 1.3B), not a
        // version. Retry the scan so a later real version token still wins.
        if major == b'1' {
            continue;
        }
        i += 1;
        while i < b.len() && matches!(b[i], b' ' | b'_' | b'.' | b'-') {
            i += 1;
        }
        let Some(&minor) = b.get(i).filter(|c| c.is_ascii_digit()) else { continue };
        // A digit run followed by `b` is a parameter count (`5b`, `14b`), not `major.minor`.
        if matches!(b.get(i + 1), Some(b'b')) {
            continue;
        }
        return Some((major - b'0', minor - b'0'));
    }
    for marker in ["high_noise", "highnoise", "high-noise", "low_noise", "lownoise", "low-noise"] {
        if lower.contains(marker) {
            return Some((2, 2));
        }
    }
    None
}

/// Whether a path names a high-noise Wan expert.
pub fn is_wan_high_noise(path: &str) -> bool {
    let l = path.to_ascii_lowercase();
    l.contains("high_noise") || l.contains("highnoise") || l.contains("high-noise")
}

/// Whether a path names a low-noise Wan expert.
pub fn is_wan_low_noise(path: &str) -> bool {
    let l = path.to_ascii_lowercase();
    l.contains("low_noise") || l.contains("lownoise") || l.contains("low-noise")
}

/// Pick high/low Wan UNETs for t2v or i2v from `unets` (marker match first, then any Wan pair).
pub fn pick_wan_unet_pair(unets: &[String], t2v: bool) -> (Option<String>, Option<String>) {
    let marker = if t2v { "t2v" } else { "i2v" };
    let pick = |require_marker: bool| -> (Option<String>, Option<String>) {
        let mut high = None;
        let mut low = None;
        for u in unets {
            if !is_wan_related(u) {
                continue;
            }
            let l = u.to_ascii_lowercase();
            if require_marker && !l.contains(marker) {
                continue;
            }
            if high.is_none() && is_wan_high_noise(u) {
                high = Some(u.clone());
            } else if low.is_none() && is_wan_low_noise(u) {
                low = Some(u.clone());
            }
            if high.is_some() && low.is_some() {
                break;
            }
        }
        (high, low)
    };
    let (mut high, mut low) = pick(true);
    if high.is_none() || low.is_none() {
        let (h2, l2) = pick(false);
        high = high.or(h2);
        low = low.or(l2);
    }
    if high.is_none() || low.is_none() {
        let d = VideoParams::default();
        if t2v {
            high = high.or_else(|| Some(d.unet_high.replace("i2v", "t2v")));
            low = low.or_else(|| Some(d.unet_low.replace("i2v", "t2v")));
        } else {
            high = high.or(Some(d.unet_high));
            low = low.or(Some(d.unet_low));
        }
    }
    (high, low)
}

/// Whether the path looks Wan-related: a `wan` token (followed by a non-letter or a known
/// family suffix like wanvideo/wanimate/wanx2v), a `wan` directory, or the lightx2v
/// speed-LoRA family. Generic video tokens (i2v/t2v) alone don't qualify — other video
/// families use them too.
pub fn is_wan_related(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    if lower.contains("lightx2v") {
        return true;
    }
    let b = lower.as_bytes();
    let mut from = 0;
    while let Some(pos) = lower[from..].find("wan").map(|p| p + from) {
        from = pos + 3;
        if pos > 0 && b[pos - 1].is_ascii_alphanumeric() {
            continue;
        }
        let rest = &lower[pos + 3..];
        let right_ok = !rest.starts_with(|c: char| c.is_ascii_alphabetic())
            || rest.starts_with("video")
            || rest.starts_with("imate")
            || rest.starts_with("x2");
        if right_ok {
            return true;
        }
    }
    false
}

/// Split a comma-separated trigger list into trimmed tokens.
pub fn split_triggers(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// Whether `trigger` already appears as a chip token in any haystack. Folds case and underscores
/// and peels attention weights, so `long_hair` / `(long hair:1.2)` count as the same token.
fn trigger_present(haystacks: &[&str], trigger: &str) -> bool {
    let needle = crate::tags::fold(trigger);
    if needle.is_empty() {
        return true;
    }
    haystacks
        .iter()
        .any(|hay| crate::tags::parse_chips(hay).iter().any(|c| crate::tags::fold(&c.tag) == needle))
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
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
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
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
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
    /// Exact LoRA filename from `/gallery/api/facets`; empty = all. Server-side filter (needs a
    /// gate that indexes LoRAs; older gates ignore the param and return everything). Session-only
    /// (`skip`): a persisted value would apply to a different account or a LoRA-unaware gate where
    /// it can't be cleared or even seen, silently mis-filtering the whole namespace.
    #[serde(skip)]
    pub lora: String,
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
            lora: String::new(),
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
    /// Translucent CPU/memory/task HUD in the top-right corner.
    #[serde(default)]
    pub perf_overlay: bool,
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
    /// Deleted gallery keys, sorted so an unchanged set serializes identically. Kept across
    /// restarts so a row the server index resurrects stays hidden instead of coming back on launch.
    #[serde(default)]
    pub trash_tombstones: Vec<Tombstone>,
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
    /// User-added guided-wizard chips, keyed by trait title, so a tag typed into "Anything else"
    /// resurfaces as a selectable chip on every future run of that step.
    #[serde(default)]
    pub wizard_custom_tags: std::collections::BTreeMap<String, Vec<String>>,
    /// Global single-axis looks (camera angles / environments) not tied to any character, shown in
    /// every Create-Main look combobox.
    #[serde(default)]
    pub global_looks: Vec<CharacterLook>,
    /// Current Create-Main combobox selections (at most one per single-axis kind), with undo records.
    #[serde(default)]
    pub active_main_looks: Vec<AppliedMainLook>,
    /// Which engine the Create prompt rewrite button uses (server `/api/expand` vs on-device pack).
    #[serde(default)]
    pub rewrite_engine: RewriteEngine,
    /// How many alternatives the Variations button asks for (`/api/variations` `count`).
    #[serde(default = "default_variation_count")]
    pub variation_count: u32,
    /// How far those alternatives may drift (`/api/variations` `strength`).
    #[serde(default)]
    pub variation_strength: VariationStrength,
    /// LoRA files whose Model/CLIP strengths the user explicitly unlinked; every other slot moves
    /// the pair together. Only slots whose two numbers currently *match* need recording — one whose
    /// numbers already differ reads as unlinked on its own ([`ActiveLora::strengths_linked`]).
    #[serde(default)]
    pub lora_unlinked: Vec<String>,
    /// User-corrected model defaults, keyed by the ComfyUI loader filename. Layered over the
    /// server catalog's parsed recommendation whenever a model is selected or reset.
    #[serde(default)]
    pub model_overrides: std::collections::BTreeMap<String, ModelOverride>,
    /// User-corrected LoRA defaults, keyed by `lora_name`.
    #[serde(default)]
    pub lora_overrides: std::collections::BTreeMap<String, LoraOverride>,
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

/// One distinct model or LoRA name across the account's gallery, with how many images used it.
#[derive(Clone, Debug, Deserialize)]
pub struct ModelFacet {
    pub name: String,
    #[serde(default)]
    pub count: i64,
}

/// `GET /gallery/api/facets` — the source of the model filter's options and the checkpoint/LoRA
/// example-count lookups. `loras` is empty on gates that don't index LoRAs yet.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Facets {
    #[serde(default)]
    pub models: Vec<ModelFacet>,
    #[serde(default)]
    pub loras: Vec<ModelFacet>,
}

impl Facets {
    /// The exact indexed name and image count for a checkpoint, matched basename-insensitively so a
    /// picker entry like `Pony/foo.safetensors` finds an index value of `foo.safetensors` (and the
    /// reverse). The returned name is the exact string to pass as the server `model` filter.
    pub fn model_example<'a>(&'a self, file: &str) -> Option<(&'a str, i64)> {
        facet_match(&self.models, file)
    }

    /// The exact indexed name and image count for a LoRA (the `lora` filter value), same match.
    pub fn lora_example<'a>(&'a self, file: &str) -> Option<(&'a str, i64)> {
        facet_match(&self.loras, file)
    }
}

fn facet_match<'a>(facets: &'a [ModelFacet], file: &str) -> Option<(&'a str, i64)> {
    // Exact name wins; basename is only a fallback. The gate sorts facets by count, so without
    // the exact-first pass a higher-count same-basename file in another folder (SDXL/x vs Pony/x)
    // would shadow the one actually picked — showing its count and opening its gallery.
    if let Some(f) = facets.iter().find(|f| f.name == file) {
        return Some((f.name.as_str(), f.count));
    }
    let base = file_basename(file);
    facets
        .iter()
        .find(|f| file_basename(&f.name) == base)
        .map(|f| (f.name.as_str(), f.count))
}

/// The trailing save counter in a ComfyUI output filename (`str0.4_00003_.png` -> 3): the last
/// all-digit `_` segment of the stem. Same-counter files across sibling strength folders are the
/// same seed of a sweep, which is what pairs them for review.
pub fn file_counter(name: &str) -> Option<u64> {
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
    stem.rsplit('_')
        .find(|seg| !seg.is_empty() && seg.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|seg| seg.parse().ok())
}

/// One row of the server trash listing (`/gallery/api/list?trash=1`). `subfolder`/`filename`
/// point at the file inside `.trash/` (thumb-fetchable); `orig_*` say where restore puts it back.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct TrashItem {
    pub id: i64,
    pub subfolder: String,
    pub filename: String,
    #[serde(default)]
    pub orig_subfolder: String,
    #[serde(default)]
    pub orig_filename: String,
    /// Unix seconds when it was deleted.
    #[serde(default)]
    pub deleted: f64,
    #[serde(default)]
    pub is_video: bool,
}

impl TrashItem {
    /// Thumbnail cache key, matching [`GalleryItem::thumb_key`].
    pub fn thumb_key(&self, size: u32) -> String {
        format!("{}/{}#{size}", self.subfolder, self.filename)
    }
}

/// A deleted gallery row, remembered so a listing indexed mid-delete can't put it back on screen.
///
/// `size` is the image's byte length when it was deleted. ComfyUI numbers a save `max(existing) + 1`
/// over the output folder and comfy-gate's soft delete moves the file out of that folder, so the
/// next render can be handed the same filename — a row with this name but a different size is a new
/// image, not the deleted one.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Tombstone {
    pub subfolder: String,
    pub filename: String,
    #[serde(default)]
    pub size: u64,
    /// Unix seconds at delete time.
    #[serde(default)]
    pub at: f64,
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

    /// merge_triggers folds underscores/case/weights, so a trigger already present in a different
    /// spelling is not re-added as a duplicate.
    #[test]
    fn merge_triggers_folds_spelling_variants() {
        // Already present as "long_hair" in the positive → not re-added.
        let mut dest = String::new();
        let added = merge_triggers(&mut dest, "long hair", "1girl, long_hair");
        assert_eq!(added, "");
        assert_eq!(dest, "");
        // Weighted variant in dest counts as present too.
        let mut dest = "(long hair:1.2)".to_string();
        let added = merge_triggers(&mut dest, "long_hair, glow", "");
        assert_eq!(added, "glow");
        assert_eq!(dest, "(long hair:1.2), glow");
    }

    /// Example-count lookup: exact name beats a basename collision, even when the collision has a
    /// higher count and sorts first (the gate returns facets count-descending).
    #[test]
    fn facet_match_prefers_exact_over_basename() {
        let facets = vec![
            ModelFacet { name: "SDXL/detail.safetensors".into(), count: 10 },
            ModelFacet { name: "Pony/detail.safetensors".into(), count: 3 },
        ];
        let f = Facets { models: facets.clone(), loras: facets };
        assert_eq!(f.model_example("Pony/detail.safetensors"), Some(("Pony/detail.safetensors", 3)));
        assert_eq!(f.lora_example("SDXL/detail.safetensors"), Some(("SDXL/detail.safetensors", 10)));
        // No exact match → basename fallback still resolves (bare picker name vs stored path).
        assert_eq!(f.model_example("detail.safetensors").map(|(_, c)| c), Some(10));
        assert_eq!(f.model_example("missing.safetensors"), None);
    }

    /// Wan version parsing across the separator/marker spellings seen in the wild.
    #[test]
    fn wan_version_parses_common_spellings() {
        assert_eq!(wan_version("Wan/wan2.2_i2v_high_noise_14B_fp8_scaled.safetensors"), Some((2, 2)));
        assert_eq!(wan_version("wan22_lora.safetensors"), Some((2, 2)));
        assert_eq!(wan_version("Wan-2.1-t2v.safetensors"), Some((2, 1)));
        assert_eq!(wan_version("wan_2_1/motion.safetensors"), Some((2, 1)));
        // A version token detached from the wan word is as likely the LoRA's own revision
        // ("..._lora_v1_..."), so it stays unknown — unknown shows under both unets.
        assert_eq!(wan_version("WanVideo_v2.2.safetensors"), None);
        // The two-expert split only exists in 2.2.
        assert_eq!(wan_version("Wan/SmoothMix_High_Noise.safetensors"), Some((2, 2)));
        // Parameter counts (14B, 1.3B) next to the wan token are NOT versions.
        assert_eq!(wan_version("Wan14Bi2vFusionX.safetensors"), None);
        assert_eq!(wan_version("wan_14B_i2v_lora.safetensors"), None);
        assert_eq!(wan_version("wan1.3b_vace.safetensors"), None);
        // A real version token still wins over a later size token.
        assert_eq!(wan_version("wan2.1_i2v_14B_fp8.safetensors"), Some((2, 1)));
        // Unversioned wan names stay unknown; unrelated names never match.
        assert_eq!(wan_version("Wan/wan_motion.safetensors"), None);
        assert_eq!(wan_version("SDXL/swan_2.2_style.safetensors"), None);
        assert_eq!(wan_version("detail_tweaker.safetensors"), None);
    }

    #[test]
    fn pick_wan_unet_pair_prefers_marker_then_any_wan() {
        let unets = vec![
            "other.safetensors".into(),
            "Wan/wan2.2_t2v_high_noise_14B.safetensors".into(),
            "Wan/wan2.2_t2v_low_noise_14B.safetensors".into(),
            "Wan/wan2.2_i2v_high_noise_14B.safetensors".into(),
            "Wan/wan2.2_i2v_low_noise_14B.safetensors".into(),
        ];
        let (h, l) = pick_wan_unet_pair(&unets, false);
        assert!(h.unwrap().contains("i2v"));
        assert!(l.unwrap().contains("i2v"));
        let (h, l) = pick_wan_unet_pair(&unets, true);
        assert!(h.unwrap().contains("t2v"));
        assert!(l.unwrap().contains("t2v"));
    }

    /// Wan-relatedness: wan token or directory, lightx2v family; generic video tokens don't count.
    #[test]
    fn is_wan_related_needs_a_wan_marker() {
        assert!(is_wan_related("Wan/anything.safetensors"));
        assert!(is_wan_related("wan2.2_i2v_lightx2v_4steps_lora_v1_high_noise.safetensors"));
        assert!(is_wan_related("lightx2v_distill.safetensors"));
        assert!(is_wan_related("wanimate_style.safetensors"));
        assert!(is_wan_related("WanVideo_style.safetensors"));
        assert!(!is_wan_related("SDXL/swan_lake.safetensors"));
        assert!(!is_wan_related("hunyuan_t2v_lora.safetensors"));
        assert!(!is_wan_related("detail_tweaker_xl.safetensors"));
        assert!(!is_wan_related("wand_of_magic.safetensors"));
    }

    /// The sweep-pairing counter: last all-digit `_` segment of the stem, or None.
    #[test]
    fn file_counter_reads_the_save_counter() {
        assert_eq!(file_counter("str0.4_00003_.png"), Some(3));
        assert_eq!(file_counter("comfyui_android_00221_.png"), Some(221));
        assert_eq!(file_counter("baseline_no_lora_00002_.png"), Some(2));
        // No counter: names without an all-digit segment.
        assert_eq!(file_counter("cover.png"), None);
        assert_eq!(file_counter("str0.4.png"), None);
    }

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
    fn settings_without_lora_unlinked_default_empty() {
        let params = serde_json::to_value(Params::default()).unwrap();
        let json = serde_json::json!({"server_url": "http://x", "params": params});
        let s: Settings = serde_json::from_value(json).unwrap();
        assert!(s.lora_unlinked.is_empty());
    }

    /// The promotion that fixes an Anima DiT filed under `models/checkpoints`, and the near-misses
    /// it must not touch — `animagine` and `animatediff` are ordinary checkpoints whose CLIP the
    /// graph really does read off the checkpoint loader.
    #[test]
    fn only_whole_token_anima_needs_separate_companions() {
        assert!(is_clipless_family("Anima/novaAnimeAM_v30.safetensors", "Other"));
        assert!(is_clipless_family("novaAnimeAM_v30.safetensors", "Anima"));
        assert!(is_clipless_family("anima-nova.safetensors", "Other"));
        assert!(is_clipless_family("nova_anima_v3.safetensors", ""));

        assert!(!is_clipless_family("animagineXL_v31.safetensors", "SDXL"));
        assert!(!is_clipless_family("AnimateDiff/mm_sd15.ckpt", "SD 1.5"));
        assert!(!is_clipless_family("novaAnimeXL_v10.safetensors", "Illustrious"));
        assert!(!is_clipless_family("JANKU_v777.safetensors", "Illustrious"));
        assert!(!is_clipless_family("", ""));
    }

    /// A catalogued family settles it, and an uncatalogued SDXL merge whose name happens to carry a
    /// standalone `anima` token must not be rebuilt under the split-companion loader — AnimaPencil
    /// XL is a working all-in-one checkpoint.
    #[test]
    fn a_catalogued_family_and_an_sdxl_marker_both_veto_the_anima_guess() {
        assert!(!is_clipless_family("Anima Pencil XL/animaPencilXL_v500.safetensors", "SDXL"));
        assert!(!is_clipless_family("Anima_Pencil_XL_v5.safetensors", "Other"));
        assert!(!is_clipless_family("anima pencil xl.safetensors", ""));
        assert!(!is_clipless_family("Anima/whatever.safetensors", "Illustrious"));
        // Still promoted when the catalog agrees, or says nothing and no other family is named.
        assert!(is_clipless_family("Anima Pencil XL/animaPencilXL_v500.safetensors", "Anima"));
        assert!(is_clipless_family("Anima/novaAnimeAM_v30.safetensors", ""));
    }

    /// An unknown `model_kind` (written by a newer build) degrades to a plain checkpoint instead of
    /// failing the whole settings file and taking the server URL and presets with it.
    #[test]
    fn an_unknown_model_kind_loads_as_a_checkpoint() {
        let of = |s: &str| serde_json::from_value::<ModelKind>(serde_json::json!(s)).unwrap();
        assert_eq!(of("Checkpoint"), ModelKind::Checkpoint);
        assert_eq!(of("Diffusion"), ModelKind::Diffusion);
        assert_eq!(of("CheckpointDiffusion"), ModelKind::CheckpointDiffusion);
        assert_eq!(of("SomeFutureTopology"), ModelKind::Checkpoint);
        // And a whole Settings blob carrying one still loads.
        let mut params = serde_json::to_value(Params::default()).unwrap();
        params["model_kind"] = serde_json::json!("SomeFutureTopology");
        let json = serde_json::json!({"server_url": "http://x", "params": params});
        let s: Settings = serde_json::from_value(json).unwrap();
        assert_eq!(s.params.model_kind, ModelKind::Checkpoint);
        assert_eq!(s.server_url, "http://x");
    }

    /// Settings written before the override maps existed load with none, so every model keeps
    /// tracking the server catalog.
    #[test]
    fn settings_without_overrides_default_empty() {
        let params = serde_json::to_value(Params::default()).unwrap();
        let json = serde_json::json!({"server_url": "http://x", "params": params});
        let s: Settings = serde_json::from_value(json).unwrap();
        assert!(s.model_overrides.is_empty());
        assert!(s.lora_overrides.is_empty());
    }

    #[test]
    fn settings_round_trip_the_override_maps() {
        let params = serde_json::to_value(Params::default()).unwrap();
        let json = serde_json::json!({
            "server_url": "", "params": params,
            "model_overrides": {
                "Anima/nova.safetensors": {
                    "model_kind": "CheckpointDiffusion", "steps": 28, "cfg": 3.5,
                    "clip_names": ["qwen_3_06b_base.safetensors"], "vae": "qwen_image_vae.safetensors",
                },
            },
            "lora_overrides": {"style.safetensors": {"strength_model": 0.65, "model_only": true}},
        });
        let s: Settings = serde_json::from_value(json).unwrap();
        let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        let m = &back.model_overrides["Anima/nova.safetensors"];
        assert_eq!(m.model_kind, Some(ModelKind::CheckpointDiffusion));
        assert_eq!(m.steps, Some(28));
        assert_eq!(m.cfg, Some(3.5));
        assert_eq!(m.vae.as_deref(), Some("qwen_image_vae.safetensors"));
        // Untouched fields stay unset rather than being pinned to a stale value.
        assert_eq!(m.sampler, None);
        assert_eq!(m.width, None);
        let l = &back.lora_overrides["style.safetensors"];
        assert_eq!(l.strength_model, Some(0.65));
        assert_eq!(l.model_only, Some(true));
        assert_eq!(l.strength_clip, None);
    }

    /// An override replaces only what it names; every other recommendation keeps coming from the
    /// catalog, so a later catalog refresh still improves the untouched fields.
    #[test]
    fn a_model_override_replaces_only_the_fields_it_sets() {
        let rec = CheckpointRecommended {
            steps: Some(20),
            cfg: Some(7.0),
            sampler: Some("dpmpp_2m".into()),
            width: Some(832),
            height: Some(1216),
            ..Default::default()
        };
        let ov = ModelOverride { cfg: Some(4.0), sampler: Some("euler".into()), ..Default::default() };
        let merged = ov.merged(&rec);
        assert_eq!(merged.cfg, Some(4.0));
        assert_eq!(merged.sampler.as_deref(), Some("euler"));
        assert_eq!(merged.steps, Some(20));
        assert_eq!(merged.width, Some(832));
        assert_eq!(merged.height, Some(1216));
        // An empty override is a no-op.
        assert!(ModelOverride::default().is_empty());
        let untouched = ModelOverride::default().merged(&rec);
        assert_eq!(untouched.cfg, Some(7.0));
        assert_eq!(untouched.sampler.as_deref(), Some("dpmpp_2m"));
    }

    /// A LoRA the catalog has never seen still gets the user's numbers, merged onto a bare entry.
    #[test]
    fn a_lora_override_stands_alone_without_a_catalog_row() {
        let ov = LoraOverride {
            strength_model: Some(0.6),
            strength_clip: Some(0.6),
            trigger_words: Some(vec!["mytrigger".into()]),
            ..Default::default()
        };
        let merged = ov.merged(&LoraEntry::bare("new.safetensors"));
        assert_eq!(merged.file, "new.safetensors");
        assert_eq!(merged.add_strengths(), (0.6, 0.6));
        assert_eq!(merged.trigger_text(), "mytrigger");
        assert_eq!(merged.strength_source, "yours");
        // Over a catalogued row, unset fields keep the catalog's.
        let cat = LoraEntry {
            trigger_words: vec!["catalog trigger".into()],
            negative_words: vec!["bad".into()],
            ..LoraEntry::bare("new.safetensors")
        };
        let only_strength =
            LoraOverride { strength_model: Some(0.3), ..Default::default() }.merged(&cat);
        assert_eq!(only_strength.trigger_text(), "catalog trigger");
        assert_eq!(only_strength.negative_text(), "bad");
        assert_eq!(only_strength.strength_model, 0.3);
    }

    /// The catalogued range is a hint, so an override outside it must survive `add_strengths`'s
    /// clamp rather than being pulled back to the catalog's window.
    #[test]
    fn a_lora_override_widens_the_range_it_needs() {
        let cat = LoraEntry {
            strength_model_min: Some(0.4),
            strength_model_max: Some(0.8),
            ..LoraEntry::bare("x.safetensors")
        };
        let ov = LoraOverride {
            strength_model: Some(1.4),
            strength_clip: Some(1.4),
            strength_model_max: Some(1.5),
            ..Default::default()
        };
        let merged = ov.merged(&cat);
        assert_eq!(merged.add_strengths(), (1.4, 1.4));
        assert_eq!(merged.strength_range(), (0.4, 1.5));
    }

    /// The same, without the user also widening the range by hand: `add_strengths` clamps to the
    /// catalogued window, so a pinned strength outside it would be silently thrown away on Add.
    #[test]
    fn a_pinned_strength_outside_the_catalogued_window_survives_add() {
        let cat = LoraEntry {
            strength_model: 0.7,
            strength_clip: 0.7,
            strength_model_min: Some(0.4),
            strength_model_max: Some(0.8),
            ..LoraEntry::bare("style.safetensors")
        };
        let over =
            LoraOverride { strength_model: Some(1.2), strength_clip: Some(1.2), ..Default::default() };
        assert_eq!(over.merged(&cat).add_strengths(), (1.2, 1.2));
        // Below the window too.
        let under =
            LoraOverride { strength_model: Some(0.1), strength_clip: Some(0.1), ..Default::default() };
        assert_eq!(under.merged(&cat).add_strengths(), (0.1, 0.1));
        // With no override the catalog's own clamp is untouched.
        assert_eq!(LoraOverride::default().merged(&cat).add_strengths(), (0.7, 0.7));
    }

    fn lora_with_range(min: Option<f32>, max: Option<f32>) -> LoraEntry {
        serde_json::from_value(serde_json::json!({
            "file": "x.safetensors",
            "strength_model_min": min,
            "strength_model_max": max,
        }))
        .unwrap()
    }

    #[test]
    fn strength_range_fills_missing_ends_from_the_fallback() {
        assert_eq!(lora_with_range(None, None).strength_range(), LORA_STRENGTH_FALLBACK);
        assert_eq!(lora_with_range(Some(0.5), Some(1.0)).strength_range(), (0.5, 1.0));
        assert_eq!(lora_with_range(Some(0.4), None).strength_range(), (0.4, 2.0));
        assert_eq!(lora_with_range(None, Some(1.2)).strength_range(), (-2.0, 1.2));
        // A catalogued min above the fallback max must not produce an inverted window.
        assert_eq!(lora_with_range(Some(3.0), None).strength_range(), (3.0, 3.0));
        // A catalog that has them backwards is still a usable window.
        assert_eq!(lora_with_range(Some(1.0), Some(0.2)).strength_range(), (0.2, 1.0));
    }

    /// The whole point of the feature: a recommendation of 0.5–1.0 must not stop the user reaching
    /// 3.5, and once there the track must contain it so the handle is still draggable.
    #[test]
    fn slider_track_pads_the_recommendation_and_grows_to_hold_the_value() {
        let rec = Some((0.5, 1.0));
        let r = lora_strength_range(rec, &[1.0, 1.0]);
        assert_eq!((*r.start(), *r.end()), (0.0, 1.5), "one step of headroom past the catalog");

        let r = lora_strength_range(rec, &[3.5, 1.0]);
        assert!(*r.end() >= 3.5, "track must contain a value bumped past the recommendation");
        assert_eq!(*r.end(), 3.5);

        // Grown ends land on the step grid, so the track doesn't jitter as the value moves.
        let r = lora_strength_range(rec, &[2.1, 1.0]);
        assert_eq!(*r.end(), 2.5);
        let r = lora_strength_range(rec, &[-2.1, 1.0]);
        assert_eq!(*r.start(), -2.5);

        // Both strengths are considered, so the two sliders share one track.
        let r = lora_strength_range(rec, &[1.0, 4.0]);
        assert_eq!(*r.end(), 4.0);

        // No catalogued range at all: the ±2 default, padded.
        let r = lora_strength_range(None, &[1.0, 1.0]);
        assert_eq!((*r.start(), *r.end()), (-2.5, 2.5));
    }

    #[test]
    fn slider_track_is_never_empty_or_inverted() {
        for base in [Some((1.0, 1.0)), Some((2.0, -2.0)), None] {
            for v in [-500.0, 0.0, 500.0] {
                let r = lora_strength_range(base, &[v, v]);
                assert!(*r.end() > *r.start(), "{base:?} @ {v}");
            }
        }
    }

    #[test]
    fn bump_steps_by_a_half_reversibly_and_stops_at_comfyui_limits() {
        // Plain arithmetic, not grid-snapping: a catalogued 0.85 survives an up-then-down trip.
        assert_eq!(bump_lora_strength(0.85, LORA_STRENGTH_STEP), 1.35);
        assert_eq!(bump_lora_strength(1.35, -LORA_STRENGTH_STEP), 0.85);
        // Float drift is rounded off so repeated taps stay on clean numbers.
        let mut v = 0.0;
        for _ in 0..7 {
            v = bump_lora_strength(v, LORA_STRENGTH_STEP);
        }
        assert_eq!(v, 3.5);
        assert_eq!(bump_lora_strength(LORA_STRENGTH_LIMIT, LORA_STRENGTH_STEP), LORA_STRENGTH_LIMIT);
        assert_eq!(
            bump_lora_strength(-LORA_STRENGTH_LIMIT, -LORA_STRENGTH_STEP),
            -LORA_STRENGTH_LIMIT
        );
    }

    #[test]
    fn strengths_linked_tracks_equality_not_a_stored_flag() {
        let mut l = card_lora("a.safetensors", 0.8);
        assert!(l.strengths_linked(), "an equal pair is linked by default");
        l.strength_clip = 0.7;
        assert!(!l.strengths_linked(), "a pasted 0.8/0.7 pair reads as unlinked, not collapsed");
        l.strength_clip = 0.8004;
        assert!(l.strengths_linked(), "slider-precision equality still counts");
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

    /// A Wan i2v setup saved as a preset must come back as that Wan setup, not as whatever image
    /// checkpoint happened to be selected — every video field lives in `params.video`.
    #[test]
    fn a_wan_preset_round_trips_every_video_field() {
        let mut p = Params {
            mode: Mode::Video,
            checkpoint: "Illustrious/some_anime_model.safetensors".into(),
            positive: "a cat turning its head".into(),
            ..Default::default()
        };
        p.video.unet_high = "Wan/high.safetensors".into();
        p.video.unet_low = "Wan/low.safetensors".into();
        p.video.clip_name = "umt5.safetensors".into();
        p.video.vae_name = "wan_vae.safetensors".into();
        p.video.length = 65;
        p.video.steps = 6;
        p.video.split_step = 3;
        p.video.cfg_high = 3.5;
        p.video.shift = 8.0;
        p.video.lora_triggers = "smooth motion".into();
        p.video.rife_multiplier = 3;
        p.video.video_t2v = true;
        p.video.loras_low.clear();

        let preset = CreatePreset { name: "wan i2v".into(), params: p.clone() };
        let json = serde_json::to_string(&preset).expect("serialize");
        let back: CreatePreset = serde_json::from_str(&json).expect("deserialize");
        let v = &back.params.video;

        assert_eq!(back.params.gen_mode(), GenMode::Txt2Video);
        assert_eq!(v.unet_high, "Wan/high.safetensors");
        assert_eq!(v.unet_low, "Wan/low.safetensors");
        assert_eq!(v.clip_name, "umt5.safetensors");
        assert_eq!(v.vae_name, "wan_vae.safetensors");
        assert_eq!((v.length, v.steps, v.split_step, v.rife_multiplier), (65, 6, 3, 3));
        assert!((v.cfg_high - 3.5).abs() < 1e-6 && (v.shift - 8.0).abs() < 1e-6);
        assert_eq!(v.lora_triggers, "smooth motion");
        assert_eq!(v.loras_high.len(), 2);
        assert!(v.loras_low.is_empty(), "an emptied low stack must not resurrect its defaults");
        // The image slot rides along untouched, for when the user switches back.
        assert_eq!(back.params.checkpoint, "Illustrious/some_anime_model.safetensors");
    }

    /// A preset file written before a `VideoParams` field existed must still load: a failed
    /// `Settings` parse blocks autosave, taking every preset and credential with it.
    #[test]
    fn video_params_tolerate_a_file_from_an_older_build() {
        let partial = r#"{"unet_high": "Wan/high.safetensors", "length": 49}"#;
        let v: VideoParams = serde_json::from_str(partial).expect("must not fail the whole parse");
        assert_eq!(v.unet_high, "Wan/high.safetensors");
        assert_eq!(v.length, 49);
        assert_eq!(v.clip_type, VideoParams::default().clip_type, "the rest falls back");
        assert_eq!(v.steps, VideoParams::default().steps);
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
            looks: vec![
                CharacterLook {
                    name: "casual".into(),
                    prompt: "hoodie, jeans, standing".into(),
                    portrait_key: "user_x/Mia/casual.png".into(),
                    kind: LookKind::Look,
                },
                CharacterLook {
                    name: "from below".into(),
                    prompt: "low angle, from below".into(),
                    portrait_key: String::new(),
                    kind: LookKind::CameraAngle,
                },
            ],
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

    /// Looks written before categories existed default to the combined `Look` kind; Settings without
    /// the global-look fields load empty.
    #[test]
    fn look_kind_defaults_and_global_looks_backward_compat() {
        let old = r#"{"name": "casual", "prompt": "hoodie"}"#;
        let look: CharacterLook = serde_json::from_str(old).expect("old look must deserialize");
        assert_eq!(look.kind, LookKind::Look);

        let params = serde_json::to_value(Params::default()).unwrap();
        let json = serde_json::json!({"server_url": "http://x", "params": params});
        let s: Settings = serde_json::from_value(json).unwrap();
        assert!(s.global_looks.is_empty());
        assert!(s.active_main_looks.is_empty());
    }

    #[test]
    fn applying_then_removing_a_main_look_restores_the_positive() {
        let mut p = Params {
            positive: "1girl, silver hair".into(),
            lora_triggers: "masterpiece".into(),
            ..Default::default()
        };
        let before = p.positive.clone();
        let injected = p.apply_main_look("low angle, from below");
        assert!(p.positive.contains("low angle"));
        p.remove_main_look(&injected);
        assert_eq!(p.positive, before);
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

    /// Auto is the only setting allowed to switch engines. An explicit pick that can't run must
    /// report unavailable, never quietly run the other engine — the whole point of the setting is
    /// that you can tell where a rewrite came from.
    #[test]
    fn rewrite_engine_resolves_without_silent_fallback() {
        use RewriteEngine::*;
        assert_eq!(Auto.resolve(true, true), Some(Server));
        assert_eq!(Auto.resolve(false, true), Some(Device));
        assert_eq!(Auto.resolve(true, false), Some(Server));
        assert_eq!(Auto.resolve(false, false), None);
        assert_eq!(Server.resolve(true, true), Some(Server));
        assert_eq!(Server.resolve(false, true), None);
        assert_eq!(Device.resolve(true, true), Some(Device));
        assert_eq!(Device.resolve(true, false), None);
        // Never resolves to Auto itself: callers match on the concrete engine.
        for chosen in RewriteEngine::ALL {
            for (s, d) in [(true, true), (true, false), (false, true), (false, false)] {
                assert_ne!(chosen.resolve(s, d), Some(Auto));
            }
        }
    }

    /// Only families comfy-gate actually has a dialect for may map; anything else must return
    /// `None` so the caller falls back to sending the loader filename for the gate to classify.
    #[test]
    fn expand_dialect_key_maps_catalog_families_and_rejects_unknowns() {
        assert_eq!(expand_dialect_key("Illustrious"), Some("illustrious"));
        assert_eq!(expand_dialect_key("NoobAI"), Some("illustrious"));
        assert_eq!(expand_dialect_key("Pony"), Some("pony"));
        assert_eq!(expand_dialect_key("Anima"), Some("anima"));
        assert_eq!(expand_dialect_key("Flux Schnell"), Some("flux"));
        assert_eq!(expand_dialect_key("SDXL Turbo"), Some("sdxl"));
        assert_eq!(expand_dialect_key("SD 1.5"), Some("sd15"));
        // Labels come out of `pretty_model_family`, so its exact spellings must round-trip.
        for fam in ["Illustrious", "Pony", "Flux", "SDXL", "SD 1.5", "Hunyuan", "Anima"] {
            assert_eq!(expand_dialect_key(&pretty_model_family(fam)), expand_dialect_key(fam));
        }
        assert_eq!(expand_dialect_key("Other"), None);
        assert_eq!(expand_dialect_key(""), None);
        assert_eq!(expand_dialect_key("Qwen"), None);
        assert_eq!(expand_dialect_key("Chroma"), None);
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
