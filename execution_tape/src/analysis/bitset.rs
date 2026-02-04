// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A small bitset used by verifier analyses.

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BitSet {
    bits: Vec<u64>,
    len: usize,
}

impl BitSet {
    #[must_use]
    pub(crate) fn new_empty(len: usize) -> Self {
        let words = len.div_ceil(64);
        Self {
            bits: vec![0; words],
            len,
        }
    }

    #[must_use]
    pub(crate) fn new_full(len: usize) -> Self {
        let mut s = Self::new_empty(len);
        for w in &mut s.bits {
            *w = !0;
        }
        // Clear unused bits in last word.
        let rem = len % 64;
        if rem != 0 {
            let mask = (1_u64 << rem) - 1;
            if let Some(last) = s.bits.last_mut() {
                *last &= mask;
            }
        }
        s
    }

    #[must_use]
    pub(crate) fn get(&self, idx: usize) -> bool {
        if idx >= self.len {
            return false;
        }
        let w = idx / 64;
        let b = idx % 64;
        (self.bits[w] >> b) & 1 == 1
    }

    pub(crate) fn set(&mut self, idx: usize) {
        if idx >= self.len {
            return;
        }
        let w = idx / 64;
        let b = idx % 64;
        self.bits[w] |= 1_u64 << b;
    }

    pub(crate) fn clear(&mut self, idx: usize) {
        if idx >= self.len {
            return;
        }
        let w = idx / 64;
        let b = idx % 64;
        self.bits[w] &= !(1_u64 << b);
    }

    pub(crate) fn intersect_with(&mut self, other: &Self) {
        for (a, b) in self.bits.iter_mut().zip(other.bits.iter()) {
            *a &= *b;
        }
    }

    pub(crate) fn union_with(&mut self, other: &Self) {
        for (a, b) in self.bits.iter_mut().zip(other.bits.iter()) {
            *a |= *b;
        }
    }

    pub(crate) fn subtract_with(&mut self, other: &Self) {
        for (a, b) in self.bits.iter_mut().zip(other.bits.iter()) {
            *a &= !*b;
        }
    }
}
