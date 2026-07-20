//! Builds a standard KSampler workflow (txt2img or img2img) from [`Params`] using rucomfyui's
//! typed nodes. Mirrors ComfyUI's default graph: checkpoint -> optional LoRA chain -> CLIP encode
//! x2 -> (empty latent or VAE-encoded input) -> KSampler -> VAE decode -> SaveImage.
//!
//! Between the decode and the save sits the enhance chain: [`crate::apps`] appends each configured
//! app's nodes onto the same graph, so upscalers and face fixes are data, not code.

use rucomfyui::nodes::all::{
    CLIPLoader, CLIPTextEncode, CheckpointLoaderSimple, DualCLIPLoader, EmptyLatentImage,
    ImageFromBatch, KSampler, KSamplerAdvanced, LoadImage, LoraLoader, LoraLoaderModelOnly,
    ModelSamplingSD3, SaveImage, SetLatentNoiseMask, UNETLoader, VAEDecode, VAEEncode, VAELoader,
    WanImageToVideo,
};
use rucomfyui::nodes::types::{
    ClipOut, ClipVisionOutputOut, ImageOut, LatentOut, ModelOut, Out, VaeOut,
};
use rucomfyui::workflow::{WorkflowInput, WorkflowMeta, WorkflowNode};
use rucomfyui::{Workflow, WorkflowGraph, WorkflowNodeId};

use crate::apps::{AppSet, Ctx, Report};
use crate::schema::SchemaSet;
use crate::types::{ActiveLora, Mode, ModelKind, Params, VideoParams};

/// The Create output filename prefix, shared by images and video outputs.
const OUTPUT_PREFIX: &str = "comfyui_android";

/// Snap a frame count to the nearest valid Wan length (`4n + 1`, minimum 1).
pub fn snap_wan_length(len: u32) -> u32 {
    let len = len.max(1);
    ((len - 1 + 2) / 4) * 4 + 1
}

/// Dispatch to the video builder for [`Mode::Video`], else the standard KSampler builder.
pub fn build_dispatch(
    p: &Params,
    input_image: Option<String>,
    apps: &AppSet,
    schemas: &SchemaSet,
) -> (Workflow, WorkflowNodeId, Report) {
    if p.mode == Mode::Video {
        build_video(p, input_image, schemas)
    } else {
        build(p, input_image, apps, schemas)
    }
}

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
            let img = g.add(LoadImage::new(name));
            let enc = g.add(VAEEncode::new(img.image, vae));
            if p.inpaint_mask {
                g.add(SetLatentNoiseMask::new(enc, img.mask))
            } else {
                enc
            }
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

/// Emit a dynamic (custom-node) node with the given literal / slot inputs.
fn dynamic(g: &WorkflowGraph, class: &str, inputs: Vec<(&str, WorkflowInput)>) -> WorkflowNodeId {
    g.add_dynamic(WorkflowNode {
        inputs: inputs.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        class_type: class.to_string(),
        meta: Some(WorkflowMeta::new(class)),
    })
}

/// `UNETLoader -> model-only LoRA chain -> ModelSamplingSD3`, skipping empty / zero-strength LoRAs.
fn sampling_model(
    g: &WorkflowGraph,
    unet: &str,
    dtype: String,
    loras: &[ActiveLora],
    shift: f32,
) -> ModelOut {
    let mut model = g.add(UNETLoader::new(unet.to_string(), dtype));
    for lora in loras {
        if lora.file.trim().is_empty() || lora.strength_model == 0.0 {
            continue;
        }
        model = g.add(LoraLoaderModelOnly::new(model, lora.file.clone(), lora.strength_model));
    }
    g.add(ModelSamplingSD3 { model, shift })
}

