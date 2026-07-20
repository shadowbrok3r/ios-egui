//! End-to-end Anima text2img on the HTP.
//!
//! Usage:
//!   anima_txt2img <lib_dir> <model_dir> [prompt] [out.png]
//! `lib_dir` holds libQnnHtp.so/libQnnSystem.so and the skel; `model_dir` is an
//! Anima model pack.
//!
//! Env: ANIMA_STEPS, ANIMA_CFG, ANIMA_SEED, ANIMA_SCHEDULER, ANIMA_SIZE,
//! ANIMA_NEGATIVE override the pack's config.json defaults.

use local_anima::{prepare_htp_env, AnimaPack, AnimaParams, Backend, QnnSystem, Session};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

fn rss_mb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1).and_then(|v| v.parse::<u64>().ok()))
        })
        .map(|kb| kb / 1024)
        .unwrap_or(0)
}

fn env<T: std::str::FromStr>(key: &str) -> Option<T> {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok())
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let lib_dir = PathBuf::from(args.next().unwrap_or_else(|| ".".into()));
    let model_dir = PathBuf::from(args.next().unwrap_or_else(|| ".".into()));
    let prompt_arg = args.next();
    let out_path = PathBuf::from(args.next().unwrap_or_else(|| "anima_out.png".into()));

    let t = Instant::now();
    let pack = match AnimaPack::open(&model_dir) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("open model pack {}: {e}", model_dir.display());
            return ExitCode::FAILURE;
        }
    };
    println!("pack opened in {:.2}s, token_emb vocab {}", t.elapsed().as_secs_f32(), pack.token_emb().vocab());

    let prompt = prompt_arg.unwrap_or_else(|| pack.config().default_prompt.clone());
    let mut params = AnimaParams::from_pack(&pack);
    if let Some(v) = env("ANIMA_STEPS") {
        params.steps = v;
    }
    if let Some(v) = env("ANIMA_CFG") {
        params.cfg = v;
    }
    if let Some(v) = env("ANIMA_SEED") {
        params.seed = v;
    }
    if let Ok(v) = std::env::var("ANIMA_SCHEDULER") {
        params.scheduler = v;
    }
    if let Ok(v) = std::env::var("ANIMA_NEGATIVE") {
        params.negative = v;
    }
    if let Some(v) = env::<usize>("ANIMA_SIZE") {
        params.width = v;
        params.height = v;
    }
    println!(
        "prompt {prompt:?}\nparams steps={} cfg={} seed={} scheduler={} size={}x{}",
        params.steps, params.cfg, params.seed, params.scheduler, params.width, params.height
    );

    prepare_htp_env(&lib_dir);
    let system = match QnnSystem::load(lib_dir.join("libQnnSystem.so")) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("load libQnnSystem.so: {e}");
            return ExitCode::FAILURE;
        }
    };
    let backend = match Backend::load(lib_dir.join("libQnnHtp.so")) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("load libQnnHtp.so: {e}");
            return ExitCode::FAILURE;
        }
    };
    let session = match Session::new(&backend) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("session: {e}");
            return ExitCode::FAILURE;
        }
    };
    // Must run after the session exists; without the burst vote the DiT graphs
    // outrun the FastRPC timeout and the DSP subsystem restarts.
    match session.set_htp_performance_mode() {
        Ok(()) => println!("htp burst mode on"),
        Err(e) => println!("htp burst mode unavailable: {e}"),
    }
    println!("rss after backend load: {} MB", rss_mb());

    let run = Instant::now();
    let mut last = Instant::now();
    let image = local_anima::text2img(&pack, &session, &system, &prompt, &params, |step, total| {
        println!("step {step}/{total} in {:.2}s, rss {} MB", last.elapsed().as_secs_f32(), rss_mb());
        last = Instant::now();
    });
    let image = match image {
        Ok(i) => i,
        Err(e) => {
            eprintln!("text2img: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("pipeline total {:.2}s, rss {} MB", run.elapsed().as_secs_f32(), rss_mb());

    if let Err(e) = image.save_png(&out_path) {
        eprintln!("write {}: {e}", out_path.display());
        return ExitCode::FAILURE;
    }
    println!("ANIMA-TXT2IMG OK {}", out_path.display());
    ExitCode::SUCCESS
}
