// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! # Recording and checked replay
//!
//! Owned in-memory VM boundary recordings and checked replay through `execution_tape::recording`.
//! This module remains `no_std + alloc` and adds no dependencies. Capture is requested at runtime;
//! ordinary VM execution retains no recordings.
//!
//! Share a prepared program in `Arc`, construct [`Recorder::new`], and pass [`TraceSink::mask`]
//! and `Some(&mut recorder)` to ordinary VM execution. Consume it with [`Recorder::finish`]
//! and inspect [`Recording::status`]. [`Recording::replay`] executes the guest
//! again in a fresh VM, checks host arguments and the final values/trap, and returns the actual
//! replay result. It invokes no live host or resource-access sink.
//!
//! Embedding-requested guest callbacks through `PendingHostCall::call_guest` are retained as
//! [`ControlCommand`]s. Replay issues those commands into the same invocation and executes the
//! guest bodies again; it checks actual callback results rather than supplying saved guest results.
//! Commands, callback returns and native host outcomes carry a shared observation sequence.
//! A callback return does not complete its parent host request or publish the root result.
//!
//! ```
//! use std::sync::Arc;
//! use execution_tape::{
//!     host::Host,
//!     recording::{RecordLimits, Recorder, Recording},
//!     trace::TraceSink,
//!     value::{FuncId, Value},
//!     verifier::VerifiedProgram,
//!     vm::{Limits, Vm},
//! };
//!
//! fn capture<H: Host>(
//!     program: Arc<VerifiedProgram>, entry: FuncId, host: H, args: &[Value],
//! ) -> Result<Recording, Box<dyn std::error::Error>> {
//!     let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
//!     // Capture successful returns and guest traps alike.
//!     let _live_result = Vm::new(host, Limits::default())
//!         .run(&program, entry, args, recorder.mask(), Some(&mut recorder));
//!     let recording = recorder.finish();
//!     recording.status().map_err(Clone::clone)?;
//!     let _checked_outcome = recording.replay()?;
//!     Ok(recording)
//! }
//! ```
//!
//! Recorded values expose function/import identities, host arguments and raw outcomes, exact
//! float bits, decimal scales, and aggregate graphs. Nodes have recording-local IDs: shared
//! references stay shared and distinct equal nodes stay distinct. `aggregate(id)` borrows one
//! node's immediate contents. Equal string/byte payloads share owned contents. The recorder
//! retains only observed graph nodes and shares the immutable prepared program.
//!
//! The recommended mask also captures guest activations through `CALL`, including exact caller
//! instruction locations. Each host observation and final outcome borrows an innermost-to-root
//! `CallStack`; suspended callers point at the instruction that entered their child. A parent-linked
//! arena shares ancestry across observations without copying a stack per call. It retains every
//! entered activation, including ones with no host calls, up to `RecordLimits::call_frames`.
//! `RecordStats::call_frames` counts this storage. Trap ancestry is pinned before VM unwinding.
//! Missing call callbacks, rejected entry and failed capture leave stack data explicitly unavailable.
//! Replay checks captured activation order, caller locations and boundary ancestry as well as values.
//!
//! Replay restores observed initial nodes before execution and adopted host-produced nodes at
//! their recorded boundary, preserving construction order, sharing and exact work debits.
//! Discarded host staging spends work without retaining nodes. Guest-created nodes are matched
//! through boundary values and allocation ordinals relative to the initial heap. VM heaps append
//! immutable nodes until root commit; this lets replay resolve a host return even when that node
//! was never an entry argument. Final heap compaction does not change recording identities.
//!
//! Capture limits bound value/node/host-call/activation counts and copied payload bytes. They do not measure
//! vector capacity, allocator overhead, the shared program or opaque custom-error payload sizes.
//! Exceeding a budget stops value/graph retention while live execution continues. The final VM
//! trap is still retained when its payload budget permits. Missing entry/completion,
//! invalid aggregate references, program mismatch and recorder reuse mark a capture incomplete.
//! The original invocation can still return or trap normally. Partial captures remain inspectable;
//! replay rejects them before running guest code.
//!
//! These recordings are in-memory artifacts. Custom errors retain their typed owned payloads;
//! object/function handles retain their original domains. External object state, host effects and
//! resource accesses are not reconstructed. No portable archive, cross-platform float guarantee,
//! is implied.
//!
//! Run `cargo run -p execution_tape_wind_tunnel --bin record_replay` from the workspace for an
//! executable capture/drop/replay example. Optional arguments select host-call count, shared graph
//! depth and byte-payload size. `cargo bench -p execution_tape_wind_tunnel --bench recording`
//! measures ordinary execution, capture/freeze/drop and fresh replay/drop separately.
//!
//! ## Optional execution explanations
//!
//! Configure [`Recorder::with_instructions`] with [`InstructionCapture`]
//! before running, and pass its recommended mask. Default recorders retain no instruction history
//! and request no register inspection. `instructions()` iterates actual attempts in execution order;
//! `instruction(index)` directly accesses one attempt, its actual successor and guest ancestry.
//! Typed block headers have no execution event. Calls complete at callee entry; returns resume the
//! suspended caller. Callback instructions link to their still-outstanding parent host attempt
//! through `InstructionStep::parent_attempt`; completion order can differ from admission order.
//! A callback return has no guest successor while embedding code decides what to do next.
//! Fuel exhaustion before fetching an instruction creates no attempted step.
//!
//! `instruction_status()` is independent of boundary `status()`: a truncated instruction prefix
//! can coexist with complete boundary data and successful replay. Replay checks retained instruction
//! locations, opcodes, successors, outcomes and watched values as well as ordinary boundary evidence.
//! A missing callback or completion reports incomplete history; it never invents successful execution.
//!
//! Each `RegisterWatch` selects one source location and virtual registers. Inputs are borrowed
//! before execution; successful local writes are borrowed afterward. Call dispatch and guest return
//! have no output-register event. Unwatched values and non-operands remain unknown. Before/after
//! aggregate observations preserve their separate identities, even when a destination aliases an
//! input. The VM builds optional source-to-bank maps only for requested functions, for that run;
//! no observation maps are retained in ordinary prepared programs. Watched values use the existing
//! value/aggregate/payload budgets; exhausting one still makes boundary capture incomplete.
//!
//! Trace forwarding wrappers must forward [`TraceSink::run_mask`] as well as instruction/value
//! callbacks. The VM reports the actual nonempty subscription before event delivery, independently
//! of `RUN`; `NONE` still emits no callbacks. History uses that evidence to distinguish zero attempts
//! from disabled callbacks. A recommended mask alone does not establish which events were enabled.

