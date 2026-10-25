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
    /// The matching completion was not observed.
    Pending,
    /// The instruction completed, including dispatch into a guest callee.
    Completed,
    /// The instruction trapped. The final VM trap is available from the recording outcome.
    Trapped,
}

/// One attempted instruction in execution order. Locations are source byte offsets.
///
/// `next` is the actual executable successor; typed block declarations have no event.
/// Calls complete at callee entry, while returns resume the suspended caller.
#[derive(Clone, Debug)]
pub struct InstructionStep<'a> {
    /// Selected actual read operands before execution.
    pub inputs: RegisterObservations<'a>,
    /// Selected actual writes after a successful local operation. Calls/returns have none.
    pub outputs: RegisterObservations<'a>,
    /// Executed instruction location.
    pub location: TraceLocation,
    /// Source opcode byte.
    pub opcode: u8,
    /// Actual successor, absent at root return, trap or missing completion.
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
    pub(crate) pending: Option<usize>,
    pub(crate) started: bool,
    pub(crate) finished: bool,
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
            pending: None,
            started: false,
            finished: false,
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
        if self.pending.is_some() || !self.started || !self.finished || !self.mask_seen {
            self.status = InstructionStatus::Incomplete;
        }
    }
}
