// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Owned in-memory recordings of VM boundary values and replay through a fresh interpreter.
//!
//! Share a prepared program in an [`Arc`], create a [`Recorder`], and pass its recommended
//! [`TraceSink::mask`] to the ordinary VM run. [`Recorder::finish`] freezes the observations.
//! [`Recording::replay`] runs the guest again with recorded host outcomes, checks host arguments
//! and the resulting values/trap, and returns the actual replay result. It invokes no live host.
//!
//! A recording owns observed values and reachable aggregate graphs, preserving sharing and
//! distinct equal nodes across boundary events. It shares the original prepared program rather
//! than cloning its immutable storage. Capture budgets constrain retained items and copied
//! byte/string payloads. Check [`Recording::status`]: missing completion, invalid graphs and
//! exhausted budgets are capture failures, separate from recorded guest traps.
//!
//! This crate is `no_std + alloc`. Recordings are in-memory artifacts, not a portable archive.
//! Typed custom errors retain their original owned payloads. Object handles remain tokens in
//! their original host domain; replay does not restore external objects, repeat host effects,
//! or replay resource-access side effects. Instruction/register history is not recorded here.

#![no_std]

extern crate alloc;

mod capture;
mod replay;
mod values;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::Range;
use execution_tape::host::HostError;
use execution_tape::program::HostSigId;
use execution_tape::trace::TraceLocation;
use execution_tape::value::FuncId;
use execution_tape::verifier::VerifiedProgram;
use execution_tape::vm::{Limits, TrapInfo};

pub use capture::Recorder;
pub use replay::{Divergence, ReplayBoundary, ReplayError, ReplayOutcome};
pub use values::{Aggregate, AggregateId, RecordedValue};

#[cfg(doc)]
use execution_tape::trace::TraceSink;

/// Limits on capture-owned items and copied byte/string payloads.
///
/// Counts bound used items; vector capacity, indexes and allocator overhead are additional.
/// The shared prepared program and opaque custom-error payloads are not copied or charged.
#[derive(Clone, Debug)]
pub struct RecordLimits {
    /// Maximum captured value occurrences, including roots and aggregate fields.
    pub values: usize,
    /// Maximum distinct observed aggregate nodes.
    pub aggregates: usize,
    /// Maximum host-call observations.
    pub host_calls: usize,
    /// Maximum copied bytes in distinct byte/string contents and trap symbol text.
    pub payload_bytes: usize,
}

impl Default for RecordLimits {
    fn default() -> Self {
        Self {
            values: 1_000_000,
            aggregates: 100_000,
            host_calls: 10_000,
            payload_bytes: 64 * 1024 * 1024,
        }
    }
}

/// The capture budget that was exhausted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureResource {
    /// Captured value occurrences.
    Values,
    /// Distinct aggregate nodes.
    Aggregates,
    /// Host-call observations.
    HostCalls,
    /// Copied byte/string contents and trap symbol text.
    PayloadBytes,
}

/// Why a recording cannot be replayed as a complete invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureFault {
    /// The sink did not observe invocation values.
    MissingEntry,
    /// The sink did not observe the final run outcome.
    MissingCompletion,
    /// One recorder was used for more than one invocation.
    MultipleRuns,
    /// The VM ran a different prepared program from the recorder's source.
    ProgramMismatch,
    /// A configured capture budget was exhausted.
    Budget {
        /// Exhausted resource.
        resource: CaptureResource,
        /// Configured item or byte limit.
        limit: usize,
    },
    /// An observed aggregate reference could not be retained.
    InvalidAggregate {
        /// Raw source handle, valid only in the observed invocation's heap domain.
        handle: u32,
    },
    /// Boundary callbacks were missing or out of order.
    InvalidSequence,
}

impl core::fmt::Display for CaptureFault {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "incomplete capture: {self:?}")
    }
}
impl core::error::Error for CaptureFault {}

/// Invocation inputs, borrowed from a recording.
#[derive(Debug)]
pub struct Invocation<'a> {
    /// Requested entry function, including rejected entries.
    pub entry: FuncId,
    /// Captured entry values in argument order.
    pub args: &'a [RecordedValue],
    /// Execution limits used by the original invocation.
    pub limits: &'a Limits,
}

/// Recorded return values and extra fuel, or the original typed host error.
#[derive(Debug)]
pub enum HostOutcome<'a> {
    /// Raw host return slots before VM ABI validation.
    Returned {
        /// Captured values in return-slot order.
        values: &'a [RecordedValue],
        /// Extra fuel requested by the host.
        extra_fuel: u64,
    },
    /// Owned operation failure borrowed from this recording.
    Failed(&'a HostError),
}

