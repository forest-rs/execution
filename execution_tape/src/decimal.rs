// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Exact decimal arithmetic behind the `dec.*` opcodes.
//!
//! A [`Decimal`] is `mantissa × 10^(-scale)` with an `i128` mantissa. Every operation here is
//! either exact or rounded exactly once from the exact result:
//! - [`add`]/[`sub`] align both operands to the larger scale by multiplying the smaller-scale
//!   mantissa by a power of ten, which never discards digits;
//! - [`cmp`] orders numeric values, so `1.0` and `1.00` compare equal;
//! - [`rescale`] and [`Division`] express the result as an integer quotient `n / d` of non-negative
//!   magnitudes, divide with remainder, and let the [`DecimalRounding`] mode decide from the sign,
//!   the remainder and the parity of the truncated quotient whether to add one unit in the last
//!   place.
//!
//! Intermediate magnitudes use a 256-bit unsigned integer ([`U256`]), wide enough that no
//! intermediate step overflows or rounds. The only failure besides division by zero is a result
//! mantissa outside `i128`, so each function fails exactly when the correctly rounded result is
//! not representable.

use core::cmp::Ordering;

use crate::value::{Decimal, DecimalRounding};

/// Why a decimal operation produced no value.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum DecimalError {
    /// The result mantissa (or the requested scale) is outside the representable range.
    Overflow,
    /// The divisor is zero.
    DivisionByZero,
}

/// `10^k` for `k` in `0..=38`; `10^39` exceeds `u128::MAX`.
const POW10: [u128; 39] = {
    let mut table = [1_u128; 39];
    let mut k = 1;
    while k < table.len() {
        table[k] = table[k - 1] * 10;
        k += 1;
    }
    table
};

/// Largest power of ten that fits in a `u128` (`10^38`).
const MAX_POW10_EXP: usize = POW10.len() - 1;

/// Returns `a + b` at scale `max(a.scale, b.scale)`, or `None` if the mantissa overflows.
pub(crate) fn add(a: Decimal, b: Decimal) -> Option<Decimal> {
    if a.scale() == b.scale() {
        let mantissa = a.mantissa().checked_add(b.mantissa())?;
        return Some(Decimal::new(mantissa, a.scale()));
    }
    add_aligned(a, b, false)
}

/// Returns `a - b` at scale `max(a.scale, b.scale)`, or `None` if the mantissa overflows.
pub(crate) fn sub(a: Decimal, b: Decimal) -> Option<Decimal> {
    if a.scale() == b.scale() {
        let mantissa = a.mantissa().checked_sub(b.mantissa())?;
        return Some(Decimal::new(mantissa, a.scale()));
    }
    add_aligned(a, b, true)
}

/// Adds `a` and `b` (or `-b`) exactly at the larger scale, in sign-magnitude form.
///
/// Only the operand with the smaller scale is multiplied. If its aligned magnitude does not fit
/// in a `u128`, it exceeds `2^128` (it is a non-zero multiple of ten, so it cannot equal `2^128`)
/// while the other magnitude is at most `2^127`, so the sum's magnitude exceeds `2^127` and
/// cannot be an `i128`: reporting overflow early is exact.
fn add_aligned(a: Decimal, b: Decimal, negate_b: bool) -> Option<Decimal> {
    let scale = a.scale().max(b.scale());
    let (a_negative, a_magnitude) = aligned(a, scale)?;
    let (b_negative, b_magnitude) = aligned(b, scale)?;
    let b_negative = b_negative != negate_b;
    let (negative, magnitude) = if a_negative == b_negative {
        (a_negative, a_magnitude.checked_add(b_magnitude)?)
    } else if a_magnitude >= b_magnitude {
        (a_negative, a_magnitude - b_magnitude)
    } else {
        (b_negative, b_magnitude - a_magnitude)
    };
    Some(Decimal::new(signed(negative, magnitude)?, scale))
}

/// Compares numeric values, ignoring scale (`1.0 == 1.00`, `-0.5 < 0`).
pub(crate) fn cmp(a: Decimal, b: Decimal) -> Ordering {
    let (ma, mb) = (a.mantissa(), b.mantissa());
    if a.scale() == b.scale() {
        return ma.cmp(&mb);
    }
    let by_sign = ma.signum().cmp(&mb.signum());
    if by_sign != Ordering::Equal || ma == 0 {
        return by_sign;
    }
    // Same non-zero sign: compare magnitudes at the larger scale. Only the smaller-scale side is
    // multiplied, and a magnitude beyond `u128` exceeds the other side's (at most `2^127`).
    let scale = a.scale().max(b.scale());
    let by_magnitude = match (aligned(a, scale), aligned(b, scale)) {
        (Some((_, x)), Some((_, y))) => x.cmp(&y),
        (None, _) => Ordering::Greater,
        (_, None) => Ordering::Less,
    };
    if ma < 0 {
        by_magnitude.reverse()
    } else {
        by_magnitude
    }
}

/// Returns `a` at `scale`: exact when `scale >= a.scale`, otherwise rounded by `rounding`.
///
/// Returns `None` if the result mantissa overflows (only possible when increasing the scale).
pub(crate) fn rescale(a: Decimal, scale: u8, rounding: DecimalRounding) -> Option<Decimal> {
    let mantissa = if scale >= a.scale() {
        let (negative, magnitude) = aligned(a, scale)?;
        signed(negative, magnitude)?
    } else {
        let m = a.mantissa();
        let divisor = U256::ONE
            .checked_mul_pow10(usize::from(a.scale() - scale))
            .unwrap_or(U256::MAX);
        round_quotient(m < 0, U256::from_u128(m.unsigned_abs()), divisor, rounding)?
    };
    Some(Decimal::new(mantissa, scale))
}

