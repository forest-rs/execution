// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Tracing hooks for `execution_tape`.
//!
//! Tracing is optional and is designed to be `no_std` friendly.
//! The VM only emits events requested by a [`TraceMask`].
//!
//! To enable tracing, pass a [`TraceMask`] and [`TraceSink`] to [`Vm::run`].
//! [`TraceMask::VALUES`] borrows values at invocation, host-call completion and root return.
//! Combine it with [`TraceMask::RUN`] to observe the final outcome; a return candidate can
//! still be rejected. The sink owns retention and recording policy.

#[cfg(doc)]
use crate::vm::Vm;

use crate::aggregates::AggHeap;
use crate::host::{HostError, SigHash, ValueRef};
use crate::program::Program;
use crate::program::{HostSigId, RegionId, SymbolId, TagId};
use crate::value::{FuncId, FuncRef, TagRef, UnitId, Value};
use crate::vm::{DataWork, Limits, TrapInfo, Units};

/// A set of trace events requested by a [`TraceSink`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TraceMask(u32);

impl core::ops::BitOr for TraceMask {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl core::ops::BitOrAssign for TraceMask {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl TraceMask {
    /// No tracing.
    pub const NONE: Self = Self(0);
    /// Trace run boundaries.
    ///
    /// Enables:
    /// - [`TraceSink::run_start`]
    /// - [`TraceSink::run_end`]
    pub const RUN: Self = Self(1 << 0);
    /// Trace each attempted instruction and its completion.
    ///
    /// Enables:
    /// - [`TraceSink::instr`]
    /// - [`TraceSink::instr_end`]
    pub const INSTR: Self = Self(1 << 1);
    /// Trace call frames.
    ///
    /// Enables (for [`ScopeKind::CallFrame`]):
    /// - [`TraceSink::scope_enter`]
    /// - [`TraceSink::scope_exit`]
    pub const CALL: Self = Self(1 << 2);
    /// Trace host calls.
    ///
    /// Enables (for [`ScopeKind::HostCall`]):
    /// - [`TraceSink::scope_enter`]
    /// - [`TraceSink::scope_exit`]
    pub const HOST: Self = Self(1 << 3);
    /// Borrow values at entry, host-call completion, guest control boundaries and root return.
    ///
    /// Enables [`TraceSink::values`] independently of scope/instruction events. Include
    /// [`Self::RUN`] to learn whether the run eventually succeeds or traps. Values and aggregate
    /// handles are borrowed only for the callback; the sink chooses what to retain.
    pub const VALUES: Self = Self(1 << 4);

    /// Borrow selected instruction inputs and local successful writes through
    /// [`TraceSink::registers`]. Source decoding and register mapping are built lazily per
    /// executed function for this run; ordinary prepared artifacts do not retain them.
    /// Use [`TraceSink::wants_registers`] to select locations before this work is performed.
    pub const REGISTERS: Self = Self(1 << 5);

    /// Returns `true` if this mask includes all bits in `other`.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
}

/// The observation boundary for instruction register values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegisterPhase {
    /// Source read operands before executing the instruction. Write-only destinations are absent.
    Inputs,
    /// Source write operands after a successful local operation. Calls and returns have no such
    /// event: dispatch does not establish caller results, and return can release a register frame.
    Outputs,
}

/// Lazy borrowed instruction operands. Iteration does not copy byte/string payloads or graphs.
/// Duplicate source operands retain their occurrence order. Effect register r0 is omitted.
pub struct RegisterValues<'a> {
    pub(crate) registers: crate::disasm::RegIter<'a>,
    pub(crate) read: &'a dyn Fn(u32) -> Option<ValueRef<'a>>,
}

impl<'a> RegisterValues<'a> {
    /// Iterates `(source register, observed value)` pairs. `None` marks an unavailable value;
    /// a sink must not invent one or treat it as an uninitialized register's default contents.
    pub fn iter(&self) -> impl Iterator<Item = (u32, Option<ValueRef<'a>>)> + '_ {
        self.registers
            .filter(|&reg| reg != 0)
            .map(|reg| (reg, (self.read)(reg)))
    }
}
impl core::fmt::Debug for RegisterValues<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RegisterValues").finish_non_exhaustive()
    }
}

