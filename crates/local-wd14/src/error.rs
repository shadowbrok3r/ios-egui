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

    #[error("{0} is not a WD14 tagger pack (no WD14 marker)")]
    NotWd14Pack(PathBuf),

    #[error("tagger pack is missing {0}")]
    MissingFile(PathBuf),

    #[error("tags.csv has no usable rows")]
    EmptyTags,

    #[error("model graph has no {0} tensors")]
    NoTensors(&'static str),

    #[error("{0}")]
    Msg(String),
}
