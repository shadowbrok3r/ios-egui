//! Parse a QNN context binary's metadata via libQnnSystem, copying everything
//! into owned Rust types before the QNN-owned memory is freed.
#![allow(unsafe_op_in_unsafe_fn)]

use crate::bindings as ffi;
use crate::error::{Error, Result};
use crate::loader::QnnSystem;
use crate::types::{cstr_opt, ContextBinaryInfo, DataType, GraphInfo, ScaleOffset, TensorInfo};
use std::ffi::c_void;
use std::ptr;

impl ContextBinaryInfo {
    /// Parse the metadata of a serialized context binary. `bytes` is the full
    /// unet.bin / vae_*.bin content. No backend is required.
    pub fn parse(system: &QnnSystem, bytes: &[u8]) -> Result<ContextBinaryInfo> {
        let create = system.ftab.systemContextCreate.ok_or(Error::MissingFn("systemContextCreate"))?;
        let get_info = system
            .ftab
            .systemContextGetBinaryInfo
            .ok_or(Error::MissingFn("systemContextGetBinaryInfo"))?;
        let free = system.ftab.systemContextFree.ok_or(Error::MissingFn("systemContextFree"))?;

        let mut handle: ffi::QnnSystemContext_Handle_t = ptr::null_mut();
        let rc = unsafe { create(&mut handle) };
        if rc != ffi::QNN_SUCCESS as u64 {
            return Err(Error::Qnn { op: "systemContextCreate", code: rc });
        }

        let mut info: *const ffi::QnnSystemContext_BinaryInfo_t = ptr::null();
        let mut info_size: ffi::Qnn_ContextBinarySize_t = 0;
        let rc = unsafe {
            get_info(
                handle,
                bytes.as_ptr() as *mut c_void,
                bytes.len() as u64,
                &mut info,
                &mut info_size,
            )
        };
        if rc != ffi::QNN_SUCCESS as u64 {
            unsafe { free(handle) };
            return Err(Error::Qnn { op: "systemContextGetBinaryInfo", code: rc });
        }
        if info.is_null() {
            unsafe { free(handle) };
            return Err(Error::Malformed("systemContextGetBinaryInfo returned null binaryInfo"));
        }

        let parsed = unsafe { convert_binary_info(info) };
        unsafe { free(handle) };
        parsed
    }
}

struct RawBinaryInfo {
    num_graphs: u32,
    graphs: *mut ffi::QnnSystemContext_GraphInfo_t,
    backend_id: u32,
    build_id: Option<String>,
    core_api_version: (u32, u32, u32),
    soc_model: Option<u32>,
}

unsafe fn convert_binary_info(info: *const ffi::QnnSystemContext_BinaryInfo_t) -> Result<ContextBinaryInfo> {
    let ver = (*info).version.0;
    let u = &(*info).__bindgen_anon_1;
    let raw = if ver == ffi::QnnSystemContext_BinaryInfoVersion_t::QNN_SYSTEM_CONTEXT_BINARY_INFO_VERSION_1.0 {
        let b = &u.contextBinaryInfoV1;
        RawBinaryInfo {
            num_graphs: b.numGraphs,
            graphs: b.graphs,
            backend_id: b.backendId,
            build_id: cstr_opt(b.buildId),
            core_api_version: (b.coreApiVersion.major, b.coreApiVersion.minor, b.coreApiVersion.patch),
            soc_model: None,
        }
    } else if ver == ffi::QnnSystemContext_BinaryInfoVersion_t::QNN_SYSTEM_CONTEXT_BINARY_INFO_VERSION_2.0 {
        let b = &u.contextBinaryInfoV2;
        RawBinaryInfo {
            num_graphs: b.numGraphs,
            graphs: b.graphs,
            backend_id: b.backendId,
            build_id: cstr_opt(b.buildId),
            core_api_version: (b.coreApiVersion.major, b.coreApiVersion.minor, b.coreApiVersion.patch),
            soc_model: None,
        }
    } else if ver == ffi::QnnSystemContext_BinaryInfoVersion_t::QNN_SYSTEM_CONTEXT_BINARY_INFO_VERSION_3.0 {
        let b = &u.contextBinaryInfoV3;
        RawBinaryInfo {
            num_graphs: b.numGraphs,
            graphs: b.graphs,
            backend_id: b.backendId,
            build_id: cstr_opt(b.buildId),
            core_api_version: (b.coreApiVersion.major, b.coreApiVersion.minor, b.coreApiVersion.patch),
            soc_model: Some(b.socModel),
        }
    } else {
        return Err(Error::Malformed("unknown QnnSystemContext_BinaryInfo version"));
    };

    if raw.num_graphs > 0 && raw.graphs.is_null() {
        return Err(Error::Malformed("numGraphs > 0 but graphs pointer is null"));
    }

    let mut graphs = Vec::with_capacity(raw.num_graphs as usize);
    for i in 0..raw.num_graphs as isize {
        graphs.push(convert_graph_info(raw.graphs.offset(i))?);
    }

    Ok(ContextBinaryInfo {
        graphs,
        backend_id: raw.backend_id,
        build_id: raw.build_id,
        core_api_version: raw.core_api_version,
        soc_model: raw.soc_model,
    })
}

