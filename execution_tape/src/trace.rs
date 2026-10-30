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
use crate::program::{HostSigId, SymbolId};
use crate::value::{FuncId, Value};
use crate::vm::{DataWork, Limits, TrapInfo};

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
    /// Borrow values at entry, host-call completion and root return.
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
        /// Function id for the frame being entered/exited.
        func: FuncId,
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
    /// Trapped.
    Trap(&'a TrapInfo),
}

/// An instruction boundary in a program.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TraceLocation {
    /// Function containing the instruction.
    pub func: FuncId,
    /// Byte offset within the function.
    pub pc: u32,
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
    /// Invocation inputs, before entry, argument, shape or execution-limit validation.
    Entry {
        /// Requested entry function, which may be absent.
        entry: FuncId,
        /// Supplied value arguments, excluding the effect token.
        args: &'a [Value],
        /// Aggregate storage at entry. Its length distinguishes initial from run-created nodes.
        aggregates: &'a AggHeap,
        /// Limits governing this invocation.
        limits: &'a Limits,
        /// Input preparation already debited against these limits.
        work: DataWork,
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
}

impl core::fmt::Debug for ValueEvent<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Inspect selected values without dumping unrelated backing heap contents.
        match self {
            Self::Entry {
                entry,
                args,
                aggregates,
                limits,
                work,
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
            } => f
                .debug_struct("HostCall")
                .field("location", location)
                .field("host_sig", host_sig)
                .field("args", args)
                .field("outcome", outcome)
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
    /// - `entry`: entry function id
    /// - `arg_count`: number of value arguments passed (not including the effect token)
    fn run_start(&mut self, _program: &Program, _entry: FuncId, _arg_count: usize) {}

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
    /// - `func`: current function id
    /// - `pc`: current instruction pc (byte offset)
    /// - `next_pc`: lexical end of this instruction, not its control-flow successor
    /// - `span_id`: best-effort span id for tracing/source mapping
    /// - `opcode`: opcode byte
    fn instr(
        &mut self,
        _program: &Program,
        _func: FuncId,
        _pc: u32,
        _next_pc: u32,
        _span_id: Option<u64>,
        _opcode: u8,
    ) {
    }

    /// Completes the most recent [`Self::instr`] attempt.
    ///
    /// Enabled by [`TraceMask::INSTR`]. Every attempt gets exactly one completion, including
    /// instructions that trap. Fuel exhaustion before fetching an instruction creates no attempt.
    /// `next` is the actual execution successor (including call/return transitions); it is `None`
    /// for a terminal return or trap. `outcome` describes this instruction, not the entire run.
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
    /// - `func`: function id at the scope boundary
    /// - `pc`: program counter at the scope boundary
    /// - `span_id`: best-effort span id at the scope boundary
    fn scope_enter(
        &mut self,
        _program: &Program,
        _kind: ScopeKind,
        _depth: usize,
        _func: FuncId,
        _pc: u32,
        _span_id: Option<u64>,
    ) {
    }

    /// Called when exiting a profiling scope, including unwinding after a trap.
    ///
    /// Each entered scope exits exactly once, inner scopes first. On a trap, `func`, `pc`, and
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
    /// - `func`: function id at the scope boundary
    /// - `pc`: program counter at the scope boundary
    /// - `span_id`: best-effort span id at the scope boundary
    fn scope_exit(
        &mut self,
        _program: &Program,
        _kind: ScopeKind,
        _depth: usize,
        _func: FuncId,
        _pc: u32,
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
    fn run_start(&mut self, program: &Program, entry: FuncId, arg_count: usize) {
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
        func: FuncId,
        pc: u32,
        next_pc: u32,
        span_id: Option<u64>,
        opcode: u8,
    ) {
        self.as_ref()
            .borrow_mut()
            .instr(program, func, pc, next_pc, span_id, opcode);
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
        func: FuncId,
        pc: u32,
        span_id: Option<u64>,
    ) {
        self.as_ref()
            .borrow_mut()
            .scope_enter(program, kind, depth, func, pc, span_id);
    }
    fn scope_exit(
        &mut self,
        program: &Program,
        kind: ScopeKind,
        depth: usize,
        func: FuncId,
        pc: u32,
        span_id: Option<u64>,
        outcome: TraceOutcome<'_>,
    ) {
        self.as_ref()
            .borrow_mut()
            .scope_exit(program, kind, depth, func, pc, span_id, outcome);
    }
    fn run_end(&mut self, program: &Program, outcome: TraceOutcome<'_>) {
        self.as_ref().borrow_mut().run_end(program, outcome);
    }
}
