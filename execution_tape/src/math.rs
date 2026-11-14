// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The floating-point operations the VM computes, as plain functions.
//!
//! Each function here is exactly what the VM runs for the opcode it names. Native Rust that calls
//! it computes the same bits as a tape program, on every platform, so native code and tape cannot
//! disagree, and any later compiler tier calls the same function. Operations that Rust spells as
//! operators (`+`, `-`, `*`, `/`, unary `-` and the comparisons) are not repeated here: the VM
//! computes exactly what those operators compute.
//!
//! # Two kinds of guarantee
//!
//! **Exact operations** (rounding, square root, remainder, `abs`, `copysign`, `min`/`max` and
//! `clamp`) have one correct result, which `docs/v1_spec.md` specifies. They are implemented in
//! this crate without `libm`. The table below names the `libm` function that computes the same
//! bits, for code that already calls `libm`.
//!
//! **Transcendental functions** (`pow`, `exp`, `ln`, `sin`, `cos`, `atan2`, in both widths) are
//! *defined* as the
//! results of the [`libm`](https://crates.io/crates/libm) crate at exactly version **0.2.16**,
//! built without its default features. They are not correctly rounded; they are reproducible.
//! Upgrading the pinned version, or replacing the implementation (for example with correctly
//! rounded routines), changes results and is a deliberate, versioned change to this module, called
//! out in the changelog and the specification.
//!
//! # NaN results
//!
//! Every non-NaN result is bit-for-bit identical on every platform. For a NaN result, the
//! specification's floating-point determinism contract guarantees only that it is a NaN, except
//! for [`abs_f32`], [`copysign_f32`] and their `f64` forms, which only move the sign bit and
//! preserve every other bit. Within that:
//! - the transcendental functions' NaN sign and payload come from the hardware arithmetic inside
//!   `libm` and differ between targets;
//! - the exact operations' NaN bits are deterministic and documented on each function, but are
//!   not part of the contract.
//!
//! # Platform independence
//!
//! `libm` is pure Rust. With default features off, its `arch` feature (hardware `sqrt`, `fma` and
//! `rint` on `x86_64` and `aarch64`) is not enabled, and `unstable-intrinsics` is never enabled, so
//! the same Rust code runs on every target. If another crate in a build enables those features
//! (Cargo unifies them), the only routines they replace are `sqrt`, `fma`, `rint`, `floor`,
//! `ceil`, `trunc` and `fabs`, each an exactly specified IEEE 754 operation whose hardware form
//! returns the same bits; only NaN payloads can change. 32-bit x86 targets without SSE2 (`i586`)
//! use x87 arithmetic and are outside this guarantee, as they are for ordinary `f32`/`f64`
//! arithmetic. None of these functions depends on the crate's `std` feature: a `std` build never
//! routes them to the platform's C library.
//!
//! # Functions and their `libm` counterparts
//!
//! Results agree bit for bit with the named `libm` 0.2.16 function for every input whose result is
//! not NaN; a NaN result is a NaN in both.
//!
//! | Function | VM opcode | `libm` counterpart |
//! |---|---|---|
//! | [`abs_f32`] / [`abs_f64`] | `f32.abs` / `f64_abs` | `fabsf` / `fabs` |
//! | [`copysign_f32`] / [`copysign_f64`] | `f32.copysign` / `f64_copysign` | `copysignf` / `copysign` |
//! | [`floor_f32`] / [`floor_f64`] | `f32.floor` / `f64_floor` | `floorf` / `floor` |
//! | [`ceil_f32`] / [`ceil_f64`] | `f32.ceil` / `f64_ceil` | `ceilf` / `ceil` |
//! | [`trunc_f32`] / [`trunc_f64`] | `f32.trunc` / `f64_trunc` | `truncf` / `trunc` |
//! | [`round_half_even_f32`] / [`round_half_even_f64`] | `f32.nearest` / `f64_nearest` | `roundevenf` / `roundeven` (and `rintf` / `rint`) |
//! | [`round_half_away_f32`] / [`round_half_away_f64`] | `f32.round_half_away` / `f64.round_half_away` | `roundf` / `round` |
//! | [`sqrt_f32`] / [`sqrt_f64`] | `f32.sqrt` / `f64_sqrt` | `sqrtf` / `sqrt` |
//! | [`rem_f32`] / [`rem_f64`] | `f32.rem` / `f64_rem` | `fmodf` / `fmod` |
//! | [`min_f32`] / [`min_f64`] | `f32.min` / `f64_min` | `fminimumf` / `fminimum` |
//! | [`max_f32`] / [`max_f64`] | `f32.max` / `f64_max` | `fmaximumf` / `fmaximum` |
//! | [`min_num_f32`] / [`min_num_f64`] | `f32.min_num` / `f64_min_num` | `fminimum_numf` / `fminimum_num` |
//! | [`max_num_f32`] / [`max_num_f64`] | `f32.max_num` / `f64_max_num` | `fmaximum_numf` / `fmaximum_num` |
//! | [`clamp_f32`] / [`clamp_f64`] | `f32.clamp` / `f64.clamp` | none (Rust's [`f32::clamp`] / [`f64::clamp`]) |
//! | [`pow_f32`] / [`pow_f64`] | `f32.pow` / `f64.pow` | `powf` / `pow` (defining) |
//! | [`exp_f32`] / [`exp_f64`] | `f32.exp` / `f64.exp` | `expf` / `exp` (defining) |
//! | [`ln_f32`] / [`ln_f64`] | `f32.ln` / `f64.ln` | `logf` / `log` (defining) |
//! | [`sin_f32`] / [`sin_f64`] | `f32.sin` / `f64.sin` | `sinf` / `sin` (defining) |
//! | [`cos_f32`] / [`cos_f64`] | `f32.cos` / `f64.cos` | `cosf` / `cos` (defining) |
//! | [`atan2_f32`] / [`atan2_f64`] | `f32.atan2` / `f64.atan2` | `atan2f` / `atan2` (defining) |
//!
//! `libm`'s `fmin`/`fmax` (IEEE 754-2008 `minNum`/`maxNum`) and Rust's `f32::min`/`f32::max` may
//! return either zero when the inputs are zeros of opposite sign; [`min_num_f32`] and friends
//! always order `-0.0` below `+0.0`.

