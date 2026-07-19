//! Dump the metadata of a QNN context binary.
//!
//! Usage:
//!   cargo run -p qnn-rs --example dump_context -- <libQnnSystem.so> <context.bin>
//! or via env vars:
//!   QNN_SYSTEM_LIB=.../libQnnSystem.so QNN_CONTEXT_BIN=.../unet.bin \
//!     cargo run -p qnn-rs --example dump_context

use qnn_rs::{ContextBinaryInfo, QnnSystem};
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let lib = args
        .next()
        .or_else(|| std::env::var("QNN_SYSTEM_LIB").ok())
        .unwrap_or_default();
    let bin = args
        .next()
        .or_else(|| std::env::var("QNN_CONTEXT_BIN").ok())
        .unwrap_or_default();

    if lib.is_empty() || bin.is_empty() {
        eprintln!("usage: dump_context <libQnnSystem.so> <context.bin>");
        eprintln!("   or: QNN_SYSTEM_LIB=... QNN_CONTEXT_BIN=... dump_context");
        return ExitCode::FAILURE;
    }

    let system = match QnnSystem::load(&lib) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("load system lib failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let (maj, min, pat) = system.api_version();
    println!(
        "loaded {lib}\n  provider: {:?}  system API v{maj}.{min}.{pat}\n",
        system.provider_name()
    );

    let bytes = match std::fs::read(&bin) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("read {bin} failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("parsing {bin} ({} bytes)\n", bytes.len());

    let info = match ContextBinaryInfo::parse(&system, &bytes) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("parse failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    let (cmaj, cmin, cpat) = info.core_api_version;
    println!(
        "backend_id={} build_id={:?} core_api=v{cmaj}.{cmin}.{cpat} soc_model={:?}",
        info.backend_id, info.build_id, info.soc_model
    );
    println!("{} graph(s):\n", info.graphs.len());

    for g in &info.graphs {
        println!("graph \"{}\"", g.name);
        print_tensors("input", &g.inputs);
        print_tensors("output", &g.outputs);
        println!();
    }

    ExitCode::SUCCESS
}

fn print_tensors(kind: &str, tensors: &[qnn_rs::TensorInfo]) {
    for t in tensors {
        let quant = match t.quant {
            Some(q) => format!("  quant(scale={}, offset={})", q.scale, q.offset),
            None => String::new(),
        };
        println!(
            "  {kind:<6} {:<40} dims={:?} dtype={:?}{quant}",
            t.name, t.dims, t.dtype
        );
    }
}
