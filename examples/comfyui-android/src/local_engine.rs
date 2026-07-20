//! Local NPU generate path, emitting [`engine::Msg`]: SD1.5 (CLIP CPU + UNet/VAE HTP via
//! `local_sd`) or Anima (DiT packs via `local_anima`). Each backend has its own process-wide
//! asset cache; only one is resident at a time.

use crate::engine::Msg;
use crate::logger::Logger;
use crate::types::{LocalBackend, Params};
use egui::Context;
use local_anima::{AnimaPack, AnimaParams, Session};
use local_sd::{
    prepare_htp_env, set_htp_performance_mode, text2img, Backend, ClipTextEncoder, ClipTokenizer,
    QnnContext, QnnSystem, Sampler, Text2ImgParams,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

/// Anima's fixed output size; the pack's graphs are built for this latent grid only.
pub const ANIMA_SIZE: u32 = 1024;

/// The SD1.5 pack marker file, mirroring `local_anima::pack::MARKER` for Anima.
const SD15_MARKER: &str = "unet.bin";

/// Selected model pack + native lib dir for one local generate.
#[derive(Clone, Debug)]
pub struct LocalPaths {
    pub lib_dir: PathBuf,
    pub model_dir: PathBuf,
    pub backend: LocalBackend,
}

/// A model pack directory found under the app external files dir.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackEntry {
    /// Subdirectory name, e.g. `qnn` or `anima_nova`.
    pub name: String,
    pub dir: PathBuf,
    pub backend: LocalBackend,
}

impl PackEntry {
    /// `name (SD1.5)` / `name (Anima)`, for the Settings picker.
    pub fn label(&self) -> String {
        format!("{} ({})", self.name, self.backend.label())
    }
}

/// Classify one directory: the `ANIMA` marker wins, else a bare `unet.bin` is an SD1.5 dir.
pub fn classify_pack(dir: &Path) -> Option<LocalBackend> {
    if AnimaPack::is_anima_pack(dir) {
        return Some(LocalBackend::Anima);
    }
    dir.join(SD15_MARKER).is_file().then_some(LocalBackend::Sd15)
}