use crate::{float, num32};

// --- f32: exact operations ------------------------------------------------------------------

/// `f32.abs`: `x` with its sign bit cleared. NaN payloads pass through unchanged.
#[inline]
#[must_use]
pub fn abs_f32(x: f32) -> f32 {
    num32::abs(x)
}

/// `f32.copysign`: the bits of `magnitude` with the sign bit of `sign`. No other bits are
/// inspected or changed, so NaN payloads pass through unchanged.
#[inline]
#[must_use]
pub fn copysign_f32(magnitude: f32, sign: f32) -> f32 {
    num32::copysign(magnitude, sign)
}

/// `f32.floor`: rounds toward negative infinity.
///
/// Exact. A zero result keeps the input's sign; infinities pass through; a NaN input returns that
/// NaN with its quiet bit set.
#[inline]
#[must_use]
pub fn floor_f32(x: f32) -> f32 {
    num32::floor(x)
}

/// `f32.ceil`: rounds toward positive infinity (`ceil(-0.5) == -0.0`).
///
/// Exact; signed zeros, infinities and NaNs as for [`floor_f32`].
#[inline]
#[must_use]
pub fn ceil_f32(x: f32) -> f32 {
    num32::ceil(x)
}

/// `f32.trunc`: rounds toward zero.
///
/// Exact; signed zeros, infinities and NaNs as for [`floor_f32`].
#[inline]
#[must_use]
pub fn trunc_f32(x: f32) -> f32 {
    num32::trunc(x)
}

/// `f32.nearest`: rounds to the nearest integer, **ties to even** (`2.5 -> 2.0`,
/// `-0.5 -> -0.0`).
///
/// This is IEEE 754 `roundToIntegralTiesToEven`, Rust's `f32::round_ties_even` and `libm`'s
/// `roundevenf`. It is *not* C's `roundf`; for that, see [`round_half_away_f32`]. Exact; signed
/// zeros, infinities and NaNs as for [`floor_f32`].
#[inline]
#[must_use]
pub fn round_half_even_f32(x: f32) -> f32 {
    num32::nearest(x)
}

/// `f32.round_half_away`: rounds to the nearest integer, **ties away from zero** (`2.5 -> 3.0`,
/// `-0.5 -> -1.0`).
///
/// This is IEEE 754 `roundToIntegralTiesToAway`, C's `roundf` and Rust's `f32::round`. It
/// differs from [`round_half_even_f32`] only on exact ties. Exact: `0.49999997` rounds to `0.0`,
/// and every value of magnitude at least 2^23 is already an integer and is returned unchanged.
/// Signed zeros, infinities and NaNs as for [`floor_f32`] (`round_half_away(-0.4) == -0.0`).
#[inline]
#[must_use]
pub fn round_half_away_f32(x: f32) -> f32 {
    num32::round_half_away(x)
}

