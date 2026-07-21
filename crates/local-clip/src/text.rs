//! CLIP text tower: tokenize a query to fixed [1,77] `input_ids`/`attention_mask`, run the HTP
//! text graph (one or two I32 inputs), and L2-normalize the 512-d `text_embeds` for cosine ranking.
//!
//! The graph pools at the first EOS via an internal argmax over `input_ids`, so padding must not
//! introduce another max token before the real EOS. HF CLIP pads `input_ids` with the EOS id
//! 49407 (attention_mask 0); we match that. BOS 49406 leads, EOS 49407 is kept last on truncation.

use crate::embed::l2_normalize;
use crate::error::{Error, Result};
use crate::pack::{ClipPack, TEXT_MODEL_FILE};
use qnn_rs::{Context, ContextOpts, QnnSystem, Session, TensorIn};
use tokenizers::Tokenizer;

/// CLIP start-of-text token id.
pub const BOS: i32 = 49406;

/// CLIP end-of-text token id; also the pad id (HF CLIP pads `input_ids` with EOS).
pub const EOS: i32 = 49407;

/// Fixed CLIP text context length.
pub const CONTEXT_LEN: usize = 77;

/// Wrap content token ids into fixed [77] `input_ids` + `attention_mask`.
/// Layout `[BOS, content..., EOS]` truncated to keep EOS last, padded with EOS id (mask 0).
pub fn build_inputs(content: &[i32]) -> (Vec<i32>, Vec<i32>) {
    let mut ids = Vec::with_capacity(CONTEXT_LEN);
    ids.push(BOS);
    let room = CONTEXT_LEN - 2; // BOS + content + EOS
    let take = content.len().min(room);
    ids.extend_from_slice(&content[..take]);
    ids.push(EOS);
    let real = ids.len();
    ids.resize(CONTEXT_LEN, EOS);
    let mask: Vec<i32> = (0..CONTEXT_LEN).map(|i| (i < real) as i32).collect();
    (ids, mask)
}

/// Encode `query` (no special tokens) and wrap it into the fixed [77] tensors.
pub fn encode_query(tokenizer: &Tokenizer, query: &str) -> Result<(Vec<i32>, Vec<i32>)> {
    let enc = tokenizer.encode(query, false).map_err(|e| Error::Tokenizer(e.to_string()))?;
    let content: Vec<i32> = enc.get_ids().iter().map(|&id| id as i32).collect();
    Ok(build_inputs(&content))
}

