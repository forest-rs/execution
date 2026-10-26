// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Interpreter for `execution_tape` bytecode (draft).
//!
//! The VM executes programs with explicit limits (fuel, call depth, host calls).
//!
//! The VM executes [`VerifiedProgram`]s only.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use crate::aggregates::{AggError, AggHeap, CheckError, ContractScratch};
use crate::arena::{BytesHandle, PayloadSource, StrHandle, ValueArena, ValueSources};
use crate::contract::{Contract, ContractPosition, ContractViolation};
use crate::host::{AccessSink, Host, HostContext, HostError, ValueRef};
use crate::program::ValueType;
use crate::program::{ConstEntry, Function, Program};
use crate::program_memory::{Checkpoint, ProgramMemory};
use crate::trace::{
    HostCallOutcome, ScopeKind, TraceLocation, TraceMask, TraceOutcome, TraceSink, ValueEvent,
};
use crate::typed::{
    AggReg, BoolReg, BytesReg, ClosureReg, DecimalReg, ExecFunc, ExecInstr, F64Reg, FuncReg,
    I64Reg, ObjReg, StrReg, U64Reg, UnitReg, VReg, VRegSlice,
};
use crate::value::{AggHandle, Closure, Decimal, FuncId, Obj, ObjHandle, Value};
use crate::verifier::VerifiedProgram;

mod observations;
#[cfg(test)]
mod payload_tests;

/// Execution limits for a VM run.
#[derive(Clone, Debug)]
pub struct Limits {
    /// Instruction budget. Each instruction costs 1 by default; host calls may charge extra.
    pub fuel: u64,
    /// Maximum call depth (frames).
    pub max_call_depth: usize,
    /// Maximum host calls.
    pub max_host_calls: u64,
    /// Maximum aggregate contract work per run: visited value/contract pairs and child edges.
    /// Separate from guest instruction fuel; shared subgraphs reuse successful proofs.
    pub max_contract_work: u64,
    /// Maximum logical byte/string work per run (64 MiB by default).
    ///
    /// Charges copied bytes for concatenation, slicing, string-to-bytes conversion, and each
    /// owned payload materialization (including aggregate fields and root returns). Equal-length
    /// comparisons charge one operand's byte length even if they differ early; unequal lengths
    /// cost zero. Bytes-to-string conversion charges twice its input length for copying and
    /// UTF-8 validation, before either operation. Direct successful host return slots containing
    /// bytes/strings charge their lengths before adoption, after all return kinds are validated.
    ///
    /// Borrowed arguments/constants/projections and guest-call transfers cost zero. This is not
    /// a heap or RSS limit: aggregate slots, frames, storage of retained results, nested host aggregate
    /// payloads, host allocations/effects, preparation, tracing/recording, and graph-adapter
    /// imports are outside this budget. Set `u64::MAX` for the maximum supported budget.
    pub max_payload_work: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            fuel: 1_000_000,
            max_call_depth: 256,
            max_host_calls: 1_000_000,
            max_contract_work: 1_000_000,
            max_payload_work: 64 * 1024 * 1024,
        }
    }
}

/// A runtime trap.
#[derive(Clone, Debug)]
pub enum Trap {
    /// Fuel limit exceeded.
    FuelExceeded,
    /// Call depth limit exceeded.
    CallDepthExceeded,
    /// Host call count limit exceeded.
    HostCallLimitExceeded,
    /// A payload operation would exceed the run's logical byte/string work budget.
    PayloadWorkExceeded {
        /// Configured logical byte limit.
        limit: u64,
        /// Successfully charged work before this rejected operation.
        used: u64,
        /// Additional logical bytes requested; rejected work is not charged.
        requested: u64,
    },
    /// A payload size or logical work calculation cannot be represented.
    PayloadSizeOverflow,
    /// Attempted to access an invalid `pc` / instruction boundary.
    InvalidPc,
    /// A register was out of bounds.
    RegOutOfBounds,
    /// A constant pool index was out of bounds.
    ConstOutOfBounds,
    /// Type mismatch (dynamic execution of invalid or unverified bytecode).
    TypeMismatch {
        /// Expected value type.
        expected: ValueType,
        /// Actual value type.
        actual: ValueType,
    },
    /// An indirect target has the right storage kinds but different compositional contracts.
    CallableSignatureMismatch {
        /// Expected program-local indirect signature.
        signature: crate::program::CallSigId,
        /// Selected guest function.
        function: FuncId,
    },
    /// Aggregate heap error.
    AggError(AggError),
    /// A concrete argument or result violates a compositional guest contract.
    ContractMismatch {
        /// Function whose signature is being checked.
        function: FuncId,
        /// Argument or result position.
        position: ContractPosition,
        /// Nested expected/observed disagreement.
        error: ContractViolation,
    },
    /// The run exhausted its aggregate contract validation work budget.
    ContractWorkExceeded {
        /// Function whose boundary was being validated.
        function: FuncId,
        /// Argument or result whose validation exhausted the budget.
        position: ContractPosition,
        /// Configured work limit for this run.
        limit: u64,
    },
    /// A type id immediate was out of bounds.
    TypeIdOutOfBounds,
    /// An element type id immediate was out of bounds.
    ElemTypeIdOutOfBounds,
    /// Immediate/provided arity mismatch.
    ArityMismatch,
    /// Host call failed.
    HostCallFailed {
        /// Host-call symbol.
        symbol: Box<str>,
        /// Signature hash carried by the host signature entry.
        sig_hash: crate::host::SigHash,
        /// Error returned by the host.
        error: HostError,
    },
    /// Host returned the wrong number of values.
    HostReturnArityMismatch {
        /// Expected number of return values.
        expected: u32,
        /// Actual number of return values.
        actual: u32,
    },
    /// Integer cast overflow (e.g. `u64_to_i64` out of range).
    IntCastOverflow,
    /// Decimal scales did not match where required (e.g. `dec_add`/`dec_sub`).
    DecimalScaleMismatch,
    /// Decimal arithmetic overflowed (`mantissa` or `scale`).
    DecimalOverflow,
    /// Integer divide-by-zero.
    DivByZero,
    /// Signed integer division overflowed (`i64::MIN / -1`).
    IntDivOverflow,
    /// Float to int conversion encountered NaN or infinity.
    FloatToIntInvalid,
    /// Index out of bounds (bytes/string ops).
    IndexOutOfBounds,
    /// String slice indices were not on UTF-8 character boundaries.
    StrNotCharBoundary,
    /// Invalid UTF-8 (runtime conversion).
    InvalidUtf8,
    /// Explicit trap instruction.
    TrapCode(u32),
}

impl fmt::Display for Trap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FuelExceeded => write!(f, "fuel limit exceeded"),
            Self::CallDepthExceeded => write!(f, "call depth limit exceeded"),
            Self::HostCallLimitExceeded => write!(f, "host call limit exceeded"),
            Self::PayloadWorkExceeded {
                limit,
                used,
                requested,
            } => write!(
                f,
                "payload work limit exceeded: {used} of {limit} logical bytes used; operation requires {requested} more; increase Limits::max_payload_work or reduce payload work"
            ),
            Self::PayloadSizeOverflow => write!(f, "payload size or work calculation overflowed"),
            Self::InvalidPc => write!(f, "invalid pc"),
            Self::RegOutOfBounds => write!(f, "register out of bounds"),
            Self::ConstOutOfBounds => write!(f, "constant out of bounds"),
            Self::TypeMismatch { expected, actual } => {
                write!(f, "type mismatch (expected {expected:?}, got {actual:?})")
            }
            Self::CallableSignatureMismatch {
                signature,
                function,
            } => write!(
                f,
                "function #{} does not match indirect signature #{}",
                function.0, signature.0
            ),
            Self::AggError(e) => write!(f, "aggregate error: {e}"),
            Self::ContractMismatch {
                function,
                position,
                error,
            } => {
                write!(f, "function #{}: ", function.0)?;
                error.fmt_at(f, *position)
            }
            Self::ContractWorkExceeded {
                function,
                position,
                limit,
            } => write!(
                f,
                "aggregate contract work limit {limit} exceeded in function #{} at {position}",
                function.0
            ),
            Self::TypeIdOutOfBounds => write!(f, "type id out of bounds"),
            Self::ElemTypeIdOutOfBounds => write!(f, "elem type id out of bounds"),
            Self::ArityMismatch => write!(f, "arity mismatch"),
            Self::HostCallFailed {
                symbol,
                sig_hash,
                error,
            } => write!(
                f,
                "host call failed: host_call={symbol} sig_hash={}: {error}",
                sig_hash.0
            ),
            Self::HostReturnArityMismatch { expected, actual } => {
                write!(
                    f,
                    "host return arity mismatch (expected {expected}, got {actual})"
                )
            }
            Self::IntCastOverflow => write!(f, "int cast overflow"),
            Self::DecimalScaleMismatch => write!(f, "decimal scale mismatch"),
            Self::DecimalOverflow => write!(f, "decimal overflow"),
            Self::DivByZero => write!(f, "divide by zero"),
            Self::IntDivOverflow => write!(f, "integer division overflow"),
            Self::FloatToIntInvalid => write!(f, "float to int invalid"),
            Self::IndexOutOfBounds => write!(f, "index out of bounds"),
            Self::StrNotCharBoundary => write!(f, "string slice not on char boundary"),
            Self::InvalidUtf8 => write!(f, "invalid utf-8"),
            Self::TrapCode(code) => write!(f, "trap({code})"),
        }
    }
}

impl core::error::Error for Trap {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::HostCallFailed { error, .. } => Some(error),
            Self::AggError(error) => Some(error),
            Self::ContractMismatch { error, .. } => Some(error),
            _ => None,
        }
    }
}

/// A trap annotated with location information.
#[derive(Clone, Debug)]
pub struct TrapInfo {
    /// Function id.
    pub func: FuncId,
    /// Byte offset pc.
    pub pc: u32,
    /// Best-effort span id for tracing/source mapping.
    pub span_id: Option<u64>,
    /// Trap kind.
    pub trap: Trap,
}

impl fmt::Display for TrapInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.span_id {
            Some(span) => write!(
                f,
                "trap at f{} pc={} span={span}: {}",
                self.func.0, self.pc, self.trap
            ),
            None => write!(f, "trap at f{} pc={}: {}", self.func.0, self.pc, self.trap),
        }
    }
}

impl core::error::Error for TrapInfo {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        Some(&self.trap)
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
struct RegBase {
    /// Base offset for the `Unit` register bank.
    unit: usize,
    /// Base offset for the `bool` register bank.
    bools: usize,
    /// Base offset for the `i64` register bank.
    i64s: usize,
    /// Base offset for the `u64` register bank.
    u64s: usize,
    /// Base offset for the `f64` register bank.
    f64s: usize,
    /// Base offset for the `Decimal` register bank.
    decimals: usize,
    /// Base offset for the bytes-handle register bank.
    bytes: usize,
    /// Base offset for the string-handle register bank.
    strs: usize,
    /// Base offset for the host object register bank.
    objs: usize,
    /// Base offset for the aggregate-handle register bank.
    aggs: usize,
    /// Base offset for the function id register bank.
    funcs: usize,
    /// Base offset for the closure register bank.
    closures: usize,
}

#[derive(Clone, Debug)]
struct Frame {
    /// Current function id.
    func: FuncId,
    /// Current program counter (byte offset).
    pc: u32,
    /// Current instruction index within the verified instruction list.
    instr_ix: usize,
    /// Total byte length of the function bytecode stream.
    byte_len: u32,
    /// Base offsets into each register bank for this frame's registers.
    base: RegBase,
    /// Return continuation for this frame.
    ///
    /// This is `None` for the entry frame.
    ///
    /// Design note: we intentionally keep this metadata compact and avoid storing duplicated
    /// caller frame state (`func`, `base`, `pc`, `instr_ix`) because the caller frame remains on
    /// stack while the callee executes.
    return_to: Option<ReturnTo>,
}

#[derive(Copy, Clone, Debug)]
struct ReturnTo {
    /// Destination return-register slice declared by the caller's `call`.
    dst_rets: VRegSlice,
}

/// Per-run execution context for [`Vm`].
///
/// This holds all transient state required to execute a single entrypoint (register file, call
/// stack, and arena-backed temporaries). Keeping it separate from [`Vm`] makes it possible to
/// reuse allocations across runs and is a stepping stone towards re-entrant execution.
#[derive(Debug, Default)]
pub struct ExecutionContext {
    /// Remaining instruction budget for the current run.
    fuel: u64,
    /// Number of host calls performed so far in the current run.
    host_calls: u64,
    /// Successfully charged logical byte/string work and the current run's limit.
    payload_work: u64,
    payload_limit: u64,

    /// Arena backing bytes/strings and other out-of-line temporaries.
    arena: ValueArena,
    contract_scratch: ContractScratch,

    // Split register file by class (SoA).
    /// Unit registers (`()`), stored as a compact `u32` to allow "uninit" tracking.
    units: Vec<u32>,
    /// Boolean registers.
    bools: Vec<bool>,
    /// `i64` registers.
    i64s: Vec<i64>,
    /// `u64` registers.
    u64s: Vec<u64>,
    /// `f64` registers.
    f64s: Vec<f64>,
    /// Decimal registers.
    decimals: Vec<Decimal>,
    /// Bytes registers (handles into `arena`).
    bytes: Vec<BytesHandle>,
    /// String registers (handles into `arena`).
    strs: Vec<StrHandle>,
    /// Host object registers.
    objs: Vec<Obj>,
    /// Aggregate registers (handles into the aggregate heap).
    aggs: Vec<AggHandle>,
    /// Function registers.
    funcs: Vec<FuncId>,
    /// Closure registers.
    closures: Vec<Closure>,

    /// Call stack.
    frames: Vec<Frame>,
}

impl ExecutionContext {
    /// Creates an empty per-run execution context.
    ///
    /// Embedders may reuse an [`ExecutionContext`] across multiple [`Vm::run_with_ctx`] calls to
    /// amortize allocations.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Drops transient run state and computed payloads while retaining reusable capacities.
    ///
    /// Returned values and retained VM aggregates are independent of this context. Call this
    /// between runs when an idle embedding should release payloads from its last invocation;
    /// the next run also clears the context automatically.
    pub fn clear(&mut self) {
        self.frames.clear();
        self.units.clear();
        self.bools.clear();
        self.i64s.clear();
        self.u64s.clear();
        self.f64s.clear();
        self.decimals.clear();
        self.bytes.clear();
        self.strs.clear();
        self.objs.clear();
        self.aggs.clear();
        self.funcs.clear();
        self.closures.clear();
        self.arena.clear();
        self.contract_scratch.reset();

        self.fuel = 0;
        self.host_calls = 0;
        self.payload_work = 0;
        self.payload_limit = 0;
    }

    fn reset(&mut self, fuel: u64, payload_limit: u64) {
        self.clear();
        self.fuel = fuel;
        self.payload_limit = payload_limit;
    }

    fn charge_payload_bytes(&mut self, len: usize, factor: u64) -> Result<(), Trap> {
        let requested = u64::try_from(len)
            .ok()
            .and_then(|len| len.checked_mul(factor))
            .ok_or(Trap::PayloadSizeOverflow)?;
        if requested > self.payload_limit - self.payload_work {
            return Err(Trap::PayloadWorkExceeded {
                limit: self.payload_limit,
                used: self.payload_work,
                requested,
            });
        }
        self.payload_work += requested;
        Ok(())
    }
}

/// A simple register VM.
pub struct Vm<H: Host> {
    host: H,
    limits: Limits,

    /// Retained aggregates and temporary allocations from the active run.
    memory: ProgramMemory,
}

impl<H: Host> fmt::Debug for Vm<H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Vm")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

/// Per-run trace state; scope storage allocates only when scope tracing is enabled.
///
/// Keeping this out of [`ExecutionContext`] avoids storing borrowed trace state in the long-lived
/// per-run context while still flattening VM hot-loop tracing code.
struct TraceCtx<'a> {
    mask: TraceMask,
    sink: Option<&'a mut dyn TraceSink>,
    scopes: Vec<(ScopeKind, usize)>,
    pending: Option<TraceLocation>,
}

impl<'a> TraceCtx<'a> {
    #[inline]
    fn new(mask: TraceMask, sink: Option<&'a mut dyn TraceSink>) -> Self {
        Self {
            mask,
            sink,
            scopes: Vec::new(),
            pending: None,
        }
    }

    #[inline(always)]
    fn enabled(&self, mask: TraceMask) -> bool {
        self.mask.contains(mask) && self.sink.is_some()
    }

    #[inline]
    fn run_start(&mut self, program: &Program, entry: FuncId, arg_count: usize) {
        if self.mask != TraceMask::NONE
            && let Some(sink) = self.sink.as_mut()
        {
            sink.run_mask(program, self.mask);
        }
        if self.enabled(TraceMask::RUN)
            && let Some(t) = self.sink.as_mut()
        {
            let t: &mut dyn TraceSink = &mut **t;
            t.run_start(program, entry, arg_count);
        }
    }

    #[inline]
    fn run_end(&mut self, program: &Program, outcome: TraceOutcome<'_>) {
        if self.enabled(TraceMask::RUN)
            && let Some(t) = self.sink.as_mut()
        {
            let t: &mut dyn TraceSink = &mut **t;
            t.run_end(program, outcome);
        }
    }

    #[inline]
    fn values<'event>(&mut self, program: &Program, event: impl FnOnce() -> ValueEvent<'event>) {
        if self.enabled(TraceMask::VALUES)
            && let Some(sink) = self.sink.as_mut()
        {
            sink.values(program, event());
        }
    }

    #[inline]
    fn instr_if_enabled(
        &mut self,
        program: &Program,
        func: FuncId,
        pc: u32,
        next_pc: u32,
        span_id: Option<u64>,
        opcode: u8,
    ) {
        if self.enabled(TraceMask::INSTR)
            && let Some(t) = self.sink.as_mut()
        {
            let t: &mut dyn TraceSink = &mut **t;
            self.pending = Some(TraceLocation { func, pc });
            t.instr(program, func, pc, next_pc, span_id, opcode);
        }
    }

    #[inline]
    fn call_scope_enter_if_enabled(
        &mut self,
        program: &Program,
        depth: usize,
        func: FuncId,
        pc: u32,
        span_id: Option<u64>,
        caller: Option<TraceLocation>,
    ) {
        if self.enabled(TraceMask::CALL)
            && let Some(t) = self.sink.as_mut()
        {
            let t: &mut dyn TraceSink = &mut **t;
            self.scopes
                .push((ScopeKind::CallFrame { func, caller }, depth));
            t.scope_enter(
                program,
                ScopeKind::CallFrame { func, caller },
                depth,
                func,
                pc,
                span_id,
            );
        }
    }

    #[inline]
    fn call_scope_exit_if_enabled(
        &mut self,
        program: &Program,
        depth: usize,
        func: FuncId,
        pc: u32,
        span_id: Option<u64>,
    ) {
        if self.enabled(TraceMask::CALL)
            && let Some(t) = self.sink.as_mut()
        {
            let t: &mut dyn TraceSink = &mut **t;
            let (kind, _) = self.scopes.pop().expect("entered call frame");
            t.scope_exit(program, kind, depth, func, pc, span_id, TraceOutcome::Ok);
        }
    }

    #[inline]
    fn host_scope_enter_if_enabled(
        &mut self,
        program: &Program,
        host_sig: crate::program::HostSigId,
        symbol: crate::program::SymbolId,
        sig_hash: crate::host::SigHash,
        depth: usize,
        func: FuncId,
        pc: u32,
        span_id: Option<u64>,
    ) {
        if self.enabled(TraceMask::HOST)
            && let Some(t) = self.sink.as_mut()
        {
            let t: &mut dyn TraceSink = &mut **t;
            self.scopes.push((
                ScopeKind::HostCall {
                    host_sig,
                    symbol,
                    sig_hash,
                },
                depth,
            ));
            t.scope_enter(
                program,
                ScopeKind::HostCall {
                    host_sig,
                    symbol,
                    sig_hash,
                },
                depth,
                func,
                pc,
                span_id,
            );
        }
    }

    #[inline]
    fn host_scope_exit_if_enabled(
        &mut self,
        program: &Program,
        host_sig: crate::program::HostSigId,
        symbol: crate::program::SymbolId,
        sig_hash: crate::host::SigHash,
        depth: usize,
        func: FuncId,
        pc: u32,
        span_id: Option<u64>,
    ) {
        if self.enabled(TraceMask::HOST)
            && let Some(t) = self.sink.as_mut()
        {
            let t: &mut dyn TraceSink = &mut **t;
            self.scopes.pop();
            t.scope_exit(
                program,
                ScopeKind::HostCall {
                    host_sig,
                    symbol,
                    sig_hash,
                },
                depth,
                func,
                pc,
                span_id,
                TraceOutcome::Ok,
            );
        }
    }
    fn instr_end(
        &mut self,
        program: &Program,
        next: Option<TraceLocation>,
        outcome: TraceOutcome<'_>,
    ) {
        if let Some(location) = self.pending.take()
            && let Some(sink) = self.sink.as_mut()
        {
            sink.instr_end(program, location, next, outcome);
        }
    }

    fn finish(&mut self, program: &Program, outcome: TraceOutcome<'_>) {
        if let TraceOutcome::Trap(error) = outcome {
            // A host ABI failure belongs to the host scope, before the instruction completes.
            if matches!(self.scopes.last(), Some((ScopeKind::HostCall { .. }, _))) {
                let (kind, depth) = self.scopes.pop().expect("open host scope");
                if let Some(sink) = self.sink.as_mut() {
                    sink.scope_exit(
                        program,
                        kind,
                        depth,
                        error.func,
                        error.pc,
                        error.span_id,
                        outcome,
                    );
                }
            }
            self.instr_end(program, None, outcome);
            while let Some((kind, depth)) = self.scopes.pop() {
                if let Some(sink) = self.sink.as_mut() {
                    sink.scope_exit(
                        program,
                        kind,
                        depth,
                        error.func,
                        error.pc,
                        error.span_id,
                        outcome,
                    );
                }
            }
        }
        debug_assert!(
            self.pending.is_none(),
            "every instruction attempt must complete"
        );
        debug_assert!(self.scopes.is_empty(), "every entered scope must close");
    }
}

trait TracePolicy {
    type RegisterState: Default;

    #[inline(always)]
    fn registers(
        _register_state: &mut Self::RegisterState,
        _trace: &mut TraceCtx<'_>,
        _program: &Program,
        _ctx: &ExecutionContext,
        _func: FuncId,
        _pc: u32,
        _base: RegBase,
        _sources: ValueSources<'_>,
        _phase: crate::trace::RegisterPhase,
    ) {
    }
    #[inline(always)]
    fn values<'a>(
        _trace: &mut TraceCtx<'_>,
        _program: &Program,
        _event: impl FnOnce() -> ValueEvent<'a>,
    ) {
    }

    #[inline(always)]
    fn instr_end(_trace: &mut TraceCtx<'_>, _program: &Program, _next: Option<TraceLocation>) {}

    fn instr(
        trace: &mut TraceCtx<'_>,
        program: &Program,
        func: FuncId,
        pc: u32,
        next_pc: u32,
        span_id: Option<u64>,
        opcode: u8,
    );

    fn call_scope_enter(
        trace: &mut TraceCtx<'_>,
        program: &Program,
        depth: usize,
        func: FuncId,
        pc: u32,
        span_id: Option<u64>,
        caller: Option<TraceLocation>,
    );

    fn call_scope_exit(
        trace: &mut TraceCtx<'_>,
        program: &Program,
        depth: usize,
        func: FuncId,
        pc: u32,
        span_id: Option<u64>,
    );

    fn host_scope_enter(
        trace: &mut TraceCtx<'_>,
        program: &Program,
        host_sig: crate::program::HostSigId,
        symbol: crate::program::SymbolId,
        sig_hash: crate::host::SigHash,
        depth: usize,
        func: FuncId,
        pc: u32,
        span_id: Option<u64>,
    );

    fn host_scope_exit(
        trace: &mut TraceCtx<'_>,
        program: &Program,
        host_sig: crate::program::HostSigId,
        symbol: crate::program::SymbolId,
        sig_hash: crate::host::SigHash,
        depth: usize,
        func: FuncId,
        pc: u32,
        span_id: Option<u64>,
    );
}

struct NoTracePolicy;
struct WithTracePolicy;

impl TracePolicy for NoTracePolicy {
    type RegisterState = ();

    #[inline(always)]
    fn instr(
        _trace: &mut TraceCtx<'_>,
        _program: &Program,
        _func: FuncId,
        _pc: u32,
        _next_pc: u32,
        _span_id: Option<u64>,
        _opcode: u8,
    ) {
    }

    #[inline(always)]
    fn call_scope_enter(
        _trace: &mut TraceCtx<'_>,
        _program: &Program,
        _depth: usize,
        _func: FuncId,
        _pc: u32,
        _span_id: Option<u64>,
        _caller: Option<TraceLocation>,
    ) {
    }

    #[inline(always)]
    fn call_scope_exit(
        _trace: &mut TraceCtx<'_>,
        _program: &Program,
        _depth: usize,
        _func: FuncId,
        _pc: u32,
        _span_id: Option<u64>,
    ) {
    }

    #[inline(always)]
    fn host_scope_enter(
        _trace: &mut TraceCtx<'_>,
        _program: &Program,
        _host_sig: crate::program::HostSigId,
        _symbol: crate::program::SymbolId,
        _sig_hash: crate::host::SigHash,
        _depth: usize,
        _func: FuncId,
        _pc: u32,
        _span_id: Option<u64>,
    ) {
    }

    #[inline(always)]
    fn host_scope_exit(
        _trace: &mut TraceCtx<'_>,
        _program: &Program,
        _host_sig: crate::program::HostSigId,
        _symbol: crate::program::SymbolId,
        _sig_hash: crate::host::SigHash,
        _depth: usize,
        _func: FuncId,
        _pc: u32,
        _span_id: Option<u64>,
    ) {
    }
}

