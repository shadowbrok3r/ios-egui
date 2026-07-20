//! Prompt -> the four `clip.bin` inputs.
//!
//! A1111-style attention markup is parsed into weighted chunks, each chunk is
//! BPE-encoded with the Qwen2 tokenizer, and the concatenated ids index the f16
//! `token_emb.bin` table to build `input_embedding` scaled by the per-token
//! weight. The T5 branch encodes the markup-stripped text into `t5_ids`.

use crate::error::{Error, Result};
use crate::tokenizer::AnimaTokenizers;

/// Qwen context length.
pub const QWEN_SEQ: usize = 512;
/// Qwen embedding width.
pub const QWEN_HIDDEN: usize = 1024;
/// `<|endoftext|>`, used as the pad id.
pub const QWEN_PAD: u32 = 151643;
/// T5 context length.
pub const T5_SEQ: usize = 512;
/// T5 `</s>`.
pub const T5_EOS: u32 = 1;
/// Weight applied by one `(...)` level.
pub const ROUND_WEIGHT: f32 = 1.1;
/// Weight applied by one `[...]` level.
pub const SQUARE_WEIGHT: f32 = 0.9;

/// The four `clip.bin` graph inputs.
#[derive(Clone, Debug)]
pub struct ClipInputs {
    /// `[1, 512, 1024]` weighted Qwen embeddings.
    pub input_embedding: Vec<f32>,
    /// `[1, 512]` T5 ids, zero-padded.
    pub t5_ids: Vec<i32>,
    /// `[1, 512]` T5 attention mask.
    pub t5_mask: Vec<f32>,
    /// `[1, 512]` Qwen attention mask.
    pub qwen_mask: Vec<f32>,
}

/// A borrowed f16 embedding matrix of shape `[vocab, hidden]`.
#[derive(Clone, Copy)]
pub struct EmbedTable<'a> {
    data: &'a [u8],
    vocab: usize,
    hidden: usize,
}

impl<'a> EmbedTable<'a> {
    /// Wrap raw `token_emb.bin` bytes as a `[len / (2 * hidden), hidden]` f16 table.
    pub fn new(data: &'a [u8], hidden: usize) -> Result<Self> {
        let row = hidden * 2;
        if hidden == 0 || data.len() % row != 0 {
            return Err(Error::BadEmbedTable { bytes: data.len(), hidden });
        }
        Ok(Self { data, vocab: data.len() / row, hidden })
    }

    /// Number of rows.
    pub fn vocab(&self) -> usize {
        self.vocab
    }

    /// Row width.
    pub fn hidden(&self) -> usize {
        self.hidden
    }

    /// Write `row(id) * scale` into `out`; out-of-range ids write zeros.
    pub fn row_into(&self, id: u32, scale: f32, out: &mut [f32]) {
        let id = id as usize;
        if id >= self.vocab {
            out[..self.hidden].fill(0.0);
            return;
        }
        let base = id * self.hidden * 2;
        for j in 0..self.hidden {
            let b = base + j * 2;
            let bits = u16::from_le_bytes([self.data[b], self.data[b + 1]]);
            out[j] = f16_to_f32(bits) * scale;
        }
    }
}

/// IEEE 754 binary16 bit pattern to f32.
pub fn f16_to_f32(bits: u16) -> f32 {
    let sign = (bits >> 15) as u32;
    let exp = ((bits >> 10) & 0x1f) as u32;
    let mant = (bits & 0x3ff) as u32;
    let out = match exp {
        0 if mant == 0 => sign << 31,
        0 => {
            let mut e = 0i32;
            let mut m = mant;
            while m & 0x400 == 0 {
                m <<= 1;
                e += 1;
            }
            (sign << 31) | (((127 - 15 - e + 1) as u32) << 23) | ((m & 0x3ff) << 13)
        }
        0x1f => (sign << 31) | 0x7f80_0000 | (mant << 13),
        _ => (sign << 31) | ((exp + 112) << 23) | (mant << 13),
    };
    f32::from_bits(out)
}

