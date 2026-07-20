//! Anima text2img orchestration.
//!
//! Stages run one context at a time to cap peak memory: clip -> drop, both DiT
//! halves -> denoise loop -> drop, vae_decoder -> drop. Every graph is named
//! `"model"`; only clip needs [`qnn_rs::Context::execute_mixed`] because
//! `t5_ids` is Int32.

use crate::error::{Error, Result};
use crate::img::{self, Image};
use crate::latent::{self, LATENT_CHANNELS, VAE_SCALE};
use crate::pack::AnimaPack;
use crate::scheduler::Scheduler;
use crate::text::{self, ClipInputs};
use qnn_rs::{Context, ContextOpts, QnnSystem, Session, TensorIn};
use rand::rngs::StdRng;
use rand::SeedableRng;
use rand_distr::{Distribution, StandardNormal};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

/// The single graph name in every Anima context binary.
pub const GRAPH: &str = "model";
/// Text context sequence length.
pub const CONTEXT_SEQ: usize = 512;
/// Text context width.
pub const CONTEXT_DIM: usize = 1024;

/// text2img parameters, defaulting to the shipped config.json values.
#[derive(Clone, Debug)]
pub struct AnimaParams {
    pub steps: usize,
    pub cfg: f32,
    pub seed: u64,
    pub scheduler: String,
    pub width: usize,
    pub height: usize,
    /// Used only when `cfg != 1.0`.
    pub negative: String,
}

impl Default for AnimaParams {
    fn default() -> Self {
        Self { steps: 10, cfg: 1.0, seed: 0, scheduler: "euler".into(), width: 1024, height: 1024, negative: String::new() }
    }
}

impl AnimaParams {
    /// Parameters seeded from a pack's config.json.
    pub fn from_pack(pack: &AnimaPack) -> Self {
        let c = pack.config();
        Self {
            steps: c.default_steps,
            cfg: c.default_cfg,
            seed: 0,
            scheduler: c.default_scheduler.clone(),
            width: c.default_width,
            height: c.default_height,
            negative: c.default_negative_prompt.clone(),
        }
    }

    /// Latent grid size.
    pub fn latent_wh(&self) -> (usize, usize) {
        (self.width / VAE_SCALE, self.height / VAE_SCALE)
    }

    /// Latent element count.
    pub fn latent_elems(&self) -> usize {
        let (w, h) = self.latent_wh();
        LATENT_CHANNELS * w * h
    }

    /// True when the negative branch must run.
    pub fn do_cfg(&self) -> bool {
        (self.cfg - 1.0).abs() > 1e-3
    }
}

/// Classifier-free guidance mix: `uncond + scale * (cond - uncond)`.
pub fn apply_cfg(uncond: &[f32], cond: &[f32], scale: f32) -> Vec<f32> {
    uncond.iter().zip(cond).map(|(&u, &c)| u + scale * (c - u)).collect()
}

fn randn(n: usize, rng: &mut StdRng) -> Vec<f32> {
    (0..n).map(|_| StandardNormal.sample(rng)).collect()
}

fn take(mut out: HashMap<String, Vec<f32>>, name: &'static str) -> Result<Vec<f32>> {
    out.remove(name).ok_or(Error::MissingOutput { graph: GRAPH, name })
}

fn check(name: &'static str, got: usize, expected: usize) -> Result<()> {
    if got == expected { Ok(()) } else { Err(Error::ShapeMismatch { name, expected, got }) }
}

/// Run `clip.bin` on prepared text inputs, returning `context[1, 512, 1024]`.
pub fn encode_text(clip: &Context<'_>, inputs: &ClipInputs) -> Result<Vec<f32>> {
    let out = clip.execute_mixed(
        GRAPH,
        &[
            ("input_embedding", TensorIn::F32(&inputs.input_embedding)),
            ("t5_ids", TensorIn::I32(&inputs.t5_ids)),
            ("t5_mask", TensorIn::F32(&inputs.t5_mask)),
            ("qwen_mask", TensorIn::F32(&inputs.qwen_mask)),
        ],
    )?;
    let ctx = take(out, "context")?;
    check("context", ctx.len(), CONTEXT_SEQ * CONTEXT_DIM)?;
    Ok(ctx)
}

