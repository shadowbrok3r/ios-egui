//! On-device model packs: what is present, and what the device can run.
//!
//! Mirrors the sibling `comfyui-android`'s arrangement rather than inventing a
//! second one. Packs live on shared storage, not in the APK — a CLIP tower is
//! hundreds of megabytes and most people designing a ring do not want one.
//!
//! Everything here compiles without the `local-npu` feature; the scanner is
//! pure filesystem work and stays host-testable, and the feature only gates the
//! crates that actually talk to the NPU. Nothing in a default build links a
//! `local-*` crate, which is the property worth protecting: the QNN runtime libs
//! are gitignored and staged separately, so a build that quietly half-enables
//! this would ship an APK whose NPU features fail with nothing readable on
//! screen.

use std::path::{Path, PathBuf};

/// What a pack is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// CLIP image (and optionally text) tower — library search and near-duplicates.
    Clip,
    /// Stable Diffusion 1.5 — pattern generation.
    Sd15,
    /// A monocular depth estimator — a photograph to a height field.
    Depth,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Clip => "CLIP",
            Kind::Sd15 => "SD 1.5",
            Kind::Depth => "Depth",
        }
    }

    /// What the app can do once this pack is present.
    pub fn buys(self) -> &'static str {
        match self {
            Kind::Clip => "search the library by picture, and flag near-duplicates on import",
            Kind::Sd15 => "generate a tileable pattern from a description",
            Kind::Depth => "read relief out of a photograph instead of its brightness",
        }
    }
}

/// The marker file that identifies each kind, the way `comfyui-android`
/// classifies its own packs.
const CLIP_MARKER: &str = "CLIPV";
const SD15_MARKER: &str = "unet.bin";
const DEPTH_MARKER: &str = "DEPTH";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pack {
    pub kind: Kind,
    pub name: String,
    pub dir: PathBuf,
}

/// Classify one directory. `None` when it is not a pack we know.
pub fn classify(dir: &Path) -> Option<Kind> {
    if dir.join(CLIP_MARKER).is_file() {
        Some(Kind::Clip)
    } else if dir.join(DEPTH_MARKER).is_file() {
        Some(Kind::Depth)
    } else if dir.join(SD15_MARKER).is_file() {
        Some(Kind::Sd15)
    } else {
        None
    }
}

/// Every usable pack directly under `root`, sorted by name.
///
/// An unreadable root yields an empty list rather than an error: "no packs" and
/// "no such directory" mean the same thing to the user.
pub fn scan(root: &Path) -> Vec<Pack> {
    let Ok(rd) = std::fs::read_dir(root) else { return Vec::new() };
    let mut out: Vec<Pack> = rd
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let dir = e.path();
            let name = dir.file_name()?.to_str()?.to_string();
            Some(Pack { kind: classify(&dir)?, name, dir })
        })
        .collect();
    out.sort_by(|a, b| (a.kind.label(), a.name.clone()).cmp(&(b.kind.label(), b.name.clone())));
    out
}

/// Merge several roots; the first directory seen wins.
pub fn scan_many(roots: &[&Path]) -> Vec<Pack> {
    let mut out: Vec<Pack> = Vec::new();
    for root in roots {
        for p in scan(root) {
            if !out.iter().any(|q| q.dir == p.dir) {
                out.push(p);
            }
        }
    }
    out.sort_by(|a, b| (a.kind.label(), a.name.clone()).cmp(&(b.kind.label(), b.name.clone())));
    out
}

/// The first pack of `kind`, if one was found.
pub fn first(packs: &[Pack], kind: Kind) -> Option<&Pack> {
    packs.iter().find(|p| p.kind == kind)
}

/// Whether this build carries the NPU crates at all.
pub const fn built_with_npu() -> bool {
    cfg!(feature = "local-npu")
}

