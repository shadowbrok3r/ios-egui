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
}

impl GalleryGroup {
    pub fn param(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Folder => "folder",
            Self::Model => "model",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::None => "No grouping",
            Self::Folder => "Folder",
            Self::Model => "Model",
        }
    }

    pub const ALL: &'static [Self] = &[Self::Folder, Self::Model, Self::None];
}

/// The gallery's query + layout state, persisted so the view survives restarts.
#[derive(Clone, Serialize, Deserialize)]
pub struct GalleryView {
    /// Exact model name from `/gallery/api/facets`; empty = all models.
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub album: Option<i64>,
    pub sort: GallerySort,
    pub group: GalleryGroup,
    /// Tiles per row, 1..=3. At 1 the tiles show near-full-resolution images.
    pub columns: usize,
}

impl Default for GalleryView {
    fn default() -> Self {
        Self {
            model: String::new(),
            album: None,
            sort: GallerySort::Newest,
            group: GalleryGroup::Folder,
            columns: 3,
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
    /// Auto-follow: pan/zoom the graph to whichever node is currently executing.
    #[serde(default)]
    pub auto_follow: bool,
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
        };
        assert_eq!(item("user_abc/Character/2026-07-16").group_label(), "Character/2026-07-16");
        // A bare namespace is the account's output root — never show the raw account id.
        assert_eq!(item("shadowbroker_531d823e-4a3b-46c8-9550-2e8f").group_label(), "Output");
        assert_eq!(item("").group_label(), "Output");
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
