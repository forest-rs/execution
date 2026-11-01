// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Code-qualified evidence for a composed execution. Numeric identities are local to this record.

use super::*;
use crate::{instance::Instance, trace::TraceLocation, vm::DataWork};
use alloc::collections::BTreeMap;
use core::{
    fmt,
    hash::{Hash, Hasher},
};

/// One exact activation in a recording's execution-local control namespace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RecordedActivation {
    /// Execution-local code segment, as displayed by live inspection.
    pub segment: u64,
    /// Activation ordinal within that segment.
    pub activation: u64,
}

/// Shared aggregate identity across the owned evidence of one composed recording.
///
/// Equality includes the capture's opaque domain. Equal local [`AggregateId`] values in
/// unrelated segment arenas do not establish sharing; compare this identity instead.
#[derive(Clone)]
pub struct AggregateIdentity {
    pub(super) domain: Arc<()>,
    pub(super) ordinal: u32,
}
impl PartialEq for AggregateIdentity {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.domain, &other.domain) && self.ordinal == other.ordinal
    }
}
impl Eq for AggregateIdentity {}
impl Hash for AggregateIdentity {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.domain).hash(state);
        self.ordinal.hash(state);
    }
}
impl fmt::Debug for AggregateIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AggregateIdentity")
            .field("capture", &Arc::as_ptr(&self.domain))
            .field("ordinal", &self.ordinal)
            .finish()
    }
}

/// One requested code segment and the evidence actually observed for it.
#[derive(Debug)]
pub struct RecordedSegment<'a> {
    /// Execution-local segment identity. The root is zero.
    pub id: u64,
    /// Actual pending parent activation; absent for the root.
    pub parent: Option<RecordedActivation>,
    /// Exact immutable code definition, including requests rejected before entry.
    pub program: &'a VerifiedProgram,
    /// Entry requested in that definition.
    pub entry: Option<FuncId>,
    /// Observed segment evidence. Absent when transfer admission failed before entry.
    /// Child evidence requires the owning execution for checked replay.
    pub recording: Option<&'a Recording>,
    /// Positive evidence that the VM rejected this requested segment before entry.
    pub denied_before_entry: bool,
}

/// An actual transfer between separately retained code segments.
#[derive(Debug)]
pub struct RecordedTransfer<'a> {
    /// Transfer observation order, independent of per-segment host/callback sequence counters.
    pub index: usize,
    /// Segment whose observer admitted this event.
    pub segment: u64,
    /// Observation order within the admitting segment, shared with host and callback boundaries.
    pub sequence: usize,
    /// Exact definition interpreting `location` and `stack`. For a returned event this is
    /// the child definition; values still belong to the admitting parent's arena.
    pub program: &'a VerifiedProgram,
    /// Observed local guest ancestry at the transfer's source instruction.
    pub stack: Option<CallStack<'a>>,
    /// Source instruction interpreted by `program`.
    pub location: TraceLocation,
    /// Code-qualified transfer and its owned value evidence.
    pub action: TransferAction<'a>,
}

/// Borrowed evidence for cross-instance control. Values belong to the admitting segment arena.
#[derive(Debug)]
pub enum TransferAction<'a> {
    /// Request an independently compiled child without completing the parent host call.
    Enter {
        /// Exact pending caller.
        parent: RecordedActivation,
        /// Requested child root, including rejected entries.
        child: RecordedActivation,
        /// Arguments in the shared store.
        args: &'a [RecordedValue],
        /// Host construction already charged before staging promotion.
        work: DataWork,
        /// Promoted aggregate nodes in construction order.
        constructed: &'a [AggregateId],
    },
    /// The VM rejected a child before entry, then terminated the execution with its admission trap.
    Rejected {
        /// Admitting caller.
        parent: RecordedActivation,
        /// Requested child which did not enter.
        child: RecordedActivation,
    },
    /// Checked child return to the original host operation, without root publication.
    Returned {
        /// Returning child root.
        child: RecordedActivation,
        /// Restored parent activation.
        parent: RecordedActivation,
        /// Actual checked results, captured in the parent's arena.
        values: &'a [RecordedValue],
    },
    /// Typed non-local return initiated in a child and targeting a live ancestor segment.
    ReturnFrom {
        /// Initiating activation.
        source: RecordedActivation,
        /// Exact target activation.
        target: RecordedActivation,
        /// Target's actual suspended instruction, in its own definition.
        target_location: TraceLocation,
        /// Same-segment callback bodies abandoned in that target.
        unwound_callbacks: usize,
        /// Proposed results, before the target's contract validation.
        values: &'a [RecordedValue],
        /// Host construction already charged before staging promotion.
        work: DataWork,
        /// All promoted nodes, including unused staging.
        constructed: &'a [AggregateId],
    },
}

