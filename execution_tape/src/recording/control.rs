// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Embedding control commands are evidence separate from native host outcomes.

use super::{AggregateId, CallStack, RecordedValue, Recording, frames::FrameId};
use crate::{trace::TraceLocation, vm::DataWork};
use alloc::vec::Vec;
use core::ops::Range;

/// One embedding command issued while a guest host instruction was pending.
///
/// Commands are ordered by admission. Sequence numbers interleave them with host outcomes and
/// transfers; a parent host outcome can follow child guest computations.
#[derive(Debug)]
pub struct ControlCommand<'a> {
    /// Zero-based observation order shared with host outcomes and control transfers.
    pub sequence: usize,
    /// Pending host instruction whose embedding issued the command.
    pub location: TraceLocation,
    /// Guest ancestry at command admission, innermost first, when captured.
    pub stack: Option<CallStack<'a>>,
    /// Requested operation and its separately observed guest result.
    pub action: ControlAction<'a>,
}

/// An embedding control operation, with values owned by its recording.
#[derive(Debug)]
pub enum ControlAction<'a> {
    /// Return supplied values from an exact live activation, abandoning its active callees.
    ReturnFrom {
        /// Invocation-local target activation ordinal, never a function ID.
        target: u64,
        /// Target frame's actual suspended call/host instruction, independent of the initiator.
        target_location: TraceLocation,

        /// Raw supplied values, validated by the VM against the target's result contracts.
        values: &'a [RecordedValue],
        /// Host construction work promoted before admission.
        work: DataWork,
        /// All promoted nodes in construction order, including unused staging.
        constructed: &'a [AggregateId],
    },
    /// Request a qualified guest function or closure without completing the pending host call.
    /// Actual code entry and checked return are reported by the execution transfer journal.
    CallGuest {
        /// Requested target, observed before admission/contract validation.
        callee: &'a RecordedValue,
        /// Supplied user arguments, excluding the hidden closure environment.
        args: &'a [RecordedValue],
        /// Prior host construction work promoted before callback admission.
        work: DataWork,
        /// Promoted nodes in original construction order, including otherwise unused staging.
        constructed: &'a [AggregateId],
    },
}

#[derive(Debug)]
pub(super) struct Command {
    pub(super) sequence: usize,
    pub(super) location: TraceLocation,
    pub(super) frame: Option<FrameId>,
    pub(super) action: Action,
}
#[derive(Debug)]
pub(super) enum Action {
    ReturnFrom {
        target: u64,
        target_location: TraceLocation,
        values: Range<usize>,
        work: DataWork,
        constructed: Vec<AggregateId>,
    },
    CallGuest {
        callee: usize,
        args: Range<usize>,
        work: DataWork,
        constructed: Vec<AggregateId>,
    },
}
impl Command {
    pub(super) fn view<'a>(&'a self, recording: &'a Recording) -> ControlCommand<'a> {
        let action = match &self.action {
            Action::ReturnFrom {
                target,
                target_location,
                values,
                work,
                constructed,
            } => ControlAction::ReturnFrom {
                target: *target,
                target_location: *target_location,
                values: recording.arena.slice(values),
                work: *work,
                constructed,
            },
            Action::CallGuest {
                callee,
                args,
                work,
                constructed,
            } => ControlAction::CallGuest {
                callee: &recording.arena.values[*callee],
                args: recording.arena.slice(args),
                work: *work,
                constructed,
            },
        };
        ControlCommand {
            sequence: self.sequence,
            location: self.location,
            stack: self
                .frame
                .map(|frame| CallStack::new(&recording.frames, frame, self.location)),
            action,
        }
    }
}

impl Action {
    pub(super) fn construction(&self) -> (DataWork, &[AggregateId]) {
        match self {
            Self::CallGuest {
                work, constructed, ..
            }
            | Self::ReturnFrom {
                work, constructed, ..
            } => (*work, constructed),
        }
    }
}