/// One full DiT pass: part1 then part2, returning the velocity prediction.
pub fn dit_velocity(part1: &Context<'_>, part2: &Context<'_>, sample: &[f32], context: &[f32], sigma: f32) -> Result<Vec<f32>> {
    let ts = [sigma];
    let out1 = part1.execute(GRAPH, &[("sample", sample), ("encoder_hidden_states", context), ("timestamp", &ts)])?;
    let hidden = out1.get("hidden").cloned().ok_or(Error::MissingOutput { graph: GRAPH, name: "hidden" })?;
    let emb = take(out1, "emb")?;
    let out2 = part2.execute(GRAPH, &[("hidden", &hidden), ("emb", &emb), ("context", context), ("timestamp", &ts)])?;
    let v = take(out2, "output")?;
    check("output", v.len(), sample.len())?;
    Ok(v)
}

/// Denormalize latents and run `vae_decoder.bin`, returning an RGB image.
pub fn decode_latents(vae: &Context<'_>, latents: &[f32], width: usize, height: usize) -> Result<Image> {
    let plane = (width / VAE_SCALE) * (height / VAE_SCALE);
    let vae_in = latent::model_to_vae(latents, plane);
    let out = vae.execute(GRAPH, &[("input", vae_in.as_slice())])?;
    let decoded = take(out, "output")?;
    check("output", decoded.len(), 3 * width * height)?;
    Ok(img::vae_output_to_image(&decoded, width as u32, height as u32))
}

/// Generate an image from `prompt`. `progress(step_done, total)` fires after
/// each denoise step. Contexts are created and dropped stage by stage, so peak
/// resident weights are one stage's, not the whole pipeline's.
pub fn text2img<F>(
    pack: &AnimaPack,
    session: &Session<'_>,
    system: &QnnSystem,
    prompt: &str,
    params: &AnimaParams,
    progress: F,
) -> Result<Image>
where
    F: FnMut(usize, usize),
{
    text2img_cancellable(pack, session, system, prompt, params, progress, None)
}