mod capture;
mod control;
mod frames;
mod instructions;
mod replay;
mod values;

use crate::host::HostError;
use crate::program::HostSigId;
use crate::trace::TraceLocation;
use crate::value::FuncId;
use crate::verifier::VerifiedProgram;
use crate::vm::{Limits, TrapInfo};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::Range;

pub use capture::Recorder;
pub use control::{ControlAction, ControlCommand, GuestReturn};
pub use frames::CallStack;
pub use instructions::{
    InstructionCapture, InstructionOutcome, InstructionStatus, InstructionStep,
    RegisterObservations, RegisterWatch,
};
pub use replay::{Divergence, ReplayBoundary, ReplayError, ReplayOutcome};
pub use values::{Aggregate, AggregateId, RecordedValue};

#[cfg(doc)]
use crate::trace::TraceSink;

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
    /// Maximum embedding control commands, including rejected guest callback requests.
    pub control_commands: usize,
    /// Maximum entered guest activations, including ones with no host observations.
    /// The effective limit is capped at `u32::MAX` for compact recording-local identities.
    pub call_frames: usize,
    /// Maximum copied bytes in distinct byte/string contents and trap symbol text.
    pub payload_bytes: usize,
}

impl Default for RecordLimits {
    fn default() -> Self {
        Self {
            values: 1_000_000,
            aggregates: 100_000,
            host_calls: 10_000,
            control_commands: 10_000,
            call_frames: 100_000,
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
    /// Embedding control commands.
    ControlCommands,
    /// Entered guest activations.
    CallFrames,
    /// Copied byte/string contents and trap symbol text.
    PayloadBytes,
}

/// Why a recording cannot be replayed as a complete invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureFault {
    /// The sink did not observe invocation values.
    MissingEntry,
    /// The invocation never returned or trapped: completion was missing or execution was cancelled.
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
    /// Input preparation already debited before guest execution began.
    pub work: crate::vm::DataWork,
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
    /// Zero-based observation order shared with control commands and callback returns.
    pub sequence: usize,
    /// Calling guest instruction.
    pub location: TraceLocation,
    /// Program-local import identity.
    pub import: HostSigId,
    /// Captured arguments.
    pub args: &'a [RecordedValue],
    /// Host construction work, including staged values discarded before completion.
    pub work: crate::vm::DataWork,
    /// Adopted host aggregate nodes, in their original construction order.
    pub constructed: &'a [AggregateId],
    /// Raw host outcome, before VM validation.
    pub outcome: HostOutcome<'a>,
    /// Actual guest callers, innermost first, when call-frame callbacks were captured.
    /// `None` means unavailable, rather than an empty or inferred stack.
    pub stack: Option<CallStack<'a>>,
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
    /// Retained optional instruction attempts; zero when history was not requested.
    pub instructions: usize,
    /// Captured value occurrences.
    pub values: usize,
    /// Discovered aggregate nodes; an incomplete capture can contain unfinished nodes.
    pub aggregates: usize,
    /// Retained host-call observations.
    pub host_calls: usize,
    /// Retained embedding control commands.
    pub control_commands: usize,
    /// Retained entered guest activations, shared by boundary stacks.
    pub call_frames: usize,
    /// Copied byte/string contents and trap symbol text, counted once per retained allocation.
    pub payload_bytes: usize,
}

#[derive(Debug)]
struct Entry {
    func: FuncId,
    args: Range<usize>,
    limits: Limits,
    work: crate::vm::DataWork,
}
#[derive(Debug)]
struct Call {
    sequence: usize,
    work: crate::vm::DataWork,
    constructed: Vec<AggregateId>,
    frame: Option<frames::FrameId>,
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
    history: Option<alloc::boxed::Box<instructions::History>>,
    program: Arc<VerifiedProgram>,
    arena: values::Arena,
    initial: Vec<AggregateId>,
    entry: Option<Entry>,
    calls: Vec<Call>,
    commands: Vec<control::Command>,
    frames: Vec<frames::Frame>,
    outcome_frame: Option<frames::FrameId>,
    candidate: Option<(TraceLocation, Range<usize>)>,
    end: Option<End>,
    fault: Option<CaptureFault>,
    fault_location: Option<TraceLocation>,
}

impl Recording {
    /// Returns optional instruction-history completeness. `None` means it was not requested.
    /// Boundary capture status is reported separately by [`Self::status`].
    pub fn instruction_status(&self) -> Option<InstructionStatus> {
        self.history.as_ref().map(|history| history.status)
    }

