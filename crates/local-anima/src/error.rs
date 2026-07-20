//! Crate error and result types.

use std::path::PathBuf;
use std::result::Result as StdResult;

pub type Result<T> = StdResult<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("qnn: {0}")]
    Qnn(#[from] qnn_rs::Error),

    #[error("image: {0}")]
    Image(#[from] image::ImageError),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("tokenizer: {0}")]
    Tokenizer(String),

    #[error("{0} is not an Anima model pack (no ANIMA marker)")]
    NotAnimaPack(PathBuf),

    #[error("model pack is missing {0}")]
    MissingFile(PathBuf),

    #[error("token_emb.bin is {bytes} bytes, not a multiple of {hidden} f16 values")]
    BadEmbedTable { bytes: usize, hidden: usize },

    #[error("tensor '{name}': expected {expected} elements, got {got}")]
    ShapeMismatch { name: &'static str, expected: usize, got: usize },

    #[error("graph '{graph}' produced no '{name}' output")]
    MissingOutput { graph: &'static str, name: &'static str },

    #[error("{0}")]
    Msg(String),
}
