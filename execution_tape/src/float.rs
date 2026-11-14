// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Exact `f64` operations with WebAssembly semantics, implemented without `std` or `libm`.
//!
//! `f64::{floor, ceil, trunc, round, round_ties_even, sqrt}` live in `std` (they lower to libm calls or
//! hardware instructions there), so this `no_std` crate implements them on the bit representation.
//! Every function here is exact: the rounding functions return an integral value chosen by their
//! rounding direction, [`sqrt`] returns the correctly rounded square root
//! (round-to-nearest, ties-to-even), [`copysign`] only moves a sign bit, and the saturating
//! conversions truncate toward zero and clamp. Results are bit-for-bit deterministic on every
//! platform.
//!
//! Signed zeros, infinities and subnormals of the rounding functions and [`sqrt`] follow
//! IEEE 754 / WebAssembly:
//! - the sign of a zero result is the sign of the input (`floor(-0.0) == -0.0`,
//!   `ceil(-0.5) == -0.0`, `sqrt(-0.0) == -0.0`);
//! - infinities pass through (`sqrt(-inf)` is NaN);
//! - a NaN input yields that NaN with its quiet bit set, preserving sign and payload;
//! - `sqrt` of a negative non-zero number yields the canonical quiet NaN, [`f64::NAN`].

const SIGN_MASK: u64 = 1 << 63;
const EXP_MASK: u64 = 0x7FF << 52;
const FRAC_MASK: u64 = (1 << 52) - 1;
const QUIET_BIT: u64 = 1 << 51;
/// Number of explicit significand bits.
const FRAC_BITS: u64 = 52;
const EXP_BIAS: u64 = 1023;
/// Bits of `1.0`.
const ONE_BITS: u64 = 0x3FF0_0000_0000_0000;
/// Bits of `0.5`.
const HALF_BITS: u64 = 0x3FE0_0000_0000_0000;

/// Rounds toward negative infinity.
#[inline]
pub(crate) fn floor(x: f64) -> f64 {
    round_to_integral(x, Rounding::Floor)
}

/// Rounds toward positive infinity.
#[inline]
pub(crate) fn ceil(x: f64) -> f64 {
    round_to_integral(x, Rounding::Ceil)
}

/// Rounds toward zero.
#[inline]
pub(crate) fn trunc(x: f64) -> f64 {
    round_to_integral(x, Rounding::Trunc)
}

/// Rounds to the nearest integer, choosing the even integer on ties.
#[inline]
pub(crate) fn nearest(x: f64) -> f64 {
    round_to_integral(x, Rounding::NearestEven)
}

/// Rounds to the nearest integer, choosing the integer farther from zero on ties.
#[inline]
pub(crate) fn round_half_away(x: f64) -> f64 {
    round_to_integral(x, Rounding::NearestAway)
}

/// Returns `magnitude` with the sign bit of `sign` (WebAssembly `f64.copysign`).
///
/// This is a pure bit operation: NaN payloads in either operand are neither inspected nor
/// quieted, and a NaN `sign` contributes only its sign bit.
#[inline]
pub(crate) fn copysign(magnitude: f64, sign: f64) -> f64 {
    f64::from_bits((magnitude.to_bits() & !SIGN_MASK) | (sign.to_bits() & SIGN_MASK))
}

/// Truncates toward zero and saturates to `i64` (WebAssembly `i64.trunc_sat_f64_s`).
///
/// Values at or beyond the `i64` range clamp to `i64::MIN` / `i64::MAX` (including the
/// infinities), and NaN converts to `0`. This never fails.
#[inline]
pub(crate) fn to_i64_sat(x: f64) -> i64 {
    // Rust defines float-to-integer `as` casts as exactly this saturating truncation.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "saturating truncation is the defined semantics of this operation"
    )]
    let out = x as i64;
    out
}

/// Truncates toward zero and saturates to `u64` (WebAssembly `i64.trunc_sat_f64_u`).
///
/// Negative values (and `-inf`) clamp to `0`, values at or above 2^64 (and `+inf`) clamp to
/// `u64::MAX`, and NaN converts to `0`. This never fails.
#[inline]
pub(crate) fn to_u64_sat(x: f64) -> u64 {
    // Rust defines float-to-integer `as` casts as exactly this saturating truncation.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "saturating truncation is the defined semantics of this operation"
    )]
    let out = x as u64;
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

/// Truncates toward zero, failing on NaN, infinities and values whose truncation is outside `i64`
/// (WebAssembly `i64.trunc_f64_s`).
#[inline]
pub(crate) fn to_i64(x: f64) -> Result<i64, FloatToIntError> {
    if !x.is_finite() {
        return Err(FloatToIntError::Invalid);
    }
    // -2^63 is exact and the next `f64` below it is -2^63 - 2048, so `x < -2^63` is exactly
    // "truncates below `i64::MIN`".
    if x < -9_223_372_036_854_775_808.0 || x >= 9_223_372_036_854_775_808.0 {
        return Err(FloatToIntError::Overflow);
    }
    Ok(to_i64_sat(x))
}

