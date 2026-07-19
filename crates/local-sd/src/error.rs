//! Crate error and result types.

use std::result::Result as StdResult;

pub type Result<T> = StdResult<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("qnn: {0}")]
    Qnn(#[from] qnn_rs::Error),

    #[error("candle: {0}")]
    Candle(#[from] candle_core::Error),

    #[error("image: {0}")]
    Image(#[from] image::ImageError),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("tokenizer: {0}")]
    Tokenizer(String),

    #[error("unsupported dtype {dtype:?} for tensor '{name}'")]
    UnsupportedDataType { name: String, dtype: qnn_rs::DataType },

    #[error("fixed-point tensor '{0}' has no quantization params")]
    MissingQuant(String),

    #[error("tensor '{name}': expected {expected} elements, got {got}")]
    ShapeMismatch { name: String, expected: usize, got: usize },

    #[error("graph in '{bin}' has no {role} tensor (expected {elems} elements)")]
    IoNotFound { bin: &'static str, role: &'static str, elems: u64 },

    #[error("{0}")]
    Msg(String),
}