/// Returns `a / b` at `scale`, rounded once from the exact quotient by `rounding`.
#[cfg(test)]
pub(crate) fn div(
    a: Decimal,
    b: Decimal,
    scale: u8,
    rounding: DecimalRounding,
) -> Result<Decimal, DecimalError> {
    Division::new(a, b, scale)?.round(rounding)
}

/// The exact quotient `a / b` at a result scale, prepared but not yet computed.
///
/// Preparation aligns the operands so that the caller can charge [`Self::steps`] before the
/// division runs.
pub(crate) struct Division {
    negative: bool,
    n: U256,
    d: U256,
    scale: u8,
}

impl Division {
    /// Aligns `a / b` to `scale`, rejecting a zero divisor or a numerator beyond 256 bits.
    pub(crate) fn new(a: Decimal, b: Decimal, scale: u8) -> Result<Self, DecimalError> {
        let (ma, mb) = (a.mantissa(), b.mantissa());
        if mb == 0 {
            return Err(DecimalError::DivisionByZero);
        }
        // (ma × 10^-sa) / (mb × 10^-sb) at `scale` has mantissa ma × 10^e / mb.
        let e = i32::from(scale) + i32::from(b.scale()) - i32::from(a.scale());
        let exponent = usize::try_from(e.unsigned_abs()).map_err(|_| DecimalError::Overflow)?;
        let mut n = U256::from_u128(ma.unsigned_abs());
        let mut d = U256::from_u128(mb.unsigned_abs());
        if e >= 0 {
            // A non-zero numerator beyond 256 bits divided by `d < 2^128` leaves a quotient of at
            // least `2^128`, so the result overflows.
            n = n
                .checked_mul_pow10(exponent)
                .ok_or(DecimalError::Overflow)?;
        } else {
            // `n <= 2^127`. A divisor beyond 256 bits makes the quotient 0 with remainder `n`, less
            // than half the divisor; saturating it at `U256::MAX` yields the same quotient,
            // remainder and comparison, so the rounded result is unchanged.
            d = d.checked_mul_pow10(exponent).unwrap_or(U256::MAX);
        }
        Ok(Self {
            negative: (ma < 0) != (mb < 0),
            n,
            d,
            scale,
        })
    }

    /// Bit steps of long division needed beyond native 128-bit division: zero when both
    /// magnitudes fit in 128 bits or the quotient is zero, otherwise the numerator's bit length
    /// (129 to 256).
    pub(crate) fn steps(&self) -> u32 {
        self.n.long_division_steps(self.d)
    }

    /// Computes the quotient and rounds it once by `rounding`.
    pub(crate) fn round(self, rounding: DecimalRounding) -> Result<Decimal, DecimalError> {
        let mantissa = round_quotient(self.negative, self.n, self.d, rounding)
            .ok_or(DecimalError::Overflow)?;
        Ok(Decimal::new(mantissa, self.scale))
    }
}

/// Returns `(sign, |m| × 10^(scale - d.scale))`, or `None` if the magnitude exceeds `u128`.
///
/// Requires `scale >= d.scale()`.
fn aligned(d: Decimal, scale: u8) -> Option<(bool, u128)> {
    let m = d.mantissa();
    let magnitude = mul_pow10(m.unsigned_abs(), usize::from(scale - d.scale()))?;
    Some((m < 0, magnitude))
}

/// Returns `magnitude × 10^exponent`, or `None` if it exceeds `u128`.
fn mul_pow10(magnitude: u128, exponent: usize) -> Option<u128> {
    if magnitude == 0 {
        return Some(0);
    }
    magnitude.checked_mul(*POW10.get(exponent)?)
}

/// Converts a sign and magnitude to an `i128`, or `None` if it is out of range.
///
/// A zero magnitude is `0` for either sign: decimals have no negative zero.
fn signed(negative: bool, magnitude: u128) -> Option<i128> {
    if negative {
        0_i128.checked_sub_unsigned(magnitude)
    } else {
        i128::try_from(magnitude).ok()
    }
}

/// Rounds the exact quotient `±(n / d)` to an integer, or `None` if it is outside `i128`.
///
/// `n` and `d` are magnitudes and `d` is non-zero. The truncated quotient `q` moves one unit away
/// from zero exactly when the mode asks for it given the remainder `r`: always for the directed
/// mode pointing away from zero, and for the half modes when `r > d - r`, or on a tie (`r == d - r`)
/// for [`DecimalRounding::HalfAwayFromZero`] or an odd `q` under [`DecimalRounding::HalfEven`].
fn round_quotient(negative: bool, n: U256, d: U256, rounding: DecimalRounding) -> Option<i128> {
    let (q, r) = n.div_rem(d);
    if q.hi != 0 {
        return None;
    }
    let q = q.lo;
    let away_from_zero = r != U256::ZERO
        && match rounding {
            DecimalRounding::TowardZero => false,
            DecimalRounding::Floor => negative,
            DecimalRounding::Ceiling => !negative,
            DecimalRounding::HalfEven | DecimalRounding::HalfAwayFromZero => {
                // Compare `r` with `d - r` rather than `2r` with `d`, which could overflow.
                match r.cmp(&d.wrapping_sub(r)) {
                    Ordering::Less => false,
                    Ordering::Greater => true,
                    Ordering::Equal => rounding == DecimalRounding::HalfAwayFromZero || q & 1 == 1,
                }
            }
        };
    let magnitude = if away_from_zero { q.checked_add(1)? } else { q };
    signed(negative, magnitude)
}

/// A 256-bit unsigned integer, just wide enough for exact decimal quotients.
///
/// Field order makes the derived ordering numeric.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct U256 {
    hi: u128,
    lo: u128,
}

