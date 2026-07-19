//! text2img orchestration: tokenizer -> CLIP (CPU) -> [per step: scale latent,
//! UNet on NPU via `qnn_rs`, scheduler step] -> VAE decode (NPU) -> RGB.
//!
//! UNet/VAE tensors are bound by element count, not name. Device execution runs
//! through `qnn_rs::Context::execute` (f32 in, f32 out; it quantizes internally).

use crate::clip::ClipTextEncoder;
use crate::error::{Error, Result};
use crate::img::{self, Image};
use crate::scheduler::{Sampler, Scheduler};
use crate::tokenizer::ClipTokenizer;
use qnn_rs::Context;
use rand::rngs::StdRng;
use rand::SeedableRng;
use rand_distr::{Distribution, StandardNormal};
use std::sync::atomic::{AtomicBool, Ordering};

/// SD1.5 VAE latent scaling factor.
pub const VAE_SCALE: f32 = 0.18215;

/// text2img parameters.
#[derive(Clone, Debug)]
pub struct Text2ImgParams {
    pub steps: usize,
    pub guidance_scale: f32,
    pub seed: u64,
    pub sampler: Sampler,
    pub latent_channels: usize,
    pub latent_h: usize,
    pub latent_w: usize,
    /// Emit a cheap latent preview every N steps (None = never).
    pub preview_every: Option<usize>,
}

impl Default for Text2ImgParams {
    fn default() -> Self {
        Self {
            steps: 20,
            guidance_scale: 7.5,
            seed: 0,
            sampler: Sampler::EulerAncestral,
            latent_channels: 4,
            latent_h: 64,
            latent_w: 64,
            preview_every: None,
        }
    }
}

impl Text2ImgParams {
    fn latent_elems(&self) -> usize {
        self.latent_channels * self.latent_h * self.latent_w
    }
    fn image_wh(&self) -> (u32, u32) {
        ((self.latent_w * 8) as u32, (self.latent_h * 8) as u32)
    }
}

/// Classifier-free guidance mix: `uncond + scale * (cond - uncond)`.
fn apply_cfg(uncond: &[f32], cond: &[f32], scale: f32) -> Vec<f32> {
    uncond.iter().zip(cond).map(|(&u, &c)| u + scale * (c - u)).collect()
}

fn randn(n: usize, rng: &mut StdRng) -> Vec<f32> {
    (0..n).map(|_| StandardNormal.sample(rng)).collect()
}

struct UnetIo {
    graph: String,
    sample: String,
    timestep: String,
    emb: String,
    output: String,
}

fn resolve_unet_io(ctx: &Context<'_>, sample_elems: u64, emb_elems: u64) -> Result<UnetIo> {
    for g in &ctx.info().graphs {
        let Some(sample) = g.inputs.iter().find(|t| t.elem_count() == sample_elems) else { continue };
        let timestep = g
            .inputs
            .iter()
            .find(|t| t.elem_count() == 1)
            .ok_or(Error::IoNotFound { bin: "unet", role: "timestep", elems: 1 })?;
        let emb = g
            .inputs
            .iter()
            .find(|t| t.elem_count() == emb_elems)
            .ok_or(Error::IoNotFound { bin: "unet", role: "text_embedding", elems: emb_elems })?;
        let output = g
            .outputs
            .iter()
            .find(|t| t.elem_count() == sample_elems)
            .or_else(|| g.outputs.first())
            .ok_or(Error::IoNotFound { bin: "unet", role: "output", elems: sample_elems })?;
        return Ok(UnetIo {
            graph: g.name.clone(),
            sample: sample.name.clone(),
            timestep: timestep.name.clone(),
            emb: emb.name.clone(),
            output: output.name.clone(),
        });
    }
    Err(Error::IoNotFound { bin: "unet", role: "sample", elems: sample_elems })
}

struct VaeIo {
    graph: String,
    input: String,
    output: String,
}

fn resolve_vae_io(ctx: &Context<'_>, in_elems: u64, out_elems: u64) -> Result<VaeIo> {
    for g in &ctx.info().graphs {
        let Some(input) = g.inputs.iter().find(|t| t.elem_count() == in_elems) else { continue };
        let output = g
            .outputs
            .iter()
            .find(|t| t.elem_count() == out_elems)
            .or_else(|| g.outputs.first())
            .ok_or(Error::IoNotFound { bin: "vae_decoder", role: "output", elems: out_elems })?;
        return Ok(VaeIo { graph: g.name.clone(), input: input.name.clone(), output: output.name.clone() });
    }
    Err(Error::IoNotFound { bin: "vae_decoder", role: "input", elems: in_elems })
}

/// Run one UNet pass, returning its epsilon output as f32.
fn unet_eps(unet: &Context<'_>, io: &UnetIo, model_in: &[f32], timestep: f32, emb: &[f32]) -> Result<Vec<f32>> {
    let ts = [timestep];
    let inputs: [(&str, &[f32]); 3] = [
        (io.sample.as_str(), model_in),
        (io.timestep.as_str(), &ts),
        (io.emb.as_str(), emb),
    ];
    let mut out = unet.execute(&io.graph, &inputs)?;
    out.remove(&io.output).ok_or_else(|| Error::Msg(format!("UNet produced no '{}' output", io.output)))
}

