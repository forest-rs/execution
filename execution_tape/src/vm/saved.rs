// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Computations a VM keeps between leases.
//!
//! An invocation that is waiting, whether between quanta, at a host request or at a yielded
//! suspension, can be saved: its stack moves into the VM and the invocation ends without
//! cancelling it. [`Vm::resume`](super::Vm::resume) later starts a new invocation on it, with a
//! fresh work allowance. Saved computations are explicit resources: each stays until resumed
//! or until the VM is dropped. Each is named by a [`ComputationId`] whose generation changes
//! when it is resumed, so an ID can be used once.

use super::*;
use alloc::borrow::Cow;
use continuation::Stack;

/// Names one computation a VM saved, until it is resumed.
///
/// IDs are generation-checked: once the computation is resumed, its ID names nothing, even after
/// the VM reuses its storage for another. An ID also names its VM, and another VM refuses it
/// with [`ResumeError::ForeignVm`]. That identity is the address of an allocation the VM keeps
/// for its lifetime, including across [`Vm::replace_host`](super::Vm::replace_host), so an ID
/// that outlives its VM could match a later VM that reuses the address.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ComputationId {
    vm: usize,
    slot: u32,
    generation: u32,
}

/// Why [`Vm::resume`](super::Vm::resume) refused, leaving the computation saved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ResumeError {
    /// The VM holds no saved computation by that ID: it was already resumed.
    Unknown,
    /// The ID names a computation another VM saved.
    ForeignVm,
    /// The options carry a trace or access observer.
    ///
    /// An observer of a resumed computation would see it mid-run, with no record of the frames
    /// and scopes already open; resuming observed computations needs that handoff first.
    Observed,
}

impl fmt::Display for ResumeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown => f.write_str("no saved computation has this ID"),
            Self::ForeignVm => f.write_str("the ID names a computation of another VM"),
            Self::Observed => {
                f.write_str("a resumed computation cannot be observed yet; resume it unobserved")
            }
        }
    }
}

impl core::error::Error for ResumeError {}

/// A saved computation: its stack and the inputs it owns.
pub(super) struct Saved {
    pub(super) stack: Stack,
    pub(super) args: Cow<'static, [Value]>,
}

impl Saved {
    /// Bytes of register, frame and argument capacity held, excluding payloads.
    fn retained_bytes(&self) -> usize {
        self.stack.ctx.retained_bytes()
            + self.stack.control.retained_bytes()
            + self.args.len() * size_of::<Value>()
    }
}

struct Slot {
    generation: u32,
    saved: Option<Box<Saved>>,
}

/// The computations a VM keeps between leases.
pub(super) struct Store {
    /// The identity its IDs carry.
    vm: usize,
    slots: Vec<Slot>,
    free: Vec<u32>,
    len: usize,
}

impl<H: Host> Vm<H> {
    /// Panics if the heap was replaced while computations were saved, before a saved handle can
    /// name another aggregate.
    pub(super) fn check_pinned(&self) {
        assert!(
            self.saved.len() == 0 || self.memory.aggs().is_pinned(),
            "the aggregate heap was replaced while the VM held saved computations; \
             their handles name nothing it holds"
        );
    }
}

impl Store {
    /// An empty store whose IDs carry `vm`.
    pub(super) fn new(vm: usize) -> Self {
        Self {
            vm,
            slots: Vec::new(),
            free: Vec::new(),
            len: 0,
        }
    }

    pub(super) fn insert(&mut self, saved: Saved) -> ComputationId {
        let saved = Some(Box::new(saved));
        self.len += 1;
        if let Some(slot) = self.free.pop() {
            let entry = &mut self.slots[slot as usize];
            entry.saved = saved;
            return ComputationId {
                vm: self.vm,
                slot,
                generation: entry.generation,
            };
        }
        let slot = u32::try_from(self.slots.len()).expect("saved computations fit u32");
        self.slots.push(Slot {
            generation: 0,
            saved,
        });
        ComputationId {
            vm: self.vm,
            slot,
            generation: 0,
        }
    }

    /// Checks that `id` names a computation this store holds.
    pub(super) fn check(&self, id: ComputationId) -> Result<(), ResumeError> {
        if id.vm != self.vm {
            return Err(ResumeError::ForeignVm);
        }
        self.slots
            .get(id.slot as usize)
            .filter(|entry| entry.generation == id.generation && entry.saved.is_some())
            .map(|_| ())
            .ok_or(ResumeError::Unknown)
    }

    /// Removes the computation `id` names, so the ID names nothing afterwards.
    pub(super) fn take(&mut self, id: ComputationId) -> Option<Box<Saved>> {
        if id.vm != self.vm {
            return None;
        }
        let entry = self
            .slots
            .get_mut(id.slot as usize)
            .filter(|entry| entry.generation == id.generation)?;
        let saved = entry.saved.take()?;
        self.len -= 1;
        // A slot whose generations are exhausted is retired, so no later ID can match it.
        if let Some(next) = entry.generation.checked_add(1) {
            entry.generation = next;
            self.free.push(id.slot);
        }
        Some(saved)
    }

    pub(super) fn len(&self) -> usize {
        self.len
    }

    /// Every saved computation, with its ID.
    pub(super) fn iter(&self) -> impl Iterator<Item = (ComputationId, &Saved)> {
        self.slots.iter().enumerate().filter_map(|(slot, entry)| {
            let id = ComputationId {
                vm: self.vm,
                slot: u32::try_from(slot).expect("saved computations fit u32"),
                generation: entry.generation,
            };
            Some((id, entry.saved.as_deref()?))
        })
    }

    /// Bytes of register, frame and argument capacity saved computations hold.
    pub(super) fn retained_bytes(&self) -> usize {
        self.iter().map(|(_, saved)| saved.retained_bytes()).sum()
    }

    pub(super) fn iter_mut(&mut self) -> impl Iterator<Item = &mut Saved> {
        self.slots
            .iter_mut()
            .filter_map(|slot| slot.saved.as_deref_mut())
    }
}
