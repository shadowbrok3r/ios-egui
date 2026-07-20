//! Anima model pack on disk: context binaries, tokenizers, the f16 Qwen
//! embedding table, and config.json defaults, keyed by the `ANIMA` marker file.

use crate::error::{Error, Result};
use crate::text::{EmbedTable, QWEN_HIDDEN};
use crate::tokenizer::AnimaTokenizers;
use memmap2::Mmap;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// The marker file that identifies an Anima pack.
pub const MARKER: &str = "ANIMA";

/// Files every pack must have for text2img.
pub const REQUIRED: [&str; 8] = [
    MARKER,
    "unet_part1.bin",
    "unet_part2.bin",
    "clip.bin",
    "vae_decoder.bin",
    "token_emb.bin",
    "tokenizer.json",
    "tokenizer_t5.json",
];

/// config.json defaults.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct AnimaConfig {
    pub default_prompt: String,
    pub default_negative_prompt: String,
    pub default_scheduler: String,
    pub default_steps: usize,
    pub default_cfg: f32,
    pub default_width: usize,
    pub default_height: usize,
}

impl Default for AnimaConfig {
    fn default() -> Self {
        Self {
            default_prompt: String::new(),
            default_negative_prompt: String::new(),
            default_scheduler: "euler".into(),
            default_steps: 10,
            default_cfg: 1.0,
            default_width: 1024,
            default_height: 1024,
        }
    }
}

/// An opened model pack. The embedding table stays mmapped for the pack's life.
pub struct AnimaPack {
    dir: PathBuf,
    config: AnimaConfig,
    token_emb: Mmap,
}

impl AnimaPack {
    /// Validate `dir` and mmap `token_emb.bin`.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        if !dir.join(MARKER).exists() {
            return Err(Error::NotAnimaPack(dir));
        }
        for name in REQUIRED {
            let p = dir.join(name);
            if !p.exists() {
                return Err(Error::MissingFile(p));
            }
        }
        let cfg_path = dir.join("config.json");
        let config = if cfg_path.exists() {
            serde_json::from_slice(&std::fs::read(&cfg_path)?)?
        } else {
            AnimaConfig::default()
        };
        let token_emb = mmap(&dir.join("token_emb.bin"))?;
        EmbedTable::new(&token_emb, QWEN_HIDDEN)?;
        Ok(Self { dir, config, token_emb })
    }

    /// True when `dir` carries the `ANIMA` marker.
    pub fn is_anima_pack(dir: impl AsRef<Path>) -> bool {
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

    pub fn unet_part1(&self) -> PathBuf {
        self.path("unet_part1.bin")
    }

    pub fn unet_part2(&self) -> PathBuf {
        self.path("unet_part2.bin")
    }

    pub fn clip(&self) -> PathBuf {
        self.path("clip.bin")
    }

    pub fn vae_decoder(&self) -> PathBuf {
        self.path("vae_decoder.bin")
    }

    /// `vae_encoder.bin`, present only in packs that support img2img.
    pub fn vae_encoder(&self) -> Option<PathBuf> {
        let p = self.path("vae_encoder.bin");
        p.exists().then_some(p)
    }

    /// config.json defaults.
    pub fn config(&self) -> &AnimaConfig {
        &self.config
    }

    /// The `[vocab, 1024]` f16 Qwen embedding table.
    pub fn token_emb(&self) -> EmbedTable<'_> {
        EmbedTable::new(&self.token_emb, QWEN_HIDDEN).expect("validated in open")
    }

    /// Load both tokenizers from this pack.
    pub fn tokenizers(&self) -> Result<AnimaTokenizers> {
        AnimaTokenizers::from_dir(&self.dir)
    }

    /// mmap a context binary from this pack.
    pub fn map(&self, name: &str) -> Result<Mmap> {
        mmap(&self.path(name))
    }
}

impl std::fmt::Debug for AnimaPack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnimaPack").field("dir", &self.dir).field("config", &self.config).finish()
    }
}

fn mmap(path: &Path) -> Result<Mmap> {
    let f = std::fs::File::open(path)?;
    Ok(unsafe { Mmap::map(&f)? })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_marker_is_rejected() {
        let dir = std::env::temp_dir().join("local-anima-pack-test-empty");
        let _ = std::fs::create_dir_all(&dir);
        assert!(matches!(AnimaPack::open(&dir), Err(Error::NotAnimaPack(_))));
        assert!(!AnimaPack::is_anima_pack(&dir));
    }

    #[test]
    fn marker_without_files_reports_the_missing_one() {
        let dir = std::env::temp_dir().join("local-anima-pack-test-marker");
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join(MARKER), b"").unwrap();
        assert!(AnimaPack::is_anima_pack(&dir));
        match AnimaPack::open(&dir) {
            Err(Error::MissingFile(p)) => assert!(p.ends_with("unet_part1.bin")),
            other => panic!("expected MissingFile, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_defaults_match_the_shipped_pack() {
        let c = AnimaConfig::default();
        assert_eq!((c.default_width, c.default_height), (1024, 1024));
        assert_eq!(c.default_steps, 10);
        assert_eq!(c.default_cfg, 1.0);
        assert_eq!(c.default_scheduler, "euler");
    }

    #[test]
    fn config_json_parses_the_shipped_keys() {
        let json = br#"{"default_prompt":"p","default_negative_prompt":"n","default_scheduler":"euler","default_steps":10,"default_cfg":1}"#;
        let c: AnimaConfig = serde_json::from_slice(json).unwrap();
        assert_eq!(c.default_prompt, "p");
        assert_eq!(c.default_steps, 10);
        assert_eq!(c.default_cfg, 1.0);
        assert_eq!(c.default_width, 1024);
    }
}
