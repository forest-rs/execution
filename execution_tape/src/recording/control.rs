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
/// callback returns; a parent host outcome can follow child callbacks.
#[derive(Debug)]
pub struct ControlCommand<'a> {
    /// Zero-based observation order shared with host outcomes and callback returns.
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
    /// End this invocation without a guest result or foreign target interpretation.
    Abandon {
        /// Admitted host construction work discarded by the command.
        work: DataWork,
    },
    /// Return supplied values from an exact live activation, abandoning its active callees.
    ReturnFrom {
        /// Invocation-local target activation ordinal, never a function ID.
        target: u64,
        /// Target frame's actual suspended call/host instruction, independent of the initiator.
        target_location: TraceLocation,
        /// Callback bodies abandoned by the transfer, including a callback-root target.
        unwound_callbacks: usize,
        /// Raw supplied values, validated by the VM against the target's result contracts.
        values: &'a [RecordedValue],
        /// Host construction work promoted before admission.
        work: DataWork,
        /// All promoted nodes in construction order, including unused staging.
        constructed: &'a [AggregateId],
    },
    /// Run a same-program guest function or closure without completing the pending host call.
    CallGuest {
        /// Requested target, observed before admission/contract validation.
        callee: &'a RecordedValue,
        /// Supplied user arguments, excluding the hidden closure environment.
        args: &'a [RecordedValue],
        /// Prior host construction work promoted before callback admission.
        work: DataWork,
        /// Promoted nodes in original construction order, including otherwise unused staging.
        constructed: &'a [AggregateId],
        /// Actual normal guest return. `None` also occurs after rejection, trap, cancellation
        /// or a subsequent non-local transfer; inspect commands and the invocation outcome separately.
        returned: Option<GuestReturn<'a>>,
    },
}

/// A guest callback's actual return to embedding code, without root heap commit.
#[derive(Debug)]
pub struct GuestReturn<'a> {
    /// Zero-based observation order shared with host outcomes and control commands.
    pub sequence: usize,
    /// Callback root's return instruction.
    pub location: TraceLocation,
    /// Checked guest result slots, independent of the pending host's result signature.
    pub values: &'a [RecordedValue],
    /// Actual returning guest ancestry, when captured.
    pub stack: Option<CallStack<'a>>,
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
    Abandon {
        work: DataWork,
    },
    ReturnFrom {
        target: u64,
        target_location: TraceLocation,
        unwound_callbacks: usize,
        values: Range<usize>,
        work: DataWork,
        constructed: Vec<AggregateId>,
    },
    CallGuest {
        callee: usize,
        args: Range<usize>,
        work: DataWork,
        constructed: Vec<AggregateId>,
        returned: Option<Returned>,
    },
}
#[derive(Debug)]
pub(super) struct Returned {
    pub(super) sequence: usize,
    pub(super) location: TraceLocation,
    pub(super) values: Range<usize>,
    pub(super) frame: Option<FrameId>,
}
impl Command {
    pub(super) fn view<'a>(&'a self, recording: &'a Recording) -> ControlCommand<'a> {
        let action = match &self.action {
            Action::Abandon { work } => ControlAction::Abandon { work: *work },
            Action::ReturnFrom {
                target,
                target_location,
                unwound_callbacks,
                values,
                work,
                constructed,
            } => ControlAction::ReturnFrom {
                target: *target,
                target_location: *target_location,
                unwound_callbacks: *unwound_callbacks,
                values: recording.arena.slice(values),
                work: *work,
                constructed,
            },
            Action::CallGuest {
                callee,
                args,
                work,
                constructed,
                returned,
            } => ControlAction::CallGuest {
                callee: &recording.arena.values[*callee],
                args: recording.arena.slice(args),
                work: *work,
                constructed,
                returned: returned.as_ref().map(|returned| GuestReturn {
                    sequence: returned.sequence,
                    location: returned.location,
                    values: recording.arena.slice(&returned.values),
                    stack: returned
                        .frame
                        .map(|frame| CallStack::new(&recording.frames, frame, returned.location)),
                }),
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
            Self::Abandon { work } => (*work, &[]),
            Self::CallGuest {
                work, constructed, ..
            }
            | Self::ReturnFrom {
                work, constructed, ..
            } => (*work, constructed),
        }
    }
}
