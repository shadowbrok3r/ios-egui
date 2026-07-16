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

/// Everything a KSampler txt2img/img2img workflow needs, plus the UI's mode selection.
#[derive(Clone, Serialize, Deserialize)]
pub struct Params {
    pub checkpoint: String,
    pub positive: String,
    pub negative: String,
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
}

impl Default for Params {
    fn default() -> Self {
        Self {
            checkpoint: String::new(),
            positive: String::new(),
            negative: "text, watermark, low quality".to_string(),
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
        }
    }
}

/// Persisted to `<documents>/comfyui_settings.json` so the server + last params survive reinstalls.
#[derive(Clone, Serialize, Deserialize)]
pub struct Settings {
    pub server_url: String,
    #[serde(default)]
    pub api_key: String,
    pub params: Params,
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