/// One line saying honestly what this build and this device can do.
///
/// Three different failures read identically on screen otherwise — a build
/// without the feature, a device with no HTP, and a device with no packs — and
/// only one of them is fixable by the person holding the phone.
pub fn status(packs: &[Pack], lib_dir: Option<&Path>) -> String {
    if !built_with_npu() {
        return "This build has no on-device models. Rebuild with --features local-npu.".into();
    }
    let runtime = lib_dir.is_some_and(|d| d.join("libQnnHtp.so").is_file());
    if !runtime {
        return "No QNN runtime on this device — the NPU libraries are not in the APK.".into();
    }
    if packs.is_empty() {
        return "NPU ready, no model packs found. Put one in the app's models folder.".into();
    }
    format!(
        "NPU ready · {}",
        packs.iter().map(|p| p.kind.label()).collect::<Vec<_>>().join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rd-npu-{tag}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("scratch");
        d
    }

    fn pack(root: &Path, name: &str, marker: &str) {
        let d = root.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(marker), "").unwrap();
    }

    #[test]
    fn each_marker_classifies_its_own_kind() {
        let r = scratch("kinds");
        pack(&r, "clip-vit", CLIP_MARKER);
        pack(&r, "sd15", SD15_MARKER);
        pack(&r, "midas", DEPTH_MARKER);
        let got = scan(&r);
        assert_eq!(got.len(), 3, "{got:?}");
        assert!(got.iter().any(|p| p.kind == Kind::Clip && p.name == "clip-vit"));
        assert!(got.iter().any(|p| p.kind == Kind::Sd15));
        assert!(got.iter().any(|p| p.kind == Kind::Depth));
    }

    #[test]
    fn a_directory_with_no_marker_is_not_a_pack() {
        let r = scratch("nomarker");
        std::fs::create_dir_all(r.join("random")).unwrap();
        std::fs::write(r.join("random").join("notes.txt"), "x").unwrap();
        assert!(scan(&r).is_empty());
    }

    #[test]
    fn a_missing_root_scans_to_nothing_rather_than_failing() {
        assert!(scan(Path::new("/no/such/models")).is_empty());
    }

    #[test]
    fn merging_roots_keeps_the_first_of_a_duplicate() {
        let a = scratch("merge-a");
        pack(&a, "clip", CLIP_MARKER);
        let b = scratch("merge-b");
        pack(&b, "sd", SD15_MARKER);
        let merged = scan_many(&[&a, &b]);
        assert_eq!(merged.len(), 2);
        // Same root twice must not double-count.
        assert_eq!(scan_many(&[&a, &a]).len(), 1);
    }

    /// The three failures a user can hit must not read the same, because only
    /// one of them is theirs to fix.
    #[test]
    fn the_status_line_tells_the_failures_apart() {
        let no_packs = status(&[], None);
        if built_with_npu() {
            assert!(no_packs.contains("QNN runtime"), "{no_packs}");
            let d = scratch("libs");
            std::fs::write(d.join("libQnnHtp.so"), "").unwrap();
            let ready = status(&[], Some(&d));
            assert!(ready.contains("no model packs"), "{ready}");
            let with = status(
                &[Pack { kind: Kind::Clip, name: "c".into(), dir: d.clone() }],
                Some(&d),
            );
            assert!(with.contains("CLIP"), "{with}");
        } else {
            assert!(no_packs.contains("local-npu"), "{no_packs}");
        }
    }

    #[test]
    fn first_finds_by_kind() {
        let ps = vec![
            Pack { kind: Kind::Sd15, name: "s".into(), dir: "/s".into() },
            Pack { kind: Kind::Clip, name: "c".into(), dir: "/c".into() },
        ];
        assert_eq!(first(&ps, Kind::Clip).map(|p| p.name.as_str()), Some("c"));
        assert!(first(&ps, Kind::Depth).is_none());
    }
}

/// The device side. Everything below needs the NPU, a runtime and a pack, and
/// is compiled only under `local-npu`.
///
/// None of it can be exercised off-device: these paths compile on the host and
/// fail at backend init there, so what CI and a laptop can prove is that the
/// calls type-check against the pack APIs. The behaviour is a device test.
#[cfg(feature = "local-npu")]
pub mod device {
    use super::Pack;
    use std::path::Path;

    /// Bring up the QNN stack once. The order is load-bearing and is the sibling
    /// app's: system, backend, session, and only then the performance mode —
    /// before `Session::new` the DSP appears to crash.
    fn stack(lib_dir: &Path) -> Result<(local_clip::QnnSystem, local_clip::Backend), String> {
        local_clip::prepare_htp_env(lib_dir);
        let system = local_clip::QnnSystem::load(lib_dir.join("libQnnSystem.so"))
            .map_err(|e| format!("QnnSystem: {e}"))?;
        let backend = local_clip::Backend::load(lib_dir.join("libQnnHtp.so"))
            .map_err(|e| format!("Backend: {e}"))?;
        Ok((system, backend))
    }

