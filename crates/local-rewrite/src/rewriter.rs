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
use std::path::Path;
use std::sync::Mutex;
use tokenizers::Tokenizer;

/// GGUF architecture this runner supports.
const ARCH: &str = "qwen2";

/// Fixed seed; irrelevant under greedy decoding but required by the sampler ctor.
const SEED: u64 = 42;

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

    /// Greedy-decode a rewrite for `user` under `system`, up to `max_tokens` new tokens.
    /// Stops on an EOS/chat stop token; returns the trimmed assistant text.
    pub fn rewrite(&self, system: &str, user: &str, max_tokens: usize) -> Result<String> {
        let prompt = build_prompt(system, user);
        let enc = self.tokenizer.encode(prompt, false).map_err(|e| Error::Tokenizer(e.to_string()))?;
        let tokens = enc.get_ids().to_vec();
        if tokens.is_empty() {
            return Err(Error::Msg("prompt tokenized to nothing".into()));
        }
        let mut model = self.model.lock().map_err(|_| Error::Msg("model lock poisoned".into()))?;
        model.clear_kv_cache();
        let mut sampler = LogitsProcessor::from_sampling(SEED, Sampling::ArgMax);
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
            let input = Tensor::new(&[next], &self.device)?.unsqueeze(0)?;
            let logits = model.forward(&input, index_pos)?;
            index_pos += 1;
            next = sampler.sample(&logits.squeeze(0)?)?;
        }
        let text =
            self.tokenizer.decode(&generated, true).map_err(|e| Error::Tokenizer(e.to_string()))?;
        Ok(text.trim().to_string())
    }
}

impl std::fmt::Debug for Rewriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rewriter").field("eos", &self.eos).finish()
    }
}
