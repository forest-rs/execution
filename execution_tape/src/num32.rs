// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! 32-bit numeric helpers: width conversions and the `f32` operations `core` lacks.
//!
//! Every function is deterministic on every platform and needs neither `std` nor `libm`.
//!
//! `f32` rounding and square root reuse the exact `f64` routines in [`crate::float`]. Widening an
//! `f32` to `f64` is exact, and:
//! - `floor`, `ceil`, `trunc` and `nearest` of an exact `f32` value are integers that are again
//!   exactly representable as `f32`, so narrowing the `f64` result is exact;
//! - `f64` has more than `2 * 24 + 2` significand bits, so rounding the correctly rounded `f64`
//!   square root to `f32` gives the correctly rounded `f32` square root (no double-rounding
//!   error);
//! - `f64` remainder is exact, so its result is the exact `f32` remainder.
//!
//! NaN conversions are spelled out rather than left to `as`: a NaN keeps its sign and the top of
//! its payload, and is quieted, matching [`crate::float`].

use crate::float;

const F32_QUIET: u32 = 1 << 22;
const F64_SIGN: u64 = 1 << 63;
const F64_QUIET: u64 = 1 << 51;
/// Significand bits `f64` has beyond `f32`.
const WIDEN_SHIFT: u32 = 52 - 23;

/// Widens exactly. A NaN keeps its sign and payload, quieted.
#[inline]
pub(crate) fn promote(x: f32) -> f64 {
    if x.is_nan() {
        let bits = x.to_bits();
        let sign = u64::from(bits >> 31) << 63;
        let payload = u64::from(bits & ((1 << 23) - 1)) << WIDEN_SHIFT;
        return f64::from_bits(sign | 0x7FF0_0000_0000_0000 | F64_QUIET | payload);
    }
    f64::from(x)
}

/// Narrows to the nearest `f32`, ties to even, overflowing to infinity. A NaN keeps its sign
/// and the top 23 bits of its payload, quieted.
#[inline]
pub(crate) fn demote(x: f64) -> f32 {
    if x.is_nan() {
        let bits = x.to_bits();
        let sign = u32::from(bits & F64_SIGN != 0) << 31;
        let payload =
            u32::try_from((bits >> WIDEN_SHIFT) & ((1 << 23) - 1)).expect("masked to 23 bits");
        return f32::from_bits(sign | 0x7F80_0000 | F32_QUIET | payload);
    }
    #[allow(
        clippy::cast_possible_truncation,
        reason = "rounding a finite or infinite f64 to the nearest f32 is the operation"
    )]
    let out = x as f32;
    out
}

/// Rounds toward negative infinity.
#[inline]
pub(crate) fn floor(x: f32) -> f32 {
    demote(float::floor(promote(x)))
}

/// Rounds toward positive infinity.
#[inline]
pub(crate) fn ceil(x: f32) -> f32 {
    demote(float::ceil(promote(x)))
}

/// Rounds toward zero.
#[inline]
pub(crate) fn trunc(x: f32) -> f32 {
    demote(float::trunc(promote(x)))
}

/// Rounds to the nearest integer, ties to even.
#[inline]
pub(crate) fn nearest(x: f32) -> f32 {
    demote(float::nearest(promote(x)))
}

/// Correctly rounded square root.
#[inline]
pub(crate) fn sqrt(x: f32) -> f32 {
    demote(float::sqrt(promote(x)))
}

/// Remainder of truncating division, with the sign of `a` (like `f64.rem`).
#[inline]
pub(crate) fn rem(a: f32, b: f32) -> f32 {
    demote(promote(a) % promote(b))
}

/// Returns `x` without its sign bit.
#[inline]
pub(crate) fn abs(x: f32) -> f32 {
    f32::from_bits(x.to_bits() & !(1 << 31))
}

/// Returns `magnitude` with the sign bit of `sign`; NaN payloads are untouched.
#[inline]
pub(crate) fn copysign(magnitude: f32, sign: f32) -> f32 {
    let mask = 1_u32 << 31;
    f32::from_bits((magnitude.to_bits() & !mask) | (sign.to_bits() & mask))
}

/// Keeps the low 32 bits (two's complement).
#[inline]
pub(crate) fn wrap_i32(x: i64) -> i32 {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "wrapping to the low 32 bits is the operation"
    )]
    let out = x as i32;
    out
}

/// Keeps the low 32 bits.
#[inline]
pub(crate) fn wrap_u32(x: u64) -> u32 {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "wrapping to the low 32 bits is the operation"
    )]
    let out = x as u32;
    out
}

/// Truncates toward zero and saturates to `i32`; NaN converts to `0`.
#[inline]
pub(crate) fn to_i32_sat(x: f64) -> i32 {
    // Rust defines float-to-integer `as` casts as exactly this saturating truncation.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "saturating truncation is the defined semantics of this operation"
    )]
    let out = x as i32;
    out
}

/// Truncates toward zero and saturates to `u32`; NaN converts to `0`.
#[inline]
pub(crate) fn to_u32_sat(x: f64) -> u32 {
    // Rust defines float-to-integer `as` casts as exactly this saturating truncation.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "saturating truncation is the defined semantics of this operation"
    )]
    let out = x as u32;
    out
}

/// Why a trapping float-to-integer conversion failed.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum FloatToIntError {
    /// NaN or an infinity.
    Invalid,
    /// Finite, but its truncation is outside the target range.
    Overflow,
}