/// Truncates toward zero, failing on NaN, infinities and values whose truncation is outside `u64`
/// (WebAssembly `i64.trunc_f64_u`).
///
/// Truncation comes first: `-0.5` truncates to `-0.0` and converts to `0`; only values at or
/// below `-1.0` are out of range below.
#[inline]
pub(crate) fn to_u64(x: f64) -> Result<u64, FloatToIntError> {
    if !x.is_finite() {
        return Err(FloatToIntError::Invalid);
    }
    if x <= -1.0 || x >= 18_446_744_073_709_551_616.0 {
        return Err(FloatToIntError::Overflow);
    }
    Ok(to_u64_sat(x))
}

/// Returns the input NaN with its quiet bit set.
#[inline]
fn quiet_nan(bits: u64) -> f64 {
    f64::from_bits(bits | QUIET_BIT)
}

#[derive(Clone, Copy)]
enum Rounding {
    Floor,
    Ceil,
    Trunc,
    NearestEven,
    NearestAway,
}

impl Rounding {
    /// Whether a value with a non-zero fractional part rounds away from zero.
    ///
    /// `frac_vs_half` compares the discarded fraction with one half, and `int_is_odd` reports the
    /// parity of the magnitude truncated toward zero.
    #[inline]
    fn away_from_zero(
        self,
        negative: bool,
        frac_vs_half: core::cmp::Ordering,
        int_is_odd: bool,
    ) -> bool {
        use core::cmp::Ordering;
        match self {
            Self::Floor => negative,
            Self::Ceil => !negative,
            Self::Trunc => false,
            Self::NearestEven => match frac_vs_half {
                Ordering::Greater => true,
                Ordering::Equal => int_is_odd,
                Ordering::Less => false,
            },
            Self::NearestAway => frac_vs_half != Ordering::Less,
        }
    }
}

fn round_to_integral(x: f64, mode: Rounding) -> f64 {
    let bits = x.to_bits();
    let sign = bits & SIGN_MASK;
    let mag = bits & !SIGN_MASK;
    let biased_exp = mag >> FRAC_BITS;

    if mag > EXP_MASK {
        return quiet_nan(bits);
    }
    // Infinities and every finite value with |x| >= 2^52 are already integral.
    if biased_exp >= EXP_BIAS + FRAC_BITS {
        return x;
    }
    if mag == 0 {
        return x;
    }
    let negative = sign != 0;

    if biased_exp < EXP_BIAS {
        // 0 < |x| < 1 (including subnormals): the result is a signed zero or a signed one, and
        // the truncated integer (zero) is even.
        let frac_vs_half = mag.cmp(&HALF_BITS);
        let away = mode.away_from_zero(negative, frac_vs_half, false);
        return f64::from_bits(sign | if away { ONE_BITS } else { 0 });
    }

    // 1 <= |x| < 2^52: the low `frac_bits` (1..=52) bits of the significand are fractional.
    let frac_bits = EXP_BIAS + FRAC_BITS - biased_exp;
    let unit = 1_u64 << frac_bits;
    let frac = mag & (unit - 1);
    if frac == 0 {
        return x;
    }
    let int = mag & !(unit - 1);
    // The units bit of the integer part sits at `frac_bits`. For `frac_bits == 52` that is the
    // lowest exponent bit, which is set for the odd biased exponent 1023 of [1, 2): the implicit
    // leading one.
    let int_is_odd = int & unit != 0;
    let frac_vs_half = frac.cmp(&(unit >> 1));
    if mode.away_from_zero(negative, frac_vs_half, int_is_odd) {
        // Adding one unit may carry out of the significand into the exponent, which is exactly
        // the next power of two.
        f64::from_bits(sign | (int + unit))
    } else {
        f64::from_bits(sign | int)
    }
}

