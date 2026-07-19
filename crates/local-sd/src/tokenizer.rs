//! SD1.5 CLIP tokenizer over the `tokenizers` crate.
//!
//! Wraps the BPE content in `[BOS] ... [EOS]`, then pads/truncates to 77 with the
//! EOS id (CLIP's pad == eos), matching diffusers `padding="max_length"`.

use crate::error::{Error, Result};
use std::path::Path;
use tokenizers::Tokenizer;

/// `<|startoftext|>`.
pub const BOS: u32 = 49406;
/// `<|endoftext|>`, also the pad id.
pub const EOS: u32 = 49407;
/// CLIP context length.
pub const MAX_LEN: usize = 77;

/// A loaded CLIP tokenizer producing fixed 77-token id sequences.
pub struct ClipTokenizer {
    inner: Tokenizer,
}

impl ClipTokenizer {
    /// Load from a `tokenizer.json` (e.g. `openai/clip-vit-large-patch14`).
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let inner = Tokenizer::from_file(path).map_err(|e| Error::Tokenizer(e.to_string()))?;
        Ok(Self { inner })
    }

    /// Load from in-memory `tokenizer.json` bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let inner = Tokenizer::from_bytes(bytes).map_err(|e| Error::Tokenizer(e.to_string()))?;
        Ok(Self { inner })
    }

    /// Encode `text` to exactly 77 ids with BOS/EOS and EOS padding.
    pub fn encode(&self, text: &str) -> Result<Vec<u32>> {
        let enc = self.inner.encode(text, false).map_err(|e| Error::Tokenizer(e.to_string()))?;
        Ok(wrap_pad_truncate(enc.get_ids()))
    }
}

/// `[BOS] content [EOS]`, padded with EOS to 77, or truncated to 77 with EOS last.
fn wrap_pad_truncate(content: &[u32]) -> Vec<u32> {
    let mut ids = Vec::with_capacity(MAX_LEN);
    ids.push(BOS);
    ids.extend_from_slice(content);
    ids.push(EOS);
    if ids.len() > MAX_LEN {
        ids.truncate(MAX_LEN);
        ids[MAX_LEN - 1] = EOS;
    } else {
        ids.resize(MAX_LEN, EOS);
    }
    ids
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_pads_to_77_with_bos_eos() {
        let ids = wrap_pad_truncate(&[1, 2, 3]);
        assert_eq!(ids.len(), MAX_LEN);
        assert_eq!(ids[0], BOS);
        assert_eq!(&ids[1..4], &[1, 2, 3]);
        assert_eq!(ids[4], EOS);
        assert!(ids[5..].iter().all(|&x| x == EOS));
    }

    #[test]
    fn truncates_long_content_to_77_ending_in_eos() {
        let content: Vec<u32> = (0..100).collect();
        let ids = wrap_pad_truncate(&content);
        assert_eq!(ids.len(), MAX_LEN);
        assert_eq!(ids[0], BOS);
        assert_eq!(ids[MAX_LEN - 1], EOS);
    }

    // Real CLIP vocab. Run with `LOCAL_SD_CLIP_TOKENIZER=/path/to/tokenizer.json
    // cargo test -p local-sd -- --ignored`. Fetch: `hf download
    // openai/clip-vit-large-patch14 tokenizer.json`.
    #[test]
    #[ignore = "needs the real CLIP tokenizer.json via LOCAL_SD_CLIP_TOKENIZER"]
    fn known_prompt_token_ids() {
        let path = std::env::var("LOCAL_SD_CLIP_TOKENIZER").expect("set LOCAL_SD_CLIP_TOKENIZER");
        let tk = ClipTokenizer::from_file(path).unwrap();
        let ids = tk.encode("a long time ago, in a galaxy far, far away...").unwrap();
        let expected = [49406u32, 320, 1538, 788, 1468, 267, 530, 320, 6545, 2384, 267, 2384, 1520, 678, 49407];
        assert_eq!(&ids[0..expected.len()], &expected);
        assert!(ids[expected.len()..].iter().all(|&x| x == EOS));
        assert_eq!(ids.len(), MAX_LEN);
    }
}