/// `f32.sqrt`: the correctly rounded square root (round to nearest, ties to even).
///
/// `sqrt(-0.0) == -0.0` and `sqrt(+inf) == +inf`; a NaN input returns that NaN with its quiet bit
/// set; any other negative input returns the canonical quiet NaN.
#[inline]
#[must_use]
pub fn sqrt_f32(x: f32) -> f32 {
    num32::sqrt(x)
}

/// `f32.rem`: the remainder of division with the quotient truncated toward zero, carrying the
/// sign of `a` (Rust's `%`, C's `fmodf`). Exact.
///
/// `rem(x, ±inf)` is `x` for finite `x`; `rem(±inf, b)` and `rem(a, ±0.0)` are the canonical quiet
/// NaN; a NaN input returns that NaN with its quiet bit set (the first operand's if both are NaN).
/// Computed in this crate rather than with `%`, which Rust lowers to the platform's `fmod`.
#[inline]
#[must_use]
pub fn rem_f32(a: f32, b: f32) -> f32 {
    num32::rem(a, b)
}

/// `f32.min`: the lesser input, propagating NaN: if either input is NaN, the result is the
/// canonical quiet NaN. `-0.0` is less than `+0.0`.
#[inline]
#[must_use]
pub fn min_f32(a: f32, b: f32) -> f32 {
    num32::demote(min_f64(num32::promote(a), num32::promote(b)))
}

/// `f32.max`: the greater input, propagating NaN like [`min_f32`]. `+0.0` is greater than
/// `-0.0`.
#[inline]
#[must_use]
pub fn max_f32(a: f32, b: f32) -> f32 {
    num32::demote(max_f64(num32::promote(a), num32::promote(b)))
}

/// `f32.min_num`: the lesser input, favoring numbers: if exactly one input is NaN, the other is
/// returned; if both are, the result is the canonical quiet NaN. `-0.0` is less than `+0.0`.
#[inline]
#[must_use]
pub fn min_num_f32(a: f32, b: f32) -> f32 {
    num32::demote(min_num_f64(num32::promote(a), num32::promote(b)))
}

/// `f32.max_num`: the greater input, favoring numbers like [`min_num_f32`]. `+0.0` is greater
/// than `-0.0`.
#[inline]
#[must_use]
pub fn max_num_f32(a: f32, b: f32) -> f32 {
    num32::demote(max_num_f64(num32::promote(a), num32::promote(b)))
}

/// `f32.clamp`: `x` limited to `lo..=hi`, or `None` when the bounds are invalid.
///
/// The bounds are invalid unless `lo <= hi`, which also rejects a NaN bound; the VM traps with
/// [`Trap::ClampBoundsInvalid`](crate::vm::Trap::ClampBoundsInvalid) exactly where this returns
/// `None`. Otherwise the result is `lo` if `x < lo`, `hi` if `x > hi`, and `x` itself (with its
/// exact bits) otherwise, which is bit-identical to Rust's [`f32::clamp`]: a NaN `x` is returned
/// unchanged, and `clamp(-0.0, 0.0, 1.0)` is `-0.0`. Where this returns `None`, [`f32::clamp`]
/// panics.
#[inline]
#[must_use]
pub fn clamp_f32(x: f32, lo: f32, hi: f32) -> Option<f32> {
    // Written as a negated `<=` so that NaN bounds fail the check.
    #[expect(
        clippy::neg_cmp_op_on_partial_ord,
        reason = "a NaN bound must be rejected, which `lo > hi` would not do"
    )]
    if !(lo <= hi) {
        return None;
    }
    Some(if x < lo {
        lo
    } else if x > hi {
        hi
    } else {
        x
    })
}

// --- f32: transcendental functions (pinned libm) --------------------------------------------

/// `f32.pow`: `base` raised to `exponent`, as computed by `libm::powf` 0.2.16.
///
/// Special cases follow C99 Annex F (`pow(x, 0.0) == 1.0` even for NaN `x`,
/// `pow(1.0, y) == 1.0` even for NaN `y`, `pow(-8.0, 1.0 / 3.0)` is NaN). A NaN result's bits are
/// not specified.
#[inline]
#[must_use]
pub fn pow_f32(base: f32, exponent: f32) -> f32 {
    libm::powf(base, exponent)
}

