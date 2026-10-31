// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use crate::recording::{CallStack, RecordedValue, frames::FrameId};
use crate::trace::TraceLocation;
use alloc::vec::Vec;

/// Optional instruction-history retention. Boundary recording remains enabled separately.
#[derive(Clone, Debug)]
pub struct InstructionCapture {
    /// Maximum retained attempts. Additional execution truncates history without losing boundaries.
    pub max_steps: usize,
    /// Locations and operand registers whose actual values should be retained.
    /// Only source inputs and successful local writes are available, not arbitrary bank contents.
    pub watches: Vec<RegisterWatch>,
}

impl Default for InstructionCapture {
    fn default() -> Self {
        Self {
            max_steps: 10_000,
            watches: Vec::new(),
        }
    }
}

/// Opt-in observation of source registers at one instruction, including repeated visits.
#[derive(Clone, Debug)]
pub struct RegisterWatch {
    /// Exact source instruction location.
    pub location: TraceLocation,
    /// Virtual register IDs. r0, non-operands and unavailable registers produce no observation.
    pub registers: Vec<u32>,
}

/// Borrowed selected register values backed by the recording's shared graph arena.
#[derive(Clone, Copy)]
pub struct RegisterObservations<'a> {
    pub(crate) entries: &'a [(u32, usize)],
    pub(crate) arena: &'a crate::recording::values::Arena,
}
impl<'a> RegisterObservations<'a> {
    /// Number of retained source operand occurrences.
    pub fn len(self) -> usize {
        self.entries.len()
    }
    /// Whether no values were retained for this phase.
    pub fn is_empty(self) -> bool {
        self.entries.is_empty()
    }
    /// Iterates source register IDs and retained values without expanding graphs.
    pub fn iter(self) -> impl ExactSizeIterator<Item = (u32, &'a RecordedValue)> {
        self.entries
            .iter()
            .map(|&(reg, value)| (reg, &self.arena.values[value]))
    }
}
impl core::fmt::Debug for RegisterObservations<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

/// Completeness of the optional instruction history, independently of boundary capture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstructionStatus {
    /// Every attempted instruction and its completion were retained.
    Complete,
    /// A bounded prefix was retained. The rest of the run was not observed here.
    Truncated {
        /// Configured maximum number of retained attempts.
        limit: usize,
    },
    /// A callback or run completion is missing, or callbacks arrived out of order.
    Incomplete,
}

/// Observed outcome of one attempted instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstructionOutcome {
    /// The embedding explicitly ended the invocation at a pending host instruction.
    /// The guest instruction produced no successful writes.
    Abandoned,
    /// The matching completion was not observed.
    Pending,
    /// The instruction completed, including dispatch into a guest callee.
    Completed,
    /// The instruction was abandoned by a typed non-local return.
    Unwound {
        /// Target activation ordinal within this invocation.
        target: u64,
    },
    /// The instruction trapped. The final VM trap is available from the recording outcome.
    Trapped,
}

/// One attempted instruction in admission order. Locations are source byte offsets.
///
/// `next` is the actual executable successor; typed block declarations have no event.
/// Calls complete at callee entry, while returns resume the suspended caller. A host attempt
/// remains outstanding during its callbacks, so completions can overlap in this admission order.
#[derive(Clone, Debug)]
pub struct InstructionStep<'a> {
    /// Selected actual read operands before execution.
    pub inputs: RegisterObservations<'a>,
    /// Selected actual writes after a successful local operation. Calls/returns have none.
    pub outputs: RegisterObservations<'a>,
    /// Executed instruction location.
    pub location: TraceLocation,
    /// Retained host attempt that remains outstanding while this callback instruction executes.
    /// `None` means no outstanding parent attempt; ordinary guest ancestry is in `stack`.
    pub parent_attempt: Option<usize>,
    /// Source opcode byte.
    pub opcode: u8,
    /// Actual guest successor, absent at root/callback return, trap or missing completion.
    pub next: Option<TraceLocation>,
    /// Observed instruction outcome, separate from the eventual invocation result.
    pub outcome: InstructionOutcome,
    /// Guest ancestry at the start of this attempt, when captured.
    pub stack: Option<CallStack<'a>>,
}

#[derive(Debug)]
pub(crate) struct Step {
    pub(crate) inputs_seen: bool,
    pub(crate) inputs: core::ops::Range<usize>,
    pub(crate) outputs: core::ops::Range<usize>,
    pub(crate) location: TraceLocation,
    pub(crate) parent: Option<usize>,
    pub(crate) opcode: u8,
    pub(crate) next: Option<TraceLocation>,
    pub(crate) outcome: InstructionOutcome,
    pub(crate) frame: Option<FrameId>,
}

#[derive(Debug)]
pub(crate) struct History {
    pub(crate) mask_seen: bool,
    pub(crate) registers: Vec<(u32, usize)>,
    pub(crate) config: InstructionCapture,
    pub(crate) steps: Vec<Step>,
    pub(crate) status: InstructionStatus,
    pending: Vec<PendingAttempt>,
    next: Option<TraceLocation>,
    admitted: bool,
    control_boundary: bool,
    returning_callback: bool,
    pub(crate) started: bool,
    pub(crate) finished: bool,
    // An explicit root transfer may close an older host attempt after newer callback steps.
    pub(crate) root_exit: Option<usize>,
}

