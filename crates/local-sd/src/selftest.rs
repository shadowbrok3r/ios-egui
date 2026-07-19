//! D1 device smoke test: load system + HTP backend, create a context from a
//! `.bin`, optionally pin HTP burst mode, run one synthetic `execute`, and
//! report graphs, IO, timing, and per-output stats. Uses only `qnn_rs` + std.
//!
//! On host (no NPU) this fails cleanly at backend/context init; the failure is
//! captured in the report rather than panicking.

use qnn_rs::{prepare_htp_env, set_htp_performance_mode, Backend, Context, QnnSystem};
use std::path::PathBuf;
use std::time::Instant;

/// Inputs for [`device_selftest`]. All models/libs are referenced by path.
#[derive(Clone, Debug)]
pub struct SelftestConfig {
    /// Path to `libQnnSystem.so`.
    pub system_lib: PathBuf,
    /// Path to the backend, `libQnnHtp.so` on device.
    pub backend_lib: PathBuf,
    /// Context binary to load and execute (e.g. `unet.bin`).
    pub model_bin: PathBuf,
    /// Directory holding the HTP skel, prepended to the loader search paths.
    pub skel_dir: Option<PathBuf>,
    /// Pin the HTP to DCVS_V3 burst before executing.
    pub set_performance_mode: bool,
    /// Graph to execute; defaults to the first graph in the binary.
    pub graph: Option<String>,
}

/// One tensor's parsed metadata.
#[derive(Clone, Debug)]
pub struct TensorSummary {
    pub name: String,
    pub dims: Vec<u32>,
    pub dtype: String,
    pub quant: Option<(f32, i32)>,
}

/// One graph's IO.
#[derive(Clone, Debug)]
pub struct GraphSummary {
    pub name: String,
    pub inputs: Vec<TensorSummary>,
    pub outputs: Vec<TensorSummary>,
}

/// Per-output statistics from the executed graph.
#[derive(Clone, Debug)]
pub struct OutputStats {
    pub name: String,
    pub dims: Vec<u32>,
    pub dtype: String,
    pub elems: usize,
    pub min: f32,
    pub max: f32,
    pub mean: f32,
}

/// The result of a self-test run.
#[derive(Clone, Debug)]
pub struct SelftestReport {
    pub ok: bool,
    pub log: Vec<String>,
    pub system_provider: Option<String>,
    pub backend_provider: Option<String>,
    pub graphs: Vec<GraphSummary>,
    pub executed_graph: Option<String>,
    pub exec_ms: Option<f64>,
    pub outputs: Vec<OutputStats>,
    pub error: Option<String>,
}

impl SelftestReport {
    fn blank() -> Self {
        Self {
            ok: false,
            log: Vec::new(),
            system_provider: None,
            backend_provider: None,
            graphs: Vec::new(),
            executed_graph: None,
            exec_ms: None,
            outputs: Vec::new(),
            error: None,
        }
    }