impl U256 {
    const ZERO: Self = Self { hi: 0, lo: 0 };
    const ONE: Self = Self { hi: 0, lo: 1 };
    const MAX: Self = Self {
        hi: u128::MAX,
        lo: u128::MAX,
    };

    const fn from_u128(lo: u128) -> Self {
        Self { hi: 0, lo }
    }

    /// Returns `self × m`, or `None` on overflow.
    fn checked_mul_u128(self, m: u128) -> Option<Self> {
        let (lo_hi, lo) = widening_mul(self.lo, m);
        let (hi_hi, hi_lo) = widening_mul(self.hi, m);
        if hi_hi != 0 {
            return None;
        }
        Some(Self {
            hi: hi_lo.checked_add(lo_hi)?,
            lo,
        })
    }

    /// Returns `self × 10^exponent`, or `None` on overflow.
    fn checked_mul_pow10(self, mut exponent: usize) -> Option<Self> {
        if self == Self::ZERO {
            return Some(self);
        }
        let mut value = self;
        while exponent > 0 {
            let step = exponent.min(MAX_POW10_EXP);
            value = value.checked_mul_u128(POW10[step])?;
            exponent -= step;
        }
        Some(value)
    }

    fn wrapping_sub(self, rhs: Self) -> Self {
        let (lo, borrow) = self.lo.overflowing_sub(rhs.lo);
        Self {
            hi: self
                .hi
                .wrapping_sub(rhs.hi)
                .wrapping_sub(u128::from(borrow)),
            lo,
        }
    }

    /// Number of significant bits.
    fn bit_len(self) -> u32 {
        if self.hi != 0 {
            256 - self.hi.leading_zeros()
        } else {
            128 - self.lo.leading_zeros()
        }
    }

    fn bit(self, index: u32) -> bool {
        let word = if index >= 128 {
            self.hi >> (index - 128)
        } else {
            self.lo >> index
        };
        word & 1 == 1
    }

    fn set_bit(&mut self, index: u32) {
        if index >= 128 {
            self.hi |= 1 << (index - 128);
        } else {
            self.lo |= 1 << index;
        }
    }

    /// Returns `(self << 1) | bit`, dropping the top bit.
    fn shl1_with(self, bit: bool) -> Self {
        Self {
            hi: (self.hi << 1) | (self.lo >> 127),
            lo: (self.lo << 1) | u128::from(bit),
        }
    }

    /// Iterations [`Self::div_rem`] runs for `self / d`: none on the native and trivial paths.
    fn long_division_steps(self, d: Self) -> u32 {
        if (self.hi == 0 && d.hi == 0) || self < d {
            0
        } else {
            self.bit_len()
        }
    }

    /// Returns `(self / d, self % d)`. `d` must be non-zero.
    ///
    /// Runs [`Self::long_division_steps`] iterations.
    fn div_rem(self, d: Self) -> (Self, Self) {
        debug_assert!(d != Self::ZERO, "division by zero");
        if self.hi == 0 && d.hi == 0 {
            return (
                Self::from_u128(self.lo / d.lo),
                Self::from_u128(self.lo % d.lo),
            );
        }
        if self < d {
            return (Self::ZERO, self);
        }
        // Restoring binary long division. The remainder stays below `d`, but shifting it can
        // carry out of 256 bits when `d > 2^255`; the shifted value then certainly exceeds `d`,
        // and wrapping subtraction yields the true (smaller than `d`) difference.
        let mut q = Self::ZERO;
        let mut r = Self::ZERO;
        for index in (0..self.bit_len()).rev() {
            let carry = r.hi >> 127 == 1;
            r = r.shl1_with(self.bit(index));
            if carry || r >= d {
                r = r.wrapping_sub(d);
                q.set_bit(index);
            }
        }
        (q, r)
    }
}

