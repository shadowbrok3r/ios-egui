//! Load both Anima DiT halves co-resident in one session and run split steps
//! on the HTP.
//!
//! Usage:
//!   anima_smoke <lib_dir> <model_dir>
//! `lib_dir` holds libQnnHtp.so/libQnnSystem.so and the v81 skel; `model_dir`
//! holds unet_part1.bin and unet_part2.bin.
//!
//! Env: ANIMA_STEPS=n repeats the step, ANIMA_SF_BYTES=auto|0|<bytes> sizes the
//! shared spill-fill group, ANIMA_UDMA=1 enables extended uDMA far-mapping.

use memmap2::Mmap;
use qnn_rs::{prepare_htp_env, Backend, ContextOpts, QnnSystem, Session};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

/// local-dream's hardcoded Anima group size, used when the runtime query is
/// unsupported (pre-2.35 binaries report 0).
const SPILL_FILL_FALLBACK: u64 = 601096192;

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

fn env_flag(key: &str) -> bool {
    matches!(std::env::var(key).as_deref(), Ok("1") | Ok("true"))
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
    // Must run after the session exists; without the burst vote the DiT graphs
    // outrun the FastRPC timeout and the DSP subsystem restarts.
    match session.set_htp_performance_mode() {
        Ok(()) => println!("htp burst mode on"),
        Err(e) => println!("htp burst mode unavailable: {e}"),
    }
    println!("rss after backend load: {} MB", rss_mb());

    let p1 = model_dir.join("unet_part1.bin");
    let p2 = model_dir.join("unet_part2.bin");
    let (m1, m2) = match (map(&p1), map(&p2)) {
        (Ok(a), Ok(b)) => (a, b),
        (a, b) => {
            eprintln!("mmap failed: part1={:?} part2={:?}", a.err(), b.err());
            return ExitCode::FAILURE;
        }
    };
    println!("mapped part1 {} MB, part2 {} MB", m1.len() / 1024 / 1024, m2.len() / 1024 / 1024);

    let udma = env_flag("ANIMA_UDMA");

    // ANIMA_SF_BYTES: unset/"auto" probes each half alone for its spill-fill
    // requirement, 0 disables the group, a number sets the group size.
    let sf = match std::env::var("ANIMA_SF_BYTES").as_deref() {
        Ok("auto") | Err(_) => {
            let plain = ContextOpts::default().with_extended_udma(udma);
            let mut probed = [0u64; 2];
            for (i, (tag, m)) in [("part1", &m1), ("part2", &m2)].iter().enumerate() {
                match session.load_context(&system, m, &plain) {
                    Ok(c) => probed[i] = c.max_spill_fill_size(),
                    Err(e) => println!("{tag} probe context failed: {e}"),
                }
                println!("{tag} probe: spill-fill query {} bytes ({:.1} MB)", probed[i], probed[i] as f64 / 1048576.0);
            }
            probed[0].max(probed[1]).max(SPILL_FILL_FALLBACK)
        }
        _ => env_u64("ANIMA_SF_BYTES").unwrap_or(0),
    };
    println!("spill-fill group size {sf} ({:.1} MB), fallback {SPILL_FILL_FALLBACK}, udma {udma}", sf as f64 / 1048576.0);

    let t = Instant::now();
    let c1 = match session.load_context(&system, &m1, &ContextOpts::spill_fill_head(sf).with_extended_udma(udma)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("part1 contextCreateFromBinary: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("part1 loaded in {:.1}s, rss {} MB (group head, requires {} bytes)", t.elapsed().as_secs_f32(), rss_mb(), c1.max_spill_fill_size());

    let t = Instant::now();
    let c2 = match session.load_context(&system, &m2, &ContextOpts::spill_fill_join(sf, &c1).with_extended_udma(udma)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("part2 contextCreateFromBinary (part1 still resident): {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("part2 loaded in {:.1}s, rss {} MB -- BOTH DiT HALVES CO-RESIDENT (requires {} bytes)", t.elapsed().as_secs_f32(), rss_mb(), c2.max_spill_fill_size());

    for (tag, c) in [("part1", &c1), ("part2", &c2)] {
        for g in &c.info().graphs {
            println!("{tag} graph {:?}", g.name);
            for t in &g.inputs {
                println!("   in  {:32} {:?} {:?}", t.name, t.dims, t.dtype);
            }
            for t in &g.outputs {
                println!("   out {:32} {:?} {:?}", t.name, t.dims, t.dtype);
            }
        }
    }

    // Deterministic pseudo-random latent; zeros stand in for real text context.
    let mut seed = 1234567u64;
    let mut rnd = || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((seed >> 33) as f32 / (1u32 << 31) as f32) * 2.0 - 1.0
    };
    let sample: Vec<f32> = (0..16 * 128 * 128).map(|_| rnd()).collect();
    let ehs = vec![0f32; 512 * 1024];
    let ts = vec![1.0f32];

    let steps = env_u64("ANIMA_STEPS").unwrap_or(1).max(1);
    let (mut d1, mut d2) = (Default::default(), Default::default());
    for step in 0..steps {
        let t = Instant::now();
        let out1 = match c1.execute("model", &[("sample", &sample), ("encoder_hidden_states", &ehs), ("timestamp", &ts)]) {
            Ok(o) => o,
            Err(e) => {
                eprintln!("part1 execute (step {step}): {e}");
                return ExitCode::FAILURE;
            }
        };
        d1 = t.elapsed();
        println!("part1 execute {:.2}s (step {step})", d1.as_secs_f32());
        let hidden = out1.get("hidden").cloned().unwrap_or_default();
        let emb = out1.get("emb").cloned().unwrap_or_default();
        stats("hidden", &hidden);
        stats("emb", &emb);

        let t = Instant::now();
        let out2 = match c2.execute("model", &[("hidden", &hidden), ("emb", &emb), ("context", &ehs), ("timestamp", &ts)]) {
            Ok(o) => o,
            Err(e) => {
                eprintln!("part2 execute (step {step}): {e}");
                return ExitCode::FAILURE;
            }
        };
        d2 = t.elapsed();
        println!("part2 execute {:.2}s (step {step})", d2.as_secs_f32());
        for (k, v) in &out2 {
            stats(k, v);
        }
    }

    println!("\nSTEP TOTAL {:.2}s -> 10 steps ~= {:.0}s, rss {} MB", (d1 + d2).as_secs_f32(), (d1 + d2).as_secs_f32() * 10.0, rss_mb());
    println!("ANIMA-SMOKE OK");
    ExitCode::SUCCESS
}
