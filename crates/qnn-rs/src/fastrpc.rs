//! FastRPC unsigned-PD control. `enable_unsigned_pd` dlopens `libcdsprpc.so` and
//! calls `remote_session_control(DSPRPC_CONTROL_UNSIGNED_MODULE)` so the HTP
//! backend can load the SDK's unsigned Hexagon skel. It sets a process-global
//! applied to FastRPC sessions opened afterward, so it runs before the HTP
//! backend's `backendCreate`. Soft no-op (logged) where `libcdsprpc.so` is
//! absent, e.g. on host. Values sourced from the canonical AOSP FastRPC header
//! `platform/external/fastrpc/inc/remote.h`.
#![allow(dead_code)]

use crate::error::Result;
use libloading::{Library, Symbol};
use std::ffi::c_void;

// remote.h: #define DSPRPC_CONTROL_UNSIGNED_MODULE (2).
const DSPRPC_CONTROL_UNSIGNED_MODULE: u32 = 2;
// remote.h domain ids: ADSP=0, MDSP=1, SDSP=2, CDSP=3.
const CDSP_DOMAIN_ID: i32 = 3;

// remote.h: struct remote_rpc_control_unsigned_module { int domain; int enable; }.
#[repr(C)]
struct RemoteRpcControlUnsignedModule {
    domain: i32,
    enable: i32,
}

// libcdsprpc.so: int remote_session_control(uint32_t req, void* data, uint32_t len).
type RemoteSessionControlFn = unsafe extern "C" fn(u32, *mut c_void, u32) -> i32;

const _: () = assert!(std::mem::size_of::<RemoteRpcControlUnsignedModule>() == 8);
const _: () = assert!(std::mem::offset_of!(RemoteRpcControlUnsignedModule, domain) == 0);
const _: () = assert!(std::mem::offset_of!(RemoteRpcControlUnsignedModule, enable) == 4);

/// Request an unsigned PD on the CDSP so the HTP backend can load the unsigned
/// skel. Soft no-op (logged) if `libcdsprpc.so` or the symbol is unavailable;
/// never panics. Logs whether it was requested and the returned code.
pub(crate) fn enable_unsigned_pd() -> Result<()> {
    let lib = match unsafe { Library::new("libcdsprpc.so") } {
        Ok(l) => l,
        Err(e) => {
            log::warn!(
                "qnn-rs: unsigned PD skipped, libcdsprpc.so not loadable: {e} \
                 (Android targetSdk>=31 needs <uses-native-library android:name=\"libcdsprpc.so\"/>)"
            );
            return Ok(());
        }
    };
    let ctrl: Symbol<RemoteSessionControlFn> = match unsafe { lib.get(b"remote_session_control\0") } {
        Ok(s) => s,
        Err(e) => {
            log::warn!("qnn-rs: unsigned PD skipped, remote_session_control missing: {e}");
            return Ok(());
        }
    };
    let mut data = RemoteRpcControlUnsignedModule { domain: CDSP_DOMAIN_ID, enable: 1 };
    let len = std::mem::size_of::<RemoteRpcControlUnsignedModule>() as u32;
    log::info!("qnn-rs: requesting unsigned PD (domain={CDSP_DOMAIN_ID}) via remote_session_control");
    let rc = unsafe { ctrl(DSPRPC_CONTROL_UNSIGNED_MODULE, &mut data as *mut _ as *mut c_void, len) };
    if rc == 0 {
        log::info!("qnn-rs: unsigned PD enabled, remote_session_control returned 0");
    } else {
        log::warn!("qnn-rs: unsigned PD request failed, remote_session_control returned {rc}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_struct_layout() {
        assert_eq!(std::mem::size_of::<RemoteRpcControlUnsignedModule>(), 8);
        assert_eq!(std::mem::offset_of!(RemoteRpcControlUnsignedModule, domain), 0);
        assert_eq!(std::mem::offset_of!(RemoteRpcControlUnsignedModule, enable), 4);
    }

    #[test]
    fn host_enable_is_ok_noop() {
        assert!(enable_unsigned_pd().is_ok());
    }
}
