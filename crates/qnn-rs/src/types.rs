//! Owned, backend-agnostic view of a QNN context-binary's metadata.

use crate::bindings as ffi;
use std::ffi::{c_char, CStr};

/// QNN tensor element type. Covers the SD-relevant types explicitly; anything
/// else is preserved as its raw enum value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataType {
    Int8,
    Int16,
    Int32,
    Int64,
    UInt8,
    UInt16,
    UInt32,
    UInt64,
    Float16,
    Float32,
    SFixedPoint8,
    SFixedPoint16,
    SFixedPoint32,
    UFixedPoint8,
    UFixedPoint16,
    UFixedPoint32,
    Bool8,
    Other(u32),
}

impl DataType {
    pub fn from_raw(v: ffi::Qnn_DataType_t) -> Self {
        use ffi::Qnn_DataType_t as D;
        match v.0 {
            x if x == D::QNN_DATATYPE_INT_8.0 => DataType::Int8,
            x if x == D::QNN_DATATYPE_INT_16.0 => DataType::Int16,
            x if x == D::QNN_DATATYPE_INT_32.0 => DataType::Int32,
            x if x == D::QNN_DATATYPE_INT_64.0 => DataType::Int64,
            x if x == D::QNN_DATATYPE_UINT_8.0 => DataType::UInt8,
            x if x == D::QNN_DATATYPE_UINT_16.0 => DataType::UInt16,
            x if x == D::QNN_DATATYPE_UINT_32.0 => DataType::UInt32,
            x if x == D::QNN_DATATYPE_UINT_64.0 => DataType::UInt64,
            x if x == D::QNN_DATATYPE_FLOAT_16.0 => DataType::Float16,
            x if x == D::QNN_DATATYPE_FLOAT_32.0 => DataType::Float32,
            x if x == D::QNN_DATATYPE_SFIXED_POINT_8.0 => DataType::SFixedPoint8,
            x if x == D::QNN_DATATYPE_SFIXED_POINT_16.0 => DataType::SFixedPoint16,
            x if x == D::QNN_DATATYPE_SFIXED_POINT_32.0 => DataType::SFixedPoint32,
            x if x == D::QNN_DATATYPE_UFIXED_POINT_8.0 => DataType::UFixedPoint8,
            x if x == D::QNN_DATATYPE_UFIXED_POINT_16.0 => DataType::UFixedPoint16,
            x if x == D::QNN_DATATYPE_UFIXED_POINT_32.0 => DataType::UFixedPoint32,
            x if x == D::QNN_DATATYPE_BOOL_8.0 => DataType::Bool8,
            other => DataType::Other(other),
        }
    }

    /// Raw QNN enum value for this type, inverse of `from_raw`.
    pub fn to_raw(self) -> ffi::Qnn_DataType_t {
        use ffi::Qnn_DataType_t as D;
        match self {
            DataType::Int8 => D::QNN_DATATYPE_INT_8,
            DataType::Int16 => D::QNN_DATATYPE_INT_16,
            DataType::Int32 => D::QNN_DATATYPE_INT_32,
            DataType::Int64 => D::QNN_DATATYPE_INT_64,
            DataType::UInt8 => D::QNN_DATATYPE_UINT_8,
            DataType::UInt16 => D::QNN_DATATYPE_UINT_16,
            DataType::UInt32 => D::QNN_DATATYPE_UINT_32,
            DataType::UInt64 => D::QNN_DATATYPE_UINT_64,
            DataType::Float16 => D::QNN_DATATYPE_FLOAT_16,
            DataType::Float32 => D::QNN_DATATYPE_FLOAT_32,
            DataType::SFixedPoint8 => D::QNN_DATATYPE_SFIXED_POINT_8,
            DataType::SFixedPoint16 => D::QNN_DATATYPE_SFIXED_POINT_16,
            DataType::SFixedPoint32 => D::QNN_DATATYPE_SFIXED_POINT_32,
            DataType::UFixedPoint8 => D::QNN_DATATYPE_UFIXED_POINT_8,
            DataType::UFixedPoint16 => D::QNN_DATATYPE_UFIXED_POINT_16,
            DataType::UFixedPoint32 => D::QNN_DATATYPE_UFIXED_POINT_32,
            DataType::Bool8 => D::QNN_DATATYPE_BOOL_8,
            DataType::Other(v) => D(v),
        }
    }

    /// True for the plain integer types (not fixed-point, float, or opaque).
    pub fn is_integer(self) -> bool {
        matches!(
            self,
            DataType::Int8
                | DataType::Int16
                | DataType::Int32
                | DataType::Int64
                | DataType::UInt8
                | DataType::UInt16
                | DataType::UInt32
                | DataType::UInt64
                | DataType::Bool8
        )
    }

    /// Byte width of one element, or None for sub-byte/opaque types.
    pub fn byte_width(self) -> Option<u32> {
        Some(match self {
            DataType::Int8 | DataType::UInt8 | DataType::SFixedPoint8 | DataType::UFixedPoint8 | DataType::Bool8 => 1,
            DataType::Int16 | DataType::UInt16 | DataType::Float16 | DataType::SFixedPoint16 | DataType::UFixedPoint16 => 2,
            DataType::Int32 | DataType::UInt32 | DataType::Float32 | DataType::SFixedPoint32 | DataType::UFixedPoint32 => 4,
            DataType::Int64 | DataType::UInt64 => 8,
            DataType::Other(_) => return None,
        })
    }
}

/// Per-tensor affine quantization: float = (quantized + offset) * scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScaleOffset {
    pub scale: f32,
    pub offset: i32,
}

/// One graph input or output tensor.
#[derive(Clone, Debug)]
pub struct TensorInfo {
    pub name: String,
    pub id: u32,
    pub dims: Vec<u32>,
    pub dtype: DataType,
    pub quant: Option<ScaleOffset>,
}

impl TensorInfo {
    /// Total element count (product of dims), 1 for a scalar/rank-0 tensor.
    pub fn elem_count(&self) -> u64 {
        self.dims.iter().map(|&d| d as u64).product::<u64>().max(1)
    }
}

/// One graph registered in the context binary.
#[derive(Clone, Debug)]
pub struct GraphInfo {
    pub name: String,
    pub inputs: Vec<TensorInfo>,
    pub outputs: Vec<TensorInfo>,
}

/// Parsed metadata of a QNN context binary (unet.bin / vae_*.bin).
#[derive(Clone, Debug)]
pub struct ContextBinaryInfo {
    pub graphs: Vec<GraphInfo>,
    pub backend_id: u32,
    pub build_id: Option<String>,
    pub core_api_version: (u32, u32, u32),
    /// Present only for binary-info V3.
    pub soc_model: Option<u32>,
}

pub(crate) unsafe fn cstr_opt(p: *const c_char) -> Option<String> {
    if p.is_null() {
        None
    } else {
        Some(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
    }
}
