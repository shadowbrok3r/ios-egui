//! Per-stage timing of `ringdesign-core` on the device it will actually run on. Pure — no egui, no
//! Android, so it runs under `cargo test` on the host too.
//!
//! The desktop table in RingDesigner's CLAUDE.md was measured on a 16-core 5.76 GHz x86 desktop and
//! reports one `build_ms` per preset. That number cannot say which stage dominates, and only the
//! sweep inside `mesh::build` is parallel — triangulation, `smooth_normals`, `volume_mm3`,
//! `surface_area_mm2`, `validate` and both passes of `castability::analyze` are serial. On eight
//! phone cores the serial tail is what decides the interactive resolution, so it is timed apart.
//!
//! `validate` and the two integrals are re-run on the finished mesh rather than read out of
//! `Report`, which is what makes their share visible: `mesh::build` already paid for them once.

use std::time::Instant;

use ringdesign_core::alpha::AlphaLibrary;
use ringdesign_core::castability;
use ringdesign_core::field::{Layer, LayerEntry, MilgrainLayer};
use ringdesign_core::mesh::{self, BuildParams};
use ringdesign_core::tiling::TilingLayer;
use ringdesign_core::{ProfileStyle, RingDesign, ShankKind};

/// One row of the report: a preset, and where its milliseconds went.
#[derive(Clone, Debug)]
pub struct Row {
    pub label: String,
    pub triangles: usize,
    /// Whole `mesh::build`, as the app would pay for it.
    pub build_ms: f64,
    /// `Mesh::validate` alone, re-run on the finished mesh.
    pub validate_ms: f64,
    /// `volume_mm3` + `surface_area_mm2`, re-run.
    pub integrals_ms: f64,
    /// `castability::analyze`, which the build worker runs straight after every build.
    pub analyze_ms: f64,
    /// Serializing a binary STL, the export cost after the build.
    pub stl_ms: f64,
    pub stl_bytes: usize,
    /// Peak displacement the layer stack applied, as a sanity check that the design built.
    pub max_relief_mm: f64,
    pub watertight: bool,
}

impl Row {
    /// Everything the build worker pays per edit: the build plus the analysis after it.
    pub fn worker_ms(&self) -> f64 {
        self.build_ms + self.analyze_ms
    }

    /// What skipping `validate` would give back, as a fraction of the worker's job.
    pub fn validate_share(&self) -> f64 {
        let total = self.worker_ms();
        if total > 0.0 { self.validate_ms / total } else { 0.0 }
    }
}

/// The whole report, plus the one-off costs paid at startup.
#[derive(Clone, Debug, Default)]
pub struct Report {
    /// `AlphaLibrary::builtin()` — 16 procedural 256x256 alphas, generated serially today.
    pub builtin_ms: f64,
    pub rows: Vec<Row>,
    /// A signet band, which is the only design that pays for `Silhouette`'s one-time build.
    pub signet_rows: Vec<Row>,
    pub threads: usize,
}

/// Presets worth measuring on a phone. Deliberately reaches below `Draft` and stops below
/// `Maximum`: 1536x448 is a 149 MB vertex buffer and is not a mobile tier under any answer.
pub const MOBILE_PRESETS: &[(&str, usize, usize)] = &[
    ("Scrub 160x80", 160, 80),
    ("Preview 256x112", 256, 112),
    ("Draft 192x96", 192, 96),
    ("Inspect 384x144", 384, 144),
    ("Fine 512x192", 512, 192),
    ("Export 768x256", 768, 256),
    ("Export 1024x320", 1024, 320),
];

/// The band the desktop's own benchmark uses: a D-shape carrying a tiled alpha and milgrain.
pub fn bench_design(lib: &AlphaLibrary) -> RingDesign {
    let mut d = RingDesign::default();
    d.profile.apply_style(ProfileStyle::DShape);
    let ctx = d.field_context();
    let name = lib.names().first().cloned().unwrap_or_default();
    d.layers
        .layers
        .push(LayerEntry::new("tile", Layer::Tiling(TilingLayer::default_for(name, &ctx))));
    d.layers.layers.push(LayerEntry::new(
        "milgrain",
        Layer::Milgrain(MilgrainLayer { v_mm: 0.55, ..MilgrainLayer::default() }),
    ));
    d
}

/// A signet head. Its `Silhouette` table is built once per outline behind a `OnceLock`, so the
/// first row of this set carries that cost and the rest do not — which is the point of timing it.
pub fn signet_design() -> RingDesign {
    let mut d = RingDesign::default();
    d.shank.kind = ShankKind::Signet;
    d
}

fn time<T>(f: impl FnOnce() -> T) -> (T, f64) {
    let t = Instant::now();
    let out = f();
    (out, t.elapsed().as_secs_f64() * 1000.0)
}

