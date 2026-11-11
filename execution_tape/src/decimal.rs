// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Exact decimal arithmetic behind the `dec.*` opcodes.
//!
//! A [`Decimal`] is `mantissa × 10^(-scale)` with an `i128` mantissa. [`add`] and [`sub`] align
//! both operands to the larger scale by multiplying the smaller-scale mantissa by a power of ten,
//! which never discards digits, and fail only when the exact result mantissa is outside `i128`.

use crate::value::Decimal;

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

#[cfg(test)]
mod tests {
    extern crate std;

    use super::{add, sub};
    use crate::value::Decimal;
    use core::cmp::Ordering;
    use std::sync::OnceLock;
    use std::vec;
    use std::vec::Vec;

    /// Reference arbitrary-precision magnitude: little-endian `u32` limbs, no high zero limbs.
    ///
    /// Deliberately simple, sharing no code with the implementation.
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

        fn trimmed(mut self) -> Self {
            while self.0.last() == Some(&0) {
                self.0.pop();
            }
            self
        }

        fn is_zero(&self) -> bool {
            self.0.is_empty()
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

    fn reference_add(a: Decimal, b: Decimal, negate_b: bool) -> Option<Decimal> {
        let scale = a.scale().max(b.scale());
        let x = Signed::scaled(a.mantissa(), u32::from(scale - a.scale()));
        let y = Signed::scaled(b.mantissa(), u32::from(scale - b.scale()));
        let y = if negate_b { y.neg() } else { y };
        x.add(&y).to_i128().map(|m| Decimal::new(m, scale))
    }

    #[track_caller]
    fn check_add_sub(a: Decimal, b: Decimal) {
        assert_eq!(add(a, b), reference_add(a, b, false), "{a:?} + {b:?}");
        assert_eq!(sub(a, b), reference_add(a, b, true), "{a:?} - {b:?}");
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
    fn add_sub_match_reference_at_boundaries() {
        let mantissas = boundary_mantissas();
        for &ma in &mantissas {
            for &mb in &mantissas {
                for sa in BOUNDARY_SCALES {
                    for sb in BOUNDARY_SCALES {
                        check_add_sub(Decimal::new(ma, sa), Decimal::new(mb, sb));
                    }
                }
            }
        }
    }

    #[test]
    fn add_sub_match_reference_on_random_inputs() {
        let mut rng = SplitMix64(5);
        for _ in 0..100_000 {
            let a = Decimal::new(rng.mantissa(), rng.scale());
            let b = Decimal::new(rng.mantissa(), rng.scale());
            check_add_sub(a, b);
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
        check_add_sub(a, Decimal::new(-10, 1));
    }
}