/// Generate an image. `progress(step_done, total, preview)` is called each step;
/// `preview` is a cheap latent RGB when `params.preview_every` fires. The final
/// decoded image is the return value. Device paths run on the HTP via `qnn_rs`.
/// When `cancel` is set, returns [`Error::Msg`] `"cancelled"` between steps.
pub fn text2img<F>(
    tokenizer: &ClipTokenizer,
    clip: &ClipTextEncoder,
    unet: &Context<'_>,
    vae_decoder: &Context<'_>,
    prompt: &str,
    negative: &str,
    params: &Text2ImgParams,
    mut progress: F,
    cancel: Option<&AtomicBool>,
) -> Result<Image>
where
    F: FnMut(usize, usize, Option<&Image>),
{
    let latent_elems = params.latent_elems();
    let emb_elems = (crate::clip::SEQ * crate::clip::HIDDEN) as u64;
    let (img_w, img_h) = params.image_wh();
    let out_elems = (3 * img_w * img_h) as u64;

    let unet_io = resolve_unet_io(unet, latent_elems as u64, emb_elems)?;
    let vae_io = resolve_vae_io(vae_decoder, latent_elems as u64, out_elems)?;

    let cond = clip.encode_tokens(&tokenizer.encode(prompt)?)?;
    let do_cfg = (params.guidance_scale - 1.0).abs() > 1e-3;
    let uncond = if do_cfg {
        Some(clip.encode_tokens(&tokenizer.encode(negative)?)?)
    } else {
        None
    };

    let mut scheduler = Scheduler::new(params.sampler, params.steps);
    let mut rng = StdRng::seed_from_u64(params.seed);
    let mut latent: Vec<f32> = randn(latent_elems, &mut rng).iter().map(|x| x * scheduler.init_noise_sigma()).collect();

    let total = scheduler.len();
    for step in 0..total {
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return Err(Error::Msg("cancelled".into()));
        }
        let sigma = scheduler.sigmas()[step];
        let model_in = scheduler.scale_model_input(&latent, step);
        let t = scheduler.timesteps()[step];

        let eps_cond = unet_eps(unet, &unet_io, &model_in, t, &cond)?;
        let eps = match &uncond {
            Some(u) => {
                let eps_uncond = unet_eps(unet, &unet_io, &model_in, t, u)?;
                apply_cfg(&eps_uncond, &eps_cond, params.guidance_scale)
            }
            None => eps_cond,
        };

        let preview = params.preview_every.and_then(|every| {
            if every > 0 && step % every == 0 {
                let pred_x0: Vec<f32> = latent.iter().zip(&eps).map(|(&x, &e)| x - sigma * e).collect();
                Some(img::latent_preview(&pred_x0, params.latent_w, params.latent_h))
            } else {
                None
            }
        });

        let noise = randn(latent_elems, &mut rng);
        scheduler.step(&eps, step, &mut latent, &noise);
        progress(step + 1, total, preview.as_ref());
    }

    let vae_in: Vec<f32> = latent.iter().map(|x| x / VAE_SCALE).collect();
    let decoded = vae_decoder
        .execute(&vae_io.graph, &[(vae_io.input.as_str(), vae_in.as_slice())])?
        .remove(&vae_io.output)
        .ok_or_else(|| Error::Msg(format!("VAE produced no '{}' output", vae_io.output)))?;
    Ok(img::vae_output_to_image(&decoded, img_w, img_h))
}

#[cfg(test)]
mod tests {
    use super::*;
    use qnn_rs::{DataType, GraphInfo, ScaleOffset, TensorInfo};

    fn t(name: &str, dims: Vec<u32>, dtype: DataType) -> TensorInfo {
        let quant = matches!(dtype, DataType::UFixedPoint16 | DataType::UFixedPoint8)
            .then_some(ScaleOffset { scale: 1.0, offset: 0 });
        TensorInfo { name: name.into(), id: 0, dims, dtype, quant }
    }

    #[test]
    fn cfg_mixes_cond_and_uncond() {
        let out = apply_cfg(&[0.0, 2.0], &[1.0, 4.0], 2.0);
        assert_eq!(out, vec![2.0, 6.0]);
    }

    #[test]
    fn cfg_scale_one_returns_cond() {
        assert_eq!(apply_cfg(&[3.0], &[5.0], 1.0), vec![5.0]);
    }

    #[test]
    fn randn_is_seed_reproducible() {
        let a = randn(8, &mut StdRng::seed_from_u64(42));
        let b = randn(8, &mut StdRng::seed_from_u64(42));
        let c = randn(8, &mut StdRng::seed_from_u64(7));
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn unet_io_bound_by_element_count() {
        let g = GraphInfo {
            name: "model".into(),
            inputs: vec![
                t("sample", vec![1, 4, 64, 64], DataType::UFixedPoint16),
                t("timestamp", vec![1], DataType::Int32),
                t("text_embedding", vec![1, 77, 768], DataType::UFixedPoint16),
            ],
            outputs: vec![t("output", vec![1, 4, 64, 64], DataType::UFixedPoint16)],
        };
        // Element-count roles must resolve regardless of name.
        let sample = g.inputs.iter().find(|x| x.elem_count() == 4 * 64 * 64).unwrap();
        let emb = g.inputs.iter().find(|x| x.elem_count() == 77 * 768).unwrap();
        let ts = g.inputs.iter().find(|x| x.elem_count() == 1).unwrap();
        assert_eq!(sample.name, "sample");
        assert_eq!(emb.name, "text_embedding");
        assert_eq!(ts.name, "timestamp");
    }
}