/// `f32.exp`: e raised to `x`, as computed by `libm::expf` 0.2.16. A NaN result's bits are not
/// specified.
#[inline]
#[must_use]
pub fn exp_f32(x: f32) -> f32 {
    libm::expf(x)
}

/// `f32.ln`: the natural logarithm of `x`, as computed by `libm::logf` 0.2.16.
///
/// `ln(±0.0) == -inf`; a negative `x` gives NaN, whose bits are not specified.
#[inline]
#[must_use]
pub fn ln_f32(x: f32) -> f32 {
    libm::logf(x)
}

/// `f32.sin`: the sine of `x` radians, as computed by `libm::sinf` 0.2.16.
///
/// Arguments of any magnitude are reduced exactly; `sin(±inf)` is NaN, whose bits are not
/// specified.
#[inline]
#[must_use]
pub fn sin_f32(x: f32) -> f32 {
    libm::sinf(x)
}

/// `f32.cos`: the cosine of `x` radians, as computed by `libm::cosf` 0.2.16.
///
/// Arguments of any magnitude are reduced exactly; `cos(±inf)` is NaN, whose bits are not
/// specified.
#[inline]
#[must_use]
pub fn cos_f32(x: f32) -> f32 {
    libm::cosf(x)
}

/// `f32.atan2`: the angle of the point `(x, y)` in radians, in `-pi..=pi`, as computed by
/// `libm::atan2f` 0.2.16. Note the argument order: `y` first. A NaN result's bits are not
/// specified.
#[inline]
#[must_use]
pub fn atan2_f32(y: f32, x: f32) -> f32 {
    libm::atan2f(y, x)
}

// --- f64: exact operations ------------------------------------------------------------------

/// `f64_abs`: `x` with its sign bit cleared. NaN payloads pass through unchanged.
#[inline]
#[must_use]
pub fn abs_f64(x: f64) -> f64 {
    x.abs()
}

/// `f64_copysign`: the bits of `magnitude` with the sign bit of `sign`. No other bits are
/// inspected or changed, so NaN payloads pass through unchanged.
#[inline]
#[must_use]
pub fn copysign_f64(magnitude: f64, sign: f64) -> f64 {
    float::copysign(magnitude, sign)
}

/// `f64_floor`: rounds toward negative infinity.
///
/// Exact. A zero result keeps the input's sign; infinities pass through; a NaN input returns that
/// NaN with its quiet bit set.
#[inline]
#[must_use]
pub fn floor_f64(x: f64) -> f64 {
    float::floor(x)
}

/// `f64_ceil`: rounds toward positive infinity (`ceil(-0.5) == -0.0`).
///
/// Exact; signed zeros, infinities and NaNs as for [`floor_f64`].
#[inline]
#[must_use]
pub fn ceil_f64(x: f64) -> f64 {
    float::ceil(x)
}

/// `f64_trunc`: rounds toward zero.
///
/// Exact; signed zeros, infinities and NaNs as for [`floor_f64`].
#[inline]
#[must_use]
pub fn trunc_f64(x: f64) -> f64 {
    float::trunc(x)
}

/// `f64_nearest`: rounds to the nearest integer, **ties to even** (`2.5 -> 2.0`,
/// `-0.5 -> -0.0`).
///
/// This is IEEE 754 `roundToIntegralTiesToEven`, Rust's `f64::round_ties_even` and `libm`'s
/// `roundeven`. It is *not* C's `round`; for that, see [`round_half_away_f64`]. Exact; signed
/// zeros, infinities and NaNs as for [`floor_f64`].
#[inline]
#[must_use]
pub fn round_half_even_f64(x: f64) -> f64 {
    float::nearest(x)
}

/// `f64.round_half_away`: rounds to the nearest integer, **ties away from zero** (`2.5 -> 3.0`,
/// `-0.5 -> -1.0`).
///
/// This is IEEE 754 `roundToIntegralTiesToAway`, C's `round` and Rust's `f64::round`. It
/// differs from [`round_half_even_f64`] only on exact ties. Exact: `0.49999999999999994` rounds
/// to `0.0`, and every value of magnitude at least 2^52 is already an integer and is returned
/// unchanged. Signed zeros, infinities and NaNs as for [`floor_f64`].
#[inline]
#[must_use]
pub fn round_half_away_f64(x: f64) -> f64 {
    float::round_half_away(x)
}

