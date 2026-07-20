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

/// Widen IEEE 754 half-precision bits to f32.
pub fn f16_to_f32(bits: u16) -> f32 {
    let sign = (bits >> 15) as u32;
    let exp = ((bits >> 10) & 0x1f) as u32;
    let mant = (bits & 0x3ff) as u32;
    let out = match exp {
        0 if mant == 0 => sign << 31,
        0 => {
            let mut e = 0i32;
            let mut m = mant;
            while m & 0x400 == 0 {
                m <<= 1;
                e += 1;
            }
            (sign << 31) | (((127 - 15 - e + 1) as u32) << 23) | ((m & 0x3ff) << 13)
        }
        0x1f => (sign << 31) | 0x7f80_0000 | (mant << 13),
        _ => (sign << 31) | ((exp + 112) << 23) | (mant << 13),
    };
    f32::from_bits(out)
}

/// Narrow an f32 to IEEE 754 half-precision bits, rounding to nearest-even.
pub fn f32_to_f16(x: f32) -> u16 {
    let bits = x.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xff) as i32;
    let mant = bits & 0x7f_ffff;
    if exp == 0xff {
        return sign | 0x7c00 | u16::from(mant != 0) << 9;
    }
    let unbiased = exp - 127;
    if unbiased > 15 {
        return sign | 0x7c00;
    }
    if unbiased >= -14 {
        let mut m = mant >> 13;
        let rem = mant & 0x1fff;
        if rem > 0x1000 || (rem == 0x1000 && m & 1 == 1) {
            m += 1;
        }
        let mut e = (unbiased + 15) as u32;
        if m == 0x400 {
            m = 0;
            e += 1;
        }
        if e >= 0x1f {
            return sign | 0x7c00;
        }
        return sign | ((e as u16) << 10) | m as u16;
    }
    if unbiased >= -24 {
        let m_full = mant | 0x80_0000;
        let shift = (-14 - unbiased + 13) as u32;
        let m = m_full >> shift;
        let rem = m_full & ((1 << shift) - 1);
        let half = 1u32 << (shift - 1);
        let mut m16 = m as u16;
        if rem > half || (rem == half && m16 & 1 == 1) {
            m16 += 1;
        }
        return sign | m16;
    }
    sign
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
    fn f16_known_bit_patterns() {
        assert_eq!(f32_to_f16(0.0), 0x0000);
        assert_eq!(f32_to_f16(-0.0), 0x8000);
        assert_eq!(f32_to_f16(1.0), 0x3c00);
        assert_eq!(f32_to_f16(-2.0), 0xc000);
        assert_eq!(f32_to_f16(0.5), 0x3800);
        assert_eq!(f32_to_f16(65504.0), 0x7bff);
        assert_eq!(f32_to_f16(1e6), 0x7c00);
        assert_eq!(f32_to_f16(f32::INFINITY), 0x7c00);
        assert!(f32_to_f16(f32::NAN) & 0x7c00 == 0x7c00 && f32_to_f16(f32::NAN) & 0x3ff != 0);
        for (bits, want) in [(0x3c00u16, 1.0f32), (0xc000, -2.0), (0x3800, 0.5), (0x7bff, 65504.0)] {
            assert_eq!(f16_to_f32(bits), want);
        }
    }

    #[test]
    fn f16_round_trip_within_half_ulp() {
        for &v in &[-4.0f32, -1.5, -0.001, 0.0, 0.002, 0.333, 1.0, 3.7, 255.0, 448.0] {
            let back = f16_to_f32(f32_to_f16(v));
            let tol = (v.abs() * 0.001).max(1e-6);
            assert!((back - v).abs() <= tol, "v={v} back={back}");
        }
    }

    #[test]
    fn f16_nearest_even_and_subnormals() {
        // 1.0 + 2^-11 sits exactly between 0x3c00 and 0x3c01: ties-to-even keeps 0x3c00.
        assert_eq!(f32_to_f16(1.0 + 2f32.powi(-11)), 0x3c00);
        assert_eq!(f32_to_f16(1.0 + 3.0 * 2f32.powi(-11)), 0x3c02);
        // Smallest subnormal and a mid-range one survive the trip.
        assert_eq!(f32_to_f16(5.9604645e-8), 0x0001);
        assert_eq!(f16_to_f32(0x0001), 5.9604645e-8);
        assert_eq!(f32_to_f16(f16_to_f32(0x0200)), 0x0200);
        // Below half the smallest subnormal flushes to zero.
        assert_eq!(f32_to_f16(1e-9), 0x0000);
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