/// Borrowed instruction data, retained only if the sink explicitly copies it.
pub struct RegisterEvent<'a> {
    /// Instruction being observed.
    pub location: TraceLocation,
    /// Inputs before execution or outputs after a successful local operation.
    pub phase: RegisterPhase,
    /// Source operand values, evaluated lazily by the consumer.
    pub values: RegisterValues<'a>,
    /// Heap containing observed aggregate handles. Its identity follows [`ValueEvent`].
    pub aggregates: &'a AggHeap,
}
impl core::fmt::Debug for RegisterEvent<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RegisterEvent")
            .field("location", &self.location)
            .field("phase", &self.phase)
            .field("values", &self.values)
            .field("aggregate_count", &self.aggregates.len_u32())
            .finish()
    }
}

/// The kind of scope being entered/exited.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ScopeKind {
    /// A VM call frame (function activation).
    CallFrame {
        /// Function of the frame being entered/exited, with the unit it runs.
        func: FuncRef,
        /// Caller instruction that entered this activation; `None` for the root frame.
        /// Preserved unchanged when this scope exits, including trap unwinding.
        caller: Option<TraceLocation>,
    },
    /// A VM host call.
    HostCall {
        /// Host signature identifier.
        host_sig: HostSigId,
        /// Host symbol identifier.
        symbol: SymbolId,
        /// Host signature hash carried in bytecode/program.
        sig_hash: SigHash,
    },
}

/// Outcome of a run, instruction attempt, or scope.
#[derive(Copy, Clone, Debug)]
pub enum TraceOutcome<'a> {
    /// Completed successfully.
    Ok,
    /// The invocation or an admitted host delivery was cancelled before guest completion.
    /// Host effects are not undone; abandoned deliveries produce incomplete evidence.
    /// An interrupted instruction attempt has no successful completion.
    Cancelled,
    /// This instruction or scope was abandoned by an exception that a catch clause handled.
    /// The throw itself and every activation or pending host request it passed through end
    /// this way. This is neither a successful completion nor a trap.
    Caught {
        /// Stack depth of the catching activation; the root activation has depth one.
        depth: usize,
    },
    /// This activation's continuation suspended to a handler outside it.
    ///
    /// Only call-frame scopes close this way. The activation is not finished: resuming its
    /// continuation enters a new scope for it, at its new depth, under its resumer.
    Suspended,
    /// Trapped.
    Trap(&'a TrapInfo),
}

/// An instruction boundary in an installed unit.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TraceLocation {
    /// Installed unit whose program contains [`Self::func`].
    pub unit: UnitId,
    /// Function containing the instruction.
    pub func: FuncId,
    /// Byte offset within the function.
    pub pc: u32,
}

impl TraceLocation {
    /// The same boundary within its program, without the unit that runs it.
    #[must_use]
    pub const fn code(self) -> CodeLocation {
        CodeLocation {
            func: self.func,
            pc: self.pc,
        }
    }
}

/// An instruction boundary within one program, independent of any installed unit.
///
/// Program artifacts such as source maps describe code this way; every unit installed from that
/// program shares them.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CodeLocation {
    /// Function containing the instruction.
    pub func: FuncId,
    /// Byte offset within the function.
    pub pc: u32,
}

impl From<TraceLocation> for CodeLocation {
    fn from(location: TraceLocation) -> Self {
        location.code()
    }
}

/// Raw outcome returned by the host, before the VM validates it.
///
/// A returned value can still violate the declared ABI or contain a dangling aggregate handle.
/// The extra fuel charge is observed before debit. A failed call does not charge extra fuel,
/// and partial writes to its return buffer are discarded.
#[derive(Copy, Clone, Debug)]
pub enum HostCallOutcome<'a> {
    /// The host returned values and an additional fuel charge.
    Returned {
        /// Raw host return slots, including any unwritten `Unit` slots.
        values: &'a [Value],
        /// Additional fuel requested by the host.
        extra_fuel: u64,
    },
    /// The host returned an owned operation error, borrowed for this callback.
    Failed(&'a HostError),
    /// The host raised a guest exception with [`HostError::throw`]. Payload aggregates have been
    /// adopted into the invocation heap; tag and payload are validated after observation.
    Threw {
        /// Raised VM tag, not yet checked to exist.
        tag: TagRef,
        /// The tag's ID in the calling program, if that program declares or imports it.
        local: Option<TagId>,
        /// Raw payload values.
        payload: &'a [Value],
    },
}

