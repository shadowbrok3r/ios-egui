//! CLIP pack on disk: the HTP context binary and an optional aesthetic head,
//! keyed by the `CLIPV` marker file.

use crate::error::{Error, Result};
use memmap2::Mmap;
use std::path::{Path, PathBuf};

/// The marker file that identifies a CLIP pack.
pub const MARKER: &str = "CLIPV";

/// Files every pack must have (the aesthetic head is optional).
pub const REQUIRED: [&str; 2] = [MARKER, "model.bin"];

/// The optional aesthetic head file inside a pack.
pub const AESTHETIC_FILE: &str = "aesthetic.bin";

/// The optional text-tower context binary inside a pack (enables typed semantic search).
pub const TEXT_MODEL_FILE: &str = "text_model.bin";

/// The optional tokenizer file inside a pack, paired with `text_model.bin`.
pub const TOKENIZER_FILE: &str = "tokenizer.json";

/// LAION aesthetic linear head: `score = dot(weights, emb) + bias` on the L2-normalized embedding.
#[derive(Clone, Debug, PartialEq)]
pub struct AestheticHead {
    pub weights: Vec<f32>,
    pub bias: f32,
}

impl AestheticHead {
    /// Embedding dimension the head expects.
    pub fn dim(&self) -> usize {
        self.weights.len()
    }
}

/// Parse `aesthetic.bin`: little-endian f32 `[w0..w(n-1), bias]`, last float is the bias.
pub fn parse_aesthetic(bytes: &[u8]) -> Result<AestheticHead> {
    if bytes.len() % 4 != 0 || bytes.len() < 8 {
        return Err(Error::BadAesthetic(format!("{} bytes is not >= 2 little-endian f32", bytes.len())));
    }
    let floats: Vec<f32> =
        bytes.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
    let (bias, weights) = floats.split_last().unwrap();
    Ok(AestheticHead { weights: weights.to_vec(), bias: *bias })
}

/// An opened CLIP pack. The context binary is mmapped per run; the aesthetic head is read on demand.
pub struct ClipPack {
    dir: PathBuf,
}

impl ClipPack {
    /// Validate `dir` carries the `CLIPV` marker and `model.bin`.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        if !dir.join(MARKER).exists() {
            return Err(Error::NotClipPack(dir));
        }
        for name in REQUIRED {
            let p = dir.join(name);
            if !p.exists() {
                return Err(Error::MissingFile(p));
            }
        }
        Ok(Self { dir })
    }

    /// True when `dir` carries the `CLIPV` marker.
    pub fn is_clip_pack(dir: impl AsRef<Path>) -> bool {
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

    pub fn model_bin(&self) -> PathBuf {
        self.path("model.bin")
    }

    /// True when the pack ships an aesthetic head.
    pub fn has_aesthetic(&self) -> bool {
        self.path(AESTHETIC_FILE).exists()
    }

    pub fn text_model(&self) -> PathBuf {
        self.path(TEXT_MODEL_FILE)
    }

    pub fn tokenizer_json(&self) -> PathBuf {
        self.path(TOKENIZER_FILE)
    }

    /// True when the pack ships the text tower and its tokenizer (typed semantic search).
    pub fn has_text(&self) -> bool {
        self.path(TEXT_MODEL_FILE).exists() && self.path(TOKENIZER_FILE).exists()
    }

    /// Parse the aesthetic head if present, `None` otherwise.
    pub fn aesthetic(&self) -> Result<Option<AestheticHead>> {
        let p = self.path(AESTHETIC_FILE);
        if !p.exists() {
            return Ok(None);
        }
        Ok(Some(parse_aesthetic(&std::fs::read(p)?)?))
    }

    /// mmap the context binary.
    pub fn map(&self, name: &str) -> Result<Mmap> {
        let f = std::fs::File::open(self.path(name))?;
        Ok(unsafe { Mmap::map(&f)? })
    }
}

impl std::fmt::Debug for ClipPack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClipPack").field("dir", &self.dir).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_marker_is_rejected() {
        let dir = std::env::temp_dir().join("local-clip-pack-empty");
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::remove_file(dir.join(MARKER));
        assert!(matches!(ClipPack::open(&dir), Err(Error::NotClipPack(_))));
        assert!(!ClipPack::is_clip_pack(&dir));
    }

    #[test]
    fn marker_without_model_reports_the_missing_one() {
        let dir = std::env::temp_dir().join("local-clip-pack-marker");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(MARKER), b"").unwrap();
        assert!(ClipPack::is_clip_pack(&dir));
        match ClipPack::open(&dir) {
            Err(Error::MissingFile(p)) => assert!(p.ends_with("model.bin")),
            other => panic!("expected MissingFile, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn aesthetic_is_optional_and_absent_reads_none() {
        let dir = std::env::temp_dir().join("local-clip-pack-noaes");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(MARKER), b"").unwrap();
        std::fs::write(dir.join("model.bin"), b"stub").unwrap();
        let pack = ClipPack::open(&dir).unwrap();
        assert!(!pack.has_aesthetic());
        assert_eq!(pack.aesthetic().unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_aesthetic_roundtrips_weights_and_bias() {
        let mut bytes = Vec::new();
        for f in [1.0f32, 2.0, 3.0, 0.5] {
            bytes.extend_from_slice(&f.to_le_bytes());
        }
        let head = parse_aesthetic(&bytes).unwrap();
        assert_eq!(head.weights, vec![1.0, 2.0, 3.0]);
        assert_eq!(head.bias, 0.5);
        assert_eq!(head.dim(), 3);
        // Too short / non-multiple-of-4 is rejected.
        assert!(matches!(parse_aesthetic(&[1, 2, 3]), Err(Error::BadAesthetic(_))));
        assert!(matches!(parse_aesthetic(&[0, 0, 0, 0]), Err(Error::BadAesthetic(_))));
    }
}