/// [`text2img`] that returns [`Error::Msg`] `"cancelled"` between stages when
/// `cancel` is set.
pub fn text2img_cancellable<F>(
    pack: &AnimaPack,
    session: &Session<'_>,
    system: &QnnSystem,
    prompt: &str,
    params: &AnimaParams,
    mut progress: F,
    cancel: Option<&AtomicBool>,
) -> Result<Image>
where
    F: FnMut(usize, usize),
{
    let cancelled = || cancel.is_some_and(|c| c.load(Ordering::Relaxed));
    let bail = || Err(Error::Msg("cancelled".into()));

    let t = Instant::now();
    let tokenizers = pack.tokenizers()?;
    let table = pack.token_emb();
    let cond_text = text::build_clip_inputs(&tokenizers, &table, prompt)?;
    let uncond_text =
        params.do_cfg().then(|| text::build_clip_inputs(&tokenizers, &table, &params.negative)).transpose()?;
    log::info!("anima: text prep {:.2}s", t.elapsed().as_secs_f32());
    if cancelled() {
        return bail();
    }

    let t = Instant::now();
    let (cond, uncond) = {
        let bytes = pack.map("clip.bin")?;
        let clip = session.load_context(system, &bytes, &ContextOpts::default())?;
        let cond = encode_text(&clip, &cond_text)?;
        let uncond = uncond_text.as_ref().map(|u| encode_text(&clip, u)).transpose()?;
        (cond, uncond)
    };
    log::info!("anima: clip {:.2}s", t.elapsed().as_secs_f32());
    if cancelled() {
        return bail();
    }

    let mut sched = Scheduler::from_name(&params.scheduler, params.steps);
    let mut rng = StdRng::seed_from_u64(params.seed);
    let n = params.latent_elems();
    let mut latents: Vec<f32> = randn(n, &mut rng).iter().map(|x| x * sched.init_noise_sigma()).collect();

    let total = sched.len();
    {
        let t = Instant::now();
        let b1 = pack.map("unet_part1.bin")?;
        let b2 = pack.map("unet_part2.bin")?;
        let part1 = session.load_context(system, &b1, &ContextOpts::default())?;
        let part2 = session.load_context(system, &b2, &ContextOpts::default())?;
        log::info!("anima: dit load {:.2}s", t.elapsed().as_secs_f32());

        for i in 0..total {
            if cancelled() {
                return bail();
            }
            let sigma = sched.sigmas()[i];
            let model_in = sched.scale_model_input(&latents);
            let v_cond = dit_velocity(&part1, &part2, &model_in, &cond, sigma)?;
            let v = match &uncond {
                Some(u) => {
                    let v_uncond = dit_velocity(&part1, &part2, &model_in, u, sigma)?;
                    apply_cfg(&v_uncond, &v_cond, params.cfg)
                }
                None => v_cond,
            };
            let noise = if sched.eta() > 0.0 { randn(n, &mut rng) } else { Vec::new() };
            let (prev, _denoised) = sched.step(&v, &latents, &noise);
            latents = prev;
            progress(i + 1, total);
        }
    }

    if cancelled() {
        return bail();
    }
    let t = Instant::now();
    let bytes = pack.map("vae_decoder.bin")?;
    let vae = session.load_context(system, &bytes, &ContextOpts::default())?;
    let image = decode_latents(&vae, &latents, params.width, params.height)?;
    log::info!("anima: vae decode {:.2}s", t.elapsed().as_secs_f32());
    Ok(image)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cfg_mixes_cond_and_uncond() {
        assert_eq!(apply_cfg(&[0.0, 2.0], &[1.0, 4.0], 2.0), vec![2.0, 6.0]);
    }

    #[test]
    fn cfg_scale_one_returns_cond() {
        assert_eq!(apply_cfg(&[3.0], &[5.0], 1.0), vec![5.0]);
    }

    #[test]
    fn default_params_match_the_shipped_config() {
        let p = AnimaParams::default();
        assert_eq!((p.width, p.height), (1024, 1024));
        assert_eq!(p.steps, 10);
        assert_eq!(p.cfg, 1.0);
        assert_eq!(p.scheduler, "euler");
        assert!(!p.do_cfg());
    }

    #[test]
    fn latent_geometry_matches_the_graph_shapes() {
        let p = AnimaParams::default();
        assert_eq!(p.latent_wh(), (128, 128));
        assert_eq!(p.latent_elems(), 16 * 128 * 128);
    }

    #[test]
    fn cfg_above_one_enables_the_negative_branch() {
        let p = AnimaParams { cfg: 4.5, ..Default::default() };
        assert!(p.do_cfg());
    }

    #[test]
    fn randn_is_seed_reproducible() {
        let a = randn(8, &mut StdRng::seed_from_u64(42));
        let b = randn(8, &mut StdRng::seed_from_u64(42));
        assert_eq!(a, b);
        assert_ne!(a, randn(8, &mut StdRng::seed_from_u64(7)));
    }

    #[test]
    fn missing_output_is_reported() {
        let out: HashMap<String, Vec<f32>> = HashMap::new();
        assert!(matches!(take(out, "context"), Err(Error::MissingOutput { name: "context", .. })));
    }

    #[test]
    fn shape_mismatch_is_reported() {
        assert!(check("context", 4, 4).is_ok());
        assert!(matches!(check("context", 3, 4), Err(Error::ShapeMismatch { expected: 4, got: 3, .. })));
    }
}
