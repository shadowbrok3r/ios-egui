//! CPU-only checks against a real model pack. Run with
//! `LOCAL_ANIMA_PACK=/path/to/anima cargo test -p local-anima -- --ignored`.

use local_anima::{build_clip_inputs, AnimaPack, QWEN_HIDDEN, QWEN_SEQ, T5_EOS, T5_SEQ};

fn pack() -> AnimaPack {
    let dir = std::env::var("LOCAL_ANIMA_PACK").expect("set LOCAL_ANIMA_PACK");
    AnimaPack::open(dir).expect("open pack")
}

#[test]
#[ignore = "needs a real Anima model pack via LOCAL_ANIMA_PACK"]
fn embedding_table_is_qwen_shaped() {
    let p = pack();
    let t = p.token_emb();
    assert_eq!(t.hidden(), QWEN_HIDDEN);
    assert_eq!(t.vocab(), 151936);
}

#[test]
#[ignore = "needs a real Anima model pack via LOCAL_ANIMA_PACK"]
fn clip_inputs_have_the_graph_shapes() {
    let p = pack();
    let tok = p.tokenizers().unwrap();
    let table = p.token_emb();
    let inputs = build_clip_inputs(&tok, &table, "masterpiece, best quality, (1girl:1.3), abstract background").unwrap();

    assert_eq!(inputs.input_embedding.len(), QWEN_SEQ * QWEN_HIDDEN);
    assert_eq!(inputs.t5_ids.len(), T5_SEQ);
    assert_eq!(inputs.t5_mask.len(), T5_SEQ);
    assert_eq!(inputs.qwen_mask.len(), QWEN_SEQ);
    assert!(inputs.input_embedding.iter().all(|v| v.is_finite()));

    let qwen_len = inputs.qwen_mask.iter().filter(|&&m| m == 1.0).count();
    let t5_len = inputs.t5_mask.iter().filter(|&&m| m == 1.0).count();
    assert!(qwen_len > 4 && qwen_len < QWEN_SEQ, "qwen_len={qwen_len}");
    assert!(t5_len > 4 && t5_len < T5_SEQ, "t5_len={t5_len}");
    assert_eq!(inputs.t5_ids[t5_len - 1], T5_EOS as i32);
    assert_eq!(inputs.t5_ids[t5_len], 0);

    // Weighted tokens must not all collapse to the same row scale.
    let head: f32 = inputs.input_embedding[..QWEN_HIDDEN].iter().map(|v| v.abs()).sum();
    assert!(head > 0.0, "first token embedding is all zero");
}

#[test]
#[ignore = "needs a real Anima model pack via LOCAL_ANIMA_PACK"]
fn empty_prompt_is_all_padding() {
    let p = pack();
    let tok = p.tokenizers().unwrap();
    let inputs = build_clip_inputs(&tok, &p.token_emb(), "").unwrap();
    assert!(inputs.qwen_mask.iter().all(|&m| m == 0.0));
    assert_eq!(inputs.t5_ids[0], T5_EOS as i32);
    assert_eq!(inputs.t5_mask[0], 1.0);
    assert!(inputs.t5_mask[1..].iter().all(|&m| m == 0.0));
}