/// Returns the correctly rounded square root (round-to-nearest, ties-to-even).
///
/// `sqrt(-0.0) == -0.0`, `sqrt(+inf) == +inf`, a NaN input is quieted (sign and payload kept), and
/// any other negative input (including `-inf`) yields [`f64::NAN`].
pub(crate) fn sqrt(x: f64) -> f64 {
    let bits = x.to_bits();
    let mag = bits & !SIGN_MASK;

    if mag == 0 {
        return x;
    }
    if mag > EXP_MASK {
        return quiet_nan(bits);
    }
    if bits & SIGN_MASK != 0 {
        return f64::NAN;
    }
    if mag == EXP_MASK {
        return x;
    }

    // Normalize to x = m * 2^(exp - EXP_BIAS - FRAC_BITS) with m in [2^52, 2^53).
    let mut m = mag & FRAC_MASK;
    // The biased exponent field is 11 bits wide; a subnormal's effective exponent may be negative.
    let mut exp = (mag >> FRAC_BITS) as i64;
    if exp == 0 {
        // Subnormal: shift the leading one up to bit 52.
        let shift = m.leading_zeros() - 11;
        m <<= shift;
        exp = 1 - i64::from(shift);
    } else {
        m |= 1 << FRAC_BITS;
    }

    // x = (m / 2^52) * 2^e. Make `e` even so that sqrt(x) = sqrt(m / 2^52) * 2^(e / 2);
    // afterwards m is in [2^52, 2^54).
    let mut e = exp - EXP_BIAS as i64;
    if e & 1 != 0 {
        m <<= 1;
        e -= 1;
    }

    // sqrt(m / 2^52) = isqrt(m * 2^52) / 2^52 with m * 2^52 in [2^104, 2^106), so the integer
    // root `q` has exactly 53 bits: a full significand.
    let (q, rem) = isqrt_shifted(m);
    // The true root lies in [q, q + 1). It exceeds q + 1/2 exactly when
    // m * 2^52 > q^2 + q + 1/4, i.e. (for integers) when rem > q. An exact tie is impossible.
    let round_up = u64::from(rem > q);

    // `q` carries the implicit leading one at bit 52, which adds one to the exponent field.
    // Rounding up may carry into the exponent, which again yields the right power of two.
    // e / 2 is in [-537, 511], so the biased result exponent is a positive normal one.
    let exp_field = (e / 2 + EXP_BIAS as i64 - 1) as u64;
    f64::from_bits((exp_field << FRAC_BITS) + q + round_up)
}

/// Returns the remainder of truncating division, `x - trunc(x / y) * y`, computed exactly.
///
/// This is IEEE 754 `fmod` (Rust's `%`): the result has the sign of `x` and a magnitude below
/// `|y|`, and it is always representable, so nothing rounds. `rem(x, ±inf)` is `x` for finite `x`
/// (including a signed zero); `rem(±inf, y)`, `rem(x, ±0)` and NaN inputs are NaN. A NaN input is
/// quieted with its payload kept; an invalid operation yields [`f64::NAN`].
///
/// Rust's `%` lowers to a call to the platform's `fmod` (the C library, or `compiler-builtins` on
/// `wasm32` and `no_std` targets). Computing it here keeps the result independent of which
/// implementation a target links.
pub(crate) fn rem(x: f64, y: f64) -> f64 {
    let x_bits = x.to_bits();
    let y_bits = y.to_bits();
    let x_mag = x_bits & !SIGN_MASK;
    let y_mag = y_bits & !SIGN_MASK;

    if x_mag > EXP_MASK {
        return quiet_nan(x_bits);
    }
    if y_mag > EXP_MASK {
        return quiet_nan(y_bits);
    }
    if x_mag == EXP_MASK || y_mag == 0 {
        return f64::NAN;
    }
    // Ordering magnitudes by their bits orders them by value; this also covers `y = ±inf` and
    // `x = ±0`.
    if x_mag < y_mag {
        return x;
    }

    // Both are finite and non-zero with |x| >= |y|. Write each as `m * 2^(e - 1075)` with an
    // integer significand `m < 2^53` and `e >= 1` (a subnormal has `e = 1` and no implicit bit),
    // so `e_x >= e_y`.
    let (m_x, e_x) = significand(x_mag);
    let (m_y, e_y) = significand(y_mag);
    // r = (m_x * 2^(e_x - e_y)) mod m_y, reducing up to 64 exponent steps at a time: r < 2^53,
    // so `r << 64` fits in `u128`.
    let m_y = u128::from(m_y);
    let mut r = u128::from(m_x) % m_y;
    let mut steps = e_x - e_y;
    while steps > 0 && r != 0 {
        let shift = steps.min(64);
        r = (r << shift) % m_y;
        steps -= shift;
    }
    let sign = x_bits & SIGN_MASK;
    let r = u64::try_from(r).expect("the remainder is below a 53-bit significand");
    if r == 0 {
        return f64::from_bits(sign);
    }

    // The result is `r * 2^(e_y - 1075)`. Normalize the leading one to bit 52 if the exponent
    // allows it, otherwise the result is subnormal.
    let lead = u64::from(r.leading_zeros() - 11);
    let bits = if e_y > lead {
        ((e_y - lead) << FRAC_BITS) | ((r << lead) & FRAC_MASK)
    } else {
        r << (e_y - 1)
    };
    f64::from_bits(sign | bits)
}

/// Splits a finite, non-zero magnitude into an integer significand and a biased exponent of at
/// least one, so that the value is `significand * 2^(exponent - 1075)`.
#[inline]
fn significand(mag: u64) -> (u64, u64) {
    let exp = mag >> FRAC_BITS;
    if exp == 0 {
        (mag, 1)
    } else {
        ((mag & FRAC_MASK) | (1 << FRAC_BITS), exp)
    }
}