/// `f64_sqrt`: the correctly rounded square root (round to nearest, ties to even).
///
/// `sqrt(-0.0) == -0.0` and `sqrt(+inf) == +inf`; a NaN input returns that NaN with its quiet bit
/// set; any other negative input returns the canonical quiet NaN.
#[inline]
#[must_use]
pub fn sqrt_f64(x: f64) -> f64 {
    float::sqrt(x)
}

/// `f64_rem`: the remainder of division with the quotient truncated toward zero, carrying the
/// sign of `a` (Rust's `%`, C's `fmod`). Exact; special cases and NaNs as for [`rem_f32`].
#[inline]
#[must_use]
pub fn rem_f64(a: f64, b: f64) -> f64 {
    float::rem(a, b)
}

/// `f64_min`: the lesser input, propagating NaN: if either input is NaN, the result is the
/// canonical quiet NaN. `-0.0` is less than `+0.0`.
#[inline]
#[must_use]
pub fn min_f64(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else {
        ordered_min(a, b)
    }
}

/// `f64_max`: the greater input, propagating NaN like [`min_f64`]. `+0.0` is greater than
/// `-0.0`.
#[inline]
#[must_use]
pub fn max_f64(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else {
        ordered_max(a, b)
    }
}

/// `f64_min_num`: the lesser input, favoring numbers: if exactly one input is NaN, the other is
/// returned; if both are, the result is the canonical quiet NaN. `-0.0` is less than `+0.0`.
#[inline]
#[must_use]
pub fn min_num_f64(a: f64, b: f64) -> f64 {
    match (a.is_nan(), b.is_nan()) {
        (true, true) => f64::NAN,
        (true, false) => b,
        (false, true) => a,
        (false, false) => ordered_min(a, b),
    }
}

/// `f64_max_num`: the greater input, favoring numbers like [`min_num_f64`]. `+0.0` is greater
/// than `-0.0`.
#[inline]
#[must_use]
pub fn max_num_f64(a: f64, b: f64) -> f64 {
    match (a.is_nan(), b.is_nan()) {
        (true, true) => f64::NAN,
        (true, false) => b,
        (false, true) => a,
        (false, false) => ordered_max(a, b),
    }
}

/// `f64.clamp`: `x` limited to `lo..=hi`, or `None` when the bounds are invalid.
///
/// Exactly as [`clamp_f32`], in `f64`: `None` unless `lo <= hi` (the VM traps with
/// [`Trap::ClampBoundsInvalid`](crate::vm::Trap::ClampBoundsInvalid) there, where
/// [`f64::clamp`] panics), and otherwise bit-identical to [`f64::clamp`].
#[inline]
#[must_use]
pub fn clamp_f64(x: f64, lo: f64, hi: f64) -> Option<f64> {
    // Written as a negated `<=` so that NaN bounds fail the check.
    #[expect(
        clippy::neg_cmp_op_on_partial_ord,
        reason = "a NaN bound must be rejected, which `lo > hi` would not do"
    )]
    if !(lo <= hi) {
        return None;
    }
    Some(if x < lo {
        lo
    } else if x > hi {
        hi
    } else {
        x
    })
}

// --- f64: transcendental functions (pinned libm) --------------------------------------------

/// `f64.pow`: `base` raised to `exponent`, as computed by `libm::pow` 0.2.16.
///
/// Special cases follow C99 Annex F like [`pow_f32`]. A NaN result's bits are not specified.
#[inline]
#[must_use]
pub fn pow_f64(base: f64, exponent: f64) -> f64 {
    libm::pow(base, exponent)
}

/// `f64.exp`: e raised to `x`, as computed by `libm::exp` 0.2.16.
///
/// Not correctly rounded: `exp_f64(1.0)` is one ulp above [`core::f64::consts::E`]. A NaN
/// result's bits are not specified.
#[inline]
#[must_use]
pub fn exp_f64(x: f64) -> f64 {
    libm::exp(x)
}

/// `f64.ln`: the natural logarithm of `x`, as computed by `libm::log` 0.2.16.
///
/// `ln(±0.0) == -inf`; a negative `x` gives NaN, whose bits are not specified.
#[inline]
#[must_use]
pub fn ln_f64(x: f64) -> f64 {
    libm::log(x)
}

