//! Builds a standard KSampler workflow (txt2img or img2img) from [`Params`] using rucomfyui's
//! typed nodes. Mirrors ComfyUI's default graph: checkpoint -> optional LoRA chain -> CLIP encode
//! x2 -> (empty latent or VAE-encoded input) -> KSampler -> VAE decode -> SaveImage.
//!
//! Between the decode and the save sits the enhance chain: [`crate::apps`] appends each configured
//! app's nodes onto the same graph, so upscalers and face fixes are data, not code.

use rucomfyui::nodes::all::{
    CLIPLoader, CLIPTextEncode, CheckpointLoaderSimple, DualCLIPLoader, EmptyLatentImage, KSampler,
    LoadImage, LoraLoader, LoraLoaderModelOnly, SaveImage, UNETLoader, VAEDecode, VAEEncode,
    VAELoader,
};
use rucomfyui::nodes::types::{ClipOut, ModelOut, VaeOut};
use rucomfyui::{Workflow, WorkflowGraph, WorkflowNodeId};

use crate::apps::{AppSet, Ctx, Report};
use crate::schema::SchemaSet;
use crate::types::{Mode, ModelKind, Params};

/// Construct the workflow and return the SaveImage node id (the node whose `Executed` output
/// carries the finished image bytes). `input_image` is the uploaded filename for img2img.
/// `apps`/`schemas` drive the enhance chain; pass empty ones to build the base graph alone.
pub fn build(
    p: &Params,
    input_image: Option<String>,
    apps: &AppSet,
    schemas: &SchemaSet,
) -> (Workflow, WorkflowNodeId, Report) {
    // Enabled steps may adjust the Create settings (hi-res fix renders the base pass small and
    // scales it up). Resolve that layer ONCE here so the base graph and every `$param:` inside an
    // app read the same numbers the Create tab is showing.
    let (p, notes) = crate::apps::effective_params(p, &p.apps, apps, Some(schemas));
    let (g, mut ctx) = build_base(&p, input_image);
    let mut report = crate::apps::apply(&g, &mut ctx, &p.apps, apps, schemas, &p);
    report.params = notes;
    let out = g.add(SaveImage::new(ctx.image, "comfyui_android"));
    (g.into_workflow(), out, report)
}

