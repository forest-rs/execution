// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Interpreter for `execution_tape` bytecode (draft).
//!
//! The VM executes programs with explicit limits (fuel, call depth, host calls).
//!
//! Prefer [`Vm::run_verified`] (or [`Vm::run_verified_traced`]) with a [`VerifiedProgram`], which
//! enables a faster interpreter path by eliding many dynamic checks.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::aggregates::{AggError, AggHeap};
use crate::bytecode::{DecodedInstr, Instr, decode_instructions};
use crate::host::{Host, HostError};
use crate::program::ValueType;
use crate::program::{ConstEntry, Function, Program};
use crate::trace::{ScopeKind, TraceEvent, TraceMask, TraceOutcome, TraceRunMode, TraceSink};
use crate::value::{AggHandle, Decimal, FuncId, Value};
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
    /// Bytecode decoding failed.
    BytecodeDecode,
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
            Self::BytecodeDecode => write!(f, "bytecode decode failed"),
            Self::InvalidPc => write!(f, "invalid pc"),
            Self::RegOutOfBounds => write!(f, "register out of bounds"),
            Self::ConstOutOfBounds => write!(f, "constant out of bounds"),
            Self::TypeMismatch { expected, actual } => {
                write!(f, "type mismatch (expected {expected:?}, got {actual:?})")
            }
            Self::AggError(e) => write!(f, "aggregate error: {e}"),
            Self::TypeIdOutOfBounds => write!(f, "type id out of bounds"),
            Self::ElemTypeIdOutOfBounds => write!(f, "element type id out of bounds"),
            Self::ArityMismatch => write!(f, "arity mismatch"),
            Self::HostCallFailed(e) => write!(f, "host call failed: {e}"),
            Self::HostReturnArityMismatch { expected, actual } => write!(
                f,
                "host return arity mismatch (expected {expected}, got {actual})"
            ),
            Self::IntCastOverflow => write!(f, "integer cast overflow"),
            Self::DecimalScaleMismatch => write!(f, "decimal scale mismatch"),
            Self::DecimalOverflow => write!(f, "decimal overflow"),
            Self::DivByZero => write!(f, "division by zero"),
            Self::IntDivOverflow => write!(f, "integer division overflow"),
            Self::FloatToIntInvalid => write!(f, "float to int conversion invalid"),
            Self::IndexOutOfBounds => write!(f, "index out of bounds"),
            Self::StrNotCharBoundary => write!(f, "string slice not on UTF-8 character boundary"),
            Self::InvalidUtf8 => write!(f, "invalid UTF-8"),
            Self::TrapCode(code) => write!(f, "trap({code})"),
        }
    }
}

impl core::error::Error for Trap {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::AggError(e) => Some(e),
            Self::HostCallFailed(e) => Some(e),
            _ => None,
        }
    }
}

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

#[derive(Clone, Debug)]
struct Frame {
    func: FuncId,
    pc: u32,
    base: usize,
    reg_count: usize,
    byte_len: u32,
    decoded: Vec<DecodedInstr>,
    rets: Vec<u32>,
    ret_base: usize,
    ret_pc: u32,
}

/// A simple register VM.
pub struct Vm<H: Host> {
    host: H,
    limits: Limits,
    host_calls: u64,
    regs: Vec<Value>,
    frames: Vec<Frame>,
    agg: AggHeap,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum RunMode {
    Unverified,
    Verified,
}

impl<H: Host> fmt::Debug for Vm<H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Vm")
            .field("limits", &self.limits)
            .field("host_calls", &self.host_calls)
            .field("regs_len", &self.regs.len())
            .field("frames_len", &self.frames.len())
            .finish_non_exhaustive()
    }
}

impl<H: Host> Vm<H> {
    /// Creates a new VM with `host` and `limits`.
    #[must_use]
    pub fn new(host: H, limits: Limits) -> Self {
        Self {
            host,
            limits,
            host_calls: 0,
            regs: Vec::new(),
            frames: Vec::new(),
            agg: AggHeap::new(),
        }
    }

    /// Executes `program` starting at `entry` with `args` as value arguments.
    ///
    /// Convention:
    /// - `r0` is the effect token (initialized to [`Value::Unit`] at entry).
    /// - value args are placed in `r1..=rN` where `N = entry.arg_count`.
    pub fn run(
        &mut self,
        program: &Program,
        entry: FuncId,
        args: &[Value],
    ) -> Result<Vec<Value>, TrapInfo> {
        self.run_with_mode(program, entry, args, RunMode::Unverified, None)
    }

    /// Executes `program` starting at `entry` while emitting trace events to `sink`.
    pub fn run_traced(
        &mut self,
        program: &Program,
        entry: FuncId,
        args: &[Value],
        sink: &mut dyn TraceSink,
    ) -> Result<Vec<Value>, TrapInfo> {
        self.run_with_mode(program, entry, args, RunMode::Unverified, Some(sink))
    }

    /// Executes a previously verified program.
    ///
    /// Even for verified bytecode, host calls are a trust boundary, so the VM still validates host
    /// ABI conformance (return arity and return types) at runtime.
    pub fn run_verified(
        &mut self,
        program: &VerifiedProgram,
        entry: FuncId,
        args: &[Value],
    ) -> Result<Vec<Value>, TrapInfo> {
        self.run_with_mode(program.program(), entry, args, RunMode::Verified, None)
    }

    /// Executes a previously verified program while emitting trace events to `sink`.
    pub fn run_verified_traced(
        &mut self,
        program: &VerifiedProgram,
        entry: FuncId,
        args: &[Value],
        sink: &mut dyn TraceSink,
    ) -> Result<Vec<Value>, TrapInfo> {
        self.run_with_mode(
            program.program(),
            entry,
            args,
            RunMode::Verified,
            Some(sink),
        )
    }

    fn run_with_mode(
        &mut self,
        program: &Program,
        entry: FuncId,
        args: &[Value],
        mode: RunMode,
        mut trace: Option<&mut dyn TraceSink>,
    ) -> Result<Vec<Value>, TrapInfo> {
        let trace_mask = trace.as_ref().map_or(TraceMask::NONE, |t| t.mask());
        if trace_mask.contains(TraceMask::RUN)
            && let Some(t) = trace.as_mut()
        {
            let t: &mut dyn TraceSink = &mut **t;
            let mode = match mode {
                RunMode::Unverified => TraceRunMode::Unverified,
                RunMode::Verified => TraceRunMode::Verified,
            };
            t.event(
                program,
                TraceEvent::RunStart {
                    entry,
                    mode,
                    arg_count: args.len(),
                },
            );
        }

        let result = match trace.as_mut() {
            Some(t) => {
                self.run_with_mode_inner(program, entry, args, mode, Some(&mut **t), trace_mask)
            }
            None => self.run_with_mode_inner(program, entry, args, mode, None, trace_mask),
        };

        if trace_mask.contains(TraceMask::RUN)
            && let Some(t) = trace.as_mut()
        {
            let t: &mut dyn TraceSink = &mut **t;
            let outcome = match &result {
                Ok(_) => TraceOutcome::Ok,
                Err(e) => TraceOutcome::Trap(e),
            };
            t.event(program, TraceEvent::RunEnd { outcome });
        }

        result
    }

