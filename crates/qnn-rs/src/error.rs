//! Error type for the QNN wrapper.

use std::result::Result as StdResult;

pub type Result<T> = StdResult<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("failed to dlopen QNN library '{path}': {source}")]
    Load {
        path: String,
        #[source]
        source: libloading::Error,
    },

    #[error("symbol '{name}' not found in QNN library: {source}")]
    Symbol {
        name: &'static str,
        #[source]
        source: libloading::Error,
    },

    #[error("QNN call '{op}' failed (error 0x{code:x})")]
    Qnn { op: &'static str, code: u64 },

    #[error("required QNN function pointer '{0}' is null in the selected provider")]
    MissingFn(&'static str),

    #[error("no compatible QNN provider found (need API major {expected})")]
    NoProvider { expected: u32 },

    #[error("malformed context binary metadata: {0}")]
    Malformed(&'static str),

    #[error("device execution is not implemented on host; this is on-device milestone D1")]
    Unimplemented,
}