/// The embedding's raw answer to a yielded suspension, before the VM validates it.
///
/// Values are observed before arity, kind, payload-work and contract checks, so an invalid
/// answer is observed before the trap it causes.
#[derive(Copy, Clone, Debug)]
pub enum SuspensionAnswer<'a> {
    /// The embedding resumed the suspension with these values for the `suspend`'s results.
    Resumed {
        /// Supplied values, in the order of the tag's results.
        values: &'a [Value],
    },
    /// The embedding raised an exception at the `suspend`.
    Threw {
        /// Raised VM tag, not yet checked to exist.
        tag: TagRef,
        /// The tag's ID in the suspending program, if that program declares or imports it.
        local: Option<TagId>,
        /// Raw payload values.
        payload: &'a [Value],
    },
    /// The embedding discarded the suspension, ending the invocation with
    /// [`crate::vm::Trap::UnhandledSuspension`].
    Discarded,
}

/// Borrowed values at an execution boundary, enabled by [`TraceMask::VALUES`].
///
/// Aggregate and closure-environment handles belong to the supplied heap. Use its snapshot
/// operation to retain a valid graph; it can reject dangling handles exposed by these raw events.
/// No values are copied by the VM to construct an event. Heap identity is stable within a run
/// until return compaction or trap rollback. Do not interpret a pre-commit handle as the final
/// returned identity, or reuse a handle map across runs.
#[derive(Copy, Clone)]
pub enum ValueEvent<'a> {
    /// Embedding-requested guest reentry while a host instruction remains pending.
    /// Observed before callback target, argument or fuel validation. IDs and handles use this
    /// invocation's original code/heap. Promoted staging keeps its original handle identities.
    CallbackEnter {
        /// Host instruction whose embedding requested the callback.
        location: TraceLocation,
        /// Requested function or closure, of any installed unit.
        callee: &'a Value,
        /// Supplied user arguments, excluding a closure's hidden environment.
        args: &'a [Value],
        /// Prior host construction work promoted to the shared invocation before reentry.
        work: DataWork,
        /// Promoted aggregate handles, including values still held by the embedding.
        created: &'a core::ops::Range<u32>,
        /// Shared invocation storage after promotion.
        aggregates: &'a AggHeap,
    },
    /// A callback returned successfully to the embedding, without completing its host call.
    CallbackReturn {
        /// The callback root's actual return instruction.
        location: TraceLocation,
        /// Original pending host instruction.
        caller: TraceLocation,
        /// Contract-checked result slots. Handles still belong to this invocation.
        values: &'a [Value],
        /// Shared invocation storage; no root commit or handle compaction occurred.
        aggregates: &'a AggHeap,
    },
    /// Invocation inputs, before entry, argument, shape or execution-limit validation.
    Entry {
        /// Requested entry function, which may be absent from its unit.
        entry: FuncRef,
        /// Supplied value arguments, excluding the effect token.
        args: &'a [Value],
        /// Aggregate storage at entry. Its length distinguishes initial from run-created nodes.
        aggregates: &'a AggHeap,
        /// Limits governing this invocation.
        limits: &'a Limits,
        /// Input preparation already debited against these limits.
        work: DataWork,
        /// The VM's units and tags when the invocation starts. Installs while it is suspended
        /// are reported by [`Self::Installed`].
        units: Units<'a>,
    },
    /// A unit was installed while this invocation was suspended, through
    /// [`crate::vm::Invocation::install`] or [`crate::vm::PendingHostCall::install`].
    ///
    /// Observers that rebuild the VM, such as recordings, repeat the install at the same point.
    Installed {
        /// The new unit.
        unit: UnitId,
        /// The VM's units and tags, including the new unit.
        units: Units<'a>,
    },
    /// Raw host outcome after the call, before fuel debit and return validation.
    ///
    /// A VM denial before invoking the host produces no such event. A returned value is not
    /// evidence of successful VM installation; observe the instruction/run outcome separately.
    HostCall {
        /// Calling guest instruction.
        location: TraceLocation,
        /// Program-local import identity.
        host_sig: HostSigId,
        /// Borrowed arguments supplied to the host.
        args: &'a [ValueRef<'a>],
        /// Raw host result.
        outcome: HostCallOutcome<'a>,
        /// Host construction work already debited, including discarded staged values.
        work: DataWork,
        /// Newly adopted host aggregate handles, in construction order.
        /// Only values reachable from raw successful return slots are adopted.
        created: &'a core::ops::Range<u32>,
        /// Storage containing argument and result aggregates.
        aggregates: &'a AggHeap,
    },
    /// Newly materialized top-level outputs, before aggregate validation and compaction.
    ///
    /// The slice excludes any pre-existing caller output-buffer prefix. It may contain dangling
    /// handles and can still be rejected. Only [`TraceSink::run_end`] with [`TraceOutcome::Ok`]
    /// establishes that the candidate survived commit. Nested function returns do not emit it.
    ReturnCandidate {
        /// Root return instruction.
        location: TraceLocation,
        /// Candidate values in return-signature order.
        values: &'a [Value],
        /// Storage before return compaction changes run-local aggregate handles.
        aggregates: &'a AggHeap,
    },
    /// A `throw` materialized its payload, before payload contracts are checked.
    ///
    /// A rejected payload ends with its contract trap instead. A caught exception is followed by
    /// [`Self::Caught`]. An uncaught exception ends the run with
    /// [`crate::vm::Trap::Uncaught`]; its rollback invalidates these handles, but the trap
    /// retains a detached snapshot of the same payload.
    Thrown {
        /// Throwing instruction.
        location: TraceLocation,
        /// VM tag being raised.
        tag: TagRef,
        /// The tag's ID in the throwing program, if it declares or imports it. A guest `throw`
        /// always names its tag; a host may raise one the calling program cannot name.
        local: Option<TagId>,
        /// Payload values in tag order.
        values: &'a [Value],
        /// Storage containing payload aggregates at the throw site.
        aggregates: &'a AggHeap,
    },
    /// A `suspend` that no guest `on` clause handles was yielded to the embedding.
    ///
    /// Only invocations that opt into
    /// [`InvocationOptions::yield_suspensions`](crate::vm::InvocationOptions::yield_suspensions)
    /// yield suspensions. Payload contracts have been checked. The suspended activations then
    /// close their scopes with [`TraceOutcome::Suspended`], innermost first; the embedding's
    /// answer is reported by [`Self::SuspensionAnswered`].
    SuspensionYielded {
        /// The `suspend` instruction.
        location: TraceLocation,
        /// VM tag being suspended to.
        tag: TagRef,
        /// The tag's ID in the suspending program.
        local: TagId,
        /// Payload values in tag order, borrowed from the suspended registers.
        payload: &'a [ValueRef<'a>],
        /// Storage containing payload aggregates.
        aggregates: &'a AggHeap,
    },
    /// The embedding answered a yielded suspension, before the VM validates the answer.
    ///
    /// A resumption or raised exception then re-enters the suspended activations' scopes,
    /// outermost first, before the values are checked or the exception raised at the `suspend`.
    SuspensionAnswered {
        /// The `suspend` instruction.
        location: TraceLocation,
        /// VM tag the suspension was yielded to.
        tag: TagRef,
        /// Raw answer.
        answer: SuspensionAnswer<'a>,
        /// Storage containing answer aggregates.
        aggregates: &'a AggHeap,
    },
    /// `throw_ref` raised a caught exception again, before propagation.
    ///
    /// The payload was observed and checked by the original [`Self::Thrown`]; it is neither
    /// copied nor checked again. A catch is again reported by [`Self::Caught`].
    Rethrown {
        /// Rethrowing instruction.
        location: TraceLocation,
        /// The exception's original VM tag.
        tag: TagRef,
        /// The tag's ID in the rethrowing program, if that program declares or imports it.
        local: Option<TagId>,
        /// Zero-based ordinal of the original [`Self::Thrown`] among this invocation's raises.
        exception: usize,
        /// The original throwing instruction.
        origin: TraceLocation,
    },
    /// A catch clause selected the exception most recently thrown or rethrown, before unwinding.
    ///
    /// Scopes and instruction attempts it abandons then close with [`TraceOutcome::Caught`].
    Caught {
        /// Throwing instruction.
        location: TraceLocation,
        /// Caught VM tag.
        tag: TagRef,
        /// The tag's ID in the program of [`Self::Caught::handler`], if it declares or imports
        /// it; `None` when a catch-all clause caught a tag that program cannot name.
        local: Option<TagId>,
        /// First executable instruction of the handler, in the catching activation.
        handler: TraceLocation,
        /// Stack depth of the catching activation; the root activation has depth one.
        depth: usize,
        /// Function-local region declaring the clause.
        region: RegionId,
        /// Clause index within its region.
        clause: u32,
        /// Guest callbacks abandoned with their pending host requests.
        unwound_callbacks: usize,
    },
}

