//! WD14 postprocessing (thresholded, ranked tags) and the thin HTP execute path.
//!
//! The classifier is one graph with one image input and one probability output.
//! SmilingWolf's v3 ONNX exports apply per-class sigmoid inside the graph, so the
//! output is already probabilities; [`Wd14Params::apply_sigmoid`] covers exports
//! that emit raw logits instead.

use crate::error::{Error, Result};
use crate::img;
use crate::pack::{Wd14Pack, Wd14Tag, CATEGORY_CHARACTER, CATEGORY_GENERAL, CATEGORY_RATING};
use qnn_rs::{Context, ContextOpts, QnnSystem, Session};

/// One tag with its predicted probability.
#[derive(Clone, Debug, PartialEq)]
pub struct ScoredTag {
    pub name: String,
    pub prob: f32,
}

impl ScoredTag {
    /// Prompt insertion form: underscores rendered as spaces.
    pub fn insert_text(&self) -> String {
        self.name.replace('_', " ")
    }

    /// Probability as a rounded whole percent.
    pub fn percent(&self) -> u32 {
        (self.prob * 100.0).round().clamp(0.0, 100.0) as u32
    }
}

/// Ranked prediction split by danbooru category.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TagResult {
    /// Top-1 rating tag (category 9), if any rows carried it.
    pub rating: Option<ScoredTag>,
    /// General tags (category 0) over the general threshold, probability descending.
    pub general: Vec<ScoredTag>,
    /// Character tags (category 4) over the character threshold, probability descending.
    pub character: Vec<ScoredTag>,
}

impl TagResult {
    /// The first `n` general tags' insertion text.
    pub fn top_general(&self, n: usize) -> Vec<String> {
        self.general.iter().take(n).map(ScoredTag::insert_text).collect()
    }
}

/// Thresholds and sigmoid handling for [`Wd14Params::rank`].
#[derive(Clone, Copy, Debug)]
pub struct Wd14Params {
    pub general_threshold: f32,
    pub character_threshold: f32,
    /// Apply sigmoid to the graph output first (only for raw-logit exports).
    pub apply_sigmoid: bool,
}

impl Default for Wd14Params {
    fn default() -> Self {
        Self { general_threshold: 0.35, character_threshold: 0.85, apply_sigmoid: false }
    }
}

fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

fn sort_desc(v: &mut [ScoredTag]) {
    v.sort_by(|a, b| b.prob.partial_cmp(&a.prob).unwrap_or(std::cmp::Ordering::Equal));
}

impl Wd14Params {
    /// Split `probs` (aligned to `tags` by index) into thresholded, ranked categories.
    pub fn rank(&self, probs: &[f32], tags: &[Wd14Tag]) -> TagResult {
        let mut general = Vec::new();
        let mut character = Vec::new();
        let mut rating: Option<ScoredTag> = None;
        for (i, tag) in tags.iter().enumerate() {
            let raw = probs.get(i).copied().unwrap_or(0.0);
            let p = if self.apply_sigmoid { sigmoid(raw) } else { raw };
            let scored = || ScoredTag { name: tag.name.clone(), prob: p };
            match tag.category {
                CATEGORY_RATING => {
                    if rating.as_ref().is_none_or(|r| p > r.prob) {
                        rating = Some(scored());
                    }
                }
                CATEGORY_CHARACTER => {
                    if p >= self.character_threshold {
                        character.push(scored());
                    }
                }
                CATEGORY_GENERAL => {
                    if p >= self.general_threshold {
                        general.push(scored());
                    }
                }
                _ => {}
            }
        }
        sort_desc(&mut general);
        sort_desc(&mut character);
        TagResult { rating, general, character }
    }
}

/// Run the classifier on a prepared input buffer, returning the raw output vector.
/// The graph is the context's sole graph; its one input/output are used by name.
pub fn infer(ctx: &Context<'_>, input: &[f32]) -> Result<Vec<f32>> {
    let info = ctx.info();
    let graph = info.graphs.first().ok_or(Error::NoTensors("graph"))?;
    let in_name = graph.inputs.first().ok_or(Error::NoTensors("input"))?.name.clone();
    let out_name = graph.outputs.first().ok_or(Error::NoTensors("output"))?.name.clone();
    let graph_name = graph.name.clone();
    let mut out = ctx.execute(&graph_name, &[(in_name.as_str(), input)])?;
    out.remove(&out_name).ok_or(Error::NoTensors("output"))
}

