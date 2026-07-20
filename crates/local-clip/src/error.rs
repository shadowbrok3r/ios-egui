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

    #[error("{0} is not a CLIP pack (no CLIPV marker)")]
    NotClipPack(PathBuf),

    #[error("CLIP pack is missing {0}")]
    MissingFile(PathBuf),

    #[error("aesthetic.bin is malformed: {0}")]
    BadAesthetic(String),

    #[error("model graph has no {0} tensors")]
    NoTensors(&'static str),

    #[error("tokenizer: {0}")]
    Tokenizer(String),

    #[error("{0}")]
    Msg(String),
}