impl core::fmt::Debug for ValueEvent<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Inspect selected values without dumping unrelated backing heap contents.
        match self {
            Self::CallbackEnter {
                location,
                callee,
                args,
                work,
                created,
                aggregates,
            } => f
                .debug_struct("CallbackEnter")
                .field("location", location)
                .field("callee", callee)
                .field("args", args)
                .field("work", work)
                .field("created", created)
                .field("aggregate_count", &aggregates.len_u32())
                .finish(),
            Self::CallbackReturn {
                location,
                caller,
                values,
                aggregates,
            } => f
                .debug_struct("CallbackReturn")
                .field("location", location)
                .field("caller", caller)
                .field("values", values)
                .field("aggregate_count", &aggregates.len_u32())
                .finish(),
            Self::Installed { unit, units } => f
                .debug_struct("Installed")
                .field("unit", unit)
                .field("units", units)
                .finish(),
            Self::Entry {
                entry,
                args,
                aggregates,
                limits,
                work,
                units: _,
            } => f
                .debug_struct("Entry")
                .field("entry", entry)
                .field("args", args)
                .field("aggregate_count", &aggregates.len_u32())
                .field("limits", limits)
                .field("work", work)
                .finish(),
            Self::HostCall {
                location,
                host_sig,
                args,
                outcome,
                aggregates,
                work,
                created,
            } => f
                .debug_struct("HostCall")
                .field("location", location)
                .field("host_sig", host_sig)
                .field("args", args)
                .field("outcome", outcome)
                .field("work", work)
                .field("created", created)
                .field("aggregate_count", &aggregates.len_u32())
                .finish(),
            Self::ReturnCandidate {
                location,
                values,
                aggregates,
            } => f
                .debug_struct("ReturnCandidate")
                .field("location", location)
                .field("values", values)
                .field("aggregate_count", &aggregates.len_u32())
                .finish(),
            Self::Thrown {
                location,
                tag,
                local,
                values,
                aggregates,
            } => f
                .debug_struct("Thrown")
                .field("location", location)
                .field("tag", tag)
                .field("local", local)
                .field("values", values)
                .field("aggregate_count", &aggregates.len_u32())
                .finish(),
            Self::SuspensionYielded {
                location,
                tag,
                local,
                payload,
                aggregates,
            } => f
                .debug_struct("SuspensionYielded")
                .field("location", location)
                .field("tag", tag)
                .field("local", local)
                .field("payload", payload)
                .field("aggregate_count", &aggregates.len_u32())
                .finish(),
            Self::SuspensionAnswered {
                location,
                tag,
                answer,
                aggregates,
            } => f
                .debug_struct("SuspensionAnswered")
                .field("location", location)
                .field("tag", tag)
                .field("answer", answer)
                .field("aggregate_count", &aggregates.len_u32())
                .finish(),
            Self::Rethrown {
                location,
                tag,
                local,
                exception,
                origin,
            } => f
                .debug_struct("Rethrown")
                .field("location", location)
                .field("tag", tag)
                .field("local", local)
                .field("exception", exception)
                .field("origin", origin)
                .finish(),
            Self::Caught {
                location,
                tag,
                local,
                handler,
                depth,
                region,
                clause,
                unwound_callbacks,
            } => f
                .debug_struct("Caught")
                .field("location", location)
                .field("tag", tag)
                .field("local", local)
                .field("handler", handler)
                .field("depth", depth)
                .field("region", region)
                .field("clause", clause)
                .field("unwound_callbacks", unwound_callbacks)
                .finish(),
        }
    }
}

