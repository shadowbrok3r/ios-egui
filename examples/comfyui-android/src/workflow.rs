//! Builds a standard KSampler workflow (txt2img or img2img) from [`Params`] using rucomfyui's
//! typed nodes. Mirrors ComfyUI's default graph: checkpoint -> optional LoRA chain -> CLIP encode
//! x2 -> (empty latent or VAE-encoded input) -> KSampler -> VAE decode -> SaveImage.

use rucomfyui::nodes::all::{
    CLIPTextEncode, CheckpointLoaderSimple, EmptyLatentImage, KSampler, LoadImage, LoraLoader,
    SaveImage, VAEDecode, VAEEncode,
};
use rucomfyui::{Workflow, WorkflowGraph, WorkflowNodeId};

use crate::types::{Mode, Params};

/// Construct the workflow and return the SaveImage node id (the node whose `Executed` output
/// carries the finished image bytes). `input_image` is the uploaded filename for img2img.
pub fn build(p: &Params, input_image: Option<String>) -> (Workflow, WorkflowNodeId) {
    let g = WorkflowGraph::new();
    let c = g.add(CheckpointLoaderSimple::new(p.checkpoint.clone()));

    let (model, clip) = {
        let mut model = c.model;
        let mut clip = c.clip;
        for lora in &p.loras {
            if lora.file.trim().is_empty() {
                continue;
            }
            let out = g.add(LoraLoader::new(
                model,
                clip,
                lora.file.clone(),
                lora.strength_model,
                lora.strength_clip,
            ));
            model = out.model;
            clip = out.clip;
        }
        (model, clip)
    };

    let latent = match input_image {
        Some(name) if p.mode == Mode::Img2Img => {
            g.add(VAEEncode::new(g.add(LoadImage::new(name)).image, c.vae))
        }
        _ => g.add(EmptyLatentImage {
            width: p.width,
            height: p.height,
            batch_size: p.batch_size,
        }),
    };

    // denoise 1.0 regenerates fully; < 1.0 keeps the input image's structure for img2img.
    let denoise = if p.mode == Mode::Img2Img { p.denoise } else { 1.0 };

    let samples = g.add(KSampler {
        model,
        seed: p.seed,
        steps: p.steps,
        cfg: p.cfg,
        sampler_name: p.sampler.clone(),
        scheduler: p.scheduler.clone(),
        positive: g.add(CLIPTextEncode::new(p.combined_positive(), clip.clone())),
        negative: g.add(CLIPTextEncode::new(p.negative.clone(), clip)),
        latent_image: latent,
        denoise,
    });

    let image = g.add(VAEDecode { samples, vae: c.vae });
    let out = g.add(SaveImage::new(image, "comfyui_android"));
    (g.into_workflow(), out)
}
