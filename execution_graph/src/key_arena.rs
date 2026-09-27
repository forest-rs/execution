// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Contiguous dependency lists in one reusable buffer. Lists occupy power-of-two blocks;
//! a released block links to its size class's free list through its first (unused) key slot.
//! Capacity is derived from length, so a range needs only an offset and a length. Growing or
//! shrinking across a size boundary relocates the list. No references survive a mutation.

use crate::dirty::DirtyKey;
use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct KeyRange {
    start: u32,
    len: u32,
}
impl KeyRange {
    pub(crate) fn len(self) -> usize {
        self.len as usize
    }
    pub(crate) fn is_empty(self) -> bool {
        self.len == 0
    }
    fn capacity(self) -> u32 {
        if self.len == 0 {
            0
        } else {
            self.len
                .checked_next_power_of_two()
                .expect("dependency list exhausted")
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct KeyArena {
    values: Vec<DirtyKey>,
    free: [Option<u32>; 32],
}
impl KeyArena {
    pub(crate) fn capacity(&self) -> usize {
        self.values.capacity()
    }
    pub(crate) fn get(&self, range: KeyRange) -> &[DirtyKey] {
        &self.values[range.start as usize..range.start as usize + range.len()]
    }

    fn allocate(&mut self, capacity: u32) -> u32 {
        let class = capacity.trailing_zeros() as usize;
        if let Some(start) = self.free[class] {
            let next = self.values[start as usize].0;
            self.free[class] =
                (next != u32::MAX as usize).then(|| u32::try_from(next).expect("valid free range"));
            start
        } else {
            let start = u32::try_from(self.values.len()).expect("dependency arena exhausted");
            let end = start
                .checked_add(capacity)
                .expect("dependency arena exhausted");
            self.values.resize(end as usize, DirtyKey(0));
            start
        }
    }

    pub(crate) fn release(&mut self, range: KeyRange) {
        let capacity = range.capacity();
        if capacity != 0 {
            let class = capacity.trailing_zeros() as usize;
            self.values[range.start as usize] =
                DirtyKey(self.free[class].map_or(u32::MAX as usize, |n| n as usize));
            self.free[class] = Some(range.start);
        }
    }

    pub(crate) fn push(&mut self, range: &mut KeyRange, key: DirtyKey) {
        let len = range.len.checked_add(1).expect("dependency list exhausted");
        let capacity = len
            .checked_next_power_of_two()
            .expect("dependency list exhausted");
        if capacity != range.capacity() {
            let start = self.allocate(capacity);
            self.values.copy_within(
                range.start as usize..range.start as usize + range.len(),
                start as usize,
            );
            self.release(*range);
            range.start = start;
        }
        self.values[range.start as usize + range.len()] = key;
        range.len = len;
    }

    pub(crate) fn remove(&mut self, range: &mut KeyRange, key: DirtyKey) {
        let Some(offset) = self
            .get(*range)
            .iter()
            .position(|&candidate| candidate == key)
        else {
            return;
        };
        let old = *range;
        self.values.copy_within(
            old.start as usize + offset + 1..old.start as usize + old.len(),
            old.start as usize + offset,
        );
        range.len -= 1;
        if range.capacity() != old.capacity() {
            if range.is_empty() {
                *range = KeyRange::default();
            } else {
                let start = self.allocate(range.capacity());
                self.values.copy_within(
                    old.start as usize..old.start as usize + range.len(),
                    start as usize,
                );
                range.start = start;
            }
            self.release(old);
        }
    }

    pub(crate) fn sort(&mut self, range: KeyRange) {
        self.values[range.start as usize..range.start as usize + range.len()].sort_unstable();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interleaved_changes_match_independent_vectors() {
        let mut arena = KeyArena::default();
        let mut lists = [KeyRange::default(); 8];
        let mut reference: [Vec<DirtyKey>; 8] = core::array::from_fn(|_| Vec::new());
        let mut state = 17_u64;
        for step in 0..10_000 {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let list = usize::try_from((state >> 32) % 8).unwrap();
            let key = DirtyKey(usize::try_from((state >> 16) % 32).unwrap());
            if step % 71 == 0 {
                arena.release(core::mem::take(&mut lists[list]));
                reference[list].clear();
            } else if reference[list].contains(&key) {
                arena.remove(&mut lists[list], key);
                reference[list].retain(|&candidate| candidate != key);
            } else {
                arena.push(&mut lists[list], key);
                reference[list].push(key);
            }
            for (range, expected) in lists.iter().zip(&reference) {
                assert_eq!(
                    arena.get(*range),
                    expected,
                    "range corruption at step {step}"
                );
            }
        }
    }

    #[test]
    fn independent_lists_relocate_and_reuse_released_ranges() {
        let mut arena = KeyArena::default();
        let mut lists = [KeyRange::default(); 16];
        for cycle in 0..100 {
            for (i, list) in lists.iter_mut().enumerate() {
                for key in 0..=i {
                    arena.push(list, DirtyKey(key));
                }
            }
            for (i, list) in lists.iter().enumerate() {
                assert_eq!(arena.get(*list), (0..=i).map(DirtyKey).collect::<Vec<_>>());
            }
            for (i, list) in lists.iter_mut().enumerate() {
                // Remove from the middle as well as the endpoints; shrinking relocates ranges.
                for key in (0..=i).rev().step_by(2) {
                    arena.remove(list, DirtyKey(key));
                }
                let expected: Vec<_> = (0..=i)
                    .filter(|key| (i - key) % 2 != 0)
                    .map(DirtyKey)
                    .collect();
                assert_eq!(arena.get(*list), expected);
                arena.release(core::mem::take(list));
            }
            assert!(
                arena.values.len() < 1024,
                "unbounded arena growth after cycle {cycle}"
            );
        }
    }
}