/// A trace sink that can receive VM events.
///
/// `Rc<RefCell<S>>` forwards all callbacks and filtering to a shared sink. This supports owned
/// observers whose evidence outlives an invocation. Release `RefCell` borrows before executing
/// or retiring the invocation; cancellation also emits observations.
pub trait TraceSink {
    /// Reports the actual event subscription once before any events for a traced invocation.
    /// This is independent of [`TraceMask::RUN`]; [`TraceMask::NONE`] emits no callbacks.
    /// The recommended [`Self::mask`] does not prove which mask the caller passed to the VM.
    fn run_mask(&mut self, _program: &Program, _mask: TraceMask) {}

    /// Selects register observations before decoding/mapping a function. Called only with
    /// [`TraceMask::REGISTERS`]. Returning false avoids constructing the event.
    fn wants_registers(&self, _location: TraceLocation, _phase: RegisterPhase) -> bool {
        true
    }

    /// Borrows the selected instruction's inputs or successful local writes. Enabled by
    /// [`TraceMask::REGISTERS`] independently of instruction/boundary tracing.
    /// No outputs are emitted for a trapping instruction, call dispatch or guest return.
    fn registers(&mut self, _program: &Program, _event: RegisterEvent<'_>) {}

    /// Returns the recommended event mask for callers to pass to the VM.
    /// The explicit mask passed to the run controls delivery.
    fn mask(&self) -> TraceMask {
        TraceMask::NONE
    }

    /// Called at the start of a VM run.
    ///
    /// Called only if the run mask includes [`TraceMask::RUN`].
    ///
    /// - `program`: program being executed (unverified container data)
    /// - `entry`: entry function and its unit
    /// - `arg_count`: number of value arguments passed (not including the effect token)
    fn run_start(&mut self, _program: &Program, _entry: FuncRef, _arg_count: usize) {}

    /// Borrows values at an execution boundary without changing execution or retaining data.
    ///
    /// Enabled only by [`TraceMask::VALUES`]. `Entry` follows `run_start` when both are enabled;
    /// the other variants precede their instruction completion. Include [`TraceMask::RUN`] for
    /// the authoritative final outcome. Missing final completion, including panic unwinding,
    /// means an incomplete observation, not success or a guest trap.
    ///
    /// These events supply recording primitives, not a replay guarantee. A recorder owns program
    /// identity, exact values, run-local aggregate sharing, host outcomes and its completeness
    /// policy. Invalid graphs and opaque host state/errors require explicit handling.
    fn values(&mut self, _program: &Program, _event: ValueEvent<'_>) {}

    /// Called before each attempted instruction, before any of its effects.
    ///
    /// Called only if the run mask includes [`TraceMask::INSTR`].
    ///
    /// - `program`: program being executed (unverified container data)
    /// - `location`: unit, function and pc (byte offset) of the instruction
    /// - `next_pc`: lexical end of this instruction, not its control-flow successor
    /// - `span_id`: best-effort span id for tracing/source mapping
    /// - `opcode`: opcode code (see [`Opcode::code`](crate::opcode::Opcode::code))
    fn instr(
        &mut self,
        _program: &Program,
        _location: TraceLocation,
        _next_pc: u32,
        _span_id: Option<u64>,
        _opcode: u16,
    ) {
    }

    /// Completes the identified outstanding [`Self::instr`] attempt.
    ///
    /// Enabled by [`TraceMask::INSTR`]. Every attempt gets exactly one completion, including
    /// instructions that trap. Fuel exhaustion before fetching an instruction creates no attempt.
    /// `next` is the actual execution successor (including call/return transitions); it is `None`
    /// for a terminal return, trap, or callback return to embedding code. `outcome` describes
    /// this instruction, not the entire run. A yielded host attempt remains outstanding while
    /// embedding-requested callbacks execute: completions nest, rather than forming pairs of
    /// adjacent events. The original host instruction is not admitted or executed twice.
    /// A call completes its dispatch when control enters the callee; the call-frame scope's exit
    /// reports whether that invocation eventually returned or trapped.
    fn instr_end(
        &mut self,
        _program: &Program,
        _location: TraceLocation,
        _next: Option<TraceLocation>,
        _outcome: TraceOutcome<'_>,
    ) {
    }

    /// Called when entering a profiling scope.
    ///
    /// Called only if the run mask includes:
    /// - [`TraceMask::CALL`] (for [`ScopeKind::CallFrame`])
    /// - [`TraceMask::HOST`] (for [`ScopeKind::HostCall`])
    ///
    /// - `program`: program being executed (unverified container data)
    /// - `kind`: the kind of scope being entered
    /// - `depth`: stack depth (frames) after entering the scope
    /// - `location`: unit, function and pc at the scope boundary
    /// - `span_id`: best-effort span id at the scope boundary
    fn scope_enter(
        &mut self,
        _program: &Program,
        _kind: ScopeKind,
        _depth: usize,
        _location: TraceLocation,
        _span_id: Option<u64>,
    ) {
    }

    /// Called when exiting a profiling scope, including unwinding after a trap.
    ///
    /// Each entered scope exits exactly once, inner scopes first. On a trap, `location` and
    /// `span_id` identify the trapping location; `kind` still identifies the scope being closed.
    /// Host success includes validation and installation of its returned values. Panic unwinding
    /// in the host or sink is outside this event contract.
    ///
    /// Called only if the run mask includes:
    /// - [`TraceMask::CALL`] (for [`ScopeKind::CallFrame`])
    /// - [`TraceMask::HOST`] (for [`ScopeKind::HostCall`])
    ///
    /// - `program`: program being executed (unverified container data)
    /// - `kind`: the kind of scope being exited
    /// - `depth`: stack depth (frames) before exiting the scope
    /// - `location`: unit, function and pc at the scope boundary
    /// - `span_id`: best-effort span id at the scope boundary
    fn scope_exit(
        &mut self,
        _program: &Program,
        _kind: ScopeKind,
        _depth: usize,
        _location: TraceLocation,
        _span_id: Option<u64>,
        _outcome: TraceOutcome<'_>,
    ) {
    }

    /// Called at the end of a VM run.
    ///
    /// Called only if the run mask includes [`TraceMask::RUN`].
    ///
    /// - `program`: program being executed (unverified container data)
    /// - `outcome`: whether the run ended successfully or trapped
    fn run_end(&mut self, _program: &Program, _outcome: TraceOutcome<'_>) {}
}

// Shared ownership keeps observations available after an owned invocation is dropped.
// Borrow only between execution calls: reentrant RefCell borrows intentionally panic.
impl<S: TraceSink + ?Sized> TraceSink for alloc::rc::Rc<core::cell::RefCell<S>> {
    fn run_mask(&mut self, program: &Program, mask: TraceMask) {
        self.as_ref().borrow_mut().run_mask(program, mask);
    }
    fn wants_registers(&self, location: TraceLocation, phase: RegisterPhase) -> bool {
        self.as_ref().borrow().wants_registers(location, phase)
    }
    fn registers(&mut self, program: &Program, event: RegisterEvent<'_>) {
        self.as_ref().borrow_mut().registers(program, event);
    }
    fn mask(&self) -> TraceMask {
        self.as_ref().borrow().mask()
    }
    fn run_start(&mut self, program: &Program, entry: FuncRef, arg_count: usize) {
        self.as_ref()
            .borrow_mut()
            .run_start(program, entry, arg_count);
    }
    fn values(&mut self, program: &Program, event: ValueEvent<'_>) {
        self.as_ref().borrow_mut().values(program, event);
    }
    fn instr(
        &mut self,
        program: &Program,
        location: TraceLocation,
        next_pc: u32,
        span_id: Option<u64>,
        opcode: u16,
    ) {
        self.as_ref()
            .borrow_mut()
            .instr(program, location, next_pc, span_id, opcode);
    }
    fn instr_end(
        &mut self,
        program: &Program,
        location: TraceLocation,
        next: Option<TraceLocation>,
        outcome: TraceOutcome<'_>,
    ) {
        self.as_ref()
            .borrow_mut()
            .instr_end(program, location, next, outcome);
    }
    fn scope_enter(
        &mut self,
        program: &Program,
        kind: ScopeKind,
        depth: usize,
        location: TraceLocation,
        span_id: Option<u64>,
    ) {
        self.as_ref()
            .borrow_mut()
            .scope_enter(program, kind, depth, location, span_id);
    }
    fn scope_exit(
        &mut self,
        program: &Program,
        kind: ScopeKind,
        depth: usize,
        location: TraceLocation,
        span_id: Option<u64>,
        outcome: TraceOutcome<'_>,
    ) {
        self.as_ref()
            .borrow_mut()
            .scope_exit(program, kind, depth, location, span_id, outcome);
    }
    fn run_end(&mut self, program: &Program, outcome: TraceOutcome<'_>) {
        self.as_ref().borrow_mut().run_end(program, outcome);
    }
}