/// Truncates toward zero, failing on NaN, infinities and out-of-range values.
#[inline]
pub(crate) fn to_i32(x: f64) -> Result<i32, FloatToIntError> {
    if !x.is_finite() {
        return Err(FloatToIntError::Invalid);
    }
    // Both bounds are exact in f64; anything strictly between them truncates into range.
    if x <= -2_147_483_649.0 || x >= 2_147_483_648.0 {
        return Err(FloatToIntError::Overflow);
    }
    Ok(to_i32_sat(x))
}

/// Truncates toward zero, failing on NaN, infinities and out-of-range values.
#[inline]
pub(crate) fn to_u32(x: f64) -> Result<u32, FloatToIntError> {
    if !x.is_finite() {
        return Err(FloatToIntError::Invalid);
    }
    if x <= -1.0 || x >= 4_294_967_296.0 {
        return Err(FloatToIntError::Overflow);
    }
    Ok(to_u32_sat(x))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn promote_and_demote_round_trip_every_nan_payload_class() {
        for bits in [
            0x7FC0_0000_u32,
            0xFFC0_0000,
            0x7F80_0001,
            0x7FBF_FFFF,
            0xFFFF_FFFF,
            0x7FC0_1234,
        ] {
            let x = f32::from_bits(bits);
            let wide = promote(x);
            assert!(wide.is_nan(), "{bits:#x}");
            assert_eq!(demote(wide).to_bits(), bits | F32_QUIET, "{bits:#x}");
        }
        for x in [
            0.0_f32,
            -0.0,
            1.5,
            f32::MIN_POSITIVE,
            f32::MAX,
            f32::INFINITY,
        ] {
            assert_eq!(demote(promote(x)).to_bits(), x.to_bits());
        }
    }

    #[test]
    fn demote_rounds_to_nearest_even_and_overflows_to_infinity() {
        // Halfway between 1.0 and the next f32 rounds to the even neighbour, 1.0.
        assert_eq!(demote(1.0 + f64::from(f32::EPSILON) / 2.0), 1.0);
        assert_eq!(demote(f64::MAX), f32::INFINITY);
        assert_eq!(demote(-f64::MAX), f32::NEG_INFINITY);
        assert_eq!(demote(1e-50).to_bits(), 0);
        assert_eq!(demote(-1e-50).to_bits(), (-0.0_f32).to_bits());
    }

    #[test]
    fn rounding_and_sqrt_match_reference_values() {
        assert_eq!(floor(-0.5).to_bits(), (-1.0_f32).to_bits());
        assert_eq!(ceil(-0.5).to_bits(), (-0.0_f32).to_bits());
        assert_eq!(trunc(2.75), 2.0);
        assert_eq!(nearest(2.5), 2.0);
        assert_eq!(nearest(3.5), 4.0);
        assert_eq!(floor(8_388_607.5), 8_388_607.0);
        assert_eq!(sqrt(2.0), core::f32::consts::SQRT_2);
        assert_eq!(sqrt(-0.0).to_bits(), (-0.0_f32).to_bits());
        assert!(sqrt(-1.0).is_nan());
        assert_eq!(rem(7.5, 2.0), 1.5);
        assert_eq!(rem(-7.5, 2.0), -1.5);
    }

    /// Square root via `f64` agrees with a direct exact check on many inputs.
    #[test]
    fn sqrt_is_correctly_rounded() {
        let mut bits = 0x3F80_0000_u32;
        for _ in 0..20_000 {
            let x = f32::from_bits(bits);
            let r = sqrt(x);
            // r is correct iff x lies within the squares of the neighbouring midpoints.
            let lo = (f64::from(r) + f64::from(f32::from_bits(r.to_bits() - 1))) / 2.0;
            let hi = (f64::from(r) + f64::from(f32::from_bits(r.to_bits() + 1))) / 2.0;
            assert!(lo * lo <= f64::from(x) && f64::from(x) <= hi * hi, "{x}");
            bits = bits.wrapping_add(0x0001_3579);
            if bits >= 0x7F00_0000 {
                bits = 0x0080_0000 + (bits & 0xFFFF);
            }
        }
    }

    #[test]
    fn trapping_conversions_accept_exactly_the_truncatable_range() {
        assert_eq!(to_i32(-2_147_483_648.9), Ok(i32::MIN));
        assert_eq!(to_i32(2_147_483_647.9), Ok(i32::MAX));
        assert_eq!(to_i32(-2_147_483_649.0), Err(FloatToIntError::Overflow));
        assert_eq!(to_i32(2_147_483_648.0), Err(FloatToIntError::Overflow));
        assert_eq!(to_u32(-0.9), Ok(0));
        assert_eq!(to_u32(4_294_967_295.9), Ok(u32::MAX));
        assert_eq!(to_u32(-1.0), Err(FloatToIntError::Overflow));
        assert_eq!(to_u32(f64::NAN), Err(FloatToIntError::Invalid));
        assert_eq!(to_i32(f64::NEG_INFINITY), Err(FloatToIntError::Invalid));
        assert_eq!(to_i32_sat(f64::NAN), 0);
        assert_eq!(to_u32_sat(-5.0), 0);
        assert_eq!(to_u32_sat(1e20), u32::MAX);
        assert_eq!(wrap_i32(0x1_8000_0000), i32::MIN);
        assert_eq!(wrap_u32(u64::MAX), u32::MAX);
    }
}
