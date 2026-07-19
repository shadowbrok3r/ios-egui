//! Device-execution surface (milestone D1): backendCreate -> deviceCreate ->
//! contextCreateFromBinary -> graphRetrieve -> graphExecute on the HTP NPU,
//! plus HTP DCVS_V3 burst-mode setup. The FFI compiles on host and device; on
//! host it fails at backend init because the HTP backend/skel are absent.
#![allow(unsafe_op_in_unsafe_fn)]

use crate::bindings as ffi;
use crate::error::{Error, Result};
use crate::htp;
use crate::loader::{Backend, QnnSystem};
use crate::quant;
use crate::types::{ContextBinaryInfo, DataType, ScaleOffset, TensorInfo};
use std::collections::HashMap;
use std::ffi::{c_char, c_void, CString};
use std::path::Path;
use std::ptr;

fn ok(code: ffi::Qnn_ErrorHandle_t) -> bool {
    code == ffi::QNN_SUCCESS as ffi::Qnn_ErrorHandle_t
}

struct GraphHandle {
    name: String,
    handle: ffi::Qnn_GraphHandle_t,
}

/// A QNN context created from a binary on a backend, with its graph handles.
/// Borrows the `Backend` so the loaded library (and thus the captured function
/// pointers) outlive every call made through this context.
pub struct Context<'b> {
    backend: &'b Backend,
    backend_handle: ffi::Qnn_BackendHandle_t,
    device_handle: ffi::Qnn_DeviceHandle_t,
    context_handle: ffi::Qnn_ContextHandle_t,
    graphs: Vec<GraphHandle>,
    info: ContextBinaryInfo,
}

