//! dlopen-based loading of the QNN system library and a backend library, and
//! selection of a compatible interface provider from `*_getProviders`.

use crate::bindings as ffi;
use crate::error::{Error, Result};
use crate::types::cstr_opt;
use libloading::{Library, Symbol};
use std::ffi::OsStr;
use std::ptr;

type SystemGetProviders =
    unsafe extern "C" fn(*mut *mut *const ffi::QnnSystemInterface_t, *mut u32) -> ffi::Qnn_ErrorHandle_t;

type BackendGetProviders =
    unsafe extern "C" fn(*mut *mut *const ffi::QnnInterface_t, *mut u32) -> ffi::Qnn_ErrorHandle_t;

/// libQnnSystem.so: provides context-binary metadata parsing (no backend needed).
pub struct QnnSystem {
    _lib: Library,
    pub(crate) ftab: ffi::QnnSystemInterface_ImplementationV1_12_t,
    provider_name: String,
    api_version: (u32, u32, u32),
}

impl QnnSystem {
    pub fn load<P: AsRef<OsStr>>(path: P) -> Result<Self> {
        let path_str = path.as_ref().to_string_lossy().into_owned();
        let lib = unsafe { Library::new(path.as_ref()) }
            .map_err(|source| Error::Load { path: path_str, source })?;

        let get: Symbol<SystemGetProviders> = unsafe { lib.get(b"QnnSystemInterface_getProviders\0") }
            .map_err(|source| Error::Symbol { name: "QnnSystemInterface_getProviders", source })?;

        let mut providers: *mut *const ffi::QnnSystemInterface_t = ptr::null_mut();
        let mut num: u32 = 0;
        let rc = unsafe { get(&mut providers, &mut num) };
        if rc != ffi::QNN_SUCCESS as u64 {
            return Err(Error::Qnn { op: "QnnSystemInterface_getProviders", code: rc });
        }
        if providers.is_null() || num == 0 {
            return Err(Error::NoProvider { expected: ffi::QNN_SYSTEM_API_VERSION_MAJOR });
        }

        let mut best: Option<(*const ffi::QnnSystemInterface_t, (u32, u32, u32))> = None;
        for i in 0..num as isize {
            let p = unsafe { *providers.offset(i) };
            if p.is_null() {
                continue;
            }
            let v = unsafe { (*p).systemApiVersion };
            if v.major != ffi::QNN_SYSTEM_API_VERSION_MAJOR {
                continue;
            }
            let ver = (v.major, v.minor, v.patch);
            match best {
                Some((_, bv)) if bv >= ver => {}
                _ => best = Some((p, ver)),
            }
        }
        let (p, api_version) = best.ok_or(Error::NoProvider { expected: ffi::QNN_SYSTEM_API_VERSION_MAJOR })?;
        let provider_name = unsafe { cstr_opt((*p).providerName) }.unwrap_or_default();
        let ftab = unsafe { (*p).__bindgen_anon_1.v1_12 };
        Ok(QnnSystem { _lib: lib, ftab, provider_name, api_version })
    }

    pub fn provider_name(&self) -> &str {
        &self.provider_name
    }

    pub fn api_version(&self) -> (u32, u32, u32) {
        self.api_version
    }
}

/// A QNN backend library (libQnnHtp.so on device, libQnnCpu.so on host). The
/// selected provider's function table drives context/graph execution (D1).
pub struct Backend {
    _lib: Library,
    // Function table for D1 device execution (contextCreateFromBinary/graphExecute).
    pub(crate) ftab: ffi::QnnInterface_ImplementationV2_37_t,
    provider_name: String,
    backend_id: u32,
    api_version: (u32, u32, u32),
}

impl Backend {
    pub fn load<P: AsRef<OsStr>>(path: P) -> Result<Self> {
        let path_str = path.as_ref().to_string_lossy().into_owned();
        let lib = unsafe { Library::new(path.as_ref()) }
            .map_err(|source| Error::Load { path: path_str, source })?;

        let get: Symbol<BackendGetProviders> = unsafe { lib.get(b"QnnInterface_getProviders\0") }
            .map_err(|source| Error::Symbol { name: "QnnInterface_getProviders", source })?;

        let mut providers: *mut *const ffi::QnnInterface_t = ptr::null_mut();
        let mut num: u32 = 0;
        let rc = unsafe { get(&mut providers, &mut num) };
        if rc != ffi::QNN_SUCCESS as u64 {
            return Err(Error::Qnn { op: "QnnInterface_getProviders", code: rc });
        }
        if providers.is_null() || num == 0 {
            return Err(Error::NoProvider { expected: ffi::QNN_API_VERSION_MAJOR });
        }

        let mut best: Option<(*const ffi::QnnInterface_t, (u32, u32, u32))> = None;
        for i in 0..num as isize {
            let p = unsafe { *providers.offset(i) };
            if p.is_null() {
                continue;
            }
            let v = unsafe { (*p).apiVersion.coreApiVersion };
            if v.major != ffi::QNN_API_VERSION_MAJOR {
                continue;
            }
            let ver = (v.major, v.minor, v.patch);
            match best {
                Some((_, bv)) if bv >= ver => {}
                _ => best = Some((p, ver)),
            }
        }
        let (p, api_version) = best.ok_or(Error::NoProvider { expected: ffi::QNN_API_VERSION_MAJOR })?;
        let provider_name = unsafe { cstr_opt((*p).providerName) }.unwrap_or_default();
        let backend_id = unsafe { (*p).backendId };
        let ftab = unsafe { (*p).__bindgen_anon_1.v2_37 };
        Ok(Backend { _lib: lib, ftab, provider_name, backend_id, api_version })
    }

    pub fn provider_name(&self) -> &str {
        &self.provider_name
    }

    pub fn backend_id(&self) -> u32 {
        self.backend_id
    }

    pub fn api_version(&self) -> (u32, u32, u32) {
        self.api_version
    }
}
