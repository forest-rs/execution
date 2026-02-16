// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Interpreter for `execution_tape` bytecode (draft).
//!
//! The VM executes programs with explicit limits (fuel, call depth, host calls).
//!
//! The VM executes [`VerifiedProgram`]s only.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use crate::aggregates::{AggError, AggHeap};
use crate::arena::{BytesHandle, StrHandle, ValueArena};
use crate::host::{AccessSink, Host, HostError, ValueRef};
use crate::program::ValueType;
use crate::program::{ConstEntry, Function, Program};
use crate::trace::{ScopeKind, TraceMask, TraceOutcome, TraceSink};
use crate::typed::{
    AggReg, BoolReg, BytesReg, DecimalReg, ExecFunc, ExecInstr, F64Reg, FuncReg, I64Reg, ObjReg,
    StrReg, U64Reg, UnitReg, VReg, VRegSlice,
};
use crate::value::{AggHandle, Decimal, FuncId, Obj, ObjHandle, Value};
use crate::verifier::VerifiedProgram;

/// Execution limits for a VM run.
#[derive(Clone, Debug)]
pub struct Limits {
    /// Instruction budget. Each instruction costs 1 by default; host calls may charge extra.
    pub fuel: u64,
    /// Maximum call depth (frames).
    pub max_call_depth: usize,
    /// Maximum host calls.
    pub max_host_calls: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            fuel: 1_000_000,
            max_call_depth: 256,
            max_host_calls: 1_000_000,
        }
    }
}

/// A runtime trap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trap {
    /// Fuel limit exceeded.
    FuelExceeded,
    /// Call depth limit exceeded.
    CallDepthExceeded,
    /// Host call count limit exceeded.
    HostCallLimitExceeded,
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
    /// Aggregate heap error.
    AggError(AggError),
    /// A type id immediate was out of bounds.
    TypeIdOutOfBounds,
    /// An element type id immediate was out of bounds.
    ElemTypeIdOutOfBounds,
    /// Immediate/provided arity mismatch.
    ArityMismatch,
    /// Host call failed.
    HostCallFailed(HostError),
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
            Self::InvalidPc => write!(f, "invalid pc"),
            Self::RegOutOfBounds => write!(f, "register out of bounds"),
            Self::ConstOutOfBounds => write!(f, "constant out of bounds"),
            Self::TypeMismatch { expected, actual } => {
                write!(f, "type mismatch (expected {expected:?}, got {actual:?})")
            }
            Self::AggError(e) => write!(f, "aggregate error: {e}"),
            Self::TypeIdOutOfBounds => write!(f, "type id out of bounds"),
            Self::ElemTypeIdOutOfBounds => write!(f, "elem type id out of bounds"),
            Self::ArityMismatch => write!(f, "arity mismatch"),
            Self::HostCallFailed(e) => write!(f, "host call failed: {e}"),
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

impl core::error::Error for Trap {}

/// A trap annotated with location information.
#[derive(Clone, Debug, PartialEq, Eq)]
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

    /// Arena backing bytes/strings and other out-of-line temporaries.
    arena: ValueArena,

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

    fn reset(&mut self, fuel: u64) {
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
        self.arena.clear();

        self.fuel = fuel;
        self.host_calls = 0;
    }
}

/// A simple register VM.
pub struct Vm<H: Host> {
    host: H,
    limits: Limits,

    /// Aggregate heap storage. This is VM-owned so embedders can inspect aggregates after a run.
    agg: AggHeap,
}

impl<H: Host> fmt::Debug for Vm<H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Vm")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

/// Stack-only helper that centralizes trace mask checks and sink dispatch.
///
/// Keeping this out of [`ExecutionContext`] avoids storing borrowed trace state in the long-lived
/// per-run context while still flattening VM hot-loop tracing code.
struct TraceCtx<'a> {
    mask: TraceMask,
    sink: Option<&'a mut dyn TraceSink>,
}

impl<'a> TraceCtx<'a> {
    #[inline]
    fn new(mask: TraceMask, sink: Option<&'a mut dyn TraceSink>) -> Self {
        Self { mask, sink }
    }

    #[inline(always)]
    fn enabled(&self, mask: TraceMask) -> bool {
        self.mask.contains(mask) && self.sink.is_some()
    }

