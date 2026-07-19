//! Local NPU generate path: CLIP CPU + UNet/VAE HTP via `local_sd`, emitting [`engine::Msg`].
//! Heavy assets (CLIP, context binaries, QNN libs) are cached for the process after the first load.

use crate::engine::Msg;
use crate::logger::Logger;
use crate::types::Params;
use egui::Context;
use local_sd::{
    prepare_htp_env, set_htp_performance_mode, text2img, Backend, ClipTextEncoder, ClipTokenizer,
    QnnContext, QnnSystem, Sampler, Text2ImgParams,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};

/// Fixed paths under the app external files `qnn/` dir + native lib dir.
#[derive(Clone, Debug)]
pub struct LocalPaths {
    pub lib_dir: PathBuf,
    pub model_dir: PathBuf,
}

impl LocalPaths {
    fn cache_key(&self) -> String {
        format!("{}|{}", self.lib_dir.display(), self.model_dir.display())
    }
    fn system_lib(&self) -> PathBuf {
        self.lib_dir.join("libQnnSystem.so")
    }
    fn backend_lib(&self) -> PathBuf {
        self.lib_dir.join("libQnnHtp.so")
    }
    fn unet(&self) -> PathBuf {
        self.model_dir.join("unet.bin")
    }
    fn vae(&self) -> PathBuf {
        self.model_dir.join("vae_decoder.bin")
    }
    fn tokenizer(&self) -> PathBuf {
        self.model_dir.join("tokenizer.json")
    }
    fn clip(&self) -> PathBuf {
        self.model_dir.join("clip.safetensors")
    }
}

struct AssetCache {
    key: String,
    tokenizer: ClipTokenizer,
    clip: ClipTextEncoder,
    system: QnnSystem,
    backend: Backend,
    unet_bytes: Vec<u8>,
    vae_bytes: Vec<u8>,
}

fn cache_slot() -> &'static Mutex<Option<AssetCache>> {
    static SLOT: OnceLock<Mutex<Option<AssetCache>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

fn run_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn load_assets(paths: &LocalPaths, log: &Logger) -> Result<AssetCache, String> {
    log.info("local-npu: loading assets (first run or path change)");
    prepare_htp_env(&paths.lib_dir);
    let tokenizer =
        ClipTokenizer::from_file(paths.tokenizer()).map_err(|e| format!("tokenizer: {e}"))?;
    let clip = ClipTextEncoder::from_safetensors(paths.clip()).map_err(|e| format!("clip: {e}"))?;
    let system = QnnSystem::load(paths.system_lib()).map_err(|e| format!("QnnSystem: {e}"))?;
    let backend = Backend::load(paths.backend_lib()).map_err(|e| format!("Backend: {e}"))?;
    let unet_bytes = std::fs::read(paths.unet()).map_err(|e| format!("unet.bin: {e}"))?;
    let vae_bytes = std::fs::read(paths.vae()).map_err(|e| format!("vae_decoder.bin: {e}"))?;
    log.info(format!(
        "local-npu: cached unet={}MB vae={}MB",
        unet_bytes.len() / (1024 * 1024),
        vae_bytes.len() / (1024 * 1024)
    ));
    Ok(AssetCache {
        key: paths.cache_key(),
        tokenizer,
        clip,
        system,
        backend,
        unet_bytes,
        vae_bytes,
    })
}

/// Map Create-tab sampler name → local_sd sampler. Returns (mapped, fallback_from) when remapped.
fn map_sampler(name: &str) -> (Sampler, Option<String>) {
    let n = name.to_ascii_lowercase().replace([' ', '-'], "_");
    if n.contains("dpmpp_2m") || n.contains("dpm++_2m") || n.contains("dpmpp_2m_karras") {
        return (Sampler::DpmPP2mKarras, None);
    }
    if n.contains("euler") && (n.contains("ancestral") || n.ends_with("_a") || n.contains("euler_a")) {
        return (Sampler::EulerAncestral, None);
    }
    if n == "euler" || n == "euler_ancestral" {
        return (Sampler::EulerAncestral, None);
    }
    (Sampler::EulerAncestral, Some(name.to_string()))
}

fn rgb_to_color_image(img: &local_sd::Image) -> egui::ColorImage {
    let mut rgba = Vec::with_capacity(img.rgb.len() / 3 * 4);
    for c in img.rgb.chunks_exact(3) {
        rgba.extend_from_slice(&[c[0], c[1], c[2], 255]);
    }
    egui::ColorImage::from_rgba_unmultiplied([img.width as usize, img.height as usize], &rgba)
}