    /// Embed one alpha, as a PNG's worth of bytes, into a normalized vector.
    pub fn embed_png(pack: &Pack, lib_dir: &Path, png: &[u8]) -> Result<Vec<f32>, String> {
        let (system, backend) = stack(lib_dir)?;
        let session = local_clip::Session::new(&backend).map_err(|e| format!("session: {e}"))?;
        let cp = local_clip::ClipPack::open(&pack.dir).map_err(|e| format!("pack: {e}"))?;
        local_clip::embed_bytes(&cp, &session, &system, png).map_err(|e| format!("embed: {e}"))
    }

    /// Embed a typed query, when the pack carries a text tower.
    ///
    /// Both towers come out of the same pack, so they share a checkpoint and
    /// their embeddings are comparable — the question the issue asked to verify
    /// before promising typed search. A pack without `text_model.bin` simply
    /// fails here and the caller falls back to the substring filter.
    pub fn embed_query(pack: &Pack, lib_dir: &Path, query: &str) -> Result<Vec<f32>, String> {
        let (system, backend) = stack(lib_dir)?;
        let session = local_clip::Session::new(&backend).map_err(|e| format!("session: {e}"))?;
        let cp = local_clip::ClipPack::open(&pack.dir).map_err(|e| format!("pack: {e}"))?;
        local_clip::embed_text(&cp, &session, &system, query)
            .map_err(|e| format!("text: {e}"))
    }

    /// Generate one seamless tile from a description.
    ///
    /// `seamless` rolls the latent between steps so the UNet never sees the wrap
    /// as an edge. The result still has to face the sand: the caller measures it
    /// with `Alpha::min_feature_px` at the layer's own cell size before offering
    /// it, because a tile finer than the detail floor casts as mush however good
    /// it looks on screen.
    pub fn generate_tile(
        pack: &Pack,
        lib_dir: &Path,
        prompt: &str,
        steps: usize,
        seed: u64,
        mut progress: impl FnMut(usize, usize),
    ) -> Result<Vec<u8>, String> {
        local_sd::prepare_htp_env(lib_dir);
        let system = local_sd::QnnSystem::load(lib_dir.join("libQnnSystem.so"))
            .map_err(|e| format!("QnnSystem: {e}"))?;
        let backend = local_sd::Backend::load(lib_dir.join("libQnnHtp.so"))
            .map_err(|e| format!("Backend: {e}"))?;
        // `Session` and `ContextOpts` come from the same `qnn-rs`; local-sd does not
        // re-export them, local-clip does, and they are the same types.
        let session = local_clip::Session::new(&backend).map_err(|e| format!("session: {e}"))?;
        let load = |name: &str| -> Result<Vec<u8>, String> {
            std::fs::read(pack.dir.join(name)).map_err(|e| format!("{name}: {e}"))
        };
        let unet_bytes = load("unet.bin")?;
        let vae_bytes = load("vae_decoder.bin")?;
        let opts = local_clip::ContextOpts::default();
        let unet = session
            .load_context(&system, &unet_bytes, &opts)
            .map_err(|e| format!("unet: {e}"))?;
        let vae = session
            .load_context(&system, &vae_bytes, &opts)
            .map_err(|e| format!("vae: {e}"))?;

        let tok = local_sd::ClipTokenizer::from_file(pack.dir.join("tokenizer.json"))
            .map_err(|e| format!("tokenizer: {e}"))?;
        let clip = local_sd::ClipTextEncoder::from_safetensors(pack.dir.join("clip.safetensors"))
            .map_err(|e| format!("clip: {e}"))?;

        let params = local_sd::Text2ImgParams {
            steps,
            seed,
            seamless: true,
            ..Default::default()
        };
        // A relief tile is a grayscale height map, so the prompt is steered
        // toward one rather than toward a photograph.
        let full = format!("seamless tileable {prompt}, flat lighting, grayscale relief, engraved");
        let img = local_sd::text2img(
            &tok,
            &clip,
            &unet,
            &vae,
            &full,
            "photo, colour, shadow, perspective, vignette",
            &params,
            |s, t, _| progress(s, t),
            None,
        )
        .map_err(|e| format!("generate: {e}"))?;
        img.to_png().map_err(|e| format!("png: {e}"))
    }
}