impl TracePolicy for WithTracePolicy {
    type RegisterState = observations::RegisterState;

    #[inline(always)]
    fn registers(
        register_state: &mut Self::RegisterState,
        trace: &mut TraceCtx<'_>,
        program: &Program,
        ctx: &ExecutionContext,
        func: FuncId,
        pc: u32,
        base: RegBase,
        sources: ValueSources<'_>,
        phase: crate::trace::RegisterPhase,
    ) {
        observations::emit(
            register_state,
            trace,
            program,
            ctx,
            func,
            pc,
            base,
            sources,
            phase,
        );
    }
    #[inline(always)]
    fn values<'a>(
        trace: &mut TraceCtx<'_>,
        program: &Program,
        event: impl FnOnce() -> ValueEvent<'a>,
    ) {
        trace.values(program, event);
    }

    #[inline(always)]
    fn instr_end(trace: &mut TraceCtx<'_>, program: &Program, next: Option<TraceLocation>) {
        trace.instr_end(program, next, TraceOutcome::Ok);
    }

    #[inline(always)]
    fn instr(
        trace: &mut TraceCtx<'_>,
        program: &Program,
        func: FuncId,
        pc: u32,
        next_pc: u32,
        span_id: Option<u64>,
        opcode: u8,
    ) {
        trace.instr_if_enabled(program, func, pc, next_pc, span_id, opcode);
    }

    #[inline(always)]
    fn call_scope_enter(
        trace: &mut TraceCtx<'_>,
        program: &Program,
        depth: usize,
        func: FuncId,
        pc: u32,
        span_id: Option<u64>,
        caller: Option<TraceLocation>,
    ) {
        trace.call_scope_enter_if_enabled(program, depth, func, pc, span_id, caller);
    }

    #[inline(always)]
    fn call_scope_exit(
        trace: &mut TraceCtx<'_>,
        program: &Program,
        depth: usize,
        func: FuncId,
        pc: u32,
        span_id: Option<u64>,
    ) {
        trace.call_scope_exit_if_enabled(program, depth, func, pc, span_id);
    }

    #[inline(always)]
    fn host_scope_enter(
        trace: &mut TraceCtx<'_>,
        program: &Program,
        host_sig: crate::program::HostSigId,
        symbol: crate::program::SymbolId,
        sig_hash: crate::host::SigHash,
        depth: usize,
        func: FuncId,
        pc: u32,
        span_id: Option<u64>,
    ) {
        trace.host_scope_enter_if_enabled(
            program, host_sig, symbol, sig_hash, depth, func, pc, span_id,
        );
    }

    #[inline(always)]
    fn host_scope_exit(
        trace: &mut TraceCtx<'_>,
        program: &Program,
        host_sig: crate::program::HostSigId,
        symbol: crate::program::SymbolId,
        sig_hash: crate::host::SigHash,
        depth: usize,
        func: FuncId,
        pc: u32,
        span_id: Option<u64>,
    ) {
        trace.host_scope_exit_if_enabled(
            program, host_sig, symbol, sig_hash, depth, func, pc, span_id,
        );
    }
}

impl<H: Host> Vm<H> {
    /// Creates a new VM with `host` and `limits`.
    #[must_use]
    pub fn new(host: H, limits: Limits) -> Self {
        Self {
            host,
            limits,
            memory: ProgramMemory::new(),
        }
    }

    /// Executes `program` starting at `entry` with `args` as value arguments.
    ///
    /// Convention:
    /// - `r0` is the effect token (initialized to `Unit` at entry).
    /// - value args are placed in `r1..=rN` where `N = entry.arg_count`.
    ///
    /// Tracing is controlled by `trace_mask`; pass `None` for `trace` to disable tracing.
    ///
    /// Even for verified bytecode, host calls are a trust boundary, so the VM validates host ABI
    /// conformance (return arity and return types) at runtime.
    ///
    /// An incorrect entry argument count traps with [`Trap::ArityMismatch`]. An unknown entry
    /// function traps with [`Trap::InvalidPc`].
    ///
    /// Only new aggregates reachable from returned values survive a successful run. Traps
    /// discard all aggregates allocated during the run. Previously retained values stay valid.
    /// Host argument handles are borrowed for the call; use [`HostContext::retain`] for durable
    /// host state. A returned dangling aggregate or closure environment traps at `ret`.
    pub fn run(
        &mut self,
        program: &VerifiedProgram,
        entry: FuncId,
        args: &[Value],
        trace_mask: TraceMask,
        trace: Option<&mut dyn TraceSink>,
    ) -> Result<Vec<Value>, TrapInfo> {
        let mut ctx = ExecutionContext::new();
        let mut outputs = Vec::new();
        self.run_with_ctx(
            &mut ctx,
            program,
            entry,
            args,
            &mut outputs,
            trace_mask,
            trace,
            None,
        )?;
        Ok(outputs)
    }

    /// Executes a program using reusable scratch and appending returns to `outputs`.
    ///
    /// Clear `outputs` before calling to replace previous results while keeping its capacity.
    /// A trap preserves its original contents. Successful calls append in signature order;
    /// aggregate handles remain valid in this VM's heap until explicitly cleared.
    /// [`Self::run`] delegates to this execution path. Entry validation follows its rules.
    pub fn run_with_ctx(
        &mut self,
        ctx: &mut ExecutionContext,
        program: &VerifiedProgram,
        entry: FuncId,
        args: &[Value],
        outputs: &mut Vec<Value>,
        trace_mask: TraceMask,
        trace: Option<&mut dyn TraceSink>,
        access: Option<&mut dyn AccessSink>,
    ) -> Result<(), TrapInfo> {
        let original_len = outputs.len();
        let checkpoint = self.memory.checkpoint();
        let program_ref = program.program();
        let mut trace = TraceCtx::new(trace_mask, trace);
        trace.run_start(program_ref, entry, args.len());

        // Keep two interpreter-loop instantiations: one no-trace fast path and one trace-capable
        // path with per-mask checks, instead of specializing every trace-mask combination.
        let trace_active = trace.enabled(TraceMask::INSTR)
            || trace.enabled(TraceMask::CALL)
            || trace.enabled(TraceMask::HOST)
            || trace.enabled(TraceMask::VALUES)
            || trace.enabled(TraceMask::REGISTERS);

        let result = if trace_active {
            trace.values(program_ref, || ValueEvent::Entry {
                entry,
                args,
                aggregates: self.memory.aggs(),
                limits: &self.limits,
            });
            self.run_body::<WithTracePolicy>(
                ctx, program, entry, args, outputs, checkpoint, &mut trace, access,
            )
        } else {
            self.run_body::<NoTracePolicy>(
                ctx, program, entry, args, outputs, checkpoint, &mut trace, access,
            )
        };

        let outcome = match &result {
            Ok(_) => TraceOutcome::Ok,
            Err(e) => TraceOutcome::Trap(e),
        };
        if result.is_err() {
            ctx.contract_scratch.clear_proofs();
            self.memory.rollback(checkpoint);
            outputs.truncate(original_len);
        }
        trace.finish(program_ref, outcome);
        trace.run_end(program_ref, outcome);

        result
    }

