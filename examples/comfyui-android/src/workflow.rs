//! Builds a standard KSampler workflow (txt2img or img2img) from [`Params`] using rucomfyui's
//! typed nodes. Mirrors ComfyUI's default graph: checkpoint -> optional LoRA chain -> CLIP encode
//! x2 -> (empty latent or VAE-encoded input) -> KSampler -> VAE decode -> SaveImage.
//!
//! Between the decode and the save sits the enhance chain: [`crate::apps`] appends each configured
//! app's nodes onto the same graph, so upscalers and face fixes are data, not code.

use rucomfyui::nodes::all::{
    CLIPTextEncode, CheckpointLoaderSimple, EmptyLatentImage, KSampler, LoadImage, LoraLoader,
    SaveImage, VAEDecode, VAEEncode,
};
use rucomfyui::{Workflow, WorkflowGraph, WorkflowNodeId};

use crate::apps::{AppSet, Ctx, Report};
use crate::schema::SchemaSet;
use crate::types::{Mode, Params};

/// Construct the workflow and return the SaveImage node id (the node whose `Executed` output
/// carries the finished image bytes). `input_image` is the uploaded filename for img2img.
/// `apps`/`schemas` drive the enhance chain; pass empty ones to build the base graph alone.
pub fn build(
    p: &Params,
    input_image: Option<String>,
    apps: &AppSet,
    schemas: &SchemaSet,
) -> (Workflow, WorkflowNodeId, Report) {
    let (g, mut ctx) = build_base(p, input_image);
    let report = crate::apps::apply(&g, &mut ctx, &p.apps, apps, schemas, p);
    let out = g.add(SaveImage::new(ctx.image, "comfyui_android"));
    (g.into_workflow(), out, report)
}

