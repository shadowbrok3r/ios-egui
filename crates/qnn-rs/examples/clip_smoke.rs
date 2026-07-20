//! Run the Anima text encoder (clip.bin) on the HTP with a mixed f32/i32 input
//! set, proving Int32 tensor binding works end to end.
//!
//! Usage:
//!   clip_smoke <lib_dir> <model_dir>
//! `lib_dir` holds libQnnHtp.so/libQnnSystem.so and the v81 skel; `model_dir`
//! holds clip.bin.
//!
//! Env: CLIP_RUNS=n repeats the execute, CLIP_TOKENS=n sets the prompt length.

use memmap2::Mmap;
use qnn_rs::{prepare_htp_env, Backend, ContextOpts, QnnSystem, Session, TensorIn};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

fn stats(name: &str, v: &[f32]) {
    let n = v.len() as f64;
    let mut min = f32::INFINITY;
    let mut max = f32::NEG_INFINITY;
    let mut sum = 0f64;
    let mut nan = 0usize;
    for &x in v {
        if x.is_nan() || x.is_infinite() {
            nan += 1;
            continue;
        }
        min = min.min(x);
        max = max.max(x);
        sum += x as f64;
    }
    let mean = sum / n;
    let var = v.iter().filter(|x| x.is_finite()).map(|&x| (x as f64 - mean).powi(2)).sum::<f64>() / n;
    println!("  {name}: len={} mean={:.5} std={:.5} min={:.4} max={:.4} nonfinite={nan}", v.len(), mean, var.sqrt(), min, max);
}

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

fn env_u64(key: &str) -> Option<u64> {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok())
}

fn map(path: &Path) -> std::io::Result<Mmap> {
    let f = std::fs::File::open(path)?;
    unsafe { Mmap::map(&f) }
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let lib_dir = PathBuf::from(args.next().unwrap_or_else(|| ".".into()));
    let model_dir = PathBuf::from(args.next().unwrap_or_else(|| ".".into()));

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
    // Must run after the session exists; without the burst vote long graphs
    // outrun the FastRPC timeout and surface as a DSP subsystem restart.
    match session.set_htp_performance_mode() {
        Ok(()) => println!("htp burst mode on"),
        Err(e) => println!("htp burst mode unavailable: {e}"),
    }
    println!("rss after backend load: {} MB", rss_mb());

    let path = model_dir.join("clip.bin");
    let m = match map(&path) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("mmap {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
    };
    println!("mapped clip.bin {} MB", m.len() / 1024 / 1024);

    let t = Instant::now();
    let ctx = match session.load_context(&system, &m, &ContextOpts::default()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("clip contextCreateFromBinary: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("clip loaded in {:.1}s, rss {} MB", t.elapsed().as_secs_f32(), rss_mb());

    let Some(g) = ctx.info().graphs.first().cloned() else {
        eprintln!("clip.bin has no graphs");
        return ExitCode::FAILURE;
    };
    println!("clip graph {:?}", g.name);
    for t in &g.inputs {
        println!("   in  {:24} {:?} {:?}", t.name, t.dims, t.dtype);
    }
    for t in &g.outputs {
        println!("   out {:24} {:?} {:?}", t.name, t.dims, t.dtype);
    }

    let seq = g.inputs.iter().find(|t| t.name == "t5_ids").map(|t| t.elem_count() as usize).unwrap_or(512);
    let embed = g.inputs.iter().find(|t| t.name == "input_embedding").map(|t| t.elem_count() as usize).unwrap_or(seq * 1024);
    let tokens = env_u64("CLIP_TOKENS").unwrap_or(12).max(1) as usize;

    let mut seed = 1234567u64;
    let mut rnd = || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (((seed >> 33) as f32 / (1u32 << 31) as f32) * 2.0 - 1.0) * 0.02
    };
    let input_embedding: Vec<f32> = (0..embed).map(|_| rnd()).collect();
    // ids 100..; 1 as EOS at the last prompt position; 0 padding after.
    let t5_ids: Vec<i32> = (0..seq)
        .map(|i| if i + 1 < tokens { 100 + i as i32 } else if i + 1 == tokens { 1 } else { 0 })
        .collect();
    let mask: Vec<f32> = (0..seq).map(|i| if i < tokens { 1.0 } else { 0.0 }).collect();
    println!("inputs: embed={} seq={} tokens={} ids[0..4]={:?}", embed, seq, tokens, &t5_ids[..4.min(seq)]);

    let runs = env_u64("CLIP_RUNS").unwrap_or(1).max(1);
    for run in 0..runs {
        let t = Instant::now();
        let out = match ctx.execute_mixed(
            &g.name,
            &[
                ("input_embedding", TensorIn::F32(&input_embedding)),
                ("t5_ids", TensorIn::I32(&t5_ids)),
                ("t5_mask", TensorIn::F32(&mask)),
                ("qwen_mask", TensorIn::F32(&mask)),
            ],
        ) {
            Ok(o) => o,
            Err(e) => {
                eprintln!("clip execute (run {run}): {e}");
                return ExitCode::FAILURE;
            }
        };
        println!("clip execute {:.3}s (run {run}), rss {} MB", t.elapsed().as_secs_f32(), rss_mb());
        for (k, v) in &out {
            stats(k, v);
            // Per-position norms show whether masked-out tail differs from the prompt.
            if let Some(w) = out.get(k).map(|v| v.len() / seq).filter(|&w| w > 1) {
                let norm = |i: usize| (v[i * w..(i + 1) * w].iter().map(|&x| (x as f64) * (x as f64)).sum::<f64>() / w as f64).sqrt();
                println!("    {k} rms: pos0={:.5} pos1={:.5} pos{}={:.5} pos{}={:.5}", norm(0), norm(1), tokens - 1, norm(tokens - 1), seq - 1, norm(seq - 1));
            }
        }
    }

    println!("CLIP-SMOKE OK");
    ExitCode::SUCCESS
}