impl<'b> Context<'b> {
    /// D1: create backend/device/context from the binary, then retrieve each
    /// graph handle. HTP-targeted binaries need the HTP backend + skel on
    /// device; the host CPU backend cannot execute them.
    pub fn from_binary(backend: &'b Backend, system: &QnnSystem, bytes: &[u8]) -> Result<Context<'b>> {
        let info = ContextBinaryInfo::parse(system, bytes)?;
        let ft = &backend.ftab;

        let backend_create = ft.backendCreate.ok_or(Error::MissingFn("backendCreate"))?;
        let context_create = ft.contextCreateFromBinary.ok_or(Error::MissingFn("contextCreateFromBinary"))?;
        let graph_retrieve = ft.graphRetrieve.ok_or(Error::MissingFn("graphRetrieve"))?;

        // Request an unsigned PD before the HTP backend opens its DSP session.
        crate::fastrpc::enable_unsigned_pd()?;

        let mut backend_handle: ffi::Qnn_BackendHandle_t = ptr::null_mut();
        let rc = unsafe { backend_create(ptr::null_mut(), ptr::null_mut(), &mut backend_handle) };
        if !ok(rc) {
            return Err(Error::Qnn { op: "backendCreate", code: rc });
        }

        let mut device_handle: ffi::Qnn_DeviceHandle_t = ptr::null_mut();
        if let Some(device_create) = ft.deviceCreate {
            let rc = unsafe { device_create(ptr::null_mut(), ptr::null_mut(), &mut device_handle) };
            if !ok(rc) {
                unsafe { free_all(ft, ptr::null_mut(), ptr::null_mut(), backend_handle) };
                return Err(Error::Qnn { op: "deviceCreate", code: rc });
            }
        }

        let mut context_handle: ffi::Qnn_ContextHandle_t = ptr::null_mut();
        let rc = unsafe {
            context_create(
                backend_handle,
                device_handle,
                ptr::null_mut(),
                bytes.as_ptr() as *const c_void,
                bytes.len() as ffi::Qnn_ContextBinarySize_t,
                &mut context_handle,
                ptr::null_mut(),
            )
        };
        if !ok(rc) {
            unsafe { free_all(ft, ptr::null_mut(), device_handle, backend_handle) };
            return Err(Error::Qnn { op: "contextCreateFromBinary", code: rc });
        }

        let mut graphs = Vec::with_capacity(info.graphs.len());
        for g in &info.graphs {
            let cname = CString::new(g.name.as_str()).map_err(|_| Error::Malformed("graph name has interior NUL"))?;
            let mut handle: ffi::Qnn_GraphHandle_t = ptr::null_mut();
            let rc = unsafe { graph_retrieve(context_handle, cname.as_ptr(), &mut handle) };
            if !ok(rc) {
                unsafe { free_all(ft, context_handle, device_handle, backend_handle) };
                return Err(Error::Qnn { op: "graphRetrieve", code: rc });
            }
            graphs.push(GraphHandle { name: g.name.clone(), handle });
        }

        Ok(Context { backend, backend_handle, device_handle, context_handle, graphs, info })
    }

    /// Parsed metadata this context was built from.
    pub fn info(&self) -> &ContextBinaryInfo {
        &self.info
    }

    /// D1: run `graph_name`. Inputs are named f32 slices, quantized to each
    /// tensor's dtype via its scale-offset; outputs are dequantized back to f32
    /// and returned keyed by tensor name.
    pub fn execute(&self, graph_name: &str, inputs: &[(&str, &[f32])]) -> Result<HashMap<String, Vec<f32>>> {
        let graph = self
            .graphs
            .iter()
            .find(|g| g.name == graph_name)
            .ok_or_else(|| Error::GraphNotFound(graph_name.to_string()))?;
        let ginfo = self
            .info
            .graphs
            .iter()
            .find(|g| g.name == graph_name)
            .ok_or_else(|| Error::GraphNotFound(graph_name.to_string()))?;
        let graph_execute = self.backend.ftab.graphExecute.ok_or(Error::MissingFn("graphExecute"))?;

        // Backing storage for the IO tensors; must outlive graphExecute.
        let mut in_bytes: Vec<Vec<u8>> = Vec::with_capacity(ginfo.inputs.len());
        let mut in_dims: Vec<Vec<u32>> = Vec::with_capacity(ginfo.inputs.len());
        let mut in_names: Vec<CString> = Vec::with_capacity(ginfo.inputs.len());
        for t in &ginfo.inputs {
            let data = find_input(inputs, &t.name)?;
            let expected = t.elem_count();
            if data.len() as u64 != expected {
                return Err(Error::ShapeMismatch { name: t.name.clone(), expected, got: data.len() });
            }
            in_bytes.push(quantize_input(t, data)?);
            in_dims.push(t.dims.clone());
            in_names.push(cstring(&t.name)?);
        }

        let mut out_bytes: Vec<Vec<u8>> = Vec::with_capacity(ginfo.outputs.len());
        let mut out_dims: Vec<Vec<u32>> = Vec::with_capacity(ginfo.outputs.len());
        let mut out_names: Vec<CString> = Vec::with_capacity(ginfo.outputs.len());
        for t in &ginfo.outputs {
            let width = t
                .dtype
                .byte_width()
                .ok_or_else(|| Error::UnsupportedDataType { kind: "output", name: t.name.clone(), dtype: t.dtype })?;
            out_bytes.push(vec![0u8; t.elem_count() as usize * width as usize]);
            out_dims.push(t.dims.clone());
            out_names.push(cstring(&t.name)?);
        }

        // Storage is now fixed; take pointers into it for the tensor structs.
        let mut input_tensors = Vec::with_capacity(ginfo.inputs.len());
        for (i, t) in ginfo.inputs.iter().enumerate() {
            input_tensors.push(unsafe {
                make_tensor(
                    t,
                    ffi::Qnn_TensorType_t::QNN_TENSOR_TYPE_APP_WRITE,
                    in_names[i].as_ptr(),
                    in_dims[i].as_ptr() as *mut u32,
                    in_bytes[i].as_ptr() as *mut c_void,
                    in_bytes[i].len() as u32,
                )
            });
        }
        let mut output_tensors = Vec::with_capacity(ginfo.outputs.len());
        for (i, t) in ginfo.outputs.iter().enumerate() {
            output_tensors.push(unsafe {
                make_tensor(
                    t,
                    ffi::Qnn_TensorType_t::QNN_TENSOR_TYPE_APP_READ,
                    out_names[i].as_ptr(),
                    out_dims[i].as_ptr() as *mut u32,
                    out_bytes[i].as_mut_ptr() as *mut c_void,
                    out_bytes[i].len() as u32,
                )
            });
        }

        let rc = unsafe {
            graph_execute(
                graph.handle,
                input_tensors.as_ptr(),
                input_tensors.len() as u32,
                output_tensors.as_mut_ptr(),
                output_tensors.len() as u32,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if !ok(rc) {
            return Err(Error::Qnn { op: "graphExecute", code: rc });
        }

        let mut out = HashMap::with_capacity(ginfo.outputs.len());
        for (i, t) in ginfo.outputs.iter().enumerate() {
            out.insert(t.name.clone(), dequantize_output(t, &out_bytes[i])?);
        }
        Ok(out)
    }
}

impl Drop for Context<'_> {
    fn drop(&mut self) {
        unsafe { free_all(&self.backend.ftab, self.context_handle, self.device_handle, self.backend_handle) };
    }
}

/// Free context/device/backend handles that are non-null, in reverse order.
unsafe fn free_all(
    ft: &ffi::QnnInterface_ImplementationV2_37_t,
    context: ffi::Qnn_ContextHandle_t,
    device: ffi::Qnn_DeviceHandle_t,
    backend: ffi::Qnn_BackendHandle_t,
) {
    if !context.is_null() {
        if let Some(f) = ft.contextFree {
            f(context, ptr::null_mut());
        }
    }
    if !device.is_null() {
        if let Some(f) = ft.deviceFree {
            f(device);
        }
    }
    if !backend.is_null() {
        if let Some(f) = ft.backendFree {
            f(backend);
        }
    }
}

/// Build a V2 `Qnn_Tensor_t` pointing at caller-owned storage. The `name`,
/// `dims`, and `data` pointers must outlive every use of the returned tensor.
unsafe fn make_tensor(
    t: &TensorInfo,
    ttype: ffi::Qnn_TensorType_t,
    name: *const c_char,
    dims: *mut u32,
    data: *mut c_void,
    data_size: u32,
) -> ffi::Qnn_Tensor_t {
    let mut tensor: ffi::Qnn_Tensor_t = std::mem::zeroed();
    tensor.version = ffi::Qnn_TensorVersion_t::QNN_TENSOR_VERSION_2;
    let v2 = &mut tensor.__bindgen_anon_1.v2;
    v2.id = t.id;
    v2.name = name;
    v2.type_ = ttype;
    v2.dataFormat = ffi::QNN_TENSOR_DATA_FORMAT_DENSE;
    v2.dataType = t.dtype.to_raw();
    v2.quantizeParams = quant_params(t.quant);
    v2.rank = t.dims.len() as u32;
    v2.dimensions = dims;
    v2.memType = ffi::Qnn_TensorMemType_t::QNN_TENSORMEMTYPE_RAW;
    v2.__bindgen_anon_1.clientBuf = ffi::Qnn_ClientBuffer_t { data, dataSize: data_size };
    tensor
}

fn quant_params(q: Option<ScaleOffset>) -> ffi::Qnn_QuantizeParams_t {
    let mut qp: ffi::Qnn_QuantizeParams_t = unsafe { std::mem::zeroed() };
    match q {
        Some(so) => {
            qp.encodingDefinition = ffi::Qnn_Definition_t::QNN_DEFINITION_DEFINED;
            qp.quantizationEncoding = ffi::Qnn_QuantizationEncoding_t::QNN_QUANTIZATION_ENCODING_SCALE_OFFSET;
            qp.__bindgen_anon_1.scaleOffsetEncoding = ffi::Qnn_ScaleOffset_t { scale: so.scale, offset: so.offset };
        }
        None => {
            qp.encodingDefinition = ffi::Qnn_Definition_t::QNN_DEFINITION_UNDEFINED;
            qp.quantizationEncoding = ffi::Qnn_QuantizationEncoding_t::QNN_QUANTIZATION_ENCODING_UNDEFINED;
        }
    }
    qp
}

fn find_input<'a>(inputs: &'a [(&str, &[f32])], name: &str) -> Result<&'a [f32]> {
    inputs
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, d)| *d)
        .ok_or_else(|| Error::MissingInput(name.to_string()))
}

