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
    /// The options carry a trace observer, and the computation waits at a host request with a
    /// guest callback running on top of it.
    ///
    /// The open scopes and instruction attempts of nested callbacks are not handed to a new
    /// observer yet. Resume it unobserved, or observe it once the callbacks have returned.
    Observed,
}

impl fmt::Display for ResumeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown => f.write_str("no saved computation has this ID"),
            Self::ForeignVm => f.write_str("the ID names a computation of another VM"),
            Self::Observed => {
                f.write_str("a computation running a guest callback cannot be observed on resuming")
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
    /// Frames the saved computations hold, counted against [`Limits::max_frames`].
    frames: usize,
    /// Frames a replay reserves for computations the recorded VM held besides its own.
    reserved: usize,
}

impl<H: Host> Vm<H> {
    /// Saves a new computation of the installed function `entry` without running it.
    ///
    /// The computation waits before its first instruction until [`Self::resume`] continues it.
    /// Entry is checked now, as [`Self::start`] checks it, including that aggregate arguments
    /// name aggregates the heap holds, and refused with its trap; so is a VM that already keeps
    /// [`Limits::max_saved_computations`] computations
    /// ([`Trap::SavedComputationLimitExceeded`]). Nothing observes it until its first resume,
    /// which enters it under that invocation's observers, as `start` would, so a recorder
    /// passed to every resume records it whole.
    pub fn spawn(&mut self, entry: FuncRef, args: Vec<Value>) -> Result<ComputationId, TrapInfo> {
        self.entry_program(entry.unit)?;
        let limit = self.limits.max_saved_computations;
        if self.saved.len() >= limit {
            return Err(TrapInfo {
                unit: entry.unit,
                func: entry.func,
                pc: Pc(0),
                span_id: None,
                trap: Trap::SavedComputationLimitExceeded { limit },
            });
        }
        self.settle();
        let checkpoint = self.memory.checkpoint();
        let mut invocation = self.start_with_inputs(
            entry,
            Cow::Owned(args),
            InvocationOptions::default(),
            checkpoint,
            DataWork::default(),
        )?;
        invocation.mark_spawned();
        Ok(invocation
            .save()
            .unwrap_or_else(|_| unreachable!("an unrun computation within the limit saves")))
    }

    /// Ends a saved computation without running it further, releasing its frames.
    ///
    /// Like dropping an invocation that runs it, this runs no guest code and undoes no host
    /// effect; unlike it, nothing is reported, as a saved computation has no observer
    /// attached. The aggregates only it reached are reclaimed by the next collection. Refuses
    /// an ID this VM holds no computation for, or another VM's.
    pub fn discard(&mut self, id: ComputationId) -> Result<(), ResumeError> {
        self.saved.get(id)?;
        self.settle();
        drop(self.saved.take(id));
        let pinned = self.saved.len() != 0;
        self.memory.aggs_mut().set_pinned(pinned);
        Ok(())
    }

    /// Counts `frames` as held by other computations in every later invocation, as a recorded
    /// VM's saved computations were, so replay refuses frames where the original did.
    pub(crate) fn reserve_frames(&mut self, frames: usize) {
        self.saved.reserved = frames;
    }

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
            frames: 0,
            reserved: 0,
        }
    }

    pub(super) fn insert(&mut self, saved: Saved) -> ComputationId {
        self.frames += saved.stack.ctx.held_frames();
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

    /// The computation `id` names, if this store holds it.
    pub(super) fn get(&self, id: ComputationId) -> Result<&Saved, ResumeError> {
        if id.vm != self.vm {
            return Err(ResumeError::ForeignVm);
        }
        self.slots
            .get(id.slot as usize)
            .filter(|entry| entry.generation == id.generation)
            .and_then(|entry| entry.saved.as_deref())
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
        self.frames -= saved.stack.ctx.held_frames();
        // A slot whose generations are exhausted is retired, so no later ID can match it.
        if let Some(next) = entry.generation.checked_add(1) {
            entry.generation = next;
            self.free.push(id.slot);
        }
        Some(saved)
    }

    /// Frames the saved computations hold.
    pub(super) fn frames(&self) -> usize {
        self.frames
    }

    /// Frames held besides a starting or resuming invocation's own: the saved computations'
    /// and any a replay reserves.
    pub(super) fn resident_frames(&self) -> usize {
        self.frames + self.reserved
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asm::{FunctionSig, ProgramBuilder, Reg};
    use crate::host::{HostContext, HostError, SigHash, ValueRef};
    use crate::program::ValueType;
    use alloc::vec;

    struct NoHost;
    impl Host for NoHost {
        fn call(
            &mut self,
            _: &str,
            _: SigHash,
            _: &[ValueRef<'_>],
            _: &mut [Value],
            _: HostContext<'_, '_>,
        ) -> Result<u64, HostError> {
            Err(HostError::UnknownSymbol)
        }
    }

    #[test]
    fn a_collection_names_the_saved_computation_whose_root_dangles() {
        let mut b = ProgramBuilder::new();
        let entry = b
            .declare_function(FunctionSig {
                arg_types: vec![ValueType::Agg],
                ret_types: vec![ValueType::Agg],
            })
            .unwrap();
        let mut a = b.function_asm(entry).unwrap();
        a.ret(Reg::EFFECT, &[Reg(1)]);
        b.define_function(entry, a).unwrap();
        let mut vm = Vm::new(NoHost, Limits::default());
        let unit = vm.install(Arc::new(b.build_verified().unwrap())).unwrap();
        let boxed = vm.aggregates_mut().tuple_new(vec![]).unwrap();
        let id = vm
            .spawn(FuncRef::new(unit, entry), vec![Value::Agg(boxed)])
            .unwrap();
        // Only a heap changed behind the VM leaves a saved root dangling; stand in for that.
        let dangling = AggHandle(999);
        let saved = vm.saved.iter_mut().next().expect("one saved computation");
        saved.args = Cow::Owned(vec![Value::Agg(dangling)]);
        assert_eq!(
            vm.collect().unwrap_err(),
            CollectError::SavedDanglingHandle {
                computation: id,
                handle: dangling,
            }
        );
    }
}