/// The typed base graph, ending at the VAE decode. Publishes every handle an app can reference.
fn build_base(p: &Params, input_image: Option<String>) -> (WorkflowGraph, Ctx) {
    let g = WorkflowGraph::new();

    // Checkpoints carry MODEL+CLIP+VAE in one file; diffusion models (Anima, Flux, Qwen-Image)
    // are bare UNETs that need the text encoder and VAE loaded alongside them.
    let (base_model, base_clip, vae): (ModelOut, ClipOut, VaeOut) = match p.model_kind {
        ModelKind::Checkpoint => {
            let c = g.add(CheckpointLoaderSimple::new(p.checkpoint.clone()));
            (c.model, c.clip, c.vae)
        }
        ModelKind::Diffusion => {
            let model = g.add(UNETLoader::new(p.unet_name.clone(), p.effective_weight_dtype()));
            let device: Option<String> =
                (!p.clip_device.trim().is_empty()).then(|| p.clip_device.clone());
            let ty = p.effective_clip_type();
            let names = p.active_clips();
            let clip = if names.len() >= 2 {
                g.add(DualCLIPLoader::new(names[0].clone(), names[1].clone(), ty, device))
            } else {
                g.add(CLIPLoader::new(names.first().cloned().unwrap_or_default(), ty, device))
            };
            (model, clip, g.add(VAELoader::new(p.vae_name.clone())))
        }
    };

    let (model, clip) = {
        let mut model = base_model;
        let mut clip = base_clip;
        for lora in &p.loras {
            if lora.file.trim().is_empty() {
                continue;
            }
            if lora.model_only {
                model =
                    g.add(LoraLoaderModelOnly::new(model, lora.file.clone(), lora.strength_model));
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
            g.add(VAEEncode::new(g.add(LoadImage::new(name)).image, vae))
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

    let image = g.add(VAEDecode { samples, vae });
    let ctx = Ctx { image, latent: samples, model, clip, vae, positive, negative };
    (g, ctx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> Params {
        Params { checkpoint: "sd.safetensors".into(), positive: "a cat".into(), ..Default::default() }
    }

    /// The user's proven-working Anima setup: a bare UNET plus a Qwen3 encoder and Qwen-Image VAE.
    fn diffusion_params() -> Params {
        Params {
            model_kind: ModelKind::Diffusion,
            unet_name: "Anima/novaAnimeAM_v30.safetensors".into(),
            clip_names: vec!["qwen_3_06b_base.safetensors".into()],
            clip_type: "stable_diffusion".into(),
            vae_name: "qwen_image_vae.safetensors".into(),
            positive: "1girl, solo".into(),
            ..Default::default()
        }
    }

    fn class_of<'a>(wf: &'a Workflow, id: &WorkflowNodeId) -> &'a str {
        &wf.0[id].class_type
    }

    /// Follow `input` back to the node feeding it.
    fn upstream(wf: &Workflow, id: &WorkflowNodeId, input: &str) -> WorkflowNodeId {
        wf.0[id].inputs[input].as_slot().unwrap().0
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

    /// "Open as graph" builds with a placeholder filename rather than no input, because the tab
    /// it produces can itself be queued — an EmptyLatentImage there is a silent txt2img.
    #[test]
    fn img2img_with_a_placeholder_name_still_has_the_img2img_shape() {
        let (apps, schemas) = (AppSet::default(), SchemaSet::default());
        let mut p = params();
        p.mode = Mode::Img2Img;
        p.denoise = 0.6;

        let (wf, out, _) = build(&p, Some(crate::engine::INPUT_IMAGE_NAME.into()), &apps, &schemas);
        let (dec, _) = wf.0[&out].inputs["images"].as_slot().unwrap();
        let (ks, _) = wf.0[&dec].inputs["samples"].as_slot().unwrap();
        let (latent, _) = wf.0[&ks].inputs["latent_image"].as_slot().unwrap();
        assert_eq!(wf.0[&latent].class_type, "VAEEncode", "img2img collapsed to txt2img");
        let (load, _) = wf.0[&latent].inputs["pixels"].as_slot().unwrap();
        assert_eq!(wf.0[&load].class_type, "LoadImage");

        // Without an input it is a txt2img graph, which is what made the preview misleading.
        let (wf2, out2, _) = build(&p, None, &apps, &schemas);
        let (dec2, _) = wf2.0[&out2].inputs["images"].as_slot().unwrap();
        let (ks2, _) = wf2.0[&dec2].inputs["samples"].as_slot().unwrap();
        let (lat2, _) = wf2.0[&ks2].inputs["latent_image"].as_slot().unwrap();
        assert_eq!(wf2.0[&lat2].class_type, "EmptyLatentImage");
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

    /// Anima models are bare UNETs: the model, the text encoder and the VAE each load separately.
    #[test]
    fn a_diffusion_model_uses_three_separate_loaders() {
        let (apps, schemas) = (AppSet::default(), SchemaSet::default());
        let (wf, out, _) = build(&diffusion_params(), None, &apps, &schemas);

        assert!(
            !wf.0.values().any(|n| n.class_type == "CheckpointLoaderSimple"),
            "diffusion path must not emit a checkpoint loader"
        );

        let dec = upstream(&wf, &out, "images");
        let ks = upstream(&wf, &dec, "samples");

        let unet = upstream(&wf, &ks, "model");
        assert_eq!(class_of(&wf, &unet), "UNETLoader");
        assert_eq!(wf.0[&unet].inputs["unet_name"].as_str().unwrap(), "Anima/novaAnimeAM_v30.safetensors");
        assert_eq!(wf.0[&unet].inputs["weight_dtype"].as_str().unwrap(), "default");

        let vae = upstream(&wf, &dec, "vae");
        assert_eq!(class_of(&wf, &vae), "VAELoader");
        assert_eq!(wf.0[&vae].inputs["vae_name"].as_str().unwrap(), "qwen_image_vae.safetensors");

        for socket in ["positive", "negative"] {
            let enc = upstream(&wf, &ks, socket);
            assert_eq!(class_of(&wf, &enc), "CLIPTextEncode");
            let clip = upstream(&wf, &enc, "clip");
            assert_eq!(class_of(&wf, &clip), "CLIPLoader");
            assert_eq!(wf.0[&clip].inputs["clip_name"].as_str().unwrap(), "qwen_3_06b_base.safetensors");
            // rucomfyui's `type_` field serializes to ComfyUI's `type` key.
            assert_eq!(wf.0[&clip].inputs["type"].as_str().unwrap(), "stable_diffusion");
        }
    }

    /// The encode and the decode must land on one VAE node, not two competing loaders.
    #[test]
    fn diffusion_img2img_shares_one_vae_with_the_decode() {
        let (apps, schemas) = (AppSet::default(), SchemaSet::default());
        let mut p = diffusion_params();
        p.mode = Mode::Img2Img;
        p.denoise = 0.6;

        let (wf, out, _) = build(&p, Some(crate::engine::INPUT_IMAGE_NAME.into()), &apps, &schemas);
        let dec = upstream(&wf, &out, "images");
        let ks = upstream(&wf, &dec, "samples");
        let enc = upstream(&wf, &ks, "latent_image");
        assert_eq!(class_of(&wf, &enc), "VAEEncode");
        assert_eq!(upstream(&wf, &enc, "vae"), upstream(&wf, &dec, "vae"), "two separate VAE loaders");
    }

    /// Two encoders fold into a DualCLIPLoader, whose ComfyUI keys drop the underscore.
    #[test]
    fn two_text_encoders_emit_a_dual_clip_loader() {
        let (apps, schemas) = (AppSet::default(), SchemaSet::default());
        let mut p = diffusion_params();
        p.clip_names = vec!["clip_l.safetensors".into(), "t5xxl.safetensors".into()];

        let (wf, _, _) = build(&p, None, &apps, &schemas);
        let dual = wf.0.values().find(|n| n.class_type == "DualCLIPLoader").expect("no DualCLIPLoader");
        assert_eq!(dual.inputs["clip_name1"].as_str().unwrap(), "clip_l.safetensors");
        assert_eq!(dual.inputs["clip_name2"].as_str().unwrap(), "t5xxl.safetensors");
    }

    /// A model-only LoRA advances MODEL while the text encode keeps reading the raw CLIP.
    #[test]
    fn a_model_only_lora_leaves_the_clip_alone() {
        let (apps, schemas) = (AppSet::default(), SchemaSet::default());
        let mut p = diffusion_params();
        p.loras = vec![crate::types::ActiveLora {
            file: "Anima/MatureFemaleSliderAnima.safetensors".into(),
            strength_model: 0.7,
            strength_clip: 0.7,
            injected: String::new(),
            model_only: true,
        }];

        let (wf, out, _) = build(&p, None, &apps, &schemas);
        let ks = upstream(&wf, &upstream(&wf, &out, "images"), "samples");
        let lora = upstream(&wf, &ks, "model");
        assert_eq!(class_of(&wf, &lora), "LoraLoaderModelOnly");
        assert_eq!(class_of(&wf, &upstream(&wf, &lora, "model")), "UNETLoader");
        // The encode still hangs off the loader directly — no CLIP passed through the LoRA.
        let enc = upstream(&wf, &ks, "positive");
        assert_eq!(class_of(&wf, &upstream(&wf, &enc, "clip")), "CLIPLoader");
    }

    /// A model+clip LoRA still threads both handles on the diffusion path.
    #[test]
    fn a_standard_lora_threads_clip_on_the_diffusion_path() {
        let (apps, schemas) = (AppSet::default(), SchemaSet::default());
        let mut p = diffusion_params();
        p.loras = vec![crate::types::ActiveLora {
            file: "GothicNeonAnima.safetensors".into(),
            strength_model: 0.7,
            strength_clip: 0.7,
            injected: String::new(),
            model_only: false,
        }];

        let (wf, out, _) = build(&p, None, &apps, &schemas);
        let ks = upstream(&wf, &upstream(&wf, &out, "images"), "samples");
        assert_eq!(class_of(&wf, &upstream(&wf, &ks, "model")), "LoraLoader");
        let enc = upstream(&wf, &ks, "positive");
        let lora = upstream(&wf, &enc, "clip");
        assert_eq!(class_of(&wf, &lora), "LoraLoader");
        assert_eq!(class_of(&wf, &upstream(&wf, &lora, "clip")), "CLIPLoader");
    }
}