/// Run the text graph on `input_ids` (and `attention_mask` when present), returning raw `text_embeds`.
/// HTP conversion often folds `attention_mask` away; a single `input_ids*` input is enough then.
fn run_text(ctx: &Context<'_>, ids: &[i32], mask: &[i32]) -> Result<Vec<f32>> {
    let info = ctx.info();
    let graph = info.graphs.first().ok_or(Error::NoTensors("graph"))?;
    if graph.inputs.is_empty() {
        return Err(Error::NoTensors("text input"));
    }
    let out_name = graph.outputs.first().ok_or(Error::NoTensors("output"))?.name.clone();
    let graph_name = graph.name.clone();
    let mut feeds: Vec<(&str, TensorIn<'_>)> = Vec::with_capacity(2);
    if graph.inputs.len() == 1 {
        feeds.push((graph.inputs[0].name.as_str(), TensorIn::I32(ids)));
    } else {
        let mask_name =
            graph.inputs.iter().find(|t| t.name.to_lowercase().contains("mask")).map(|t| t.name.as_str());
        let (ids_name, mask_name) = match mask_name {
            Some(m) => {
                let i = graph.inputs.iter().find(|t| t.name != m).ok_or(Error::NoTensors("input_ids"))?;
                (i.name.as_str(), m)
            }
            None => (graph.inputs[0].name.as_str(), graph.inputs[1].name.as_str()),
        };
        feeds.push((ids_name, TensorIn::I32(ids)));
        feeds.push((mask_name, TensorIn::I32(mask)));
    }
    let mut out = ctx.execute_mixed(&graph_name, &feeds)?;
    out.remove(&out_name).ok_or(Error::NoTensors("output"))
}

/// Embed `query` with the pack's text tower: tokenize, run the HTP graph, L2-normalize the 512-d
/// embedding so [`crate::cosine`] against indexed image embeddings is a plain dot product.
pub fn embed_text(pack: &ClipPack, session: &Session<'_>, system: &QnnSystem, query: &str) -> Result<Vec<f32>> {
    let tokenizer =
        Tokenizer::from_file(pack.tokenizer_json()).map_err(|e| Error::Tokenizer(e.to_string()))?;
    let (ids, mask) = encode_query(&tokenizer, query)?;
    let bytes = pack.map(TEXT_MODEL_FILE)?;
    let ctx = session.load_context(system, &bytes, &ContextOpts::default())?;
    let mut emb = run_text(&ctx, &ids, &mask)?;
    l2_normalize(&mut emb);
    Ok(emb)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Tokenizer {
        let p = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixture_tokenizer.json");
        Tokenizer::from_file(p).expect("load fixture tokenizer")
    }

    #[test]
    fn short_query_pads_to_context_len_with_bos_eos() {
        // One content token -> [BOS, t, EOS, EOS-pad...]; mask 1 for the first three only.
        let (ids, mask) = build_inputs(&[5]);
        assert_eq!(ids.len(), CONTEXT_LEN);
        assert_eq!(mask.len(), CONTEXT_LEN);
        assert_eq!(ids[0], BOS);
        assert_eq!(ids[1], 5);
        assert_eq!(ids[2], EOS);
        assert!(ids[3..].iter().all(|&x| x == EOS));
        assert_eq!(&mask[..3], &[1, 1, 1]);
        assert!(mask[3..].iter().all(|&m| m == 0));
    }

    #[test]
    fn empty_query_is_bos_eos_only() {
        let (ids, mask) = build_inputs(&[]);
        assert_eq!(ids[0], BOS);
        assert_eq!(ids[1], EOS);
        assert_eq!(&mask[..2], &[1, 1]);
        assert!(mask[2..].iter().all(|&m| m == 0));
    }

    #[test]
    fn long_query_truncates_keeping_eos_at_index_76() {
        // 100 content tokens overflow; keep BOS + first 75 + EOS, all attended.
        let content: Vec<i32> = (0..100).map(|i| i % 7 + 1).collect();
        let (ids, mask) = build_inputs(&content);
        assert_eq!(ids.len(), CONTEXT_LEN);
        assert_eq!(ids[0], BOS);
        assert_eq!(ids[CONTEXT_LEN - 1], EOS);
        assert_eq!(ids[1], content[0]);
        assert_eq!(ids[CONTEXT_LEN - 2], content[CONTEXT_LEN - 3]);
        assert!(mask.iter().all(|&m| m == 1));
    }

    #[test]
    fn encode_wraps_tokenizer_output_in_bos_eos() {
        // Fixture-relative: assert placement/mask, not the fixture's content ids.
        let tok = fixture();
        let (ids, mask) = encode_query(&tok, "hello world").unwrap();
        assert_eq!(ids.len(), CONTEXT_LEN);
        assert_eq!(ids[0], BOS);
        let real = mask.iter().filter(|&&m| m == 1).count();
        assert!(real >= 3, "BOS + >=1 content + EOS, got {real}");
        assert_eq!(ids[real - 1], EOS);
        assert!(ids[real..].iter().all(|&x| x == EOS));
        assert!(mask[real..].iter().all(|&m| m == 0));
    }

    #[test]
    fn encode_truncates_long_input_to_eos_last() {
        // A whitespace query of >75 tokens forces truncation through encode_query.
        let tok = fixture();
        let query = "a ".repeat(120);
        let (ids, mask) = encode_query(&tok, &query).unwrap();
        assert_eq!(ids.len(), CONTEXT_LEN);
        assert_eq!(ids[0], BOS);
        assert_eq!(ids[CONTEXT_LEN - 1], EOS);
        assert!(mask.iter().all(|&m| m == 1));
    }
}