/// Returns the 256-bit product `a × b` as `(high, low)` halves.
fn widening_mul(a: u128, b: u128) -> (u128, u128) {
    const LOW: u128 = u64::MAX as u128;
    let (a1, a0) = (a >> 64, a & LOW);
    let (b1, b0) = (b >> 64, b & LOW);
    let p00 = a0 * b0;
    let p01 = a0 * b1;
    let p10 = a1 * b0;
    let p11 = a1 * b1;
    // Each partial product is below `2^128`, and `mid` sums three values below `2^64`.
    let mid = (p00 >> 64) + (p01 & LOW) + (p10 & LOW);
    let lo = (mid << 64) | (p00 & LOW);
    let hi = p11 + (p01 >> 64) + (p10 >> 64) + (mid >> 64);
    (hi, lo)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::{DecimalError, Division, U256, add, cmp, div, rescale, sub, widening_mul};
    use crate::value::{Decimal, DecimalRounding};
    use core::cmp::Ordering;
    use std::sync::OnceLock;
    use std::vec;
    use std::vec::Vec;

    /// Reference arbitrary-precision magnitude: little-endian `u32` limbs, no high zero limbs.
    ///
    /// Deliberately simple and independent of [`U256`].
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Big(Vec<u32>);

    impl Big {
        fn from_halves(hi: u128, lo: u128) -> Self {
            let limbs = (0..8)
                .map(|i| {
                    let word = if i < 4 { lo } else { hi };
                    u32::try_from((word >> (32 * (i % 4))) & 0xFFFF_FFFF).unwrap()
                })
                .collect();
            Self(limbs).trimmed()
        }

        fn from_u128(v: u128) -> Self {
            Self::from_halves(0, v)
        }

        fn from_u256(v: U256) -> Self {
            Self::from_halves(v.hi, v.lo)
        }

        fn trimmed(mut self) -> Self {
            while self.0.last() == Some(&0) {
                self.0.pop();
            }
            self
        }

        fn is_zero(&self) -> bool {
            self.0.is_empty()
        }

        fn is_even(&self) -> bool {
            self.0.first().is_none_or(|limb| limb & 1 == 0)
        }

        fn to_u128(&self) -> Option<u128> {
            if self.0.len() > 4 {
                return None;
            }
            Some(
                self.0
                    .iter()
                    .rev()
                    .fold(0_u128, |acc, &limb| (acc << 32) | u128::from(limb)),
            )
        }

        fn mul(&self, other: &Self) -> Self {
            let mut out = vec![0_u64; self.0.len() + other.0.len() + 1];
            for (i, &x) in self.0.iter().enumerate() {
                let mut carry = 0_u64;
                for (j, &y) in other.0.iter().enumerate() {
                    let v = out[i + j] + u64::from(x) * u64::from(y) + carry;
                    out[i + j] = v & 0xFFFF_FFFF;
                    carry = v >> 32;
                }
                out[i + other.0.len()] += carry;
            }
            Self(out.into_iter().map(|v| u32::try_from(v).unwrap()).collect()).trimmed()
        }

        fn add(&self, other: &Self) -> Self {
            let len = self.0.len().max(other.0.len());
            let mut out = Vec::with_capacity(len + 1);
            let mut carry = 0_u64;
            for i in 0..len {
                let v = u64::from(*self.0.get(i).unwrap_or(&0))
                    + u64::from(*other.0.get(i).unwrap_or(&0))
                    + carry;
                out.push(u32::try_from(v & 0xFFFF_FFFF).unwrap());
                carry = v >> 32;
            }
            out.push(u32::try_from(carry).unwrap());
            Self(out).trimmed()
        }

        /// `self - other`, requiring `self >= other`.
        fn sub(&self, other: &Self) -> Self {
            let mut out = self.0.clone();
            sub_limbs(&mut out, &other.0);
            Self(out).trimmed()
        }

        fn cmp_mag(&self, other: &Self) -> Ordering {
            cmp_limbs(&self.0, &other.0)
        }

        fn bits(&self) -> usize {
            self.0.last().map_or(0, |&top| {
                32 * self.0.len() - usize::try_from(top.leading_zeros()).unwrap()
            })
        }

        /// Schoolbook binary long division.
        fn div_rem(&self, d: &Self) -> (Self, Self) {
            assert!(!d.is_zero(), "division by zero");
            let mut q = vec![0_u32; self.0.len()];
            // `r < d` before each shift, so `2r + 1` fits in one more limb than `d`.
            let mut r = vec![0_u32; d.0.len() + 1];
            for i in (0..self.bits()).rev() {
                let mut carry = (self.0[i / 32] >> (i % 32)) & 1;
                for limb in &mut r {
                    let next = *limb >> 31;
                    *limb = (*limb << 1) | carry;
                    carry = next;
                }
                if cmp_limbs(&r, &d.0) != Ordering::Less {
                    sub_limbs(&mut r, &d.0);
                    q[i / 32] |= 1 << (i % 32);
                }
            }
            (Self(q).trimmed(), Self(r).trimmed())
        }
    }

    /// Compares little-endian limb slices of any length.
    fn cmp_limbs(a: &[u32], b: &[u32]) -> Ordering {
        let len = a.len().max(b.len());
        (0..len)
            .rev()
            .map(|i| a.get(i).unwrap_or(&0).cmp(b.get(i).unwrap_or(&0)))
            .find(|o| *o != Ordering::Equal)
            .unwrap_or(Ordering::Equal)
    }

    /// `a -= b`, requiring `a >= b`.
    fn sub_limbs(a: &mut [u32], b: &[u32]) {
        assert!(cmp_limbs(a, b) != Ordering::Less, "negative difference");
        let mut borrow = false;
        for (i, limb) in a.iter_mut().enumerate() {
            let (v, b1) = limb.overflowing_sub(*b.get(i).unwrap_or(&0));
            let (v, b2) = v.overflowing_sub(u32::from(borrow));
            *limb = v;
            borrow = b1 || b2;
        }
    }

    /// `10^k` for every exponent a decimal operation can need.
    fn pow10(k: u32) -> &'static Big {
        static TABLE: OnceLock<Vec<Big>> = OnceLock::new();
        let table = TABLE.get_or_init(|| {
            let ten = Big::from_u128(10);
            let mut table = vec![Big::from_u128(1)];
            for _ in 0..=520 {
                let next = table.last().unwrap().mul(&ten);
                table.push(next);
            }
            table
        });
        &table[usize::try_from(k).unwrap()]
    }

    /// A signed reference value.
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Signed {
        negative: bool,
        magnitude: Big,
    }

    impl Signed {
        fn new(negative: bool, magnitude: Big) -> Self {
            Self {
                negative: negative && !magnitude.is_zero(),
                magnitude,
            }
        }

        /// `m × 10^k`.
        fn scaled(m: i128, k: u32) -> Self {
            Self::new(m < 0, Big::from_u128(m.unsigned_abs()).mul(pow10(k)))
        }

        fn add(&self, other: &Self) -> Self {
            if self.negative == other.negative {
                return Self::new(self.negative, self.magnitude.add(&other.magnitude));
            }
            if self.magnitude.cmp_mag(&other.magnitude) == Ordering::Less {
                Self::new(other.negative, other.magnitude.sub(&self.magnitude))
            } else {
                Self::new(self.negative, self.magnitude.sub(&other.magnitude))
            }
        }

        fn neg(&self) -> Self {
            Self::new(!self.negative, self.magnitude.clone())
        }

        fn cmp(&self, other: &Self) -> Ordering {
            match (self.negative, other.negative) {
                (false, true) => Ordering::Greater,
                (true, false) => Ordering::Less,
                (false, false) => self.magnitude.cmp_mag(&other.magnitude),
                (true, true) => other.magnitude.cmp_mag(&self.magnitude),
            }
        }

        fn to_i128(&self) -> Option<i128> {
            let magnitude = self.magnitude.to_u128()?;
            if self.negative {
                // -(2^127) is the only negative magnitude without a positive counterpart.
                let below_min = (1_u128 << 127).checked_sub(magnitude)?;
                Some(i128::MIN + i128::try_from(below_min).unwrap())
            } else {
                i128::try_from(magnitude).ok()
            }
        }
    }

    /// Reference division for every mode: `(ma × 10^(s + sb)) / (mb × 10^sa)`, rounded from the
    /// floor and ceiling of the exact signed quotient rather than from a truncated magnitude.
    fn reference_div(a: Decimal, b: Decimal, s: u8) -> [Option<i128>; 5] {
        assert!(b.mantissa() != 0, "reference divisor is non-zero");
        let mut num_exp = u32::from(s) + u32::from(b.scale());
        let mut den_exp = u32::from(a.scale());
        let common = num_exp.min(den_exp);
        num_exp -= common;
        den_exp -= common;
        let n = Big::from_u128(a.mantissa().unsigned_abs()).mul(pow10(num_exp));
        let d = Big::from_u128(b.mantissa().unsigned_abs()).mul(pow10(den_exp));
        // A quotient of at least 2^129 overflows in every mode.
        if n.bits() > d.bits() + 130 {
            return [None; 5];
        }
        let (q, r) = n.div_rem(&d);
        assert_eq!(q.mul(&d).add(&r), n, "reference division is exact");
        let negative = (a.mantissa() < 0) != (b.mantissa() < 0);
        let truncated = Signed::new(negative, q);
        let exact = r.is_zero();
        let one = Signed::new(false, Big::from_u128(1));
        // floor <= exact quotient <= ceiling, with equality when exact.
        let (floor, ceiling) = if exact {
            (truncated.clone(), truncated)
        } else if negative {
            (truncated.add(&one.neg()), truncated)
        } else {
            (truncated.clone(), truncated.add(&one))
        };
        // Twice the distance from `floor` up to the exact quotient, against one unit (`d`).
        let floor_distance = if negative && !exact { d.sub(&r) } else { r };
        let half = floor_distance.add(&floor_distance).cmp_mag(&d);
        DecimalRounding::ALL.map(|mode| {
            let rounded = match mode {
                _ if exact => &floor,
                DecimalRounding::Floor => &floor,
                DecimalRounding::Ceiling => &ceiling,
                DecimalRounding::TowardZero if negative => &ceiling,
                DecimalRounding::TowardZero => &floor,
                DecimalRounding::HalfEven | DecimalRounding::HalfAwayFromZero => match half {
                    Ordering::Less => &floor,
                    Ordering::Greater => &ceiling,
                    Ordering::Equal => match mode {
                        DecimalRounding::HalfEven if floor.magnitude.is_even() => &floor,
                        DecimalRounding::HalfEven => &ceiling,
                        _ if negative => &floor,
                        _ => &ceiling,
                    },
                },
            };
            rounded.to_i128()
        })
    }

    fn reference_add(a: Decimal, b: Decimal, negate_b: bool) -> Option<Decimal> {
        let scale = a.scale().max(b.scale());
        let x = Signed::scaled(a.mantissa(), u32::from(scale - a.scale()));
        let y = Signed::scaled(b.mantissa(), u32::from(scale - b.scale()));
        let y = if negate_b { y.neg() } else { y };
        x.add(&y).to_i128().map(|m| Decimal::new(m, scale))
    }

    /// Cross-multiplied comparison: `ma × 10^sb` against `mb × 10^sa`.
    fn reference_cmp(a: Decimal, b: Decimal) -> Ordering {
        let x = Signed::scaled(a.mantissa(), u32::from(b.scale()));
        x.cmp(&Signed::scaled(b.mantissa(), u32::from(a.scale())))
    }

    #[track_caller]
    #[test]
    fn long_division_steps_count_the_loop_iterations() {
        let big = U256 { hi: 1 << 72, lo: 5 };
        assert_eq!(U256::from_u128(u128::MAX).long_division_steps(U256::ONE), 0);
        assert_eq!(U256::from_u128(7).long_division_steps(big), 0);
        assert_eq!(big.long_division_steps(U256::from_u128(3)), 128 + 73);
        assert_eq!(big.long_division_steps(big), 128 + 73);
        assert_eq!(U256::MAX.long_division_steps(U256::ONE), 256);
        let ten = Decimal::new(10, 0);
        assert_eq!(Division::new(ten, ten, 37).unwrap().steps(), 0);
        assert_eq!(Division::new(ten, ten, 38).unwrap().steps(), 130);
    }

    fn check_div(a: Decimal, b: Decimal, s: u8) {
        for (mode, want) in DecimalRounding::ALL.into_iter().zip(reference_div(a, b, s)) {
            let want = want
                .map(|m| Decimal::new(m, s))
                .ok_or(DecimalError::Overflow);
            assert_eq!(div(a, b, s, mode), want, "{a:?} / {b:?} at {s} {mode:?}");
        }
    }

    #[track_caller]
    fn check_rescale(a: Decimal, s: u8) {
        let reference = reference_div(a, Decimal::new(1, 0), s);
        for (mode, want) in DecimalRounding::ALL.into_iter().zip(reference) {
            let want = want.map(|m| Decimal::new(m, s));
            assert_eq!(rescale(a, s, mode), want, "rescale {a:?} to {s} {mode:?}");
        }
    }

    #[track_caller]
    fn check_add_sub_cmp(a: Decimal, b: Decimal) {
        assert_eq!(add(a, b), reference_add(a, b, false), "{a:?} + {b:?}");
        assert_eq!(sub(a, b), reference_add(a, b, true), "{a:?} - {b:?}");
        assert_eq!(cmp(a, b), reference_cmp(a, b), "{a:?} cmp {b:?}");
    }

    /// `SplitMix64`: a tiny deterministic generator for reproducible inputs.
    struct SplitMix64(u64);

    impl SplitMix64 {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }

        fn u128(&mut self) -> u128 {
            (u128::from(self.next()) << 64) | u128::from(self.next())
        }

        /// A mantissa with a uniformly chosen bit length, so every magnitude is exercised.
        fn mantissa(&mut self) -> i128 {
            let bits = u32::try_from(self.below(128)).unwrap();
            let raw = self.u128();
            let magnitude = if bits == 0 {
                0
            } else {
                i128::try_from(raw >> (128 - bits)).unwrap()
            };
            if self.next() & 1 == 1 {
                // Reaches `i128::MIN` when the magnitude is `i128::MAX`.
                -magnitude - i128::from(magnitude == i128::MAX && self.next() & 1 == 1)
            } else {
                magnitude
            }
        }

        /// A scale biased toward the small values used in practice, with occasional extremes.
        fn scale(&mut self) -> u8 {
            let scale = match self.below(8) {
                0 => self.below(256),
                1 => 36 + self.below(6),
                _ => self.below(12),
            };
            u8::try_from(scale).unwrap()
        }
    }

    const P38: i128 = 10_i128.pow(38);

    fn boundary_mantissas() -> Vec<i128> {
        vec![
            i128::MIN,
            i128::MIN + 1,
            i128::MIN / 2,
            -P38,
            -10_i128.pow(19),
            -12_345,
            -25,
            -15,
            -6,
            -5,
            -1,
            0,
            1,
            4,
            5,
            10,
            15,
            25,
            12_345,
            10_i128.pow(19),
            P38 / 2,
            P38,
            i128::MAX / 10,
            i128::MAX - 1,
            i128::MAX,
        ]
    }

    const BOUNDARY_SCALES: [u8; 10] = [0, 1, 2, 19, 37, 38, 39, 77, 78, 255];

    #[test]
    fn widening_mul_matches_reference() {
        let mut rng = SplitMix64(1);
        let mut values = vec![0, 1, 2, u128::MAX, u128::MAX - 1, 1 << 64, (1 << 64) - 1];
        values.extend((0..200).map(|_| rng.u128()));
        for &a in &values {
            for &b in &values {
                let (hi, lo) = widening_mul(a, b);
                let want = Big::from_u128(a).mul(&Big::from_u128(b));
                assert_eq!(Big::from_halves(hi, lo), want, "{a} * {b}");
            }
        }
    }

    #[test]
    fn u256_checked_mul_matches_reference() {
        let mut rng = SplitMix64(6);
        let two256 = Big::from_u256(U256::MAX).add(&Big::from_u128(1));
        let mut values = vec![
            U256::ZERO,
            U256::ONE,
            U256::MAX,
            U256 {
                hi: 1,
                lo: u128::MAX,
            },
            U256 {
                hi: 0,
                lo: u128::MAX,
            },
        ];
        values.extend((0..100).map(|_| U256 {
            hi: rng.u128() >> rng.below(128),
            lo: rng.u128(),
        }));
        let mut factors = vec![0, 1, 10, u128::MAX, 1 << 127];
        factors.extend((0..100).map(|_| rng.u128() >> rng.below(128)));
        for &v in &values {
            for &m in &factors {
                let product = Big::from_u256(v).mul(&Big::from_u128(m));
                let want = (product.cmp_mag(&two256) == Ordering::Less).then_some(product);
                let got = v.checked_mul_u128(m).map(Big::from_u256);
                assert_eq!(got, want, "{v:?} * {m}");
            }
        }
    }

    #[test]
    fn u256_division_matches_reference() {
        let mut rng = SplitMix64(2);
        let random = |rng: &mut SplitMix64| {
            let (hi, lo) = (rng.u128(), rng.u128());
            let shift = 255 - u32::try_from(rng.below(256)).unwrap();
            match shift {
                0 => U256 { hi, lo },
                1..128 => U256 {
                    hi: hi >> shift,
                    lo: (lo >> shift) | (hi << (128 - shift)),
                },
                _ => U256 {
                    hi: 0,
                    lo: hi >> (shift - 128),
                },
            }
        };
        let mut cases = vec![
            (U256::MAX, U256::ONE),
            (U256::MAX, U256::MAX),
            (
                U256::MAX,
                U256 {
                    hi: 1 << 127,
                    lo: 0,
                },
            ),
            (
                U256::MAX,
                U256 {
                    hi: 1 << 127,
                    lo: 1,
                },
            ),
            (U256::MAX, U256::from_u128(3)),
            (U256 { hi: 1, lo: 0 }, U256::from_u128(2)),
            (U256::from_u128(7), U256 { hi: 1, lo: 0 }),
        ];
        for _ in 0..5_000 {
            cases.push((random(&mut rng), random(&mut rng)));
        }
        for (n, d) in cases {
            if d == U256::ZERO {
                continue;
            }
            let (q, r) = n.div_rem(d);
            let want = Big::from_u256(n).div_rem(&Big::from_u256(d));
            assert_eq!(
                (Big::from_u256(q), Big::from_u256(r)),
                want,
                "{n:?} / {d:?}"
            );
        }
    }

    #[test]
    fn checked_mul_pow10_detects_overflow_exactly() {
        // 10^77 < 2^256 < 10^78.
        let p77 = U256::ONE.checked_mul_pow10(77).unwrap();
        assert_eq!(Big::from_u256(p77), *pow10(77));
        assert_eq!(U256::ONE.checked_mul_pow10(78), None);
        assert_eq!(U256::ZERO.checked_mul_pow10(510), Some(U256::ZERO));
        // u128::MAX × 10^38 < 2^256 < u128::MAX × 10^39.
        let fits = U256::from_u128(u128::MAX).checked_mul_pow10(38).unwrap();
        assert_eq!(
            Big::from_u256(fits),
            Big::from_u128(u128::MAX).mul(pow10(38))
        );
        assert_eq!(U256::from_u128(u128::MAX).checked_mul_pow10(39), None);
    }

    /// The canonical rounding table (as in Java's `RoundingMode` documentation): each input
    /// rounded to an integer under each mode.
    #[test]
    fn rounding_table_matches_definition() {
        use DecimalRounding::{Ceiling, Floor, HalfAwayFromZero, HalfEven, TowardZero};
        let tenths = [55, 25, 16, 11, 10, -10, -11, -16, -25, -55];
        let table: [(DecimalRounding, [i128; 10]); 5] = [
            (HalfEven, [6, 2, 2, 1, 1, -1, -1, -2, -2, -6]),
            (HalfAwayFromZero, [6, 3, 2, 1, 1, -1, -1, -2, -3, -6]),
            (TowardZero, [5, 2, 1, 1, 1, -1, -1, -1, -2, -5]),
            (Floor, [5, 2, 1, 1, 1, -1, -2, -2, -3, -6]),
            (Ceiling, [6, 3, 2, 2, 1, -1, -1, -1, -2, -5]),
        ];
        for (mode, expected) in table {
            for (input, want) in tenths.into_iter().zip(expected) {
                let want = Decimal::new(want, 0);
                assert_eq!(
                    rescale(Decimal::new(input, 1), 0, mode),
                    Some(want),
                    "{input}/10 {mode:?}"
                );
                let ten = Decimal::new(10, 0);
                assert_eq!(
                    div(Decimal::new(input, 0), ten, 0, mode),
                    Ok(want),
                    "{input}/10 {mode:?}"
                );
            }
        }
    }

    #[test]
    fn small_operands_match_i128_reference_exhaustively() {
        // Small enough that `ma × 10^(s + sb)` and `mb × 10^sa` fit in `i128`; this reference is
        // a third formulation, using Euclidean division.
        for ma in -30_i128..=30 {
            for mb in -30_i128..=30 {
                for sa in 0..3_u8 {
                    for sb in 0..3_u8 {
                        let (a, b) = (Decimal::new(ma, sa), Decimal::new(mb, sb));
                        check_add_sub_cmp(a, b);
                        if mb == 0 {
                            continue;
                        }
                        for s in 0..4_u8 {
                            let num = ma * 10_i128.pow(u32::from(s + sb));
                            let den = mb * 10_i128.pow(u32::from(sa));
                            let (num, den) = if den < 0 { (-num, -den) } else { (num, den) };
                            let floor = num.div_euclid(den);
                            let rem = num.rem_euclid(den);
                            let ceiling = if rem == 0 { floor } else { floor + 1 };
                            for mode in DecimalRounding::ALL {
                                let want = match mode {
                                    _ if rem == 0 => floor,
                                    DecimalRounding::Floor => floor,
                                    DecimalRounding::Ceiling => ceiling,
                                    DecimalRounding::TowardZero if num < 0 => ceiling,
                                    DecimalRounding::TowardZero => floor,
                                    _ => match (2 * rem).cmp(&den) {
                                        Ordering::Less => floor,
                                        Ordering::Greater => ceiling,
                                        Ordering::Equal => match mode {
                                            DecimalRounding::HalfEven if floor % 2 == 0 => floor,
                                            DecimalRounding::HalfEven => ceiling,
                                            _ if num < 0 => floor,
                                            _ => ceiling,
                                        },
                                    },
                                };
                                assert_eq!(
                                    div(a, b, s, mode),
                                    Ok(Decimal::new(want, s)),
                                    "{a:?} / {b:?} at {s} {mode:?}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn division_matches_reference_at_boundaries() {
        let mantissas = boundary_mantissas();
        let scales = [0_u8, 1, 19, 38, 39, 77, 255];
        for &ma in &mantissas {
            for &mb in mantissas.iter().filter(|&&m| m != 0) {
                for sa in scales {
                    for sb in scales {
                        for s in scales {
                            check_div(Decimal::new(ma, sa), Decimal::new(mb, sb), s);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn division_by_zero_is_reported_for_every_scale_and_mode() {
        for ma in [i128::MIN, -1, 0, 1, i128::MAX] {
            for sb in [0, 1, 255] {
                for mode in DecimalRounding::ALL {
                    assert_eq!(
                        div(Decimal::new(ma, 3), Decimal::new(0, sb), 2, mode),
                        Err(DecimalError::DivisionByZero)
                    );
                }
            }
        }
    }

    /// Quotients just around the `i128` limits, where rounding one unit away from zero decides
    /// overflow.
    #[test]
    fn division_overflow_is_exact_at_the_i128_limits() {
        for limit in [i128::MAX.unsigned_abs(), i128::MIN.unsigned_abs()] {
            for d in [1_u32, 2, 3, 6, 7, 9, 11, 20, 99, 1_000_003] {
                for e in 0..4_u8 {
                    // ma ≈ limit × d / 10^e, so that ma × 10^e / d ≈ limit.
                    let scaled = Big::from_u128(limit).mul(&Big::from_u128(u128::from(d)));
                    let (base, _) = scaled.div_rem(pow10(u32::from(e)));
                    let Some(base) = base.to_u128() else {
                        continue;
                    };
                    for delta in -3..=3 {
                        let Some(magnitude) = base.checked_add_signed(delta) else {
                            continue;
                        };
                        let Ok(ma) = i128::try_from(magnitude) else {
                            continue;
                        };
                        let mb = i128::from(d);
                        for (ma, mb) in [(ma, mb), (-ma, mb), (ma, -mb), (-ma, -mb)] {
                            check_div(Decimal::new(ma, 0), Decimal::new(mb, 0), e);
                        }
                    }
                }
            }
        }
        // 2^127 fits only as a negative result.
        for mode in DecimalRounding::ALL {
            let min = Decimal::new(i128::MIN, 0);
            let one = Decimal::new(1, 0);
            let minus_one = Decimal::new(-1, 0);
            assert_eq!(div(min, one, 0, mode), Ok(min));
            assert_eq!(div(min, minus_one, 0, mode), Err(DecimalError::Overflow));
        }
    }

    #[test]
    fn division_matches_reference_on_random_inputs() {
        let mut rng = SplitMix64(3);
        for _ in 0..50_000 {
            let a = Decimal::new(rng.mantissa(), rng.scale());
            let b = match rng.mantissa() {
                0 => Decimal::new(1, rng.scale()),
                mb => Decimal::new(mb, rng.scale()),
            };
            check_div(a, b, rng.scale());
        }
    }

    #[test]
    fn rescale_matches_reference_at_boundaries() {
        for ma in boundary_mantissas() {
            for sa in BOUNDARY_SCALES {
                for s in BOUNDARY_SCALES {
                    check_rescale(Decimal::new(ma, sa), s);
                }
            }
        }
        // Exact ties, and their neighbors, at every digit position, for both signs.
        for k in 1..=38_u32 {
            let half = 5 * 10_i128.pow(k - 1);
            for kept in [0_i128, 1, 2, 7, 8] {
                let Some(m) = kept
                    .checked_mul(10_i128.pow(k))
                    .and_then(|v| v.checked_add(half))
                else {
                    continue;
                };
                for m in [m - 1, m, m + 1] {
                    for m in [m, -m] {
                        check_rescale(Decimal::new(m, u8::try_from(k).unwrap()), 0);
                    }
                }
            }
        }
    }

    #[test]
    fn rescale_increase_is_exact_or_overflows() {
        let mode = DecimalRounding::HalfEven;
        let p37 = Decimal::new(P38 / 10, 0);
        assert_eq!(rescale(p37, 1, mode), Some(Decimal::new(P38, 1)));
        assert_eq!(rescale(p37, 2, mode), None);
        assert_eq!(rescale(Decimal::new(i128::MAX, 0), 1, mode), None);
        assert_eq!(rescale(Decimal::new(i128::MIN, 0), 1, mode), None);
        let min = Decimal::new(i128::MIN, 3);
        assert_eq!(rescale(min, 3, mode), Some(min));
        assert_eq!(
            rescale(Decimal::new(0, 0), 255, mode),
            Some(Decimal::new(0, 255))
        );
        assert_eq!(
            rescale(Decimal::new(-1, 0), 38, mode),
            Some(Decimal::new(-P38, 38))
        );
        assert_eq!(rescale(Decimal::new(-1, 0), 39, mode), None);
    }

    #[test]
    fn rescale_matches_reference_on_random_inputs() {
        let mut rng = SplitMix64(4);
        for _ in 0..50_000 {
            check_rescale(Decimal::new(rng.mantissa(), rng.scale()), rng.scale());
        }
    }

    #[test]
    fn add_sub_cmp_match_reference_at_boundaries() {
        let mantissas = boundary_mantissas();
        for &ma in &mantissas {
            for &mb in &mantissas {
                for sa in BOUNDARY_SCALES {
                    for sb in BOUNDARY_SCALES {
                        check_add_sub_cmp(Decimal::new(ma, sa), Decimal::new(mb, sb));
                    }
                }
            }
        }
    }

    #[test]
    fn add_sub_cmp_match_reference_on_random_inputs() {
        let mut rng = SplitMix64(5);
        for _ in 0..100_000 {
            let a = Decimal::new(rng.mantissa(), rng.scale());
            let b = Decimal::new(rng.mantissa(), rng.scale());
            check_add_sub_cmp(a, b);
        }
    }

    /// Aligning one operand can exceed `i128` while the exact sum still fits, when the other
    /// operand has the opposite sign.
    #[test]
    fn aligned_sum_fits_even_when_an_aligned_operand_does_not() {
        // 10 × (MAX / 10 + 1) exceeds MAX, but subtracting 1.0 brings it back in range.
        let a = Decimal::new(i128::MAX / 10 + 1, 0);
        let want = Decimal::new((i128::MAX / 10) * 10, 1);
        assert_eq!(add(a, Decimal::new(-10, 1)), Some(want));
        assert_eq!(sub(a, Decimal::new(10, 1)), Some(want));
        check_add_sub_cmp(a, Decimal::new(-10, 1));
    }

    #[test]
    fn comparison_ignores_scale() {
        let c = |a: (i128, u8), b: (i128, u8)| cmp(Decimal::new(a.0, a.1), Decimal::new(b.0, b.1));
        assert_eq!(c((10, 1), (100, 2)), Ordering::Equal);
        assert_eq!(c((0, 0), (0, 255)), Ordering::Equal);
        assert_eq!(c((-5, 1), (0, 3)), Ordering::Less);
        // MAX × 10^-38 ≈ 1.7.
        assert_eq!(c((1, 0), (i128::MAX, 38)), Ordering::Less);
        assert_eq!(c((2, 0), (i128::MAX, 38)), Ordering::Greater);
        assert_eq!(c((-1, 0), (i128::MIN, 255)), Ordering::Less);
        assert_eq!(c((i128::MIN, 0), (-1, 255)), Ordering::Less);
    }
}
