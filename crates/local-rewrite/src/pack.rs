//! Rewrite pack on disk: the quantized GGUF model and its tokenizer, keyed by the
//! `RWTR` marker file. Mirrors the WD14/CLIP pack shape.

use crate::error::{Error, Result};
use std::path::{Path, PathBuf};

/// The marker file that identifies a rewrite pack.
pub const MARKER: &str = "RWTR";

/// The quantized model file inside a pack.
pub const MODEL_FILE: &str = "model.gguf";

/// The tokenizer file inside a pack.
pub const TOKENIZER_FILE: &str = "tokenizer.json";

/// Files every pack must have.
pub const REQUIRED: [&str; 3] = [MARKER, MODEL_FILE, TOKENIZER_FILE];

/// An opened rewrite pack: just the validated directory; files are read on load.
#[derive(Clone)]
pub struct RewritePack {
    dir: PathBuf,
}

impl RewritePack {
    /// Validate `dir` carries the `RWTR` marker plus the model and tokenizer.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        if !dir.join(MARKER).exists() {
            return Err(Error::NotRewritePack(dir));
        }
        for name in REQUIRED {
            let p = dir.join(name);
            if !p.exists() {
                return Err(Error::MissingFile(p));
            }
        }
        Ok(Self { dir })
    }

    /// True when `dir` carries the `RWTR` marker.
    pub fn is_rewrite_pack(dir: impl AsRef<Path>) -> bool {
        dir.as_ref().join(MARKER).exists()
    }

    /// The pack directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Path of `name` inside the pack.
    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    pub fn model_gguf(&self) -> PathBuf {
        self.path(MODEL_FILE)
    }

    pub fn tokenizer_json(&self) -> PathBuf {
        self.path(TOKENIZER_FILE)
    }
}

impl std::fmt::Debug for RewritePack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RewritePack").field("dir", &self.dir).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_marker_is_rejected() {
        let dir = std::env::temp_dir().join("local-rewrite-pack-empty");
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::remove_file(dir.join(MARKER));
        assert!(matches!(RewritePack::open(&dir), Err(Error::NotRewritePack(_))));
        assert!(!RewritePack::is_rewrite_pack(&dir));
    }

    #[test]
    fn marker_without_files_reports_the_missing_one() {
        let dir = std::env::temp_dir().join("local-rewrite-pack-marker");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(MARKER), b"").unwrap();
        assert!(RewritePack::is_rewrite_pack(&dir));
        match RewritePack::open(&dir) {
            Err(Error::MissingFile(p)) => assert!(p.ends_with(MODEL_FILE)),
            other => panic!("expected MissingFile, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn full_pack_opens_and_exposes_paths() {
        let dir = std::env::temp_dir().join("local-rewrite-pack-full");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in REQUIRED {
            std::fs::write(dir.join(name), b"stub").unwrap();
        }
        let pack = RewritePack::open(&dir).unwrap();
        assert!(pack.model_gguf().ends_with(MODEL_FILE));
        assert!(pack.tokenizer_json().ends_with(TOKENIZER_FILE));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
