//! Scale-offset quant/dequant of whole f32 buffers against a [`TensorInfo`].
//!
//! Reuses the public `qnn_rs::quant` primitives (`real = (q + offset) * scale`).
//! `qnn_rs::Context::execute` quantizes/dequantizes internally, so the pipeline
//! passes f32; these helpers cover standalone/offline conversion and tests.

use crate::error::{Error, Result};
use qnn_rs::quant as q;
use qnn_rs::{DataType, ScaleOffset, TensorInfo};

fn scale_offset(t: &TensorInfo) -> Result<ScaleOffset> {
    t.quant.ok_or_else(|| Error::MissingQuant(t.name.clone()))
}

/// Quantize `data` to the tensor's dtype, little-endian, one element per dim product.
pub fn quantize(t: &TensorInfo, data: &[f32]) -> Result<Vec<u8>> {
    let expected = t.elem_count() as usize;
    if data.len() != expected {
        return Err(Error::ShapeMismatch { name: t.name.clone(), expected, got: data.len() });
    }
    use DataType::*;
    let bytes = match t.dtype {
        Float32 => data.iter().flat_map(|&x| x.to_le_bytes()).collect(),
        Int32 => data.iter().flat_map(|&x| (x.round() as i32).to_le_bytes()).collect(),
        UFixedPoint8 => {
            let so = scale_offset(t)?;
            data.iter().map(|&x| q::quantize_ufixed(x, so.scale, so.offset, 8) as u8).collect()
        }
        UFixedPoint16 => {
            let so = scale_offset(t)?;
            data.iter().flat_map(|&x| (q::quantize_ufixed(x, so.scale, so.offset, 16) as u16).to_le_bytes()).collect()
        }
        SFixedPoint8 => {
            let so = scale_offset(t)?;
            data.iter().map(|&x| q::quantize_sfixed(x, so.scale, so.offset, 8) as i8 as u8).collect()
        }
        SFixedPoint16 => {
            let so = scale_offset(t)?;
            data.iter().flat_map(|&x| (q::quantize_sfixed(x, so.scale, so.offset, 16) as i16).to_le_bytes()).collect()
        }
        other => return Err(Error::UnsupportedDataType { name: t.name.clone(), dtype: other }),
    };
    Ok(bytes)
}

/// Dequantize little-endian `bytes` of the tensor's dtype back to f32.
pub fn dequantize(t: &TensorInfo, bytes: &[u8]) -> Result<Vec<f32>> {
    use DataType::*;
    let out: Vec<f32> = match t.dtype {
        Float32 => bytes.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect(),
        Int32 => bytes.chunks_exact(4).map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f32).collect(),
        UFixedPoint8 => {
            let so = scale_offset(t)?;
            bytes.iter().map(|&b| q::dequantize_ufixed(b as u32, so.scale, so.offset)).collect()
        }
        UFixedPoint16 => {
            let so = scale_offset(t)?;
            bytes.chunks_exact(2).map(|c| q::dequantize_ufixed(u16::from_le_bytes([c[0], c[1]]) as u32, so.scale, so.offset)).collect()
        }
        SFixedPoint8 => {
            let so = scale_offset(t)?;
            bytes.iter().map(|&b| q::dequantize_sfixed(b as i8 as i32, so.scale, so.offset)).collect()
        }
        SFixedPoint16 => {
            let so = scale_offset(t)?;
            bytes.chunks_exact(2).map(|c| q::dequantize_sfixed(i16::from_le_bytes([c[0], c[1]]) as i32, so.scale, so.offset)).collect()
        }
        other => return Err(Error::UnsupportedDataType { name: t.name.clone(), dtype: other }),
    };
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // AnythingV5 unet.bin `sample` params (qnn-rs README "Verified output").
    const SCALE: f32 = 0.00014645085;
    const OFFSET: i32 = -33954;

    fn tensor(dtype: DataType, dims: Vec<u32>, quant: Option<ScaleOffset>) -> TensorInfo {
        TensorInfo { name: "t".into(), id: 0, dims, dtype, quant }
    }

    #[test]
    fn ufixed16_round_trips_within_one_step() {
        let t = tensor(DataType::UFixedPoint16, vec![4], Some(ScaleOffset { scale: SCALE, offset: OFFSET }));
        let data = vec![-4.0f32, -0.5, 0.0, 3.7];
        let bytes = quantize(&t, &data).unwrap();
        assert_eq!(bytes.len(), 8);
        let back = dequantize(&t, &bytes).unwrap();
        for (a, b) in data.iter().zip(&back) {
            assert!((a - b).abs() <= SCALE, "a={a} b={b}");
        }
    }

    #[test]
    fn ufixed8_round_trips_within_one_step() {
        let t = tensor(DataType::UFixedPoint8, vec![3], Some(ScaleOffset { scale: 0.02, offset: -128 }));
        let data = vec![-2.5f32, 0.0, 2.54];
        let bytes = quantize(&t, &data).unwrap();
        assert_eq!(bytes.len(), 3);
        let back = dequantize(&t, &bytes).unwrap();
        for (a, b) in data.iter().zip(&back) {
            assert!((a - b).abs() <= 0.02, "a={a} b={b}");
        }
    }

    #[test]
    fn int32_and_float32_pass_through() {
        let ti = tensor(DataType::Int32, vec![1], None);
        assert_eq!(dequantize(&ti, &quantize(&ti, &[999.0]).unwrap()).unwrap(), vec![999.0]);
        let tf = tensor(DataType::Float32, vec![2], None);
        assert_eq!(dequantize(&tf, &quantize(&tf, &[1.5, -2.0]).unwrap()).unwrap(), vec![1.5, -2.0]);
    }

    #[test]
    fn shape_mismatch_is_reported() {
        let t = tensor(DataType::Float32, vec![4], None);
        assert!(matches!(quantize(&t, &[0.0, 1.0]), Err(Error::ShapeMismatch { .. })));
    }

    #[test]
    fn missing_quant_params_error() {
        let t = tensor(DataType::UFixedPoint16, vec![1], None);
        assert!(matches!(quantize(&t, &[0.0]), Err(Error::MissingQuant(_))));
    }
}