/// `f64.sin`: the sine of `x` radians, as computed by `libm::sin` 0.2.16.
///
/// Arguments of any magnitude are reduced exactly; `sin(±inf)` is NaN, whose bits are not
/// specified.
#[inline]
#[must_use]
pub fn sin_f64(x: f64) -> f64 {
    libm::sin(x)
}

/// `f64.cos`: the cosine of `x` radians, as computed by `libm::cos` 0.2.16.
///
/// Arguments of any magnitude are reduced exactly; `cos(±inf)` is NaN, whose bits are not
/// specified.
#[inline]
#[must_use]
pub fn cos_f64(x: f64) -> f64 {
    libm::cos(x)
}

/// `f64.atan2`: the angle of the point `(x, y)` in radians, in `-pi..=pi`, as computed by
/// `libm::atan2` 0.2.16. Note the argument order: `y` first. A NaN result's bits are not
/// specified.
#[inline]
#[must_use]
pub fn atan2_f64(y: f64, x: f64) -> f64 {
    libm::atan2(y, x)
}

/// The lesser of two non-NaN values, ordering `-0.0` below `+0.0`.
#[inline]
fn ordered_min(a: f64, b: f64) -> f64 {
    if a < b {
        a
    } else if a > b {
        b
    } else if a == 0.0 {
        if a.is_sign_negative() || b.is_sign_negative() {
            -0.0
        } else {
            0.0
        }
    } else {
        a
    }
}

/// The greater of two non-NaN values, ordering `+0.0` above `-0.0`.
#[inline]
fn ordered_max(a: f64, b: f64) -> f64 {
    if a > b {
        a
    } else if a < b {
        b
    } else if a == 0.0 {
        if a.is_sign_positive() || b.is_sign_positive() {
            0.0
        } else {
            -0.0
        }
    } else {
        a
    }
}

#[cfg(test)]
mod tests {
    //! Checks every `libm` counterpart the module documentation claims, bit for bit on non-NaN
    //! results and NaN-ness otherwise.

    extern crate std;

    use super::*;
    use std::vec::Vec;

    type Unary32 = fn(f32) -> f32;
    type Unary64 = fn(f64) -> f64;
    type Binary32 = fn(f32, f32) -> f32;
    type Binary64 = fn(f64, f64) -> f64;

    fn same32(a: f32, b: f32) -> bool {
        (a.is_nan() && b.is_nan()) || a.to_bits() == b.to_bits()
    }

