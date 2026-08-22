//! Exports built off the UI thread: one thread per job, the file written
//! there, the share sheet opened on the UI thread once it lands. Export
//! jobs are never queued behind preview builds, so none is ever dropped as
//! stale.

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;

use ringdesign_core::alpha::AlphaLibrary;
use ringdesign_core::mesh::BuildParams;
use ringdesign_core::RingDesign;

use crate::ring::METAL_TINT;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportKind {
    Stl,
    ThreeMf,
    Glb,
    Sheet,
    Render,
    Turntable,
}

impl ExportKind {
    pub fn ext(self) -> &'static str {
        match self {
            ExportKind::Stl => ".stl",
            ExportKind::ThreeMf => ".3mf",
            ExportKind::Glb => ".glb",
            ExportKind::Sheet => "_sheet.html",
            ExportKind::Render => ".png",
            ExportKind::Turntable => ".gif",
        }
    }

    /// Explicit types: MediaProvider renames a file whose extension disagrees
    /// with its type, and the generic table has no `stl` entry.
    pub fn mime(self) -> &'static str {
        match self {
            ExportKind::Stl => "model/stl",
            ExportKind::ThreeMf => "model/3mf",
            ExportKind::Glb => "model/gltf-binary",
            ExportKind::Sheet => "text/html",
            ExportKind::Render => "image/png",
            ExportKind::Turntable => "image/gif",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ExportKind::Stl => "STL",
            ExportKind::ThreeMf => "3MF",
            ExportKind::Glb => "GLB",
            ExportKind::Sheet => "casting sheet",
            ExportKind::Render => "render",
            ExportKind::Turntable => "turntable",
        }
    }
}

pub struct ExportJob {
    pub kind: ExportKind,
    pub path: PathBuf,
    pub design: RingDesign,
    pub lib: Arc<AlphaLibrary>,
    pub params: BuildParams,
    /// Patternmaker's shrink as `(percent, metal name)`; the file is cut
    /// oversize and named as a pattern.
    pub shrink: Option<(f64, String)>,
    /// The app's name and version, on the casting sheet.
    pub generator: String,
}

pub struct ExportDone {
    pub kind: ExportKind,
    pub path: PathBuf,
    pub name: String,
    pub status: String,
    pub ok: bool,
}

/// Builds and writes the file; the caller shares it.
pub fn run(job: ExportJob) -> ExportDone {
    let name = job
        .path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| format!("ring{}", job.kind.ext()));
    let result = write(&job).map_err(|e| e.to_string());
    let (ok, status) = match result {
        Ok(s) => (true, s),
        Err(e) => (false, format!("{} failed: {e}", job.kind.label())),
    };
    ExportDone { kind: job.kind, path: job.path, name, status, ok }
}

fn write(job: &ExportJob) -> Result<String, Box<dyn std::error::Error>> {
    use ringdesign_core::metal;
    if let Some(dir) = job.path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let out = ringdesign_core::mesh::build(&job.design, &job.lib, job.params);
    let mb = |bytes: usize| bytes as f64 / 1048576.0;
    Ok(match job.kind {
        ExportKind::Stl | ExportKind::ThreeMf => {
            let (mesh, name) = match &job.shrink {
                Some((pct, metal)) => (
                    out.mesh.scaled(metal::pattern_scale(*pct)),
                    format!("{} [pattern +{pct:.1}% for {metal}]", job.design.name),
                ),
                None => (out.mesh.clone(), job.design.name.clone()),
            };
            let bytes = if job.kind == ExportKind::ThreeMf {
                ringdesign_core::threemf::write_3mf(&job.path, &mesh, &name, &job.design.size.display())?
            } else {
                ringdesign_core::stl::write_stl(&job.path, &mesh, &name)?
            };
            format!("{} tris · {:.1} MB", out.report.validation.triangle_count, mb(bytes))
        }
        ExportKind::Glb => {
            let bytes = ringdesign_core::gltf::write_glb(&job.path, &out.mesh, &job.design.name, METAL_TINT)?;
            format!("GLB · {:.1} MB", mb(bytes))
        }
        ExportKind::Sheet => {
            let field = ringdesign_core::castability::attributed_field_report(
                &job.design,
                &job.lib,
                &job.design.draft,
                160,
                112,
            );
            let stones = ringdesign_core::stones::report(&job.design, field.parting_z_mm);
            let dfm = ringdesign_core::dfm::findings(&job.design);
            let page = ringdesign_core::spec::html(&job.design, &out.report, &field, stones.as_ref(), &dfm, &job.generator);
            std::fs::write(&job.path, page)?;
            "casting sheet".into()
        }
        ExportKind::Render => {
            ringdesign_core::render::write_png(&job.path, &out.mesh, 0.55, 1.12, 1280, METAL_TINT)?;
            "render".into()
        }
        ExportKind::Turntable => {
            ringdesign_core::render::write_turntable_gif(&job.path, &out.mesh, 36, 480, METAL_TINT)?;
            "turntable".into()
        }
    })
}