pub(super) struct ExecutionData {
    pub(super) segments: Vec<Segment>,
    pub(super) indices: BTreeMap<u64, usize>,
    pub(super) instances: Vec<Instance>,
    pub(super) transfers: Vec<Transfer>,
    pub(super) children: Vec<Option<Recording>>,
    pub(super) failure: Option<u64>,
}
pub(super) struct Segment {
    pub(super) id: u64,
    pub(super) parent: Option<RecordedActivation>,
    pub(super) instance: Option<usize>,
    pub(super) program: Arc<VerifiedProgram>,
    pub(super) entry: Option<FuncId>,
    pub(super) denied: bool,
}
pub(super) struct Transfer {
    pub(super) segment: u64,
    pub(super) sequence: usize,
    pub(super) location: TraceLocation,
    pub(super) frame: Option<frames::FrameId>,
    pub(super) action: Action,
}
pub(super) enum Action {
    Enter {
        parent: RecordedActivation,
        child: RecordedActivation,
        args: Range<usize>,
        work: DataWork,
        constructed: Vec<AggregateId>,
    },
    Rejected {
        parent: RecordedActivation,
        child: RecordedActivation,
    },
    Returned {
        child: RecordedActivation,
        parent: RecordedActivation,
        values: Range<usize>,
    },
    ReturnFrom {
        source: RecordedActivation,
        target: RecordedActivation,
        target_location: TraceLocation,
        unwound_callbacks: usize,
        values: Range<usize>,
        work: DataWork,
        constructed: Vec<AggregateId>,
    },
}
impl Action {
    pub(super) fn is_command(&self) -> bool {
        !matches!(self, Self::Returned { .. } | Self::Rejected { .. })
    }
    pub(super) fn construction(&self) -> (DataWork, &[AggregateId]) {
        match self {
            Self::Enter {
                work, constructed, ..
            }
            | Self::ReturnFrom {
                work, constructed, ..
            } => (*work, constructed),
            Self::Returned { .. } | Self::Rejected { .. } => (DataWork::default(), &[]),
        }
    }
}

impl ExecutionData {
    pub(super) fn index(&self, id: u64) -> Option<usize> {
        self.indices.get(&id).copied()
    }
    pub(super) fn evidence<'a>(
        &'a self,
        root: &'a Recording,
        index: usize,
    ) -> Option<&'a Recording> {
        if index == 0 {
            Some(root)
        } else {
            self.children.get(index - 1)?.as_ref()
        }
    }
}

impl Recording {
    /// Iterates the actual root and requested child segments, in admission order.
    /// A denied transfer retains its requested code but has no fabricated child entry evidence.
    pub fn segments(&self) -> impl ExactSizeIterator<Item = RecordedSegment<'_>> + '_ {
        let count = self
            .execution
            .as_ref()
            .map_or(1, |execution| execution.segments.len());
        (0..count).map(|index| {
            if let Some(execution) = &self.execution {
                let segment = &execution.segments[index];
                RecordedSegment {
                    id: segment.id,
                    parent: segment.parent,
                    program: &segment.program,
                    entry: segment.entry,
                    recording: execution.evidence(self, index),
                    denied_before_entry: segment.denied,
                }
            } else {
                RecordedSegment {
                    id: 0,
                    parent: None,
                    program: &self.program,
                    entry: self.entry.as_ref().map(|entry| entry.func),
                    recording: Some(self),
                    denied_before_entry: false,
                }
            }
        })
    }

