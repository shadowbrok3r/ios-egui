//! Device-execution surface (milestone D1). Declared here so the API shape is
//! fixed, but the bring-up (backendCreate -> deviceCreate -> contextCreateFromBinary
//! -> graphRetrieve -> graphExecute) runs only on the HTP NPU on device. On host
//! these return `Error::Unimplemented`.

use crate::error::{Error, Result};
use crate::loader::Backend;
use crate::loader::QnnSystem;

/// A QNN context created from a binary on a backend. Holds retrieved graph handles.
pub struct Context {
    _private: (),
}

impl Context {
    /// D1: create a backend, device, and context from the binary, then retrieve
    /// each graph handle. HTP-targeted binaries need the HTP backend + skel on
    /// device; the host CPU backend cannot execute them.
    pub fn from_binary(_backend: &Backend, _system: &QnnSystem, _bytes: &[u8]) -> Result<Context> {
        Err(Error::Unimplemented)
    }

    /// D1: run the named graph. Input/output tensors are quantized/dequantized
    /// against the metadata's scale-offset (see `ContextBinaryInfo`).
    pub fn execute(&self, _graph_name: &str) -> Result<()> {
        Err(Error::Unimplemented)
    }
}

/// D1 (device-only): configure the HTP DCVS_V3 power/perf mode via
/// deviceGetInfrastructure -> createPowerConfigId -> setPowerConfig. No-op on host.
pub fn set_htp_performance_mode(_backend: &Backend) -> Result<()> {
    Err(Error::Unimplemented)
}
