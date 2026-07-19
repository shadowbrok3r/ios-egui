//! D2 device smoke: full text2img (CLIP CPU + UNet/VAE on HTP) and PNG write.
//! Never panics; failures land in [`Text2ImgReport::error`].

use crate::clip::ClipTextEncoder;
use crate::pipeline::{text2img, Text2ImgParams};
use crate::scheduler::Sampler;
use crate::tokenizer::ClipTokenizer;
use qnn_rs::{prepare_htp_env, set_htp_performance_mode, Backend, Context, QnnSystem};
use std::path::PathBuf;
use std::time::Instant;

/// Paths and generation knobs for [`device_text2img`].
#[derive(Clone, Debug)]
pub struct Text2ImgConfig {
    pub system_lib: PathBuf,
    pub backend_lib: PathBuf,
    pub skel_dir: Option<PathBuf>,
    pub unet_bin: PathBuf,
    pub vae_decoder_bin: PathBuf,
    pub tokenizer: PathBuf,
    pub clip_weights: PathBuf,
    /// Where to write the PNG (parent dirs must exist).
    pub output_png: PathBuf,
    pub prompt: String,
    pub negative: String,
    pub steps: usize,
    pub guidance_scale: f32,
    pub seed: u64,
    pub set_performance_mode: bool,
}

impl Default for Text2ImgConfig {
    fn default() -> Self {
        Self {
            system_lib: PathBuf::new(),
            backend_lib: PathBuf::new(),
            skel_dir: None,
            unet_bin: PathBuf::new(),
            vae_decoder_bin: PathBuf::new(),
            tokenizer: PathBuf::new(),
            clip_weights: PathBuf::new(),
            output_png: PathBuf::new(),
            prompt: "a cute anime cat, masterpiece, best quality".into(),
            negative: "lowres, bad anatomy, bad hands, text, error, worst quality".into(),
            steps: 8,
            guidance_scale: 7.5,
            seed: 42,
            set_performance_mode: true,
        }
    }
}

/// Result of a D2 text2img smoke run.
#[derive(Clone, Debug)]
pub struct Text2ImgReport {
    pub ok: bool,
    pub log: Vec<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub png_path: Option<String>,
    pub png_bytes: Option<usize>,
    pub total_ms: Option<f64>,
    pub steps_done: Option<usize>,
    pub error: Option<String>,
}

impl Text2ImgReport {
    fn blank() -> Self {
        Self {
            ok: false,
            log: Vec::new(),
            width: None,
            height: None,
            png_path: None,
            png_bytes: None,
            total_ms: None,
            steps_done: None,
            error: None,
        }
    }

    /// Human-readable diagnostic dump.
    pub fn pretty(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!("local-sd text2img self-test: {}\n", if self.ok { "OK" } else { "FAILED" }));
        if let (Some(w), Some(h)) = (self.width, self.height) {
            s.push_str(&format!("image: {w}x{h}\n"));
        }
        if let Some(p) = &self.png_path {
            s.push_str(&format!("png: {p}\n"));
        }
        if let Some(n) = self.png_bytes {
            s.push_str(&format!("png bytes: {n}\n"));
        }
        if let Some(ms) = self.total_ms {
            s.push_str(&format!("total: {ms:.1} ms\n"));
        }
        if let Some(n) = self.steps_done {
            s.push_str(&format!("steps: {n}\n"));
        }
        if let Some(e) = &self.error {
            s.push_str(&format!("error: {e}\n"));
        }
        for line in &self.log {
            s.push_str(&format!("  · {line}\n"));
        }
        s
    }
}

/// Load libs + models, run text2img, write PNG. Never panics.
pub fn device_text2img(cfg: Text2ImgConfig) -> Text2ImgReport {
    let mut report = Text2ImgReport::blank();
    match run(&cfg, &mut report) {
        Ok(()) => report.ok = true,
        Err(e) => {
            report.ok = false;
            report.error = Some(e);
        }
    }
    report
}