    fn run_with_mode_inner(
        &mut self,
        program: &Program,
        entry: FuncId,
        args: &[Value],
        mode: RunMode,
        mut trace: Option<&mut dyn TraceSink>,
        trace_mask: TraceMask,
    ) -> Result<Vec<Value>, TrapInfo> {
        self.frames.clear();
        self.regs.clear();
        self.host_calls = 0;

        let entry_fn = program
            .functions
            .get(entry.0 as usize)
            .ok_or_else(|| self.trap(entry, 0, None, Trap::InvalidPc))?;
        if args.len() != entry_fn.arg_count as usize {
            return Err(self.trap(entry, 0, None, Trap::InvalidPc));
        }
        validate_entry_args(program, entry_fn, args).map_err(|t| self.trap(entry, 0, None, t))?;

        self.push_frame(program, entry, entry_fn, args, Vec::new(), 0, 0)
            .map_err(|t| self.trap(entry, 0, None, t))?;

        if trace_mask.contains(TraceMask::CALL)
            && let Some(t) = trace.as_mut()
        {
            let t: &mut dyn TraceSink = &mut **t;
            t.event(
                program,
                TraceEvent::ScopeEnter {
                    kind: ScopeKind::CallFrame { func: entry },
                    depth: self.frames.len(),
                    func: entry,
                    pc: 0,
                    span_id: self.span_at(program, entry, 0),
                },
            );
        }

        loop {
            if self.limits.fuel == 0 {
                return Err(self.trap(
                    self.cur_func(),
                    self.cur_pc(),
                    self.cur_span(program),
                    Trap::FuelExceeded,
                ));
            }
            self.limits.fuel -= 1;

            let frame_index = self
                .frames
                .len()
                .checked_sub(1)
                .ok_or_else(|| self.trap(entry, 0, None, Trap::InvalidPc))?;

            let (func_id, pc, base, reg_count, _byte_len) = {
                let f = &self.frames[frame_index];
                (f.func, f.pc, f.base, f.reg_count, f.byte_len)
            };
            let span_id = self.span_at(program, func_id, pc);

            let (opcode, instr, next_pc) = {
                let f = &self.frames[frame_index];
                fetch_at_pc(&f.decoded, f.pc, f.byte_len)
                    .ok_or_else(|| self.trap(func_id, pc, span_id, Trap::InvalidPc))?
            };

            if trace_mask.contains(TraceMask::INSTR)
                && let Some(t) = trace.as_mut()
            {
                let t: &mut dyn TraceSink = &mut **t;
                t.event(
                    program,
                    TraceEvent::Instr {
                        func: func_id,
                        pc,
                        next_pc,
                        span_id,
                        opcode,
                    },
                );
            }

            match instr {
                Instr::Nop => self.frames[frame_index].pc = next_pc,
                Instr::Mov { dst, src } => {
                    let v = read_reg_at(&self.regs, base, reg_count, src)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    write_reg_at(&mut self.regs, base, reg_count, dst, v)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }
                Instr::Trap { code } => {
                    return Err(self.trap(func_id, pc, span_id, Trap::TrapCode(code)));
                }

                Instr::ConstUnit { dst } => {
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::Unit)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }
                Instr::ConstBool { dst, imm } => {
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::Bool(imm))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }
                Instr::ConstI64 { dst, imm } => {
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::I64(imm))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }
                Instr::ConstU64 { dst, imm } => {
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::U64(imm))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }
                Instr::ConstF64 { dst, bits } => {
                    write_reg_at(
                        &mut self.regs,
                        base,
                        reg_count,
                        dst,
                        Value::F64(f64::from_bits(bits)),
                    )
                    .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }
                Instr::ConstDecimal {
                    dst,
                    mantissa,
                    scale,
                } => {
                    write_reg_at(
                        &mut self.regs,
                        base,
                        reg_count,
                        dst,
                        Value::Decimal(Decimal { mantissa, scale }),
                    )
                    .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }
                Instr::ConstPool { dst, idx } => {
                    let c = program
                        .const_pool
                        .get(idx.0 as usize)
                        .ok_or_else(|| self.trap(func_id, pc, span_id, Trap::ConstOutOfBounds))?;
                    let v = const_to_value(c, &program.const_bytes_data, &program.const_str_data)
                        .ok_or_else(|| {
                        self.trap(func_id, pc, span_id, Trap::ConstOutOfBounds)
                    })?;
                    write_reg_at(&mut self.regs, base, reg_count, dst, v)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::DecAdd { dst, a, b } => {
                    let da = read_decimal_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let db = read_decimal_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    if da.scale != db.scale {
                        return Err(self.trap(func_id, pc, span_id, Trap::DecimalScaleMismatch));
                    }
                    let mantissa = da
                        .mantissa
                        .checked_add(db.mantissa)
                        .ok_or_else(|| self.trap(func_id, pc, span_id, Trap::DecimalOverflow))?;
                    write_reg_at(
                        &mut self.regs,
                        base,
                        reg_count,
                        dst,
                        Value::Decimal(Decimal {
                            mantissa,
                            scale: da.scale,
                        }),
                    )
                    .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::DecSub { dst, a, b } => {
                    let da = read_decimal_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let db = read_decimal_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    if da.scale != db.scale {
                        return Err(self.trap(func_id, pc, span_id, Trap::DecimalScaleMismatch));
                    }
                    let mantissa = da
                        .mantissa
                        .checked_sub(db.mantissa)
                        .ok_or_else(|| self.trap(func_id, pc, span_id, Trap::DecimalOverflow))?;
                    write_reg_at(
                        &mut self.regs,
                        base,
                        reg_count,
                        dst,
                        Value::Decimal(Decimal {
                            mantissa,
                            scale: da.scale,
                        }),
                    )
                    .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::DecMul { dst, a, b } => {
                    let da = read_decimal_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let db = read_decimal_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let mantissa = da
                        .mantissa
                        .checked_mul(db.mantissa)
                        .ok_or_else(|| self.trap(func_id, pc, span_id, Trap::DecimalOverflow))?;
                    let scale_u16 = u16::from(da.scale) + u16::from(db.scale);
                    let scale = u8::try_from(scale_u16)
                        .map_err(|_| self.trap(func_id, pc, span_id, Trap::DecimalOverflow))?;
                    write_reg_at(
                        &mut self.regs,
                        base,
                        reg_count,
                        dst,
                        Value::Decimal(Decimal { mantissa, scale }),
                    )
                    .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::F64Add { dst, a, b } => {
                    let ai = read_f64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let bi = read_f64_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::F64(ai + bi))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::F64Sub { dst, a, b } => {
                    let ai = read_f64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let bi = read_f64_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::F64(ai - bi))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::F64Mul { dst, a, b } => {
                    let ai = read_f64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let bi = read_f64_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::F64(ai * bi))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::F64Div { dst, a, b } => {
                    let ai = read_f64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let bi = read_f64_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::F64(ai / bi))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::I64Add { dst, a, b } => {
                    let ai = read_i64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let bi = read_i64_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;

                    write_reg_at(
                        &mut self.regs,
                        base,
                        reg_count,
                        dst,
                        Value::I64(ai.wrapping_add(bi)),
                    )
                    .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::I64Sub { dst, a, b } => {
                    let ai = read_i64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let bi = read_i64_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    write_reg_at(
                        &mut self.regs,
                        base,
                        reg_count,
                        dst,
                        Value::I64(ai.wrapping_sub(bi)),
                    )
                    .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::I64Mul { dst, a, b } => {
                    let ai = read_i64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let bi = read_i64_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    write_reg_at(
                        &mut self.regs,
                        base,
                        reg_count,
                        dst,
                        Value::I64(ai.wrapping_mul(bi)),
                    )
                    .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::U64Add { dst, a, b } => {
                    let ai = read_u64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let bi = read_u64_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    write_reg_at(
                        &mut self.regs,
                        base,
                        reg_count,
                        dst,
                        Value::U64(ai.wrapping_add(bi)),
                    )
                    .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::U64Sub { dst, a, b } => {
                    let ai = read_u64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let bi = read_u64_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    write_reg_at(
                        &mut self.regs,
                        base,
                        reg_count,
                        dst,
                        Value::U64(ai.wrapping_sub(bi)),
                    )
                    .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::U64Mul { dst, a, b } => {
                    let ai = read_u64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let bi = read_u64_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    write_reg_at(
                        &mut self.regs,
                        base,
                        reg_count,
                        dst,
                        Value::U64(ai.wrapping_mul(bi)),
                    )
                    .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::U64And { dst, a, b } => {
                    let ai = read_u64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let bi = read_u64_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::U64(ai & bi))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::U64Or { dst, a, b } => {
                    let ai = read_u64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let bi = read_u64_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::U64(ai | bi))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::I64Eq { dst, a, b } => {
                    let ai = read_i64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let bi = read_i64_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::Bool(ai == bi))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::I64Lt { dst, a, b } => {
                    let ai = read_i64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let bi = read_i64_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::Bool(ai < bi))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::U64Eq { dst, a, b } => {
                    let ai = read_u64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let bi = read_u64_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::Bool(ai == bi))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::U64Lt { dst, a, b } => {
                    let ai = read_u64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let bi = read_u64_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::Bool(ai < bi))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::U64Xor { dst, a, b } => {
                    let ai = read_u64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let bi = read_u64_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::U64(ai ^ bi))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::U64Shl { dst, a, b } => {
                    let ai = read_u64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let bi = read_u64_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let sh = (bi & 63) as u32;
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::U64(ai << sh))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::U64Shr { dst, a, b } => {
                    let ai = read_u64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let bi = read_u64_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let sh = (bi & 63) as u32;
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::U64(ai >> sh))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::U64Gt { dst, a, b } => {
                    let ai = read_u64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let bi = read_u64_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::Bool(ai > bi))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::BoolNot { dst, a } => {
                    let v = read_reg_at(&self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let Value::Bool(b) = v else {
                        return Err(self.trap(
                            func_id,
                            pc,
                            span_id,
                            trap_expected(mode, ValueType::Bool, &v),
                        ));
                    };
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::Bool(!b))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::F64Eq { dst, a, b } => {
                    let ai = read_f64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let bi = read_f64_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::Bool(ai == bi))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::F64Lt { dst, a, b } => {
                    let ai = read_f64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let bi = read_f64_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::Bool(ai < bi))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::F64Gt { dst, a, b } => {
                    let ai = read_f64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let bi = read_f64_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::Bool(ai > bi))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::F64Le { dst, a, b } => {
                    let ai = read_f64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let bi = read_f64_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::Bool(ai <= bi))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::F64Ge { dst, a, b } => {
                    let ai = read_f64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let bi = read_f64_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::Bool(ai >= bi))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::U64Le { dst, a, b } => {
                    let ai = read_u64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let bi = read_u64_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::Bool(ai <= bi))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::U64Ge { dst, a, b } => {
                    let ai = read_u64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let bi = read_u64_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::Bool(ai >= bi))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::I64And { dst, a, b } => {
                    let ai = read_i64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let bi = read_i64_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::I64(ai & bi))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::U64ToI64 { dst, a } => {
                    let u = read_u64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let i = i64::try_from(u)
                        .map_err(|_| self.trap(func_id, pc, span_id, Trap::IntCastOverflow))?;
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::I64(i))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::I64ToU64 { dst, a } => {
                    let i = read_i64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let u = u64::try_from(i)
                        .map_err(|_| self.trap(func_id, pc, span_id, Trap::IntCastOverflow))?;
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::U64(u))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::I64Or { dst, a, b } => {
                    let ai = read_i64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let bi = read_i64_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::I64(ai | bi))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::I64Xor { dst, a, b } => {
                    let ai = read_i64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let bi = read_i64_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::I64(ai ^ bi))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::Select { dst, cond, a, b } => {
                    let c = read_reg_at(&self.regs, base, reg_count, cond)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let Value::Bool(c) = c else {
                        return Err(self.trap(
                            func_id,
                            pc,
                            span_id,
                            trap_expected(mode, ValueType::Bool, &c),
                        ));
                    };
                    let va = read_reg_at(&self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let vb = read_reg_at(&self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    if mode == RunMode::Unverified {
                        let ta = value_type_of(&va);
                        let tb = value_type_of(&vb);
                        if ta != tb {
                            return Err(self.trap(
                                func_id,
                                pc,
                                span_id,
                                Trap::TypeMismatch {
                                    expected: ta,
                                    actual: tb,
                                },
                            ));
                        }
                    }
                    write_reg_at(
                        &mut self.regs,
                        base,
                        reg_count,
                        dst,
                        if c { va } else { vb },
                    )
                    .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::I64Gt { dst, a, b } => {
                    let ai = read_i64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let bi = read_i64_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::Bool(ai > bi))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::I64Le { dst, a, b } => {
                    let ai = read_i64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let bi = read_i64_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::Bool(ai <= bi))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::I64Ge { dst, a, b } => {
                    let ai = read_i64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let bi = read_i64_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::Bool(ai >= bi))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::I64Shl { dst, a, b } => {
                    let ai = read_i64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let bi = read_i64_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let sh = (bi as u64 & 63) as u32;
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::I64(ai << sh))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::I64Shr { dst, a, b } => {
                    let ai = read_i64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let bi = read_i64_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let sh = (bi as u64 & 63) as u32;
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::I64(ai >> sh))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::Br {
                    cond,
                    pc_true,
                    pc_false,
                } => {
                    let v = read_reg_at(&self.regs, base, reg_count, cond)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let b = matches!(v, Value::Bool(true));
                    self.frames[frame_index].pc = if b { pc_true } else { pc_false };
                }
                Instr::Jmp { pc_target } => self.frames[frame_index].pc = pc_target,

                Instr::Call {
                    eff_out,
                    func_id: callee,
                    eff_in: _,
                    args,
                    rets,
                } => {
                    let callee_fn = program
                        .functions
                        .get(callee.0 as usize)
                        .ok_or_else(|| self.trap(func_id, pc, span_id, Trap::InvalidPc))?;

                    let mut call_args = Vec::with_capacity(args.len());
                    for a in &args {
                        call_args.push(
                            read_reg_at(&self.regs, base, reg_count, *a)
                                .map_err(|t| self.trap(func_id, pc, span_id, t))?,
                        );
                    }

                    // v1: effect token is `Unit`.
                    write_reg_at(&mut self.regs, base, reg_count, eff_out, Value::Unit)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;

                    let ret_base = base;
                    let ret_pc = next_pc;
                    self.push_frame(
                        program, callee, callee_fn, &call_args, rets, ret_base, ret_pc,
                    )
                    .map_err(|t| self.trap(func_id, pc, span_id, t))?;

                    if trace_mask.contains(TraceMask::CALL)
                        && let Some(t) = trace.as_mut()
                    {
                        let t: &mut dyn TraceSink = &mut **t;
                        t.event(
                            program,
                            TraceEvent::ScopeEnter {
                                kind: ScopeKind::CallFrame { func: callee },
                                depth: self.frames.len(),
                                func: callee,
                                pc: 0,
                                span_id: self.span_at(program, callee, 0),
                            },
                        );
                    }
                }

                Instr::Ret { eff_in: _, rets } => {
                    if trace_mask.contains(TraceMask::CALL)
                        && let Some(t) = trace.as_mut()
                    {
                        let t: &mut dyn TraceSink = &mut **t;
                        t.event(
                            program,
                            TraceEvent::ScopeExit {
                                kind: ScopeKind::CallFrame { func: func_id },
                                depth: self.frames.len(),
                                func: func_id,
                                pc,
                                span_id,
                            },
                        );
                    }

                    let mut ret_vals = Vec::with_capacity(rets.len());
                    for r in &rets {
                        ret_vals.push(
                            read_reg_at(&self.regs, base, reg_count, *r)
                                .map_err(|t| self.trap(func_id, pc, span_id, t))?,
                        );
                    }

                    let finished = self
                        .frames
                        .pop()
                        .ok_or_else(|| self.trap(func_id, pc, span_id, Trap::InvalidPc))?;
                    self.regs.truncate(finished.base);

                    if self.frames.is_empty() {
                        return Ok(ret_vals);
                    }

                    let caller_index = self.frames.len() - 1;
                    let caller_func = self.frames[caller_index].func;
                    let caller_pc = self.frames[caller_index].pc;
                    let caller_span = self.span_at(program, caller_func, caller_pc);

                    if finished.rets.len() != ret_vals.len() {
                        return Err(self.trap(
                            caller_func,
                            caller_pc,
                            caller_span,
                            Trap::InvalidPc,
                        ));
                    }
                    for (dst, v) in finished.rets.iter().zip(ret_vals.into_iter()) {
                        let idx = finished.ret_base + (*dst as usize);
                        if idx >= self.regs.len() {
                            return Err(self.trap(
                                caller_func,
                                caller_pc,
                                caller_span,
                                Trap::RegOutOfBounds,
                            ));
                        }
                        self.regs[idx] = v;
                    }
                    self.frames[caller_index].pc = finished.ret_pc;
                }

                Instr::HostCall {
                    eff_out,
                    host_sig,
                    eff_in: _,
                    args,
                    rets,
                } => {
                    if self.host_calls >= self.limits.max_host_calls {
                        return Err(self.trap(func_id, pc, span_id, Trap::HostCallLimitExceeded));
                    }
                    self.host_calls += 1;

                    let hs = program
                        .host_sig(host_sig)
                        .ok_or_else(|| self.trap(func_id, pc, span_id, Trap::ConstOutOfBounds))?;
                    let sym = program
                        .symbol_str(hs.symbol)
                        .map_err(|_| self.trap(func_id, pc, span_id, Trap::ConstOutOfBounds))?;

                    let arg_types = program
                        .host_sig_args(hs)
                        .map_err(|_| self.trap(func_id, pc, span_id, Trap::ConstOutOfBounds))?;
                    let ret_types = program
                        .host_sig_rets(hs)
                        .map_err(|_| self.trap(func_id, pc, span_id, Trap::ConstOutOfBounds))?;

                    let mut call_args = Vec::with_capacity(args.len());
                    for (i, a) in args.iter().enumerate() {
                        call_args.push(
                            read_reg_at(&self.regs, base, reg_count, *a)
                                .map_err(|t| self.trap(func_id, pc, span_id, t))?,
                        );
                        if mode == RunMode::Unverified
                            && let Some(&expected) = arg_types.get(i)
                        {
                            check_value_type(&call_args[i], expected)
                                .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                        }
                    }

                    if trace_mask.contains(TraceMask::HOST)
                        && let Some(t) = trace.as_mut()
                    {
                        let t: &mut dyn TraceSink = &mut **t;
                        t.event(
                            program,
                            TraceEvent::ScopeEnter {
                                kind: ScopeKind::HostCall {
                                    host_sig,
                                    symbol: hs.symbol,
                                    sig_hash: hs.sig_hash,
                                },
                                depth: self.frames.len(),
                                func: func_id,
                                pc,
                                span_id,
                            },
                        );
                    }

                    let (mut out_vals, extra_fuel) =
                        self.host.call(sym, hs.sig_hash, &call_args).map_err(|e| {
                            self.trap(func_id, pc, span_id, Trap::HostCallFailed(e))
                        })?;
                    self.limits.fuel = self.limits.fuel.saturating_sub(extra_fuel);

                    if trace_mask.contains(TraceMask::HOST)
                        && let Some(t) = trace.as_mut()
                    {
                        let t: &mut dyn TraceSink = &mut **t;
                        t.event(
                            program,
                            TraceEvent::ScopeExit {
                                kind: ScopeKind::HostCall {
                                    host_sig,
                                    symbol: hs.symbol,
                                    sig_hash: hs.sig_hash,
                                },
                                depth: self.frames.len(),
                                func: func_id,
                                pc,
                                span_id,
                            },
                        );
                    }

                    write_reg_at(&mut self.regs, base, reg_count, eff_out, Value::Unit)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;

                    let expected = u32::try_from(ret_types.len()).unwrap_or(u32::MAX);
                    let actual = u32::try_from(out_vals.len()).unwrap_or(u32::MAX);
                    if out_vals.len() != ret_types.len() || out_vals.len() != rets.len() {
                        return Err(self.trap(
                            func_id,
                            pc,
                            span_id,
                            Trap::HostReturnArityMismatch { expected, actual },
                        ));
                    }
                    for (v, &expected) in out_vals.iter().zip(ret_types.iter()) {
                        check_value_type(v, expected)
                            .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    }
                    for (dst, v) in rets.iter().zip(out_vals.drain(..)) {
                        write_reg_at(&mut self.regs, base, reg_count, *dst, v)
                            .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    }
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::TupleNew { dst, values } => {
                    let mut vals = Vec::with_capacity(values.len());
                    for r in &values {
                        vals.push(
                            read_reg_at(&self.regs, base, reg_count, *r)
                                .map_err(|t| self.trap(func_id, pc, span_id, t))?,
                        );
                    }
                    let h = self.agg.tuple_new(vals);
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::Agg(h))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::TupleGet { dst, tuple, index } => {
                    let v = read_reg_at(&self.regs, base, reg_count, tuple)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let Value::Agg(h) = v else {
                        return Err(self.trap(
                            func_id,
                            pc,
                            span_id,
                            trap_expected(mode, ValueType::Agg, &v),
                        ));
                    };
                    let out = self
                        .agg
                        .tuple_get(h, index as usize)
                        .map_err(|e| self.trap(func_id, pc, span_id, Trap::AggError(e)))?;
                    write_reg_at(&mut self.regs, base, reg_count, dst, out)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::StructNew {
                    dst,
                    type_id,
                    values,
                } => {
                    let st = program
                        .types
                        .structs
                        .get(type_id.0 as usize)
                        .ok_or_else(|| self.trap(func_id, pc, span_id, Trap::TypeIdOutOfBounds))?;
                    let field_types = program
                        .types
                        .struct_field_types(st)
                        .map_err(|_| self.trap(func_id, pc, span_id, Trap::TypeIdOutOfBounds))?;
                    if field_types.len() != values.len() {
                        return Err(self.trap(func_id, pc, span_id, Trap::ArityMismatch));
                    }

                    let mut vals = Vec::with_capacity(values.len());
                    for (reg, &expected) in values.iter().zip(field_types.iter()) {
                        let v = read_reg_at(&self.regs, base, reg_count, *reg)
                            .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                        if mode == RunMode::Unverified {
                            check_value_type(&v, expected)
                                .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                        }
                        vals.push(v);
                    }
                    let h = self.agg.struct_new(type_id, vals);
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::Agg(h))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::StructGet {
                    dst,
                    st,
                    field_index,
                } => {
                    let v = read_reg_at(&self.regs, base, reg_count, st)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let Value::Agg(h) = v else {
                        return Err(self.trap(
                            func_id,
                            pc,
                            span_id,
                            trap_expected(mode, ValueType::Agg, &v),
                        ));
                    };
                    let out = self
                        .agg
                        .struct_get(h, field_index as usize)
                        .map_err(|e| self.trap(func_id, pc, span_id, Trap::AggError(e)))?;
                    write_reg_at(&mut self.regs, base, reg_count, dst, out)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::ArrayNew {
                    dst,
                    elem_type_id,
                    len,
                    values,
                } => {
                    let expected = program
                        .types
                        .array_elems
                        .get(elem_type_id.0 as usize)
                        .copied()
                        .ok_or_else(|| {
                            self.trap(func_id, pc, span_id, Trap::ElemTypeIdOutOfBounds)
                        })?;
                    if len as usize != values.len() {
                        return Err(self.trap(func_id, pc, span_id, Trap::ArityMismatch));
                    }

                    let mut vals = Vec::with_capacity(values.len());
                    for reg in &values {
                        let v = read_reg_at(&self.regs, base, reg_count, *reg)
                            .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                        if mode == RunMode::Unverified {
                            check_value_type(&v, expected)
                                .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                        }
                        vals.push(v);
                    }
                    let h = self.agg.array_new(elem_type_id, vals);
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::Agg(h))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::ArrayLen { dst, arr } => {
                    let h = read_agg_handle_at(mode, &self.regs, base, reg_count, arr)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let n = self
                        .agg
                        .array_len(h)
                        .map_err(|e| self.trap(func_id, pc, span_id, Trap::AggError(e)))?;
                    write_reg_at(
                        &mut self.regs,
                        base,
                        reg_count,
                        dst,
                        Value::U64(u64::try_from(n).unwrap_or(u64::MAX)),
                    )
                    .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::ArrayGet { dst, arr, index } => {
                    let h = read_agg_handle_at(mode, &self.regs, base, reg_count, arr)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;

                    let ix = read_u64_at(mode, &self.regs, base, reg_count, index)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let ix = usize::try_from(ix).unwrap_or(usize::MAX);

                    let out = self
                        .agg
                        .array_get(h, ix)
                        .map_err(|e| self.trap(func_id, pc, span_id, Trap::AggError(e)))?;
                    write_reg_at(&mut self.regs, base, reg_count, dst, out)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::ArrayGetImm { dst, arr, index } => {
                    let h = read_agg_handle_at(mode, &self.regs, base, reg_count, arr)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let ix = usize::try_from(index).unwrap_or(usize::MAX);
                    let out = self
                        .agg
                        .array_get(h, ix)
                        .map_err(|e| self.trap(func_id, pc, span_id, Trap::AggError(e)))?;
                    write_reg_at(&mut self.regs, base, reg_count, dst, out)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::TupleLen { dst, tuple } => {
                    let h = read_agg_handle_at(mode, &self.regs, base, reg_count, tuple)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let n = self
                        .agg
                        .tuple_len(h)
                        .map_err(|e| self.trap(func_id, pc, span_id, Trap::AggError(e)))?;
                    write_reg_at(
                        &mut self.regs,
                        base,
                        reg_count,
                        dst,
                        Value::U64(u64::try_from(n).unwrap_or(u64::MAX)),
                    )
                    .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::StructFieldCount { dst, st } => {
                    let h = read_agg_handle_at(mode, &self.regs, base, reg_count, st)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let n = self
                        .agg
                        .struct_field_count(h)
                        .map_err(|e| self.trap(func_id, pc, span_id, Trap::AggError(e)))?;
                    write_reg_at(
                        &mut self.regs,
                        base,
                        reg_count,
                        dst,
                        Value::U64(u64::try_from(n).unwrap_or(u64::MAX)),
                    )
                    .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::BytesLen { dst, bytes } => {
                    let v = read_reg_at(&self.regs, base, reg_count, bytes)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let Value::Bytes(b) = v else {
                        return Err(self.trap(
                            func_id,
                            pc,
                            span_id,
                            trap_expected(mode, ValueType::Bytes, &v),
                        ));
                    };
                    write_reg_at(
                        &mut self.regs,
                        base,
                        reg_count,
                        dst,
                        Value::U64(u64::try_from(b.len()).unwrap_or(u64::MAX)),
                    )
                    .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::StrLen { dst, s } => {
                    let v = read_reg_at(&self.regs, base, reg_count, s)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let Value::Str(s) = v else {
                        return Err(self.trap(
                            func_id,
                            pc,
                            span_id,
                            trap_expected(mode, ValueType::Str, &v),
                        ));
                    };
                    write_reg_at(
                        &mut self.regs,
                        base,
                        reg_count,
                        dst,
                        Value::U64(u64::try_from(s.len()).unwrap_or(u64::MAX)),
                    )
                    .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::I64Div { dst, a, b } => {
                    let a = read_i64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let b = read_i64_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    if b == 0 {
                        return Err(self.trap(func_id, pc, span_id, Trap::DivByZero));
                    }
                    if a == i64::MIN && b == -1 {
                        return Err(self.trap(func_id, pc, span_id, Trap::IntDivOverflow));
                    }
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::I64(a / b))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::I64Rem { dst, a, b } => {
                    let a = read_i64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let b = read_i64_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    if b == 0 {
                        return Err(self.trap(func_id, pc, span_id, Trap::DivByZero));
                    }
                    if a == i64::MIN && b == -1 {
                        return Err(self.trap(func_id, pc, span_id, Trap::IntDivOverflow));
                    }
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::I64(a % b))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::U64Div { dst, a, b } => {
                    let a = read_u64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let b = read_u64_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    if b == 0 {
                        return Err(self.trap(func_id, pc, span_id, Trap::DivByZero));
                    }
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::U64(a / b))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::U64Rem { dst, a, b } => {
                    let a = read_u64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let b = read_u64_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    if b == 0 {
                        return Err(self.trap(func_id, pc, span_id, Trap::DivByZero));
                    }
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::U64(a % b))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::I64ToF64 { dst, a } => {
                    let a = read_i64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::F64(a as f64))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::U64ToF64 { dst, a } => {
                    let a = read_u64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::F64(a as f64))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::F64ToI64 { dst, a } => {
                    let a = read_f64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    if !a.is_finite() {
                        return Err(self.trap(func_id, pc, span_id, Trap::FloatToIntInvalid));
                    }
                    if a < i64::MIN as f64 || a > i64::MAX as f64 {
                        return Err(self.trap(func_id, pc, span_id, Trap::IntCastOverflow));
                    }
                    #[allow(
                        clippy::cast_possible_truncation,
                        reason = "we implement truncating float-to-int casts after explicit finite/range checks"
                    )]
                    let out = a as i64;
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::I64(out))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::F64ToU64 { dst, a } => {
                    let a = read_f64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    if !a.is_finite() {
                        return Err(self.trap(func_id, pc, span_id, Trap::FloatToIntInvalid));
                    }
                    if a < 0.0 || a > u64::MAX as f64 {
                        return Err(self.trap(func_id, pc, span_id, Trap::IntCastOverflow));
                    }
                    #[allow(
                        clippy::cast_possible_truncation,
                        reason = "we implement truncating float-to-int casts after explicit finite/range checks"
                    )]
                    let out = a as u64;
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::U64(out))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::DecToI64 { dst, a } => {
                    let a = read_decimal_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    if a.scale != 0 {
                        return Err(self.trap(func_id, pc, span_id, Trap::DecimalScaleMismatch));
                    }
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::I64(a.mantissa))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::DecToU64 { dst, a } => {
                    let a = read_decimal_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    if a.scale != 0 {
                        return Err(self.trap(func_id, pc, span_id, Trap::DecimalScaleMismatch));
                    }
                    if a.mantissa < 0 {
                        return Err(self.trap(func_id, pc, span_id, Trap::IntCastOverflow));
                    }
                    write_reg_at(
                        &mut self.regs,
                        base,
                        reg_count,
                        dst,
                        Value::U64(u64::try_from(a.mantissa).unwrap_or(u64::MAX)),
                    )
                    .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::I64ToDec { dst, a, scale } => {
                    let a = read_i64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let mut factor: i64 = 1;
                    for _ in 0..scale {
                        factor = factor.checked_mul(10).ok_or_else(|| {
                            self.trap(func_id, pc, span_id, Trap::DecimalOverflow)
                        })?;
                    }
                    let mantissa = a
                        .checked_mul(factor)
                        .ok_or_else(|| self.trap(func_id, pc, span_id, Trap::DecimalOverflow))?;
                    write_reg_at(
                        &mut self.regs,
                        base,
                        reg_count,
                        dst,
                        Value::Decimal(Decimal { mantissa, scale }),
                    )
                    .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::U64ToDec { dst, a, scale } => {
                    let a = read_u64_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let mut factor: i64 = 1;
                    for _ in 0..scale {
                        factor = factor.checked_mul(10).ok_or_else(|| {
                            self.trap(func_id, pc, span_id, Trap::DecimalOverflow)
                        })?;
                    }
                    let mantissa = i128::from(a)
                        .checked_mul(i128::from(factor))
                        .and_then(|x| i64::try_from(x).ok())
                        .ok_or_else(|| self.trap(func_id, pc, span_id, Trap::DecimalOverflow))?;
                    write_reg_at(
                        &mut self.regs,
                        base,
                        reg_count,
                        dst,
                        Value::Decimal(Decimal { mantissa, scale }),
                    )
                    .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::BytesEq { dst, a, b } => {
                    let a = read_bytes_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let b = read_bytes_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::Bool(a == b))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::StrEq { dst, a, b } => {
                    let a = read_str_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let b = read_str_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::Bool(a == b))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::BytesConcat { dst, a, b } => {
                    let a = read_bytes_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let b = read_bytes_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let mut out = Vec::with_capacity(a.len() + b.len());
                    out.extend_from_slice(&a);
                    out.extend_from_slice(&b);
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::Bytes(out))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::StrConcat { dst, a, b } => {
                    let a = read_str_at(mode, &self.regs, base, reg_count, a)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let b = read_str_at(mode, &self.regs, base, reg_count, b)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let mut out = String::with_capacity(a.len() + b.len());
                    out.push_str(&a);
                    out.push_str(&b);
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::Str(out))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::BytesGet { dst, bytes, index } => {
                    let bytes = read_bytes_at(mode, &self.regs, base, reg_count, bytes)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let index = read_u64_at(mode, &self.regs, base, reg_count, index)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let index = usize::try_from(index).unwrap_or(usize::MAX);
                    let b = bytes
                        .get(index)
                        .ok_or_else(|| self.trap(func_id, pc, span_id, Trap::IndexOutOfBounds))?;
                    write_reg_at(
                        &mut self.regs,
                        base,
                        reg_count,
                        dst,
                        Value::U64(u64::from(*b)),
                    )
                    .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::BytesGetImm { dst, bytes, index } => {
                    let bytes = read_bytes_at(mode, &self.regs, base, reg_count, bytes)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let index = usize::try_from(index).unwrap_or(usize::MAX);
                    let b = bytes
                        .get(index)
                        .ok_or_else(|| self.trap(func_id, pc, span_id, Trap::IndexOutOfBounds))?;
                    write_reg_at(
                        &mut self.regs,
                        base,
                        reg_count,
                        dst,
                        Value::U64(u64::from(*b)),
                    )
                    .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::BytesSlice {
                    dst,
                    bytes,
                    start,
                    end,
                } => {
                    let bytes = read_bytes_at(mode, &self.regs, base, reg_count, bytes)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let start = read_u64_at(mode, &self.regs, base, reg_count, start)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let end = read_u64_at(mode, &self.regs, base, reg_count, end)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let start = usize::try_from(start).unwrap_or(usize::MAX);
                    let end = usize::try_from(end).unwrap_or(usize::MAX);
                    if start > end || end > bytes.len() {
                        return Err(self.trap(func_id, pc, span_id, Trap::IndexOutOfBounds));
                    }
                    write_reg_at(
                        &mut self.regs,
                        base,
                        reg_count,
                        dst,
                        Value::Bytes(bytes[start..end].to_vec()),
                    )
                    .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::StrSlice { dst, s, start, end } => {
                    let s = read_str_at(mode, &self.regs, base, reg_count, s)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let start = read_u64_at(mode, &self.regs, base, reg_count, start)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let end = read_u64_at(mode, &self.regs, base, reg_count, end)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let start = usize::try_from(start).unwrap_or(usize::MAX);
                    let end = usize::try_from(end).unwrap_or(usize::MAX);
                    if start > end || end > s.len() {
                        return Err(self.trap(func_id, pc, span_id, Trap::IndexOutOfBounds));
                    }
                    if !s.is_char_boundary(start) || !s.is_char_boundary(end) {
                        return Err(self.trap(func_id, pc, span_id, Trap::StrNotCharBoundary));
                    }
                    write_reg_at(
                        &mut self.regs,
                        base,
                        reg_count,
                        dst,
                        Value::Str(String::from(&s[start..end])),
                    )
                    .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::StrToBytes { dst, s } => {
                    let s = read_str_at(mode, &self.regs, base, reg_count, s)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    write_reg_at(
                        &mut self.regs,
                        base,
                        reg_count,
                        dst,
                        Value::Bytes(s.as_bytes().to_vec()),
                    )
                    .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
                }

                Instr::BytesToStr { dst, bytes } => {
                    let bytes = read_bytes_at(mode, &self.regs, base, reg_count, bytes)
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    let out = String::from_utf8(bytes)
                        .map_err(|_| self.trap(func_id, pc, span_id, Trap::InvalidUtf8))?;
                    write_reg_at(&mut self.regs, base, reg_count, dst, Value::Str(out))
                        .map_err(|t| self.trap(func_id, pc, span_id, t))?;
                    self.frames[frame_index].pc = next_pc;
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

    fn push_frame(
        &mut self,
        program: &Program,
        func_id: FuncId,
        func: &Function,
        args: &[Value],
        rets: Vec<u32>,
        ret_base: usize,
        ret_pc: u32,
    ) -> Result<(), Trap> {
        if self.frames.len() >= self.limits.max_call_depth {
            return Err(Trap::CallDepthExceeded);
        }

        let bytecode = program
            .function_bytecode(func)
            .map_err(|_| Trap::InvalidPc)?;
        let decoded = decode_instructions(bytecode).map_err(|_| Trap::BytecodeDecode)?;
        let byte_len = u32::try_from(bytecode.len()).map_err(|_| Trap::InvalidPc)?;

        let reg_count = func.reg_count as usize;
        let base = self.regs.len();
        self.regs.resize(base + reg_count, Value::Unit);

        // r0 effect
        if reg_count > 0 {
            self.regs[base] = Value::Unit;
        }
        for (i, v) in args.iter().enumerate() {
            let dst = base + 1 + i;
            if dst >= base + reg_count {
                return Err(Trap::RegOutOfBounds);
            }
            self.regs[dst] = v.clone();
        }

        self.frames.push(Frame {
            func: func_id,
            pc: 0,
            base,
            reg_count,
            byte_len,
            decoded,
            rets,
            ret_base,
            ret_pc,
        });

        Ok(())
    }

    fn cur_func(&self) -> FuncId {
        self.frames.last().map(|f| f.func).unwrap_or(FuncId(0))
    }

    fn cur_pc(&self) -> u32 {
        self.frames.last().map(|f| f.pc).unwrap_or(0)
    }

    fn cur_span(&self, program: &Program) -> Option<u64> {
        let frame = self.frames.last()?;
        self.span_at(program, frame.func, frame.pc)
    }

    fn span_at(&self, program: &Program, func_id: FuncId, pc: u32) -> Option<u64> {
        let func = program.functions.get(func_id.0 as usize)?;
        let spans = program.function_spans(func).ok()?;
        let mut cur: Option<u64> = None;
        let mut at: u64 = 0;
        for s in spans {
            at = at.checked_add(s.pc_delta)?;
            if at <= u64::from(pc) {
                cur = Some(s.span_id);
            } else {
                break;
            }
        }
        cur
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

fn fetch_at_pc(decoded: &[DecodedInstr], pc: u32, byte_len: u32) -> Option<(u8, Instr, u32)> {
    for (i, di) in decoded.iter().enumerate() {
        if di.offset == pc {
            let next_pc = decoded.get(i + 1).map(|n| n.offset).unwrap_or(byte_len);
            return Some((di.opcode, di.instr.clone(), next_pc));
        }
    }
    None
}

fn const_to_value(c: &ConstEntry, const_bytes: &[u8], const_str: &str) -> Option<Value> {
    Some(match c {
        ConstEntry::Unit => Value::Unit,
        ConstEntry::Bool(b) => Value::Bool(*b),
        ConstEntry::I64(i) => Value::I64(*i),
        ConstEntry::U64(u) => Value::U64(*u),
        ConstEntry::F64(bits) => Value::F64(f64::from_bits(*bits)),
        ConstEntry::Decimal { mantissa, scale } => Value::Decimal(Decimal {
            mantissa: *mantissa,
            scale: *scale,
        }),
        ConstEntry::Bytes(r) => {
            let start = r.offset as usize;
            let end = r.end().ok()? as usize;
            Value::Bytes(const_bytes.get(start..end)?.to_vec())
        }
        ConstEntry::Str(r) => {
            let start = r.offset as usize;
            let end = r.end().ok()? as usize;
            Value::Str(const_str.get(start..end)?.into())
        }
    })
}

fn value_type_of(v: &Value) -> ValueType {
    match v {
        Value::Unit => ValueType::Unit,
        Value::Bool(_) => ValueType::Bool,
        Value::I64(_) => ValueType::I64,
        Value::U64(_) => ValueType::U64,
        Value::F64(_) => ValueType::F64,
        Value::Decimal(_) => ValueType::Decimal,
        Value::Bytes(_) => ValueType::Bytes,
        Value::Str(_) => ValueType::Str,
        Value::Obj(o) => ValueType::Obj(o.host_type),
        Value::Agg(_) => ValueType::Agg,
        Value::Func(_) => ValueType::Func,
    }
}

fn check_value_type(v: &Value, expected: ValueType) -> Result<(), Trap> {
    if expected == ValueType::Any {
        return Ok(());
    }
    let actual = value_type_of(v);
    if actual != expected {
        return Err(Trap::TypeMismatch { expected, actual });
    }
    Ok(())
}

fn validate_entry_args(program: &Program, entry_fn: &Function, args: &[Value]) -> Result<(), Trap> {
    let arg_types = program
        .function_arg_types(entry_fn)
        .map_err(|_| Trap::InvalidPc)?;
    if arg_types.len() != args.len() {
        return Err(Trap::InvalidPc);
    }
    for (v, &t) in args.iter().zip(arg_types.iter()) {
        check_value_type(v, t)?;
    }
    Ok(())
}

fn trap_expected(mode: RunMode, expected: ValueType, actual: &Value) -> Trap {
    debug_assert!(
        mode == RunMode::Unverified || value_type_of(actual) == expected,
        "verified execution saw unexpected value type"
    );
    match mode {
        RunMode::Unverified => Trap::TypeMismatch {
            expected,
            actual: value_type_of(actual),
        },
        RunMode::Verified => Trap::InvalidPc,
    }
}

fn read_i64_at(
    mode: RunMode,
    regs: &[Value],
    base: usize,
    reg_count: usize,
    reg: u32,
) -> Result<i64, Trap> {
    let v = read_reg_at(regs, base, reg_count, reg)?;
    match v {
        Value::I64(x) => Ok(x),
        other => Err(trap_expected(mode, ValueType::I64, &other)),
    }
}

fn read_u64_at(
    mode: RunMode,
    regs: &[Value],
    base: usize,
    reg_count: usize,
    reg: u32,
) -> Result<u64, Trap> {
    let v = read_reg_at(regs, base, reg_count, reg)?;
    match v {
        Value::U64(x) => Ok(x),
        other => Err(trap_expected(mode, ValueType::U64, &other)),
    }
}

fn read_f64_at(
    mode: RunMode,
    regs: &[Value],
    base: usize,
    reg_count: usize,
    reg: u32,
) -> Result<f64, Trap> {
    let v = read_reg_at(regs, base, reg_count, reg)?;
    match v {
        Value::F64(x) => Ok(x),
        other => Err(trap_expected(mode, ValueType::F64, &other)),
    }
}

fn read_bytes_at(
    mode: RunMode,
    regs: &[Value],
    base: usize,
    reg_count: usize,
    reg: u32,
) -> Result<Vec<u8>, Trap> {
    let v = read_reg_at(regs, base, reg_count, reg)?;
    match v {
        Value::Bytes(x) => Ok(x),
        other => Err(trap_expected(mode, ValueType::Bytes, &other)),
    }
}

fn read_str_at(
    mode: RunMode,
    regs: &[Value],
    base: usize,
    reg_count: usize,
    reg: u32,
) -> Result<String, Trap> {
    let v = read_reg_at(regs, base, reg_count, reg)?;
    match v {
        Value::Str(x) => Ok(x),
        other => Err(trap_expected(mode, ValueType::Str, &other)),
    }
}

fn read_agg_handle_at(
    mode: RunMode,
    regs: &[Value],
    base: usize,
    reg_count: usize,
    reg: u32,
) -> Result<AggHandle, Trap> {
    let v = read_reg_at(regs, base, reg_count, reg)?;
    match v {
        Value::Agg(x) => Ok(x),
        other => Err(trap_expected(mode, ValueType::Agg, &other)),
    }
}

fn read_decimal_at(
    mode: RunMode,
    regs: &[Value],
    base: usize,
    reg_count: usize,
    reg: u32,
) -> Result<Decimal, Trap> {
    let v = read_reg_at(regs, base, reg_count, reg)?;
    match v {
        Value::Decimal(x) => Ok(x),
        other => Err(trap_expected(mode, ValueType::Decimal, &other)),
    }
}

fn read_reg_at(regs: &[Value], base: usize, reg_count: usize, reg: u32) -> Result<Value, Trap> {
    let idx = base + (reg as usize);
    if idx >= base + reg_count {
        return Err(Trap::RegOutOfBounds);
    }
    Ok(regs[idx].clone())
}

fn write_reg_at(
    regs: &mut [Value],
    base: usize,
    reg_count: usize,
    reg: u32,
    v: Value,
) -> Result<(), Trap> {
    let idx = base + (reg as usize);
    if idx >= base + reg_count {
        return Err(Trap::RegOutOfBounds);
    }
    regs[idx] = v;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asm::Asm;
    use crate::asm::FunctionSig;
    use crate::asm::ProgramBuilder;
    use crate::host::{HostSig, SigHash, sig_hash};
    use crate::program::{FunctionDef, Program, StructTypeDef, SymbolId, TypeTableDef, ValueType};
    use crate::trace::{TraceEvent, TraceMask, TraceOutcome, TraceSink};
    use alloc::vec;
    use alloc::vec::Vec;

    struct TestHost;

    impl Host for TestHost {
        fn call(
            &mut self,
            symbol: &str,
            _sig_hash: SigHash,
            args: &[Value],
        ) -> Result<(Vec<Value>, u64), HostError> {
            match symbol {
                "id" => Ok((args.to_vec(), 0)),
                _ => Err(HostError::UnknownSymbol),
            }
        }
    }

    struct CountingHost {
        calls: u64,
    }

    impl Host for CountingHost {
        fn call(
            &mut self,
            _symbol: &str,
            _sig_hash: SigHash,
            _args: &[Value],
        ) -> Result<(Vec<Value>, u64), HostError> {
            self.calls += 1;
            Ok((Vec::new(), 0))
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
                reg_count: 2,
            },
        )
        .unwrap();
        let p = pb.build_checked().unwrap();

        let mut vm = Vm::new(TestHost, Limits::default());
        let out = vm.run(&p, FuncId(0), &[]).unwrap();
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

            fn event(&mut self, _program: &Program, event: TraceEvent<'_>) {
                match event {
                    TraceEvent::RunStart { .. } => self.starts += 1,
                    TraceEvent::Instr { opcode, .. } => self.instrs.push(opcode),
                    TraceEvent::RunEnd { outcome } => {
                        self.ends += 1;
                        assert!(matches!(outcome, TraceOutcome::Ok));
                    }
                    _ => {}
                }
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
                reg_count: 2,
            },
        )
        .unwrap();
        let p = pb.build_checked().unwrap();

        let mut vm = Vm::new(TestHost, Limits::default());
        let mut trace = CollectingTrace {
            starts: 0,
            ends: 0,
            instrs: Vec::new(),
        };
        let out = vm.run_traced(&p, FuncId(0), &[], &mut trace).unwrap();
        assert_eq!(out, vec![Value::I64(7)]);
        assert_eq!(trace.starts, 1);
        assert_eq!(trace.ends, 1);
        assert_eq!(trace.instrs, vec![0x12, 0x51]);
    }

    #[test]
    fn vm_trace_scopes_fire_for_call_frames_and_host_calls() {
        struct ScopeTrace {
            events: Vec<ScopeKind>,
        }

        impl TraceSink for ScopeTrace {
            fn mask(&self) -> TraceMask {
                TraceMask::CALL | TraceMask::HOST
            }

            fn event(&mut self, _program: &Program, event: TraceEvent<'_>) {
                match event {
                    TraceEvent::ScopeEnter { kind, .. } | TraceEvent::ScopeExit { kind, .. } => {
                        self.events.push(kind);
                    }
                    _ => {}
                }
            }
        }

        let sig = HostSig {
            args: vec![ValueType::I64],
            rets: vec![ValueType::I64],
        };

        let mut pb = ProgramBuilder::new();
        let host_sig = pb.host_sig_for("id", sig.clone());

        let f0 = pb.declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
            reg_count: 2,
        });
        let f1 = pb.declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
            reg_count: 3,
        });

        let mut a1 = Asm::new();
        a1.const_i64(1, 9);
        a1.host_call(0, host_sig, 0, &[1], &[2]);
        a1.ret(0, &[2]);
        pb.define_function(f1, a1).unwrap();

        let mut a0 = Asm::new();
        a0.call(0, f1, 0, &[], &[1]);
        a0.ret(0, &[1]);
        pb.define_function(f0, a0).unwrap();

        let p = pb.build_checked().unwrap();
        let mut vm = Vm::new(TestHost, Limits::default());
        let mut trace = ScopeTrace { events: Vec::new() };
        let out = vm.run_traced(&p, f0, &[], &mut trace).unwrap();
        assert_eq!(out, vec![Value::I64(9)]);

        let f0_kind = ScopeKind::CallFrame { func: f0 };
        let f1_kind = ScopeKind::CallFrame { func: f1 };
        let host_kind = ScopeKind::HostCall {
            host_sig,
            symbol: SymbolId(0),
            sig_hash: sig_hash(&sig),
        };
        assert_eq!(
            trace.events,
            vec![f0_kind, f1_kind, host_kind, host_kind, f1_kind, f0_kind]
        );
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
                reg_count: 3,
            },
        )
        .unwrap();
        let p = pb.build_verified().unwrap();

        let mut vm = Vm::new(TestHost, Limits::default());
        let out = vm.run_verified(&p, FuncId(0), &[]).unwrap();
        assert_eq!(out, vec![Value::I64(9)]);
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
                reg_count: 3,
            },
        )
        .unwrap();
        let p = pb.build_checked().unwrap();

        let mut vm = Vm::new(TestHost, Limits::default());
        let out = vm.run(&p, FuncId(0), &[]).unwrap();
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
                reg_count: 4,
            },
        )
        .unwrap();
        let p = pb.build_checked().unwrap();

        let mut vm = Vm::new(TestHost, Limits::default());
        let out = vm.run(&p, FuncId(0), &[]).unwrap();
        assert_eq!(out, vec![Value::I64(16)]);
    }

    #[test]
    fn vm_tuple_new_get() {
        let mut a = Asm::new();
        a.const_bool(1, true);
        a.tuple_new(2, &[1]);
        a.tuple_get(3, 2, 0);
        a.ret(0, &[3]);

        let mut pb = ProgramBuilder::new();
        pb.push_function_checked(
            a,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::Any],
                reg_count: 4,
            },
        )
        .unwrap();
        let p = pb.build_checked().unwrap();

        let mut vm = Vm::new(TestHost, Limits::default());
        let out = vm.run(&p, FuncId(0), &[]).unwrap();
        assert_eq!(out, vec![Value::Bool(true)]);
    }

    #[test]
    fn vm_struct_new_get() {
        let mut pb = ProgramBuilder::new();
        let st = pb.struct_type(StructTypeDef {
            field_names: vec!["a".into(), "b".into()],
            field_types: vec![ValueType::I64, ValueType::Bool],
        });

        let mut a = Asm::new();
        a.const_i64(1, 5);
        a.const_bool(2, true);
        a.struct_new(3, st, &[1, 2]);
        a.struct_get(4, 3, 1);
        a.ret(0, &[4]);

        pb.push_function_checked(
            a,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::Any],
                reg_count: 5,
            },
        )
        .unwrap();
        let p = pb.build_checked().unwrap();

        let mut vm = Vm::new(TestHost, Limits::default());
        let out = vm.run(&p, FuncId(0), &[]).unwrap();
        assert_eq!(out, vec![Value::Bool(true)]);
    }

    #[test]
    fn vm_array_new_len_get() {
        let mut pb = ProgramBuilder::new();
        let elem = pb.array_elem(ValueType::U64);

        let mut a = Asm::new();
        a.const_u64(1, 7);
        a.const_u64(2, 8);
        a.const_u64(3, 1);
        a.array_new(4, elem, &[1, 2]);
        a.array_len(5, 4);
        a.array_get(6, 4, 3);
        a.ret(0, &[5, 6]);

        pb.push_function_checked(
            a,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::U64, ValueType::Any],
                reg_count: 7,
            },
        )
        .unwrap();
        let p = pb.build_checked().unwrap();

        let mut vm = Vm::new(TestHost, Limits::default());
        let out = vm.run(&p, FuncId(0), &[]).unwrap();
        assert_eq!(out, vec![Value::U64(2), Value::U64(8)]);
    }

    #[test]
    fn vm_fuel_is_enforced() {
        // An infinite loop: jmp 0
        let mut a = Asm::new();
        let l0 = a.label();
        a.place(l0).unwrap();
        a.jmp(l0);
        let mut pb = ProgramBuilder::new();
        pb.push_function_checked(
            a,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![],
                reg_count: 1,
            },
        )
        .unwrap();
        let p = pb.build_checked().unwrap();

        let limits = Limits {
            fuel: 3,
            ..Limits::default()
        };
        let mut vm = Vm::new(TestHost, limits);
        let err = vm.run(&p, FuncId(0), &[]).unwrap_err();
        assert_eq!(err.trap, Trap::FuelExceeded);
        assert_eq!(err.func, FuncId(0));
    }

    #[test]
    fn vm_host_call_limit_is_enforced() {
        // host_call r0, sym0, hash, r0, argc=0, retc=0; jmp 0
        let sig = HostSig {
            args: vec![],
            rets: vec![],
        };

        let mut pb = ProgramBuilder::new();
        let host_sig = pb.host_sig_for("noop", sig.clone());

        let mut a = Asm::new();
        let l0 = a.label();
        a.place(l0).unwrap();
        a.host_call(0, host_sig, 0, &[], &[]);
        a.jmp(l0);
        pb.push_function_checked(
            a,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![],
                reg_count: 1,
            },
        )
        .unwrap();
        let p = pb.build_checked().unwrap();

        let limits = Limits {
            fuel: 100,
            max_host_calls: 2,
            ..Limits::default()
        };
        let mut vm = Vm::new(CountingHost { calls: 0 }, limits);
        let err = vm.run(&p, FuncId(0), &[]).unwrap_err();
        assert_eq!(err.trap, Trap::HostCallLimitExceeded);
    }

    #[test]
    fn vm_span_id_is_reported() {
        // const_unit r0; trap 1
        let mut a = Asm::new();
        a.span(7);
        a.const_unit(0);
        a.span(9);
        a.trap(1);
        let parts = a
            .finish_checked_parts(&FunctionSig {
                arg_types: vec![],
                ret_types: vec![],
                reg_count: 1,
            })
            .unwrap();
        let p = Program::new(
            vec![],
            vec![],
            vec![],
            TypeTableDef::default(),
            vec![FunctionDef {
                arg_types: vec![],
                ret_types: vec![],
                reg_count: 1,
                bytecode: parts.bytecode,
                spans: parts.spans,
            }],
        );

        let mut vm = Vm::new(TestHost, Limits::default());
        let err = vm.run(&p, FuncId(0), &[]).unwrap_err();
        assert_eq!(err.trap, Trap::TrapCode(1));
        assert_eq!(err.span_id, Some(9));
    }

    #[test]
    fn vm_run_verified_validates_host_return_types() {
        struct BadHost;

        impl Host for BadHost {
            fn call(
                &mut self,
                symbol: &str,
                _sig_hash: SigHash,
                _args: &[Value],
            ) -> Result<(Vec<Value>, u64), HostError> {
                match symbol {
                    "bad" => Ok((vec![Value::Bool(true)], 0)),
                    _ => Err(HostError::UnknownSymbol),
                }
            }
        }

        let sig = HostSig {
            args: vec![],
            rets: vec![ValueType::I64],
        };

        let mut pb = ProgramBuilder::new();
        let host_sig = pb.host_sig_for("bad", sig);

        let mut a = Asm::new();
        a.host_call(0, host_sig, 0, &[], &[1]);
        a.ret(0, &[1]);
        pb.push_function_checked(
            a,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
                reg_count: 2,
            },
        )
        .unwrap();
        let p = pb.build_verified().unwrap();

        let mut vm = Vm::new(BadHost, Limits::default());
        let err = vm.run_verified(&p, FuncId(0), &[]).unwrap_err();
        assert_eq!(
            err.trap,
            Trap::TypeMismatch {
                expected: ValueType::I64,
                actual: ValueType::Bool
            }
        );
    }
}
