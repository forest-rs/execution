// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Register iteration shared by verification and disassembly.

use crate::bytecode::{MAX_REGISTER_LISTS, MAX_SCALAR_OPERANDS};
use crate::program::Reg;

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

/// A borrowed list of register operands, such as a call's arguments or a block's parameters.
///
/// Disassembly lends these from the decoded instruction without copying them.
#[derive(Copy, Clone, Default, PartialEq, Eq, Hash)]
pub struct RegList<'a>(&'a [u32]);

impl<'a> RegList<'a> {
    pub(crate) fn new(regs: &'a [u32]) -> Self {
        Self(regs)
    }

    pub(crate) fn raw(self) -> &'a [u32] {
        self.0
    }

    /// Number of registers in the list.
    #[must_use]
    pub fn len(self) -> usize {
        self.0.len()
    }

    /// Whether the list is empty.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.0.is_empty()
    }

    /// Returns the register at `index`, or `None` when it is out of range.
    #[must_use]
    pub fn get(self, index: usize) -> Option<Reg> {
        self.0.get(index).copied().map(Reg)
    }

    /// Iterates the registers in operand order.
    pub fn iter(self) -> impl DoubleEndedIterator<Item = Reg> + ExactSizeIterator + 'a {
        self.into_iter()
    }
}

impl<'a> IntoIterator for RegList<'a> {
    type Item = Reg;
    type IntoIter = core::iter::Map<core::slice::Iter<'a, u32>, fn(&u32) -> Reg>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter().map(|&register| Reg(register))
    }
}

impl core::fmt::Debug for RegList<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
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
    pub fn one(a: Reg) -> Self {
        Self::new(&[a.0], &[])
    }

    /// Returns an iterator over two registers.
    #[must_use]
    pub fn two(a: Reg, b: Reg) -> Self {
        Self::new(&[a.0, b.0], &[])
    }

    /// Returns an iterator over three registers.
    #[must_use]
    pub fn three(a: Reg, b: Reg, c: Reg) -> Self {
        Self::new(&[a.0, b.0, c.0], &[])
    }

    /// Returns an iterator over a list of registers.
    #[must_use]
    pub fn slice(tail: RegList<'a>) -> Self {
        Self::new(&[], &[tail.0])
    }

    /// Returns an iterator over `first`, followed by all registers in `tail`.
    #[must_use]
    pub fn one_plus_slice(first: Reg, tail: RegList<'a>) -> Self {
        Self::new(&[first.0], &[tail.0])
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
    type Item = Reg;
    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(Reg)
    }
}

#[cfg(test)]
mod tests {
    use super::{OperandIter, Reg, RegIter, RegList};
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
            let registers: Vec<_> = expected.iter().copied().map(Reg).collect();
            assert_eq!(source.collect::<Vec<_>>(), expected);
            assert_eq!(copy.collect::<Vec<_>>(), expected);
            assert_eq!(exposed.collect::<Vec<_>>(), registers);
            assert_eq!(exposed_copy.by_ref().collect::<Vec<_>>(), registers);
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
            (RegIter::one(Reg(1)), vec![1]),
            (RegIter::two(Reg(1), Reg(2)), vec![1, 2]),
            (RegIter::three(Reg(1), Reg(2), Reg(3)), vec![1, 2, 3]),
            (RegIter::slice(RegList::new(&[4, 5])), vec![4, 5]),
            (
                RegIter::one_plus_slice(Reg(1), RegList::new(&[4, 5])),
                vec![1, 4, 5],
            ),
        ] {
            let expected: Vec<_> = expected.into_iter().map(Reg).collect();
            assert_eq!(iterator.collect::<Vec<_>>(), expected);
        }
    }
}