/// Drop the in-process asset cache (frees ~1GB host RAM). Next generate reloads from disk.
pub fn drop_cache() {
    if let Ok(mut g) = cache_slot().lock() {
        *g = None;
    }
}

/// Blocking text2img; sends Progress / Preview / Result / Done or GenError on `tx`.
pub fn run(
    paths: LocalPaths,
    params: Params,
    tx: Sender<Msg>,
    ctx: Context,
    log: Logger,
    cancel: Arc<AtomicBool>,
) {
    let send = |m: Msg| {
        let _ = tx.send(m);
        ctx.request_repaint();
    };

    if cancel.load(Ordering::Relaxed) {
        send(Msg::Cancelled);
        return;
    }

    // One HTP session at a time (context create is not re-entrant).
    let _gate = match run_lock().lock() {
        Ok(g) => g,
        Err(_) => {
            send(Msg::GenError("local-npu: internal lock poisoned".into()));
            return;
        }
    };

    let (sampler, sampler_fallback) = map_sampler(&params.sampler);
    log.info(format!(
        "local-npu: generate steps={} cfg={} seed={} sampler={}→{:?}",
        params.steps, params.cfg, params.seed, params.sampler, sampler
    ));
    send(Msg::Queued);
    if let Some(from) = &sampler_fallback {
        let note = format!("Local NPU: sampler '{from}' → Euler a (only Euler a / DPM++ 2M)");
        log.warn(note.clone());
        send(Msg::Status(note));
    }
    if params.width != 512 || params.height != 512 {
        let note = format!(
            "Local NPU: {}x{} → 512x512 (fixed latent)",
            params.width, params.height
        );
        log.warn(note.clone());
        send(Msg::Status(note));
    }

    let result = (|| -> Result<(), String> {
        send(Msg::Status("Local NPU: loading…".into()));
        let key = paths.cache_key();
        {
            let mut slot = cache_slot().lock().map_err(|_| "cache lock poisoned".to_string())?;
            let need = slot.as_ref().map(|c| c.key != key).unwrap_or(true);
            if need {
                *slot = Some(load_assets(&paths, &log)?);
            } else {
                log.info("local-npu: using cached assets");
            }
        }

        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }

        let mut slot = cache_slot().lock().map_err(|_| "cache lock poisoned".to_string())?;
        let cache = slot.as_mut().ok_or("cache empty after load")?;

        send(Msg::Status("Local NPU: creating HTP contexts…".into()));
        let unet = QnnContext::from_binary(&cache.backend, &cache.system, &cache.unet_bytes)
            .map_err(|e| format!("unet: {e}"))?;
        let vae = QnnContext::from_binary(&cache.backend, &cache.system, &cache.vae_bytes)
            .map_err(|e| format!("vae: {e}"))?;
        let _ = set_htp_performance_mode(&cache.backend);

        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }

        let t2i = Text2ImgParams {
            steps: params.steps.max(1) as usize,
            guidance_scale: params.cfg,
            seed: params.seed,
            sampler,
            preview_every: Some(2),
            ..Text2ImgParams::default()
        };
        let prompt = params.combined_positive();
        let negative = params.negative.clone();
        send(Msg::Status("Local NPU: sampling…".into()));

        let image = text2img(
            &cache.tokenizer,
            &cache.clip,
            &unet,
            &vae,
            &prompt,
            &negative,
            &t2i,
            |step, total, preview| {
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                let _ = tx.send(Msg::Progress { value: step as u32, max: total as u32 });
                if let Some(p) = preview {
                    let _ = tx.send(Msg::Preview(rgb_to_color_image(p)));
                }
                ctx.request_repaint();
            },
            Some(&cancel),
        )
        .map_err(|e| format!("text2img: {e}"))?;

        // Drop HTP contexts before releasing the cache lock (free NSP memory between runs).
        drop(vae);
        drop(unet);
        drop(slot);

        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }

        let png = image.to_png().map_err(|e| format!("png: {e}"))?;
        let ci = rgb_to_color_image(&image);
        log.info(format!(
            "local-npu: done {}x{} ({} bytes png)",
            image.width,
            image.height,
            png.len()
        ));
        send(Msg::Result { image: ci, bytes: png });
        Ok(())
    })();

    match result {
        Ok(()) => send(Msg::Done),
        Err(e) if e == "cancelled" || cancel.load(Ordering::Relaxed) => send(Msg::Cancelled),
        Err(e) => {
            log.error(format!("local-npu: {e}"));
            send(Msg::GenError(e));
        }
    }
}