impl History {
    pub(crate) fn new(mut config: InstructionCapture) -> Self {
        config
            .watches
            .sort_by_key(|watch| (watch.location.func.0, watch.location.pc));
        let mut watches: Vec<RegisterWatch> = Vec::new();
        for mut watch in config.watches {
            if let Some(previous) = watches
                .last_mut()
                .filter(|previous| previous.location == watch.location)
            {
                previous.registers.append(&mut watch.registers);
            } else {
                watches.push(watch);
            }
        }
        for watch in &mut watches {
            watch.registers.sort_unstable();
            watch.registers.dedup();
            watch.registers.retain(|&reg| reg != 0);
        }
        watches.retain(|watch| !watch.registers.is_empty());
        config.watches = watches;
        Self {
            mask_seen: false,
            registers: Vec::new(),
            config,
            steps: Vec::new(),
            status: InstructionStatus::Complete,
            pending: Vec::new(),
            next: None,
            admitted: false,
            control_boundary: false,
            returning_callback: false,
            started: false,
            finished: false,
            root_exit: None,
        }
    }

    pub(crate) fn watch(&self, location: TraceLocation) -> Option<&RegisterWatch> {
        self.config
            .watches
            .binary_search_by_key(&(location.func.0, location.pc), |watch| {
                (watch.location.func.0, watch.location.pc)
            })
            .ok()
            .map(|index| &self.config.watches[index])
    }

    pub(crate) fn finish(&mut self) {
        if !self.pending.is_empty()
            || self.returning_callback
            || !self.started
            || !self.finished
            || !self.mask_seen
        {
            self.status = InstructionStatus::Incomplete;
        }
    }
}

#[derive(Debug)]
struct PendingAttempt {
    location: TraceLocation,
    index: Option<usize>,
    suspended: bool,
}
impl History {
    pub(crate) fn current(&self) -> Option<usize> {
        self.pending.last().and_then(|pending| pending.index)
    }
    pub(crate) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }
    pub(crate) fn begin_transfer(&mut self, target: u64) {
        self.control_boundary = true;
        if target == 0 {
            self.root_exit = self.current();
        }
    }
    pub(crate) fn begin_callback(&mut self, location: TraceLocation) {
        if self.status == InstructionStatus::Incomplete {
            return;
        }
        let Some(pending) = self.pending.last_mut().filter(|pending| {
            pending.location == location && (!pending.suspended || self.control_boundary)
        }) else {
            self.status = InstructionStatus::Incomplete;
            return;
        };
        pending.suspended = true;
        self.control_boundary = true;
    }
    pub(crate) fn end_callback(&mut self) {
        if self.status != InstructionStatus::Incomplete {
            self.returning_callback = true;
        }
    }
    pub(crate) fn record_instruction(
        &mut self,
        location: TraceLocation,
        opcode: u8,
        frame: Option<FrameId>,
    ) {
        if self.status == InstructionStatus::Incomplete {
            return;
        }
        if !self.started
            || self.finished
            || self
                .pending
                .last()
                .is_some_and(|pending| !pending.suspended)
            || (self.status == InstructionStatus::Complete
                && self.admitted
                && !self.control_boundary
                && self.next != Some(location))
        {
            self.status = InstructionStatus::Incomplete;
            return;
        }
        self.control_boundary = false;
        self.admitted = true;
        let index = if self.status == InstructionStatus::Complete
            && self.steps.len() < self.config.max_steps
        {
            let index = self.steps.len();
            self.steps.push(Step {
                inputs_seen: false,
                inputs: self.registers.len()..self.registers.len(),
                outputs: self.registers.len()..self.registers.len(),
                location,
                opcode,
                parent: self.current(),
                next: None,
                outcome: InstructionOutcome::Pending,
                frame,
            });
            Some(index)
        } else {
            self.status = InstructionStatus::Truncated {
                limit: self.config.max_steps,
            };
            None
        };
        self.pending.push(PendingAttempt {
            location,
            index,
            suspended: false,
        });
    }
    pub(crate) fn complete_instruction(
        &mut self,
        location: TraceLocation,
        next: Option<TraceLocation>,
        outcome: crate::trace::TraceOutcome<'_>,
    ) {
        if self.status == InstructionStatus::Incomplete {
            return;
        }
        let Some(pending) = self.pending.pop() else {
            self.status = InstructionStatus::Incomplete;
            return;
        };
        if pending.location != location
            || (pending.suspended
                && matches!(outcome, crate::trace::TraceOutcome::Ok)
                && !self.control_boundary)
        {
            self.status = InstructionStatus::Incomplete;
            return;
        }
        let actual = match outcome {
            crate::trace::TraceOutcome::Abandoned(_) => InstructionOutcome::Abandoned,
            crate::trace::TraceOutcome::Ok => InstructionOutcome::Completed,
            crate::trace::TraceOutcome::Trap(_) => InstructionOutcome::Trapped,
            crate::trace::TraceOutcome::Unwound { target } => {
                InstructionOutcome::Unwound { target }
            }
            crate::trace::TraceOutcome::Cancelled => {
                self.status = InstructionStatus::Incomplete;
                InstructionOutcome::Pending
            }
        };
        if let Some(index) = pending.index {
            if !self.steps[index].inputs_seen && self.watch(location).is_some() {
                self.status = InstructionStatus::Incomplete;
            }
            self.steps[index].next = next;
            self.steps[index].outcome = actual;
        }
        self.next = next;
        if self.returning_callback {
            self.returning_callback = false;
            if !matches!(outcome, crate::trace::TraceOutcome::Ok) {
                self.status = InstructionStatus::Incomplete;
            }
            if let Some(parent) = self.pending.last_mut() {
                parent.suspended = false;
            } else {
                self.status = InstructionStatus::Incomplete;
            }
        }
        if matches!(
            outcome,
            crate::trace::TraceOutcome::Trap(_) | crate::trace::TraceOutcome::Cancelled
        ) {
            self.control_boundary = false;
        }
    }
}