/// Integer square root of `m * 2^52` for `m` in `[2^52, 2^54)`, returning `(root, remainder)`.
///
/// Classic restoring digit-by-digit square root, consuming two radicand bits per step. The
/// radicand needs 106 bits, but its low 52 bits are zero and the running remainder never exceeds
/// `2 * root`, so all state fits in `u64`.
fn isqrt_shifted(m: u64) -> (u64, u64) {
    debug_assert!(
        (1 << 52..1 << 54).contains(&m),
        "significand must be normalized"
    );
    let mut root: u64 = 0;
    let mut rem: u64 = 0;
    // 53 pairs of radicand bits, most significant first; pair `i` covers radicand bits
    // 2i+1..=2i, which are bits 2i-51..=2i-52 of `m` (or zero below bit 52 of the radicand).
    for i in (0..53_u32).rev() {
        let pair = if 2 * i >= 52 {
            (m >> (2 * i - 52)) & 3
        } else {
            0
        };
        rem = (rem << 2) | pair;
        let trial = (root << 2) | 1;
        root <<= 1;
        if rem >= trial {
            rem -= trial;
            root |= 1;
        }
    }
    (root, rem)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::{
        FloatToIntError, ceil, copysign, floor, nearest, rem, round_half_away, sqrt, to_i64,
        to_i64_sat, to_u64, to_u64_sat, trunc,
    };
    use std::vec::Vec;

    /// Bitwise equality, treating any two NaNs as equal.
    fn same(a: f64, b: f64) -> bool {
        (a.is_nan() && b.is_nan()) || a.to_bits() == b.to_bits()
    }

    #[track_caller]
    fn assert_same(op: &str, x: f64, got: f64, want: f64) {
        assert!(
            same(got, want),
            "{op}({x:e} = {:#018x}) = {got:e} ({:#018x}), want {want:e} ({:#018x})",
            x.to_bits(),
            got.to_bits(),
            want.to_bits()
        );
    }

    /// `SplitMix64`: a tiny deterministic generator for reproducible bit patterns.
    struct SplitMix64(u64);

    impl SplitMix64 {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
    }

    fn edge_cases() -> Vec<f64> {
        let mut xs = Vec::new();
        let base = [
            0.0,
            0.25,
            0.49999999999999994,
            0.5,
            0.5000000000000001,
            0.7,
            0.9999999999999999,
            1.0,
            1.0000000000000002,
            1.25,
            1.5,
            1.75,
            2.0,
            2.5,
            3.5,
            4.5,
            5.5,
            1e10 + 0.5,
            4503599627370495.5,        // 2^52 - 0.5
            4503599627370496.0,        // 2^52
            4503599627370497.0,        // 2^52 + 1
            9007199254740991.0,        // 2^53 - 1
            9007199254740992.0,        // 2^53
            2251799813685248.0 - 0.25, // 2^51 - 0.25
            2251799813685248.5,        // 2^51 + 0.5
            f64::MIN_POSITIVE,
            f64::from_bits(1),                     // smallest subnormal
            f64::from_bits(0x000F_FFFF_FFFF_FFFF), // largest subnormal
            f64::from_bits(0x0008_0000_0000_0000), // a mid subnormal
            f64::MAX,
            f64::EPSILON,
            core::f64::consts::PI,
            core::f64::consts::E,
            4.0 - f64::EPSILON * 2.0,
            2.0 - f64::EPSILON,
            1e300,
            1e-300,
            f64::INFINITY,
        ];
        for &x in &base {
            xs.push(x);
            xs.push(-x);
            // Neighbours probe the boundaries of every interesting value.
            if x.is_finite() {
                xs.push(f64::from_bits(x.to_bits() + 1));
                xs.push(-f64::from_bits(x.to_bits() + 1));
                if x != 0.0 {
                    xs.push(f64::from_bits(x.to_bits() - 1));
                    xs.push(-f64::from_bits(x.to_bits() - 1));
                }
            }
        }
        // Every half-integer and quarter in a small range, and every power of two.
        for i in -64_i32..=64 {
            xs.push(f64::from(i) * 0.25);
        }
        for e in -1074_i32..=1023 {
            xs.push(pow2(e));
        }
        xs
    }

    fn pow2(e: i32) -> f64 {
        if e >= -1022 {
            f64::from_bits(((e + 1023) as u64) << 52)
        } else {
            f64::from_bits(1 << (e + 1074))
        }
    }

    fn random_inputs(count: usize) -> Vec<f64> {
        let mut rng = SplitMix64(0x5EED_F00D_CAFE_BEEF);
        let mut xs = Vec::with_capacity(count);
        for i in 0..count {
            let r = rng.next();
            let x = match i % 4 {
                // Any bit pattern.
                0 => f64::from_bits(r),
                // Magnitudes in [2^-8, 2^60), where rounding is most interesting.
                1 => {
                    let exp = 1015 + (r >> 58) % 68;
                    f64::from_bits((r & (1 << 63 | super::FRAC_MASK)) | (exp << 52))
                }
                // Values near half-integers.
                2 => {
                    let k = (r >> 12) as i64 as f64 * (1.0 / 4096.0);
                    let x = k.trunc() + 0.5;
                    let nudge = (r & 3) as i64 - 1;
                    f64::from_bits(x.to_bits().wrapping_add(nudge as u64))
                }
                // Subnormals.
                _ => f64::from_bits(r & (1 << 63 | super::FRAC_MASK)),
            };
            xs.push(x);
        }
        xs
    }

    fn check_against_std(xs: &[f64]) {
        for &x in xs {
            assert_same("floor", x, floor(x), x.floor());
            assert_same("ceil", x, ceil(x), x.ceil());
            assert_same("trunc", x, trunc(x), x.trunc());
            assert_same("nearest", x, nearest(x), x.round_ties_even());
            assert_same("round_half_away", x, round_half_away(x), x.round());
            assert_same("sqrt", x, sqrt(x), x.sqrt());
        }
    }

    #[test]
    fn matches_std_on_edge_cases() {
        check_against_std(&edge_cases());
    }

    #[test]
    fn matches_std_on_pseudo_random_inputs() {
        check_against_std(&random_inputs(400_000));
    }

    #[test]
    fn signed_zero_results() {
        let neg_zero = (-0.0_f64).to_bits();
        let pos_zero = 0.0_f64.to_bits();
        assert_eq!(floor(-0.0).to_bits(), neg_zero);
        assert_eq!(floor(0.0).to_bits(), pos_zero);
        assert_eq!(floor(0.7).to_bits(), pos_zero);
        assert_eq!(ceil(-0.5).to_bits(), neg_zero);
        assert_eq!(ceil(-0.0).to_bits(), neg_zero);
        assert_eq!(trunc(-0.7).to_bits(), neg_zero);
        assert_eq!(trunc(0.7).to_bits(), pos_zero);
        assert_eq!(nearest(-0.5).to_bits(), neg_zero);
        assert_eq!(nearest(0.5).to_bits(), pos_zero);
        assert_eq!(nearest(-0.49999999999999994).to_bits(), neg_zero);
        assert_eq!(nearest(-f64::from_bits(1)).to_bits(), neg_zero);
        assert_eq!(round_half_away(-0.49999999999999994).to_bits(), neg_zero);
        assert_eq!(round_half_away(-0.0).to_bits(), neg_zero);
        assert_eq!(sqrt(-0.0).to_bits(), neg_zero);
        assert_eq!(sqrt(0.0).to_bits(), pos_zero);
    }

    #[test]
    fn rounding_values() {
        assert_eq!(floor(-0.5), -1.0);
        assert_eq!(floor(-1.0), -1.0);
        assert_eq!(floor(1.9999999999999998), 1.0);
        assert_eq!(ceil(0.3), 1.0);
        assert_eq!(ceil(-1.5), -1.0);
        assert_eq!(ceil(f64::from_bits(1)), 1.0);
        assert_eq!(trunc(-2.9), -2.0);
        assert_eq!(nearest(0.49999999999999994), 0.0);
        assert_eq!(nearest(0.5000000000000001), 1.0);
        assert_eq!(nearest(1.5), 2.0);
        assert_eq!(nearest(2.5), 2.0);
        assert_eq!(nearest(3.5), 4.0);
        assert_eq!(nearest(-2.5), -2.0);
        assert_eq!(nearest(-3.5), -4.0);
        assert_eq!(nearest(4503599627370495.5), 4503599627370496.0);
        // Already integral values (|x| >= 2^52) are unchanged.
        assert_eq!(round_half_away(0.49999999999999994), 0.0);
        assert_eq!(round_half_away(0.5), 1.0);
        assert_eq!(round_half_away(2.5), 3.0);
        assert_eq!(round_half_away(-2.5), -3.0);
        assert_eq!(round_half_away(4503599627370495.5), 4503599627370496.0);
        for x in [4503599627370497.0, 9007199254740993.0, 1e300, f64::MAX] {
            for op in [floor, ceil, trunc, nearest, round_half_away] {
                assert_eq!(op(x).to_bits(), x.to_bits());
                assert_eq!(op(-x).to_bits(), (-x).to_bits());
            }
        }
    }

    #[test]
    fn infinities_pass_through() {
        for op in [floor, ceil, trunc, nearest, round_half_away] {
            assert_eq!(op(f64::INFINITY), f64::INFINITY);
            assert_eq!(op(f64::NEG_INFINITY), f64::NEG_INFINITY);
        }
        assert_eq!(sqrt(f64::INFINITY), f64::INFINITY);
        assert_eq!(sqrt(f64::NEG_INFINITY).to_bits(), f64::NAN.to_bits());
    }

    #[test]
    fn nan_inputs_are_quieted_with_payload() {
        let quiet = 0x7FF8_0000_0000_0001_u64;
        let signaling = 0x7FF0_0000_0000_0001_u64;
        let neg_signaling = 0xFFF4_0000_0000_0000_u64;
        for op in [floor, ceil, trunc, nearest, round_half_away, sqrt] {
            assert_eq!(op(f64::from_bits(quiet)).to_bits(), quiet);
            assert_eq!(op(f64::from_bits(signaling)).to_bits(), quiet);
            assert_eq!(
                op(f64::from_bits(neg_signaling)).to_bits(),
                0xFFFC_0000_0000_0000
            );
        }
    }

    #[test]
    fn sqrt_of_negative_is_canonical_nan() {
        for x in [-1.0, -f64::from_bits(1), -f64::MAX, -4.0] {
            assert_eq!(sqrt(x).to_bits(), f64::NAN.to_bits());
        }
    }

    #[test]
    fn sqrt_values() {
        assert_eq!(sqrt(4.0), 2.0);
        assert_eq!(sqrt(2.0), core::f64::consts::SQRT_2);
        assert_eq!(sqrt(f64::from_bits(1)), pow2(-537));
        assert_eq!(sqrt(f64::MAX), 1.3407807929942596e154);
        // Perfect squares of every magnitude are exact.
        let mut rng = SplitMix64(7);
        for _ in 0..10_000 {
            let k = (rng.next() >> 38) as f64; // < 2^26, so k * k is exact
            assert_eq!(sqrt(k * k), k);
        }
    }

    #[test]
    fn sqrt_is_correctly_rounded() {
        // Independent of std: r = sqrt(x) is correctly rounded iff x lies strictly between the
        // squares of the midpoints on either side of r. Check that with exact integer arithmetic
        // on significands scaled to a common exponent.
        let mut rng = SplitMix64(0xA11CE);
        for _ in 0..20_000 {
            // Normal x in [1, 4) keeps r in [1, 2) so both significands share one scale.
            let frac = rng.next() & super::FRAC_MASK;
            let x = f64::from_bits(frac | ((1023 + (rng.next() & 1)) << 52));
            let r = sqrt(x);
            assert!(
                (1.0..2.0).contains(&r),
                "sqrt({x:e}) = {r:e} is outside [1, 2)"
            );
            // x = xs / 2^52, r = rs / 2^52; midpoint (2 rs ± 1) / 2^53.
            let xs = u128::from(x.to_bits() & super::FRAC_MASK | 1 << 52)
                << ((x.to_bits() >> 52) - 1023);
            let rs = u128::from(r.to_bits() & super::FRAC_MASK | 1 << 52);
            // Compare x * 2^106 against ((2 rs ± 1))^2 * 2^0: x * 2^106 = xs * 2^54.
            let lhs = xs << 54;
            let lo = (2 * rs - 1) * (2 * rs - 1);
            let hi = (2 * rs + 1) * (2 * rs + 1);
            assert!(
                lo < lhs && lhs < hi,
                "sqrt({x:e}) = {r:e} is not correctly rounded"
            );
        }
    }

    /// The exact integer value of `trunc(x)` for finite `x` with `|x| < 2^120`, computed from the
    /// bit representation (independently of float-to-integer casts).
    fn truncated_value(x: f64) -> i128 {
        let bits = x.to_bits();
        let biased = ((bits >> 52) & 0x7FF) as i32;
        if biased < 1023 {
            return 0;
        }
        let m = i128::from(bits & super::FRAC_MASK | 1 << 52);
        let shift = biased - 1075;
        let mag = if shift >= 0 { m << shift } else { m >> -shift };
        if bits >> 63 == 1 { -mag } else { mag }
    }

    fn reference_sat(x: f64, min: i128, max: i128) -> i128 {
        if x.is_nan() {
            0
        } else if x == f64::INFINITY || x >= 2.0_f64.powi(120) {
            max
        } else if x == f64::NEG_INFINITY || x <= -(2.0_f64.powi(120)) {
            min
        } else {
            truncated_value(x).clamp(min, max)
        }
    }

    fn saturation_inputs() -> Vec<f64> {
        let two63 = 9_223_372_036_854_775_808.0_f64;
        let two64 = 18_446_744_073_709_551_616.0_f64;
        let mut xs = Vec::new();
        for x in [
            0.0,
            0.5,
            0.9999999999999999,
            1.0,
            1.5,
            2.5,
            1e18,
            two63,
            two64,
            f64::MAX,
            f64::from_bits(1),
            f64::INFINITY,
        ] {
            xs.push(x);
            xs.push(-x);
            if x.is_finite() {
                for neighbour in [
                    f64::from_bits(x.to_bits() + 1),
                    f64::from_bits(x.to_bits().saturating_sub(1)),
                ] {
                    xs.push(neighbour);
                    xs.push(-neighbour);
                }
            }
        }
        xs.extend([
            f64::NAN,
            -f64::NAN,
            f64::from_bits(0x7FF0_0000_0000_0001),
            f64::from_bits(0xFFF4_0000_0000_0000),
        ]);
        xs.extend(random_inputs(100_000));
        xs
    }

    #[test]
    fn to_i64_sat_truncates_and_clamps() {
        let two63 = 9_223_372_036_854_775_808.0_f64;
        assert_eq!(to_i64_sat(two63), i64::MAX);
        assert_eq!(
            to_i64_sat(f64::from_bits(two63.to_bits() - 1)),
            9_223_372_036_854_774_784
        );
        assert_eq!(to_i64_sat(-two63), i64::MIN);
        assert_eq!(to_i64_sat(-f64::from_bits(two63.to_bits() + 1)), i64::MIN);
        assert_eq!(to_i64_sat(f64::INFINITY), i64::MAX);
        assert_eq!(to_i64_sat(f64::NEG_INFINITY), i64::MIN);
        assert_eq!(to_i64_sat(f64::NAN), 0);
        assert_eq!(to_i64_sat(-f64::NAN), 0);
        assert_eq!(to_i64_sat(-0.0), 0);
        assert_eq!(to_i64_sat(-0.9999999999999999), 0);
        assert_eq!(to_i64_sat(-1.5), -1);
        assert_eq!(to_i64_sat(2.9), 2);
        for x in saturation_inputs() {
            let want = reference_sat(x, i128::from(i64::MIN), i128::from(i64::MAX));
            assert_eq!(i128::from(to_i64_sat(x)), want, "to_i64_sat({x:e})");
        }
    }

    #[test]
    fn to_u64_sat_truncates_and_clamps() {
        let two64 = 18_446_744_073_709_551_616.0_f64;
        assert_eq!(to_u64_sat(two64), u64::MAX);
        assert_eq!(
            to_u64_sat(f64::from_bits(two64.to_bits() - 1)),
            18_446_744_073_709_549_568
        );
        assert_eq!(to_u64_sat(9_223_372_036_854_775_808.0), 1 << 63);
        assert_eq!(to_u64_sat(f64::INFINITY), u64::MAX);
        assert_eq!(to_u64_sat(f64::NEG_INFINITY), 0);
        assert_eq!(to_u64_sat(f64::NAN), 0);
        assert_eq!(to_u64_sat(-0.0), 0);
        assert_eq!(to_u64_sat(-0.5), 0);
        assert_eq!(to_u64_sat(-1.0), 0);
        assert_eq!(to_u64_sat(-f64::MAX), 0);
        assert_eq!(to_u64_sat(2.9), 2);
        for x in saturation_inputs() {
            let want = reference_sat(x, 0, i128::from(u64::MAX));
            assert_eq!(i128::from(to_u64_sat(x)), want, "to_u64_sat({x:e})");
        }
    }

    /// Truncate first, then range-check: the trapping conversions' reference semantics.
    fn reference_trapping(x: f64, min: i128, max: i128) -> Result<i128, FloatToIntError> {
        if !x.is_finite() {
            return Err(FloatToIntError::Invalid);
        }
        if x.abs() >= 2.0_f64.powi(120) {
            return Err(FloatToIntError::Overflow);
        }
        let t = truncated_value(x);
        if (min..=max).contains(&t) {
            Ok(t)
        } else {
            Err(FloatToIntError::Overflow)
        }
    }

    #[test]
    fn trapping_conversions_truncate_before_range_checking() {
        let two63 = 9_223_372_036_854_775_808.0_f64;
        let two64 = 18_446_744_073_709_551_616.0_f64;
        assert_eq!(to_u64(-0.0), Ok(0));
        assert_eq!(to_u64(-0.5), Ok(0));
        assert_eq!(to_u64(-0.9999999999999999), Ok(0));
        assert_eq!(to_u64(-1.0), Err(FloatToIntError::Overflow));
        assert_eq!(
            to_u64(f64::from_bits(two64.to_bits() - 1)),
            Ok(18_446_744_073_709_549_568)
        );
        assert_eq!(to_u64(two64), Err(FloatToIntError::Overflow));
        assert_eq!(to_u64(f64::NAN), Err(FloatToIntError::Invalid));
        assert_eq!(to_u64(f64::NEG_INFINITY), Err(FloatToIntError::Invalid));
        assert_eq!(to_i64(-0.5), Ok(0));
        assert_eq!(to_i64(-two63), Ok(i64::MIN));
        assert_eq!(
            to_i64(-f64::from_bits(two63.to_bits() + 1)),
            Err(FloatToIntError::Overflow)
        );
        assert_eq!(to_i64(two63), Err(FloatToIntError::Overflow));
        assert_eq!(to_i64(f64::INFINITY), Err(FloatToIntError::Invalid));
        for x in saturation_inputs() {
            let i64_range = (i128::from(i64::MIN), i128::from(i64::MAX));
            let want = reference_trapping(x, i64_range.0, i64_range.1);
            assert_eq!(to_i64(x).map(i128::from), want, "to_i64({x:e})");
            let want = reference_trapping(x, 0, i128::from(u64::MAX));
            assert_eq!(to_u64(x).map(i128::from), want, "to_u64({x:e})");
        }
    }

    #[test]
    fn copysign_moves_only_the_sign_bit() {
        let sign = 1_u64 << 63;
        let signaling = 0x7FF0_0000_0000_0001_u64;
        let samples = [
            0.0,
            -0.0,
            1.5,
            -2.0,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::from_bits(1),
            f64::MAX,
            f64::NAN,
            f64::from_bits(signaling),
            f64::from_bits(signaling | sign),
        ];
        for a in samples {
            for b in samples {
                let want = (a.to_bits() & !sign) | (b.to_bits() & sign);
                assert_eq!(copysign(a, b).to_bits(), want, "copysign({a:e}, {b:e})");
            }
        }
        // A signaling NaN magnitude stays signaling: the payload is untouched.
        assert_eq!(
            copysign(f64::from_bits(signaling), -1.0).to_bits(),
            signaling | sign
        );
        assert_eq!(copysign(3.0, -0.0), -3.0);
        assert_eq!(copysign(-3.0, f64::from_bits(signaling)), 3.0);
        let mut rng = SplitMix64(0xC0DE);
        for _ in 0..10_000 {
            let (a, b) = (f64::from_bits(rng.next()), f64::from_bits(rng.next()));
            assert_eq!(
                copysign(a, b).to_bits(),
                (a.to_bits() & !sign) | (b.to_bits() & sign),
                "copysign({a:e}, {b:e})"
            );
        }
    }

    #[test]
    fn rem_matches_std_on_edge_cases() {
        // Every power of two would make the grid large; a spread of them keeps every exponent
        // distance class.
        let mut xs: Vec<f64> = edge_cases()
            .into_iter()
            .enumerate()
            .filter(|&(i, x)| x.to_bits() & super::FRAC_MASK != 0 || i % 23 == 0 || x.abs() < 8.0)
            .map(|(_, x)| x)
            .collect();
        xs.extend([f64::NAN, -f64::NAN, f64::from_bits(0x7FF0_0000_0000_0001)]);
        xs.sort_by_key(|x| x.to_bits());
        xs.dedup_by_key(|x| x.to_bits());
        for &x in &xs {
            for &y in &xs {
                let (got, want) = (rem(x, y), x % y);
                assert!(
                    same(got, want),
                    "rem({x:e}, {y:e}) = {got:e} ({:#018x}), want {want:e} ({:#018x})",
                    got.to_bits(),
                    want.to_bits()
                );
            }
        }
    }

    #[test]
    fn rem_matches_std_on_pseudo_random_inputs() {
        let xs = random_inputs(40_000);
        let ys = random_inputs(40_001);
        let mut rng = SplitMix64(0x0F0D);
        for (&x, &y) in xs.iter().zip(ys.iter().skip(1)) {
            // Also bring the exponents close, where most of the quotient bits are significant.
            let near =
                f64::from_bits((y.to_bits() & !super::EXP_MASK) | (x.to_bits() & super::EXP_MASK));
            for y in [y, near, f64::from_bits(rng.next())] {
                let (got, want) = (rem(x, y), x % y);
                assert!(
                    same(got, want),
                    "rem({x:e}, {y:e}) = {got:e} ({:#018x}), want {want:e} ({:#018x})",
                    got.to_bits(),
                    want.to_bits()
                );
            }
        }
    }

    #[test]
    fn rem_values() {
        let bits = |x: f64| x.to_bits();
        assert_eq!(rem(5.5, 2.0), 1.5);
        assert_eq!(rem(-5.5, 2.0), -1.5);
        assert_eq!(rem(5.5, -2.0), 1.5);
        assert_eq!(rem(-5.5, -2.0), -1.5);
        assert_eq!(bits(rem(-4.0, 2.0)), bits(-0.0));
        assert_eq!(bits(rem(-0.0, 1.0)), bits(-0.0));
        assert_eq!(bits(rem(-0.0, f64::INFINITY)), bits(-0.0));
        assert_eq!(rem(3.0, f64::NEG_INFINITY), 3.0);
        assert_eq!(rem(f64::MAX, 3.0), 2.0);
        assert_eq!(rem(f64::MAX, f64::from_bits(1)), 0.0);
        // A subnormal result is exact.
        assert_eq!(
            bits(rem(f64::MIN_POSITIVE * 1.5, f64::MIN_POSITIVE)),
            0x0008_0000_0000_0000
        );
        assert_eq!(bits(rem(f64::INFINITY, 1.0)), bits(f64::NAN));
        assert_eq!(bits(rem(1.0, 0.0)), bits(f64::NAN));
        assert_eq!(bits(rem(1.0, -0.0)), bits(f64::NAN));
        // A NaN operand is quieted and kept.
        assert_eq!(
            bits(rem(f64::from_bits(0x7FF0_0000_0000_0001), 1.0)),
            0x7FF8_0000_0000_0001
        );
        assert_eq!(
            bits(rem(1.0, f64::from_bits(0xFFF0_0000_0000_0002))),
            0xFFF8_0000_0000_0002
        );
    }
}