/// Tag one RGBA image: preprocess, run the HTP graph, rank the output.
pub fn tag(
    pack: &Wd14Pack,
    session: &Session<'_>,
    system: &QnnSystem,
    rgba: &[u8],
    w: u32,
    h: u32,
    params: &Wd14Params,
) -> Result<TagResult> {
    let input = img::preprocess(rgba, w, h, img::INPUT_SIZE);
    let bytes = pack.map("model.bin")?;
    let ctx = session.load_context(system, &bytes, &ContextOpts::default())?;
    let probs = infer(&ctx, &input)?;
    Ok(params.rank(&probs, pack.tags()))
}

/// [`tag`] from encoded (png/jpeg/webp) image bytes.
pub fn tag_bytes(
    pack: &Wd14Pack,
    session: &Session<'_>,
    system: &QnnSystem,
    bytes: &[u8],
    params: &Wd14Params,
) -> Result<TagResult> {
    let (rgba, w, h) = img::decode_rgba(bytes)?;
    tag(pack, session, system, &rgba, w, h, params)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags() -> Vec<Wd14Tag> {
        vec![
            Wd14Tag { name: "general".into(), category: CATEGORY_RATING, count: 0 },
            Wd14Tag { name: "explicit".into(), category: CATEGORY_RATING, count: 0 },
            Wd14Tag { name: "1girl".into(), category: CATEGORY_GENERAL, count: 0 },
            Wd14Tag { name: "solo".into(), category: CATEGORY_GENERAL, count: 0 },
            Wd14Tag { name: "smile".into(), category: CATEGORY_GENERAL, count: 0 },
            Wd14Tag { name: "hakurei_reimu".into(), category: CATEGORY_CHARACTER, count: 0 },
        ]
    }

    #[test]
    fn rank_thresholds_and_orders() {
        let probs = [0.9, 0.2, 0.98, 0.4, 0.1, 0.95];
        let out = Wd14Params::default().rank(&probs, &tags());
        // Rating is top-1: "general" (0.9) beats "explicit" (0.2).
        assert_eq!(out.rating.unwrap().name, "general");
        // General over 0.35, descending: 1girl(0.98) then solo(0.4); smile(0.1) dropped.
        let names: Vec<&str> = out.general.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["1girl", "solo"]);
        // Character over 0.85.
        assert_eq!(out.character.len(), 1);
        assert_eq!(out.character[0].name, "hakurei_reimu");
    }

    #[test]
    fn character_threshold_gates_out_low_confidence() {
        let probs = [0.1, 0.1, 0.5, 0.1, 0.1, 0.8];
        let out = Wd14Params::default().rank(&probs, &tags());
        // 0.8 < 0.85 character threshold.
        assert!(out.character.is_empty());
        assert_eq!(out.general.len(), 1);
        assert_eq!(out.top_general(5), vec!["1girl"]);
    }

    #[test]
    fn sigmoid_flag_maps_logits_before_thresholding() {
        // Logit 2.0 -> ~0.88 (over both thresholds); logit -2.0 -> ~0.12.
        let probs = [2.0, -2.0, 2.0, -2.0, -2.0, 2.0];
        let params = Wd14Params { apply_sigmoid: true, ..Default::default() };
        let out = params.rank(&probs, &tags());
        assert_eq!(out.general.len(), 1);
        assert_eq!(out.character.len(), 1);
        assert!((out.general[0].prob - 0.8808).abs() < 1e-3);
    }

    #[test]
    fn scored_tag_formats_percent_and_spaces() {
        let t = ScoredTag { name: "long_hair".into(), prob: 0.874 };
        assert_eq!(t.percent(), 87);
        assert_eq!(t.insert_text(), "long hair");
    }
}
