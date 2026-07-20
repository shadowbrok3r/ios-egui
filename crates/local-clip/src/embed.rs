//! CLIP embedding: the thin HTP execute path plus pure vector math.
//!
//! The visual tower is one graph with one image input and one embedding output.
//! [`embed`] returns the raw embedding; the device helpers additionally
//! L2-normalize it, which is what [`cosine`] and [`aesthetic_score`] expect.

use crate::error::{Error, Result};
use crate::img;
use crate::pack::{AestheticHead, ClipPack};
use qnn_rs::{Context, ContextOpts, QnnSystem, Session};

/// Run the visual tower on a prepared NCHW input, returning the raw embedding.
/// The graph is the context's sole graph; its one input/output are used by name.
pub fn embed(ctx: &Context<'_>, input: &[f32]) -> Result<Vec<f32>> {
    let info = ctx.info();
    let graph = info.graphs.first().ok_or(Error::NoTensors("graph"))?;
    let in_name = graph.inputs.first().ok_or(Error::NoTensors("input"))?.name.clone();
    let out_name = graph.outputs.first().ok_or(Error::NoTensors("output"))?.name.clone();
    let graph_name = graph.name.clone();
    let mut out = ctx.execute(&graph_name, &[(in_name.as_str(), input)])?;
    out.remove(&out_name).ok_or(Error::NoTensors("output"))
}

/// L2-normalize in place; a zero vector is left unchanged.
pub fn l2_normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Dot product; equals cosine similarity when both inputs are L2-normalized.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    dot(a, b)
}

/// LAION aesthetic score: `dot(weights, emb) + bias` on the L2-normalized embedding.
pub fn aesthetic_score(head: &AestheticHead, emb_normalized: &[f32]) -> f32 {
    dot(&head.weights, emb_normalized) + head.bias
}

/// Embed one RGB image: preprocess, run the HTP graph, L2-normalize the embedding.
pub fn embed_image(
    pack: &ClipPack,
    session: &Session<'_>,
    system: &QnnSystem,
    rgb: &[u8],
    w: u32,
    h: u32,
) -> Result<Vec<f32>> {
    let input = img::preprocess(rgb, w, h, img::INPUT_SIZE);
    let bytes = pack.map("model.bin")?;
    let ctx = session.load_context(system, &bytes, &ContextOpts::default())?;
    let mut emb = embed(&ctx, &input)?;
    l2_normalize(&mut emb);
    Ok(emb)
}

/// [`embed_image`] from encoded (png/jpeg) image bytes.
pub fn embed_bytes(
    pack: &ClipPack,
    session: &Session<'_>,
    system: &QnnSystem,
    bytes: &[u8],
) -> Result<Vec<f32>> {
    let (rgb, w, h) = img::decode_rgb(bytes)?;
    embed_image(pack, session, system, &rgb, w, h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l2_normalize_scales_to_unit_length() {
        let mut v = [3.0f32, 4.0];
        l2_normalize(&mut v);
        assert!((v[0] - 0.6).abs() < 1e-6);
        assert!((v[1] - 0.8).abs() < 1e-6);
        // A zero vector is left unchanged (no divide by zero).
        let mut z = [0.0f32, 0.0];
        l2_normalize(&mut z);
        assert_eq!(z, [0.0, 0.0]);
    }

    #[test]
    fn cosine_of_normalized_vectors() {
        // Orthogonal unit vectors -> 0; identical unit vector -> 1.
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert!((cosine(&[0.6, 0.8], &[0.6, 0.8]) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn aesthetic_score_is_linear_head() {
        let head = AestheticHead { weights: vec![1.0, 2.0, 3.0], bias: 0.5 };
        // 1*1 + 2*1 + 3*1 + 0.5 = 6.5.
        assert!((aesthetic_score(&head, &[1.0, 1.0, 1.0]) - 6.5).abs() < 1e-6);
        // A zero embedding scores the bias alone.
        assert!((aesthetic_score(&head, &[0.0, 0.0, 0.0]) - 0.5).abs() < 1e-6);
    }
}
