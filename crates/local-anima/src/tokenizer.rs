//! Anima's two tokenizers: Qwen2 BPE (`tokenizer.json`) and T5
//! (`tokenizer_t5.json`). Both encode without the model's special tokens; the
//! Qwen stream gets no BOS/EOS at all and the T5 stream gets its EOS appended
//! by [`crate::text::t5_finalize`].

use crate::error::{Error, Result};
use std::path::Path;
use tokenizers::Tokenizer;

/// The Qwen2 and T5 tokenizers of a model pack.
pub struct AnimaTokenizers {
    qwen: Tokenizer,
    t5: Tokenizer,
}

impl AnimaTokenizers {
    /// Load `tokenizer.json` and `tokenizer_t5.json` from a model pack dir.
    pub fn from_dir(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref();
        Self::from_files(dir.join("tokenizer.json"), dir.join("tokenizer_t5.json"))
    }

    /// Load both tokenizers from explicit paths.
    pub fn from_files(qwen: impl AsRef<Path>, t5: impl AsRef<Path>) -> Result<Self> {
        Ok(Self { qwen: load(qwen)?, t5: load(t5)? })
    }

    /// Load both tokenizers from in-memory `tokenizer.json` bytes.
    pub fn from_bytes(qwen: &[u8], t5: &[u8]) -> Result<Self> {
        Ok(Self {
            qwen: Tokenizer::from_bytes(qwen).map_err(|e| Error::Tokenizer(e.to_string()))?,
            t5: Tokenizer::from_bytes(t5).map_err(|e| Error::Tokenizer(e.to_string()))?,
        })
    }

    /// Qwen2 BPE ids for `text`, no special tokens.
    pub fn encode_qwen(&self, text: &str) -> Result<Vec<u32>> {
        encode(&self.qwen, text)
    }

    /// T5 ids for `text`, no special tokens.
    pub fn encode_t5(&self, text: &str) -> Result<Vec<u32>> {
        encode(&self.t5, text)
    }
}

fn load(path: impl AsRef<Path>) -> Result<Tokenizer> {
    Tokenizer::from_file(path).map_err(|e| Error::Tokenizer(e.to_string()))
}

fn encode(tk: &Tokenizer, text: &str) -> Result<Vec<u32>> {
    let enc = tk.encode(text, false).map_err(|e| Error::Tokenizer(e.to_string()))?;
    Ok(enc.get_ids().to_vec())
}