    fn run_body<P: TracePolicy>(
        &mut self,
        ctx: &mut ExecutionContext,
        program: &VerifiedProgram,
        entry: FuncId,
        args: &[Value],
        outputs: &mut Vec<Value>,
        checkpoint: Checkpoint,
        trace: &mut TraceCtx<'_>,
        mut access: Option<&mut dyn AccessSink>,
    ) -> Result<(), TrapInfo> {
        let output_start = outputs.len();
        let program_ref = program.program();
        let max_call_depth = self.limits.max_call_depth;
        let max_host_calls = self.limits.max_host_calls;
        let mut register_state = P::RegisterState::default();
        ctx.reset(self.limits.fuel, self.limits.max_payload_work);

        let entry_fn = program_ref
            .functions
            .get(entry.0 as usize)
            .ok_or_else(|| ctx.trap(entry, 0, None, Trap::InvalidPc))?;
        validate_entry_args(program_ref, entry_fn, args)
            .map_err(|t| ctx.trap(entry, 0, None, t))?;

        let entry_vf = program
            .verified(entry)
            .ok_or_else(|| ctx.trap(entry, 0, None, Trap::InvalidPc))?;
        if max_call_depth == 0 {
            return Err(ctx.trap(
                entry,
                0,
                entry_vf.span_at_ix(0).map(|id| id.get()),
                Trap::CallDepthExceeded,
            ));
        }
        let entry_base = ctx.alloc_frame(entry_vf);
        ctx.init_args(entry_base, entry_vf, args)
            .map_err(|t| ctx.trap(entry, 0, None, t))?;
        self.check_arg_contracts(ctx, program_ref, entry, entry_vf, entry_base)
            .map_err(|t| ctx.trap(entry, 0, entry_vf.span_at_ix(0).map(|id| id.get()), t))?;
        ctx.frames.push(Frame {
            func: entry,
            pc: entry_vf.pc_at_ix(0).unwrap_or(0),
            instr_ix: 0,
            byte_len: entry_vf.byte_len,
            base: entry_base,
            return_to: None,
        });

        P::call_scope_enter(
            trace,
            program_ref,
            ctx.frames.len(),
            entry,
            entry_vf.pc_at_ix(0).unwrap_or(0),
            entry_vf.span_at_ix(0).map(|id| id.get()),
            None,
        );

        loop {
            if ctx.fuel == 0 {
                return Err(ctx.trap(
                    ctx.cur_func(),
                    ctx.cur_pc(),
                    ctx.cur_span(program_ref),
                    Trap::FuelExceeded,
                ));
            }
            ctx.fuel -= 1;

            let frame_index = ctx
                .frames
                .len()
                .checked_sub(1)
                .ok_or_else(|| ctx.trap(entry, 0, None, Trap::InvalidPc))?;

            let (func_id, pc, instr_ix, base, _byte_len) = {
                let f = &ctx.frames[frame_index];
                (f.func, f.pc, f.instr_ix, f.base, f.byte_len)
            };

            let vf = program.verified(func_id).ok_or_else(|| {
                let span_id = ctx.span_at(program_ref, func_id, pc);
                ctx.trap(func_id, pc, span_id, Trap::InvalidPc)
            })?;
            // Keep span ids as plain `u64` in the dispatch loop: carrying `SpanId`/`NonZeroU64`
            // through this hot path caused measurable regressions in branch-heavy benchmarks.
            // The verifier/program pipeline still enforces non-zero span ids via `SpanId`.
            let span_id = vf.span_at_ix(instr_ix).map(|id| id.get());
            let (instr, actual_pc, next_pc) = vf.fetch_at_ix(instr_ix).ok_or_else(|| {
                let trap_span = span_id.or_else(|| ctx.span_at(program_ref, func_id, pc));
                ctx.trap(func_id, pc, trap_span, Trap::InvalidPc)
            })?;
            debug_assert_eq!(
                pc, actual_pc,
                "frame pc must match verified instruction offset"
            );
            let next_instr_ix = instr_ix.saturating_add(1);

            // Source bytes remain authoritative for reporting. Untraced dispatch can eliminate
            // this non-panicking lookup; prepared instructions keep source offset/end instead of
            // duplicating the opcode and growing every instruction record.
            let opcode = program_ref
                .functions
                .get(func_id.0 as usize)
                .and_then(|f| {
                    program_ref
                        .bytecode_data
                        .get(f.bytecode.offset as usize + pc as usize)
                })
                .copied()
                .unwrap_or_default();
            P::instr(trace, program_ref, func_id, pc, next_pc, span_id, opcode);

            // Default fallthrough: advance to the next decoded instruction.
            ctx.frames[frame_index].pc = next_pc;
            ctx.frames[frame_index].instr_ix = next_instr_ix;

            let sources = ValueSources::new(program_ref, args, self.memory.aggs());
            P::registers(
                &mut register_state,
                trace,
                program_ref,
                ctx,
                func_id,
                pc,
                base,
                sources,
                crate::trace::RegisterPhase::Inputs,
            );
            match instr {
                ExecInstr::Nop => {}
                ExecInstr::Trap { code } => {
                    return Err(ctx.trap(func_id, pc, span_id, Trap::TrapCode(*code)));
                }

                ExecInstr::MovUnit { dst, src } => {
                    let v = ctx.read_unit(base, *src);
                    ctx.write_unit(base, *dst, v);
                    ctx.frames[frame_index].pc = next_pc;
                    ctx.frames[frame_index].instr_ix = next_instr_ix;
                }
                ExecInstr::MovBool { dst, src } => {
                    let v = ctx.read_bool(base, *src);
                    ctx.write_bool(base, *dst, v);
                    ctx.frames[frame_index].pc = next_pc;
                    ctx.frames[frame_index].instr_ix = next_instr_ix;
                }
                ExecInstr::MovI64 { dst, src } => {
                    let v = ctx.read_i64(base, *src);
                    ctx.write_i64(base, *dst, v);
                    ctx.frames[frame_index].pc = next_pc;
                    ctx.frames[frame_index].instr_ix = next_instr_ix;
                }
                ExecInstr::MovU64 { dst, src } => {
                    let v = ctx.read_u64(base, *src);
                    ctx.write_u64(base, *dst, v);
                    ctx.frames[frame_index].pc = next_pc;
                    ctx.frames[frame_index].instr_ix = next_instr_ix;
                }
                ExecInstr::MovF64 { dst, src } => {
                    let v = ctx.read_f64(base, *src);
                    ctx.write_f64(base, *dst, v);
                    ctx.frames[frame_index].pc = next_pc;
                    ctx.frames[frame_index].instr_ix = next_instr_ix;
                }
                ExecInstr::MovDecimal { dst, src } => {
                    let v = ctx.read_decimal(base, *src);
                    ctx.write_decimal(base, *dst, v);
                    ctx.frames[frame_index].pc = next_pc;
                    ctx.frames[frame_index].instr_ix = next_instr_ix;
                }
                ExecInstr::MovBytes { dst, src } => {
                    let v = ctx.read_bytes_handle(base, *src);
                    ctx.write_bytes_handle(base, *dst, v);
                    ctx.frames[frame_index].pc = next_pc;
                    ctx.frames[frame_index].instr_ix = next_instr_ix;
                }
                ExecInstr::MovStr { dst, src } => {
                    let v = ctx.read_str_handle(base, *src);
                    ctx.write_str_handle(base, *dst, v);
                    ctx.frames[frame_index].pc = next_pc;
                    ctx.frames[frame_index].instr_ix = next_instr_ix;
                }
                ExecInstr::MovObj { dst, src } => {
                    let v = ctx.read_obj(base, *src);
                    ctx.write_obj(base, *dst, v);
                    ctx.frames[frame_index].pc = next_pc;
                    ctx.frames[frame_index].instr_ix = next_instr_ix;
                }
                ExecInstr::MovAgg { dst, src } => {
                    let v = ctx.read_agg_handle(base, *src);
                    ctx.write_agg_handle(base, *dst, v);
                    ctx.frames[frame_index].pc = next_pc;
                    ctx.frames[frame_index].instr_ix = next_instr_ix;
                }
                ExecInstr::MovFunc { dst, src } => {
                    let v = ctx.read_func(base, *src);
                    ctx.write_func(base, *dst, v);
                    ctx.frames[frame_index].pc = next_pc;
                    ctx.frames[frame_index].instr_ix = next_instr_ix;
                }
                ExecInstr::MovClosure { dst, src } => {
                    let v = ctx.read_closure(base, *src);
                    ctx.write_closure(base, *dst, v);
                    ctx.frames[frame_index].pc = next_pc;
                    ctx.frames[frame_index].instr_ix = next_instr_ix;
                }

                ExecInstr::ConstUnit { dst } => {
                    ctx.write_unit(base, *dst, 0);
                    ctx.frames[frame_index].pc = next_pc;
                    ctx.frames[frame_index].instr_ix = next_instr_ix;
                }
                ExecInstr::ConstBool { dst, imm } => {
                    ctx.write_bool(base, *dst, *imm);
                    ctx.frames[frame_index].pc = next_pc;
                    ctx.frames[frame_index].instr_ix = next_instr_ix;
                }
                ExecInstr::ConstI64 { dst, imm } => {
                    ctx.write_i64(base, *dst, *imm);
                    ctx.frames[frame_index].pc = next_pc;
                    ctx.frames[frame_index].instr_ix = next_instr_ix;
                }
                ExecInstr::ConstU64 { dst, imm } => {
                    ctx.write_u64(base, *dst, *imm);
                    ctx.frames[frame_index].pc = next_pc;
                    ctx.frames[frame_index].instr_ix = next_instr_ix;
                }
                ExecInstr::ConstF64 { dst, bits } => {
                    ctx.write_f64(base, *dst, f64::from_bits(*bits));
                    ctx.frames[frame_index].pc = next_pc;
                    ctx.frames[frame_index].instr_ix = next_instr_ix;
                }
                ExecInstr::ConstDecimal { dst, value } => {
                    ctx.write_decimal(base, *dst, *value);
                    ctx.frames[frame_index].pc = next_pc;
                    ctx.frames[frame_index].instr_ix = next_instr_ix;
                }
                ExecInstr::ConstFunc { dst, func_id } => {
                    ctx.write_func(base, *dst, *func_id);
                    ctx.frames[frame_index].pc = next_pc;
                    ctx.frames[frame_index].instr_ix = next_instr_ix;
                }

                ExecInstr::ConstPoolUnit { dst, idx } => {
                    let ConstEntry::Unit = program_ref
                        .const_pool
                        .get(idx.0 as usize)
                        .ok_or_else(|| ctx.trap(func_id, pc, span_id, Trap::ConstOutOfBounds))?
                    else {
                        return Err(ctx.trap(func_id, pc, span_id, Trap::InvalidPc));
                    };
                    ctx.write_unit(base, *dst, 0);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::ConstPoolBool { dst, idx } => {
                    let ConstEntry::Bool(b) = program_ref
                        .const_pool
                        .get(idx.0 as usize)
                        .ok_or_else(|| ctx.trap(func_id, pc, span_id, Trap::ConstOutOfBounds))?
                    else {
                        return Err(ctx.trap(func_id, pc, span_id, Trap::InvalidPc));
                    };
                    ctx.write_bool(base, *dst, *b);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::ConstPoolI64 { dst, idx } => {
                    let ConstEntry::I64(i) = program_ref
                        .const_pool
                        .get(idx.0 as usize)
                        .ok_or_else(|| ctx.trap(func_id, pc, span_id, Trap::ConstOutOfBounds))?
                    else {
                        return Err(ctx.trap(func_id, pc, span_id, Trap::InvalidPc));
                    };
                    ctx.write_i64(base, *dst, *i);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::ConstPoolU64 { dst, idx } => {
                    let ConstEntry::U64(u) = program_ref
                        .const_pool
                        .get(idx.0 as usize)
                        .ok_or_else(|| ctx.trap(func_id, pc, span_id, Trap::ConstOutOfBounds))?
                    else {
                        return Err(ctx.trap(func_id, pc, span_id, Trap::InvalidPc));
                    };
                    ctx.write_u64(base, *dst, *u);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::ConstPoolF64 { dst, idx } => {
                    let ConstEntry::F64(bits) = program_ref
                        .const_pool
                        .get(idx.0 as usize)
                        .ok_or_else(|| ctx.trap(func_id, pc, span_id, Trap::ConstOutOfBounds))?
                    else {
                        return Err(ctx.trap(func_id, pc, span_id, Trap::InvalidPc));
                    };
                    ctx.write_f64(base, *dst, f64::from_bits(*bits));
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::ConstPoolBytes { dst, idx } => {
                    let h = ctx.arena.borrow_bytes(PayloadSource::Constant(*idx));
                    ctx.write_bytes_handle(base, *dst, h);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::ConstPoolStr { dst, idx } => {
                    let h = ctx.arena.borrow_str(PayloadSource::Constant(*idx));
                    ctx.write_str_handle(base, *dst, h);
                    ctx.frames[frame_index].pc = next_pc;
                }

                ExecInstr::DecAdd { dst, a, b } => {
                    let da = ctx.read_decimal(base, *a);
                    let db = ctx.read_decimal(base, *b);
                    if da.scale() != db.scale() {
                        return Err(ctx.trap(func_id, pc, span_id, Trap::DecimalScaleMismatch));
                    }
                    let mantissa = da
                        .mantissa()
                        .checked_add(db.mantissa())
                        .ok_or_else(|| ctx.trap(func_id, pc, span_id, Trap::DecimalOverflow))?;
                    ctx.write_decimal(base, *dst, Decimal::new(mantissa, da.scale()));
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::DecSub { dst, a, b } => {
                    let da = ctx.read_decimal(base, *a);
                    let db = ctx.read_decimal(base, *b);
                    if da.scale() != db.scale() {
                        return Err(ctx.trap(func_id, pc, span_id, Trap::DecimalScaleMismatch));
                    }
                    let mantissa = da
                        .mantissa()
                        .checked_sub(db.mantissa())
                        .ok_or_else(|| ctx.trap(func_id, pc, span_id, Trap::DecimalOverflow))?;
                    ctx.write_decimal(base, *dst, Decimal::new(mantissa, da.scale()));
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::DecMul { dst, a, b } => {
                    let da = ctx.read_decimal(base, *a);
                    let db = ctx.read_decimal(base, *b);
                    let mantissa = da
                        .mantissa()
                        .checked_mul(db.mantissa())
                        .ok_or_else(|| ctx.trap(func_id, pc, span_id, Trap::DecimalOverflow))?;
                    let scale_u16 = u16::from(da.scale()) + u16::from(db.scale());
                    let scale = u8::try_from(scale_u16)
                        .map_err(|_| ctx.trap(func_id, pc, span_id, Trap::DecimalOverflow))?;
                    ctx.write_decimal(base, *dst, Decimal::new(mantissa, scale));
                    ctx.frames[frame_index].pc = next_pc;
                }

                ExecInstr::F64Add { dst, a, b } => {
                    ctx.write_f64(base, *dst, ctx.read_f64(base, *a) + ctx.read_f64(base, *b));
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::F64Sub { dst, a, b } => {
                    ctx.write_f64(base, *dst, ctx.read_f64(base, *a) - ctx.read_f64(base, *b));
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::F64Mul { dst, a, b } => {
                    ctx.write_f64(base, *dst, ctx.read_f64(base, *a) * ctx.read_f64(base, *b));
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::F64Div { dst, a, b } => {
                    ctx.write_f64(base, *dst, ctx.read_f64(base, *a) / ctx.read_f64(base, *b));
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::F64Neg { dst, a } => {
                    ctx.write_f64(base, *dst, -ctx.read_f64(base, *a));
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::F64Abs { dst, a } => {
                    ctx.write_f64(base, *dst, ctx.read_f64(base, *a).abs());
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::F64Min { dst, a, b } => {
                    let out = f64_min(ctx.read_f64(base, *a), ctx.read_f64(base, *b));
                    ctx.write_f64(base, *dst, out);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::F64Max { dst, a, b } => {
                    let out = f64_max(ctx.read_f64(base, *a), ctx.read_f64(base, *b));
                    ctx.write_f64(base, *dst, out);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::F64MinNum { dst, a, b } => {
                    let out = f64_min_num(ctx.read_f64(base, *a), ctx.read_f64(base, *b));
                    ctx.write_f64(base, *dst, out);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::F64MaxNum { dst, a, b } => {
                    let out = f64_max_num(ctx.read_f64(base, *a), ctx.read_f64(base, *b));
                    ctx.write_f64(base, *dst, out);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::F64Rem { dst, a, b } => {
                    ctx.write_f64(base, *dst, ctx.read_f64(base, *a) % ctx.read_f64(base, *b));
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::F64ToBits { dst, a } => {
                    ctx.write_u64(base, *dst, ctx.read_f64(base, *a).to_bits());
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::F64FromBits { dst, a } => {
                    ctx.write_f64(base, *dst, f64::from_bits(ctx.read_u64(base, *a)));
                    ctx.frames[frame_index].pc = next_pc;
                }

                ExecInstr::I64Add { dst, a, b } => {
                    ctx.write_i64(
                        base,
                        *dst,
                        ctx.read_i64(base, *a).wrapping_add(ctx.read_i64(base, *b)),
                    );
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::I64Sub { dst, a, b } => {
                    ctx.write_i64(
                        base,
                        *dst,
                        ctx.read_i64(base, *a).wrapping_sub(ctx.read_i64(base, *b)),
                    );
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::I64Mul { dst, a, b } => {
                    ctx.write_i64(
                        base,
                        *dst,
                        ctx.read_i64(base, *a).wrapping_mul(ctx.read_i64(base, *b)),
                    );
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::I64And { dst, a, b } => {
                    ctx.write_i64(base, *dst, ctx.read_i64(base, *a) & ctx.read_i64(base, *b));
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::I64Or { dst, a, b } => {
                    ctx.write_i64(base, *dst, ctx.read_i64(base, *a) | ctx.read_i64(base, *b));
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::I64Xor { dst, a, b } => {
                    ctx.write_i64(base, *dst, ctx.read_i64(base, *a) ^ ctx.read_i64(base, *b));
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::I64Shl { dst, a, b } => {
                    let sh = (ctx.read_i64(base, *b) as u64 & 63) as u32;
                    ctx.write_i64(base, *dst, ctx.read_i64(base, *a) << sh);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::I64Shr { dst, a, b } => {
                    let sh = (ctx.read_i64(base, *b) as u64 & 63) as u32;
                    ctx.write_i64(base, *dst, ctx.read_i64(base, *a) >> sh);
                    ctx.frames[frame_index].pc = next_pc;
                }

                ExecInstr::U64Add { dst, a, b } => {
                    ctx.write_u64(
                        base,
                        *dst,
                        ctx.read_u64(base, *a).wrapping_add(ctx.read_u64(base, *b)),
                    );
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::U64Sub { dst, a, b } => {
                    ctx.write_u64(
                        base,
                        *dst,
                        ctx.read_u64(base, *a).wrapping_sub(ctx.read_u64(base, *b)),
                    );
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::U64Mul { dst, a, b } => {
                    ctx.write_u64(
                        base,
                        *dst,
                        ctx.read_u64(base, *a).wrapping_mul(ctx.read_u64(base, *b)),
                    );
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::U64And { dst, a, b } => {
                    ctx.write_u64(base, *dst, ctx.read_u64(base, *a) & ctx.read_u64(base, *b));
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::U64Or { dst, a, b } => {
                    ctx.write_u64(base, *dst, ctx.read_u64(base, *a) | ctx.read_u64(base, *b));
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::U64Xor { dst, a, b } => {
                    ctx.write_u64(base, *dst, ctx.read_u64(base, *a) ^ ctx.read_u64(base, *b));
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::U64Shl { dst, a, b } => {
                    let sh = (ctx.read_u64(base, *b) & 63) as u32;
                    ctx.write_u64(base, *dst, ctx.read_u64(base, *a) << sh);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::U64Shr { dst, a, b } => {
                    let sh = (ctx.read_u64(base, *b) & 63) as u32;
                    ctx.write_u64(base, *dst, ctx.read_u64(base, *a) >> sh);
                    ctx.frames[frame_index].pc = next_pc;
                }

                ExecInstr::I64Eq { dst, a, b } => {
                    ctx.write_bool(base, *dst, ctx.read_i64(base, *a) == ctx.read_i64(base, *b));
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::I64Lt { dst, a, b } => {
                    ctx.write_bool(base, *dst, ctx.read_i64(base, *a) < ctx.read_i64(base, *b));
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::I64Gt { dst, a, b } => {
                    ctx.write_bool(base, *dst, ctx.read_i64(base, *a) > ctx.read_i64(base, *b));
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::I64Le { dst, a, b } => {
                    ctx.write_bool(base, *dst, ctx.read_i64(base, *a) <= ctx.read_i64(base, *b));
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::I64Ge { dst, a, b } => {
                    ctx.write_bool(base, *dst, ctx.read_i64(base, *a) >= ctx.read_i64(base, *b));
                    ctx.frames[frame_index].pc = next_pc;
                }

                ExecInstr::U64Eq { dst, a, b } => {
                    ctx.write_bool(base, *dst, ctx.read_u64(base, *a) == ctx.read_u64(base, *b));
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::U64Lt { dst, a, b } => {
                    ctx.write_bool(base, *dst, ctx.read_u64(base, *a) < ctx.read_u64(base, *b));
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::U64Gt { dst, a, b } => {
                    ctx.write_bool(base, *dst, ctx.read_u64(base, *a) > ctx.read_u64(base, *b));
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::U64Le { dst, a, b } => {
                    ctx.write_bool(base, *dst, ctx.read_u64(base, *a) <= ctx.read_u64(base, *b));
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::U64Ge { dst, a, b } => {
                    ctx.write_bool(base, *dst, ctx.read_u64(base, *a) >= ctx.read_u64(base, *b));
                    ctx.frames[frame_index].pc = next_pc;
                }

                ExecInstr::F64Eq { dst, a, b } => {
                    ctx.write_bool(base, *dst, ctx.read_f64(base, *a) == ctx.read_f64(base, *b));
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::F64Lt { dst, a, b } => {
                    ctx.write_bool(base, *dst, ctx.read_f64(base, *a) < ctx.read_f64(base, *b));
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::F64Gt { dst, a, b } => {
                    ctx.write_bool(base, *dst, ctx.read_f64(base, *a) > ctx.read_f64(base, *b));
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::F64Le { dst, a, b } => {
                    ctx.write_bool(base, *dst, ctx.read_f64(base, *a) <= ctx.read_f64(base, *b));
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::F64Ge { dst, a, b } => {
                    ctx.write_bool(base, *dst, ctx.read_f64(base, *a) >= ctx.read_f64(base, *b));
                    ctx.frames[frame_index].pc = next_pc;
                }

                ExecInstr::BoolNot { dst, a } => {
                    ctx.write_bool(base, *dst, !ctx.read_bool(base, *a));
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::BoolAnd { dst, a, b } => {
                    ctx.write_bool(
                        base,
                        *dst,
                        ctx.read_bool(base, *a) & ctx.read_bool(base, *b),
                    );
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::BoolOr { dst, a, b } => {
                    ctx.write_bool(
                        base,
                        *dst,
                        ctx.read_bool(base, *a) | ctx.read_bool(base, *b),
                    );
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::BoolXor { dst, a, b } => {
                    ctx.write_bool(
                        base,
                        *dst,
                        ctx.read_bool(base, *a) ^ ctx.read_bool(base, *b),
                    );
                    ctx.frames[frame_index].pc = next_pc;
                }

                ExecInstr::U64ToI64 { dst, a } => {
                    let u = ctx.read_u64(base, *a);
                    let i = i64::try_from(u)
                        .map_err(|_| ctx.trap(func_id, pc, span_id, Trap::IntCastOverflow))?;
                    ctx.write_i64(base, *dst, i);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::I64ToU64 { dst, a } => {
                    let i = ctx.read_i64(base, *a);
                    let u = u64::try_from(i)
                        .map_err(|_| ctx.trap(func_id, pc, span_id, Trap::IntCastOverflow))?;
                    ctx.write_u64(base, *dst, u);
                    ctx.frames[frame_index].pc = next_pc;
                }

                ExecInstr::SelectUnit { dst, cond, a, b } => {
                    let out = if ctx.read_bool(base, *cond) {
                        ctx.read_unit(base, *a)
                    } else {
                        ctx.read_unit(base, *b)
                    };
                    ctx.write_unit(base, *dst, out);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::SelectBool { dst, cond, a, b } => {
                    let out = if ctx.read_bool(base, *cond) {
                        ctx.read_bool(base, *a)
                    } else {
                        ctx.read_bool(base, *b)
                    };
                    ctx.write_bool(base, *dst, out);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::SelectI64 { dst, cond, a, b } => {
                    let out = if ctx.read_bool(base, *cond) {
                        ctx.read_i64(base, *a)
                    } else {
                        ctx.read_i64(base, *b)
                    };
                    ctx.write_i64(base, *dst, out);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::SelectU64 { dst, cond, a, b } => {
                    let out = if ctx.read_bool(base, *cond) {
                        ctx.read_u64(base, *a)
                    } else {
                        ctx.read_u64(base, *b)
                    };
                    ctx.write_u64(base, *dst, out);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::SelectF64 { dst, cond, a, b } => {
                    let out = if ctx.read_bool(base, *cond) {
                        ctx.read_f64(base, *a)
                    } else {
                        ctx.read_f64(base, *b)
                    };
                    ctx.write_f64(base, *dst, out);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::SelectDecimal { dst, cond, a, b } => {
                    let out = if ctx.read_bool(base, *cond) {
                        ctx.read_decimal(base, *a)
                    } else {
                        ctx.read_decimal(base, *b)
                    };
                    ctx.write_decimal(base, *dst, out);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::SelectBytes { dst, cond, a, b } => {
                    let out = if ctx.read_bool(base, *cond) {
                        ctx.read_bytes_handle(base, *a)
                    } else {
                        ctx.read_bytes_handle(base, *b)
                    };
                    ctx.write_bytes_handle(base, *dst, out);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::SelectStr { dst, cond, a, b } => {
                    let out = if ctx.read_bool(base, *cond) {
                        ctx.read_str_handle(base, *a)
                    } else {
                        ctx.read_str_handle(base, *b)
                    };
                    ctx.write_str_handle(base, *dst, out);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::SelectObj { dst, cond, a, b } => {
                    let out = if ctx.read_bool(base, *cond) {
                        ctx.read_obj(base, *a)
                    } else {
                        ctx.read_obj(base, *b)
                    };
                    ctx.write_obj(base, *dst, out);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::SelectAgg { dst, cond, a, b } => {
                    let out = if ctx.read_bool(base, *cond) {
                        ctx.read_agg_handle(base, *a)
                    } else {
                        ctx.read_agg_handle(base, *b)
                    };
                    ctx.write_agg_handle(base, *dst, out);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::SelectFunc { dst, cond, a, b } => {
                    let out = if ctx.read_bool(base, *cond) {
                        ctx.read_func(base, *a)
                    } else {
                        ctx.read_func(base, *b)
                    };
                    ctx.write_func(base, *dst, out);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::SelectClosure { dst, cond, a, b } => {
                    let out = if ctx.read_bool(base, *cond) {
                        ctx.read_closure(base, *a)
                    } else {
                        ctx.read_closure(base, *b)
                    };
                    ctx.write_closure(base, *dst, out);
                    ctx.frames[frame_index].pc = next_pc;
                }

                ExecInstr::Br {
                    cond,
                    true_ix,
                    false_ix,
                    true_copies,
                    false_copies,
                } => {
                    let (target_ix_u32, copies) = if ctx.read_bool(base, *cond) {
                        (*true_ix, *true_copies)
                    } else {
                        (*false_ix, *false_copies)
                    };
                    for pair in vf.vregs(copies).as_chunks::<2>().0 {
                        ctx.copy_vreg(base, pair[1], base, pair[0])
                            .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                    }
                    let target_ix = usize::try_from(target_ix_u32)
                        .map_err(|_| ctx.trap(func_id, pc, span_id, Trap::InvalidPc))?;
                    let target_pc = vf
                        .pc_at_ix(target_ix)
                        .ok_or_else(|| ctx.trap(func_id, pc, span_id, Trap::InvalidPc))?;
                    ctx.frames[frame_index].pc = target_pc;
                    ctx.frames[frame_index].instr_ix = target_ix;
                }
                ExecInstr::Jmp { target_ix, copies } => {
                    for pair in vf.vregs(*copies).as_chunks::<2>().0 {
                        ctx.copy_vreg(base, pair[1], base, pair[0])
                            .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                    }
                    let target_ix = usize::try_from(*target_ix)
                        .map_err(|_| ctx.trap(func_id, pc, span_id, Trap::InvalidPc))?;
                    let target_pc = vf
                        .pc_at_ix(target_ix)
                        .ok_or_else(|| ctx.trap(func_id, pc, span_id, Trap::InvalidPc))?;
                    ctx.frames[frame_index].pc = target_pc;
                    ctx.frames[frame_index].instr_ix = target_ix;
                }

                ExecInstr::Call {
                    eff_out,
                    func_id: callee,
                    eff_in: _,
                    args,
                    rets: dst_rets,
                } => {
                    if ctx.frames.len() >= max_call_depth {
                        return Err(ctx.trap(func_id, pc, span_id, Trap::CallDepthExceeded));
                    }

                    let callee_fn = program_ref
                        .functions
                        .get(callee.0 as usize)
                        .ok_or_else(|| ctx.trap(func_id, pc, span_id, Trap::InvalidPc))?;
                    let callee_vf = program
                        .verified(*callee)
                        .ok_or_else(|| ctx.trap(func_id, pc, span_id, Trap::InvalidPc))?;

                    // v1: effect token is `Unit` (stored as 0).
                    ctx.write_unit(base, *eff_out, 0);

                    let callee_base = ctx.alloc_frame(callee_vf);
                    let args = vf.vregs(*args);
                    if args.len() != callee_vf.reg_layout.arg_regs.len() {
                        return Err(ctx.trap(func_id, pc, span_id, Trap::InvalidPc));
                    }
                    for (src, dst) in args
                        .iter()
                        .copied()
                        .zip(callee_vf.reg_layout.arg_regs.iter().copied())
                    {
                        ctx.copy_vreg(base, src, callee_base, dst)
                            .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                    }

                    self.check_arg_contracts(ctx, program_ref, *callee, callee_vf, callee_base)
                        .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;

                    ctx.frames.push(Frame {
                        func: *callee,
                        pc: callee_vf.pc_at_ix(0).unwrap_or(0),
                        instr_ix: 0,
                        byte_len: callee_vf.byte_len,
                        base: callee_base,
                        return_to: Some(ReturnTo {
                            dst_rets: *dst_rets,
                        }),
                    });

                    P::call_scope_enter(
                        trace,
                        program_ref,
                        ctx.frames.len(),
                        *callee,
                        callee_vf.pc_at_ix(0).unwrap_or(0),
                        callee_vf.span_at_ix(0).map(|id| id.get()),
                        Some(TraceLocation { func: func_id, pc }),
                    );

                    debug_assert_eq!(
                        callee_fn.bytecode.len, callee_vf.byte_len,
                        "verified byte_len must match decoded bytecode length"
                    );
                }

                ExecInstr::Ret { eff_in: _, rets } => {
                    if ctx.frames.len() == 1 {
                        let rets = vf.vregs(*rets);
                        outputs.reserve(rets.len());
                        for &r in rets {
                            outputs.push(
                                ctx.materialize_vreg(base, r, sources)
                                    .map_err(|t| ctx.trap(func_id, pc, span_id, t))?,
                            );
                        }
                        P::values(trace, program_ref, || ValueEvent::ReturnCandidate {
                            location: TraceLocation { func: func_id, pc },
                            values: &outputs[output_start..],
                            aggregates: self.memory.aggs(),
                        });
                        self.check_result_contracts(ctx, program_ref, func_id, vf, base, rets)
                            .map_err(|error| ctx.trap(func_id, pc, span_id, error))?;
                        // Proof keys use precommit handles and must not survive compaction.
                        ctx.contract_scratch.clear_proofs();
                        self.memory
                            .commit(checkpoint, &mut outputs[output_start..])
                            .map_err(|error| {
                                ctx.trap(func_id, pc, span_id, Trap::AggError(error))
                            })?;
                        P::call_scope_exit(
                            trace,
                            program_ref,
                            ctx.frames.len(),
                            func_id,
                            pc,
                            span_id,
                        );
                        P::instr_end(trace, program_ref, None);
                        return Ok(());
                    }

                    self.check_result_contracts(
                        ctx,
                        program_ref,
                        func_id,
                        vf,
                        base,
                        vf.vregs(*rets),
                    )
                    .map_err(|error| ctx.trap(func_id, pc, span_id, error))?;
                    let finished = ctx
                        .frames
                        .pop()
                        .ok_or_else(|| ctx.trap(func_id, pc, span_id, Trap::InvalidPc))?;

                    let Some(ret) = finished.return_to else {
                        return Err(ctx.trap(func_id, pc, span_id, Trap::InvalidPc));
                    };

                    let rets = vf.vregs(*rets);

                    let caller_index = ctx.frames.len() - 1;
                    let (caller_func, caller_base) = {
                        let f = &ctx.frames[caller_index];
                        (f.func, f.base)
                    };
                    let caller_vf = program
                        .verified(caller_func)
                        .ok_or_else(|| ctx.trap(func_id, pc, span_id, Trap::InvalidPc))?;
                    let dst_rets = caller_vf.vregs(ret.dst_rets);

                    if dst_rets.len() != rets.len() {
                        return Err(ctx.trap(func_id, pc, span_id, Trap::InvalidPc));
                    }

                    for (&dst, &src) in dst_rets.iter().zip(rets.iter()) {
                        ctx.copy_vreg(base, src, caller_base, dst)
                            .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                    }

                    ctx.truncate_to(base);
                    P::call_scope_exit(
                        trace,
                        program_ref,
                        ctx.frames.len() + 1,
                        func_id,
                        pc,
                        span_id,
                    );
                }

                ExecInstr::HostCall {
                    eff_out,
                    host_sig,
                    eff_in: _,
                    args,
                    rets,
                } => {
                    if ctx.host_calls >= max_host_calls {
                        return Err(ctx.trap(func_id, pc, span_id, Trap::HostCallLimitExceeded));
                    }
                    ctx.host_calls += 1;

                    let hs = program_ref
                        .host_sig(*host_sig)
                        .ok_or_else(|| ctx.trap(func_id, pc, span_id, Trap::ConstOutOfBounds))?;
                    let sym = program_ref
                        .symbol_str(hs.symbol)
                        .map_err(|_| ctx.trap(func_id, pc, span_id, Trap::ConstOutOfBounds))?;

                    let ret_types = program_ref
                        .host_sig_rets(hs)
                        .map_err(|_| ctx.trap(func_id, pc, span_id, Trap::ConstOutOfBounds))?;

                    let args = vf.vregs(*args);

                    // Stack buffer for the common case (0–8 args); Vec fallback for the rest.
                    // 8 × 24 bytes = 192 bytes on the stack — every host call in the codebase
                    // uses 0–2 args, so this is generous headroom without measurable stack cost.
                    const HOST_CALL_INLINE_ARGS: usize = 8;
                    let mut buf = [ValueRef::Unit; HOST_CALL_INLINE_ARGS];
                    let mut vec_fallback: Vec<ValueRef<'_>>;
                    let call_args_mut: &mut [ValueRef<'_>] = if args.len() <= HOST_CALL_INLINE_ARGS
                    {
                        &mut buf[..args.len()]
                    } else {
                        vec_fallback = vec![ValueRef::Unit; args.len()];
                        &mut vec_fallback
                    };

                    for (slot, &a) in call_args_mut.iter_mut().zip(args.iter()) {
                        *slot = read_value_ref_at(
                            &ctx.arena,
                            sources,
                            &ctx.units,
                            &ctx.bools,
                            &ctx.i64s,
                            &ctx.u64s,
                            &ctx.f64s,
                            &ctx.decimals,
                            &ctx.bytes,
                            &ctx.strs,
                            &ctx.objs,
                            &ctx.aggs,
                            &ctx.funcs,
                            &ctx.closures,
                            base,
                            a,
                        )
                        .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                    }

                    let call_args: &[ValueRef<'_>] = call_args_mut;

                    P::host_scope_enter(
                        trace,
                        program_ref,
                        *host_sig,
                        hs.symbol,
                        hs.sig_hash,
                        ctx.frames.len(),
                        func_id,
                        pc,
                        span_id,
                    );

                    // Stack buffer for the common case (0–8 rets); Vec fallback for the rest.
                    // Value is Clone but not Copy, so use `from_fn` instead of repeat syntax.
                    // 8 × ~32 bytes = ~256 bytes on the stack — every host call in the codebase
                    // returns 0–2 values, so this is generous headroom.
                    const HOST_CALL_INLINE_RETS: usize = 8;
                    let mut ret_buf: [Value; HOST_CALL_INLINE_RETS] =
                        core::array::from_fn(|_| Value::Unit);
                    let mut ret_vec_fallback: Vec<Value>;
                    let ret_slots: &mut [Value] = if ret_types.len() <= HOST_CALL_INLINE_RETS {
                        &mut ret_buf[..ret_types.len()]
                    } else {
                        ret_vec_fallback = vec![Value::Unit; ret_types.len()];
                        &mut ret_vec_fallback
                    };

                    let access_for_call = access
                        .as_mut()
                        .map(|access| &mut **access as &mut dyn AccessSink);
                    let host_ctx = HostContext::new(
                        program_ref,
                        *host_sig,
                        self.memory.aggs(),
                        access_for_call,
                    );
                    let host_result =
                        self.host
                            .call(sym, hs.sig_hash, call_args, ret_slots, host_ctx);
                    P::values(trace, program_ref, || ValueEvent::HostCall {
                        location: TraceLocation { func: func_id, pc },
                        host_sig: *host_sig,
                        args: call_args,
                        outcome: match &host_result {
                            Ok(extra_fuel) => HostCallOutcome::Returned {
                                values: ret_slots,
                                extra_fuel: *extra_fuel,
                            },
                            Err(error) => HostCallOutcome::Failed(error),
                        },
                        aggregates: self.memory.aggs(),
                    });
                    let extra_fuel = host_result.map_err(|error| {
                        ctx.trap(
                            func_id,
                            pc,
                            span_id,
                            Trap::HostCallFailed {
                                symbol: Box::from(sym),
                                sig_hash: hs.sig_hash,
                                error,
                            },
                        )
                    })?;
                    ctx.fuel = ctx.fuel.saturating_sub(extra_fuel);

                    // v1: effect token is `Unit`.
                    ctx.write_unit(base, *eff_out, 0);

                    let rets = vf.vregs(*rets);
                    // The VM controls the slice size, so arity is guaranteed correct for
                    // verified programs. Keep the Trap variant (public API) but it's
                    // unreachable from verified programs.
                    debug_assert_eq!(
                        ret_slots.len(),
                        rets.len(),
                        "HostReturnArityMismatch: expected {}, got {}",
                        rets.len(),
                        ret_slots.len()
                    );
                    for (v, &expected) in ret_slots.iter().zip(ret_types.iter()) {
                        v.check_type(expected)
                            .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                    }
                    let payload_bytes = ret_slots
                        .iter()
                        .try_fold(0_usize, |total, value| {
                            let len = match value {
                                Value::Bytes(bytes) => bytes.len(),
                                Value::Str(s) => s.len(),
                                _ => 0,
                            };
                            total.checked_add(len).ok_or(Trap::PayloadSizeOverflow)
                        })
                        .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                    ctx.charge_payload_bytes(payload_bytes, 1)
                        .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                    for (dst, v) in rets.iter().copied().zip(ret_slots.iter_mut()) {
                        let val = core::mem::replace(v, Value::Unit);
                        ctx.write_owned_value(base, dst, val)
                            .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                    }
                    P::host_scope_exit(
                        trace,
                        program_ref,
                        *host_sig,
                        hs.symbol,
                        hs.sig_hash,
                        ctx.frames.len(),
                        func_id,
                        pc,
                        span_id,
                    );
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::CallIndirectFunc {
                    eff_out,
                    call_sig,
                    callee,
                    eff_in: _,
                    args,
                    rets: dst_rets,
                } => {
                    if ctx.frames.len() >= max_call_depth {
                        return Err(ctx.trap(func_id, pc, span_id, Trap::CallDepthExceeded));
                    }

                    let callee_func = ctx.read_func(base, *callee);
                    let callee_fn = program_ref
                        .functions
                        .get(callee_func.0 as usize)
                        .ok_or_else(|| ctx.trap(func_id, pc, span_id, Trap::InvalidPc))?;
                    let callee_vf = program
                        .verified(callee_func)
                        .ok_or_else(|| ctx.trap(func_id, pc, span_id, Trap::InvalidPc))?;

                    validate_indirect_call_signature(
                        program,
                        *call_sig,
                        callee_func,
                        callee_fn,
                        IndirectCalleeKind::Func,
                    )
                    .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;

                    // v1: effect token is `Unit` (stored as 0).
                    ctx.write_unit(base, *eff_out, 0);

                    let callee_base = ctx.alloc_frame(callee_vf);
                    let args = vf.vregs(*args);
                    if args.len() != callee_vf.reg_layout.arg_regs.len() {
                        return Err(ctx.trap(func_id, pc, span_id, Trap::InvalidPc));
                    }
                    for (src, dst) in args
                        .iter()
                        .copied()
                        .zip(callee_vf.reg_layout.arg_regs.iter().copied())
                    {
                        ctx.copy_vreg(base, src, callee_base, dst)
                            .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                    }

                    self.check_arg_contracts(ctx, program_ref, callee_func, callee_vf, callee_base)
                        .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;

                    ctx.frames.push(Frame {
                        func: callee_func,
                        pc: callee_vf.pc_at_ix(0).unwrap_or(0),
                        instr_ix: 0,
                        byte_len: callee_vf.byte_len,
                        base: callee_base,
                        return_to: Some(ReturnTo {
                            dst_rets: *dst_rets,
                        }),
                    });

                    P::call_scope_enter(
                        trace,
                        program_ref,
                        ctx.frames.len(),
                        callee_func,
                        callee_vf.pc_at_ix(0).unwrap_or(0),
                        callee_vf.span_at_ix(0).map(|id| id.get()),
                        Some(TraceLocation { func: func_id, pc }),
                    );

                    debug_assert_eq!(
                        callee_fn.bytecode.len, callee_vf.byte_len,
                        "verified byte_len must match decoded bytecode length"
                    );
                }
                ExecInstr::CallIndirectClosure {
                    eff_out,
                    call_sig,
                    callee,
                    eff_in: _,
                    args,
                    rets: dst_rets,
                } => {
                    if ctx.frames.len() >= max_call_depth {
                        return Err(ctx.trap(func_id, pc, span_id, Trap::CallDepthExceeded));
                    }

                    let callee = ctx.read_closure(base, *callee);
                    let callee_fn = program_ref
                        .functions
                        .get(callee.func.0 as usize)
                        .ok_or_else(|| ctx.trap(func_id, pc, span_id, Trap::InvalidPc))?;
                    let callee_vf = program
                        .verified(callee.func)
                        .ok_or_else(|| ctx.trap(func_id, pc, span_id, Trap::InvalidPc))?;

                    validate_indirect_call_signature(
                        program,
                        *call_sig,
                        callee.func,
                        callee_fn,
                        IndirectCalleeKind::Closure,
                    )
                    .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;

                    // v1: effect token is `Unit` (stored as 0).
                    ctx.write_unit(base, *eff_out, 0);

                    let callee_base = ctx.alloc_frame(callee_vf);
                    let args = vf.vregs(*args);
                    let Some((&env_dst, user_arg_dsts)) =
                        callee_vf.reg_layout.arg_regs.split_first()
                    else {
                        return Err(ctx.trap(func_id, pc, span_id, Trap::InvalidPc));
                    };
                    let VReg::Agg(env_dst) = env_dst else {
                        return Err(ctx.trap(func_id, pc, span_id, Trap::InvalidPc));
                    };
                    ctx.write_agg_handle(callee_base, env_dst, callee.env);

                    if args.len() != user_arg_dsts.len() {
                        return Err(ctx.trap(func_id, pc, span_id, Trap::InvalidPc));
                    }
                    for (src, dst) in args.iter().copied().zip(user_arg_dsts.iter().copied()) {
                        ctx.copy_vreg(base, src, callee_base, dst)
                            .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                    }

                    self.check_arg_contracts(ctx, program_ref, callee.func, callee_vf, callee_base)
                        .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;

                    // Even an unrefined environment must resolve before entering a closure body.
                    let environment_contract =
                        program_ref.verified_contracts(callee_fn.arg_types)[0];
                    if matches!(
                        program_ref.contracts.get(environment_contract),
                        Some(Contract::Kind(_))
                    ) {
                        ctx.contract_scratch
                            .check_value(
                                program_ref,
                                self.memory.aggs(),
                                &Value::Agg(callee.env),
                                environment_contract,
                                self.limits.max_contract_work,
                            )
                            .map_err(|error| {
                                ctx.trap(
                                    func_id,
                                    pc,
                                    span_id,
                                    self.contract_trap(
                                        callee.func,
                                        ContractPosition::Argument {
                                            callee: callee.func,
                                            index: 0,
                                        },
                                        error,
                                    ),
                                )
                            })?;
                    }

                    ctx.frames.push(Frame {
                        func: callee.func,
                        pc: callee_vf.pc_at_ix(0).unwrap_or(0),
                        instr_ix: 0,
                        byte_len: callee_vf.byte_len,
                        base: callee_base,
                        return_to: Some(ReturnTo {
                            dst_rets: *dst_rets,
                        }),
                    });

                    P::call_scope_enter(
                        trace,
                        program_ref,
                        ctx.frames.len(),
                        callee.func,
                        callee_vf.pc_at_ix(0).unwrap_or(0),
                        callee_vf.span_at_ix(0).map(|id| id.get()),
                        Some(TraceLocation { func: func_id, pc }),
                    );

                    debug_assert_eq!(
                        callee_fn.bytecode.len, callee_vf.byte_len,
                        "verified byte_len must match decoded bytecode length"
                    );
                }
                ExecInstr::ClosureNew { dst, func, env } => {
                    let closure = Closure {
                        func: ctx.read_func(base, *func),
                        env: ctx.read_agg_handle(base, *env),
                    };
                    let body = program_ref
                        .functions
                        .get(closure.func.0 as usize)
                        .ok_or_else(|| ctx.trap(func_id, pc, span_id, Trap::InvalidPc))?;
                    let env_contract = *program_ref
                        .verified_contracts(body.arg_types)
                        .first()
                        .ok_or_else(|| ctx.trap(func_id, pc, span_id, Trap::ArityMismatch))?;
                    if program_ref.contracts.kind(env_contract) != Some(ValueType::Agg) {
                        return Err(ctx.trap(
                            func_id,
                            pc,
                            span_id,
                            Trap::TypeMismatch {
                                expected: ValueType::Agg,
                                actual: program_ref.contracts.kind(env_contract).unwrap(),
                            },
                        ));
                    }
                    ctx.contract_scratch
                        .check_value(
                            program_ref,
                            self.memory.aggs(),
                            &Value::Agg(closure.env),
                            env_contract,
                            self.limits.max_contract_work,
                        )
                        .map_err(|error| {
                            ctx.trap(
                                func_id,
                                pc,
                                span_id,
                                self.contract_trap(
                                    closure.func,
                                    ContractPosition::Argument {
                                        callee: closure.func,
                                        index: 0,
                                    },
                                    error,
                                ),
                            )
                        })?;
                    ctx.write_closure(base, *dst, closure);
                    ctx.frames[frame_index].pc = next_pc;
                }

                ExecInstr::TupleNew { dst, values } => {
                    let values = vf.vregs(*values);
                    let mut vals = Vec::with_capacity(values.len());
                    for &r in values {
                        vals.push(
                            ctx.materialize_vreg(base, r, sources)
                                .map_err(|t| ctx.trap(func_id, pc, span_id, t))?,
                        );
                    }
                    let h =
                        self.memory.aggs_mut().tuple_new(vals).map_err(|error| {
                            ctx.trap(func_id, pc, span_id, Trap::AggError(error))
                        })?;
                    ctx.write_agg_handle(base, AggReg(dst.0), h);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::TupleGet { dst, tuple, index } => {
                    let h = ctx.read_agg_handle(base, AggReg(tuple.0));
                    let out = self
                        .memory
                        .aggs()
                        .tuple_get_ref(h, *index as usize)
                        .map_err(|e| ctx.trap(func_id, pc, span_id, Trap::AggError(e)))?;
                    ctx.write_borrowed_value(
                        base,
                        *dst,
                        out,
                        PayloadSource::Aggregate {
                            handle: h,
                            index: *index as usize,
                        },
                    )
                    .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                    ctx.frames[frame_index].pc = next_pc;
                }

                ExecInstr::StructNew {
                    dst,
                    type_id,
                    values,
                } => {
                    let values = vf.vregs(*values);
                    let st = program_ref
                        .types
                        .structs
                        .get(type_id.0 as usize)
                        .ok_or_else(|| ctx.trap(func_id, pc, span_id, Trap::TypeIdOutOfBounds))?;
                    let field_types = program_ref
                        .types
                        .struct_field_types(st)
                        .map_err(|_| ctx.trap(func_id, pc, span_id, Trap::TypeIdOutOfBounds))?;
                    if field_types.len() != values.len() {
                        return Err(ctx.trap(func_id, pc, span_id, Trap::ArityMismatch));
                    }

                    let mut vals = Vec::with_capacity(values.len());
                    for &r in values {
                        vals.push(
                            ctx.materialize_vreg(base, r, sources)
                                .map_err(|t| ctx.trap(func_id, pc, span_id, t))?,
                        );
                    }
                    let h = self
                        .memory
                        .aggs_mut()
                        .struct_new(*type_id, vals)
                        .map_err(|error| ctx.trap(func_id, pc, span_id, Trap::AggError(error)))?;
                    ctx.write_agg_handle(base, AggReg(dst.0), h);
                    ctx.frames[frame_index].pc = next_pc;
                }

                ExecInstr::StructGet {
                    dst,
                    st,
                    field_index,
                } => {
                    let h = ctx.read_agg_handle(base, AggReg(st.0));
                    let out = self
                        .memory
                        .aggs()
                        .struct_get_ref(h, *field_index as usize)
                        .map_err(|e| ctx.trap(func_id, pc, span_id, Trap::AggError(e)))?;
                    ctx.write_borrowed_value(
                        base,
                        *dst,
                        out,
                        PayloadSource::Aggregate {
                            handle: h,
                            index: *field_index as usize,
                        },
                    )
                    .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                    ctx.frames[frame_index].pc = next_pc;
                }

                ExecInstr::ArrayNew {
                    dst,
                    elem_type_id,
                    len,
                    values,
                } => {
                    let values = vf.vregs(*values);
                    program_ref
                        .types
                        .array_elems
                        .get(elem_type_id.0 as usize)
                        .ok_or_else(|| {
                            ctx.trap(func_id, pc, span_id, Trap::ElemTypeIdOutOfBounds)
                        })?;
                    if *len as usize != values.len() {
                        return Err(ctx.trap(func_id, pc, span_id, Trap::ArityMismatch));
                    }

                    let mut vals = Vec::with_capacity(values.len());
                    for &r in values {
                        vals.push(
                            ctx.materialize_vreg(base, r, sources)
                                .map_err(|t| ctx.trap(func_id, pc, span_id, t))?,
                        );
                    }
                    let h = self
                        .memory
                        .aggs_mut()
                        .array_new(*elem_type_id, vals)
                        .map_err(|error| ctx.trap(func_id, pc, span_id, Trap::AggError(error)))?;
                    ctx.write_agg_handle(base, AggReg(dst.0), h);
                    ctx.frames[frame_index].pc = next_pc;
                }

                ExecInstr::ArrayLen { dst, arr } => {
                    let h = ctx.read_agg_handle(base, AggReg(arr.0));
                    let n = self
                        .memory
                        .aggs()
                        .array_len(h)
                        .map_err(|e| ctx.trap(func_id, pc, span_id, Trap::AggError(e)))?;
                    ctx.write_u64(base, *dst, u64::try_from(n).unwrap_or(u64::MAX));
                    ctx.frames[frame_index].pc = next_pc;
                }

                ExecInstr::ArrayGet { dst, arr, index } => {
                    let h = ctx.read_agg_handle(base, AggReg(arr.0));
                    let ix = usize::try_from(ctx.read_u64(base, *index)).unwrap_or(usize::MAX);
                    let out = self
                        .memory
                        .aggs()
                        .array_get_ref(h, ix)
                        .map_err(|e| ctx.trap(func_id, pc, span_id, Trap::AggError(e)))?;
                    ctx.write_borrowed_value(
                        base,
                        *dst,
                        out,
                        PayloadSource::Aggregate {
                            handle: h,
                            index: ix,
                        },
                    )
                    .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                    ctx.frames[frame_index].pc = next_pc;
                }

                ExecInstr::ArrayGetImm { dst, arr, index } => {
                    let h = ctx.read_agg_handle(base, AggReg(arr.0));
                    let ix = usize::try_from(*index).unwrap_or(usize::MAX);
                    let out = self
                        .memory
                        .aggs()
                        .array_get_ref(h, ix)
                        .map_err(|e| ctx.trap(func_id, pc, span_id, Trap::AggError(e)))?;
                    ctx.write_borrowed_value(
                        base,
                        *dst,
                        out,
                        PayloadSource::Aggregate {
                            handle: h,
                            index: ix,
                        },
                    )
                    .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                    ctx.frames[frame_index].pc = next_pc;
                }

                ExecInstr::TupleLen { dst, tuple } => {
                    let h = ctx.read_agg_handle(base, AggReg(tuple.0));
                    let n = self
                        .memory
                        .aggs()
                        .tuple_len(h)
                        .map_err(|e| ctx.trap(func_id, pc, span_id, Trap::AggError(e)))?;
                    ctx.write_u64(base, *dst, u64::try_from(n).unwrap_or(u64::MAX));
                    ctx.frames[frame_index].pc = next_pc;
                }

                ExecInstr::StructFieldCount { dst, st } => {
                    let h = ctx.read_agg_handle(base, AggReg(st.0));
                    let n = self
                        .memory
                        .aggs()
                        .struct_field_count(h)
                        .map_err(|e| ctx.trap(func_id, pc, span_id, Trap::AggError(e)))?;
                    ctx.write_u64(base, *dst, u64::try_from(n).unwrap_or(u64::MAX));
                    ctx.frames[frame_index].pc = next_pc;
                }

                ExecInstr::BytesLen { dst, bytes } => {
                    let b = ctx
                        .read_bytes(bytes, base, sources)
                        .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                    ctx.write_u64(base, *dst, u64::try_from(b.len()).unwrap_or(u64::MAX));
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::StrLen { dst, s } => {
                    let s = ctx
                        .read_str(s, base, sources)
                        .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                    ctx.write_u64(base, *dst, u64::try_from(s.len()).unwrap_or(u64::MAX));
                    ctx.frames[frame_index].pc = next_pc;
                }

                ExecInstr::I64Div { dst, a, b } => {
                    let a = ctx.read_i64(base, *a);
                    let b = ctx.read_i64(base, *b);
                    if b == 0 {
                        return Err(ctx.trap(func_id, pc, span_id, Trap::DivByZero));
                    }
                    if a == i64::MIN && b == -1 {
                        return Err(ctx.trap(func_id, pc, span_id, Trap::IntDivOverflow));
                    }
                    ctx.write_i64(base, *dst, a / b);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::I64Rem { dst, a, b } => {
                    let a = ctx.read_i64(base, *a);
                    let b = ctx.read_i64(base, *b);
                    if b == 0 {
                        return Err(ctx.trap(func_id, pc, span_id, Trap::DivByZero));
                    }
                    if a == i64::MIN && b == -1 {
                        return Err(ctx.trap(func_id, pc, span_id, Trap::IntDivOverflow));
                    }
                    ctx.write_i64(base, *dst, a % b);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::U64Div { dst, a, b } => {
                    let a = ctx.read_u64(base, *a);
                    let b = ctx.read_u64(base, *b);
                    if b == 0 {
                        return Err(ctx.trap(func_id, pc, span_id, Trap::DivByZero));
                    }
                    ctx.write_u64(base, *dst, a / b);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::U64Rem { dst, a, b } => {
                    let a = ctx.read_u64(base, *a);
                    let b = ctx.read_u64(base, *b);
                    if b == 0 {
                        return Err(ctx.trap(func_id, pc, span_id, Trap::DivByZero));
                    }
                    ctx.write_u64(base, *dst, a % b);
                    ctx.frames[frame_index].pc = next_pc;
                }

                ExecInstr::I64ToF64 { dst, a } => {
                    ctx.write_f64(base, *dst, ctx.read_i64(base, *a) as f64);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::U64ToF64 { dst, a } => {
                    ctx.write_f64(base, *dst, ctx.read_u64(base, *a) as f64);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::F64ToI64 { dst, a } => {
                    let a = ctx.read_f64(base, *a);
                    if !a.is_finite() {
                        return Err(ctx.trap(func_id, pc, span_id, Trap::FloatToIntInvalid));
                    }
                    // The integer maximum rounds up to the exclusive upper bound in f64.
                    if a < i64::MIN as f64 || a >= i64::MAX as f64 {
                        return Err(ctx.trap(func_id, pc, span_id, Trap::IntCastOverflow));
                    }
                    #[allow(
                        clippy::cast_possible_truncation,
                        reason = "we implement truncating float-to-int casts after explicit finite/range checks"
                    )]
                    let out = a as i64;
                    ctx.write_i64(base, *dst, out);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::F64ToU64 { dst, a } => {
                    let a = ctx.read_f64(base, *a);
                    if !a.is_finite() {
                        return Err(ctx.trap(func_id, pc, span_id, Trap::FloatToIntInvalid));
                    }
                    // The integer maximum rounds up to the exclusive upper bound in f64.
                    if a < 0.0 || a >= u64::MAX as f64 {
                        return Err(ctx.trap(func_id, pc, span_id, Trap::IntCastOverflow));
                    }
                    #[allow(
                        clippy::cast_possible_truncation,
                        reason = "we implement truncating float-to-int casts after explicit finite/range checks"
                    )]
                    let out = a as u64;
                    ctx.write_u64(base, *dst, out);
                    ctx.frames[frame_index].pc = next_pc;
                }

                ExecInstr::DecToI64 { dst, a } => {
                    let a = ctx.read_decimal(base, *a);
                    if a.scale() != 0 {
                        return Err(ctx.trap(func_id, pc, span_id, Trap::DecimalScaleMismatch));
                    }
                    let value = i64::try_from(a.mantissa())
                        .map_err(|_| ctx.trap(func_id, pc, span_id, Trap::IntCastOverflow))?;
                    ctx.write_i64(base, *dst, value);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::DecToU64 { dst, a } => {
                    let a = ctx.read_decimal(base, *a);
                    if a.scale() != 0 {
                        return Err(ctx.trap(func_id, pc, span_id, Trap::DecimalScaleMismatch));
                    }
                    let value = u64::try_from(a.mantissa())
                        .map_err(|_| ctx.trap(func_id, pc, span_id, Trap::IntCastOverflow))?;
                    ctx.write_u64(base, *dst, value);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::I64ToDec { dst, a, scale } => {
                    let mut mantissa = i128::from(ctx.read_i64(base, *a));
                    for _ in 0..*scale {
                        mantissa = mantissa
                            .checked_mul(10)
                            .ok_or_else(|| ctx.trap(func_id, pc, span_id, Trap::DecimalOverflow))?;
                    }
                    ctx.write_decimal(base, *dst, Decimal::new(mantissa, *scale));
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::U64ToDec { dst, a, scale } => {
                    let mut mantissa = i128::from(ctx.read_u64(base, *a));
                    for _ in 0..*scale {
                        mantissa = mantissa
                            .checked_mul(10)
                            .ok_or_else(|| ctx.trap(func_id, pc, span_id, Trap::DecimalOverflow))?;
                    }
                    ctx.write_decimal(base, *dst, Decimal::new(mantissa, *scale));
                    ctx.frames[frame_index].pc = next_pc;
                }

                ExecInstr::BytesEq { dst, a, b } => {
                    let work = {
                        let a = ctx
                            .read_bytes(a, base, sources)
                            .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                        let b = ctx
                            .read_bytes(b, base, sources)
                            .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                        if a.len() == b.len() { a.len() } else { 0 }
                    };
                    ctx.charge_payload_bytes(work, 1)
                        .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                    let a = ctx
                        .read_bytes(a, base, sources)
                        .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                    let b = ctx
                        .read_bytes(b, base, sources)
                        .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                    ctx.write_bool(base, *dst, a == b);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::StrEq { dst, a, b } => {
                    let work = {
                        let a = ctx
                            .read_str(a, base, sources)
                            .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                        let b = ctx
                            .read_str(b, base, sources)
                            .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                        if a.len() == b.len() { a.len() } else { 0 }
                    };
                    ctx.charge_payload_bytes(work, 1)
                        .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                    let a = ctx
                        .read_str(a, base, sources)
                        .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                    let b = ctx
                        .read_str(b, base, sources)
                        .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                    ctx.write_bool(base, *dst, a == b);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::BytesConcat { dst, a, b } => {
                    let len = {
                        let a = ctx
                            .read_bytes(a, base, sources)
                            .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                        let b = ctx
                            .read_bytes(b, base, sources)
                            .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                        a.len()
                            .checked_add(b.len())
                            .filter(|&len| isize::try_from(len).is_ok())
                            .ok_or_else(|| {
                                ctx.trap(func_id, pc, span_id, Trap::PayloadSizeOverflow)
                            })?
                    };
                    ctx.charge_payload_bytes(len, 1)
                        .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                    let out = {
                        let a = ctx
                            .read_bytes(a, base, sources)
                            .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                        let b = ctx
                            .read_bytes(b, base, sources)
                            .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                        let mut out = Vec::with_capacity(len);
                        out.extend_from_slice(a);
                        out.extend_from_slice(b);
                        out
                    };
                    let h = ctx.arena.alloc_bytes(out);
                    ctx.write_bytes_handle(base, *dst, h);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::StrConcat { dst, a, b } => {
                    let len = {
                        let a = ctx
                            .read_str(a, base, sources)
                            .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                        let b = ctx
                            .read_str(b, base, sources)
                            .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                        a.len()
                            .checked_add(b.len())
                            .filter(|&len| isize::try_from(len).is_ok())
                            .ok_or_else(|| {
                                ctx.trap(func_id, pc, span_id, Trap::PayloadSizeOverflow)
                            })?
                    };
                    ctx.charge_payload_bytes(len, 1)
                        .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                    let out = {
                        let a = ctx
                            .read_str(a, base, sources)
                            .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                        let b = ctx
                            .read_str(b, base, sources)
                            .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                        let mut out = String::with_capacity(len);
                        out.push_str(a);
                        out.push_str(b);
                        out
                    };
                    let h = ctx.arena.alloc_str(out);
                    ctx.write_str_handle(base, *dst, h);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::BytesGet { dst, bytes, index } => {
                    let bytes = ctx
                        .read_bytes(bytes, base, sources)
                        .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                    let index = usize::try_from(ctx.read_u64(base, *index)).unwrap_or(usize::MAX);
                    let b = bytes
                        .get(index)
                        .ok_or_else(|| ctx.trap(func_id, pc, span_id, Trap::IndexOutOfBounds))?;
                    ctx.write_u64(base, *dst, u64::from(*b));
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::BytesGetImm { dst, bytes, index } => {
                    let bytes = ctx
                        .read_bytes(bytes, base, sources)
                        .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                    let index = usize::try_from(*index).unwrap_or(usize::MAX);
                    let b = bytes
                        .get(index)
                        .ok_or_else(|| ctx.trap(func_id, pc, span_id, Trap::IndexOutOfBounds))?;
                    ctx.write_u64(base, *dst, u64::from(*b));
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::BytesSlice {
                    dst,
                    bytes,
                    start,
                    end,
                } => {
                    let start = usize::try_from(ctx.read_u64(base, *start)).unwrap_or(usize::MAX);
                    let end = usize::try_from(ctx.read_u64(base, *end)).unwrap_or(usize::MAX);
                    let len = ctx
                        .read_bytes(bytes, base, sources)
                        .map_err(|t| ctx.trap(func_id, pc, span_id, t))?
                        .get(start..end)
                        .ok_or_else(|| ctx.trap(func_id, pc, span_id, Trap::IndexOutOfBounds))?
                        .len();
                    ctx.charge_payload_bytes(len, 1)
                        .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                    let out = ctx
                        .read_bytes(bytes, base, sources)
                        .map_err(|t| ctx.trap(func_id, pc, span_id, t))?[start..end]
                        .to_vec();
                    let h = ctx.arena.alloc_bytes(out);
                    ctx.write_bytes_handle(base, *dst, h);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::StrSlice { dst, s, start, end } => {
                    let start = usize::try_from(ctx.read_u64(base, *start)).unwrap_or(usize::MAX);
                    let end = usize::try_from(ctx.read_u64(base, *end)).unwrap_or(usize::MAX);
                    {
                        let s = ctx
                            .read_str(s, base, sources)
                            .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                        if start > end || end > s.len() {
                            return Err(ctx.trap(func_id, pc, span_id, Trap::IndexOutOfBounds));
                        }
                        if !s.is_char_boundary(start) || !s.is_char_boundary(end) {
                            return Err(ctx.trap(func_id, pc, span_id, Trap::StrNotCharBoundary));
                        }
                    }
                    ctx.charge_payload_bytes(end - start, 1)
                        .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                    let out = String::from(
                        &ctx.read_str(s, base, sources)
                            .map_err(|t| ctx.trap(func_id, pc, span_id, t))?
                            [start..end],
                    );
                    let h = ctx.arena.alloc_str(out);
                    ctx.write_str_handle(base, *dst, h);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::StrToBytes { dst, s } => {
                    let len = ctx
                        .read_str(s, base, sources)
                        .map_err(|t| ctx.trap(func_id, pc, span_id, t))?
                        .len();
                    ctx.charge_payload_bytes(len, 1)
                        .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                    let out = {
                        let s = ctx
                            .read_str(s, base, sources)
                            .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                        s.as_bytes().to_vec()
                    };
                    let h = ctx.arena.alloc_bytes(out);
                    ctx.write_bytes_handle(base, *dst, h);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::BytesToStr { dst, bytes } => {
                    let len = ctx
                        .read_bytes(bytes, base, sources)
                        .map_err(|t| ctx.trap(func_id, pc, span_id, t))?
                        .len();
                    ctx.charge_payload_bytes(len, 2)
                        .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                    let out = {
                        let bytes = ctx
                            .read_bytes(bytes, base, sources)
                            .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                        String::from_utf8(bytes.to_vec())
                            .map_err(|_| ctx.trap(func_id, pc, span_id, Trap::InvalidUtf8))?
                    };
                    let h = ctx.arena.alloc_str(out);
                    ctx.write_str_handle(base, *dst, h);
                    ctx.frames[frame_index].pc = next_pc;
                }
            }
            P::registers(
                &mut register_state,
                trace,
                program_ref,
                ctx,
                func_id,
                pc,
                base,
                ValueSources::new(program_ref, args, self.memory.aggs()),
                crate::trace::RegisterPhase::Outputs,
            );
            P::instr_end(
                trace,
                program_ref,
                ctx.frames.last().map(|frame| TraceLocation {
                    func: frame.func,
                    pc: frame.pc,
                }),
            );
        }
    }

    fn contract_value(ctx: &ExecutionContext, base: RegBase, register: VReg) -> Value {
        match register {
            VReg::Agg(reg) => Value::Agg(ctx.read_agg_handle(base, reg)),
            VReg::Func(reg) => Value::Func(ctx.read_func(base, reg)),
            VReg::Closure(reg) => Value::Closure(ctx.read_closure(base, reg)),
            _ => unreachable!("refined contract kind"),
        }
    }

    fn check_arg_contracts(
        &self,
        ctx: &mut ExecutionContext,
        program: &Program,
        callee: FuncId,
        vf: &ExecFunc,
        base: RegBase,
    ) -> Result<(), Trap> {
        for check in &vf.contract_checks[..vf.arg_check_count as usize] {
            let value =
                Self::contract_value(ctx, base, vf.reg_layout.arg_regs[check.index as usize]);
            ctx.contract_scratch
                .check_value(
                    program,
                    self.memory.aggs(),
                    &value,
                    check.contract,
                    self.limits.max_contract_work,
                )
                .map_err(|error| {
                    self.contract_trap(
                        callee,
                        ContractPosition::Argument {
                            callee,
                            index: check.index,
                        },
                        error,
                    )
                })?;
        }
        Ok(())
    }

    fn check_result_contracts(
        &self,
        ctx: &mut ExecutionContext,
        program: &Program,
        function: FuncId,
        vf: &ExecFunc,
        base: RegBase,
        rets: &[VReg],
    ) -> Result<(), Trap> {
        for check in &vf.contract_checks[vf.arg_check_count as usize..] {
            let value = Self::contract_value(ctx, base, rets[check.index as usize]);
            ctx.contract_scratch
                .check_value(
                    program,
                    self.memory.aggs(),
                    &value,
                    check.contract,
                    self.limits.max_contract_work,
                )
                .map_err(|error| {
                    self.contract_trap(
                        function,
                        ContractPosition::Result { index: check.index },
                        error,
                    )
                })?;
        }
        Ok(())
    }

    fn contract_trap(
        &self,
        function: FuncId,
        position: ContractPosition,
        error: CheckError,
    ) -> Trap {
        match error {
            CheckError::Mismatch(error) => Trap::ContractMismatch {
                function,
                position,
                error,
            },
            CheckError::Limit => Trap::ContractWorkExceeded {
                function,
                position,
                limit: self.limits.max_contract_work,
            },
        }
    }

    /// Returns the embedded host, for inspecting state between runs.
    #[must_use]
    pub fn host(&self) -> &H {
        &self.host
    }

    /// Returns the embedded host, for updating host-owned state between runs.
    pub fn host_mut(&mut self) -> &mut H {
        &mut self.host
    }

    /// Returns a reference to the aggregate heap.
    pub fn aggregates(&self) -> &AggHeap {
        self.memory.aggs()
    }

    /// Returns a mutable reference to the aggregate heap.
    pub fn aggregates_mut(&mut self) -> &mut AggHeap {
        self.memory.aggs_mut()
    }
}

impl ExecutionContext {
    fn alloc_frame(&mut self, vf: &ExecFunc) -> RegBase {
        let counts = vf.reg_layout.counts;
        let base = RegBase {
            unit: self.units.len(),
            bools: self.bools.len(),
            i64s: self.i64s.len(),
            u64s: self.u64s.len(),
            f64s: self.f64s.len(),
            decimals: self.decimals.len(),
            bytes: self.bytes.len(),
            strs: self.strs.len(),
            objs: self.objs.len(),
            aggs: self.aggs.len(),
            funcs: self.funcs.len(),
            closures: self.closures.len(),
        };

        self.units.resize(base.unit + counts.unit, 0);
        self.bools.resize(base.bools + counts.bools, false);
        self.i64s.resize(base.i64s + counts.i64s, 0);
        self.u64s.resize(base.u64s + counts.u64s, 0);
        self.f64s.resize(base.f64s + counts.f64s, 0.0);
        self.decimals
            .resize(base.decimals + counts.decimals, Decimal::new(0, 0));
        self.bytes.resize(base.bytes + counts.bytes, BytesHandle(0));
        self.strs.resize(base.strs + counts.strs, StrHandle(0));
        self.objs.resize(
            base.objs + counts.objs,
            Obj {
                host_type: crate::program::HostTypeId(0),
                handle: ObjHandle(0),
            },
        );
        self.aggs.resize(base.aggs + counts.aggs, AggHandle(0));
        self.funcs.resize(base.funcs + counts.funcs, FuncId(0));
        self.closures.resize(
            base.closures + counts.closures,
            Closure {
                func: FuncId(0),
                env: AggHandle(0),
            },
        );

        base
    }

    fn truncate_to(&mut self, base: RegBase) {
        self.units.truncate(base.unit);
        self.bools.truncate(base.bools);
        self.i64s.truncate(base.i64s);
        self.u64s.truncate(base.u64s);
        self.f64s.truncate(base.f64s);
        self.decimals.truncate(base.decimals);
        self.bytes.truncate(base.bytes);
        self.strs.truncate(base.strs);
        self.objs.truncate(base.objs);
        self.aggs.truncate(base.aggs);
        self.funcs.truncate(base.funcs);
        self.closures.truncate(base.closures);
    }

    fn init_args(&mut self, base: RegBase, vf: &ExecFunc, args: &[Value]) -> Result<(), Trap> {
        if args.len() != vf.reg_layout.arg_regs.len() {
            return Err(Trap::InvalidPc);
        }
        for (index, (dst, v)) in vf
            .reg_layout
            .arg_regs
            .iter()
            .copied()
            .zip(args.iter())
            .enumerate()
        {
            self.write_borrowed_value(base, dst, v, PayloadSource::Argument(index))?;
        }
        Ok(())
    }

    fn write_owned_value(&mut self, base: RegBase, dst: VReg, value: Value) -> Result<(), Trap> {
        match (dst, value) {
            (VReg::Bytes(reg), Value::Bytes(bytes)) => {
                let handle = self.arena.alloc_bytes(bytes);
                self.write_bytes_handle(base, reg, handle);
                Ok(())
            }
            (VReg::Str(reg), Value::Str(s)) => {
                let handle = self.arena.alloc_str(s);
                self.write_str_handle(base, reg, handle);
                Ok(())
            }
            (_, value) => self.write_inline_value(base, dst, &value),
        }
    }

    fn write_borrowed_value(
        &mut self,
        base: RegBase,
        dst: VReg,
        value: &Value,
        source: PayloadSource,
    ) -> Result<(), Trap> {
        match (dst, value) {
            (VReg::Bytes(reg), Value::Bytes(_)) => {
                let handle = self.arena.borrow_bytes(source);
                self.write_bytes_handle(base, reg, handle);
                Ok(())
            }
            (VReg::Str(reg), Value::Str(_)) => {
                let handle = self.arena.borrow_str(source);
                self.write_str_handle(base, reg, handle);
                Ok(())
            }
            _ => self.write_inline_value(base, dst, value),
        }
    }

    fn write_inline_value(&mut self, base: RegBase, dst: VReg, v: &Value) -> Result<(), Trap> {
        match (dst, v) {
            (VReg::Unit(r), Value::Unit) => {
                self.write_unit(base, r, 0);
                Ok(())
            }
            (VReg::Bool(r), Value::Bool(b)) => {
                self.write_bool(base, r, *b);
                Ok(())
            }
            (VReg::I64(r), Value::I64(i)) => {
                self.write_i64(base, r, *i);
                Ok(())
            }
            (VReg::U64(r), Value::U64(u)) => {
                self.write_u64(base, r, *u);
                Ok(())
            }
            (VReg::F64(r), Value::F64(f)) => {
                self.write_f64(base, r, *f);
                Ok(())
            }
            (VReg::Decimal(r), Value::Decimal(d)) => {
                self.write_decimal(base, r, *d);
                Ok(())
            }
            (VReg::Obj(r), Value::Obj(o)) => {
                self.write_obj(base, r, *o);
                Ok(())
            }
            (VReg::Agg(r), Value::Agg(h)) => {
                self.write_agg_handle(base, r, *h);
                Ok(())
            }
            (VReg::Func(r), Value::Func(f)) => {
                self.write_func(base, r, *f);
                Ok(())
            }
            (VReg::Closure(r), Value::Closure(c)) => {
                self.write_closure(base, r, *c);
                Ok(())
            }
            _ => Err(Trap::InvalidPc),
        }
    }

    fn materialize_vreg(
        &mut self,
        base: RegBase,
        v: VReg,
        sources: ValueSources<'_>,
    ) -> Result<Value, Trap> {
        Ok(match v {
            VReg::Unit(_) => Value::Unit,
            VReg::Bool(r) => Value::Bool(self.read_bool(base, r)),
            VReg::I64(r) => Value::I64(self.read_i64(base, r)),
            VReg::U64(r) => Value::U64(self.read_u64(base, r)),
            VReg::F64(r) => Value::F64(self.read_f64(base, r)),
            VReg::Decimal(r) => Value::Decimal(self.read_decimal(base, r)),
            VReg::Bytes(r) => {
                let handle = self.read_bytes_handle(base, r);
                let len = self
                    .arena
                    .bytes(handle, sources)
                    .ok_or(Trap::InvalidPc)?
                    .len();
                self.charge_payload_bytes(len, 1)?;
                Value::Bytes(
                    self.arena
                        .bytes(handle, sources)
                        .ok_or(Trap::InvalidPc)?
                        .to_vec(),
                )
            }
            VReg::Str(r) => {
                let handle = self.read_str_handle(base, r);
                let len = self
                    .arena
                    .str(handle, sources)
                    .ok_or(Trap::InvalidPc)?
                    .len();
                self.charge_payload_bytes(len, 1)?;
                Value::Str(String::from(
                    self.arena.str(handle, sources).ok_or(Trap::InvalidPc)?,
                ))
            }
            VReg::Obj(r) => Value::Obj(self.read_obj(base, r)),
            VReg::Agg(r) => Value::Agg(self.read_agg_handle(base, r)),
            VReg::Func(r) => Value::Func(self.read_func(base, r)),
            VReg::Closure(r) => Value::Closure(self.read_closure(base, r)),
        })
    }

    fn copy_vreg(
        &mut self,
        src_base: RegBase,
        src: VReg,
        dst_base: RegBase,
        dst: VReg,
    ) -> Result<(), Trap> {
        match (src, dst) {
            (VReg::Unit(s), VReg::Unit(d)) => {
                let v = self.read_unit(src_base, s);
                self.write_unit(dst_base, d, v);
                Ok(())
            }
            (VReg::Bool(s), VReg::Bool(d)) => {
                let v = self.read_bool(src_base, s);
                self.write_bool(dst_base, d, v);
                Ok(())
            }
            (VReg::I64(s), VReg::I64(d)) => {
                let v = self.read_i64(src_base, s);
                self.write_i64(dst_base, d, v);
                Ok(())
            }
            (VReg::U64(s), VReg::U64(d)) => {
                let v = self.read_u64(src_base, s);
                self.write_u64(dst_base, d, v);
                Ok(())
            }
            (VReg::F64(s), VReg::F64(d)) => {
                let v = self.read_f64(src_base, s);
                self.write_f64(dst_base, d, v);
                Ok(())
            }
            (VReg::Decimal(s), VReg::Decimal(d)) => {
                let v = self.read_decimal(src_base, s);
                self.write_decimal(dst_base, d, v);
                Ok(())
            }
            (VReg::Bytes(s), VReg::Bytes(d)) => {
                let v = self.read_bytes_handle(src_base, s);
                self.write_bytes_handle(dst_base, d, v);
                Ok(())
            }
            (VReg::Str(s), VReg::Str(d)) => {
                let v = self.read_str_handle(src_base, s);
                self.write_str_handle(dst_base, d, v);
                Ok(())
            }
            (VReg::Obj(s), VReg::Obj(d)) => {
                let v = self.read_obj(src_base, s);
                self.write_obj(dst_base, d, v);
                Ok(())
            }
            (VReg::Agg(s), VReg::Agg(d)) => {
                let v = self.read_agg_handle(src_base, s);
                self.write_agg_handle(dst_base, d, v);
                Ok(())
            }
            (VReg::Func(s), VReg::Func(d)) => {
                let v = self.read_func(src_base, s);
                self.write_func(dst_base, d, v);
                Ok(())
            }
            (VReg::Closure(s), VReg::Closure(d)) => {
                let v = self.read_closure(src_base, s);
                self.write_closure(dst_base, d, v);
                Ok(())
            }
            _ => Err(Trap::InvalidPc),
        }
    }

    #[inline(always)]
    #[must_use]
    fn read_unit(&self, base: RegBase, r: UnitReg) -> u32 {
        self.units[base.unit + r.0 as usize]
    }
    #[inline(always)]
    fn write_unit(&mut self, base: RegBase, r: UnitReg, v: u32) {
        self.units[base.unit + r.0 as usize] = v;
    }

    #[inline(always)]
    #[must_use]
    fn read_bool(&self, base: RegBase, r: BoolReg) -> bool {
        self.bools[base.bools + r.0 as usize]
    }
    #[inline(always)]
    fn write_bool(&mut self, base: RegBase, r: BoolReg, v: bool) {
        self.bools[base.bools + r.0 as usize] = v;
    }

    #[inline(always)]
    #[must_use]
    fn read_i64(&self, base: RegBase, r: I64Reg) -> i64 {
        self.i64s[base.i64s + r.0 as usize]
    }
    #[inline(always)]
    fn write_i64(&mut self, base: RegBase, r: I64Reg, v: i64) {
        self.i64s[base.i64s + r.0 as usize] = v;
    }

    #[inline(always)]
    #[must_use]
    fn read_u64(&self, base: RegBase, r: U64Reg) -> u64 {
        self.u64s[base.u64s + r.0 as usize]
    }
    #[inline(always)]
    fn write_u64(&mut self, base: RegBase, r: U64Reg, v: u64) {
        self.u64s[base.u64s + r.0 as usize] = v;
    }

    #[inline(always)]
    #[must_use]
    fn read_f64(&self, base: RegBase, r: F64Reg) -> f64 {
        self.f64s[base.f64s + r.0 as usize]
    }
    #[inline(always)]
    fn write_f64(&mut self, base: RegBase, r: F64Reg, v: f64) {
        self.f64s[base.f64s + r.0 as usize] = v;
    }

    #[inline(always)]
    #[must_use]
    fn read_decimal(&self, base: RegBase, r: DecimalReg) -> Decimal {
        self.decimals[base.decimals + r.0 as usize]
    }
    #[inline(always)]
    fn write_decimal(&mut self, base: RegBase, r: DecimalReg, v: Decimal) {
        self.decimals[base.decimals + r.0 as usize] = v;
    }

    #[inline(always)]
    #[must_use]
    fn read_bytes_handle(&self, base: RegBase, r: BytesReg) -> BytesHandle {
        self.bytes[base.bytes + r.0 as usize]
    }
    #[inline(always)]
    fn write_bytes_handle(&mut self, base: RegBase, r: BytesReg, v: BytesHandle) {
        self.bytes[base.bytes + r.0 as usize] = v;
    }

    #[inline(always)]
    #[must_use]
    fn read_str_handle(&self, base: RegBase, r: StrReg) -> StrHandle {
        self.strs[base.strs + r.0 as usize]
    }
    #[inline(always)]
    fn write_str_handle(&mut self, base: RegBase, r: StrReg, v: StrHandle) {
        self.strs[base.strs + r.0 as usize] = v;
    }

    #[inline(always)]
    #[must_use]
    fn read_obj(&self, base: RegBase, r: ObjReg) -> Obj {
        self.objs[base.objs + r.0 as usize]
    }
    #[inline(always)]
    fn write_obj(&mut self, base: RegBase, r: ObjReg, v: Obj) {
        self.objs[base.objs + r.0 as usize] = v;
    }

    #[inline(always)]
    #[must_use]
    fn read_agg_handle(&self, base: RegBase, r: AggReg) -> AggHandle {
        self.aggs[base.aggs + r.0 as usize]
    }
    #[inline(always)]
    fn write_agg_handle(&mut self, base: RegBase, r: AggReg, v: AggHandle) {
        self.aggs[base.aggs + r.0 as usize] = v;
    }

    #[inline(always)]
    #[must_use]
    fn read_func(&self, base: RegBase, r: FuncReg) -> FuncId {
        self.funcs[base.funcs + r.0 as usize]
    }
    #[inline(always)]
    fn write_func(&mut self, base: RegBase, r: FuncReg, v: FuncId) {
        self.funcs[base.funcs + r.0 as usize] = v;
    }

    #[inline(always)]
    #[must_use]
    fn read_closure(&self, base: RegBase, r: ClosureReg) -> Closure {
        self.closures[base.closures + r.0 as usize]
    }
    #[inline(always)]
    fn write_closure(&mut self, base: RegBase, r: ClosureReg, v: Closure) {
        self.closures[base.closures + r.0 as usize] = v;
    }

    #[inline]
    #[must_use = "reads can trap; handle the Result"]
    fn read_bytes<'a>(
        &'a self,
        reg: &BytesReg,
        base: RegBase,
        sources: ValueSources<'a>,
    ) -> Result<&'a [u8], Trap> {
        let h = self.read_bytes_handle(base, *reg);
        self.arena.bytes(h, sources).ok_or(Trap::InvalidPc)
    }

    #[inline]
    #[must_use = "reads can trap; handle the Result"]
    fn read_str<'a>(
        &'a self,
        reg: &StrReg,
        base: RegBase,
        sources: ValueSources<'a>,
    ) -> Result<&'a str, Trap> {
        let h = self.read_str_handle(base, *reg);
        self.arena.str(h, sources).ok_or(Trap::InvalidPc)
    }

    #[inline(always)]
    #[must_use]
    fn cur_func(&self) -> FuncId {
        self.frames.last().map(|f| f.func).unwrap_or(FuncId(0))
    }

    #[inline(always)]
    #[must_use]
    fn cur_pc(&self) -> u32 {
        self.frames.last().map(|f| f.pc).unwrap_or(0)
    }

    #[inline]
    #[must_use]
    fn cur_span(&self, program: &Program) -> Option<u64> {
        self.span_at(program, self.cur_func(), self.cur_pc())
    }

    #[inline]
    #[must_use]
    fn span_at(&self, program: &Program, func: FuncId, pc: u32) -> Option<u64> {
        let f = program.functions.get(func.0 as usize)?;
        let spans = f.spans(program).ok()?;
        let mut cur_pc: u64 = 0;
        let mut out: Option<u64> = None;
        for s in spans {
            cur_pc = cur_pc.saturating_add(s.pc_delta);
            if cur_pc > u64::from(pc) {
                break;
            }
            out = Some(s.span_id.get());
        }
        out
    }

    fn trap(&self, func: FuncId, pc: u32, span_id: Option<u64>, trap: Trap) -> TrapInfo {
        TrapInfo {
            func,
            pc,
            span_id,
            trap,
        }
    }
}

impl Value {
    /// Returns the corresponding [`ValueType`] tag.
    #[inline]
    #[must_use]
    pub fn value_type(&self) -> ValueType {
        match self {
            Self::Unit => ValueType::Unit,
            Self::Bool(_) => ValueType::Bool,
            Self::I64(_) => ValueType::I64,
            Self::U64(_) => ValueType::U64,
            Self::F64(_) => ValueType::F64,
            Self::Decimal(_) => ValueType::Decimal,
            Self::Bytes(_) => ValueType::Bytes,
            Self::Str(_) => ValueType::Str,
            Self::Obj(o) => ValueType::Obj(o.host_type),
            Self::Agg(_) => ValueType::Agg,
            Self::Func(_) => ValueType::Func,
            Self::Closure(_) => ValueType::Closure,
        }
    }

    #[inline]
    pub(crate) fn check_type(&self, expected: ValueType) -> Result<(), Trap> {
        let actual = self.value_type();
        if actual != expected {
            return Err(Trap::TypeMismatch { expected, actual });
        }
        Ok(())
    }
}

#[inline]
fn validate_entry_args(program: &Program, entry_fn: &Function, args: &[Value]) -> Result<(), Trap> {
    let arg_types = entry_fn.arg_types(program).map_err(|_| Trap::InvalidPc)?;
    if arg_types.len() != args.len() {
        return Err(Trap::ArityMismatch);
    }
    for (v, &t) in args.iter().zip(arg_types.iter()) {
        v.check_type(t)?;
    }
    Ok(())
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum IndirectCalleeKind {
    Func,
    Closure,
}

#[inline]
fn validate_indirect_call_signature(
    verified: &VerifiedProgram,
    call_sig: crate::program::CallSigId,
    callee_func: FuncId,
    callee_fn: &Function,
    callee_kind: IndirectCalleeKind,
) -> Result<(), Trap> {
    let expected = verified
        .call_sig_fingerprint(call_sig)
        .ok_or(Trap::InvalidPc)?;
    let actual = match callee_kind {
        IndirectCalleeKind::Func => verified.function_direct_sig_fingerprint(callee_func),
        IndirectCalleeKind::Closure => verified.function_closure_sig_fingerprint(callee_func),
    };

    if let Some(actual) = actual {
        // Fast path: both hash and canonical signature key match.
        if expected.hash == actual.hash && expected.key == actual.key {
            return Ok(());
        }
    }

    // Preserve exact trap behavior and remain collision-safe by validating the full type vectors
    // whenever fingerprints do not prove equivalence.
    validate_indirect_call_signature_types(
        verified.program(),
        call_sig,
        callee_fn,
        matches!(callee_kind, IndirectCalleeKind::Closure),
    )?;
    Err(Trap::CallableSignatureMismatch {
        signature: call_sig,
        function: callee_func,
    })
}

#[inline]
fn validate_indirect_call_signature_types(
    program: &Program,
    call_sig: crate::program::CallSigId,
    callee_fn: &Function,
    is_closure: bool,
) -> Result<(), Trap> {
    let call_sig_entry = program.call_sig(call_sig).ok_or(Trap::InvalidPc)?;
    let expected_args = program
        .call_sig_args(call_sig_entry)
        .map_err(|_| Trap::InvalidPc)?;
    let expected_rets = program
        .call_sig_rets(call_sig_entry)
        .map_err(|_| Trap::InvalidPc)?;
    let actual_args = callee_fn.arg_types(program).map_err(|_| Trap::InvalidPc)?;
    let actual_rets = callee_fn.ret_types(program).map_err(|_| Trap::InvalidPc)?;

    if is_closure {
        let Some((first, rest)) = actual_args.split_first() else {
            return Err(Trap::ArityMismatch);
        };
        if *first != ValueType::Agg {
            return Err(Trap::TypeMismatch {
                expected: ValueType::Agg,
                actual: *first,
            });
        }
        validate_signature_types(expected_args, rest.iter())?;
    } else {
        validate_signature_types(expected_args, actual_args.iter())?;
    }

    validate_signature_types(expected_rets, actual_rets.iter())
}

#[inline]
fn validate_signature_types<'a>(
    expected: crate::contract::ContractKinds<'_>,
    actual: impl ExactSizeIterator<Item = &'a ValueType>,
) -> Result<(), Trap> {
    if expected.len() != actual.len() {
        return Err(Trap::ArityMismatch);
    }
    for (&expected, &actual) in expected.iter().zip(actual) {
        if expected != actual {
            return Err(Trap::TypeMismatch { expected, actual });
        }
    }
    Ok(())
}

#[inline]
#[must_use = "host ABI reads can trap; handle the Result"]
fn read_value_ref_at<'a>(
    arena: &'a ValueArena,
    sources: ValueSources<'a>,
    units: &'a [u32],
    bools: &'a [bool],
    i64s: &'a [i64],
    u64s: &'a [u64],
    f64s: &'a [f64],
    decimals: &'a [Decimal],
    bytes: &'a [BytesHandle],
    strs: &'a [StrHandle],
    objs: &'a [Obj],
    aggs: &'a [AggHandle],
    funcs: &'a [FuncId],
    closures: &'a [Closure],
    base: RegBase,
    v: VReg,
) -> Result<ValueRef<'a>, Trap> {
    Ok(match v {
        VReg::Unit(r) => {
            let _ = units[base.unit + r.0 as usize];
            ValueRef::Unit
        }
        VReg::Bool(r) => ValueRef::Bool(bools[base.bools + r.0 as usize]),
        VReg::I64(r) => ValueRef::I64(i64s[base.i64s + r.0 as usize]),
        VReg::U64(r) => ValueRef::U64(u64s[base.u64s + r.0 as usize]),
        VReg::F64(r) => ValueRef::F64(f64s[base.f64s + r.0 as usize]),
        VReg::Decimal(r) => ValueRef::Decimal(&decimals[base.decimals + r.0 as usize]),
        VReg::Bytes(r) => {
            let h = bytes[base.bytes + r.0 as usize];
            ValueRef::Bytes(arena.bytes(h, sources).ok_or(Trap::InvalidPc)?)
        }
        VReg::Str(r) => {
            let h = strs[base.strs + r.0 as usize];
            ValueRef::Str(arena.str(h, sources).ok_or(Trap::InvalidPc)?)
        }
        VReg::Obj(r) => ValueRef::Obj(objs[base.objs + r.0 as usize]),
        VReg::Agg(r) => ValueRef::Agg(aggs[base.aggs + r.0 as usize]),
        VReg::Func(r) => ValueRef::Func(funcs[base.funcs + r.0 as usize]),
        VReg::Closure(r) => ValueRef::Closure(closures[base.closures + r.0 as usize]),
    })
}

#[inline]
fn f64_min_basic(a: f64, b: f64) -> f64 {
    if a < b {
        a
    } else if a > b {
        b
    } else if a == 0.0 {
        if a.is_sign_negative() || b.is_sign_negative() {
            -0.0
        } else {
            0.0
        }
    } else {
        a
    }
}

#[inline]
fn f64_max_basic(a: f64, b: f64) -> f64 {
    if a > b {
        a
    } else if a < b {
        b
    } else if a == 0.0 {
        if a.is_sign_positive() || b.is_sign_positive() {
            0.0
        } else {
            -0.0
        }
    } else {
        a
    }
}

#[inline]
fn f64_min(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else {
        f64_min_basic(a, b)
    }
}

#[inline]
fn f64_max(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else {
        f64_max_basic(a, b)
    }
}

#[inline]
fn f64_min_num(a: f64, b: f64) -> f64 {
    match (a.is_nan(), b.is_nan()) {
        (true, true) => f64::NAN,
        (true, false) => b,
        (false, true) => a,
        (false, false) => f64_min_basic(a, b),
    }
}

#[inline]
fn f64_max_num(a: f64, b: f64) -> f64 {
    match (a.is_nan(), b.is_nan()) {
        (true, true) => f64::NAN,
        (true, false) => b,
        (false, true) => a,
        (false, false) => f64_max_basic(a, b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clearing_context_releases_payloads_and_preserves_register_capacity() {
        let mut ctx = ExecutionContext::new();
        let bytes = ctx.arena.alloc_bytes(vec![7; 8192]);
        let text = ctx.arena.alloc_str(String::from("computed"));
        ctx.i64s.extend([1, 2, 3]);
        let capacity = ctx.i64s.capacity();
        let program = ProgramBuilder::new().build();
        let heap = AggHeap::new();
        let sources = ValueSources::new(&program, &[], &heap);
        assert_eq!(ctx.arena.bytes(bytes, sources).unwrap().len(), 8192);
        assert_eq!(ctx.arena.str(text, sources), Some("computed"));
        ctx.clear();
        assert_eq!(ctx.arena.bytes(bytes, sources), None);
        assert_eq!(ctx.arena.str(text, sources), None);
        assert!(ctx.i64s.is_empty());
        assert_eq!(ctx.i64s.capacity(), capacity);
    }
    use crate::asm::{Asm, FunctionSig, ProgramBuilder};
    use crate::bytecode::Instr;
    use crate::host::{AccessSink, HostContext, HostSig, ResourceKeyRef, SigHash};
    use crate::program::{ByteRange, CallSigEntry, FunctionDef, Program, TypeTableDef, ValueType};
    use crate::trace::{TraceMask, TraceOutcome, TraceSink};
    use crate::value::AggType;
    use crate::verifier::{VerifyConfig, verify_program_owned};
    use alloc::format;
    use alloc::vec;
    use alloc::vec::Vec;

    #[test]
    fn aggregate_contracts_are_checked_before_entry_and_every_call_kind() {
        for mode in 0..4 {
            let mut pb = ProgramBuilder::new();
            let integer = pb.contract(Contract::Kind(ValueType::I64)).unwrap();
            let tuple_contract = pb.contract(Contract::Tuple(&[Some(integer)])).unwrap();
            let mut body = Asm::new();
            body.set_register_types(vec![ValueType::Unit, ValueType::Agg, ValueType::I64]);
            body.tuple_get(2, 1, 0);
            body.ret(0, &[2]);
            let target = pb
                .push_function_checked(
                    body,
                    crate::asm::FunctionAbi {
                        args: vec![tuple_contract.into()],
                        rets: vec![ValueType::I64.into()],
                    },
                )
                .unwrap();
            let entry = if mode == 0 {
                target
            } else {
                let mut caller = Asm::new();
                caller.set_register_types(vec![
                    ValueType::Unit,
                    ValueType::Agg,
                    ValueType::Func,
                    ValueType::Closure,
                    ValueType::I64,
                ]);
                match mode {
                    1 => {
                        caller.call(0, target, 0, &[1], &[4]);
                    }
                    2 => {
                        let sig = pb
                            .call_sig(crate::asm::FunctionAbi {
                                args: vec![tuple_contract.into()],
                                rets: vec![ValueType::I64.into()],
                            })
                            .unwrap();
                        caller.const_func(2, target);
                        caller.call_indirect(0, sig, 2, 0, &[1], &[4]);
                    }
                    _ => {
                        let sig = pb
                            .call_sig(FunctionSig {
                                arg_types: vec![],
                                ret_types: vec![ValueType::I64],
                            })
                            .unwrap();
                        caller.const_func(2, target);
                        caller.closure_new(3, 2, 1);
                        caller.call_indirect(0, sig, 3, 0, &[], &[4]);
                    }
                }
                caller.ret(0, &[4]);
                pb.push_function_checked(
                    caller,
                    FunctionSig {
                        arg_types: vec![ValueType::Agg],
                        ret_types: vec![ValueType::I64],
                    },
                )
                .unwrap()
            };
            let p = pb.build_verified().unwrap();
            let mut vm = Vm::new(TestHost, Limits::default());
            let wrong = vm
                .aggregates_mut()
                .tuple_new(vec![Value::Bool(true)])
                .unwrap();
            let good = vm.aggregates_mut().tuple_new(vec![Value::I64(42)]).unwrap();
            let mut ctx = ExecutionContext::new();
            let mut outputs = Vec::new();
            for handle in [wrong, AggHandle(u32::MAX)] {
                let e = vm
                    .run_with_ctx(
                        &mut ctx,
                        &p,
                        entry,
                        &[Value::Agg(handle)],
                        &mut outputs,
                        TraceMask::NONE,
                        None,
                        None,
                    )
                    .unwrap_err();
                assert_eq!(e.func, entry);
                assert!(
                    matches!(e.trap, Trap::ContractMismatch { position:ContractPosition::Argument{callee,index:0}, .. } if callee == target),
                    "mode={mode}: {e:?}"
                );
            }
            vm.run_with_ctx(
                &mut ctx,
                &p,
                entry,
                &[Value::Agg(good)],
                &mut outputs,
                TraceMask::NONE,
                None,
                None,
            )
            .unwrap();
            assert_eq!(outputs, vec![Value::I64(42)]);
        }
    }

    #[test]
    fn trace_attempts_and_scopes_complete_on_success_and_traps() {
        #[derive(Default)]
        struct Sink {
            pending: Option<TraceLocation>,
            scopes: Vec<(ScopeKind, usize)>,
            ends: Vec<(TraceLocation, Option<TraceLocation>, bool)>,
            scope_ends: usize,
            trapped_scopes: usize,
        }
        impl TraceSink for Sink {
            fn instr(&mut self, _: &Program, func: FuncId, pc: u32, _: u32, _: Option<u64>, _: u8) {
                assert!(self.pending.replace(TraceLocation { func, pc }).is_none());
            }
            fn instr_end(
                &mut self,
                _: &Program,
                location: TraceLocation,
                next: Option<TraceLocation>,
                outcome: TraceOutcome<'_>,
            ) {
                assert_eq!(self.pending.take(), Some(location));
                self.ends
                    .push((location, next, matches!(outcome, TraceOutcome::Trap(_))));
            }
            fn scope_enter(
                &mut self,
                _: &Program,
                kind: ScopeKind,
                depth: usize,
                _: FuncId,
                _: u32,
                _: Option<u64>,
            ) {
                self.scopes.push((kind, depth));
            }
            fn scope_exit(
                &mut self,
                _: &Program,
                kind: ScopeKind,
                depth: usize,
                _: FuncId,
                _: u32,
                _: Option<u64>,
                outcome: TraceOutcome<'_>,
            ) {
                assert_eq!(self.scopes.pop(), Some((kind, depth)));
                self.scope_ends += 1;
                self.trapped_scopes += usize::from(matches!(outcome, TraceOutcome::Trap(_)));
            }
        }
        struct HostMode(u8);
        impl Host for HostMode {
            fn call(
                &mut self,
                _: &str,
                _: SigHash,
                _: &[ValueRef<'_>],
                rets: &mut [Value],
                _: HostContext<'_, '_>,
            ) -> Result<u64, HostError> {
                match self.0 {
                    0 => Err(HostError::Failed),
                    1 => Ok(0),
                    _ => {
                        rets[0] = Value::I64(42);
                        Ok(0)
                    }
                }
            }
        }
        for mode in 0..7 {
            let mut pb = ProgramBuilder::new();
            let hs = pb.host_sig_for(
                "trace.test",
                HostSig {
                    args: vec![],
                    rets: vec![ValueType::I64],
                },
            );
            let mut callee = Asm::new();
            callee.set_register_types(vec![ValueType::Unit, ValueType::I64]);
            if mode == 2 {
                callee.trap(99);
            }
            callee.host_call_sig_id(0, hs, 0, &[], &[1]);
            callee.ret(0, &[1]);
            let target = pb
                .push_function_checked(
                    callee,
                    FunctionSig {
                        arg_types: vec![],
                        ret_types: vec![ValueType::I64],
                    },
                )
                .unwrap();
            let mut caller = Asm::new();
            caller.set_register_types(vec![ValueType::Unit, ValueType::I64]);
            caller.call(0, target, 0, &[], &[1]);
            caller.ret(0, &[1]);
            let entry = pb
                .push_function_checked(
                    caller,
                    FunctionSig {
                        arg_types: vec![],
                        ret_types: vec![ValueType::I64],
                    },
                )
                .unwrap();
            let p = pb.build_verified().unwrap();
            for mask in [
                TraceMask::INSTR,
                TraceMask::CALL,
                TraceMask::HOST,
                TraceMask::INSTR | TraceMask::CALL | TraceMask::HOST,
            ] {
                let mut limits = Limits::default();
                if mode == 3 {
                    limits.fuel = 0;
                }
                if mode == 4 {
                    limits.fuel = 1;
                }
                if mode == 5 {
                    limits.max_call_depth = 1;
                }
                let mut vm = Vm::new(HostMode(mode), limits);
                let mut sink = Sink::default();
                let result = vm.run(&p, entry, &[], mask, Some(&mut sink));
                assert_eq!(result.is_ok(), mode == 6, "mode={mode}: {result:?}");
                assert!(sink.pending.is_none());
                assert!(sink.scopes.is_empty());
                if mask.contains(TraceMask::INSTR) {
                    if mode == 3 {
                        assert!(sink.ends.is_empty());
                    } else if mode == 4 {
                        assert_eq!(sink.ends.len(), 1);
                        assert!(!sink.ends[0].2);
                    } else {
                        assert_eq!(sink.ends.last().unwrap().2, mode != 6);
                    }
                    if mode == 6 {
                        assert_eq!(
                            sink.ends[0].1,
                            Some(TraceLocation {
                                func: target,
                                pc: 0
                            })
                        );
                        assert_eq!(sink.ends[2].1.unwrap().func, entry);
                        assert_eq!(sink.ends.last().unwrap().1, None);
                    }
                }
                if mask.contains(TraceMask::CALL) {
                    assert!(sink.scope_ends > 0);
                    assert_eq!(sink.trapped_scopes > 0, mode != 6);
                }
                if mask == TraceMask::HOST && (mode == 0 || mode == 1) {
                    assert_eq!(sink.trapped_scopes, 1);
                }
            }
        }
    }

    #[test]
    fn typed_host_failures_outlive_host_state_and_reach_instruction_outcomes() {
        use alloc::rc::Rc;
        use core::cell::Cell;
        use core::error::Error;

        // Deliberately neither Clone nor Eq: the host's domain model need not provide either.
        #[derive(Debug)]
        struct OperationFailure {
            operand: String,
            code: u32,
        }
        impl fmt::Display for OperationFailure {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "operation {} rejected {}", self.code, self.operand)
            }
        }
        impl Error for OperationFailure {}
        fn find<'a>(mut error: &'a (dyn Error + 'static)) -> &'a OperationFailure {
            loop {
                if let Some(failure) = error.downcast_ref::<OperationFailure>() {
                    return failure;
                }
                error = error.source().expect("operation failure in source chain");
            }
        }
        struct FailingHost {
            dropped: Rc<Cell<bool>>,
            operand: String,
        }
        impl Drop for FailingHost {
            fn drop(&mut self) {
                self.dropped.set(true);
            }
        }
        impl Host for FailingHost {
            fn call(
                &mut self,
                _: &str,
                _: SigHash,
                _: &[ValueRef<'_>],
                _: &mut [Value],
                _: HostContext<'_, '_>,
            ) -> Result<u64, HostError> {
                Err(HostError::custom(OperationFailure {
                    operand: self.operand.clone(),
                    code: 17,
                }))
            }
        }
        #[derive(Default)]
        struct Observe(bool);
        impl TraceSink for Observe {
            fn instr_end(
                &mut self,
                _: &Program,
                _: TraceLocation,
                _: Option<TraceLocation>,
                outcome: TraceOutcome<'_>,
            ) {
                if let TraceOutcome::Trap(error) = outcome {
                    assert_eq!(find(error).code, 17);
                    self.0 = true;
                }
            }
        }
        let mut pb = ProgramBuilder::new();
        let import = pb.host_sig_for(
            "fallible",
            HostSig {
                args: vec![],
                rets: vec![],
            },
        );
        let mut body = Asm::new();
        body.set_register_types(vec![ValueType::Unit, ValueType::Agg]);
        body.tuple_new(1, &[]);
        body.host_call_sig_id(0, import, 0, &[], &[]);
        body.ret(0, &[]);
        let entry = pb
            .push_function_checked(
                body,
                FunctionSig {
                    arg_types: vec![],
                    ret_types: vec![],
                },
            )
            .unwrap();
        let p = pb.build_verified().unwrap();
        let dropped = Rc::new(Cell::new(false));
        let mut observer = Observe::default();
        let error = {
            let mut vm = Vm::new(
                FailingHost {
                    dropped: Rc::clone(&dropped),
                    operand: "owned input".into(),
                },
                Limits::default(),
            );
            let error = vm
                .run(&p, entry, &[], TraceMask::INSTR, Some(&mut observer))
                .unwrap_err();
            assert_eq!(vm.aggregates().len_u32(), 0);
            error
        };
        assert!(dropped.get());
        assert!(observer.0);
        let failure = find(&error);
        assert_eq!(failure.operand, "owned input");
        assert_eq!(failure.code, 17);
        let cloned = error.clone();
        assert!(core::ptr::eq(failure, find(&cloned)));
        assert_eq!(error.func, entry);
        assert!(format!("{error}").contains("operation 17 rejected owned input"));
    }

    #[test]
    fn host_context_identifies_imports_and_host_state_is_accessible_between_runs() {
        #[derive(Default)]
        struct StateHost {
            offset: i64,
            calls: Vec<crate::program::HostSigId>,
        }
        impl Host for StateHost {
            fn call(
                &mut self,
                _: &str,
                _: SigHash,
                _: &[ValueRef<'_>],
                rets: &mut [Value],
                ctx: HostContext<'_, '_>,
            ) -> Result<u64, HostError> {
                let id = ctx.host_sig_id();
                assert!(ctx.program().host_sig(id).is_some());
                self.calls.push(id);
                rets[0] = Value::I64(self.offset + i64::from(id.0));
                Ok(0)
            }
        }
        let mut pb = ProgramBuilder::new();
        let sig = HostSig {
            args: vec![],
            rets: vec![ValueType::I64],
        };
        pb.host_sig_for("unused", sig.clone());
        let first = pb.host_sig_for("first", sig.clone());
        let second = pb.host_sig_for("second", sig);
        let mut body = Asm::new();
        body.set_register_types(vec![ValueType::Unit, ValueType::I64, ValueType::I64]);
        body.host_call_sig_id(0, first, 0, &[], &[1]);
        body.host_call_sig_id(0, second, 0, &[], &[2]);
        body.ret(0, &[1, 2]);
        let entry = pb
            .push_function_checked(
                body,
                FunctionSig {
                    arg_types: vec![],
                    ret_types: vec![ValueType::I64; 2],
                },
            )
            .unwrap();
        let program = pb.build_verified().unwrap();
        let mut vm = Vm::new(StateHost::default(), Limits::default());
        assert_eq!(
            vm.run(&program, entry, &[], TraceMask::NONE, None).unwrap(),
            [Value::I64(1), Value::I64(2)]
        );
        assert_eq!(vm.host().calls, [first, second]);
        vm.host_mut().offset = 10;
        assert_eq!(
            vm.run(&program, entry, &[], TraceMask::NONE, None).unwrap(),
            [Value::I64(11), Value::I64(12)]
        );
        assert_eq!(vm.host().calls, [first, second, first, second]);
    }

    #[test]
    fn temporary_aggregates_do_not_accumulate_across_returns_or_traps() {
        for mode in ["scalar", "retained", "trap"] {
            let mut pb = ProgramBuilder::new();
            let mut body = Asm::new();
            body.set_register_types(vec![
                ValueType::Unit,
                ValueType::Agg,
                ValueType::I64,
                ValueType::Agg,
            ]);
            body.const_i64(2, 42);
            for _ in 0..32 {
                body.tuple_new(3, &[2]);
            }
            if mode == "trap" {
                body.trap(7);
            } else {
                body.ret(0, &[if mode == "scalar" { 2 } else { 1 }]);
            }
            let entry = pb
                .push_function_checked(
                    body,
                    FunctionSig {
                        arg_types: vec![ValueType::Agg],
                        ret_types: vec![if mode == "scalar" {
                            ValueType::I64
                        } else {
                            ValueType::Agg
                        }],
                    },
                )
                .unwrap();
            let program = pb.build_verified().unwrap();
            let mut vm = Vm::new(TestHost, Limits::default());
            let root = vm.aggregates_mut().tuple_new(vec![Value::I64(1)]).unwrap();
            let mut ctx = ExecutionContext::new();
            let mut outputs = vec![Value::Unit];
            for _ in 0..100 {
                outputs.truncate(1);
                let result = vm.run_with_ctx(
                    &mut ctx,
                    &program,
                    entry,
                    &[Value::Agg(root)],
                    &mut outputs,
                    TraceMask::NONE,
                    None,
                    None,
                );
                assert_eq!(result.is_err(), mode == "trap");
                assert_eq!(vm.aggregates().len_u32(), 1);
                assert_eq!(vm.aggregates().tuple_get(root, 0), Ok(Value::I64(1)));
                assert_eq!(outputs[0], Value::Unit);
                match mode {
                    "scalar" => assert_eq!(outputs[1..], [Value::I64(42)]),
                    "retained" => assert_eq!(outputs[1..], [Value::Agg(root)]),
                    _ => assert_eq!(outputs.len(), 1),
                }
            }
        }
    }

    #[test]
    fn returned_graphs_are_compacted_with_sharing_and_closure_environments() {
        let mut pb = ProgramBuilder::new();
        let mut target = Asm::new();
        target.set_register_types(vec![ValueType::Unit, ValueType::Agg]);
        target.ret(0, &[]);
        let target = pb
            .push_function_checked(
                target,
                FunctionSig {
                    arg_types: vec![ValueType::Agg],
                    ret_types: vec![],
                },
            )
            .unwrap();
        let text = pb.const_str("retained text");
        let mut body = Asm::new();
        body.set_register_types(vec![
            ValueType::Unit,
            ValueType::Str,
            ValueType::Agg,
            ValueType::Agg,
            ValueType::Func,
            ValueType::Closure,
            ValueType::Agg,
        ]);
        body.const_pool(1, text);
        body.tuple_new(2, &[1]); // Unreachable allocation before the environment.
        body.tuple_new(3, &[1]);
        body.const_func(4, target);
        body.closure_new(5, 4, 3);
        body.tuple_new(6, &[3, 5, 3]);
        body.tuple_new(2, &[6]); // Unreachable allocation after the returned graph.
        body.ret(0, &[6, 5]);
        let entry = pb
            .push_function_checked(
                body,
                FunctionSig {
                    arg_types: vec![],
                    ret_types: vec![ValueType::Agg, ValueType::Closure],
                },
            )
            .unwrap();
        let mut passthrough = Asm::new();
        passthrough.set_register_types(vec![ValueType::Unit, ValueType::Agg, ValueType::Agg]);
        passthrough.tuple_get(2, 1, 0);
        passthrough.ret(0, &[2]);
        let opaque = pb.contract(Contract::Kind(ValueType::Agg)).unwrap();
        let closure = pb.contract(Contract::Kind(ValueType::Closure)).unwrap();
        let read_contract = pb
            .contract(Contract::Tuple(&[
                Some(opaque),
                Some(closure),
                Some(opaque),
            ]))
            .unwrap();
        let read = pb
            .push_function_checked(
                passthrough,
                crate::asm::FunctionAbi {
                    args: vec![read_contract.into()],
                    rets: vec![ValueType::Agg.into()],
                },
            )
            .unwrap();
        let program = pb.build_verified().unwrap();
        let mut vm = Vm::new(TestHost, Limits::default());
        let mut prior = Vec::new();
        for run in 0..3 {
            let outputs = vm.run(&program, entry, &[], TraceMask::NONE, None).unwrap();
            assert_eq!(vm.aggregates().len_u32(), (run + 1) * 2);
            let [Value::Agg(root), Value::Closure(closure)] = outputs[..] else {
                panic!("unexpected returned graph");
            };
            assert_eq!(
                vm.aggregates().tuple_get(root, 0),
                Ok(Value::Agg(closure.env))
            );
            assert_eq!(
                vm.aggregates().tuple_get(root, 1),
                Ok(Value::Closure(closure))
            );
            assert_eq!(
                vm.aggregates().tuple_get(root, 2),
                Ok(Value::Agg(closure.env))
            );
            assert_eq!(closure.func, target);
            assert_eq!(
                vm.aggregates().tuple_get(closure.env, 0),
                Ok(Value::Str("retained text".into()))
            );
            prior.push(root);
            for &old_root in &prior {
                assert_eq!(
                    vm.run(
                        &program,
                        read,
                        &[Value::Agg(old_root)],
                        TraceMask::NONE,
                        None
                    )
                    .unwrap(),
                    [vm.aggregates().tuple_get(old_root, 0).unwrap()]
                );
            }
        }
    }

    #[test]
    fn dangling_return_traps_before_success_events_and_preserves_outputs() {
        #[derive(Default)]
        struct Ends {
            instructions: Vec<bool>,
            scopes: Vec<bool>,
        }
        impl TraceSink for Ends {
            fn instr_end(
                &mut self,
                _: &Program,
                _: TraceLocation,
                _: Option<TraceLocation>,
                outcome: TraceOutcome<'_>,
            ) {
                self.instructions.push(matches!(outcome, TraceOutcome::Ok));
            }

            fn scope_exit(
                &mut self,
                _: &Program,
                _: ScopeKind,
                _: usize,
                _: FuncId,
                _: u32,
                _: Option<u64>,
                outcome: TraceOutcome<'_>,
            ) {
                self.scopes.push(matches!(outcome, TraceOutcome::Ok));
            }
        }
        for ty in [ValueType::Agg, ValueType::Closure] {
            let mut pb = ProgramBuilder::new();
            let mut body = Asm::new();
            body.set_register_types(vec![ValueType::Unit, ty, ValueType::Agg]);
            body.tuple_new(2, &[]);
            body.ret(0, &[1]);
            let entry = pb
                .push_function_checked(
                    body,
                    FunctionSig {
                        arg_types: vec![ty],
                        ret_types: vec![ty],
                    },
                )
                .unwrap();
            let program = pb.build_verified().unwrap();
            let invalid = AggHandle(99);
            let arg = if ty == ValueType::Agg {
                Value::Agg(invalid)
            } else {
                Value::Closure(Closure {
                    func: entry,
                    env: invalid,
                })
            };
            let mut vm = Vm::new(TestHost, Limits::default());
            let mut outputs = vec![Value::I64(12)];
            let mut ctx = ExecutionContext::new();
            let mut ends = Ends::default();
            let error = vm
                .run_with_ctx(
                    &mut ctx,
                    &program,
                    entry,
                    &[arg],
                    &mut outputs,
                    TraceMask::INSTR | TraceMask::CALL,
                    Some(&mut ends),
                    None,
                )
                .unwrap_err();
            assert!(matches!(
                error.trap,
                Trap::AggError(AggError::UnresolvedStagedHandle)
            ));
            assert_eq!(outputs, [Value::I64(12)]);
            assert_eq!(vm.aggregates().len_u32(), 0);
            assert_eq!(ends.instructions, [true, false]);
            assert_eq!(ends.scopes, [false]);
            let env = vm.aggregates_mut().tuple_new(vec![]).unwrap();
            let valid = if ty == ValueType::Agg {
                Value::Agg(env)
            } else {
                Value::Closure(Closure { func: entry, env })
            };
            vm.run_with_ctx(
                &mut ctx,
                &program,
                entry,
                core::slice::from_ref(&valid),
                &mut outputs,
                TraceMask::NONE,
                None,
                None,
            )
            .unwrap();
            assert_eq!(outputs, [Value::I64(12), valid]);
            assert_eq!(vm.aggregates().len_u32(), 1);
        }
    }

    #[test]
    fn hosts_can_retain_borrowed_graphs_even_when_the_run_traps() {
        #[derive(Default)]
        struct RetainingHost {
            retained: Vec<crate::aggregates::ValueSnapshot>,
        }
        impl Host for RetainingHost {
            fn call(
                &mut self,
                _: &str,
                _: SigHash,
                args: &[ValueRef<'_>],
                rets: &mut [Value],
                ctx: HostContext<'_, '_>,
            ) -> Result<u64, HostError> {
                self.retained
                    .push(ctx.retain(args).map_err(|_| HostError::Failed)?);
                rets[0] = args[0].to_value();
                Ok(0)
            }
        }
        for trap in [true, false] {
            let mut pb = ProgramBuilder::new();
            let sig = pb.host_sig_for(
                "remember",
                HostSig {
                    args: vec![ValueType::Agg],
                    rets: vec![ValueType::Agg],
                },
            );
            let mut body = Asm::new();
            body.set_register_types(vec![
                ValueType::Unit,
                ValueType::I64,
                ValueType::Agg,
                ValueType::Agg,
            ]);
            body.const_i64(1, 42);
            body.tuple_new(2, &[1]);
            body.host_call_sig_id(0, sig, 0, &[2], &[3]);
            if trap {
                body.trap(7);
            } else {
                body.ret(0, &[3]);
            }
            let entry = pb
                .push_function_checked(
                    body,
                    FunctionSig {
                        arg_types: vec![],
                        ret_types: vec![ValueType::Agg],
                    },
                )
                .unwrap();
            let program = pb.build_verified().unwrap();
            let mut vm = Vm::new(RetainingHost::default(), Limits::default());
            let result = vm.run(&program, entry, &[], TraceMask::NONE, None);
            assert_eq!(result.is_err(), trap);
            assert_eq!(vm.aggregates().len_u32(), u32::from(!trap));
            if let Ok(outputs) = result {
                let [Value::Agg(root)] = outputs[..] else {
                    panic!("expected retained host return");
                };
                assert_eq!(vm.aggregates().tuple_get(root, 0), Ok(Value::I64(42)));
            }
            vm.aggregates_mut().clear();
            let values = vm.memory.aggs_mut().import(&vm.host.retained[0]).unwrap();
            let [Value::Agg(root)] = values[..] else {
                panic!("expected imported retained argument");
            };
            assert_eq!(vm.aggregates().tuple_get(root, 0), Ok(Value::I64(42)));
        }
    }

    #[test]
    fn reusable_results_append_preserve_on_traps_and_outlive_context_reset() {
        let mut pb = ProgramBuilder::new();
        let mut body = Asm::new();
        body.set_register_types(vec![
            ValueType::Unit,
            ValueType::I64,
            ValueType::Bytes,
            ValueType::Str,
            ValueType::Agg,
        ]);
        body.ret(0, &[1, 2, 3, 4]);
        let types = vec![
            ValueType::I64,
            ValueType::Bytes,
            ValueType::Str,
            ValueType::Agg,
        ];
        let entry = pb
            .push_function_checked(
                body,
                FunctionSig {
                    arg_types: types.clone(),
                    ret_types: types,
                },
            )
            .unwrap();
        let mut failing = Asm::new();
        failing.set_register_types(vec![ValueType::Unit]);
        failing.trap(123);
        let failure = pb
            .push_function_checked(
                failing,
                FunctionSig {
                    arg_types: vec![],
                    ret_types: vec![],
                },
            )
            .unwrap();
        let p = pb.build_verified().unwrap();
        let mut vm = Vm::new(TestHost, Limits::default());
        let aggregate = vm.aggregates_mut().tuple_new(vec![Value::I64(99)]).unwrap();
        let args = vec![
            Value::I64(42),
            Value::Bytes(vec![1, 2]),
            Value::Str("text".into()),
            Value::Agg(aggregate),
        ];
        let mut outputs = Vec::with_capacity(16);
        outputs.push(Value::Unit);
        let pointer = outputs.as_ptr();
        let mut ctx = ExecutionContext::new();
        vm.run_with_ctx(
            &mut ctx,
            &p,
            entry,
            &args,
            &mut outputs,
            TraceMask::NONE,
            None,
            None,
        )
        .unwrap();
        let first = outputs.clone();
        let error = vm
            .run_with_ctx(
                &mut ctx,
                &p,
                failure,
                &[],
                &mut outputs,
                TraceMask::NONE,
                None,
                None,
            )
            .unwrap_err();
        assert!(matches!(error.trap, Trap::TrapCode(123)));
        assert_eq!(outputs, first);
        vm.run_with_ctx(
            &mut ctx,
            &p,
            entry,
            &args,
            &mut outputs,
            TraceMask::NONE,
            None,
            None,
        )
        .unwrap();
        assert_eq!(outputs.as_ptr(), pointer);
        assert_eq!(&outputs[1..5], args);
        assert_eq!(&outputs[5..], args);
        assert_eq!(vm.aggregates().tuple_get(aggregate, 0), Ok(Value::I64(99)));
    }

    #[test]
    fn borrowed_payloads_survive_calls_projection_and_program_switches() {
        fn program(bytes: &[u8], text: &str) -> VerifiedProgram {
            let mut pb = ProgramBuilder::new();
            let mut callee = Asm::new();
            callee.set_register_types(vec![ValueType::Unit, ValueType::Bytes, ValueType::Str]);
            callee.ret(0, &[1, 2]);
            let types = vec![ValueType::Bytes, ValueType::Str];
            let target = pb
                .push_function_checked(
                    callee,
                    FunctionSig {
                        arg_types: types.clone(),
                        ret_types: types.clone(),
                    },
                )
                .unwrap();
            let bytes_id = pb.const_bytes(bytes);
            let text_id = pb.const_str(text);
            let mut caller = Asm::new();
            caller.set_register_types(vec![
                ValueType::Unit,
                ValueType::Bytes,
                ValueType::Str,
                ValueType::Bytes,
                ValueType::Str,
                ValueType::Agg,
                ValueType::Bytes,
                ValueType::Str,
                ValueType::Bytes,
                ValueType::Str,
            ]);
            caller.const_pool(1, bytes_id);
            caller.const_pool(2, text_id);
            caller.call(0, target, 0, &[1, 2], &[3, 4]);
            caller.tuple_new(5, &[3, 4]);
            caller.tuple_get(6, 5, 0);
            caller.tuple_get(7, 5, 1);
            caller.bytes_concat(8, 6, 1);
            caller.str_concat(9, 7, 2);
            caller.ret(0, &[6, 7, 8, 9]);
            pb.push_function_checked(
                caller,
                FunctionSig {
                    arg_types: vec![],
                    ret_types: [types.clone(), types].concat(),
                },
            )
            .unwrap();
            pb.build_verified().unwrap()
        }
        let mut ctx = ExecutionContext::new();
        let mut vm = Vm::new(TestHost, Limits::default());
        let mut outputs = Vec::new();
        for (bytes, text) in [(&b"first"[..], "\u{00e9}"), (&b"second"[..], "\u{1f980}")] {
            let p = program(bytes, text);
            vm.run_with_ctx(
                &mut ctx,
                &p,
                FuncId(1),
                &[],
                &mut outputs,
                TraceMask::NONE,
                None,
                None,
            )
            .unwrap();
            // Owned outputs are independent of program, source heap, and subsequent context reuse.
            drop(p);
            vm.aggregates_mut().clear();
        }
        assert_eq!(
            outputs,
            vec![
                Value::Bytes(b"first".to_vec()),
                Value::Str("\u{00e9}".into()),
                Value::Bytes(b"firstfirst".to_vec()),
                Value::Str("\u{00e9}\u{00e9}".into()),
                Value::Bytes(b"second".to_vec()),
                Value::Str("\u{1f980}".into()),
                Value::Bytes(b"secondsecond".to_vec()),
                Value::Str("\u{1f980}\u{1f980}".into()),
            ]
        );
    }

    #[test]
    fn host_reads_payloads_projected_from_all_aggregate_kinds() {
        struct InspectHost;
        impl Host for InspectHost {
            fn call(
                &mut self,
                _: &str,
                _: SigHash,
                args: &[ValueRef<'_>],
                rets: &mut [Value],
                _: HostContext<'_, '_>,
            ) -> Result<u64, HostError> {
                let [
                    ValueRef::Bytes(a),
                    ValueRef::Str(b),
                    ValueRef::Bytes(c),
                    ValueRef::Bytes(d),
                    ValueRef::Str(e),
                ] = args
                else {
                    panic!("unexpected host arguments")
                };
                assert_eq!(*a, b"second");
                assert_eq!(*b, "second");
                assert_eq!(*c, b"second");
                assert_eq!(*d, b"first");
                assert_eq!(*e, "second");
                rets[0] = Value::Bytes(a.to_vec());
                rets[1] = Value::Str((*e).into());
                Ok(0)
            }
        }
        let mut pb = ProgramBuilder::new();
        let b1 = pb.const_bytes(b"first");
        let b2 = pb.const_bytes(b"second");
        let s1 = pb.const_str("first");
        let s2 = pb.const_str("second");
        let st = pb.struct_type(crate::program::StructTypeDef {
            field_names: vec!["bytes".into(), "text".into()],
            field_types: vec![ValueType::Bytes, ValueType::Str],
        });
        let ab = pb.array_elem(ValueType::Bytes);
        let ats = pb.array_elem(ValueType::Str);
        let hs = pb.host_sig_for(
            "inspect",
            HostSig {
                args: vec![
                    ValueType::Bytes,
                    ValueType::Str,
                    ValueType::Bytes,
                    ValueType::Bytes,
                    ValueType::Str,
                ],
                rets: vec![ValueType::Bytes, ValueType::Str],
            },
        );
        let mut a = Asm::new();
        a.set_register_types(vec![
            ValueType::Unit,
            ValueType::Bytes,
            ValueType::Bytes,
            ValueType::Str,
            ValueType::Str,
            ValueType::Agg,
            ValueType::Bytes,
            ValueType::Str,
            ValueType::Agg,
            ValueType::U64,
            ValueType::Bytes,
            ValueType::Bytes,
            ValueType::Agg,
            ValueType::Str,
            ValueType::Agg,
            ValueType::Str,
            ValueType::Bytes,
            ValueType::Str,
        ]);
        a.const_pool(1, b1);
        a.const_pool(2, b2);
        a.const_pool(3, s1);
        a.const_pool(4, s2);
        a.struct_new(5, st, &[2, 4]);
        a.struct_get(6, 5, 0);
        a.struct_get(7, 5, 1);
        a.array_new(8, ab, &[1, 2]);
        a.const_u64(9, 1);
        a.array_get(10, 8, 9);
        a.array_get_imm(11, 8, 0);
        a.array_new(12, ats, &[3, 4]);
        a.array_get_imm(13, 12, 1);
        a.tuple_new(14, &[6, 7, 10, 11, 13]);
        a.tuple_get(15, 14, 4);
        a.host_call_sig_id(0, hs, 0, &[6, 7, 10, 11, 15], &[16, 17]);
        a.ret(0, &[16, 17]);
        pb.push_function_checked(
            a,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::Bytes, ValueType::Str],
            },
        )
        .unwrap();
        let p = pb.build_verified().unwrap();
        let mut vm = Vm::new(InspectHost, Limits::default());
        assert_eq!(
            vm.run(&p, FuncId(0), &[], TraceMask::NONE, None).unwrap(),
            vec![
                Value::Bytes(b"second".to_vec()),
                Value::Str("second".into())
            ]
        );
    }

    struct TestHost;

    impl Host for TestHost {
        fn call(
            &mut self,
            symbol: &str,
            _sig_hash: SigHash,
            args: &[ValueRef<'_>],
            rets: &mut [Value],
            _ctx: HostContext<'_, '_>,
        ) -> Result<u64, HostError> {
            match symbol {
                "id" => {
                    for (slot, arg) in rets.iter_mut().zip(args) {
                        *slot = arg.to_value();
                    }
                    Ok(0)
                }
                _ => Err(HostError::UnknownSymbol),
            }
        }
    }

    fn add_call_sig(p: &mut Program, args: &[ValueType], rets: &[ValueType]) -> u32 {
        let mut table = crate::contract::ContractTableBuilder::new();
        for (id, contract) in p.contracts.iter() {
            assert_eq!(table.intern(contract).unwrap(), id);
        }
        let args: Vec<_> = args
            .iter()
            .map(|&kind| table.intern(Contract::Kind(kind)).unwrap())
            .collect();
        let rets: Vec<_> = rets
            .iter()
            .map(|&kind| table.intern(Contract::Kind(kind)).unwrap())
            .collect();
        p.contracts = table.finish();
        let args_off = u32::try_from(p.contract_ids.len()).expect("verified address space");
        p.contract_ids.extend_from_slice(&args);
        let rets_off = u32::try_from(p.contract_ids.len()).expect("verified address space");
        p.contract_ids.extend_from_slice(&rets);
        let id = u32::try_from(p.call_sigs.len()).expect("verified address space");
        p.call_sigs.push(CallSigEntry {
            args: ByteRange {
                offset: args_off,
                len: u32::try_from(args.len()).expect("verified address space"),
            },
            rets: ByteRange {
                offset: rets_off,
                len: u32::try_from(rets.len()).expect("verified address space"),
            },
        });
        id
    }

    #[test]
    fn vm_runs_const_and_ret() {
        // const_i64 r1, 7; ret r0, r1
        let mut a = Asm::new();
        a.set_register_types(vec![ValueType::Unit, ValueType::I64]);
        a.const_i64(1, 7);
        a.ret(0, &[1]);

        let mut pb = ProgramBuilder::new();
        pb.push_function_checked(
            a,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
            },
        )
        .unwrap();
        let p = pb.build_verified().unwrap();

        let mut vm = Vm::new(TestHost, Limits::default());
        let out = vm.run(&p, FuncId(0), &[], TraceMask::NONE, None).unwrap();
        assert_eq!(out, vec![Value::I64(7)]);
    }

    #[test]
    fn vm_trace_hooks_fire() {
        struct CollectingTrace {
            starts: u32,
            ends: u32,
            instrs: Vec<u8>,
        }

        impl TraceSink for CollectingTrace {
            fn mask(&self) -> TraceMask {
                TraceMask::RUN | TraceMask::INSTR
            }

            fn run_start(&mut self, _program: &Program, _entry: FuncId, _arg_count: usize) {
                self.starts += 1;
            }

            fn instr(
                &mut self,
                _program: &Program,
                _func: FuncId,
                _pc: u32,
                _next_pc: u32,
                _span_id: Option<u64>,
                opcode: u8,
            ) {
                self.instrs.push(opcode);
            }

            fn run_end(&mut self, _program: &Program, outcome: TraceOutcome<'_>) {
                self.ends += 1;
                assert!(matches!(outcome, TraceOutcome::Ok));
            }
        }

        let mut a = Asm::new();
        a.set_register_types(vec![ValueType::Unit, ValueType::I64]);
        a.const_i64(1, 7);
        a.ret(0, &[1]);

        let mut pb = ProgramBuilder::new();
        pb.push_function_checked(
            a,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
            },
        )
        .unwrap();
        let p = pb.build_verified().unwrap();

        let mut vm = Vm::new(TestHost, Limits::default());
        let mut trace = CollectingTrace {
            starts: 0,
            ends: 0,
            instrs: Vec::new(),
        };
        let out = vm
            .run(
                &p,
                FuncId(0),
                &[],
                TraceMask::RUN | TraceMask::INSTR,
                Some(&mut trace),
            )
            .unwrap();
        assert_eq!(out, vec![Value::I64(7)]);
        assert_eq!(trace.starts, 1);
        assert_eq!(trace.ends, 1);
        assert!(!trace.instrs.is_empty());
    }

    #[test]
    fn vm_calls_host() {
        // const_i64 r1, 9; host_call r0, sym0, hash, r0, argc=1 r1, retc=1 r2; ret r0, 1, r2
        let sig = HostSig {
            args: vec![ValueType::I64],
            rets: vec![ValueType::I64],
        };

        let mut pb = ProgramBuilder::new();
        let host_sig = pb.host_sig_for("id", sig.clone());

        let mut a = Asm::new();
        a.set_register_types(vec![ValueType::Unit, ValueType::I64, ValueType::I64]);
        a.const_i64(1, 9);
        a.host_call(0, host_sig, 0, &[1], &[2]);
        a.ret(0, &[2]);
        pb.push_function_checked(
            a,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
            },
        )
        .unwrap();
        let p = pb.build_verified().unwrap();

        let mut vm = Vm::new(TestHost, Limits::default());
        let out = vm.run(&p, FuncId(0), &[], TraceMask::NONE, None).unwrap();
        assert_eq!(out, vec![Value::I64(9)]);
    }

    #[test]
    fn vm_calls_host_zero_args() {
        // 0-arg host call exercises the empty inline-slice path.
        let sig = HostSig {
            args: vec![],
            rets: vec![],
        };

        let mut pb = ProgramBuilder::new();
        let host_sig = pb.host_sig_for("id", sig);

        let mut a = Asm::new();
        a.set_register_types(vec![ValueType::Unit, ValueType::I64]);
        // host_call with 0 args, 0 rets — should succeed without touching the arg buffer.
        a.host_call(0, host_sig, 0, &[], &[]);
        a.const_i64(1, 42);
        a.ret(0, &[1]);
        pb.push_function_checked(
            a,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
            },
        )
        .unwrap();
        let p = pb.build_verified().unwrap();

        let mut vm = Vm::new(TestHost, Limits::default());
        let out = vm.run(&p, FuncId(0), &[], TraceMask::NONE, None).unwrap();
        assert_eq!(out, vec![Value::I64(42)]);
    }

    #[test]
    fn host_call_trap_includes_symbol_and_signature() {
        let sig = HostSig {
            args: vec![],
            rets: vec![],
        };
        let expected_hash = crate::host::sig_hash(&sig);

        let mut pb = ProgramBuilder::new();
        let host_sig = pb.host_sig_for("missing.literal", sig);

        let mut a = Asm::new();
        a.set_register_types(vec![ValueType::Unit, ValueType::I64]);
        a.host_call(0, host_sig, 0, &[], &[]);
        a.const_i64(1, 42);
        a.ret(0, &[1]);
        pb.push_function_checked(
            a,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
            },
        )
        .unwrap();
        let p = pb.build_verified().unwrap();

        let mut vm = Vm::new(TestHost, Limits::default());
        let err = vm
            .run(&p, FuncId(0), &[], TraceMask::NONE, None)
            .unwrap_err();
        assert!(matches!(
            &err.trap,
            Trap::HostCallFailed { symbol, sig_hash, error: HostError::UnknownSymbol }
                if symbol.as_ref() == "missing.literal" && *sig_hash == expected_hash
        ));
        let rendered = format!("{err}");
        assert!(rendered.contains("host_call=missing.literal"));
        assert!(rendered.contains(&format!("sig_hash={}", expected_hash.0)));
        assert!(rendered.contains("unknown host symbol"));
    }

    #[test]
    fn vm_calls_host_nine_args() {
        // 9 args forces the Vec heap-fallback path (inline buffer is 8).
        let n: u32 = 9;
        let sig = HostSig {
            args: vec![ValueType::I64; n as usize],
            rets: vec![ValueType::I64; n as usize],
        };

        let mut pb = ProgramBuilder::new();
        let host_sig = pb.host_sig_for("id", sig);

        // Register layout:
        //   r0         = effect token
        //   r1 ..= r9  = arg values (distinct i64s 10..=18)
        //   r10..= r18 = ret values written by the host
        let mut a = Asm::new();
        a.set_register_types(vec![
            ValueType::Unit,
            ValueType::I64,
            ValueType::I64,
            ValueType::I64,
            ValueType::I64,
            ValueType::I64,
            ValueType::I64,
            ValueType::I64,
            ValueType::I64,
            ValueType::I64,
            ValueType::I64,
            ValueType::I64,
            ValueType::I64,
            ValueType::I64,
            ValueType::I64,
            ValueType::I64,
            ValueType::I64,
            ValueType::I64,
            ValueType::I64,
        ]);
        let arg_regs: Vec<u32> = (1..=n).collect();
        let ret_regs: Vec<u32> = (n + 1..=2 * n).collect();

        for (i, &r) in arg_regs.iter().enumerate() {
            a.const_i64(r, 10 + i as i64);
        }
        a.host_call(0, host_sig, 0, &arg_regs, &ret_regs);
        a.ret(0, &ret_regs);

        pb.push_function_checked(
            a,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64; n as usize],
            },
        )
        .unwrap();
        let p = pb.build_verified().unwrap();

        let mut vm = Vm::new(TestHost, Limits::default());
        let out = vm.run(&p, FuncId(0), &[], TraceMask::NONE, None).unwrap();
        // "id" host echoes args back — verify ordering is preserved.
        let expected: Vec<Value> = (10..10 + i64::from(n)).map(Value::I64).collect();
        assert_eq!(out, expected);
    }

    #[test]
    fn vm_traps_when_host_leaves_ret_slot_unwritten() {
        // A host that intentionally does NOT write into `rets`, leaving the
        // pre-filled `Value::Unit` in a slot declared as `I64`.
        struct LazyHost;

        impl Host for LazyHost {
            fn call(
                &mut self,
                _symbol: &str,
                _sig_hash: SigHash,
                _args: &[ValueRef<'_>],
                _rets: &mut [Value],
                _ctx: HostContext<'_, '_>,
            ) -> Result<u64, HostError> {
                // Deliberately leave rets[0] as Value::Unit.
                Ok(0)
            }
        }

        let sig = HostSig {
            args: vec![],
            rets: vec![ValueType::I64],
        };

        let mut pb = ProgramBuilder::new();
        let host_sig = pb.host_sig_for("lazy", sig);

        let mut a = Asm::new();
        a.set_register_types(vec![ValueType::Unit, ValueType::I64]);
        a.host_call(0, host_sig, 0, &[], &[1]);
        a.ret(0, &[1]);
        pb.push_function_checked(
            a,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
            },
        )
        .unwrap();
        let p = pb.build_verified().unwrap();

        let mut vm = Vm::new(LazyHost, Limits::default());
        let err = vm
            .run(&p, FuncId(0), &[], TraceMask::NONE, None)
            .unwrap_err();
        assert!(matches!(
            err.trap,
            Trap::TypeMismatch {
                expected: ValueType::I64,
                actual: ValueType::Unit,
            }
        ));
    }

    #[derive(Debug, Default)]
    struct CountingAccessSink {
        reads: usize,
        writes: usize,
    }

    impl AccessSink for CountingAccessSink {
        fn read(&mut self, _key: ResourceKeyRef<'_>) {
            self.reads += 1;
        }

        fn write(&mut self, _key: ResourceKeyRef<'_>) {
            self.writes += 1;
        }
    }

    struct RecordingHost;

    impl Host for RecordingHost {
        fn call(
            &mut self,
            symbol: &str,
            sig_hash: SigHash,
            args: &[ValueRef<'_>],
            rets: &mut [Value],
            mut ctx: HostContext<'_, '_>,
        ) -> Result<u64, HostError> {
            ctx.record_read(ResourceKeyRef::OpaqueHost { op: sig_hash });
            ctx.record_read(ResourceKeyRef::HostState {
                op: sig_hash,
                key: 123,
            });
            match symbol {
                "id" => {
                    for (slot, arg) in rets.iter_mut().zip(args) {
                        *slot = arg.to_value();
                    }
                    Ok(0)
                }
                _ => Err(HostError::UnknownSymbol),
            }
        }
    }

    #[test]
    fn vm_host_call_can_record_accesses() {
        let sig = HostSig {
            args: vec![ValueType::I64],
            rets: vec![ValueType::I64],
        };

        let mut pb = ProgramBuilder::new();
        let host_sig = pb.host_sig_for("id", sig);

        let mut a = Asm::new();
        a.set_register_types(vec![ValueType::Unit, ValueType::I64, ValueType::I64]);
        a.const_i64(1, 9);
        a.host_call(0, host_sig, 0, &[1], &[2]);
        a.ret(0, &[2]);
        pb.push_function_checked(
            a,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
            },
        )
        .unwrap();
        let p = pb.build_verified().unwrap();

        let mut vm = Vm::new(RecordingHost, Limits::default());
        let mut ctx = ExecutionContext::new();
        let mut access = CountingAccessSink::default();

        let mut out = Vec::new();
        vm.run_with_ctx(
            &mut ctx,
            &p,
            FuncId(0),
            &[],
            &mut out,
            TraceMask::NONE,
            None,
            Some(&mut access),
        )
        .unwrap();

        assert_eq!(out, vec![Value::I64(9)]);
        assert!(access.reads > 0);
    }

    #[test]
    fn vm_host_call_can_read_aggregate_argument() {
        struct SumTupleHost;

        impl Host for SumTupleHost {
            fn call(
                &mut self,
                symbol: &str,
                _sig_hash: SigHash,
                args: &[ValueRef<'_>],
                rets: &mut [Value],
                ctx: HostContext<'_, '_>,
            ) -> Result<u64, HostError> {
                if symbol != "tuple.sum2" {
                    return Err(HostError::UnknownSymbol);
                }
                let [ValueRef::Agg(tuple)] = args else {
                    return Err(HostError::Failed);
                };
                if ctx.agg_type(*tuple).map_err(|_| HostError::Failed)?
                    != (AggType::Tuple { arity: 2 })
                {
                    return Err(HostError::Failed);
                }
                let ValueRef::I64(lhs) = ctx
                    .tuple_get_ref(*tuple, 0)
                    .map_err(|_| HostError::Failed)?
                else {
                    return Err(HostError::Failed);
                };
                let ValueRef::I64(rhs) = ctx
                    .tuple_get_ref(*tuple, 1)
                    .map_err(|_| HostError::Failed)?
                else {
                    return Err(HostError::Failed);
                };
                rets[0] = Value::I64(lhs + rhs);
                Ok(0)
            }
        }

        let sig = HostSig {
            args: vec![ValueType::Agg],
            rets: vec![ValueType::I64],
        };
        let mut pb = ProgramBuilder::new();
        let host_sig = pb.host_sig_for("tuple.sum2", sig);
        let mut a = Asm::new();
        a.set_register_types(vec![
            ValueType::Unit,
            ValueType::I64,
            ValueType::I64,
            ValueType::Agg,
            ValueType::I64,
        ]);
        a.const_i64(1, 20);
        a.const_i64(2, 22);
        a.tuple_new(3, &[1, 2]);
        a.host_call(0, host_sig, 0, &[3], &[4]);
        a.ret(0, &[4]);
        let entry = pb
            .push_function_checked(
                a,
                FunctionSig {
                    arg_types: vec![],
                    ret_types: vec![ValueType::I64],
                },
            )
            .unwrap();
        let p = pb.build_verified().unwrap();

        let mut vm = Vm::new(SumTupleHost, Limits::default());
        let out = vm
            .run(&p, entry, &[], TraceMask::NONE, None)
            .expect("host should read tuple contents");
        assert_eq!(out, vec![Value::I64(42)]);
    }

    #[test]
    fn vm_returns_closure_value() {
        let mut a = Asm::new();
        a.set_register_types(vec![ValueType::Unit, ValueType::Closure]);
        a.ret(0, &[1]);

        let mut pb = ProgramBuilder::new();
        pb.push_function_checked(
            a,
            FunctionSig {
                arg_types: vec![ValueType::Closure],
                ret_types: vec![ValueType::Closure],
            },
        )
        .unwrap();
        let p = pb.build_verified().unwrap();

        let closure = Closure {
            func: FuncId(123),
            env: AggHandle(0),
        };

        let mut vm = Vm::new(TestHost, Limits::default());
        vm.aggregates_mut().tuple_new(vec![]).unwrap();
        let out = vm
            .run(
                &p,
                FuncId(0),
                &[Value::Closure(closure)],
                TraceMask::NONE,
                None,
            )
            .unwrap();
        assert_eq!(out, vec![Value::Closure(closure)]);
    }

    #[test]
    fn vm_stores_and_loads_closure_via_tuple() {
        let mut a = Asm::new();
        a.set_register_types(vec![
            ValueType::Unit,
            ValueType::Closure,
            ValueType::Agg,
            ValueType::Closure,
        ]);
        a.tuple_new(2, &[1]);
        a.tuple_get(3, 2, 0);
        a.ret(0, &[3]);

        let mut pb = ProgramBuilder::new();
        pb.push_function_checked(
            a,
            FunctionSig {
                arg_types: vec![ValueType::Closure],
                ret_types: vec![ValueType::Closure],
            },
        )
        .unwrap();
        let p = pb.build_verified().unwrap();

        let closure = Closure {
            func: FuncId(42),
            env: AggHandle(0),
        };

        let mut vm = Vm::new(TestHost, Limits::default());
        vm.aggregates_mut().tuple_new(vec![]).unwrap();
        let out = vm
            .run(
                &p,
                FuncId(0),
                &[Value::Closure(closure)],
                TraceMask::NONE,
                None,
            )
            .unwrap();
        assert_eq!(out, vec![Value::Closure(closure)]);
    }

    #[test]
    fn vm_executes_closure_new() {
        let bytecode = crate::bytecode::encode_instructions(&[
            Instr::ConstFunc {
                dst: 2,
                func_id: FuncId(0),
            },
            Instr::ClosureNew {
                dst: 3,
                func: 2,
                env: 1,
            },
            Instr::Ret {
                eff_in: 0,
                rets: vec![3],
            },
        ])
        .unwrap();

        let p = Program::new(
            vec![],
            vec![],
            vec![],
            TypeTableDef::default(),
            vec![FunctionDef {
                arg_types: vec![ValueType::Agg],
                ret_types: vec![ValueType::Closure],
                register_types: vec![
                    ValueType::Unit,
                    ValueType::Agg,
                    ValueType::Func,
                    ValueType::Closure,
                ],
                bytecode,
                spans: vec![],
            }],
        );
        let p = verify_program_owned(p, &VerifyConfig::default()).unwrap();

        let mut vm = Vm::new(TestHost, Limits::default());
        let env = vm
            .aggregates_mut()
            .tuple_new(vec![Value::I64(11), Value::Bool(true)])
            .unwrap();

        let out = vm
            .run(&p, FuncId(0), &[Value::Agg(env)], TraceMask::NONE, None)
            .unwrap();
        let closure = match out.as_slice() {
            [Value::Closure(c)] => *c,
            other => panic!("expected single closure return, got {other:?}"),
        };

        assert_eq!(closure.func, FuncId(0));
        assert_eq!(closure.env, env);
        assert_eq!(vm.aggregates().tuple_len(closure.env), Ok(2));
        assert_eq!(
            vm.aggregates().tuple_get(closure.env, 0),
            Ok(Value::I64(11))
        );
        assert_eq!(
            vm.aggregates().tuple_get(closure.env, 1),
            Ok(Value::Bool(true))
        );
    }

    #[test]
    fn vm_executes_call_indirect_via_func() {
        let entry_bytecode = crate::bytecode::encode_instructions(&[
            Instr::ConstFunc {
                dst: 1,
                func_id: FuncId(1),
            },
            Instr::ConstI64 { dst: 2, imm: 9 },
            Instr::CallIndirect {
                eff_out: 0,
                call_sig: 0,
                callee: 1,
                eff_in: 0,
                args: vec![2],
                rets: vec![3],
            },
            Instr::Ret {
                eff_in: 0,
                rets: vec![3],
            },
        ])
        .unwrap();
        let callee_bytecode = crate::bytecode::encode_instructions(&[Instr::Ret {
            eff_in: 0,
            rets: vec![1],
        }])
        .unwrap();

        let mut p = Program::new(
            vec![],
            vec![],
            vec![],
            TypeTableDef::default(),
            vec![
                FunctionDef {
                    arg_types: vec![],
                    ret_types: vec![ValueType::I64],
                    register_types: vec![
                        ValueType::Unit,
                        ValueType::Func,
                        ValueType::I64,
                        ValueType::I64,
                    ],
                    bytecode: entry_bytecode,
                    spans: vec![],
                },
                FunctionDef {
                    arg_types: vec![ValueType::I64],
                    ret_types: vec![ValueType::I64],
                    register_types: vec![ValueType::Unit, ValueType::I64],
                    bytecode: callee_bytecode,
                    spans: vec![],
                },
            ],
        );
        assert_eq!(
            add_call_sig(&mut p, &[ValueType::I64], &[ValueType::I64]),
            0
        );
        let p = verify_program_owned(p, &VerifyConfig::default()).unwrap();

        let mut vm = Vm::new(TestHost, Limits::default());
        let out = vm.run(&p, FuncId(0), &[], TraceMask::NONE, None).unwrap();
        assert_eq!(out, vec![Value::I64(9)]);
    }

    #[test]
    fn vm_executes_call_indirect_via_closure_with_env_injection() {
        let entry_bytecode = crate::bytecode::encode_instructions(&[
            Instr::ConstI64 { dst: 1, imm: 11 },
            Instr::ConstI64 { dst: 2, imm: 22 },
            Instr::TupleNew {
                dst: 3,
                values: vec![1, 2],
            },
            Instr::ConstFunc {
                dst: 4,
                func_id: FuncId(1),
            },
            Instr::ClosureNew {
                dst: 5,
                func: 4,
                env: 3,
            },
            Instr::CallIndirect {
                eff_out: 0,
                call_sig: 0,
                callee: 5,
                eff_in: 0,
                args: vec![],
                rets: vec![6],
            },
            Instr::TupleLen { dst: 7, tuple: 6 },
            Instr::Ret {
                eff_in: 0,
                rets: vec![7],
            },
        ])
        .unwrap();
        let callee_bytecode = crate::bytecode::encode_instructions(&[Instr::Ret {
            eff_in: 0,
            rets: vec![1],
        }])
        .unwrap();

        let mut p = Program::new(
            vec![],
            vec![],
            vec![],
            TypeTableDef::default(),
            vec![
                FunctionDef {
                    arg_types: vec![],
                    ret_types: vec![ValueType::U64],
                    register_types: vec![
                        ValueType::Unit,
                        ValueType::I64,
                        ValueType::I64,
                        ValueType::Agg,
                        ValueType::Func,
                        ValueType::Closure,
                        ValueType::Agg,
                        ValueType::U64,
                    ],
                    bytecode: entry_bytecode,
                    spans: vec![],
                },
                FunctionDef {
                    arg_types: vec![ValueType::Agg],
                    ret_types: vec![ValueType::Agg],
                    register_types: vec![ValueType::Unit, ValueType::Agg],
                    bytecode: callee_bytecode,
                    spans: vec![],
                },
            ],
        );
        assert_eq!(add_call_sig(&mut p, &[], &[ValueType::Agg]), 0);
        let p = verify_program_owned(p, &VerifyConfig::default()).unwrap();

        let mut vm = Vm::new(TestHost, Limits::default());
        let out = vm.run(&p, FuncId(0), &[], TraceMask::NONE, None).unwrap();
        assert_eq!(out, vec![Value::U64(2)]);
    }

    #[test]
    fn vm_executes_closure_that_reads_typed_tuple_env() {
        let mut pb = ProgramBuilder::new();
        let call_sig = pb
            .call_sig(FunctionSig {
                arg_types: vec![ValueType::I64],
                ret_types: vec![ValueType::I64],
            })
            .unwrap();

        let mut entry = Asm::new();
        entry.set_register_types(vec![
            ValueType::Unit,
            ValueType::I64,
            ValueType::Agg,
            ValueType::Func,
            ValueType::Closure,
            ValueType::I64,
            ValueType::I64,
        ]);
        entry.const_i64(1, 40);
        entry.tuple_new(2, &[1]);
        entry.const_func(3, FuncId(1));
        entry.closure_new(4, 3, 2);
        entry.const_i64(5, 2);
        entry.call_indirect(0, call_sig, 4, 0, &[5], &[6]);
        entry.ret(0, &[6]);
        pb.push_function_checked(
            entry,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
            },
        )
        .unwrap();

        let mut body = Asm::new();
        body.set_register_types(vec![
            ValueType::Unit,
            ValueType::Agg,
            ValueType::I64,
            ValueType::I64,
            ValueType::I64,
        ]);
        body.tuple_get(3, 1, 0);
        body.i64_add(4, 3, 2);
        body.ret(0, &[4]);
        let integer = pb.contract(Contract::Kind(ValueType::I64)).unwrap();
        let env_contract = pb.contract(Contract::Tuple(&[Some(integer)])).unwrap();
        pb.push_function_checked(
            body,
            crate::asm::FunctionAbi {
                args: vec![env_contract.into(), ValueType::I64.into()],
                rets: vec![ValueType::I64.into()],
            },
        )
        .unwrap();

        let p = pb.build_verified().unwrap();
        let mut vm = Vm::new(TestHost, Limits::default());
        let out = vm.run(&p, FuncId(0), &[], TraceMask::NONE, None).unwrap();
        assert_eq!(out, vec![Value::I64(42)]);
    }

    #[test]
    fn vm_traps_call_indirect_on_signature_arity_mismatch() {
        let entry_bytecode = crate::bytecode::encode_instructions(&[
            Instr::ConstI64 { dst: 2, imm: 7 },
            Instr::CallIndirect {
                eff_out: 0,
                call_sig: 0,
                callee: 1,
                eff_in: 0,
                args: vec![2],
                rets: vec![3],
            },
            Instr::Ret {
                eff_in: 0,
                rets: vec![3],
            },
        ])
        .unwrap();
        let callee_bytecode = crate::bytecode::encode_instructions(&[
            Instr::ConstI64 { dst: 1, imm: 99 },
            Instr::Ret {
                eff_in: 0,
                rets: vec![1],
            },
        ])
        .unwrap();

        let mut p = Program::new(
            vec![],
            vec![],
            vec![],
            TypeTableDef::default(),
            vec![
                FunctionDef {
                    arg_types: vec![ValueType::Func],
                    ret_types: vec![ValueType::I64],
                    register_types: vec![
                        ValueType::Unit,
                        ValueType::Func,
                        ValueType::I64,
                        ValueType::I64,
                    ],
                    bytecode: entry_bytecode,
                    spans: vec![],
                },
                FunctionDef {
                    arg_types: vec![],
                    ret_types: vec![ValueType::I64],
                    register_types: vec![ValueType::Unit, ValueType::I64],
                    bytecode: callee_bytecode,
                    spans: vec![],
                },
            ],
        );
        assert_eq!(
            add_call_sig(&mut p, &[ValueType::I64], &[ValueType::I64]),
            0
        );
        let p = verify_program_owned(p, &VerifyConfig::default()).unwrap();

        let mut vm = Vm::new(TestHost, Limits::default());
        let err = vm
            .run(
                &p,
                FuncId(0),
                &[Value::Func(FuncId(1))],
                TraceMask::NONE,
                None,
            )
            .unwrap_err();
        assert!(matches!(err.trap, Trap::ArityMismatch));
    }

    #[test]
    fn vm_traps_call_indirect_on_signature_type_mismatch() {
        let entry_bytecode = crate::bytecode::encode_instructions(&[
            Instr::ConstI64 { dst: 2, imm: 7 },
            Instr::CallIndirect {
                eff_out: 0,
                call_sig: 0,
                callee: 1,
                eff_in: 0,
                args: vec![2],
                rets: vec![3],
            },
            Instr::Ret {
                eff_in: 0,
                rets: vec![3],
            },
        ])
        .unwrap();
        let callee_bytecode = crate::bytecode::encode_instructions(&[
            Instr::ConstI64 { dst: 2, imm: 99 },
            Instr::Ret {
                eff_in: 0,
                rets: vec![2],
            },
        ])
        .unwrap();

        let mut p = Program::new(
            vec![],
            vec![],
            vec![],
            TypeTableDef::default(),
            vec![
                FunctionDef {
                    arg_types: vec![ValueType::Func],
                    ret_types: vec![ValueType::I64],
                    register_types: vec![
                        ValueType::Unit,
                        ValueType::Func,
                        ValueType::I64,
                        ValueType::I64,
                    ],
                    bytecode: entry_bytecode,
                    spans: vec![],
                },
                FunctionDef {
                    arg_types: vec![ValueType::U64],
                    ret_types: vec![ValueType::I64],
                    register_types: vec![ValueType::Unit, ValueType::U64, ValueType::I64],
                    bytecode: callee_bytecode,
                    spans: vec![],
                },
            ],
        );
        assert_eq!(
            add_call_sig(&mut p, &[ValueType::I64], &[ValueType::I64]),
            0
        );
        let p = verify_program_owned(p, &VerifyConfig::default()).unwrap();

        let mut vm = Vm::new(TestHost, Limits::default());
        let err = vm
            .run(
                &p,
                FuncId(0),
                &[Value::Func(FuncId(1))],
                TraceMask::NONE,
                None,
            )
            .unwrap_err();
        assert!(matches!(
            err.trap,
            Trap::TypeMismatch {
                expected: ValueType::I64,
                actual: ValueType::U64,
            }
        ));
    }

    #[test]
    fn vm_traps_call_indirect_when_closure_has_invalid_func_id() {
        let bytecode = crate::bytecode::encode_instructions(&[
            Instr::CallIndirect {
                eff_out: 0,
                call_sig: 0,
                callee: 1,
                eff_in: 0,
                args: vec![],
                rets: vec![2],
            },
            Instr::Ret {
                eff_in: 0,
                rets: vec![2],
            },
        ])
        .unwrap();

        let mut p = Program::new(
            vec![],
            vec![],
            vec![],
            TypeTableDef::default(),
            vec![FunctionDef {
                arg_types: vec![ValueType::Closure],
                ret_types: vec![ValueType::I64],
                register_types: vec![ValueType::Unit, ValueType::Closure, ValueType::I64],
                bytecode,
                spans: vec![],
            }],
        );
        assert_eq!(add_call_sig(&mut p, &[], &[ValueType::I64]), 0);
        let p = verify_program_owned(p, &VerifyConfig::default()).unwrap();

        let mut vm = Vm::new(TestHost, Limits::default());
        let err = vm
            .run(
                &p,
                FuncId(0),
                &[Value::Closure(Closure {
                    func: FuncId(99),
                    env: AggHandle(0),
                })],
                TraceMask::NONE,
                None,
            )
            .unwrap_err();
        assert!(matches!(err.trap, Trap::InvalidPc));
    }

    #[test]
    fn vm_branches() {
        let mut a = Asm::new();
        a.set_register_types(vec![ValueType::Unit, ValueType::Bool, ValueType::I64]);
        let l_then = a.label();
        let l_else = a.label();
        a.const_bool(1, true);
        a.br(1, l_then, l_else);
        a.place(l_then).unwrap();
        a.const_i64(2, 1);
        a.ret(0, &[2]);
        a.place(l_else).unwrap();
        a.const_i64(2, 2);
        a.ret(0, &[2]);
        let mut pb = ProgramBuilder::new();
        pb.push_function_checked(
            a,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
            },
        )
        .unwrap();
        let p = pb.build_verified().unwrap();

        let mut vm = Vm::new(TestHost, Limits::default());
        let out = vm.run(&p, FuncId(0), &[], TraceMask::NONE, None).unwrap();
        assert_eq!(out, vec![Value::I64(1)]);
    }

    #[test]
    fn vm_i64_add() {
        let mut a = Asm::new();
        a.set_register_types(vec![
            ValueType::Unit,
            ValueType::I64,
            ValueType::I64,
            ValueType::I64,
        ]);
        a.const_i64(1, 7);
        a.const_i64(2, 9);
        a.i64_add(3, 1, 2);
        a.ret(0, &[3]);

        let mut pb = ProgramBuilder::new();
        pb.push_function_checked(
            a,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
            },
        )
        .unwrap();
        let p = pb.build_verified().unwrap();

        let mut vm = Vm::new(TestHost, Limits::default());
        let out = vm.run(&p, FuncId(0), &[], TraceMask::NONE, None).unwrap();
        assert_eq!(out, vec![Value::I64(16)]);
    }

    #[test]
    fn vm_mov_closure() {
        // mov r2, r1; ret [r2] — r1 is the closure arg, r2 is the copy
        let mut a = Asm::new();
        a.set_register_types(vec![
            ValueType::Unit,
            ValueType::Closure,
            ValueType::Closure,
        ]);
        a.mov(2, 1);
        a.ret(0, &[2]);

        let mut pb = ProgramBuilder::new();
        pb.push_function_checked(
            a,
            FunctionSig {
                arg_types: vec![ValueType::Closure],
                ret_types: vec![ValueType::Closure],
            },
        )
        .unwrap();
        let p = pb.build_verified().unwrap();

        let closure = Closure {
            func: FuncId(42),
            env: AggHandle(0),
        };

        let mut vm = Vm::new(TestHost, Limits::default());
        vm.aggregates_mut().tuple_new(vec![]).unwrap();
        let out = vm
            .run(
                &p,
                FuncId(0),
                &[Value::Closure(closure)],
                TraceMask::NONE,
                None,
            )
            .unwrap();
        assert_eq!(out, vec![Value::Closure(closure)]);
    }

    #[test]
    fn vm_select_closure() {
        // select r4, r1, r2, r3; ret [r4]
        // r1=cond (Bool), r2/r3=closures
        let mut a = Asm::new();
        a.set_register_types(vec![
            ValueType::Unit,
            ValueType::Bool,
            ValueType::Closure,
            ValueType::Closure,
            ValueType::Closure,
        ]);
        a.select(4, 1, 2, 3);
        a.ret(0, &[4]);

        let mut pb = ProgramBuilder::new();
        pb.push_function_checked(
            a,
            FunctionSig {
                arg_types: vec![ValueType::Bool, ValueType::Closure, ValueType::Closure],
                ret_types: vec![ValueType::Closure],
            },
        )
        .unwrap();
        let p = pb.build_verified().unwrap();

        let closure_a = Closure {
            func: FuncId(10),
            env: AggHandle(0),
        };
        let closure_b = Closure {
            func: FuncId(20),
            env: AggHandle(1),
        };

        // cond=true → selects r2 (closure_a)
        let mut vm = Vm::new(TestHost, Limits::default());
        vm.aggregates_mut().tuple_new(vec![]).unwrap();
        vm.aggregates_mut().tuple_new(vec![]).unwrap();
        let out = vm
            .run(
                &p,
                FuncId(0),
                &[
                    Value::Bool(true),
                    Value::Closure(closure_a),
                    Value::Closure(closure_b),
                ],
                TraceMask::NONE,
                None,
            )
            .unwrap();
        assert_eq!(out, vec![Value::Closure(closure_a)]);

        // cond=false → selects r3 (closure_b)
        let mut vm = Vm::new(TestHost, Limits::default());
        vm.aggregates_mut().tuple_new(vec![]).unwrap();
        vm.aggregates_mut().tuple_new(vec![]).unwrap();
        let out = vm
            .run(
                &p,
                FuncId(0),
                &[
                    Value::Bool(false),
                    Value::Closure(closure_a),
                    Value::Closure(closure_b),
                ],
                TraceMask::NONE,
                None,
            )
            .unwrap();
        assert_eq!(out, vec![Value::Closure(closure_b)]);
    }

    #[test]
    fn vm_recursive_indirect_call_traps_at_depth_limit() {
        // func0 (entry): creates closure wrapping func1, calls it via call.indirect
        // func1 (recursive): sig (Agg, I64) -> I64
        //   if n == 0 → return 0
        //   else → create closure over same env, call.indirect with n-1

        // -- func1 (recursive) --
        // r0 = eff, r1 = env (Agg), r2 = n (I64)
        // r3 = 0, r4 = (n == 0), branch on r4
        // base case: ret [r3]
        // recursive case: const_func r5 = FuncId(1), closure_new r6 = (r5, r1),
        //   i64_sub r7 = r2 - r3_one, call_indirect r8 = r6(r7), ret [r8]
        let mut a1 = Asm::new();
        a1.set_register_types(vec![
            ValueType::Unit,
            ValueType::Agg,
            ValueType::I64,
            ValueType::I64,
            ValueType::Bool,
            ValueType::Func,
            ValueType::Closure,
            ValueType::I64,
            ValueType::I64,
            ValueType::I64,
        ]);
        let l_base = a1.label();
        let l_recurse = a1.label();
        a1.const_i64(3, 0);
        a1.i64_eq(4, 2, 3);
        a1.br(4, l_base, l_recurse);
        // base case
        a1.place(l_base).unwrap();
        a1.ret(0, &[3]);
        // recursive case
        a1.place(l_recurse).unwrap();
        a1.const_func(5, FuncId(1));
        a1.closure_new(6, 5, 1); // closure over same env
        a1.const_i64(7, 1);
        a1.i64_sub(8, 2, 7); // n - 1

        let mut pb = ProgramBuilder::new();
        let sig = pb
            .call_sig(FunctionSig {
                arg_types: vec![ValueType::I64],
                ret_types: vec![ValueType::I64],
            })
            .unwrap();
        a1.call_indirect(0, sig, 6, 0, &[8], &[9]);
        a1.ret(0, &[9]);

        // -- func0 (entry) --
        // create a tuple env, create closure over func1, call it with n=100
        let mut a0 = Asm::new();
        a0.set_register_types(vec![
            ValueType::Unit,
            ValueType::I64,
            ValueType::Agg,
            ValueType::Func,
            ValueType::Closure,
            ValueType::I64,
            ValueType::I64,
        ]);
        a0.const_i64(1, 0);
        a0.tuple_new(2, &[1]); // env = (0,)
        a0.const_func(3, FuncId(1));
        a0.closure_new(4, 3, 2);
        a0.const_i64(5, 100); // n=100, well above depth limit
        a0.call_indirect(0, sig, 4, 0, &[5], &[6]);
        a0.ret(0, &[6]);

        pb.push_function_checked(
            a0,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
            },
        )
        .unwrap();
        pb.push_function_checked(
            a1,
            FunctionSig {
                arg_types: vec![ValueType::Agg, ValueType::I64],
                ret_types: vec![ValueType::I64],
            },
        )
        .unwrap();
        let p = pb.build_verified().unwrap();

        let mut vm = Vm::new(
            TestHost,
            Limits {
                max_call_depth: 10,
                ..Limits::default()
            },
        );
        let err = vm
            .run(&p, FuncId(0), &[], TraceMask::NONE, None)
            .unwrap_err();
        assert!(matches!(err.trap, Trap::CallDepthExceeded));
    }

    #[test]
    fn vm_multiple_closures_same_func_different_envs() {
        // callee (func1): sig (Agg) -> Agg — returns its env tuple directly
        // entry (func0): creates two envs, two closures, calls each, returns both Agg results

        // -- func1 (callee) --
        // r0 = eff, r1 = env (Agg)
        // ret [r1]
        let mut a1 = Asm::new();
        a1.set_register_types(vec![ValueType::Unit, ValueType::Agg]);
        a1.ret(0, &[1]);

        let mut pb = ProgramBuilder::new();
        let sig = pb
            .call_sig(FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::Agg],
            })
            .unwrap();

        // -- func0 (entry) --
        let mut a0 = Asm::new();
        a0.set_register_types(vec![
            ValueType::Unit,
            ValueType::I64,
            ValueType::Agg,
            ValueType::I64,
            ValueType::Agg,
            ValueType::Func,
            ValueType::Closure,
            ValueType::Closure,
            ValueType::Agg,
            ValueType::Agg,
        ]);
        a0.const_i64(1, 11);
        a0.tuple_new(2, &[1]); // env_a = (11,)
        a0.const_i64(3, 22);
        a0.tuple_new(4, &[3]); // env_b = (22,)
        a0.const_func(5, FuncId(1));
        a0.closure_new(6, 5, 2); // closure_a
        a0.closure_new(7, 5, 4); // closure_b
        a0.call_indirect(0, sig, 6, 0, &[], &[8]); // call closure_a → Agg
        a0.call_indirect(0, sig, 7, 0, &[], &[9]); // call closure_b → Agg
        a0.ret(0, &[8, 9]);

        pb.push_function_checked(
            a0,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::Agg, ValueType::Agg],
            },
        )
        .unwrap();
        pb.push_function_checked(
            a1,
            FunctionSig {
                arg_types: vec![ValueType::Agg],
                ret_types: vec![ValueType::Agg],
            },
        )
        .unwrap();
        let p = pb.build_verified().unwrap();

        let mut vm = Vm::new(TestHost, Limits::default());
        let out = vm.run(&p, FuncId(0), &[], TraceMask::NONE, None).unwrap();

        // Each closure should have returned its own env
        let (env_a, env_b) = match out.as_slice() {
            [Value::Agg(a), Value::Agg(b)] => (*a, *b),
            other => panic!("expected two Agg returns, got {other:?}"),
        };
        assert_eq!(vm.aggregates().tuple_get(env_a, 0), Ok(Value::I64(11)));
        assert_eq!(vm.aggregates().tuple_get(env_b, 0), Ok(Value::I64(22)));
    }

    #[test]
    fn vm_traps_call_indirect_closure_ret_type_mismatch() {
        // callee (func1): sig (Agg) -> U64 (returns U64)
        // call_sig: () -> I64 (expects I64 return)
        // Should trap with TypeMismatch { expected: I64, actual: U64 }

        // -- func1 (callee) --
        // r0 = eff, r1 = env (Agg)
        // const_u64 r2, 42; ret [r2]
        let callee_bytecode = crate::bytecode::encode_instructions(&[
            Instr::ConstU64 { dst: 2, imm: 42 },
            Instr::Ret {
                eff_in: 0,
                rets: vec![2],
            },
        ])
        .unwrap();

        // -- func0 (entry) --
        // const_i64 r1, 0; tuple_new r2, [r1]  → env
        // const_func r3, FuncId(1)
        // closure_new r4, r3, r2
        // call_indirect r5 = r4() with mismatched call_sig
        // ret [r5]
        let entry_bytecode = crate::bytecode::encode_instructions(&[
            Instr::CallIndirect {
                eff_out: 0,
                call_sig: 0,
                callee: 1,
                eff_in: 0,
                args: vec![],
                rets: vec![2],
            },
            Instr::Ret {
                eff_in: 0,
                rets: vec![2],
            },
        ])
        .unwrap();
        let mut p = Program::new(
            vec![],
            vec![],
            vec![],
            TypeTableDef::default(),
            vec![
                FunctionDef {
                    arg_types: vec![ValueType::Closure],
                    ret_types: vec![ValueType::I64],
                    register_types: vec![ValueType::Unit, ValueType::Closure, ValueType::I64],
                    bytecode: entry_bytecode,
                    spans: vec![],
                },
                FunctionDef {
                    arg_types: vec![ValueType::Agg],
                    ret_types: vec![ValueType::U64],
                    register_types: vec![ValueType::Unit, ValueType::Agg, ValueType::U64],
                    bytecode: callee_bytecode,
                    spans: vec![],
                },
            ],
        );
        // call_sig: () -> I64 — mismatches callee's return type of U64
        assert_eq!(add_call_sig(&mut p, &[], &[ValueType::I64]), 0);
        let p = verify_program_owned(p, &VerifyConfig::default()).unwrap();

        let mut vm = Vm::new(TestHost, Limits::default());
        let env = vm.aggregates_mut().tuple_new(vec![]).unwrap();
        let err = vm
            .run(
                &p,
                FuncId(0),
                &[Value::Closure(Closure {
                    func: FuncId(1),
                    env,
                })],
                TraceMask::NONE,
                None,
            )
            .unwrap_err();
        assert!(matches!(
            err.trap,
            Trap::TypeMismatch {
                expected: ValueType::I64,
                actual: ValueType::U64,
            }
        ));
    }

    // Keep the rest of the legacy tests in conformance; PR6 focuses on the execution model.
    // (Full coverage remains in `execution_tape_conformance`.)
}