    /// Iterates retained attempts without reconstructing a path from static control flow.
    /// Check [`Self::instruction_status`] before treating the prefix as the whole execution.
    pub fn instructions(&self) -> impl ExactSizeIterator<Item = InstructionStep<'_>> {
        self.history
            .as_ref()
            .map_or(&[][..], |h| h.steps.as_slice())
            .iter()
            .map(|step| InstructionStep {
                inputs: RegisterObservations {
                    entries: &self.history.as_ref().expect("history").registers
                        [step.inputs.clone()],
                    arena: &self.arena,
                },
                outputs: RegisterObservations {
                    entries: &self.history.as_ref().expect("history").registers
                        [step.outputs.clone()],
                    arena: &self.arena,
                },
                location: step.location,
                parent_attempt: step.parent,
                opcode: step.opcode,
                next: step.next,
                outcome: step.outcome,
                stack: step
                    .frame
                    .map(|frame| CallStack::new(&self.frames, frame, step.location)),
            })
    }

    /// Looks up one retained attempt by its execution ordinal, without scanning history.
    pub fn instruction(&self, index: usize) -> Option<InstructionStep<'_>> {
        let step = self.history.as_ref()?.steps.get(index)?;
        Some(InstructionStep {
            inputs: RegisterObservations {
                entries: &self.history.as_ref()?.registers[step.inputs.clone()],
                arena: &self.arena,
            },
            outputs: RegisterObservations {
                entries: &self.history.as_ref()?.registers[step.outputs.clone()],
                arena: &self.arena,
            },
            location: step.location,
            parent_attempt: step.parent,
            opcode: step.opcode,
            next: step.next,
            outcome: step.outcome,
            stack: step
                .frame
                .map(|frame| CallStack::new(&self.frames, frame, step.location)),
        })
    }
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
            work: entry.work,
        })
    }

    /// Iterates retained host calls in execution order.
    pub fn host_calls(&self) -> impl ExactSizeIterator<Item = HostCall<'_>> {
        self.calls.iter().map(|call| HostCall {
            sequence: call.sequence,
            location: call.location,
            stack: call
                .frame
                .map(|top| CallStack::new(&self.frames, top, call.location)),
            import: call.import,
            work: call.work,
            constructed: &call.constructed,
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

    /// Iterates captured embedding commands in admission order, without expanding value graphs.
    /// Sequence numbers interleave commands, callback returns and host outcomes.
    pub fn control_commands(&self) -> impl ExactSizeIterator<Item = ControlCommand<'_>> {
        self.commands.iter().map(|command| command.view(self))
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

    /// Returns the final return/trap instruction when the VM outcome was captured.
    ///
    /// Rejected invocations can report the VM's synthetic entry location. A root return's
    /// location refers to its precommit candidate, before aggregate handles are compacted.
    pub fn outcome_location(&self) -> Option<TraceLocation> {
        match self.end.as_ref()? {
            End::Returned => self.candidate.as_ref().map(|(location, _)| *location),
            End::Trapped(trap) => Some(TraceLocation {
                func: trap.func,
                pc: trap.pc,
            }),
        }
    }

    /// Borrows the guest call stack at the captured final return or trap, innermost first.
    ///
    /// `None` means no stack was observed: call callbacks may have been omitted, invocation
    /// validation may have rejected entry, completion may be missing, or capture may have failed.
    /// Trap stacks are retained before VM unwinding removes activations. No inactive callee
    /// is synthesized for a rejected call. This contains guest frames only.
    pub fn outcome_stack(&self) -> Option<CallStack<'_>> {
        Some(CallStack::new(
            &self.frames,
            self.outcome_frame?,
            self.outcome_location()?,
        ))
    }

    /// Returns used storage counts, not allocator traffic, capacity or RSS.
    pub fn stats(&self) -> RecordStats {
        RecordStats {
            instructions: self.history.as_ref().map_or(0, |h| h.steps.len()),
            values: self.arena.values.len(),
            aggregates: self.arena.nodes.len(),
            host_calls: self.calls.len(),
            control_commands: self.commands.len(),
            call_frames: self.frames.len(),
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