/// Split an A1111-style prompt into `(text, weight)` chunks.
pub fn parse_weights(text: &str) -> Vec<(String, f32)> {
    let chars: Vec<char> = text.chars().collect();
    let mut res: Vec<(String, f32)> = Vec::new();
    let mut cur = String::new();
    let mut round: Vec<usize> = Vec::new();
    let mut square: Vec<usize> = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && i + 1 < chars.len() {
            cur.push(chars[i + 1]);
            i += 2;
            continue;
        }
        match c {
            '(' => {
                flush(&mut cur, &mut res);
                round.push(res.len());
            }
            '[' => {
                flush(&mut cur, &mut res);
                square.push(res.len());
            }
            ':' if !round.is_empty() => {
                let mut j = i + 1;
                let mut num = String::new();
                while j < chars.len() && chars[j] != ')' {
                    num.push(chars[j]);
                    j += 1;
                }
                match (j < chars.len()).then(|| num.trim().parse::<f32>()) {
                    Some(Ok(w)) => {
                        flush(&mut cur, &mut res);
                        let start = round.pop().unwrap();
                        scale_from(&mut res, start, w);
                        i = j + 1;
                        continue;
                    }
                    _ => cur.push(c),
                }
            }
            ')' if !round.is_empty() => {
                flush(&mut cur, &mut res);
                let start = round.pop().unwrap();
                scale_from(&mut res, start, ROUND_WEIGHT);
            }
            ']' if !square.is_empty() => {
                flush(&mut cur, &mut res);
                let start = square.pop().unwrap();
                scale_from(&mut res, start, SQUARE_WEIGHT);
            }
            _ => cur.push(c),
        }
        i += 1;
    }
    flush(&mut cur, &mut res);
    merge_adjacent(res)
}

fn flush(cur: &mut String, res: &mut Vec<(String, f32)>) {
    if !cur.is_empty() {
        res.push((std::mem::take(cur), 1.0));
    }
}

fn scale_from(res: &mut [(String, f32)], start: usize, factor: f32) {
    for e in res[start..].iter_mut() {
        e.1 *= factor;
    }
}

fn merge_adjacent(res: Vec<(String, f32)>) -> Vec<(String, f32)> {
    let mut out: Vec<(String, f32)> = Vec::with_capacity(res.len());
    for (t, w) in res {
        match out.last_mut() {
            Some(last) if (last.1 - w).abs() < 1e-6 => last.0.push_str(&t),
            _ => out.push((t, w)),
        }
    }
    out
}

/// Concatenation of the chunk texts, i.e. the prompt with markup removed.
pub fn plain_text(chunks: &[(String, f32)]) -> String {
    chunks.iter().map(|(t, _)| t.as_str()).collect()
}

/// Encode each chunk and concatenate, carrying each chunk's weight onto its
/// ids. Truncated to `QWEN_SEQ`; no BOS/EOS.
pub fn weighted_ids<F>(chunks: &[(String, f32)], mut encode: F) -> Result<(Vec<u32>, Vec<f32>)>
where
    F: FnMut(&str) -> Result<Vec<u32>>,
{
    let mut ids = Vec::new();
    let mut weights = Vec::new();
    for (text, w) in chunks {
        if text.is_empty() {
            continue;
        }
        let chunk = encode(text)?;
        weights.extend(std::iter::repeat_n(*w, chunk.len()));
        ids.extend(chunk);
        if ids.len() >= QWEN_SEQ {
            break;
        }
    }
    ids.truncate(QWEN_SEQ);
    weights.truncate(QWEN_SEQ);
    Ok((ids, weights))
}

/// Build `input_embedding` `[512 * 1024]` and `qwen_mask` `[512]`.
pub fn qwen_embedding(table: &EmbedTable<'_>, ids: &[u32], weights: &[f32]) -> (Vec<f32>, Vec<f32>) {
    let hidden = table.hidden();
    let len = ids.len().min(QWEN_SEQ);
    let mut emb = vec![0f32; QWEN_SEQ * hidden];
    let mut mask = vec![0f32; QWEN_SEQ];
    for pos in 0..QWEN_SEQ {
        let (id, w) = if pos < len {
            mask[pos] = 1.0;
            (ids[pos], weights.get(pos).copied().unwrap_or(1.0))
        } else {
            (QWEN_PAD, 1.0)
        };
        table.row_into(id, w, &mut emb[pos * hidden..(pos + 1) * hidden]);
    }
    (emb, mask)
}

/// Append `T5_EOS` when missing, then truncate to `T5_SEQ`.
pub fn t5_finalize(ids: &[u32]) -> Vec<u32> {
    let mut v = ids.to_vec();
    if v.last() != Some(&T5_EOS) {
        v.push(T5_EOS);
    }
    v.truncate(T5_SEQ);
    v
}

/// Zero-filled `t5_ids` `[512]` with `ids` at the front, plus its mask.
pub fn t5_inputs(ids: &[u32]) -> (Vec<i32>, Vec<f32>) {
    let len = ids.len().min(T5_SEQ);
    let mut out = vec![0i32; T5_SEQ];
    let mut mask = vec![0f32; T5_SEQ];
    for pos in 0..len {
        out[pos] = ids[pos] as i32;
        mask[pos] = 1.0;
    }
    (out, mask)
}