fn cstring(s: &str) -> Result<CString> {
    CString::new(s).map_err(|_| Error::Malformed("tensor name has interior NUL"))
}

fn require_quant(t: &TensorInfo) -> Result<ScaleOffset> {
    t.quant.ok_or(Error::Malformed("fixed-point tensor missing quantization params"))
}

fn quantize_input(t: &TensorInfo, data: &[f32]) -> Result<Vec<u8>> {
    use DataType::*;
    let bytes = match t.dtype {
        Float32 => data.iter().flat_map(|&x| x.to_le_bytes()).collect(),
        Int8 => data.iter().map(|&x| x.round() as i8 as u8).collect(),
        Int16 => data.iter().flat_map(|&x| (x.round() as i16).to_le_bytes()).collect(),
        Int32 => data.iter().flat_map(|&x| (x.round() as i32).to_le_bytes()).collect(),
        UInt8 => data.iter().map(|&x| x.round().clamp(0.0, 255.0) as u8).collect(),
        UInt16 => data.iter().flat_map(|&x| (x.round().clamp(0.0, 65535.0) as u16).to_le_bytes()).collect(),
        UInt32 => data.iter().flat_map(|&x| (x.round().max(0.0) as u32).to_le_bytes()).collect(),
        UFixedPoint8 => {
            let so = require_quant(t)?;
            data.iter().map(|&x| quant::quantize_ufixed(x, so.scale, so.offset, 8) as u8).collect()
        }
        UFixedPoint16 => {
            let so = require_quant(t)?;
            data.iter().flat_map(|&x| (quant::quantize_ufixed(x, so.scale, so.offset, 16) as u16).to_le_bytes()).collect()
        }
        UFixedPoint32 => {
            let so = require_quant(t)?;
            data.iter().flat_map(|&x| quant::quantize_ufixed(x, so.scale, so.offset, 32).to_le_bytes()).collect()
        }
        SFixedPoint8 => {
            let so = require_quant(t)?;
            data.iter().map(|&x| quant::quantize_sfixed(x, so.scale, so.offset, 8) as i8 as u8).collect()
        }
        SFixedPoint16 => {
            let so = require_quant(t)?;
            data.iter().flat_map(|&x| (quant::quantize_sfixed(x, so.scale, so.offset, 16) as i16).to_le_bytes()).collect()
        }
        SFixedPoint32 => {
            let so = require_quant(t)?;
            data.iter().flat_map(|&x| quant::quantize_sfixed(x, so.scale, so.offset, 32).to_le_bytes()).collect()
        }
        other => return Err(Error::UnsupportedDataType { kind: "input", name: t.name.clone(), dtype: other }),
    };
    Ok(bytes)
}