/// Usable packs directly under `root`, sorted by name. Unreadable roots yield an empty list.
pub fn scan_packs(root: &Path) -> Vec<PackEntry> {
    let Ok(rd) = std::fs::read_dir(root) else { return Vec::new() };
    let mut out: Vec<PackEntry> = rd
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let dir = e.path();
            let name = dir.file_name()?.to_str()?.to_string();
            Some(PackEntry { backend: classify_pack(&dir)?, name, dir })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Merge packs from several roots; first-seen dir wins.
pub fn scan_packs_many(roots: &[&Path]) -> Vec<PackEntry> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for root in roots {
        for p in scan_packs(root) {
            if seen.insert(p.dir.clone()) {
                out.push(p);
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Create-tab settings a local pack recommends: fixed latent size plus sampler defaults.
pub struct LocalDefaults {
    pub width: u32,
    pub height: u32,
    pub steps: u32,
    pub cfg: f32,
    pub scheduler: String,
}

/// Recommended defaults for `entry`: Anima reads its `config.json`, SD1.5 is fixed 512² at the
/// `local_sd` pipeline defaults.
pub fn local_defaults(entry: &PackEntry) -> LocalDefaults {
    match entry.backend {
        LocalBackend::Anima => {
            let c = AnimaPack::open(&entry.dir).map(|p| p.config().clone()).unwrap_or_default();
            LocalDefaults {
                width: c.default_width as u32,
                height: c.default_height as u32,
                steps: c.default_steps as u32,
                cfg: c.default_cfg,
                scheduler: c.default_scheduler,
            }
        }
        LocalBackend::Sd15 => {
            let d = Text2ImgParams::default();
            LocalDefaults {
                width: 512,
                height: 512,
                steps: d.steps as u32,
                cfg: d.guidance_scale,
                scheduler: "normal".into(),
            }
        }
    }
}

/// The `backend` pack called `name`, else the first pack of `backend`. Never crosses backends.
pub fn pick_pack<'a>(packs: &'a [PackEntry], name: &str, backend: LocalBackend) -> Option<&'a PackEntry> {
    packs
        .iter()
        .find(|p| p.backend == backend && p.name == name)
        .or_else(|| packs.iter().find(|p| p.backend == backend))
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

/// Anima's cached handles. The DiT/VAE context binaries are mmapped per run inside
/// `local_anima::text2img`, so only the dlopened libs and the pack header live here.
struct AnimaCache {
    key: String,
    pack: AnimaPack,
    system: QnnSystem,
    backend: Backend,
}

fn cache_slot() -> &'static Mutex<Option<AssetCache>> {
    static SLOT: OnceLock<Mutex<Option<AssetCache>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

fn anima_slot() -> &'static Mutex<Option<AnimaCache>> {
    static SLOT: OnceLock<Mutex<Option<AnimaCache>>> = OnceLock::new();
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

fn rgb_to_color_image(width: u32, height: u32, rgb: &[u8]) -> egui::ColorImage {
    let mut rgba = Vec::with_capacity(rgb.len() / 3 * 4);
    for c in rgb.chunks_exact(3) {
        rgba.extend_from_slice(&[c[0], c[1], c[2], 255]);
    }
    egui::ColorImage::from_rgba_unmultiplied([width as usize, height as usize], &rgba)
}

/// Drop both in-process asset caches (frees ~1GB host RAM). Next generate reloads from disk.
pub fn drop_cache() {
    drop_sd_cache();
    drop_anima_cache();
}

fn drop_sd_cache() {
    if let Ok(mut g) = cache_slot().lock() {
        *g = None;
    }
}

fn drop_anima_cache() {
    if let Ok(mut g) = anima_slot().lock() {
        *g = None;
    }
}

/// Blocking text2img on the selected backend; sends Progress / Preview / Result / Done or GenError.
pub fn run(
    paths: LocalPaths,
    params: Params,
    tx: Sender<Msg>,
    ctx: Context,
    log: Logger,
    cancel: Arc<AtomicBool>,
) {
    match paths.backend {
        LocalBackend::Sd15 => run_sd15(paths, params, tx, ctx, log, cancel),
        LocalBackend::Anima => run_anima(paths, params, tx, ctx, log, cancel),
    }
}

fn run_sd15(
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
    drop_anima_cache();

    let (sampler, sampler_fallback) = map_sampler(&params.sampler);
    log.info(format!(
        "local-npu: generate steps={} cfg={} seed={} sampler={}→{:?}",
        params.steps, params.cfg, params.seed, params.sampler, sampler
    ));
    send(Msg::Queued);
    if let Some(from) = &sampler_fallback {
        let note = format!("Local NPU: sampler '{from}' -> Euler a (only Euler a / DPM++ 2M)");
        log.warn(note.clone());
        send(Msg::Status(note));
    }
    if params.width != 512 || params.height != 512 {
        let note = format!(
            "Local NPU: {}x{} -> 512x512 (fixed latent)",
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
                    let _ = tx.send(Msg::Preview(rgb_to_color_image(p.width, p.height, &p.rgb)));
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
        let ci = rgb_to_color_image(image.width, image.height, &image.rgb);
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

/// Open the pack and dlopen the QNN libs for `paths`.
fn load_anima(paths: &LocalPaths, log: &Logger) -> Result<AnimaCache, String> {
    log.info(format!("local-anima: opening pack {}", paths.model_dir.display()));
    prepare_htp_env(&paths.lib_dir);
    let pack = AnimaPack::open(&paths.model_dir).map_err(|e| format!("pack: {e}"))?;
    let system = QnnSystem::load(paths.system_lib()).map_err(|e| format!("QnnSystem: {e}"))?;
    let backend = Backend::load(paths.backend_lib()).map_err(|e| format!("Backend: {e}"))?;
    let c = pack.config();
    log.info(format!(
        "local-anima: pack ok {}x{} steps={} cfg={} scheduler={}",
        c.default_width, c.default_height, c.default_steps, c.default_cfg, c.default_scheduler
    ));
    Ok(AnimaCache { key: paths.cache_key(), pack, system, backend })
}

/// Pack defaults (size, cfg, scheduler) with the Create-tab steps / seed / negative applied.
fn anima_params(pack: &AnimaPack, params: &Params) -> AnimaParams {
    let mut p = AnimaParams::from_pack(pack);
    p.steps = params.steps.max(1) as usize;
    p.seed = params.seed;
    p.negative = params.negative.clone();
    p
}

fn run_anima(
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

    let _gate = match run_lock().lock() {
        Ok(g) => g,
        Err(_) => {
            send(Msg::GenError("local-anima: internal lock poisoned".into()));
            return;
        }
    };
    drop_sd_cache();

    log.info(format!(
        "local-anima: generate steps={} seed={} pack={}",
        params.steps,
        params.seed,
        paths.model_dir.display()
    ));
    send(Msg::Queued);
    if params.width != ANIMA_SIZE || params.height != ANIMA_SIZE {
        let note = format!("Anima: {}x{} -> {ANIMA_SIZE}² (fixed latent)", params.width, params.height);
        log.warn(note.clone());
        send(Msg::Status(note));
    }

    let result = (|| -> Result<(), String> {
        send(Msg::Status("Anima: loading pack…".into()));
        let key = paths.cache_key();
        {
            let mut slot = anima_slot().lock().map_err(|_| "cache lock poisoned".to_string())?;
            if slot.as_ref().map(|c| c.key != key).unwrap_or(true) {
                // Free the previous pack's handles before opening the new one.
                *slot = None;
                *slot = Some(load_anima(&paths, &log)?);
            } else {
                log.info("local-anima: using cached pack");
            }
        }

        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }

        let prompt = params.combined_positive();
        let image = {
            let slot = anima_slot().lock().map_err(|_| "cache lock poisoned".to_string())?;
            let cache = slot.as_ref().ok_or("cache empty after load")?;
            let aparams = anima_params(&cache.pack, &params);
            send(Msg::Status("Anima: creating HTP session…".into()));
            let session = Session::new(&cache.backend).map_err(|e| format!("session: {e}"))?;
            // Performance mode only takes after Session::new; before it the DSP appears to crash.
            if let Err(e) = session.set_htp_performance_mode() {
                log.warn(format!("local-anima: performance mode unavailable: {e}"));
            }
            send(Msg::Status("Anima: sampling…".into()));
            local_anima::text2img_cancellable(
                &cache.pack,
                &session,
                &cache.system,
                &prompt,
                &aparams,
                |step, total| {
                    let _ = tx.send(Msg::Progress { value: step as u32, max: total as u32 });
                    ctx.request_repaint();
                },
                Some(&cancel),
            )
            .map_err(|e| format!("text2img: {e}"))?
        };

        if cancel.load(Ordering::Relaxed) {
            return Err("cancelled".into());
        }

        let png = image.to_png().map_err(|e| format!("png: {e}"))?;
        let ci = rgb_to_color_image(image.width, image.height, &image.rgb);
        log.info(format!(
            "local-anima: done {}x{} ({} bytes png)",
            image.width,
            image.height,
            png.len()
        ));
        send(Msg::Result { image: ci, bytes: png });
        Ok(())
    })();

    match result {
        Ok(()) => send(Msg::Done),
        Err(e) if e.ends_with("cancelled") || cancel.load(Ordering::Relaxed) => send(Msg::Cancelled),
        Err(e) => {
            log.error(format!("local-anima: {e}"));
            send(Msg::GenError(e));
        }
    }
}

/// D3 Anima smoke result: one `key = value` line per stage.
#[derive(Clone, Debug)]
pub struct AnimaSmoke {
    pub ok: bool,
    pub lines: Vec<String>,
}

impl AnimaSmoke {
    pub fn pretty(&self) -> String {
        self.lines.join("\n")
    }
}

fn kv(key: &str, value: impl AsRef<str>) -> String {
    format!("{key:<9}= {}", value.as_ref())
}

/// Open `pack_dir`, run a short generation, and write `d3-smoke.png` into the pack dir.
pub fn anima_smoke(lib_dir: PathBuf, pack_dir: PathBuf, steps: usize, prompt: String) -> AnimaSmoke {
    let _gate = run_lock().lock();
    drop_cache();
    let started = Instant::now();
    let out_png = pack_dir.join("d3-smoke.png");
    let mut lines = vec![
        kv("pack", pack_dir.display().to_string()),
        kv("libs", lib_dir.display().to_string()),
        kv("prompt", &prompt),
        kv("steps", steps.to_string()),
        kv("output", out_png.display().to_string()),
    ];
    let res = anima_smoke_inner(&lib_dir, &pack_dir, steps, &prompt, &out_png, &mut lines);
    if let Err(e) = &res {
        lines.push(kv("error", e));
    }
    lines.push(kv("total", format!("{:.2}s", started.elapsed().as_secs_f32())));
    lines.push(kv("result", if res.is_ok() { "PASS" } else { "FAIL" }));
    AnimaSmoke { ok: res.is_ok(), lines }
}

fn anima_smoke_inner(
    lib_dir: &Path,
    pack_dir: &Path,
    steps: usize,
    prompt: &str,
    out_png: &Path,
    lines: &mut Vec<String>,
) -> Result<(), String> {
    let t = Instant::now();
    prepare_htp_env(lib_dir);
    let pack = AnimaPack::open(pack_dir).map_err(|e| format!("pack open: {e}"))?;
    lines.push(kv("open", format!("{:.2}s", t.elapsed().as_secs_f32())));
    let c = pack.config();
    lines.push(kv(
        "config",
        format!(
            "{}x{} steps={} cfg={} sched={}",
            c.default_width, c.default_height, c.default_steps, c.default_cfg, c.default_scheduler
        ),
    ));

    let t = Instant::now();
    let system = QnnSystem::load(lib_dir.join("libQnnSystem.so")).map_err(|e| format!("QnnSystem: {e}"))?;
    let backend = Backend::load(lib_dir.join("libQnnHtp.so")).map_err(|e| format!("Backend: {e}"))?;
    let session = Session::new(&backend).map_err(|e| format!("session: {e}"))?;
    if let Err(e) = session.set_htp_performance_mode() {
        lines.push(kv("perf", format!("unavailable: {e}")));
    }
    lines.push(kv("session", format!("{:.2}s", t.elapsed().as_secs_f32())));

    let mut params = AnimaParams::from_pack(&pack);
    params.steps = steps.max(1);
    params.seed = 0;

    let t = Instant::now();
    let mut per_step: Vec<String> = Vec::new();
    let mut last = Instant::now();
    let image = local_anima::text2img(&pack, &session, &system, prompt, &params, |i, n| {
        let dt = last.elapsed().as_secs_f32();
        last = Instant::now();
        log::info!("D3-ANIMA step {i}/{n} {dt:.2}s");
        per_step.push(format!("{dt:.2}s"));
    })
    .map_err(|e| format!("text2img: {e}"))?;
    lines.push(kv("generate", format!("{:.2}s", t.elapsed().as_secs_f32())));
    lines.push(kv("per-step", per_step.join(" ")));
    lines.push(kv("image", image_stats(&image)));

    let t = Instant::now();
    let png = image.to_png().map_err(|e| format!("png: {e}"))?;
    std::fs::write(out_png, &png).map_err(|e| format!("write {}: {e}", out_png.display()))?;
    lines.push(kv("png", format!("{} bytes in {:.2}s", png.len(), t.elapsed().as_secs_f32())));
    Ok(())
}

/// The first subdirectory under `root` carrying the WD14 tagger marker, if any. WD14 packs coexist
/// with SD1.5/Anima generate packs (a different kind), so this scans independently of `classify_pack`.
pub fn find_wd14_pack(root: &Path) -> Option<PathBuf> {
    std::fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|d| d.is_dir())
        .find(|d| local_wd14::Wd14Pack::is_wd14_pack(d))
}

/// The first WD14 pack under any of `roots`.
pub fn find_wd14_pack_many(roots: &[&Path]) -> Option<PathBuf> {
    roots.iter().find_map(|r| find_wd14_pack(r))
}

fn find_clip_pack(root: &Path) -> Option<PathBuf> {
    std::fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|d| d.is_dir())
        .find(|d| local_clip::ClipPack::is_clip_pack(d))
}

/// The first CLIP pack under any of `roots`.
pub fn find_clip_pack_many(roots: &[&Path]) -> Option<PathBuf> {
    roots.iter().find_map(|r| find_clip_pack(r))
}

/// The prompt-Rewriter pack marker file; the pack may not exist yet on any device.
const RWTR_MARKER: &str = "RWTR";

/// The first subdirectory under `root` carrying the Rewriter marker, if any.
pub fn find_rewriter_pack(root: &Path) -> Option<PathBuf> {
    std::fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|d| d.is_dir())
        .find(|d| d.join(RWTR_MARKER).is_file())
}

/// The first Rewriter pack under any of `roots`.
pub fn find_rewriter_pack_many(roots: &[&Path]) -> Option<PathBuf> {
    roots.iter().find_map(|r| find_rewriter_pack(r))
}

/// Newest modification time among `dir`'s direct entries, falling back to the dir's own mtime.
pub fn dir_newest_mtime(dir: &Path) -> Option<std::time::SystemTime> {
    let mut newest = std::fs::metadata(dir).ok()?.modified().ok();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if let Ok(m) = e.metadata().and_then(|md| md.modified()) {
                newest = Some(newest.map_or(m, |n| n.max(m)));
            }
        }
    }
    newest
}