/// Build the Wan 2.2 image-to-video graph, returning the last-frame `SaveImage` node id.
///
/// `input_image` is the uploaded start-image filename; `None` yields a text-to-video graph. VHS
/// output is always emitted; RIFE and GPU-clean nodes appear only when `schemas` has their class.
pub fn build_video(
    p: &Params,
    input_image: Option<String>,
    schemas: &SchemaSet,
) -> (Workflow, WorkflowNodeId, Report) {
    let v: &VideoParams = &p.video;
    let g = WorkflowGraph::new();
    let mut report = Report::default();
    let dtype = match v.weight_dtype.trim() {
        "" => "default".to_string(),
        s => s.to_string(),
    };

    let device = (!v.clip_device.trim().is_empty()).then(|| v.clip_device.clone());
    let clip = g.add(CLIPLoader::new(v.clip_name.clone(), v.clip_type.clone(), device));
    let vae = g.add(VAELoader::new(v.vae_name.clone()));
    let positive = g.add(CLIPTextEncode::new(p.combined_positive(), clip.clone()));
    let negative = g.add(CLIPTextEncode::new(p.negative.clone(), clip));

    let length = snap_wan_length(v.length);
    let wan = match input_image {
        Some(name) => {
            let img = g.add(LoadImage::new(name));
            g.add(WanImageToVideo::new(
                positive,
                negative,
                vae,
                v.width,
                v.height,
                length,
                1u32,
                None::<ClipVisionOutputOut>,
                Some(img.image),
            ))
        }
        None => g.add(WanImageToVideo::new(
            positive,
            negative,
            vae,
            v.width,
            v.height,
            length,
            1u32,
            None::<ClipVisionOutputOut>,
            None::<ImageOut>,
        )),
    };

    let high_model =
        sampling_model(&g, &v.unet_high, dtype.clone(), &v.loras_high, v.shift);
    let high = g.add(KSamplerAdvanced {
        model: high_model,
        add_noise: "enable".to_string(),
        noise_seed: p.seed,
        steps: v.steps,
        cfg: v.cfg_high,
        sampler_name: v.sampler.clone(),
        scheduler: v.scheduler.clone(),
        positive: wan.positive,
        negative: wan.negative,
        latent_image: wan.latent,
        start_at_step: 0u32,
        end_at_step: v.split_step,
        return_with_leftover_noise: "enable".to_string(),
    });

    let low_model = sampling_model(&g, &v.unet_low, dtype, &v.loras_low, v.shift);
    let low = g.add(KSamplerAdvanced {
        model: low_model,
        add_noise: "disable".to_string(),
        noise_seed: 0u64,
        steps: v.steps,
        cfg: v.cfg_low,
        sampler_name: v.sampler.clone(),
        scheduler: v.scheduler.clone(),
        positive: wan.positive,
        negative: wan.negative,
        latent_image: high,
        start_at_step: v.split_step,
        end_at_step: v.steps,
        return_with_leftover_noise: "disable".to_string(),
    });

    let clean = v.gpu_clean && schemas.has_node("easy cleanGpuUsed");
    if v.gpu_clean && !clean {
        report.warnings.push("GPU-clean skipped — no 'easy cleanGpuUsed' node".into());
    }
    // Free the experts before the VAE decode.
    let samples = if clean {
        let n = dynamic(&g, "easy cleanGpuUsed", vec![("anything", low.into())]);
        LatentOut::from_dynamic(n, 0)
    } else {
        low
    };
    let decode = g.add(VAEDecode { samples, vae });

    let last = g.add(ImageFromBatch::new(decode, -1i64, 1u32));
    let out = g.add(SaveImage::new(last, OUTPUT_PREFIX));

    let rife = v.rife && schemas.has_node("RIFE VFI");
    if v.rife && !rife {
        report.warnings.push("RIFE interpolation skipped — no 'RIFE VFI' node".into());
    }
    let (video_images, frame_rate) = if rife {
        // Free the VAE before the RIFE pass, matching V2's placement.
        let frames = if clean {
            let n = dynamic(&g, "easy cleanGpuUsed", vec![("anything", decode.into())]);
            WorkflowInput::slot(n, 0)
        } else {
            decode.into()
        };
        let mult = v.rife_multiplier.max(1);
        let n = dynamic(
            &g,
            "RIFE VFI",
            vec![
                ("ckpt_name", WorkflowInput::String(v.rife_ckpt.clone())),
                ("clear_cache_after_n_frames", WorkflowInput::I64(10)),
                ("multiplier", WorkflowInput::I64(mult as i64)),
                ("fast_mode", WorkflowInput::Boolean(false)),
                ("ensemble", WorkflowInput::Boolean(true)),
                ("scale_factor", WorkflowInput::I64(1)),
                ("frames", frames),
            ],
        );
        (WorkflowInput::slot(n, 0), 16 * mult as i64)
    } else {
        (decode.into(), 16)
    };
    dynamic(
        &g,
        "VHS_VideoCombine",
        vec![
            ("frame_rate", WorkflowInput::I64(frame_rate)),
            ("loop_count", WorkflowInput::I64(0)),
            ("filename_prefix", WorkflowInput::String(OUTPUT_PREFIX.to_string())),
            ("format", WorkflowInput::String("video/h264-mp4".to_string())),
            ("pix_fmt", WorkflowInput::String("yuv420p".to_string())),
            ("crf", WorkflowInput::I64(19)),
            ("save_metadata", WorkflowInput::Boolean(true)),
            ("trim_to_audio", WorkflowInput::Boolean(false)),
            ("pingpong", WorkflowInput::Boolean(false)),
            ("save_output", WorkflowInput::Boolean(true)),
            ("images", video_images),
        ],
    );

    (g.into_workflow(), out, report)
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

    /// Inpainting threads the VAE-encoded latent and the LoadImage mask through SetLatentNoiseMask.
    #[test]
    fn masked_img2img_inserts_a_set_latent_noise_mask() {
        let (apps, schemas) = (AppSet::default(), SchemaSet::default());
        let mut p = params();
        p.mode = Mode::Img2Img;
        p.inpaint_mask = true;
        p.denoise = 0.45;

        let (wf, out, _) = build(&p, Some(crate::engine::INPUT_IMAGE_NAME.into()), &apps, &schemas);
        let ks = upstream(&wf, &upstream(&wf, &out, "images"), "samples");
        let mask_node = upstream(&wf, &ks, "latent_image");
        assert_eq!(class_of(&wf, &mask_node), "SetLatentNoiseMask");
        // samples <- VAEEncode, mask <- LoadImage's second output.
        let enc = upstream(&wf, &mask_node, "samples");
        assert_eq!(class_of(&wf, &enc), "VAEEncode");
        let (load, slot) = wf.0[&mask_node].inputs["mask"].as_slot().unwrap();
        assert_eq!(wf.0[&load].class_type, "LoadImage");
        assert_eq!(slot, 1, "mask reads LoadImage's mask output slot");
        // One LoadImage feeds both the encode's pixels and the mask.
        assert_eq!(upstream(&wf, &enc, "pixels"), load);
        assert_eq!(wf.0[&ks].inputs["denoise"].as_f64().unwrap() as f32, 0.45);
    }

    /// Unmasked img2img keeps the plain VAEEncode -> KSampler shape with no mask node.
    #[test]
    fn unmasked_img2img_has_no_set_latent_noise_mask() {
        let (apps, schemas) = (AppSet::default(), SchemaSet::default());
        let mut p = params();
        p.mode = Mode::Img2Img;
        p.inpaint_mask = false;
        p.denoise = 0.6;

        let (wf, out, _) = build(&p, Some(crate::engine::INPUT_IMAGE_NAME.into()), &apps, &schemas);
        assert!(!wf.0.values().any(|n| n.class_type == "SetLatentNoiseMask"));
        let ks = upstream(&wf, &upstream(&wf, &out, "images"), "samples");
        let enc = upstream(&wf, &ks, "latent_image");
        assert_eq!(class_of(&wf, &enc), "VAEEncode");
        assert_eq!(class_of(&wf, &upstream(&wf, &enc, "pixels")), "LoadImage");
    }

    /// The inpaint flag is inert for txt2img: no input image, so the latent stays empty.
    #[test]
    fn txt2img_ignores_the_inpaint_flag() {
        let (apps, schemas) = (AppSet::default(), SchemaSet::default());
        let mut p = params();
        p.mode = Mode::Txt2Img;
        p.inpaint_mask = true;

        let (wf, out, _) = build(&p, Some(crate::engine::INPUT_IMAGE_NAME.into()), &apps, &schemas);
        assert!(!wf.0.values().any(|n| n.class_type == "SetLatentNoiseMask"));
        let ks = upstream(&wf, &upstream(&wf, &out, "images"), "samples");
        assert_eq!(class_of(&wf, &upstream(&wf, &ks, "latent_image")), "EmptyLatentImage");
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

    fn video_params() -> Params {
        Params {
            mode: Mode::Video,
            positive: "the camera pans".into(),
            seed: 42,
            ..Default::default()
        }
    }

    /// Walk a KSamplerAdvanced's model input back through ModelSamplingSD3 + the LoRA chain
    /// to the UNETLoader feeding it.
    fn unet_of(wf: &Workflow, sampler: &WorkflowNodeId) -> WorkflowNodeId {
        let msd = upstream(wf, sampler, "model");
        assert_eq!(class_of(wf, &msd), "ModelSamplingSD3");
        let mut m = upstream(wf, &msd, "model");
        while class_of(wf, &m) == "LoraLoaderModelOnly" {
            m = upstream(wf, &m, "model");
        }
        m
    }

    #[test]
    fn video_graph_mirrors_the_two_expert_topology() {
        let schemas = SchemaSet::default();
        let p = video_params();
        let (wf, out, _) = build_video(&p, Some(crate::engine::INPUT_IMAGE_NAME.into()), &schemas);

        assert!(
            !wf.0.values().any(|n| n.class_type == "CheckpointLoaderSimple"),
            "video path must not emit a checkpoint loader"
        );
        assert_eq!(class_of(&wf, &out), "SaveImage");

        // out SaveImage <- ImageFromBatch(-1) <- VAEDecode <- LOW sampler.
        let batch = upstream(&wf, &out, "images");
        assert_eq!(class_of(&wf, &batch), "ImageFromBatch");
        assert_eq!(wf.0[&batch].inputs["batch_index"].as_i64().unwrap(), -1);
        let decode = upstream(&wf, &batch, "image");
        assert_eq!(class_of(&wf, &decode), "VAEDecode");
        let low = upstream(&wf, &decode, "samples");
        assert_eq!(class_of(&wf, &low), "KSamplerAdvanced");
        assert_eq!(wf.0[&low].inputs["add_noise"].as_str().unwrap(), "disable");
        assert_eq!(wf.0[&low].inputs["cfg"].as_f64().unwrap() as f32, 1.0);
        assert_eq!(wf.0[&low].inputs["start_at_step"].as_u64().unwrap(), 4);
        assert_eq!(wf.0[&low].inputs["end_at_step"].as_u64().unwrap(), 8);
        assert_eq!(
            wf.0[&low].inputs["return_with_leftover_noise"].as_str().unwrap(),
            "disable"
        );

        // LOW latent <- HIGH sampler.
        let high = upstream(&wf, &low, "latent_image");
        assert_eq!(class_of(&wf, &high), "KSamplerAdvanced");
        assert_eq!(wf.0[&high].inputs["add_noise"].as_str().unwrap(), "enable");
        assert_eq!(wf.0[&high].inputs["cfg"].as_f64().unwrap() as f32, 2.5);
        assert_eq!(wf.0[&high].inputs["start_at_step"].as_u64().unwrap(), 0);
        assert_eq!(wf.0[&high].inputs["end_at_step"].as_u64().unwrap(), 4);
        assert_eq!(wf.0[&high].inputs["noise_seed"].as_u64().unwrap(), 42);
        assert_eq!(
            wf.0[&high].inputs["return_with_leftover_noise"].as_str().unwrap(),
            "enable"
        );

        // Model chains reach the right expert, each through a shift-5 ModelSamplingSD3.
        assert_eq!(
            wf.0[&upstream(&wf, &high, "model")].inputs["shift"].as_f64().unwrap() as f32,
            5.0
        );
        let unet_high = unet_of(&wf, &high);
        assert_eq!(class_of(&wf, &unet_high), "UNETLoader");
        assert_eq!(
            wf.0[&unet_high].inputs["unet_name"].as_str().unwrap(),
            "Wan/wan2.2_i2v_high_noise_14B_fp8_scaled.safetensors"
        );
        let unet_low = unet_of(&wf, &low);
        assert_eq!(
            wf.0[&unet_low].inputs["unet_name"].as_str().unwrap(),
            "Wan/wan2.2_i2v_low_noise_14B_fp8_scaled.safetensors"
        );

        // Both samplers' conditioning comes from the one WanImageToVideo.
        let wan = upstream(&wf, &high, "positive");
        assert_eq!(class_of(&wf, &wan), "WanImageToVideo");
        assert_eq!(upstream(&wf, &high, "negative"), wan);
        assert_eq!(upstream(&wf, &low, "positive"), wan);
        assert_eq!(upstream(&wf, &low, "negative"), wan);
        // Wan consumes the two encodes, the VAE, and the LoadImage start image.
        assert_eq!(class_of(&wf, &upstream(&wf, &wan, "positive")), "CLIPTextEncode");
        assert_eq!(class_of(&wf, &upstream(&wf, &wan, "negative")), "CLIPTextEncode");
        assert_eq!(class_of(&wf, &upstream(&wf, &wan, "vae")), "VAELoader");
        assert_eq!(class_of(&wf, &upstream(&wf, &wan, "start_image")), "LoadImage");
        // The HIGH sampler's latent seeds from Wan's latent output.
        assert_eq!(upstream(&wf, &high, "latent_image"), wan);
    }

    #[test]
    fn video_rife_and_clean_nodes_appear_with_the_schema() {
        let schemas = schemas_with(
            r#"{
                "RIFE VFI": {"input": {"required": {"frames": ["IMAGE"], "ckpt_name": [["rife49.pth"]], "multiplier": ["INT", {"default": 2}]}}, "output": ["IMAGE"]},
                "VHS_VideoCombine": {"input": {"required": {"images": ["IMAGE"], "frame_rate": ["FLOAT", {"default": 8.0}]}}, "output": []},
                "easy cleanGpuUsed": {"input": {"required": {"anything": ["*"]}}, "output": ["*"]}
            }"#,
        );
        let (wf, _out, report) =
            build_video(&video_params(), Some(crate::engine::INPUT_IMAGE_NAME.into()), &schemas);
        assert!(report.warnings.is_empty(), "unexpected warnings: {:?}", report.warnings);

        let vhs =
            wf.0.iter().find(|(_, n)| n.class_type == "VHS_VideoCombine").expect("no VHS").0;
        assert_eq!(wf.0[vhs].inputs["frame_rate"].as_i64().unwrap(), 32);
        let rife = upstream(&wf, vhs, "images");
        assert_eq!(class_of(&wf, &rife), "RIFE VFI");
        assert_eq!(wf.0[&rife].inputs["multiplier"].as_i64().unwrap(), 2);
        // RIFE frames flow through a GPU-clean node fed by the decode.
        let clean2 = upstream(&wf, &rife, "frames");
        assert_eq!(class_of(&wf, &clean2), "easy cleanGpuUsed");
        assert_eq!(class_of(&wf, &upstream(&wf, &clean2, "anything")), "VAEDecode");
        // A GPU-clean node also sits between the LOW sampler and the decode.
        let decode = upstream(&wf, &clean2, "anything");
        let clean1 = upstream(&wf, &decode, "samples");
        assert_eq!(class_of(&wf, &clean1), "easy cleanGpuUsed");
        assert_eq!(class_of(&wf, &upstream(&wf, &clean1, "anything")), "KSamplerAdvanced");
    }

    #[test]
    fn video_without_optional_nodes_still_emits_vhs_and_no_error() {
        let schemas = SchemaSet::default();
        let (wf, _out, report) =
            build_video(&video_params(), Some(crate::engine::INPUT_IMAGE_NAME.into()), &schemas);
        assert!(!wf.0.values().any(|n| n.class_type == "RIFE VFI"));
        assert!(!wf.0.values().any(|n| n.class_type == "easy cleanGpuUsed"));
        let vhs =
            wf.0.iter().find(|(_, n)| n.class_type == "VHS_VideoCombine").expect("no VHS").0;
        assert_eq!(wf.0[vhs].inputs["frame_rate"].as_i64().unwrap(), 16);
        // VHS reads the decode directly when RIFE is absent.
        assert_eq!(class_of(&wf, &upstream(&wf, vhs, "images")), "VAEDecode");
        // The defaults request RIFE + clean, so their absence is reported.
        assert_eq!(report.warnings.len(), 2);
    }

    #[test]
    fn video_skips_empty_and_zero_strength_loras() {
        let schemas = SchemaSet::default();
        let mut p = video_params();
        p.video.loras_high = vec![
            ActiveLora {
                file: "Wan/keep.safetensors".into(),
                strength_model: 0.5,
                strength_clip: 0.5,
                injected: String::new(),
                model_only: true,
            },
            ActiveLora {
                file: "Wan/zero.safetensors".into(),
                strength_model: 0.0,
                strength_clip: 0.0,
                injected: String::new(),
                model_only: true,
            },
            ActiveLora {
                file: "   ".into(),
                strength_model: 1.0,
                strength_clip: 1.0,
                injected: String::new(),
                model_only: true,
            },
        ];
        let (wf, _out, _) = build_video(&p, None, &schemas);
        let loras: Vec<_> =
            wf.0.values().filter(|n| n.class_type == "LoraLoaderModelOnly").collect();
        // Only the one non-empty, non-zero high LoRA (plus the two default low LoRAs) survive.
        let names: Vec<&str> =
            loras.iter().map(|n| n.inputs["lora_name"].as_str().unwrap()).collect();
        assert!(names.contains(&"Wan/keep.safetensors"));
        assert!(!names.iter().any(|n| n.trim().is_empty() || *n == "Wan/zero.safetensors"));
    }

    #[test]
    fn snap_wan_length_rounds_to_4n_plus_1() {
        assert_eq!(snap_wan_length(80), 81);
        assert_eq!(snap_wan_length(81), 81);
        assert_eq!(snap_wan_length(82), 81);
        assert_eq!(snap_wan_length(83), 85);
        assert_eq!(snap_wan_length(0), 1);
        assert_eq!(snap_wan_length(1), 1);
        assert_eq!(snap_wan_length(5), 5);
        assert_eq!(snap_wan_length(8), 9);
        assert_eq!((snap_wan_length(81) - 1) % 4, 0);
    }

    #[test]
    fn text_to_video_omits_the_load_image() {
        let schemas = SchemaSet::default();
        let (wf, _out, _) = build_video(&video_params(), None, &schemas);
        assert!(!wf.0.values().any(|n| n.class_type == "LoadImage"));
        let wan = wf.0.values().find(|n| n.class_type == "WanImageToVideo").expect("no Wan");
        assert!(!wan.inputs.contains_key("start_image"));
    }
}