    fn same64(a: f64, b: f64) -> bool {
        (a.is_nan() && b.is_nan()) || a.to_bits() == b.to_bits()
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

    /// Special values, their neighbours, and an even stride over every `f32` bit pattern.
    fn f32_inputs() -> Vec<f32> {
        let base = [
            0.0,
            0.5,
            0.499_999_97,
            1.0,
            1.5,
            2.5,
            8_388_607.5,
            8_388_608.0,
            8_388_609.0,
            16_777_217.0,
            f32::MIN_POSITIVE,
            f32::from_bits(1),
            f32::MAX,
            f32::INFINITY,
            f32::NAN,
            f32::from_bits(0x7FA0_0001),
        ];
        let mut xs = Vec::new();
        for x in base {
            for x in [x, -x] {
                xs.push(x);
                if x.is_finite() {
                    xs.push(f32::from_bits(x.to_bits() + 1));
                    if x != 0.0 {
                        xs.push(f32::from_bits(x.to_bits() - 1));
                    }
                }
            }
        }
        for i in -64_i16..=64 {
            xs.push(f32::from(i) * 0.25);
        }
        xs.extend((0..=u32::MAX).step_by(0x4001).map(f32::from_bits));
        xs
    }

    fn f64_inputs() -> Vec<f64> {
        let base = [
            0.0,
            0.5,
            0.499_999_999_999_999_94,
            1.0,
            2.5,
            4_503_599_627_370_495.5,
            4_503_599_627_370_496.0,
            4_503_599_627_370_497.0,
            f64::MIN_POSITIVE,
            f64::from_bits(1),
            f64::MAX,
            f64::INFINITY,
            f64::NAN,
        ];
        let mut xs = Vec::new();
        for x in base {
            for x in [x, -x] {
                xs.push(x);
                if x.is_finite() {
                    xs.push(f64::from_bits(x.to_bits() + 1));
                    if x != 0.0 {
                        xs.push(f64::from_bits(x.to_bits() - 1));
                    }
                }
            }
        }
        let mut rng = SplitMix64(0x6D61_7468);
        for i in 0..100_000 {
            let r = rng.next();
            xs.push(match i % 3 {
                0 => f64::from_bits(r),
                // Magnitudes in [2^-8, 2^60), where rounding is most interesting.
                1 => f64::from_bits((r & 0x800F_FFFF_FFFF_FFFF) | ((1015 + (r >> 58) % 68) << 52)),
                // Near half-integers.
                _ => (r >> 40) as f64 * 0.5,
            });
        }
        xs
    }

    #[test]
    fn unary_f32_operations_match_their_libm_counterparts() {
        let ops: [(&str, Unary32, Unary32); 8] = [
            ("abs", abs_f32, libm::fabsf),
            ("floor", floor_f32, libm::floorf),
            ("ceil", ceil_f32, libm::ceilf),
            ("trunc", trunc_f32, libm::truncf),
            ("round_half_even", round_half_even_f32, libm::roundevenf),
            ("round_half_even", round_half_even_f32, libm::rintf),
            ("round_half_away", round_half_away_f32, libm::roundf),
            ("sqrt", sqrt_f32, libm::sqrtf),
        ];
        for x in f32_inputs() {
            for (name, ours, theirs) in ops {
                let (a, b) = (ours(x), theirs(x));
                assert!(
                    same32(a, b),
                    "{name}({x:e} = {:#x}): {a:e} vs {b:e}",
                    x.to_bits()
                );
            }
        }
    }

    #[test]
    fn unary_f64_operations_match_their_libm_counterparts() {
        let ops: [(&str, Unary64, Unary64); 8] = [
            ("abs", abs_f64, libm::fabs),
            ("floor", floor_f64, libm::floor),
            ("ceil", ceil_f64, libm::ceil),
            ("trunc", trunc_f64, libm::trunc),
            ("round_half_even", round_half_even_f64, libm::roundeven),
            ("round_half_even", round_half_even_f64, libm::rint),
            ("round_half_away", round_half_away_f64, libm::round),
            ("sqrt", sqrt_f64, libm::sqrt),
        ];
        for x in f64_inputs() {
            for (name, ours, theirs) in ops {
                let (a, b) = (ours(x), theirs(x));
                assert!(
                    same64(a, b),
                    "{name}({x:e} = {:#x}): {a:e} vs {b:e}",
                    x.to_bits()
                );
            }
        }
    }

    #[test]
    fn binary_operations_match_their_libm_counterparts() {
        let ops32: [(&str, Binary32, Binary32); 6] = [
            ("copysign", copysign_f32, libm::copysignf),
            ("rem", rem_f32, libm::fmodf),
            ("min", min_f32, libm::fminimumf),
            ("max", max_f32, libm::fmaximumf),
            ("min_num", min_num_f32, libm::fminimum_numf),
            ("max_num", max_num_f32, libm::fmaximum_numf),
        ];
        let ops64: [(&str, Binary64, Binary64); 6] = [
            ("copysign", copysign_f64, libm::copysign),
            ("rem", rem_f64, libm::fmod),
            ("min", min_f64, libm::fminimum),
            ("max", max_f64, libm::fmaximum),
            ("min_num", min_num_f64, libm::fminimum_num),
            ("max_num", max_num_f64, libm::fmaximum_num),
        ];
        let xs32: Vec<f32> = f32_inputs().into_iter().step_by(97).collect();
        for &a in &xs32 {
            for &b in &xs32 {
                for (name, ours, theirs) in ops32 {
                    let (x, y) = (ours(a, b), theirs(a, b));
                    assert!(same32(x, y), "{name}({a:e}, {b:e}): {x:e} vs {y:e}");
                }
            }
        }
        let xs64: Vec<f64> = f64_inputs().into_iter().step_by(331).collect();
        for &a in &xs64 {
            for &b in &xs64 {
                for (name, ours, theirs) in ops64 {
                    let (x, y) = (ours(a, b), theirs(a, b));
                    assert!(same64(x, y), "{name}({a:e}, {b:e}): {x:e} vs {y:e}");
                }
            }
        }
    }

    /// `round_half_away` against a direct statement of its definition and against `std`.
    #[test]
    fn round_half_away_is_exact() {
        for x in f32_inputs() {
            let want = if !x.is_finite() || x.abs() >= 8_388_608.0 {
                x
            } else {
                let t = trunc_f32(x);
                // `x - t` is exact below 2^23.
                let r = if (x - t).abs() >= 0.5 {
                    t + copysign_f32(1.0, x)
                } else {
                    t
                };
                copysign_f32(r, x)
            };
            let got = round_half_away_f32(x);
            assert!(same32(got, want), "{x:e}: {got:e} vs {want:e}");
            assert!(same32(got, x.round()), "{x:e}: {got:e} vs std");
        }
        // Where `trunc(x + copysign(0.5, x))` goes wrong.
        assert_eq!(round_half_away_f32(0.499_999_97), 0.0);
        assert_eq!(round_half_away_f32(8_388_609.0), 8_388_609.0);
        assert_eq!(round_half_away_f64(0.499_999_999_999_999_94), 0.0);
        assert_eq!(
            round_half_away_f64(4_503_599_627_370_497.0),
            4_503_599_627_370_497.0
        );
    }

    #[test]
    fn clamp_agrees_with_core_and_rejects_unordered_bounds() {
        let xs: Vec<f32> = f32_inputs().into_iter().step_by(1021).collect();
        for &lo in &xs {
            for &hi in &xs {
                for &x in &xs {
                    let got = clamp_f32(x, lo, hi);
                    if lo <= hi {
                        let want = x.clamp(lo, hi);
                        assert_eq!(
                            got.map(f32::to_bits),
                            Some(want.to_bits()),
                            "clamp({x:e}, {lo:e}, {hi:e})"
                        );
                    } else {
                        assert_eq!(got, None, "clamp({x:e}, {lo:e}, {hi:e})");
                    }
                }
            }
        }
        assert_eq!(
            clamp_f32(-0.0, 0.0, 1.0).map(f32::to_bits),
            Some(0x8000_0000)
        );
        assert_eq!(clamp_f32(0.5, f32::NAN, 1.0), None);
        assert_eq!(clamp_f32(0.5, 1.0, 0.0), None);
        let nan = f32::from_bits(0x7FA0_0001);
        assert_eq!(
            clamp_f32(nan, 0.0, 1.0).map(f32::to_bits),
            Some(0x7FA0_0001)
        );

        let xs: Vec<f64> = f64_inputs().into_iter().step_by(4099).collect();
        for &lo in &xs {
            for &hi in &xs {
                for &x in &xs {
                    let got = clamp_f64(x, lo, hi);
                    if lo <= hi {
                        assert_eq!(
                            got.map(f64::to_bits),
                            Some(x.clamp(lo, hi).to_bits()),
                            "clamp({x:e}, {lo:e}, {hi:e})"
                        );
                    } else {
                        assert_eq!(got, None, "clamp({x:e}, {lo:e}, {hi:e})");
                    }
                }
            }
        }
        assert_eq!(
            clamp_f64(-0.0, 0.0, 1.0).map(f64::to_bits),
            Some(0x8000_0000_0000_0000)
        );
        assert_eq!(clamp_f64(0.5, 0.0, f64::NAN), None);
    }

    /// The transcendental functions are their `libm` counterparts by construction; this pins
    /// that each calls the one its documentation names, in the right argument order.
    #[test]
    fn transcendental_functions_call_their_defining_libm_routines() {
        let xs: Vec<f64> = f64_inputs().into_iter().step_by(211).collect();
        let unary: [(&str, Unary64, Unary64); 4] = [
            ("exp", exp_f64, libm::exp),
            ("ln", ln_f64, libm::log),
            ("sin", sin_f64, libm::sin),
            ("cos", cos_f64, libm::cos),
        ];
        let binary: [(&str, Binary64, Binary64); 2] = [
            ("pow", pow_f64, libm::pow),
            ("atan2", atan2_f64, libm::atan2),
        ];
        for &a in &xs {
            for (name, ours, theirs) in unary {
                assert!(same64(ours(a), theirs(a)), "{name}({a:e})");
            }
            for &b in xs.iter().step_by(7) {
                for (name, ours, theirs) in binary {
                    assert!(same64(ours(a, b), theirs(a, b)), "{name}({a:e}, {b:e})");
                }
            }
        }
        assert!(!same64(atan2_f64(1.0, 0.0), libm::atan2(0.0, 1.0)));
    }
}
