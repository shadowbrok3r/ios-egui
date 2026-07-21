//! The CPU LLM runner: a quantized Qwen2.5 GGUF loaded through candle, greedy-decoding
//! a rewrite from a system + user prompt. Inference is CPU; only the pack files are needed
//! at runtime. The template math in [`crate::templates`] is what the host tests exercise.

use crate::error::{Error, Result};
use crate::pack::RewritePack;
use crate::templates::build_prompt;
use candle_core::quantized::gguf_file;
use candle_core::{Device, Tensor};
use candle_transformers::generation::{LogitsProcessor, Sampling};
use candle_transformers::models::quantized_qwen2::ModelWeights;
use candle_transformers::utils::apply_repeat_penalty;
use std::path::Path;
use std::sync::Mutex;
use tokenizers::Tokenizer;

/// GGUF architecture this runner supports.
const ARCH: &str = "qwen2";

/// Fixed seed so a given prompt rewrites the same way every time.
const SEED: u64 = 42;

/// Qwen2.5-Instruct's own generation_config sampling. The 0.5B model degenerates into short
/// phrase loops under pure greedy decoding ("illustrious NoobAI illustrious NoobAI …"), so
/// ArgMax is exactly the wrong choice for it.
const TOP_K: usize = 20;
const TOP_P: f64 = 0.8;
const TEMPERATURE: f64 = 0.7;

/// Repetition penalty over the *generated* tail only — penalizing the prompt too would push
/// the model away from reusing the user's own tag words, which is the whole task.
const REPEAT_PENALTY: f32 = 1.1;
const REPEAT_LAST_N: usize = 64;

/// An opened rewriter: the quantized model behind a mutex (its KV cache mutates per run),
/// the tokenizer, and the resolved chat stop-token ids.
pub struct Rewriter {
    model: Mutex<ModelWeights>,
    tokenizer: Tokenizer,
    device: Device,
    eos: Vec<u32>,
}

impl Rewriter {
    /// Open the pack at `dir` and load its model + tokenizer onto the CPU.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        Self::from_pack(&RewritePack::open(dir)?)
    }

    /// Load an already-validated pack.
    pub fn from_pack(pack: &RewritePack) -> Result<Self> {
        let device = Device::Cpu;
        let mut file = std::fs::File::open(pack.model_gguf())?;
        let content = gguf_file::Content::read(&mut file)?;
        let arch = content
            .metadata
            .get("general.architecture")
            .and_then(|v| v.to_string().ok())
            .cloned()
            .unwrap_or_default();
        if arch != ARCH {
            return Err(Error::UnsupportedArch(arch));
        }
        let model = ModelWeights::from_gguf(content, &mut file, &device)?;
        let tokenizer =
            Tokenizer::from_file(pack.tokenizer_json()).map_err(|e| Error::Tokenizer(e.to_string()))?;
        let eos = ["<|im_end|>", "<|endoftext|>"]
            .iter()
            .filter_map(|t| tokenizer.token_to_id(t))
            .collect::<Vec<_>>();
        log::info!("local-rewrite: loaded {} pack, {} stop token(s)", ARCH, eos.len());
        Ok(Self { model: Mutex::new(model), tokenizer, device, eos })
    }

    /// True when `id` is a chat-template stop token.
    fn is_eos(&self, id: u32) -> bool {
        self.eos.contains(&id)
    }

    /// Decode a rewrite for `user` under `system`, up to `max_tokens` new tokens, with the
    /// Qwen-recommended sampler, a repetition penalty, and a tail-loop cutoff. Stops on an
    /// EOS/chat stop token; returns the trimmed assistant text.
    pub fn rewrite(&self, system: &str, user: &str, max_tokens: usize) -> Result<String> {
        let prompt = build_prompt(system, user);
        let enc = self.tokenizer.encode(prompt, false).map_err(|e| Error::Tokenizer(e.to_string()))?;
        let tokens = enc.get_ids().to_vec();
        if tokens.is_empty() {
            return Err(Error::Msg("prompt tokenized to nothing".into()));
        }
        let mut model = self.model.lock().map_err(|_| Error::Msg("model lock poisoned".into()))?;
        model.clear_kv_cache();
        let mut sampler = LogitsProcessor::from_sampling(
            SEED,
            Sampling::TopKThenTopP { k: TOP_K, p: TOP_P, temperature: TEMPERATURE },
        );
        let input = Tensor::new(tokens.as_slice(), &self.device)?.unsqueeze(0)?;
        let logits = model.forward(&input, 0)?;
        let mut next = sampler.sample(&logits.squeeze(0)?)?;
        let mut index_pos = tokens.len();
        let mut generated: Vec<u32> = Vec::new();
        for _ in 0..max_tokens {
            if self.is_eos(next) {
                break;
            }
            generated.push(next);
            if tail_is_looping(&generated) {
                log::warn!("local-rewrite: tail loop after {} tokens; cutting", generated.len());
                break;
            }
            let input = Tensor::new(&[next], &self.device)?.unsqueeze(0)?;
            let logits = model.forward(&input, index_pos)?.squeeze(0)?;
            index_pos += 1;
            let start = generated.len().saturating_sub(REPEAT_LAST_N);
            let logits = apply_repeat_penalty(&logits, REPEAT_PENALTY, &generated[start..])?;
            next = sampler.sample(&logits)?;
        }
        let text =
            self.tokenizer.decode(&generated, true).map_err(|e| Error::Tokenizer(e.to_string()))?;
        Ok(text.trim().to_string())
    }
}

/// True when the generated tail is a short cycle: the last `p` tokens repeated 3+ times in a
/// row (4+ for a single-token run). Sampling makes accidental exact n-gram triples all but
/// impossible, so this only ever fires on genuine degeneration — cutting there returns the
/// useful prefix instead of burning the rest of the token budget on "illustrious NoobAI" x20.
fn tail_is_looping(generated: &[u32]) -> bool {
    let n = generated.len();
    for p in 1..=10 {
        let reps = if p == 1 { 4 } else { 3 };
        if n < p * reps {
            continue;
        }
        let motif = &generated[n - p..];
        if (1..reps).all(|r| &generated[n - (r + 1) * p..n - r * p] == motif) {
            return true;
        }
    }
    false
}

impl std::fmt::Debug for Rewriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rewriter").field("eos", &self.eos).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::tail_is_looping;

    #[test]
    fn loop_detector_fires_on_short_cycles_only() {
        // Period 2, three repeats: 7 8 7 8 7 8.
        assert!(tail_is_looping(&[1, 2, 3, 7, 8, 7, 8, 7, 8]));
        // Period 1 needs four repeats.
        assert!(!tail_is_looping(&[1, 5, 5, 5]));
        assert!(tail_is_looping(&[1, 5, 5, 5, 5]));
        // Two repeats of a phrase is normal text, not a loop.
        assert!(!tail_is_looping(&[7, 8, 7, 8]));
        // Distinct tags with the odd shared token don't trip it.
        assert!(!tail_is_looping(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]));
        assert!(!tail_is_looping(&[]));
        // Period 5, three repeats.
        let mut v = vec![9, 9, 9];
        for _ in 0..3 {
            v.extend_from_slice(&[1, 2, 3, 4, 5]);
        }
        assert!(tail_is_looping(&v));
    }
}