/// Short relative age from a second count: "just now", "5m ago", "3h ago", "2d ago", "4w ago".
pub fn humanize_ago(secs: u64) -> String {
    const MIN: u64 = 60;
    const HOUR: u64 = 60 * MIN;
    const DAY: u64 = 24 * HOUR;
    const WEEK: u64 = 7 * DAY;
    const YEAR: u64 = 365 * DAY;
    if secs < MIN {
        "just now".into()
    } else if secs < HOUR {
        format!("{}m ago", secs / MIN)
    } else if secs < DAY {
        format!("{}h ago", secs / HOUR)
    } else if secs < WEEK {
        format!("{}d ago", secs / DAY)
    } else if secs < YEAR {
        format!("{}w ago", secs / WEEK)
    } else {
        format!("{}y ago", secs / YEAR)
    }
}

/// Embed encoded image bytes with the CLIP pack; blocking, L2-normalized embedding plus the
/// aesthetic score when the pack ships a head. Serialized with generation like `read_tags`.
pub fn embed_clip(
    lib_dir: PathBuf,
    pack_dir: PathBuf,
    image: Vec<u8>,
) -> Result<(Vec<f32>, Option<f32>), String> {
    let _gate = run_lock().lock();
    drop_cache();
    let t = Instant::now();
    local_clip::prepare_htp_env(&lib_dir);
    let pack = local_clip::ClipPack::open(&pack_dir).map_err(|e| format!("pack: {e}"))?;
    let system = local_clip::QnnSystem::load(lib_dir.join("libQnnSystem.so"))
        .map_err(|e| format!("QnnSystem: {e}"))?;
    let backend =
        local_clip::Backend::load(lib_dir.join("libQnnHtp.so")).map_err(|e| format!("Backend: {e}"))?;
    let session = local_clip::Session::new(&backend).map_err(|e| format!("session: {e}"))?;
    if let Err(e) = session.set_htp_performance_mode() {
        log::warn!("local-clip: performance mode unavailable: {e}");
    }
    let emb = local_clip::embed_bytes(&pack, &session, &system, &image)
        .map_err(|e| format!("embed: {e}"))?;
    let score = match pack.aesthetic() {
        Ok(head) => head.map(|h| local_clip::aesthetic_score(&h, &emb)),
        Err(e) => {
            log::warn!("local-clip: aesthetic head unreadable: {e}");
            None
        }
    };
    log::info!("local-clip: {}-d embedding in {:.2}s", emb.len(), t.elapsed().as_secs_f32());
    Ok((emb, score))
}