    #[inline]
    fn run_start(&mut self, program: &Program, entry: FuncId, arg_count: usize) {
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
    fn instr(
        &mut self,
        program: &Program,
        func: FuncId,
        pc: u32,
        next_pc: u32,
        span_id: Option<u64>,
        opcode: u8,
    ) {
        if let Some(t) = self.sink.as_mut() {
            let t: &mut dyn TraceSink = &mut **t;
            t.instr(program, func, pc, next_pc, span_id, opcode);
        }
    }

    #[inline]
    fn scope_enter(
        &mut self,
        program: &Program,
        kind: ScopeKind,
        depth: usize,
        func: FuncId,
        pc: u32,
        span_id: Option<u64>,
    ) {
        if let Some(t) = self.sink.as_mut() {
            let t: &mut dyn TraceSink = &mut **t;
            t.scope_enter(program, kind, depth, func, pc, span_id);
        }
    }

    #[inline]
    fn scope_exit(
        &mut self,
        program: &Program,
        kind: ScopeKind,
        depth: usize,
        func: FuncId,
        pc: u32,
        span_id: Option<u64>,
    ) {
        if let Some(t) = self.sink.as_mut() {
            let t: &mut dyn TraceSink = &mut **t;
            t.scope_exit(program, kind, depth, func, pc, span_id);
        }
    }
}

impl<H: Host> Vm<H> {
    /// Creates a new VM with `host` and `limits`.
    #[must_use]
    pub fn new(host: H, limits: Limits) -> Self {
        Self {
            host,
            limits,
            agg: AggHeap::new(),
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
    pub fn run(
        &mut self,
        program: &VerifiedProgram,
        entry: FuncId,
        args: &[Value],
        trace_mask: TraceMask,
        trace: Option<&mut dyn TraceSink>,
    ) -> Result<Vec<Value>, TrapInfo> {
        let mut ctx = ExecutionContext::new();
        self.run_with_ctx(&mut ctx, program, entry, args, trace_mask, trace, None)
    }

    /// Executes `program` starting at `entry` using an explicit per-run [`ExecutionContext`].
    ///
    /// Keeping the context separate allows embedders to reuse allocations across runs.
    pub fn run_with_ctx(
        &mut self,
        ctx: &mut ExecutionContext,
        program: &VerifiedProgram,
        entry: FuncId,
        args: &[Value],
        trace_mask: TraceMask,
        trace: Option<&mut dyn TraceSink>,
        access: Option<&mut dyn AccessSink>,
    ) -> Result<Vec<Value>, TrapInfo> {
        let program_ref = program.program();
        let mut trace = TraceCtx::new(trace_mask, trace);
        trace.run_start(program_ref, entry, args.len());

        let result = self.run_body(ctx, program, entry, args, &mut trace, access);

        let outcome = match &result {
            Ok(_) => TraceOutcome::Ok,
            Err(e) => TraceOutcome::Trap(e),
        };
        trace.run_end(program_ref, outcome);

        result
    }

    fn run_body(
        &mut self,
        ctx: &mut ExecutionContext,
        program: &VerifiedProgram,
        entry: FuncId,
        args: &[Value],
        trace: &mut TraceCtx<'_>,
        mut access: Option<&mut dyn AccessSink>,
    ) -> Result<Vec<Value>, TrapInfo> {
        let program_ref = program.program();
        let max_call_depth = self.limits.max_call_depth;
        let max_host_calls = self.limits.max_host_calls;
        let trace_instr = trace.enabled(TraceMask::INSTR);
        let trace_call = trace.enabled(TraceMask::CALL);
        let trace_host = trace.enabled(TraceMask::HOST);
        ctx.reset(self.limits.fuel);

        let entry_fn = program_ref
            .functions
            .get(entry.0 as usize)
            .ok_or_else(|| ctx.trap(entry, 0, None, Trap::InvalidPc))?;
        if args.len() != entry_fn.arg_count as usize {
            return Err(ctx.trap(entry, 0, None, Trap::InvalidPc));
        }
        validate_entry_args(program_ref, entry_fn, args)
            .map_err(|t| ctx.trap(entry, 0, None, t))?;

        let entry_vf = program
            .verified(entry)
            .ok_or_else(|| ctx.trap(entry, 0, None, Trap::InvalidPc))?;
        let entry_base = ctx.alloc_frame(entry_vf);
        ctx.init_args(entry_base, entry_vf, args)
            .map_err(|t| ctx.trap(entry, 0, None, t))?;
        ctx.frames.push(Frame {
            func: entry,
            pc: 0,
            instr_ix: 0,
            byte_len: entry_vf.byte_len,
            base: entry_base,
            return_to: None,
        });

        if trace_call {
            trace.scope_enter(
                program_ref,
                ScopeKind::CallFrame { func: entry },
                ctx.frames.len(),
                entry,
                0,
                entry_vf.span_at_ix(0).map(|id| id.get()),
            );
        }

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
            let (opcode, instr, actual_pc, next_pc) =
                vf.fetch_at_ix(instr_ix).ok_or_else(|| {
                    let trap_span = span_id.or_else(|| ctx.span_at(program_ref, func_id, pc));
                    ctx.trap(func_id, pc, trap_span, Trap::InvalidPc)
                })?;
            debug_assert_eq!(
                pc, actual_pc,
                "frame pc must match verified instruction offset"
            );
            let next_instr_ix = instr_ix.saturating_add(1);

            if trace_instr {
                trace.instr(program_ref, func_id, pc, next_pc, span_id, opcode);
            }

            // Default fallthrough: advance to the next decoded instruction.
            ctx.frames[frame_index].pc = next_pc;
            ctx.frames[frame_index].instr_ix = next_instr_ix;

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
                ExecInstr::ConstDecimal {
                    dst,
                    mantissa,
                    scale,
                } => {
                    ctx.write_decimal(
                        base,
                        *dst,
                        Decimal {
                            mantissa: *mantissa,
                            scale: *scale,
                        },
                    );
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
                ExecInstr::ConstPoolDecimal { dst, idx } => {
                    let ConstEntry::Decimal { mantissa, scale } = program_ref
                        .const_pool
                        .get(idx.0 as usize)
                        .ok_or_else(|| ctx.trap(func_id, pc, span_id, Trap::ConstOutOfBounds))?
                    else {
                        return Err(ctx.trap(func_id, pc, span_id, Trap::InvalidPc));
                    };
                    ctx.write_decimal(
                        base,
                        *dst,
                        Decimal {
                            mantissa: *mantissa,
                            scale: *scale,
                        },
                    );
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::ConstPoolBytes { dst, idx } => {
                    let ConstEntry::Bytes(r) = program_ref
                        .const_pool
                        .get(idx.0 as usize)
                        .ok_or_else(|| ctx.trap(func_id, pc, span_id, Trap::ConstOutOfBounds))?
                    else {
                        return Err(ctx.trap(func_id, pc, span_id, Trap::InvalidPc));
                    };
                    let start = r.offset as usize;
                    let end = r
                        .end()
                        .map_err(|_| ctx.trap(func_id, pc, span_id, Trap::InvalidPc))?
                        as usize;
                    let bytes = program_ref
                        .const_bytes_data
                        .get(start..end)
                        .ok_or_else(|| ctx.trap(func_id, pc, span_id, Trap::InvalidPc))?
                        .to_vec();
                    let h = ctx.arena.alloc_bytes(bytes);
                    ctx.write_bytes_handle(base, *dst, h);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::ConstPoolStr { dst, idx } => {
                    let ConstEntry::Str(r) = program_ref
                        .const_pool
                        .get(idx.0 as usize)
                        .ok_or_else(|| ctx.trap(func_id, pc, span_id, Trap::ConstOutOfBounds))?
                    else {
                        return Err(ctx.trap(func_id, pc, span_id, Trap::InvalidPc));
                    };
                    let start = r.offset as usize;
                    let end = r
                        .end()
                        .map_err(|_| ctx.trap(func_id, pc, span_id, Trap::InvalidPc))?
                        as usize;
                    let s = program_ref
                        .const_str_data
                        .get(start..end)
                        .ok_or_else(|| ctx.trap(func_id, pc, span_id, Trap::InvalidPc))?;
                    let h = ctx.arena.alloc_str_from_str(s);
                    ctx.write_str_handle(base, *dst, h);
                    ctx.frames[frame_index].pc = next_pc;
                }

                ExecInstr::DecAdd { dst, a, b } => {
                    let da = ctx.read_decimal(base, *a);
                    let db = ctx.read_decimal(base, *b);
                    if da.scale != db.scale {
                        return Err(ctx.trap(func_id, pc, span_id, Trap::DecimalScaleMismatch));
                    }
                    let mantissa = da
                        .mantissa
                        .checked_add(db.mantissa)
                        .ok_or_else(|| ctx.trap(func_id, pc, span_id, Trap::DecimalOverflow))?;
                    ctx.write_decimal(
                        base,
                        *dst,
                        Decimal {
                            mantissa,
                            scale: da.scale,
                        },
                    );
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::DecSub { dst, a, b } => {
                    let da = ctx.read_decimal(base, *a);
                    let db = ctx.read_decimal(base, *b);
                    if da.scale != db.scale {
                        return Err(ctx.trap(func_id, pc, span_id, Trap::DecimalScaleMismatch));
                    }
                    let mantissa = da
                        .mantissa
                        .checked_sub(db.mantissa)
                        .ok_or_else(|| ctx.trap(func_id, pc, span_id, Trap::DecimalOverflow))?;
                    ctx.write_decimal(
                        base,
                        *dst,
                        Decimal {
                            mantissa,
                            scale: da.scale,
                        },
                    );
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::DecMul { dst, a, b } => {
                    let da = ctx.read_decimal(base, *a);
                    let db = ctx.read_decimal(base, *b);
                    let mantissa = da
                        .mantissa
                        .checked_mul(db.mantissa)
                        .ok_or_else(|| ctx.trap(func_id, pc, span_id, Trap::DecimalOverflow))?;
                    let scale_u16 = u16::from(da.scale) + u16::from(db.scale);
                    let scale = u8::try_from(scale_u16)
                        .map_err(|_| ctx.trap(func_id, pc, span_id, Trap::DecimalOverflow))?;
                    ctx.write_decimal(base, *dst, Decimal { mantissa, scale });
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

                ExecInstr::Br {
                    cond,
                    true_ix,
                    false_ix,
                } => {
                    let target_ix_u32 = if ctx.read_bool(base, *cond) {
                        *true_ix
                    } else {
                        *false_ix
                    };
                    let target_ix = usize::try_from(target_ix_u32)
                        .map_err(|_| ctx.trap(func_id, pc, span_id, Trap::InvalidPc))?;
                    let target_pc = vf
                        .pc_at_ix(target_ix)
                        .ok_or_else(|| ctx.trap(func_id, pc, span_id, Trap::InvalidPc))?;
                    ctx.frames[frame_index].pc = target_pc;
                    ctx.frames[frame_index].instr_ix = target_ix;
                }
                ExecInstr::Jmp { target_ix } => {
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

                    ctx.frames.push(Frame {
                        func: *callee,
                        pc: 0,
                        instr_ix: 0,
                        byte_len: callee_vf.byte_len,
                        base: callee_base,
                        return_to: Some(ReturnTo {
                            dst_rets: *dst_rets,
                        }),
                    });

                    if trace_call {
                        trace.scope_enter(
                            program_ref,
                            ScopeKind::CallFrame { func: *callee },
                            ctx.frames.len(),
                            *callee,
                            0,
                            callee_vf.span_at_ix(0).map(|id| id.get()),
                        );
                    }

                    debug_assert_eq!(
                        callee_fn.bytecode.len, callee_vf.byte_len,
                        "verified byte_len must match decoded bytecode length"
                    );
                }

                ExecInstr::Ret { eff_in: _, rets } => {
                    if trace_call {
                        trace.scope_exit(
                            program_ref,
                            ScopeKind::CallFrame { func: func_id },
                            ctx.frames.len(),
                            func_id,
                            pc,
                            span_id,
                        );
                    }

                    if ctx.frames.len() == 1 {
                        let rets = vf.vregs(*rets);
                        let mut out: Vec<Value> = Vec::with_capacity(rets.len());
                        for &r in rets {
                            out.push(
                                ctx.materialize_vreg(base, r)
                                    .map_err(|t| ctx.trap(func_id, pc, span_id, t))?,
                            );
                        }
                        return Ok(out);
                    }

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
                            base,
                            a,
                        )
                        .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                    }

                    let call_args: &[ValueRef<'_>] = call_args_mut;

                    if trace_host {
                        trace.scope_enter(
                            program_ref,
                            ScopeKind::HostCall {
                                host_sig: *host_sig,
                                symbol: hs.symbol,
                                sig_hash: hs.sig_hash,
                            },
                            ctx.frames.len(),
                            func_id,
                            pc,
                            span_id,
                        );
                    }

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

                    let access_for_call = access.as_mut().map(|a| &mut **a as &mut dyn AccessSink);
                    let extra_fuel = self
                        .host
                        .call(sym, hs.sig_hash, call_args, ret_slots, access_for_call)
                        .map_err(|e| ctx.trap(func_id, pc, span_id, Trap::HostCallFailed(e)))?;
                    ctx.fuel = ctx.fuel.saturating_sub(extra_fuel);

                    if trace_host {
                        trace.scope_exit(
                            program_ref,
                            ScopeKind::HostCall {
                                host_sig: *host_sig,
                                symbol: hs.symbol,
                                sig_hash: hs.sig_hash,
                            },
                            ctx.frames.len(),
                            func_id,
                            pc,
                            span_id,
                        );
                    }

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
                    for (dst, v) in rets.iter().copied().zip(ret_slots.iter_mut()) {
                        let val = core::mem::replace(v, Value::Unit);
                        ctx.intern_value_to_vreg(base, dst, &val)
                            .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                    }
                    ctx.frames[frame_index].pc = next_pc;
                }

                ExecInstr::TupleNew { dst, values } => {
                    let values = vf.vregs(*values);
                    let mut vals = Vec::with_capacity(values.len());
                    for &r in values {
                        vals.push(
                            ctx.materialize_vreg(base, r)
                                .map_err(|t| ctx.trap(func_id, pc, span_id, t))?,
                        );
                    }
                    let h = self.agg.tuple_new(vals);
                    ctx.write_agg_handle(base, AggReg(dst.0), h);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::TupleGet { dst, tuple, index } => {
                    let h = ctx.read_agg_handle(base, AggReg(tuple.0));
                    let out = self
                        .agg
                        .tuple_get(h, *index as usize)
                        .map_err(|e| ctx.trap(func_id, pc, span_id, Trap::AggError(e)))?;
                    ctx.intern_value_to_vreg(base, *dst, &out)
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
                            ctx.materialize_vreg(base, r)
                                .map_err(|t| ctx.trap(func_id, pc, span_id, t))?,
                        );
                    }
                    let h = self.agg.struct_new(*type_id, vals);
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
                        .agg
                        .struct_get(h, *field_index as usize)
                        .map_err(|e| ctx.trap(func_id, pc, span_id, Trap::AggError(e)))?;
                    ctx.intern_value_to_vreg(base, *dst, &out)
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
                            ctx.materialize_vreg(base, r)
                                .map_err(|t| ctx.trap(func_id, pc, span_id, t))?,
                        );
                    }
                    let h = self.agg.array_new(*elem_type_id, vals);
                    ctx.write_agg_handle(base, AggReg(dst.0), h);
                    ctx.frames[frame_index].pc = next_pc;
                }

                ExecInstr::ArrayLen { dst, arr } => {
                    let h = ctx.read_agg_handle(base, AggReg(arr.0));
                    let n = self
                        .agg
                        .array_len(h)
                        .map_err(|e| ctx.trap(func_id, pc, span_id, Trap::AggError(e)))?;
                    ctx.write_u64(base, *dst, u64::try_from(n).unwrap_or(u64::MAX));
                    ctx.frames[frame_index].pc = next_pc;
                }

                ExecInstr::ArrayGet { dst, arr, index } => {
                    let h = ctx.read_agg_handle(base, AggReg(arr.0));
                    let ix = usize::try_from(ctx.read_u64(base, *index)).unwrap_or(usize::MAX);
                    let out = self
                        .agg
                        .array_get(h, ix)
                        .map_err(|e| ctx.trap(func_id, pc, span_id, Trap::AggError(e)))?;
                    ctx.intern_value_to_vreg(base, *dst, &out)
                        .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                    ctx.frames[frame_index].pc = next_pc;
                }

                ExecInstr::ArrayGetImm { dst, arr, index } => {
                    let h = ctx.read_agg_handle(base, AggReg(arr.0));
                    let ix = usize::try_from(*index).unwrap_or(usize::MAX);
                    let out = self
                        .agg
                        .array_get(h, ix)
                        .map_err(|e| ctx.trap(func_id, pc, span_id, Trap::AggError(e)))?;
                    ctx.intern_value_to_vreg(base, *dst, &out)
                        .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                    ctx.frames[frame_index].pc = next_pc;
                }

                ExecInstr::TupleLen { dst, tuple } => {
                    let h = ctx.read_agg_handle(base, AggReg(tuple.0));
                    let n = self
                        .agg
                        .tuple_len(h)
                        .map_err(|e| ctx.trap(func_id, pc, span_id, Trap::AggError(e)))?;
                    ctx.write_u64(base, *dst, u64::try_from(n).unwrap_or(u64::MAX));
                    ctx.frames[frame_index].pc = next_pc;
                }

                ExecInstr::StructFieldCount { dst, st } => {
                    let h = ctx.read_agg_handle(base, AggReg(st.0));
                    let n = self
                        .agg
                        .struct_field_count(h)
                        .map_err(|e| ctx.trap(func_id, pc, span_id, Trap::AggError(e)))?;
                    ctx.write_u64(base, *dst, u64::try_from(n).unwrap_or(u64::MAX));
                    ctx.frames[frame_index].pc = next_pc;
                }

                ExecInstr::BytesLen { dst, bytes } => {
                    let b = ctx
                        .read_bytes(bytes, base)
                        .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                    ctx.write_u64(base, *dst, u64::try_from(b.len()).unwrap_or(u64::MAX));
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::StrLen { dst, s } => {
                    let s = ctx
                        .read_str(s, base)
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
                    if a < i64::MIN as f64 || a > i64::MAX as f64 {
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
                    if a < 0.0 || a > u64::MAX as f64 {
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
                    if a.scale != 0 {
                        return Err(ctx.trap(func_id, pc, span_id, Trap::DecimalScaleMismatch));
                    }
                    ctx.write_i64(base, *dst, a.mantissa);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::DecToU64 { dst, a } => {
                    let a = ctx.read_decimal(base, *a);
                    if a.scale != 0 {
                        return Err(ctx.trap(func_id, pc, span_id, Trap::DecimalScaleMismatch));
                    }
                    if a.mantissa < 0 {
                        return Err(ctx.trap(func_id, pc, span_id, Trap::IntCastOverflow));
                    }
                    ctx.write_u64(base, *dst, u64::try_from(a.mantissa).unwrap_or(u64::MAX));
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::I64ToDec { dst, a, scale } => {
                    let a = ctx.read_i64(base, *a);
                    let mut factor: i64 = 1;
                    for _ in 0..*scale {
                        factor = factor
                            .checked_mul(10)
                            .ok_or_else(|| ctx.trap(func_id, pc, span_id, Trap::DecimalOverflow))?;
                    }
                    let mantissa = a
                        .checked_mul(factor)
                        .ok_or_else(|| ctx.trap(func_id, pc, span_id, Trap::DecimalOverflow))?;
                    ctx.write_decimal(
                        base,
                        *dst,
                        Decimal {
                            mantissa,
                            scale: *scale,
                        },
                    );
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::U64ToDec { dst, a, scale } => {
                    let a = ctx.read_u64(base, *a);
                    let mut factor: i64 = 1;
                    for _ in 0..*scale {
                        factor = factor
                            .checked_mul(10)
                            .ok_or_else(|| ctx.trap(func_id, pc, span_id, Trap::DecimalOverflow))?;
                    }
                    let mantissa = i128::from(a)
                        .checked_mul(i128::from(factor))
                        .and_then(|x| i64::try_from(x).ok())
                        .ok_or_else(|| ctx.trap(func_id, pc, span_id, Trap::DecimalOverflow))?;
                    ctx.write_decimal(
                        base,
                        *dst,
                        Decimal {
                            mantissa,
                            scale: *scale,
                        },
                    );
                    ctx.frames[frame_index].pc = next_pc;
                }

                ExecInstr::BytesEq { dst, a, b } => {
                    let a = ctx
                        .read_bytes(a, base)
                        .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                    let b = ctx
                        .read_bytes(b, base)
                        .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                    ctx.write_bool(base, *dst, a == b);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::StrEq { dst, a, b } => {
                    let a = ctx
                        .read_str(a, base)
                        .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                    let b = ctx
                        .read_str(b, base)
                        .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                    ctx.write_bool(base, *dst, a == b);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::BytesConcat { dst, a, b } => {
                    let out = {
                        let a = ctx
                            .read_bytes(a, base)
                            .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                        let b = ctx
                            .read_bytes(b, base)
                            .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                        let mut out = Vec::with_capacity(a.len() + b.len());
                        out.extend_from_slice(a);
                        out.extend_from_slice(b);
                        out
                    };
                    let h = ctx.arena.alloc_bytes(out);
                    ctx.write_bytes_handle(base, *dst, h);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::StrConcat { dst, a, b } => {
                    let out = {
                        let a = ctx
                            .read_str(a, base)
                            .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                        let b = ctx
                            .read_str(b, base)
                            .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                        let mut out = String::with_capacity(a.len() + b.len());
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
                        .read_bytes(bytes, base)
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
                        .read_bytes(bytes, base)
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
                    let out = {
                        let bytes = ctx
                            .read_bytes(bytes, base)
                            .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                        let start =
                            usize::try_from(ctx.read_u64(base, *start)).unwrap_or(usize::MAX);
                        let end = usize::try_from(ctx.read_u64(base, *end)).unwrap_or(usize::MAX);
                        bytes
                            .get(start..end)
                            .ok_or_else(|| ctx.trap(func_id, pc, span_id, Trap::IndexOutOfBounds))?
                            .to_vec()
                    };
                    let h = ctx.arena.alloc_bytes(out);
                    ctx.write_bytes_handle(base, *dst, h);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::StrSlice { dst, s, start, end } => {
                    let out = {
                        let s = ctx
                            .read_str(s, base)
                            .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                        let start =
                            usize::try_from(ctx.read_u64(base, *start)).unwrap_or(usize::MAX);
                        let end = usize::try_from(ctx.read_u64(base, *end)).unwrap_or(usize::MAX);
                        if start > end || end > s.len() {
                            return Err(ctx.trap(func_id, pc, span_id, Trap::IndexOutOfBounds));
                        }
                        if !s.is_char_boundary(start) || !s.is_char_boundary(end) {
                            return Err(ctx.trap(func_id, pc, span_id, Trap::StrNotCharBoundary));
                        }
                        String::from(&s[start..end])
                    };
                    let h = ctx.arena.alloc_str(out);
                    ctx.write_str_handle(base, *dst, h);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::StrToBytes { dst, s } => {
                    let out = {
                        let s = ctx
                            .read_str(s, base)
                            .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                        s.as_bytes().to_vec()
                    };
                    let h = ctx.arena.alloc_bytes(out);
                    ctx.write_bytes_handle(base, *dst, h);
                    ctx.frames[frame_index].pc = next_pc;
                }
                ExecInstr::BytesToStr { dst, bytes } => {
                    let out = {
                        let bytes = ctx
                            .read_bytes(bytes, base)
                            .map_err(|t| ctx.trap(func_id, pc, span_id, t))?;
                        String::from_utf8(bytes.to_vec())
                            .map_err(|_| ctx.trap(func_id, pc, span_id, Trap::InvalidUtf8))?
                    };
                    let h = ctx.arena.alloc_str(out);
                    ctx.write_str_handle(base, *dst, h);
                    ctx.frames[frame_index].pc = next_pc;
                }
            }
        }
    }

    /// Returns a reference to the aggregate heap.
    pub fn aggregates(&self) -> &AggHeap {
        &self.agg
    }

    /// Returns a mutable reference to the aggregate heap.
    pub fn aggregates_mut(&mut self) -> &mut AggHeap {
        &mut self.agg
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
        };

        self.units.resize(base.unit + counts.unit, 0);
        self.bools.resize(base.bools + counts.bools, false);
        self.i64s.resize(base.i64s + counts.i64s, 0);
        self.u64s.resize(base.u64s + counts.u64s, 0);
        self.f64s.resize(base.f64s + counts.f64s, 0.0);
        self.decimals.resize(
            base.decimals + counts.decimals,
            Decimal {
                mantissa: 0,
                scale: 0,
            },
        );
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
    }

    fn init_args(&mut self, base: RegBase, vf: &ExecFunc, args: &[Value]) -> Result<(), Trap> {
        if args.len() != vf.reg_layout.arg_regs.len() {
            return Err(Trap::InvalidPc);
        }
        for (dst, v) in vf.reg_layout.arg_regs.iter().copied().zip(args.iter()) {
            self.intern_value_to_vreg(base, dst, v)?;
        }
        Ok(())
    }

    fn intern_value_to_vreg(&mut self, base: RegBase, dst: VReg, v: &Value) -> Result<(), Trap> {
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
            (VReg::Bytes(r), Value::Bytes(b)) => {
                let h = self.arena.alloc_bytes_from_slice(b);
                self.write_bytes_handle(base, r, h);
                Ok(())
            }
            (VReg::Str(r), Value::Str(s)) => {
                let h = self.arena.alloc_str_from_str(s.as_str());
                self.write_str_handle(base, r, h);
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
            _ => Err(Trap::InvalidPc),
        }
    }

    fn materialize_vreg(&self, base: RegBase, v: VReg) -> Result<Value, Trap> {
        Ok(match v {
            VReg::Unit(_) => Value::Unit,
            VReg::Bool(r) => Value::Bool(self.read_bool(base, r)),
            VReg::I64(r) => Value::I64(self.read_i64(base, r)),
            VReg::U64(r) => Value::U64(self.read_u64(base, r)),
            VReg::F64(r) => Value::F64(self.read_f64(base, r)),
            VReg::Decimal(r) => Value::Decimal(self.read_decimal(base, r)),
            VReg::Bytes(r) => Value::Bytes(
                self.arena
                    .bytes(self.read_bytes_handle(base, r))
                    .ok_or(Trap::InvalidPc)?
                    .to_vec(),
            ),
            VReg::Str(r) => Value::Str(String::from(
                self.arena
                    .str(self.read_str_handle(base, r))
                    .ok_or(Trap::InvalidPc)?,
            )),
            VReg::Obj(r) => Value::Obj(self.read_obj(base, r)),
            VReg::Agg(r) => Value::Agg(self.read_agg_handle(base, r)),
            VReg::Func(r) => Value::Func(self.read_func(base, r)),
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

    #[inline]
    #[must_use = "reads can trap; handle the Result"]
    fn read_bytes<'a>(&'a self, reg: &BytesReg, base: RegBase) -> Result<&'a [u8], Trap> {
        let h = self.read_bytes_handle(base, *reg);
        self.arena.bytes(h).ok_or(Trap::InvalidPc)
    }

    #[inline]
    #[must_use = "reads can trap; handle the Result"]
    fn read_str<'a>(&'a self, reg: &StrReg, base: RegBase) -> Result<&'a str, Trap> {
        let h = self.read_str_handle(base, *reg);
        self.arena.str(h).ok_or(Trap::InvalidPc)
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
        return Err(Trap::InvalidPc);
    }
    for (v, &t) in args.iter().zip(arg_types.iter()) {
        v.check_type(t)?;
    }
    Ok(())
}

#[inline]
#[must_use = "host ABI reads can trap; handle the Result"]
fn read_value_ref_at<'a>(
    arena: &'a ValueArena,
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
        VReg::Decimal(r) => ValueRef::Decimal(decimals[base.decimals + r.0 as usize]),
        VReg::Bytes(r) => {
            let h = bytes[base.bytes + r.0 as usize];
            ValueRef::Bytes(arena.bytes(h).ok_or(Trap::InvalidPc)?)
        }
        VReg::Str(r) => {
            let h = strs[base.strs + r.0 as usize];
            ValueRef::Str(arena.str(h).ok_or(Trap::InvalidPc)?)
        }
        VReg::Obj(r) => ValueRef::Obj(objs[base.objs + r.0 as usize]),
        VReg::Agg(r) => ValueRef::Agg(aggs[base.aggs + r.0 as usize]),
        VReg::Func(r) => ValueRef::Func(funcs[base.funcs + r.0 as usize]),
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
    use crate::asm::{Asm, FunctionSig, ProgramBuilder};
    use crate::host::{AccessSink, HostSig, ResourceKeyRef, SigHash};
    use crate::program::{Program, ValueType};
    use crate::trace::{TraceMask, TraceOutcome, TraceSink};
    use alloc::vec;
    use alloc::vec::Vec;

    struct TestHost;

    impl Host for TestHost {
        fn call(
            &mut self,
            symbol: &str,
            _sig_hash: SigHash,
            args: &[ValueRef<'_>],
            rets: &mut [Value],
            _access: Option<&mut dyn AccessSink>,
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

    #[test]
    fn vm_runs_const_and_ret() {
        // const_i64 r1, 7; ret r0, r1
        let mut a = Asm::new();
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
                _access: Option<&mut dyn AccessSink>,
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
        assert_eq!(
            err.trap,
            Trap::TypeMismatch {
                expected: ValueType::I64,
                actual: ValueType::Unit,
            }
        );
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
            access: Option<&mut dyn AccessSink>,
        ) -> Result<u64, HostError> {
            if let Some(a) = access {
                a.read(ResourceKeyRef::OpaqueHost { op: sig_hash });
                a.read(ResourceKeyRef::HostState {
                    op: sig_hash,
                    key: 123,
                });
            }
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

        let out = vm
            .run_with_ctx(
                &mut ctx,
                &p,
                FuncId(0),
                &[],
                TraceMask::NONE,
                None,
                Some(&mut access),
            )
            .unwrap();

        assert_eq!(out, vec![Value::I64(9)]);
        assert!(access.reads > 0);
    }

    #[test]
    fn vm_branches() {
        let mut a = Asm::new();
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

    // Keep the rest of the legacy tests in conformance; PR6 focuses on the execution model.
    // (Full coverage remains in `execution_tape_conformance`.)
}
