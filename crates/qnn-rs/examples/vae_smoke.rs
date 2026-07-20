//! Decode a synthetic Anima latent through vae_decoder.bin and write a PNG.
//!
//! Usage:
//!   vae_smoke <lib_dir> <model_dir> [out.png]
//! Feeds zeros in model space (i.e. the Wan 2.1 per-channel mean after
//! denormalization), so a correct decode is smooth colour, not noise.

use memmap2::Mmap;
use qnn_rs::{prepare_htp_env, Backend, ContextOpts, QnnSystem, Session};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

const WAN_MEAN: [f32; 16] = [
    -0.7571, -0.7089, -0.9113, 0.1075, -0.1745, 0.9653, -0.1517, 1.5508, 0.4134, -0.0715, 0.5517, -0.3632, -0.1922,
    -0.9497, 0.2503, -0.2921,
];
const WAN_STD: [f32; 16] = [
    2.8184, 1.4541, 2.3275, 2.6558, 1.2196, 1.7708, 2.6052, 2.0743, 3.2687, 2.1526, 2.8652, 1.5579, 1.6382, 1.1253,
    2.8251, 1.9160,
];

fn map(path: &Path) -> std::io::Result<Mmap> {
    let f = std::fs::File::open(path)?;
    unsafe { Mmap::map(&f) }
}

/// Minimal 8-bit RGB PNG writer (deflate stored blocks + zlib/CRC framing).
fn write_png(path: &Path, w: u32, h: u32, rgb: &[u8]) -> std::io::Result<()> {
    fn crc32(data: &[u8]) -> u32 {
        let mut table = [0u32; 256];
        for (i, e) in table.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB88320 ^ (c >> 1) } else { c >> 1 };
            }
            *e = c;
        }
        let mut c = 0xFFFF_FFFFu32;
        for &b in data {
            c = table[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
        }
        c ^ 0xFFFF_FFFF
    }
    fn chunk(out: &mut Vec<u8>, tag: &[u8; 4], body: &[u8]) {
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(tag);
        out.extend_from_slice(body);
        let mut crc_in = tag.to_vec();
        crc_in.extend_from_slice(body);
        out.extend_from_slice(&crc32(&crc_in).to_be_bytes());
    }
    let mut raw = Vec::with_capacity((w * h * 3 + h) as usize);
    for y in 0..h as usize {
        raw.push(0);
        raw.extend_from_slice(&rgb[y * w as usize * 3..(y + 1) * w as usize * 3]);
    }
    let mut z = vec![0x78, 0x01];
    let mut a = 1u32;
    let mut b = 0u32;
    for &byte in &raw {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    for (i, part) in raw.chunks(65535).enumerate() {
        let last = ((i + 1) * 65535 >= raw.len()) as u8;
        z.push(last);
        z.extend_from_slice(&(part.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(part.len() as u16)).to_le_bytes());
        z.extend_from_slice(part);
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut png, b"IHDR", &ihdr);
    chunk(&mut png, b"IDAT", &z);
    chunk(&mut png, b"IEND", &[]);
    std::fs::write(path, png)
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let lib_dir = PathBuf::from(args.next().unwrap_or_else(|| ".".into()));
    let model_dir = PathBuf::from(args.next().unwrap_or_else(|| ".".into()));
    let out = PathBuf::from(args.next().unwrap_or_else(|| "/sdcard/Download/vae_smoke.png".into()));

    prepare_htp_env(&lib_dir);
    let system = match QnnSystem::load(lib_dir.join("libQnnSystem.so")) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("system: {e}");
            return ExitCode::FAILURE;
        }
    };
    let backend = match Backend::load(lib_dir.join("libQnnHtp.so")) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("backend: {e}");
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
    match session.set_htp_performance_mode() {
        Ok(()) => println!("htp burst mode on"),
        Err(e) => println!("burst unavailable: {e}"),
    }

    let m = match map(&model_dir.join("vae_decoder.bin")) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("mmap vae_decoder.bin: {e}");
            return ExitCode::FAILURE;
        }
    };
    let t = Instant::now();
    let ctx = match session.load_context(&system, &m, &ContextOpts::default()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("vae contextCreateFromBinary: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("vae_decoder loaded in {:.1}s ({} MB)", t.elapsed().as_secs_f32(), m.len() / 1024 / 1024);

    // Model-space zeros -> vae space is exactly the per-channel mean.
    let mut lat = vec![0f32; 16 * 128 * 128];
    for c in 0..16 {
        let v = 0.0 * WAN_STD[c] + WAN_MEAN[c];
        for i in 0..128 * 128 {
            lat[c * 128 * 128 + i] = v;
        }
    }

    let t = Instant::now();
    let outmap = match ctx.execute("model", &[("input", &lat)]) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("vae execute: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("vae execute {:.2}s", t.elapsed().as_secs_f32());
    let px = match outmap.get("output") {
        Some(v) => v,
        None => {
            eprintln!("no 'output' tensor");
            return ExitCode::FAILURE;
        }
    };
    let (mut mn, mut mx, mut sum, mut nf) = (f32::INFINITY, f32::NEG_INFINITY, 0f64, 0usize);
    for &x in px {
        if !x.is_finite() {
            nf += 1;
            continue;
        }
        mn = mn.min(x);
        mx = mx.max(x);
        sum += x as f64;
    }
    println!("  output: len={} mean={:.5} min={:.4} max={:.4} nonfinite={nf}", px.len(), sum / px.len() as f64, mn, mx);

    let (w, h) = (1024usize, 1024usize);
    let mut rgb = vec![0u8; w * h * 3];
    for y in 0..h {
        for x in 0..w {
            for c in 0..3 {
                let v = px[c * w * h + y * w + x];
                rgb[(y * w + x) * 3 + c] = (((v + 1.0) * 0.5 * 255.0).round()).clamp(0.0, 255.0) as u8;
            }
        }
    }
    if let Err(e) = write_png(&out, w as u32, h as u32, &rgb) {
        eprintln!("write png: {e}");
        return ExitCode::FAILURE;
    }
    println!("VAE-SMOKE OK {}", out.display());
    ExitCode::SUCCESS
}