    /// Iterates admitted cross-instance transfer observations. Host outcomes remain separately
    /// available from each segment's ordinary boundary evidence.
    pub fn transfers(&self) -> impl ExactSizeIterator<Item = RecordedTransfer<'_>> + '_ {
        self.execution
            .as_ref()
            .map_or(&[][..], |execution| execution.transfers.as_slice())
            .iter()
            .enumerate()
            .map(|(index, transfer)| {
                let execution = self.execution.as_ref().expect("transfer owner");
                let evidence = execution
                    .index(transfer.segment)
                    .and_then(|segment| execution.evidence(self, segment))
                    .expect("observed source segment");
                let action = match &transfer.action {
                    Action::Enter {
                        parent,
                        child,
                        args,
                        work,
                        constructed,
                    } => TransferAction::Enter {
                        parent: *parent,
                        child: *child,
                        args: evidence.arena.slice(args),
                        work: *work,
                        constructed,
                    },
                    Action::Rejected { parent, child } => TransferAction::Rejected {
                        parent: *parent,
                        child: *child,
                    },
                    Action::Returned {
                        child,
                        parent,
                        values,
                    } => TransferAction::Returned {
                        child: *child,
                        parent: *parent,
                        values: evidence.arena.slice(values),
                    },
                    Action::ReturnFrom {
                        source,
                        target,
                        target_location,
                        unwound_callbacks,
                        values,
                        work,
                        constructed,
                    } => TransferAction::ReturnFrom {
                        source: *source,
                        target: *target,
                        target_location: *target_location,
                        unwound_callbacks: *unwound_callbacks,
                        values: evidence.arena.slice(values),
                        work: *work,
                        constructed,
                    },
                };
                RecordedTransfer {
                    index,
                    segment: transfer.segment,
                    sequence: transfer.sequence,
                    program: if let Action::Returned { child, .. } = &transfer.action {
                        &execution.segments[execution.index(child.segment).expect("child code")]
                            .program
                    } else {
                        evidence.program()
                    },
                    stack: if let Action::Returned { child, .. } = &transfer.action {
                        execution
                            .index(child.segment)
                            .and_then(|index| execution.evidence(self, index))
                            .and_then(Self::outcome_stack)
                    } else {
                        transfer
                            .frame
                            .map(|frame| CallStack::new(&evidence.frames, frame, transfer.location))
                    },
                    location: transfer.location,
                    action,
                }
            })
    }

    /// Compares sharing across segment arenas without exposing a live VM heap handle.
    pub fn aggregate_identity(&self, aggregate: AggregateId) -> Option<AggregateIdentity> {
        Some(AggregateIdentity {
            domain: Arc::clone(&self.identity),
            ordinal: self.arena.nodes.get(aggregate.0 as usize)?.origin,
        })
    }

    /// Exact code definition interpreting the final trap's local function and instruction IDs.
    /// On a successful root return this is the root definition. A partial composed capture
    /// may lack the failing segment's identity; `None` keeps that absence explicit.
    /// `outcome_location()` and `outcome_stack()` remain local segment context, whereas
    /// the fields of `Outcome::Trapped` use this definition.
    pub fn outcome_program(&self) -> Option<&VerifiedProgram> {
        if let Some(program) = &self.outcome_definition {
            return Some(program);
        }
        if self.context_required && matches!(self.end, Some(End::Trapped(_))) {
            return None;
        }
        if let Some(execution) = &self.execution
            && execution.failure.is_none()
            && self.fault.is_some()
            && matches!(self.end, Some(End::Trapped(_)))
        {
            return None;
        }
        Some(
            self.execution
                .as_ref()
                .and_then(|execution| {
                    execution
                        .failure
                        .and_then(|id| execution.index(id))
                        .map(|index| &*execution.segments[index].program)
                })
                .unwrap_or(&self.program),
        )
    }
}