unsafe fn convert_graph_info(g: *const ffi::QnnSystemContext_GraphInfo_t) -> Result<GraphInfo> {
    let ver = (*g).version.0;
    let u = &(*g).__bindgen_anon_1;
    let (name, n_in, ins, n_out, outs) =
        if ver == ffi::QnnSystemContext_GraphInfoVersion_t::QNN_SYSTEM_CONTEXT_GRAPH_INFO_VERSION_1.0 {
            let v = &u.graphInfoV1;
            (v.graphName, v.numGraphInputs, v.graphInputs, v.numGraphOutputs, v.graphOutputs)
        } else if ver == ffi::QnnSystemContext_GraphInfoVersion_t::QNN_SYSTEM_CONTEXT_GRAPH_INFO_VERSION_2.0 {
            let v = &u.graphInfoV2;
            (v.graphName, v.numGraphInputs, v.graphInputs, v.numGraphOutputs, v.graphOutputs)
        } else if ver == ffi::QnnSystemContext_GraphInfoVersion_t::QNN_SYSTEM_CONTEXT_GRAPH_INFO_VERSION_3.0 {
            let v = &u.graphInfoV3;
            (v.graphName, v.numGraphInputs, v.graphInputs, v.numGraphOutputs, v.graphOutputs)
        } else {
            return Err(Error::Malformed("unknown QnnSystemContext_GraphInfo version"));
        };

    Ok(GraphInfo {
        name: cstr_opt(name).unwrap_or_default(),
        inputs: convert_tensor_array(ins, n_in),
        outputs: convert_tensor_array(outs, n_out),
    })
}

unsafe fn convert_tensor_array(arr: *const ffi::Qnn_Tensor_t, count: u32) -> Vec<TensorInfo> {
    let mut out = Vec::with_capacity(count as usize);
    if arr.is_null() {
        return out;
    }
    for i in 0..count as isize {
        out.push(tensor_to_info(arr.offset(i)));
    }
    out
}

unsafe fn tensor_to_info(t: *const ffi::Qnn_Tensor_t) -> TensorInfo {
    let ver = (*t).version.0;
    let u = &(*t).__bindgen_anon_1;
    // v1 and v2 share an identical leading layout for these fields.
    let (id, name, dtype, rank, dims_ptr, qp) =
        if ver == ffi::Qnn_TensorVersion_t::QNN_TENSOR_VERSION_2.0 {
            let v = &u.v2;
            (v.id, v.name, v.dataType, v.rank, v.dimensions, &v.quantizeParams as *const _)
        } else {
            let v = &u.v1;
            (v.id, v.name, v.dataType, v.rank, v.dimensions, &v.quantizeParams as *const _)
        };

    let mut dims = Vec::with_capacity(rank as usize);
    if !dims_ptr.is_null() {
        for i in 0..rank as isize {
            dims.push(*dims_ptr.offset(i));
        }
    }

    TensorInfo {
        name: cstr_opt(name).unwrap_or_default(),
        id,
        dims,
        dtype: DataType::from_raw(dtype),
        quant: quant_from(qp),
    }
}

unsafe fn quant_from(qp: *const ffi::Qnn_QuantizeParams_t) -> Option<ScaleOffset> {
    if qp.is_null() {
        return None;
    }
    let defined = (*qp).encodingDefinition.0 == ffi::Qnn_Definition_t::QNN_DEFINITION_DEFINED.0;
    let scale_offset = (*qp).quantizationEncoding.0
        == ffi::Qnn_QuantizationEncoding_t::QNN_QUANTIZATION_ENCODING_SCALE_OFFSET.0;
    if defined && scale_offset {
        let so = (*qp).__bindgen_anon_1.scaleOffsetEncoding;
        Some(ScaleOffset { scale: so.scale, offset: so.offset })
    } else {
        None
    }
}
