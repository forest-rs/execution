// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Execution state the VM owns between runs and lends to one lease at a time.
//!
//! A run or a turn borrows the VM's register file, control and work budget for its
//! duration and returns them when it ends, so their buffers are reused instead of rebuilt and no
//! caller has to keep scratch storage of its own. A turn takes the register file and control
//! out of the lent box and gives them back by pointer ([`Held`]), so starting and finishing a
//! turn copies neither, nor any buffer contents. While they are lent the VM records the lease:
//! where its allocations begin. A lease that never returns, because its turn was
//! forgotten or a host panicked mid-run, is settled the next time the VM lends, collects,
//! registers or releases a group, snapshots or retains values, or hands out its heap mutably, by
//! discarding those allocations as a cancelled run would. A lease closes only when nothing is
//! left to discard: a trap already discarded the turn's allocations, a finished turn published
//! its returned values, and a saved computation keeps what it allocated. A returned turn that
//! was never finished is settled like any other, so forgetting it discards its outputs as
//! dropping it does.

use super::*;

/// The register file, control and work budget one lease executes with.
///
/// Each part is boxed: a turn takes them by pointer and gives them back, and its stack keeps
/// the register file and control, so they move into a saved computation and back without
/// being copied. After a save those are gone from here until a lease that needs them makes
/// fresh ones; a resumed computation brings its own.
#[derive(Default)]
pub(super) struct Exec {
    pub(super) ctx: Held<Registers>,
    pub(super) control: Held<ExecutionControl>,
    pub(super) budget: Held<Budget>,
}

impl Exec {
    /// Bytes of buffer capacity held for the next lease.
    pub(super) fn retained_bytes(&self) -> usize {
        self.ctx.get().map_or(0, Registers::retained_bytes)
            + self
                .control
                .get()
                .map_or(0, ExecutionControl::retained_bytes)
    }

    /// Makes sure every part is here, for a run that executes on them in place.
    pub(super) fn fill(&mut self) {
        self.ctx.fill();
        self.control.fill();
        self.budget.fill();
    }
}

/// Boxed execution state that a lease takes and gives back by pointer.
///
/// It is empty only after its contents moved to another holder: the VM's spare after a turn
/// took it, or a stack after it gave it back or was saved. Reading an empty one is a VM bug,
/// so dereferencing it panics.
pub(super) struct Held<T>(Option<Box<T>>);

impl<T: Default> Default for Held<T> {
    fn default() -> Self {
        Self(Some(Box::default()))
    }
}

impl<T> Held<T> {
    /// Nothing held.
    pub(super) fn empty() -> Self {
        Self(None)
    }

    /// Moves the contents out by pointer, leaving this empty.
    pub(super) fn take(&mut self) -> Self {
        Self(self.0.take())
    }

    /// The contents, unless they moved out.
    pub(super) fn get(&self) -> Option<&T> {
        self.0.as_deref()
    }
}

impl<T: Default> Held<T> {
    /// Fresh contents in place of none.
    fn fill(&mut self) {
        if self.0.is_none() {
            self.0 = Some(Box::default());
        }
    }

    /// Moves the contents out by pointer, or fresh contents if they already moved.
    pub(super) fn take_or_default(&mut self) -> Self {
        Self(Some(self.0.take().unwrap_or_default()))
    }
}

impl<T> core::ops::Deref for Held<T> {
    type Target = T;

    #[inline(always)]
    fn deref(&self) -> &T {
        self.0.as_deref().expect("held execution state is present")
    }
}

impl<T> core::ops::DerefMut for Held<T> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut T {
        self.0
            .as_deref_mut()
            .expect("held execution state is present")
    }
}

/// What the VM remembers while its execution state is lent.
#[derive(Clone, Copy)]
pub(super) struct Lease {
    /// Where the lease's own aggregates begin.
    pub(super) checkpoint: Checkpoint,
}

impl<H: Host> Vm<H> {
    /// Lends this VM's execution state to a lease whose allocations begin at `checkpoint`.
    ///
    /// The caller settles any earlier lease before choosing `checkpoint`.
    pub(super) fn lend(&mut self, checkpoint: Checkpoint) -> Box<Exec> {
        debug_assert!(
            self.lease.is_none(),
            "a lease is settled before lending again"
        );
        self.lease = Some(Lease { checkpoint });
        // Each lease takes the parts it uses: a resumed computation brings its own register
        // file and control, so none are made for it here.
        self.exec.take().unwrap_or_default()
    }

    /// Takes back execution state when its lease ends.
    pub(super) fn restore(&mut self, exec: Box<Exec>) {
        self.exec = Some(exec);
        self.lease = None;
    }

    /// Ends the open lease's claim on the heap once nothing is left to discard: after a trap
    /// rolled it back, when [`Turn::finish`] publishes returned values, or when a computation is
    /// saved. Settling a closed lease discards nothing.
    pub(super) fn close_lease(&mut self) {
        self.lease = None;
    }

    /// Moves the open lease's checkpoint after a collection compacted the heap.
    pub(super) fn relocate_lease(&mut self, checkpoint: Checkpoint) {
        if let Some(lease) = &mut self.lease {
            lease.checkpoint = checkpoint;
        }
    }

    /// Aborts a lease that never returned its state, discarding the aggregates it allocated.
    ///
    /// The lent state is gone with its borrower; the next lease starts from fresh state.
    pub(super) fn settle(&mut self) {
        if let Some(lease) = self.lease.take() {
            self.memory.rollback(lease.checkpoint);
        }
    }

    /// Releases the buffers the VM keeps between runs for its execution state.
    ///
    /// Runs reuse their register banks, frames, continuation segments and payload descriptors,
    /// which therefore stay at the largest size any run needed; [`HeapStats::scratch_bytes`]
    /// reports that. This frees them, and the next run allocates what it needs again. Values,
    /// registered groups and installed units are unaffected.
    pub fn shrink_scratch(&mut self) {
        self.settle();
        self.exec = None;
    }

    /// The budget the last run or turn returned.
    #[cfg(test)]
    pub(super) fn exec_budget(&self) -> &Budget {
        &self.exec.as_ref().expect("no lease is open").budget
    }

    /// A fresh identity for a resumable stack, unique within this VM.
    pub(super) fn next_stack(&mut self) -> u64 {
        self.stacks += 1;
        self.stacks
    }
}