/// One host-call observation, without expanding its aggregate graph.
#[derive(Debug)]
pub struct HostCall<'a> {
    /// Calling guest instruction.
    pub location: TraceLocation,
    /// Program-local import identity.
    pub import: HostSigId,
    /// Captured arguments.
    pub args: &'a [RecordedValue],
    /// Raw host outcome, before VM validation.
    pub outcome: HostOutcome<'a>,
}

/// Authoritative final VM outcome of the original invocation.
#[derive(Debug)]
pub enum Outcome<'a> {
    /// Committed return values, interpreted through this recording's graph IDs.
    Returned(&'a [RecordedValue]),
    /// Original VM trap, with its typed error payload intact.
    Trapped(&'a TrapInfo),
}

/// Used capture-owned storage, excluding the shared program and opaque error payloads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordStats {
    /// Captured value occurrences.
    pub values: usize,
    /// Discovered aggregate nodes; an incomplete capture can contain unfinished nodes.
    pub aggregates: usize,
    /// Retained host-call observations.
    pub host_calls: usize,
    /// Copied byte/string contents and trap symbol text, counted once per retained allocation.
    pub payload_bytes: usize,
}

#[derive(Debug)]
struct Entry {
    func: FuncId,
    args: Range<usize>,
    limits: Limits,
}
#[derive(Debug)]
struct Call {
    location: TraceLocation,
    import: HostSigId,
    args: Range<usize>,
    outcome: CallOutcome,
}
#[derive(Debug)]
enum CallOutcome {
    Returned { values: Range<usize>, fuel: u64 },
    Failed(HostError),
}
#[derive(Debug)]
enum End {
    Returned,
    Trapped(TrapInfo),
}

/// Immutable observations of one invocation, including an explicit completeness result.
///
/// Construction indexes are discarded when the recorder finishes. Aggregate IDs belong to
/// this recording, not to a VM heap. Incomplete recordings remain inspectable, but replay rejects
/// them before executing guest code. Callback arguments and outcome payloads are never formatted.
pub struct Recording {
    program: Arc<VerifiedProgram>,
    arena: values::Arena,
    initial: Vec<AggregateId>,
    entry: Option<Entry>,
    calls: Vec<Call>,
    candidate: Option<(TraceLocation, Range<usize>)>,
    end: Option<End>,
    fault: Option<CaptureFault>,
    fault_location: Option<TraceLocation>,
}

impl Recording {
    /// Returns whether all required boundary data and the final outcome were captured.
    pub fn status(&self) -> Result<(), &CaptureFault> {
        self.fault.as_ref().map_or(Ok(()), Err)
    }

    /// Returns the instruction where capture first failed, when one was available.
    /// Entry and invocation-protocol failures have no instruction location.
    pub fn fault_location(&self) -> Option<TraceLocation> {
        self.fault_location
    }

    /// Borrows the shared prepared program that gives function/import IDs their meaning.
    pub fn program(&self) -> &VerifiedProgram {
        &self.program
    }

    /// Borrows invocation values and limits, if captured.
    pub fn invocation(&self) -> Option<Invocation<'_>> {
        self.entry.as_ref().map(|entry| Invocation {
            entry: entry.func,
            args: self.arena.slice(&entry.args),
            limits: &entry.limits,
        })
    }

    /// Iterates retained host calls in execution order.
    pub fn host_calls(&self) -> impl ExactSizeIterator<Item = HostCall<'_>> {
        self.calls.iter().map(|call| HostCall {
            location: call.location,
            import: call.import,
            args: self.arena.slice(&call.args),
            outcome: match &call.outcome {
                CallOutcome::Returned { values, fuel } => HostOutcome::Returned {
                    values: self.arena.slice(values),
                    extra_fuel: *fuel,
                },
                CallOutcome::Failed(error) => HostOutcome::Failed(error),
            },
        })
    }

    /// Borrows one complete aggregate node without expanding other nodes.
    pub fn aggregate(&self, id: AggregateId) -> Option<Aggregate<'_>> {
        self.arena.aggregate(id)
    }

    /// Borrows the authoritative VM outcome, when it was retained.
    ///
    /// A capture fault can coexist with a VM trap. Inspect [`Self::status`] separately.
    pub fn outcome(&self) -> Option<Outcome<'_>> {
        match self.end.as_ref()? {
            End::Returned => self
                .candidate
                .as_ref()
                .map(|(_, values)| Outcome::Returned(self.arena.slice(values))),
            End::Trapped(error) => Some(Outcome::Trapped(error)),
        }
    }

    /// Returns used storage counts, not allocator traffic, capacity or RSS.
    pub fn stats(&self) -> RecordStats {
        RecordStats {
            values: self.arena.values.len(),
            aggregates: self.arena.nodes.len(),
            host_calls: self.calls.len(),
            payload_bytes: self.arena.payload_bytes,
        }
    }
}

impl core::fmt::Debug for Recording {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Recording")
            .field("status", &self.status())
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}
