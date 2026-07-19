//! Pure scale-offset quant/dequant math, matching QNN's DataUtil `floatToTfN` /
//! `tfNToFloat`. No FFI, no device — unit-tested on host.
//!
//! QNN affine convention (see `ScaleOffset`): `real = (quantized + offset) * scale`,
//! so `quantized = round(real / scale - offset)` clamped to the type's range.

/// Largest representable value of an unsigned fixed-point type of `bits` width.
fn ufixed_max(bits: u32) -> f64 {
    ((1u64 << bits) - 1) as f64
}

/// Quantize one f32 to an unsigned fixed-point integer via QNN scale-offset.
pub fn quantize_ufixed(value: f32, scale: f32, offset: i32, bits: u32) -> u32 {
    let max = ufixed_max(bits);
    let q = (value as f64 / scale as f64 - offset as f64).round();
    q.clamp(0.0, max) as u32
}

/// Dequantize an unsigned fixed-point integer to f32 via QNN scale-offset.
pub fn dequantize_ufixed(q: u32, scale: f32, offset: i32) -> f32 {
    ((q as f64 + offset as f64) * scale as f64) as f32
}

/// Quantize one f32 to a signed fixed-point integer via QNN scale-offset.
pub fn quantize_sfixed(value: f32, scale: f32, offset: i32, bits: u32) -> i32 {
    let lo = -(1i64 << (bits - 1)) as f64;
    let hi = ((1i64 << (bits - 1)) - 1) as f64;
    let q = (value as f64 / scale as f64 - offset as f64).round();
    q.clamp(lo, hi) as i32
}

/// Dequantize a signed fixed-point integer to f32 via QNN scale-offset.
pub fn dequantize_sfixed(q: i32, scale: f32, offset: i32) -> f32 {
    ((q as f64 + offset as f64) * scale as f64) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real AnythingV5 unet.bin params (README "Verified output").
    const SAMPLE_SCALE: f32 = 0.00014645085;
    const SAMPLE_OFFSET: i32 = -33954;

    #[test]
    fn ufixed16_round_trip_within_one_step() {
        for &v in &[-4.0f32, -1.5, -0.001, 0.0, 0.002, 1.0, 3.7] {
            let q = quantize_ufixed(v, SAMPLE_SCALE, SAMPLE_OFFSET, 16);
            let back = dequantize_ufixed(q, SAMPLE_SCALE, SAMPLE_OFFSET);
            assert!((back - v).abs() <= SAMPLE_SCALE, "v={v} back={back} q={q}");
        }
    }

    #[test]
    fn ufixed16_stays_in_range_and_clamps() {
        for &v in &[-100.0f32, 100.0, f32::MIN, f32::MAX] {
            let q = quantize_ufixed(v, SAMPLE_SCALE, SAMPLE_OFFSET, 16);
            assert!(q <= 65535, "q={q} out of u16 range");
        }
        assert_eq!(quantize_ufixed(-1e30, SAMPLE_SCALE, SAMPLE_OFFSET, 16), 0);
        assert_eq!(quantize_ufixed(1e30, SAMPLE_SCALE, SAMPLE_OFFSET, 16), 65535);
    }

    #[test]
    fn dequantize_zero_is_offset_times_scale() {
        let got = dequantize_ufixed(0, SAMPLE_SCALE, SAMPLE_OFFSET);
        assert!((got - (SAMPLE_OFFSET as f32 * SAMPLE_SCALE)).abs() < 1e-6);
    }

    #[test]
    fn quantize_is_left_inverse_of_dequantize() {
        for q in [0u32, 1, 100, 33954, 60000, 65535] {
            let v = dequantize_ufixed(q, SAMPLE_SCALE, SAMPLE_OFFSET);
            assert_eq!(quantize_ufixed(v, SAMPLE_SCALE, SAMPLE_OFFSET, 16), q);
        }
    }

    #[test]
    fn ufixed8_round_trip() {
        let (scale, offset) = (0.02f32, -128);
        for &v in &[-2.5f32, 0.0, 2.54] {
            let q = quantize_ufixed(v, scale, offset, 8);
            assert!(q <= 255);
            let back = dequantize_ufixed(q, scale, offset);
            assert!((back - v).abs() <= scale, "v={v} back={back}");
        }
    }

    #[test]
    fn sfixed16_round_trip_and_clamp() {
        let (scale, offset) = (0.001f32, 0);
        for &v in &[-30.0f32, -0.5, 0.0, 0.5, 30.0] {
            let q = quantize_sfixed(v, scale, offset, 16);
            assert!((-32768..=32767).contains(&q));
            let back = dequantize_sfixed(q, scale, offset);
            assert!((back - v).abs() <= scale, "v={v} back={back}");
        }
        assert_eq!(quantize_sfixed(1e30, scale, offset, 16), 32767);
        assert_eq!(quantize_sfixed(-1e30, scale, offset, 16), -32768);
    }
}
