//! CLIP ViT-L/14 text encoder on CPU via `candle-transformers`.
//!
//! Produces the SD1.5 `[1, 77, 768]` last-hidden-state used as the UNet
//! `text_embedding`. Weights are the CLIP text model in safetensors with
//! `text_model.*` tensor names (e.g. `openai/clip-vit-large-patch14`).

use crate::error::Result;
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::stable_diffusion::clip::{ClipTextTransformer, Config};
use std::path::Path;

/// CLIP context length.
pub const SEQ: usize = 77;
/// CLIP ViT-L/14 hidden size.
pub const HIDDEN: usize = 768;

/// A CPU CLIP text encoder.
pub struct ClipTextEncoder {
    model: ClipTextTransformer,
    device: Device,
}

impl ClipTextEncoder {
    /// Load the text encoder from a safetensors file (`text_model.*` weights).
    pub fn from_safetensors(path: impl AsRef<Path>) -> Result<Self> {
        let device = Device::Cpu;
        let paths = [path.as_ref().to_path_buf()];
        let vb = unsafe { VarBuilder::from_mmaped_safetensors(&paths, DType::F32, &device)? };
        let model = ClipTextTransformer::new(vb, &Config::v1_5())?;
        Ok(Self { model, device })
    }

    /// Encode 77 token ids to a row-major `[1, 77, 768]` embedding (`77 * 768` f32).
    pub fn encode_tokens(&self, token_ids: &[u32]) -> Result<Vec<f32>> {
        let len = token_ids.len();
        let tokens = Tensor::from_vec(token_ids.to_vec(), (1, len), &self.device)?;
        let hidden = self.model.forward_with_mask(&tokens, usize::MAX)?;
        let flat = hidden.flatten_all()?.to_vec1::<f32>()?;
        Ok(flat)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_weights_errors_without_panicking() {
        assert!(ClipTextEncoder::from_safetensors("/nonexistent/clip.safetensors").is_err());
    }

    // Real CLIP text weights. Run with `LOCAL_SD_CLIP_WEIGHTS=/path/to/model.safetensors
    // cargo test -p local-sd -- --ignored`. Fetch: `hf download
    // openai/clip-vit-large-patch14 model.safetensors`.
    #[test]
    #[ignore = "needs real CLIP weights via LOCAL_SD_CLIP_WEIGHTS"]
    fn encodes_to_77x768() {
        let path = std::env::var("LOCAL_SD_CLIP_WEIGHTS").expect("set LOCAL_SD_CLIP_WEIGHTS");
        let enc = ClipTextEncoder::from_safetensors(path).unwrap();
        let ids = vec![super::super::tokenizer::BOS]
            .into_iter()
            .chain(std::iter::repeat(super::super::tokenizer::EOS).take(SEQ - 1))
            .collect::<Vec<_>>();
        let out = enc.encode_tokens(&ids).unwrap();
        assert_eq!(out.len(), SEQ * HIDDEN);
        assert!(out.iter().all(|x| x.is_finite()));
    }
}