    /// A human-readable diagnostic dump.
    pub fn pretty(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!("local-sd device self-test: {}\n", if self.ok { "OK" } else { "FAILED" }));
        if let Some(p) = &self.system_provider {
            s.push_str(&format!("system provider: {p}\n"));
        }
        if let Some(p) = &self.backend_provider {
            s.push_str(&format!("backend provider: {p}\n"));
        }
        for g in &self.graphs {
            s.push_str(&format!("graph \"{}\"\n", g.name));
            for i in &g.inputs {
                s.push_str(&format!("  in  {:16} {:?} {}{}\n", i.name, i.dims, i.dtype, fmt_quant(&i.quant)));
            }
            for o in &g.outputs {
                s.push_str(&format!("  out {:16} {:?} {}{}\n", o.name, o.dims, o.dtype, fmt_quant(&o.quant)));
            }
        }
        if let Some(g) = &self.executed_graph {
            s.push_str(&format!("executed: \"{g}\"\n"));
        }
        if let Some(ms) = self.exec_ms {
            s.push_str(&format!("execute: {ms:.2} ms\n"));
        }
        for o in &self.outputs {
            s.push_str(&format!(
                "  {} [{} elems]: min={:.5} max={:.5} mean={:.5}\n",
                o.name, o.elems, o.min, o.max, o.mean
            ));
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

fn fmt_quant(q: &Option<(f32, i32)>) -> String {
    match q {
        Some((scale, offset)) => format!("  quant(scale={scale}, offset={offset})"),
        None => String::new(),
    }
}

fn summarize(t: &qnn_rs::TensorInfo) -> TensorSummary {
    TensorSummary {
        name: t.name.clone(),
        dims: t.dims.clone(),
        dtype: format!("{:?}", t.dtype),
        quant: t.quant.map(|q| (q.scale, q.offset)),
    }
}

/// Deterministic synthetic value in `[-1, 1]` (xorshift64), no rng dependency.
fn synth_latent(n: usize) -> Vec<f32> {
    let mut state = 0x9E3779B97F4A7C15u64;
    (0..n)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state as f32 / u64::MAX as f32) * 2.0 - 1.0
        })
        .collect()
}

/// Load libs + binary, run one synthetic execute, and return a diagnostic report.
/// Never panics; runtime failures land in `report.error` with `ok = false`.
pub fn device_selftest(cfg: SelftestConfig) -> SelftestReport {
    let mut report = SelftestReport::blank();
    match run(&cfg, &mut report) {
        Ok(()) => report.ok = true,
        Err(e) => {
            report.ok = false;
            report.error = Some(e);
        }
    }
    report
}

fn run(cfg: &SelftestConfig, report: &mut SelftestReport) -> Result<(), String> {
    if let Some(skel) = &cfg.skel_dir {
        prepare_htp_env(skel);
        report.log.push(format!("prepare_htp_env({})", skel.display()));
    }

    let system = QnnSystem::load(&cfg.system_lib).map_err(|e| format!("QnnSystem::load: {e}"))?;
    report.system_provider = Some(format!("{} v{:?}", system.provider_name(), system.api_version()));
    report.log.push("system loaded".into());

    let bytes = std::fs::read(&cfg.model_bin).map_err(|e| format!("read {}: {e}", cfg.model_bin.display()))?;
    report.log.push(format!("read {} ({} bytes)", cfg.model_bin.display(), bytes.len()));

    // Parse metadata (host-capable) and record graph summaries before device init.
    let info = qnn_rs::ContextBinaryInfo::parse(&system, &bytes).map_err(|e| format!("parse: {e}"))?;
    for g in &info.graphs {
        report.graphs.push(GraphSummary {
            name: g.name.clone(),
            inputs: g.inputs.iter().map(summarize).collect(),
            outputs: g.outputs.iter().map(summarize).collect(),
        });
    }
    report.log.push(format!("parsed {} graph(s)", info.graphs.len()));

    let backend = Backend::load(&cfg.backend_lib).map_err(|e| format!("Backend::load: {e}"))?;
    report.backend_provider =
        Some(format!("{} (id {}) v{:?}", backend.provider_name(), backend.backend_id(), backend.api_version()));
    report.log.push("backend loaded".into());

    let ctx = Context::from_binary(&backend, &system, &bytes).map_err(|e| format!("Context::from_binary: {e}"))?;
    report.log.push("context created".into());

    if cfg.set_performance_mode {
        set_htp_performance_mode(&backend).map_err(|e| format!("set_htp_performance_mode: {e}"))?;
        report.log.push("HTP burst mode set".into());
    }

    let graph = match &cfg.graph {
        Some(name) => info
            .graphs
            .iter()
            .find(|g| &g.name == name)
            .ok_or_else(|| format!("graph '{name}' not found"))?,
        None => info.graphs.first().ok_or("binary has no graphs")?,
    };
    let graph_name = graph.name.clone();
    report.executed_graph = Some(graph_name.clone());

    let synth: Vec<(String, Vec<f32>)> = graph
        .inputs
        .iter()
        .map(|t| {
            let n = t.elem_count() as usize;
            let data = if n == 1 {
                vec![999.0]
            } else if t.dims.len() >= 4 {
                synth_latent(n)
            } else {
                vec![0.0; n]
            };
            (t.name.clone(), data)
        })
        .collect();
    let inputs: Vec<(&str, &[f32])> = synth.iter().map(|(n, d)| (n.as_str(), d.as_slice())).collect();

    let dtypes: Vec<(String, Vec<u32>, String)> =
        graph.outputs.iter().map(|t| (t.name.clone(), t.dims.clone(), format!("{:?}", t.dtype))).collect();

    let t0 = Instant::now();
    let out = ctx.execute(&graph_name, &inputs).map_err(|e| format!("execute: {e}"))?;
    report.exec_ms = Some(t0.elapsed().as_secs_f64() * 1000.0);
    report.log.push("execute returned".into());

    for (name, dims, dtype) in dtypes {
        if let Some(values) = out.get(&name) {
            report.outputs.push(stats(name, dims, dtype, values));
        }
    }
    Ok(())
}