/// The typed base graph, ending at the VAE decode. Publishes every handle an app can reference.
fn build_base(p: &Params, input_image: Option<String>) -> (WorkflowGraph, Ctx) {
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

    let positive = g.add(CLIPTextEncode::new(p.combined_positive(), clip.clone()));
    let negative = g.add(CLIPTextEncode::new(p.negative.clone(), clip.clone()));
    let samples = g.add(KSampler {
        model,
        seed: p.seed,
        steps: p.steps,
        cfg: p.cfg,
        sampler_name: p.sampler.clone(),
        scheduler: p.scheduler.clone(),
        positive,
        negative,
        latent_image: latent,
        denoise,
    });

    let image = g.add(VAEDecode { samples, vae: c.vae });
    let ctx = Ctx { image, latent: samples, model, clip, vae: c.vae, positive, negative };
    (g, ctx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> Params {
        Params { checkpoint: "sd.safetensors".into(), positive: "a cat".into(), ..Default::default() }
    }

    fn schemas_with(json: &str) -> SchemaSet {
        crate::schema::parse(&serde_json::from_str(json).unwrap())
    }

    #[test]
    fn base_graph_ends_at_a_single_save_image() {
        let (apps, schemas) = (AppSet::default(), SchemaSet::default());
        let (wf, out, report) = build(&params(), None, &apps, &schemas);
        assert!(report.applied.is_empty() && report.skipped.is_empty());
        let saves: Vec<_> = wf.0.iter().filter(|(_, n)| n.class_type == "SaveImage").collect();
        assert_eq!(saves.len(), 1);
        assert_eq!(*saves[0].0, out);
        // The save consumes the decode directly when no apps are configured.
        let (src, _) = wf.0[&out].inputs["images"].as_slot().unwrap();
        assert_eq!(wf.0[&src].class_type, "VAEDecode");
    }

    #[test]
    fn an_app_splices_between_decode_and_save() {
        let apps = AppSet::builtin();
        let schemas = schemas_with(
            r#"{"ImageSharpen": {"input": {"required": {"image": ["IMAGE"], "sharpen_radius": ["INT", {"default": 1}], "sigma": ["FLOAT", {"default": 1.0}], "alpha": ["FLOAT", {"default": 1.0}]}}, "output": ["IMAGE"]}}"#,
        );
        let mut p = params();
        p.apps = vec![crate::types::AppStep::new(apps.get("sharpen").unwrap())];
        let (wf, out, report) = build(&p, None, &apps, &schemas);
        assert_eq!(report.applied, vec!["Sharpen"]);
        assert_eq!(wf.0.values().filter(|n| n.class_type == "SaveImage").count(), 1);
        let (sharp, _) = wf.0[&out].inputs["images"].as_slot().unwrap();
        assert_eq!(wf.0[&sharp].class_type, "ImageSharpen");
        let (dec, _) = wf.0[&sharp].inputs["image"].as_slot().unwrap();
        assert_eq!(wf.0[&dec].class_type, "VAEDecode");
    }

    #[test]
    fn typed_and_dynamic_nodes_share_one_id_allocator() {
        let apps = AppSet::builtin();
        let schemas = schemas_with(
            r#"{"ImageScaleBy": {"input": {"required": {"image": ["IMAGE"], "upscale_method": [["lanczos"]], "scale_by": ["FLOAT", {"default": 1.0}]}}, "output": ["IMAGE"]}}"#,
        );
        let mut p = params();
        p.apps = vec![crate::types::AppStep::new(apps.get("upscale.scale").unwrap())];
        let (wf, _, _) = build(&p, None, &apps, &schemas);
        let mut ids: Vec<u32> = wf.0.keys().map(|k| k.0).collect();
        let n = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), n, "duplicate node ids");
        // No holes: the dynamic nodes continued the typed allocator.
        assert_eq!(ids.last().unwrap() - ids[0] + 1, n as u32);
    }

    #[test]
    fn hires_fix_rebinds_the_latent_and_reuses_the_base_vae() {
        let apps = AppSet::builtin();
        let schemas = schemas_with(
            r#"{
            "LatentUpscaleBy": {"input": {"required": {"samples": ["LATENT"], "upscale_method": [["bislerp"]], "scale_by": ["FLOAT", {"default": 1.5}]}}, "output": ["LATENT"]},
            "KSampler": {"input": {"required": {"model": ["MODEL"], "positive": ["CONDITIONING"], "negative": ["CONDITIONING"], "latent_image": ["LATENT"], "seed": ["INT", {"default": 0}], "steps": ["INT", {"default": 20}], "cfg": ["FLOAT", {"default": 8.0}], "sampler_name": [["euler"]], "scheduler": [["normal"]], "denoise": ["FLOAT", {"default": 1.0}]}}, "output": ["LATENT"]},
            "VAEDecode": {"input": {"required": {"samples": ["LATENT"], "vae": ["VAE"]}}, "output": ["IMAGE"]}
        }"#,
        );
        let mut p = params();
        p.apps = vec![crate::types::AppStep::new(apps.get("hires.fix").unwrap())];
        let (wf, out, report) = build(&p, None, &apps, &schemas);
        assert_eq!(report.applied, vec!["Hi-res fix"]);
        // Save <- second VAEDecode <- second KSampler <- LatentUpscaleBy <- base KSampler latent.
        let (dec2, _) = wf.0[&out].inputs["images"].as_slot().unwrap();
        let (ks2, _) = wf.0[&dec2].inputs["samples"].as_slot().unwrap();
        let (lat, _) = wf.0[&ks2].inputs["latent_image"].as_slot().unwrap();
        assert_eq!(wf.0[&lat].class_type, "LatentUpscaleBy");
        let (base_ks, _) = wf.0[&lat].inputs["samples"].as_slot().unwrap();
        assert_eq!(wf.0[&base_ks].class_type, "KSampler");
        // Both decodes hang off the one checkpoint loader's VAE.
        let (vae_a, _) = wf.0[&dec2].inputs["vae"].as_slot().unwrap();
        assert_eq!(wf.0[&vae_a].class_type, "CheckpointLoaderSimple");
    }
}