/// Run the WD14 tagger on encoded image bytes; blocking, ranked tags or an error string. Serialized
/// with generation (one HTP session at a time) and drops any resident SD/Anima cache first.
pub fn read_tags(
    lib_dir: PathBuf,
    pack_dir: PathBuf,
    image: Vec<u8>,
) -> Result<local_wd14::TagResult, String> {
    let _gate = run_lock().lock();
    drop_cache();
    let t = Instant::now();
    local_wd14::prepare_htp_env(&lib_dir);
    let pack = local_wd14::Wd14Pack::open(&pack_dir).map_err(|e| format!("pack: {e}"))?;
    let system = local_wd14::QnnSystem::load(lib_dir.join("libQnnSystem.so"))
        .map_err(|e| format!("QnnSystem: {e}"))?;
    let backend =
        local_wd14::Backend::load(lib_dir.join("libQnnHtp.so")).map_err(|e| format!("Backend: {e}"))?;
    let session = local_wd14::Session::new(&backend).map_err(|e| format!("session: {e}"))?;
    if let Err(e) = session.set_htp_performance_mode() {
        log::warn!("local-wd14: performance mode unavailable: {e}");
    }
    let result =
        local_wd14::tag_bytes(&pack, &session, &system, &image, &local_wd14::Wd14Params::default())
            .map_err(|e| format!("tag: {e}"))?;
    log::info!(
        "local-wd14: {} general, {} character in {:.2}s",
        result.general.len(),
        result.character.len(),
        t.elapsed().as_secs_f32()
    );
    Ok(result)
}