/// Build every `clip.bin` input from a prompt.
pub fn build_clip_inputs(tok: &AnimaTokenizers, table: &EmbedTable<'_>, prompt: &str) -> Result<ClipInputs> {
    let chunks = parse_weights(prompt);
    let (ids, weights) = weighted_ids(&chunks, |t| tok.encode_qwen(t))?;
    let (input_embedding, qwen_mask) = qwen_embedding(table, &ids, &weights);
    let t5 = t5_finalize(&tok.encode_t5(&plain_text(&chunks))?);
    let (t5_ids, t5_mask) = t5_inputs(&t5);
    Ok(ClipInputs { input_embedding, t5_ids, t5_mask, qwen_mask })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(text: &str) -> Vec<(String, f32)> {
        parse_weights(text)
    }

    #[test]
    fn plain_prompt_is_one_unweighted_chunk() {
        assert_eq!(w("a cat"), vec![("a cat".to_string(), 1.0)]);
    }

    #[test]
    fn round_brackets_boost() {
        let c = w("a (red) cat");
        assert_eq!(c.len(), 3);
        assert_eq!(c[0], ("a ".to_string(), 1.0));
        assert_eq!(c[1].0, "red");
        assert!((c[1].1 - 1.1).abs() < 1e-6);
        assert_eq!(c[2], (" cat".to_string(), 1.0));
    }

    #[test]
    fn square_brackets_attenuate() {
        let c = w("[blur]");
        assert_eq!(c[0].0, "blur");
        assert!((c[0].1 - 0.9).abs() < 1e-6);
    }

    #[test]
    fn explicit_weight_syntax() {
        let c = w("(sharp:1.3), soft");
        assert_eq!(c[0].0, "sharp");
        assert!((c[0].1 - 1.3).abs() < 1e-6);
        assert_eq!(c[1], (", soft".to_string(), 1.0));
    }

    #[test]
    fn nesting_multiplies() {
        let c = w("((x))");
        assert!((c[0].1 - 1.21).abs() < 1e-5, "got {}", c[0].1);
        let c = w("([y])");
        assert!((c[0].1 - 0.99).abs() < 1e-5, "got {}", c[0].1);
        let c = w("((z:2.0))");
        assert!((c[0].1 - 2.2).abs() < 1e-5, "got {}", c[0].1);
    }

    #[test]
    fn backslash_escapes_brackets() {
        assert_eq!(w("\\(x\\)"), vec![("(x)".to_string(), 1.0)]);
        assert_eq!(w("\\[y\\]"), vec![("[y]".to_string(), 1.0)]);
    }

    #[test]
    fn adjacent_equal_weights_merge() {
        assert_eq!(w("(a)(b)"), vec![("ab".to_string(), ROUND_WEIGHT)]);
    }

    #[test]
    fn unmatched_brackets_do_not_panic() {
        assert_eq!(plain_text(&w("(unclosed")), "unclosed");
        assert_eq!(plain_text(&w("stray) text")), "stray) text");
        assert_eq!(plain_text(&w("")), "");
    }

    #[test]
    fn colon_outside_brackets_is_literal() {
        assert_eq!(w("time: 12:30"), vec![("time: 12:30".to_string(), 1.0)]);
    }

    #[test]
    fn plain_text_strips_markup() {
        assert_eq!(plain_text(&w("a (red:1.5) [dull] cat")), "a red dull cat");
    }

    #[test]
    fn f16_known_bit_patterns() {
        let cases: [(u16, f32); 8] = [
            (0x0000, 0.0),
            (0x8000, -0.0),
            (0x3c00, 1.0),
            (0xbc00, -1.0),
            (0x4000, 2.0),
            (0xc500, -5.0),
            (0x3555, 0.33325195),
            (0x7bff, 65504.0),
        ];
        for (bits, want) in cases {
            let got = f16_to_f32(bits);
            assert!((got - want).abs() <= 1e-6 * want.abs().max(1.0), "0x{bits:04x} -> {got} want {want}");
        }
    }

    #[test]
    fn f16_subnormals_and_specials() {
        assert!((f16_to_f32(0x0001) - 5.9604645e-8).abs() < 1e-13);
        assert!((f16_to_f32(0x0200) - 3.0517578e-5).abs() < 1e-10);
        assert!((f16_to_f32(0x03ff) - 6.0975552e-5).abs() < 1e-10);
        assert!(f16_to_f32(0x7c00).is_infinite() && f16_to_f32(0x7c00) > 0.0);
        assert!(f16_to_f32(0xfc00).is_infinite() && f16_to_f32(0xfc00) < 0.0);
        assert!(f16_to_f32(0x7e00).is_nan());
        assert_eq!(f16_to_f32(0x8000).to_bits(), (-0.0f32).to_bits());
    }

    /// Row `i` is filled with `(i % 251) / 4`, exactly representable in f16.
    fn row_value(i: usize) -> f32 {
        (i % 251) as f32 / 4.0
    }

    fn table_bytes(vocab: usize, hidden: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(vocab * hidden * 2);
        for i in 0..vocab {
            let bits = f32_to_f16(row_value(i));
            for _ in 0..hidden {
                v.extend_from_slice(&bits.to_le_bytes());
            }
        }
        v
    }

    fn f32_to_f16(x: f32) -> u16 {
        let b = x.to_bits();
        let sign = ((b >> 16) & 0x8000) as u16;
        let exp = ((b >> 23) & 0xff) as i32 - 127 + 15;
        let mant = ((b >> 13) & 0x3ff) as u16;
        if x == 0.0 { sign } else { sign | ((exp as u16) << 10) | mant }
    }

    #[test]
    fn embed_table_rejects_ragged_data() {
        assert!(EmbedTable::new(&[0u8; 7], 4).is_err());
        assert!(EmbedTable::new(&[0u8; 16], 4).is_ok());
    }

    #[test]
    fn embed_table_reports_vocab_and_hidden() {
        let data = table_bytes(6, 4);
        let t = EmbedTable::new(&data, 4).unwrap();
        assert_eq!((t.vocab(), t.hidden()), (6, 4));
    }

    #[test]
    fn embedding_pads_masks_and_weights() {
        let data = table_bytes(QWEN_PAD as usize + 1, 2);
        let t = EmbedTable::new(&data, 2).unwrap();
        let ids = [3u32, 7, 11];
        let weights = [1.0f32, 2.0, 0.5];
        let (emb, mask) = qwen_embedding(&t, &ids, &weights);
        assert_eq!(emb.len(), QWEN_SEQ * 2);
        assert_eq!(mask.len(), QWEN_SEQ);
        for (pos, (&id, &wt)) in ids.iter().zip(&weights).enumerate() {
            let want = row_value(id as usize) * wt;
            assert_eq!(&emb[pos * 2..pos * 2 + 2], &[want, want], "pos={pos}");
        }
        // Every position past the prompt holds the pad row, unweighted.
        let pad = row_value(QWEN_PAD as usize);
        assert_eq!(&emb[6..8], &[pad, pad]);
        assert_eq!(&emb[(QWEN_SEQ - 1) * 2..], &[pad, pad]);
        assert_eq!(&mask[0..3], &[1.0, 1.0, 1.0]);
        assert!(mask[3..].iter().all(|&m| m == 0.0));
    }

    #[test]
    fn embedding_zero_fills_out_of_range_ids() {
        let data = table_bytes(16, 2);
        let t = EmbedTable::new(&data, 2).unwrap();
        let (emb, mask) = qwen_embedding(&t, &[99u32], &[1.0]);
        assert_eq!(&emb[0..2], &[0.0, 0.0]);
        assert_eq!(mask[0], 1.0);
    }

    #[test]
    fn weighted_ids_concatenates_and_truncates() {
        let chunks = vec![("aa".to_string(), 1.1), ("bbb".to_string(), 0.9)];
        let (ids, weights) = weighted_ids(&chunks, |t| Ok((0..t.len() as u32).collect())).unwrap();
        assert_eq!(ids, vec![0, 1, 0, 1, 2]);
        assert_eq!(weights, vec![1.1, 1.1, 0.9, 0.9, 0.9]);

        let long = vec![("x".repeat(1000), 1.0)];
        let (ids, weights) = weighted_ids(&long, |t| Ok((0..t.len() as u32).collect())).unwrap();
        assert_eq!(ids.len(), QWEN_SEQ);
        assert_eq!(weights.len(), QWEN_SEQ);
    }

    #[test]
    fn t5_appends_eos_once() {
        assert_eq!(t5_finalize(&[5, 6]), vec![5, 6, T5_EOS]);
        assert_eq!(t5_finalize(&[5, T5_EOS]), vec![5, T5_EOS]);
        assert_eq!(t5_finalize(&[]), vec![T5_EOS]);
        assert_eq!(t5_finalize(&vec![9u32; 600]).len(), T5_SEQ);
    }

    #[test]
    fn t5_inputs_zero_fill_and_mask() {
        let (ids, mask) = t5_inputs(&[4, 9, T5_EOS]);
        assert_eq!(ids.len(), T5_SEQ);
        assert_eq!(&ids[0..3], &[4, 9, 1]);
        assert!(ids[3..].iter().all(|&x| x == 0));
        assert_eq!(&mask[0..3], &[1.0, 1.0, 1.0]);
        assert!(mask[3..].iter().all(|&m| m == 0.0));
    }
}