/// Runs the job on its own thread; the receiver yields once.
pub fn spawn(job: ExportJob, ctx: egui::Context) -> Receiver<ExportDone> {
    let (tx, rx) = channel();
    let name = format!("ring-export-{}", job.kind.label());
    std::thread::Builder::new()
        .name(name)
        .spawn(move || {
            let _ = tx.send(run(job));
            ctx.request_repaint();
        })
        .expect("spawn export thread");
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_name_their_files_and_types() {
        let all = [ExportKind::Stl, ExportKind::ThreeMf, ExportKind::Glb, ExportKind::Sheet, ExportKind::Render, ExportKind::Turntable];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a.ext(), b.ext());
                assert_ne!(a.mime(), b.mime());
            }
        }
        assert_eq!(ExportKind::Stl.mime(), "model/stl");
        assert_eq!(ExportKind::Sheet.ext(), "_sheet.html");
    }

    #[test]
    fn an_export_writes_its_file_and_reports() {
        let dir = std::env::temp_dir().join(format!("rd-export-{}", std::process::id()));
        let lib = Arc::new(AlphaLibrary::builtin());
        let small = BuildParams { theta_steps: 96, profile_steps: 48, ..Default::default() };
        let job = |kind: ExportKind, shrink: Option<(f64, String)>| ExportJob {
            kind,
            path: dir.join(format!("ring{}", kind.ext())),
            design: RingDesign::default(),
            lib: lib.clone(),
            params: small,
            shrink,
            generator: "test".into(),
        };
        let stl = run(job(ExportKind::Stl, None));
        assert!(stl.ok, "{}", stl.status);
        assert!(std::fs::metadata(&stl.path).unwrap().len() > 84);
        assert_eq!(stl.name, "ring.stl");
        let pattern = run(job(ExportKind::Stl, Some((1.9, "Silver 925".into()))));
        assert!(pattern.ok);
        assert!(std::fs::read(&pattern.path).unwrap().len() >= std::fs::read(&stl.path).unwrap().len());
        let tmf = run(job(ExportKind::ThreeMf, None));
        assert!(tmf.ok && std::fs::read(&tmf.path).unwrap().starts_with(b"PK"));
        let glb = run(job(ExportKind::Glb, None));
        assert!(glb.ok && std::fs::read(&glb.path).unwrap().starts_with(b"glTF"));
        let sheet = run(job(ExportKind::Sheet, None));
        assert!(sheet.ok);
        assert!(std::fs::read_to_string(&sheet.path).unwrap().to_lowercase().contains("<html"));
        let bad = run(ExportJob { path: PathBuf::from("/proc/no/such/dir/ring.stl"), ..job(ExportKind::Stl, None) });
        assert!(!bad.ok && bad.status.starts_with("STL failed"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