/// Measure one preset against one design.
pub fn row(label: &str, design: &RingDesign, lib: &AlphaLibrary, theta: usize, profile: usize) -> Row {
    let params = BuildParams { theta_steps: theta, profile_steps: profile, ..Default::default() };

    let (out, build_ms) = time(|| mesh::build(design, lib, params));
    let (validation, validate_ms) = time(|| out.mesh.validate());
    let (_, integrals_ms) = time(|| (out.mesh.volume_mm3(), out.mesh.surface_area_mm2()));
    let (_, analyze_ms) = time(|| {
        castability::analyze(&out.mesh, &design.draft, design.inner_radius_mm())
    });
    let (bytes, stl_ms) = time(|| ringdesign_core::stl::to_stl_binary(&out.mesh));

    Row {
        label: label.to_owned(),
        triangles: out.report.validation.triangle_count,
        build_ms,
        validate_ms,
        integrals_ms,
        analyze_ms,
        stl_ms,
        stl_bytes: bytes.len(),
        max_relief_mm: out.report.max_relief_mm,
        watertight: validation.watertight,
    }
}

/// Run the whole set. Slow by design — this is the measurement, not a warm-up.
pub fn run() -> Report {
    let (lib, builtin_ms) = time(AlphaLibrary::builtin);
    let design = bench_design(&lib);
    let signet = signet_design();

    let rows = MOBILE_PRESETS
        .iter()
        .map(|&(name, t, p)| row(name, &design, &lib, t, p))
        .collect();

    // Two presets only: enough to price the one-time Silhouette build (which lands on the first)
    // and to see the crest-line phantom move with resolution.
    let signet_rows = [("signet 192x96", 192, 96), ("signet 384x144", 384, 144)]
        .iter()
        .map(|&(name, t, p)| row(name, &signet, &lib, t, p))
        .collect();

    Report {
        builtin_ms,
        rows,
        signet_rows,
        threads: std::thread::available_parallelism().map_or(0, |n| n.get()),
    }
}

impl Report {
    /// Fixed-width text, so a logcat dump and the on-screen table are the same thing.
    pub fn to_text(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!(
            "{} cores | AlphaLibrary::builtin {:.0} ms\n\n",
            self.threads, self.builtin_ms
        ));
        s.push_str(&format!(
            "{:<17}{:>9}{:>9}{:>9}{:>9}{:>9}{:>9}\n",
            "preset", "tris", "build", "valid", "integ", "analyze", "stl"
        ));
        for r in self.rows.iter().chain(&self.signet_rows) {
            s.push_str(&format!(
                "{:<17}{:>9}{:>9.0}{:>9.0}{:>9.0}{:>9.0}{:>9.0}\n",
                r.label, r.triangles, r.build_ms, r.validate_ms, r.integrals_ms, r.analyze_ms,
                r.stl_ms
            ));
        }
        s.push_str("\nms. `valid` and `integ` are re-runs of work `build` already did — that is\n");
        s.push_str("their share, not extra. `analyze` runs after every build in the worker.\n");
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scrub_row_builds_a_watertight_mesh_with_relief_on_it() {
        let lib = AlphaLibrary::builtin();
        let d = bench_design(&lib);
        let r = row("scrub", &d, &lib, 160, 80);
        assert!(r.watertight, "the sweep closes in both directions, so it cannot not be");
        assert_eq!(r.triangles, 160 * 80 * 2);
        assert!(r.max_relief_mm > 0.0, "the tiled alpha and milgrain should displace something");
        assert!(r.stl_bytes > 84, "a binary STL is at least a header and a count");
    }

    #[test]
    fn the_signet_design_is_a_signet() {
        let d = signet_design();
        assert_eq!(d.shank.kind, ShankKind::Signet);
    }

    #[test]
    fn the_report_renders_every_row() {
        let report = Report {
            builtin_ms: 12.0,
            rows: vec![Row {
                label: "x".into(),
                triangles: 1,
                build_ms: 1.0,
                validate_ms: 2.0,
                integrals_ms: 3.0,
                analyze_ms: 4.0,
                stl_ms: 5.0,
                stl_bytes: 84,
                max_relief_mm: 0.1,
                watertight: true,
            }],
            signet_rows: Vec::new(),
            threads: 8,
        };
        let text = report.to_text();
        assert!(text.contains("8 cores"));
        assert!(text.contains('x'));
    }

    #[test]
    fn validate_share_is_a_fraction_of_the_workers_job() {
        let r = Row {
            label: "x".into(),
            triangles: 1,
            build_ms: 30.0,
            validate_ms: 10.0,
            integrals_ms: 0.0,
            analyze_ms: 10.0,
            stl_ms: 0.0,
            stl_bytes: 0,
            max_relief_mm: 0.0,
            watertight: true,
        };
        assert_eq!(r.worker_ms(), 40.0);
        assert!((r.validate_share() - 0.25).abs() < 1e-9);
    }
}
