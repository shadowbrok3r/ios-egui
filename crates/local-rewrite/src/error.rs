//! Crate error and result types.

use std::path::PathBuf;
use std::result::Result as StdResult;

pub type Result<T> = StdResult<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("candle: {0}")]
    Candle(#[from] candle_core::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("tokenizer: {0}")]
    Tokenizer(String),

    #[error("{0} is not a rewrite pack (no RWTR marker)")]
    NotRewritePack(PathBuf),

    #[error("rewrite pack is missing {0}")]
    MissingFile(PathBuf),

    #[error("unsupported GGUF architecture {0:?} (expected qwen2)")]
    UnsupportedArch(String),

    #[error("{0}")]
    Msg(String),
}
