// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Execution-owned suspended storage and one-use authority, independent of physical slot reuse.

use super::ContinuationId;
use alloc::{sync::Arc, vec::Vec};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LookupError {
    ForeignExecution,
    Consumed,
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StorageError {
    Limit,
    Capacity,
    Closed,
    ForeignExecution,
}

struct Slot<T> {
    revision: u64,
    saved: Option<T>,
    next_free: Option<u32>,
}

/// The execution owns saved stacks. Cloned keys alias the same single-use authority.
/// Namespace registration is lazy, so executions without continuations allocate no control arena.
pub(crate) struct Resumptions<T> {
    owner: Option<Arc<()>>,
    slots: Vec<Slot<T>>,
    free: Option<u32>,
    live: usize,
    closed: bool,
}

impl<T> Default for Resumptions<T> {
    fn default() -> Self {
        Self {
            owner: None,
            slots: Vec::new(),
            free: None,
            live: 0,
            closed: false,
        }
    }
}

impl<T> Resumptions<T> {
    pub(crate) fn len(&self) -> usize {
        self.live
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (ContinuationId, &T)> {
        self.slots.iter().enumerate().filter_map(|(index, slot)| {
            Some((
                ContinuationId {
                    owner: self.owner.as_ref()?.clone(),
                    slot: u32::try_from(index).expect("arena slot"),
                    revision: slot.revision,
                },
                slot.saved.as_ref()?,
            ))
        })
    }

    /// Rejected admission returns its storage to the execution owner, without dropping frames.
    pub(crate) fn insert(
        &mut self,
        owner: &Arc<()>,
        saved: T,
        limit: usize,
    ) -> Result<ContinuationId, (StorageError, T)> {
        let error = if self.closed {
            Some(StorageError::Closed)
        } else if self
            .owner
            .as_ref()
            .is_some_and(|prior| !Arc::ptr_eq(prior, owner))
        {
            Some(StorageError::ForeignExecution)
        } else if self.live >= limit {
            Some(StorageError::Limit)
        } else {
            None
        };
        if let Some(error) = error {
            return Err((error, saved));
        }
        let slot = if let Some(slot) = self.free {
            slot
        } else {
            let Ok(slot) = u32::try_from(self.slots.len()) else {
                return Err((StorageError::Capacity, saved));
            };
            self.slots.push(Slot {
                revision: 0,
                saved: None,
                next_free: None,
            });
            slot
        };
        let entry = &mut self.slots[slot as usize];
        self.free = entry.next_free.take();
        entry.saved = Some(saved);
        self.live += 1;
        self.owner.get_or_insert_with(|| Arc::clone(owner));
        Ok(ContinuationId {
            owner: Arc::clone(owner),
            slot,
            revision: entry.revision,
        })
    }

    fn index(&self, key: &ContinuationId) -> Result<usize, LookupError> {
        if !self
            .owner
            .as_ref()
            .is_some_and(|owner| Arc::ptr_eq(owner, &key.owner))
        {
            return Err(LookupError::ForeignExecution);
        }
        if self.closed {
            return Err(LookupError::Closed);
        }
        let index = key.slot as usize;
        let entry = self.slots.get(index).ok_or(LookupError::Consumed)?;
        if entry.revision != key.revision || entry.saved.is_none() {
            return Err(LookupError::Consumed);
        }
        Ok(index)
    }

    pub(crate) fn get(&self, key: &ContinuationId) -> Result<&T, LookupError> {
        let index = self.index(key)?;
        Ok(self.slots[index].saved.as_ref().expect("live capability"))
    }

    /// Consumes authority before lending storage to arbitrary execution or observation code.
    pub(crate) fn take(&mut self, key: &ContinuationId) -> Result<T, LookupError> {
        let index = self.index(key)?;
        let entry = &mut self.slots[index];
        let saved = entry.saved.take().expect("live capability");
        self.live -= 1;
        // Exhaustion permanently retires this physical slot. Never wrap a generation witness.
        if let Some(revision) = entry.revision.checked_add(1) {
            entry.revision = revision;
            entry.next_free = self.free;
            self.free = Some(key.slot);
        }
        Ok(saved)
    }

    /// Invalidates every key before the owner starts cancelling individual retained stacks.
    pub(crate) fn close(&mut self) -> impl Iterator<Item = T> + use<T> {
        self.closed = true;
        self.live = 0;
        self.free = None;
        core::mem::take(&mut self.slots)
            .into_iter()
            .filter_map(|slot| slot.saved)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::rc::Rc;
    use core::cell::Cell;

    #[test]
    fn aliases_cannot_resume_twice_or_retarget_reused_storage() {
        let owner = Arc::new(());
        let mut saved = Resumptions::default();
        let first = saved.insert(&owner, "first", 1).unwrap();
        let alias = first.clone();
        assert_eq!(saved.get(&alias), Ok(&"first"));
        assert_eq!(saved.take(&first), Ok("first"));
        assert_eq!(saved.take(&alias), Err(LookupError::Consumed));
        let next = saved.insert(&owner, "next", 1).unwrap();
        assert_eq!(
            first.slot, next.slot,
            "reuse capacity without reusing authority"
        );
        assert_ne!(first, next);
        assert_eq!(saved.get(&alias), Err(LookupError::Consumed));
        assert_eq!(saved.take(&next), Ok("next"));
        assert_eq!(saved.len(), 0);
        assert_eq!(saved.slots.len(), 1);
    }

    #[test]
    fn a_foreign_denial_leaves_both_executions_usable() {
        let mut first = Resumptions::default();
        let mut second = Resumptions::default();
        let a = first.insert(&Arc::new(()), 11, 1).unwrap();
        let b = second.insert(&Arc::new(()), 99, 1).unwrap();
        assert_eq!(first.take(&b), Err(LookupError::ForeignExecution));
        assert_eq!(second.take(&a), Err(LookupError::ForeignExecution));
        assert_eq!(first.take(&a), Ok(11));
        assert_eq!(second.take(&b), Ok(99));
    }

    #[test]
    fn admission_denial_returns_storage_without_changing_existing_authority() {
        let owner = Arc::new(());
        let mut saved = Resumptions::default();
        let first = saved.insert(&owner, 11, 1).unwrap();
        assert_eq!(saved.insert(&owner, 22, 1), Err((StorageError::Limit, 22)));
        assert_eq!(
            saved.insert(&Arc::new(()), 33, 2),
            Err((StorageError::ForeignExecution, 33))
        );
        assert_eq!(saved.get(&first), Ok(&11));
        assert_eq!(saved.len(), 1);
    }

    #[test]
    fn generation_exhaustion_retires_a_slot_and_never_resurrects_an_alias() {
        let owner = Arc::new(());
        let mut saved = Resumptions::default();
        let mut last = saved.insert(&owner, 11, 1).unwrap();
        saved.slots[0].revision = u64::MAX;
        last.revision = u64::MAX;
        assert_eq!(saved.take(&last), Ok(11));
        let next = saved.insert(&owner, 22, 1).unwrap();
        assert_ne!(last.slot, next.slot);
        assert_eq!(saved.get(&last), Err(LookupError::Consumed));
        assert_eq!(saved.take(&next), Ok(22));
    }

    #[test]
    fn close_invalidates_siblings_before_releasing_any_of_their_storage() {
        struct Retained(Rc<Cell<usize>>);
        impl Drop for Retained {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }
        let owner = Arc::new(());
        let drops = Rc::new(Cell::new(0));
        let mut saved = Resumptions::default();
        let a = saved
            .insert(&owner, Retained(drops.clone()), 2)
            .ok()
            .unwrap();
        let b = saved
            .insert(&owner, Retained(drops.clone()), 2)
            .ok()
            .unwrap();
        let mut closing = saved.close();
        assert!(matches!(saved.get(&a), Err(LookupError::Closed)));
        assert!(matches!(saved.get(&b), Err(LookupError::Closed)));
        assert_eq!(saved.len(), 0);
        drop(closing.next());
        assert_eq!(drops.get(), 1, "a sibling still owns its retained storage");
        drop(closing);
        assert_eq!(drops.get(), 2);
        assert!(matches!(
            saved.insert(&owner, Retained(drops.clone()), 2),
            Err((StorageError::Closed, _))
        ));
    }

    #[test]
    fn unused_execution_keeps_control_storage_lazy() {
        let saved = Resumptions::<()>::default();
        assert!(saved.owner.is_none());
        assert_eq!(saved.slots.capacity(), 0);
    }
}