/// `WxH mean=… min=… max=…` — a flat/black decode shows up as min == max.
fn image_stats(image: &local_anima::Image) -> String {
    let n = image.rgb.len().max(1);
    let mean = image.rgb.iter().map(|&b| b as f64).sum::<f64>() / n as f64;
    let (min, max) = image.rgb.iter().fold((255u8, 0u8), |(lo, hi), &b| (lo.min(b), hi.max(b)));
    format!("{}x{} mean={mean:.1} min={min} max={max}", image.width, image.height)
}

/// Progress from a pack import worker.
pub enum ImportMsg {
    Progress(String),
    Done(Result<String, String>),
}

/// Fetch a pack zip and unpack it into `root/<name>`.
///
/// Archive layout varies (published packs nest under `output/qnn_models_*`), so files are
/// flattened onto the pack dir by basename and the result is classified before the import
/// counts as a success.
pub fn spawn_import(
    url: String,
    root: PathBuf,
    name: String,
    ctx: Context,
) -> std::sync::mpsc::Receiver<ImportMsg> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let send = |m: ImportMsg| {
            let _ = tx.send(m);
            ctx.request_repaint();
        };
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(r) => r,
            Err(e) => return send(ImportMsg::Done(Err(format!("runtime: {e}")))),
        };
        let dest = root.join(&name);
        let tmp = root.join(format!(".{name}.zip.part"));
        let res = rt.block_on(async {
            send(ImportMsg::Progress("connecting...".into()));
            let resp = reqwest::Client::new()
                .get(&url)
                .send()
                .await
                .map_err(|e| format!("request: {e}"))?;
            if !resp.status().is_success() {
                return Err(format!("server returned {}", resp.status()));
            }
            let total = resp.content_length();
            std::fs::create_dir_all(&root).map_err(|e| format!("mkdir: {e}"))?;
            let mut f = std::fs::File::create(&tmp).map_err(|e| format!("create: {e}"))?;
            let mut got = 0u64;
            let mut tick = 0u64;
            let mut resp = resp;
            while let Some(chunk) = resp.chunk().await.map_err(|e| format!("read: {e}"))? {
                use std::io::Write;
                f.write_all(&chunk).map_err(|e| format!("write: {e}"))?;
                got += chunk.len() as u64;
                if got - tick > 32 * 1024 * 1024 {
                    tick = got;
                    let msg = match total {
                        Some(t) if t > 0 => format!(
                            "downloading {:.0}% ({:.1}/{:.1} GB)",
                            got as f64 / t as f64 * 100.0,
                            got as f64 / 1e9,
                            t as f64 / 1e9
                        ),
                        _ => format!("downloading {:.1} GB", got as f64 / 1e9),
                    };
                    send(ImportMsg::Progress(msg));
                }
            }
            Ok::<(), String>(())
        });
        if let Err(e) = res {
            let _ = std::fs::remove_file(&tmp);
            return send(ImportMsg::Done(Err(e)));
        }
        send(ImportMsg::Progress("extracting...".into()));
        let unpack = (|| -> Result<usize, String> {
            let f = std::fs::File::open(&tmp).map_err(|e| format!("open zip: {e}"))?;
            let mut zipf = zip::ZipArchive::new(f).map_err(|e| format!("not a zip: {e}"))?;
            std::fs::create_dir_all(&dest).map_err(|e| format!("mkdir: {e}"))?;
            let mut n = 0usize;
            for i in 0..zipf.len() {
                let mut e = zipf.by_index(i).map_err(|e| format!("entry {i}: {e}"))?;
                if e.is_dir() {
                    continue;
                }
                let Some(base) = e.enclosed_name().and_then(|p| p.file_name().map(|s| s.to_owned()))
                else {
                    continue;
                };
                let out = dest.join(base);
                let mut w = std::fs::File::create(&out).map_err(|e| format!("write {out:?}: {e}"))?;
                std::io::copy(&mut e, &mut w).map_err(|e| format!("copy: {e}"))?;
                n += 1;
            }
            Ok(n)
        })();
        let _ = std::fs::remove_file(&tmp);
        match unpack {
            Err(e) => send(ImportMsg::Done(Err(e))),
            Ok(n) => match classify_pack(&dest) {
                Some(b) => send(ImportMsg::Done(Ok(format!(
                    "imported '{name}' ({}) - {n} files",
                    b.label()
                )))),
                None => send(ImportMsg::Done(Err(format!(
                    "unpacked {n} files but '{name}' is not a usable pack (no ANIMA marker or unet.bin)"
                )))),
            },
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn classifies_packs_by_marker() {
        let root = tmp("comfyui-packs-classify");
        let anima = root.join("anima");
        std::fs::create_dir_all(&anima).unwrap();
        std::fs::write(anima.join("ANIMA"), b"").unwrap();
        let sd = root.join("qnn");
        std::fs::create_dir_all(&sd).unwrap();
        std::fs::write(sd.join("unet.bin"), b"x").unwrap();
        let empty = root.join("junk");
        std::fs::create_dir_all(&empty).unwrap();
        assert_eq!(classify_pack(&anima), Some(LocalBackend::Anima));
        assert_eq!(classify_pack(&sd), Some(LocalBackend::Sd15));
        assert_eq!(classify_pack(&empty), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_lists_only_pack_dirs_sorted() {
        let root = tmp("comfyui-packs-scan");
        for name in ["anima_nova", "anima"] {
            let d = root.join(name);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("ANIMA"), b"").unwrap();
        }
        let sd = root.join("qnn");
        std::fs::create_dir_all(&sd).unwrap();
        std::fs::write(sd.join("unet.bin"), b"x").unwrap();
        std::fs::create_dir_all(root.join("cache")).unwrap();
        std::fs::write(root.join("stray.txt"), b"x").unwrap();
        let packs = scan_packs(&root);
        let names: Vec<&str> = packs.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["anima", "anima_nova", "qnn"]);
        assert_eq!(packs[0].backend, LocalBackend::Anima);
        assert_eq!(packs[2].backend, LocalBackend::Sd15);
        assert_eq!(packs[1].label(), "anima_nova (Anima)");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scan_of_a_missing_root_is_empty() {
        assert!(scan_packs(Path::new("/nope/does/not/exist")).is_empty());
    }

    #[test]
    fn finds_the_rewriter_pack_by_marker() {
        let root = tmp("comfyui-packs-rwtr");
        let rw = root.join("rewriter");
        std::fs::create_dir_all(&rw).unwrap();
        std::fs::write(rw.join("RWTR"), b"").unwrap();
        assert_eq!(find_rewriter_pack(&root), Some(rw));
        assert!(find_rewriter_pack(Path::new("/nope/does/not/exist")).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn humanize_ago_buckets_by_unit() {
        assert_eq!(humanize_ago(0), "just now");
        assert_eq!(humanize_ago(59), "just now");
        assert_eq!(humanize_ago(60), "1m ago");
        assert_eq!(humanize_ago(5 * 60), "5m ago");
        assert_eq!(humanize_ago(3600), "1h ago");
        assert_eq!(humanize_ago(3 * 3600), "3h ago");
        assert_eq!(humanize_ago(24 * 3600), "1d ago");
        assert_eq!(humanize_ago(2 * 24 * 3600), "2d ago");
        assert_eq!(humanize_ago(7 * 24 * 3600), "1w ago");
        assert_eq!(humanize_ago(365 * 24 * 3600), "1y ago");
    }

    #[test]
    fn finds_the_wd14_pack_alongside_generate_packs() {
        let root = tmp("comfyui-packs-wd14");
        let wd = root.join("wd14");
        std::fs::create_dir_all(&wd).unwrap();
        std::fs::write(wd.join("WD14"), b"").unwrap();
        let sd = root.join("qnn");
        std::fs::create_dir_all(&sd).unwrap();
        std::fs::write(sd.join("unet.bin"), b"x").unwrap();
        assert_eq!(find_wd14_pack(&root), Some(wd));
        assert!(find_wd14_pack(Path::new("/nope/does/not/exist")).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn pick_stays_within_the_selected_backend() {
        let packs = vec![
            PackEntry { name: "anima".into(), dir: "/a".into(), backend: LocalBackend::Anima },
            PackEntry { name: "anima_nova".into(), dir: "/n".into(), backend: LocalBackend::Anima },
            PackEntry { name: "qnn".into(), dir: "/q".into(), backend: LocalBackend::Sd15 },
        ];
        assert_eq!(pick_pack(&packs, "anima_nova", LocalBackend::Anima).unwrap().name, "anima_nova");
        assert_eq!(pick_pack(&packs, "", LocalBackend::Sd15).unwrap().name, "qnn");
        // A name from the other backend falls back to the first pack of the selected one.
        assert_eq!(pick_pack(&packs, "qnn", LocalBackend::Anima).unwrap().name, "anima");
        assert!(pick_pack(&packs[..1], "qnn", LocalBackend::Sd15).is_none());
        assert!(pick_pack(&[], "qnn", LocalBackend::Sd15).is_none());
    }

    #[test]
    fn smoke_reports_fail_when_the_pack_is_missing() {
        let report = anima_smoke("/nope/libs".into(), "/nope/pack".into(), 2, "cat".into());
        assert!(!report.ok);
        let pretty = report.pretty();
        assert!(pretty.contains("result   = FAIL"), "{pretty}");
        assert!(pretty.contains("error"), "{pretty}");
    }

    #[test]
    fn image_stats_report_the_range() {
        let img = local_anima::Image { width: 2, height: 1, rgb: vec![0, 10, 20, 30, 40, 250] };
        assert_eq!(image_stats(&img), "2x1 mean=58.3 min=0 max=250");
    }
}
