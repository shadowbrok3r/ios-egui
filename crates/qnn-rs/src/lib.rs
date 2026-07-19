//! Pure-Rust dlopen wrapper for Qualcomm QNN (AI Engine Direct / QAIRT).
//!
//! Milestone D0 (this crate): load libQnnSystem.so at runtime and parse a
//! Stable Diffusion QNN context binary's metadata — graphs, tensors, shapes,
//! datatypes, and per-tensor scale-offset quantization — on any host. No QNN
//! libraries are linked; everything is resolved via `*_getProviders`.
//!
//! Milestone D1 (on device): [`Context::from_binary`]/[`Context::execute`] and
//! [`set_htp_performance_mode`] drive UNet/VAE on the Snapdragon HTP NPU. The
//! FFI compiles on host but fails at backend init there (no NPU); quant/dequant
//! math ([`quant`]) is host-testable.
//!
//! ```no_run
//! use qnn_rs::{QnnSystem, ContextBinaryInfo};
//! let system = QnnSystem::load("/path/to/libQnnSystem.so")?;
//! let bytes = std::fs::read("/path/to/unet.bin")?;
//! let info = ContextBinaryInfo::parse(&system, &bytes)?;
//! for g in &info.graphs {
//!     println!("graph {} ({} in, {} out)", g.name, g.inputs.len(), g.outputs.len());
//! }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

#[allow(non_upper_case_globals, non_camel_case_types, non_snake_case, dead_code, improper_ctypes)]
mod bindings;

mod device;
mod error;
mod fastrpc;
mod htp;
mod loader;
mod parse;
pub mod quant;
mod types;

pub use device::{prepare_htp_env, set_htp_performance_mode, Context};
pub use error::{Error, Result};
pub use loader::{Backend, QnnSystem};
pub use types::{ContextBinaryInfo, DataType, GraphInfo, ScaleOffset, TensorInfo};

/// Raw bindgen-generated FFI to the QNN C API. Needed for D1 device execution.
pub mod ffi {
    pub use crate::bindings::*;
}
