// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Register iteration shared by verification and disassembly.

use crate::bytecode::{MAX_REGISTER_LISTS, MAX_SCALAR_OPERANDS};

#[derive(Copy, Clone, Debug)]
pub(crate) struct OperandIter<'a, const SCALARS: usize, const LISTS: usize> {
    prefix: [u32; SCALARS],
    prefix_len: u8,
    prefix_idx: u8,
    tails: [&'a [u32]; LISTS],
    tail_idx: u8,
    item_idx: usize,
}
impl<'a, const SCALARS: usize, const LISTS: usize> OperandIter<'a, SCALARS, LISTS> {
    #[inline]
    pub(crate) const fn from_parts(
        prefix: [u32; SCALARS],
        prefix_len: u8,
        tails: [&'a [u32]; LISTS],
    ) -> Self {
        Self {
            prefix,
            prefix_len,
            prefix_idx: 0,
            tails,
            tail_idx: 0,
            item_idx: 0,
        }
    }
}

/// An iterator over register operands without allocation.
#[derive(Copy, Clone, Debug)]
pub struct RegIter<'a> {
    inner: OperandIter<'a, MAX_SCALAR_OPERANDS, MAX_REGISTER_LISTS>,
}

impl<'a> RegIter<'a> {
    fn new(prefix: &[u32], tails: &[&'a [u32]]) -> Self {
        let mut scalars = [0; MAX_SCALAR_OPERANDS];
        scalars[..prefix.len()].copy_from_slice(prefix);
        let mut lists = [&[][..]; MAX_REGISTER_LISTS];
        lists[..tails.len()].copy_from_slice(tails);
        Self {
            inner: OperandIter::from_parts(
                scalars,
                u8::try_from(prefix.len()).expect("scalar count fits u8"),
                lists,
            ),
        }
    }
    pub(crate) fn from_operands<const SCALARS: usize, const LISTS: usize>(
        source: OperandIter<'a, SCALARS, LISTS>,
    ) -> Self {
        let mut out = Self::new(&source.prefix, &source.tails);
        out.inner.prefix_len = source.prefix_len;
        out.inner.prefix_idx = source.prefix_idx;
        out.inner.tail_idx = source.tail_idx;
        out.inner.item_idx = source.item_idx;
        out
    }

    /// Returns an empty register iterator.
    #[must_use]
    pub fn empty() -> Self {
        Self::new(&[], &[])
    }

    /// Returns an iterator over a single register.
    #[must_use]
    pub fn one(a: u32) -> Self {
        Self::new(&[a], &[])
    }

    /// Returns an iterator over two registers.
    #[must_use]
    pub fn two(a: u32, b: u32) -> Self {
        Self::new(&[a, b], &[])
    }

    /// Returns an iterator over three registers.
    #[must_use]
    pub fn three(a: u32, b: u32, c: u32) -> Self {
        Self::new(&[a, b, c], &[])
    }

    /// Returns an iterator over a slice of registers.
    #[must_use]
    pub fn slice(tail: &'a [u32]) -> Self {
        Self::new(&[], &[tail])
    }

    /// Returns an iterator over `first`, followed by all registers in `tail`.
    #[must_use]
    pub fn one_plus_slice(first: u32, tail: &'a [u32]) -> Self {
        Self::new(&[first], &[tail])
    }
}

impl<const SCALARS: usize, const LISTS: usize> Iterator for OperandIter<'_, SCALARS, LISTS> {
    type Item = u32;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        // The common one-result form needs no dynamic array index or cursor.
        if SCALARS == 1 {
            if self.prefix_len != 0 {
                self.prefix_len = 0;
                return Some(self.prefix[0]);
            }
        } else if SCALARS != 0 && self.prefix_idx < self.prefix_len {
            let out = self.prefix[usize::from(self.prefix_idx)];
            self.prefix_idx += 1;
            return Some(out);
        }
        if LISTS == 1 {
            let out = self.tails[0].get(self.item_idx).copied()?;
            self.item_idx += 1;
            return Some(out);
        }
        while let Some(tail) = self.tails.get(usize::from(self.tail_idx)) {
            if let Some(out) = tail.get(self.item_idx).copied() {
                self.item_idx += 1;
                return Some(out);
            }
            self.tail_idx += 1;
            self.item_idx = 0;
        }
        None
    }
}

impl Iterator for RegIter<'_> {
    type Item = u32;
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

#[cfg(test)]
mod tests {
    use super::{OperandIter, RegIter};
    use alloc::vec;
    use alloc::vec::Vec;

    #[test]
    fn partially_consumed_operands_keep_their_position_when_copied_or_exposed() {
        let tail = [8, 9];
        for consumed in 0..=3 {
            let mut source = OperandIter::<1, 1>::from_parts([7], 1, [&tail]);
            for _ in 0..consumed {
                source.next();
            }
            let copy = source;
            let exposed = RegIter::from_operands(source);
            let mut exposed_copy = exposed;
            let expected: Vec<_> = [7, 8, 9].into_iter().skip(consumed).collect();
            assert_eq!(source.collect::<Vec<_>>(), expected);
            assert_eq!(copy.collect::<Vec<_>>(), expected);
            assert_eq!(exposed.collect::<Vec<_>>(), expected);
            assert_eq!(exposed_copy.by_ref().collect::<Vec<_>>(), expected);
            assert_eq!(exposed_copy.next(), None);
            assert_eq!(exposed_copy.next(), None);
        }
        let mut scalar = OperandIter::<1, 0>::from_parts([42], 1, []);
        assert_eq!(scalar.next(), Some(42));
        let mut exposed = RegIter::from_operands(scalar);
        assert_eq!(exposed.next(), None);
        assert_eq!(exposed.next(), None);
    }

    #[test]
    fn public_constructors_keep_their_register_order() {
        for (iterator, expected) in [
            (RegIter::empty(), vec![]),
            (RegIter::one(1), vec![1]),
            (RegIter::two(1, 2), vec![1, 2]),
            (RegIter::three(1, 2, 3), vec![1, 2, 3]),
            (RegIter::slice(&[4, 5]), vec![4, 5]),
            (RegIter::one_plus_slice(1, &[4, 5]), vec![1, 4, 5]),
        ] {
            assert_eq!(iterator.collect::<Vec<_>>(), expected);
        }
    }
}