fn dequantize_output(t: &TensorInfo, bytes: &[u8]) -> Result<Vec<f32>> {
    use DataType::*;
    let out: Vec<f32> = match t.dtype {
        Float32 => bytes.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect(),
        Int8 => bytes.iter().map(|&b| b as i8 as f32).collect(),
        Int16 => bytes.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32).collect(),
        Int32 => bytes.chunks_exact(4).map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f32).collect(),
        UInt8 => bytes.iter().map(|&b| b as f32).collect(),
        UInt16 => bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]]) as f32).collect(),
        UInt32 => bytes.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f32).collect(),
        UFixedPoint8 => {
            let so = require_quant(t)?;
            bytes.iter().map(|&b| quant::dequantize_ufixed(b as u32, so.scale, so.offset)).collect()
        }
        UFixedPoint16 => {
            let so = require_quant(t)?;
            bytes.chunks_exact(2).map(|c| quant::dequantize_ufixed(u16::from_le_bytes([c[0], c[1]]) as u32, so.scale, so.offset)).collect()
        }
        UFixedPoint32 => {
            let so = require_quant(t)?;
            bytes.chunks_exact(4).map(|c| quant::dequantize_ufixed(u32::from_le_bytes([c[0], c[1], c[2], c[3]]), so.scale, so.offset)).collect()
        }
        SFixedPoint8 => {
            let so = require_quant(t)?;
            bytes.iter().map(|&b| quant::dequantize_sfixed(b as i8 as i32, so.scale, so.offset)).collect()
        }
        SFixedPoint16 => {
            let so = require_quant(t)?;
            bytes.chunks_exact(2).map(|c| quant::dequantize_sfixed(i16::from_le_bytes([c[0], c[1]]) as i32, so.scale, so.offset)).collect()
        }
        SFixedPoint32 => {
            let so = require_quant(t)?;
            bytes.chunks_exact(4).map(|c| quant::dequantize_sfixed(i32::from_le_bytes([c[0], c[1], c[2], c[3]]), so.scale, so.offset)).collect()
        }
        other => return Err(Error::UnsupportedDataType { kind: "output", name: t.name.clone(), dtype: other }),
    };
    Ok(out)
}