fn run(cfg: &Text2ImgConfig, report: &mut Text2ImgReport) -> Result<(), String> {
    if let Some(skel) = &cfg.skel_dir {
        prepare_htp_env(skel);
        report.log.push(format!("prepare_htp_env({})", skel.display()));
    }

    let t0 = Instant::now();
    let tokenizer = ClipTokenizer::from_file(&cfg.tokenizer).map_err(|e| format!("tokenizer: {e}"))?;
    report.log.push(format!("tokenizer loaded ({})", cfg.tokenizer.display()));

    let clip = ClipTextEncoder::from_safetensors(&cfg.clip_weights).map_err(|e| format!("clip: {e}"))?;
    report.log.push(format!("clip loaded ({})", cfg.clip_weights.display()));

    let system = QnnSystem::load(&cfg.system_lib).map_err(|e| format!("QnnSystem::load: {e}"))?;
    report.log.push("system loaded".into());
    let backend = Backend::load(&cfg.backend_lib).map_err(|e| format!("Backend::load: {e}"))?;
    report.log.push("backend loaded".into());

    let unet_bytes = std::fs::read(&cfg.unet_bin).map_err(|e| format!("read unet: {e}"))?;
    report.log.push(format!("read unet ({} bytes)", unet_bytes.len()));
    let unet = Context::from_binary(&backend, &system, &unet_bytes).map_err(|e| format!("unet context: {e}"))?;
    drop(unet_bytes);
    report.log.push("unet context created".into());

    let vae_bytes = std::fs::read(&cfg.vae_decoder_bin).map_err(|e| format!("read vae: {e}"))?;
    report.log.push(format!("read vae_decoder ({} bytes)", vae_bytes.len()));
    let vae = Context::from_binary(&backend, &system, &vae_bytes).map_err(|e| format!("vae context: {e}"))?;
    drop(vae_bytes);
    report.log.push("vae context created".into());

    if cfg.set_performance_mode {
        set_htp_performance_mode(&backend).map_err(|e| format!("set_htp_performance_mode: {e}"))?;
        report.log.push("HTP burst mode set".into());
    }

    let params = Text2ImgParams {
        steps: cfg.steps.max(1),
        guidance_scale: cfg.guidance_scale,
        seed: cfg.seed,
        sampler: Sampler::EulerAncestral,
        preview_every: None,
        ..Text2ImgParams::default()
    };
    report.log.push(format!(
        "text2img steps={} cfg={} seed={} prompt={:?}",
        params.steps, params.guidance_scale, params.seed, cfg.prompt
    ));

    let mut last_step = 0usize;
    let image = text2img(
        &tokenizer,
        &clip,
        &unet,
        &vae,
        &cfg.prompt,
        &cfg.negative,
        &params,
        |step, total, _| {
            last_step = step;
            if step == 1 || step == total || step % 2 == 0 {
                log::info!("local-sd text2img step {step}/{total}");
            }
        },
        None,
    )
    .map_err(|e| format!("text2img: {e}"))?;
    report.steps_done = Some(last_step);
    report.width = Some(image.width);
    report.height = Some(image.height);
    report.log.push(format!("decoded {}x{}", image.width, image.height));

    let png = image.to_png().map_err(|e| format!("png encode: {e}"))?;
    if let Some(parent) = cfg.output_png.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    std::fs::write(&cfg.output_png, &png).map_err(|e| format!("write {}: {e}", cfg.output_png.display()))?;
    report.png_path = Some(cfg.output_png.display().to_string());
    report.png_bytes = Some(png.len());
    report.total_ms = Some(t0.elapsed().as_secs_f64() * 1000.0);
    report.log.push(format!("wrote {} ({} bytes)", cfg.output_png.display(), png.len()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bogus_paths_fail_without_panicking() {
        let report = device_text2img(Text2ImgConfig {
            system_lib: "/nonexistent/libQnnSystem.so".into(),
            backend_lib: "/nonexistent/libQnnHtp.so".into(),
            unet_bin: "/nonexistent/unet.bin".into(),
            vae_decoder_bin: "/nonexistent/vae.bin".into(),
            tokenizer: "/nonexistent/tokenizer.json".into(),
            clip_weights: "/nonexistent/clip.safetensors".into(),
            output_png: "/tmp/local-sd-d2-bogus.png".into(),
            ..Text2ImgConfig::default()
        });
        assert!(!report.ok);
        assert!(report.error.is_some());
        assert!(report.pretty().contains("FAILED"));
    }
}