fn stats(name: String, dims: Vec<u32>, dtype: String, values: &[f32]) -> OutputStats {
    let elems = values.len();
    let mut min = f32::INFINITY;
    let mut max = f32::NEG_INFINITY;
    let mut sum = 0.0f64;
    for &v in values {
        min = min.min(v);
        max = max.max(v);
        sum += v as f64;
    }
    OutputStats {
        name,
        dims,
        dtype,
        elems,
        min: if elems == 0 { 0.0 } else { min },
        max: if elems == 0 { 0.0 } else { max },
        mean: if elems == 0 { 0.0 } else { (sum / elems as f64) as f32 },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bogus_paths_fail_without_panicking() {
        let report = device_selftest(SelftestConfig {
            system_lib: "/nonexistent/libQnnSystem.so".into(),
            backend_lib: "/nonexistent/libQnnHtp.so".into(),
            model_bin: "/nonexistent/unet.bin".into(),
            skel_dir: None,
            set_performance_mode: false,
            graph: None,
        });
        assert!(!report.ok);
        assert!(report.error.is_some());
        assert!(report.pretty().contains("FAILED"));
    }

    #[test]
    fn synth_latent_is_bounded_and_deterministic() {
        let a = synth_latent(1000);
        let b = synth_latent(1000);
        assert_eq!(a, b);
        assert!(a.iter().all(|v| (-1.0..=1.0).contains(v)));
    }

    // Real QNN libs + a context binary. On host the report parses graphs then
    // fails at device init; on device it should run one execute. Run with:
    // LOCAL_SD_QNN_SYSTEM=.../libQnnSystem.so LOCAL_SD_QNN_BACKEND=.../libQnnHtp.so
    // LOCAL_SD_MODEL_BIN=.../unet.bin cargo test -p local-sd -- --ignored --nocapture
    #[test]
    #[ignore = "needs QNN libs + a context binary via LOCAL_SD_QNN_* env"]
    fn selftest_against_real_binary() {
        let system_lib = std::env::var("LOCAL_SD_QNN_SYSTEM").expect("LOCAL_SD_QNN_SYSTEM").into();
        let backend_lib = std::env::var("LOCAL_SD_QNN_BACKEND").expect("LOCAL_SD_QNN_BACKEND").into();
        let model_bin = std::env::var("LOCAL_SD_MODEL_BIN").expect("LOCAL_SD_MODEL_BIN").into();
        let skel_dir = std::env::var("LOCAL_SD_SKEL_DIR").ok().map(Into::into);
        let report = device_selftest(SelftestConfig {
            system_lib,
            backend_lib,
            model_bin,
            skel_dir,
            set_performance_mode: false,
            graph: None,
        });
        println!("{}", report.pretty());
        // Metadata parse must succeed even without an NPU.
        assert!(!report.graphs.is_empty(), "no graphs parsed");
    }
}