/// D1 (device-only): lock the HTP to DCVS_V3 burst (max voltage corners, DCVS
/// disabled) via deviceGetInfrastructure -> createPowerConfigId -> setPowerConfig.
/// The backend/device (FastRPC session) must already exist; call after
/// `Context::from_binary`. The power-config id is intentionally kept alive so
/// the vote persists for the process. No-op path fails at init on host.
pub fn set_htp_performance_mode(backend: &Backend) -> Result<()> {
    let get_infra = backend.ftab.deviceGetInfrastructure.ok_or(Error::MissingFn("deviceGetInfrastructure"))?;

    let mut device_infra: ffi::QnnDevice_Infrastructure_t = ptr::null_mut();
    let rc = unsafe { get_infra(&mut device_infra) };
    if !ok(rc) {
        return Err(Error::Qnn { op: "deviceGetInfrastructure", code: rc });
    }
    if device_infra.is_null() {
        return Err(Error::Malformed("deviceGetInfrastructure returned null"));
    }

    let infra = unsafe { &*(device_infra as *const htp::QnnHtpDevice_Infrastructure_t) };
    if infra.infraType != htp::QNN_HTP_DEVICE_INFRASTRUCTURE_TYPE_PERF {
        return Err(Error::Malformed("HTP infrastructure is not perf type"));
    }
    let perf = infra.perfInfra;
    let create = perf.createPowerConfigId.ok_or(Error::MissingFn("createPowerConfigId"))?;
    let set = perf.setPowerConfig.ok_or(Error::MissingFn("setPowerConfig"))?;

    let mut power_config_id: u32 = 0;
    let rc = unsafe { create(0, 0, &mut power_config_id) };
    if !ok(rc) {
        return Err(Error::Qnn { op: "createPowerConfigId", code: rc });
    }

    let max = htp::DCVS_VOLTAGE_VCORNER_MAX_VOLTAGE_CORNER;
    let dcvs = htp::QnnHtpPerfInfrastructure_DcvsV3_t {
        contextId: power_config_id,
        setDcvsEnable: 1,
        dcvsEnable: 0,
        powerMode: htp::QNN_HTP_PERF_INFRASTRUCTURE_POWERMODE_PERFORMANCE_MODE,
        setSleepLatency: 1,
        sleepLatency: 40,
        setSleepDisable: 0,
        sleepDisable: 0,
        setBusParams: 1,
        busVoltageCornerMin: max,
        busVoltageCornerTarget: max,
        busVoltageCornerMax: max,
        setCoreParams: 1,
        coreVoltageCornerMin: max,
        coreVoltageCornerTarget: max,
        coreVoltageCornerMax: max,
    };
    let power_config = htp::QnnHtpPerfInfrastructure_PowerConfig_t {
        option: htp::QNN_HTP_PERF_INFRASTRUCTURE_POWER_CONFIGOPTION_DCVS_V3,
        config: htp::QnnHtpPerfInfrastructure_PowerConfig_union { dcvsV3Config: dcvs },
    };
    let configs: [*const htp::QnnHtpPerfInfrastructure_PowerConfig_t; 2] = [&power_config, ptr::null()];
    let rc = unsafe { set(power_config_id, configs.as_ptr()) };
    if !ok(rc) {
        return Err(Error::Qnn { op: "setPowerConfig", code: rc });
    }
    Ok(())
}

/// Point the HTP/FastRPC loaders at `skel_dir` (the dir holding
/// `libQnnHtpV81Skel.so`) before the HTP backend is dlopened. Prepends the dir
/// to ADSP_LIBRARY_PATH/DSP_LIBRARY_PATH (`;`-separated, DSP search path) and
/// LD_LIBRARY_PATH (`:`-separated, host stub libs) via `setenv` so the native
/// loaders observe it. Call once at startup, before threads and dlopen.
pub fn prepare_htp_env(skel_dir: &Path) {
    let dir = skel_dir.to_string_lossy();
    prepend_env("ADSP_LIBRARY_PATH", &dir, ';');
    prepend_env("DSP_LIBRARY_PATH", &dir, ';');
    prepend_env("LD_LIBRARY_PATH", &dir, ':');
}

fn prepend_env(key: &str, value: &str, sep: char) {
    let new = match std::env::var_os(key) {
        Some(cur) if !cur.is_empty() => format!("{value}{sep}{}", cur.to_string_lossy()),
        _ => value.to_string(),
    };
    let (Ok(k), Ok(v)) = (CString::new(key), CString::new(new)) else { return };
    unsafe { libc::setenv(k.as_ptr(), v.as_ptr(), 1) };
}
