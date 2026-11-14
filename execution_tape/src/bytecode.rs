// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Bytecode decoding for `execution_tape` (draft).
//!
//! This module defines a minimal opcode set and a decoder that parses an instruction stream into
//! typed instructions with byte offsets. The encoding is currently draft and will evolve alongside
//! the verifier and interpreter.

use alloc::vec::Vec;
use core::fmt;

use crate::format::{DecodeError, Reader};
use crate::opcode::{Opcode, OpcodePrefix};
use crate::program::{ConstId, HostSigId};
use crate::program::{ElemTypeId, TypeId};
use crate::register_operands::OperandIter;
use crate::value::FuncId;

use crate::value::Decimal;

/// A bytecode decoding error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BytecodeError {
    /// The byte stream was malformed.
    Decode(DecodeError),
    /// The opcode is not assigned.
    UnknownOpcode {
        /// Byte offset of the opcode.
        offset: u32,
        /// The unassigned opcode code (see [`Opcode::code`]).
        opcode: u16,
    },
}

impl From<DecodeError> for BytecodeError {
    fn from(e: DecodeError) -> Self {
        Self::Decode(e)
    }
}

/// A bytecode encoding error.
#[allow(
    dead_code,
    reason = "used by generated encoder; will be part of codec API"
)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum EncodeError {
    /// Attempted to encode a value that does not fit the bytecode constraints.
    OutOfBounds,
    /// A `reg_list` count field did not match the actual list length.
    RegListCountMismatch {
        /// The opcode being encoded.
        opcode: Opcode,
        /// The `reg_list` field name.
        field: &'static str,
        /// The count field value.
        count: u32,
        /// The actual `reg_list` length.
        actual: usize,
    },
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfBounds => write!(f, "out of bounds"),
            Self::RegListCountMismatch {
                opcode,
                field,
                count,
                actual,
            } => write!(
                f,
                "reg list count mismatch for {opcode:?}.{field}: count={count}, actual={actual}"
            ),
        }
    }
}

/// A decoded instruction with its byte offset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DecodedInstr {
    /// Byte offset within the function bytecode stream.
    pub offset: u32,
    /// Opcode.
    pub opcode: Opcode,
    /// The instruction.
    pub instr: Instr,
}

/// Instruction set (draft).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Instr {
    /// No-op.
    Nop,
    /// Typed block parameter declaration, erased during preparation.
    Block { params: Vec<u32> },
    /// Jump carrying block arguments.
    JmpArgs { pc_target: u32, args: Vec<u32> },
    /// Branch carrying a combined argument list split at `true_arg_count`.
    BrArgs {
        cond: u32,
        pc_true: u32,
        pc_false: u32,
        true_arg_count: u32,
        args: Vec<u32>,
    },
    /// `dst = src`.
    Mov { dst: u32, src: u32 },
    /// Trap unconditionally with a trap code.
    Trap { code: u32 },
    /// Raise exception `tag` with payload registers `args`.
    Throw {
        eff_in: u32,
        tag: u32,
        args: Vec<u32>,
    },
    /// Rethrow the exception referenced by `exn`.
    ThrowRef { eff_in: u32, exn: u32 },
    /// Create an unstarted continuation of the function or closure in `func`.
    ContNew { dst: u32, call_sig: u32, func: u32 },
    /// Run continuation `cont` with `args` until it returns `rets` or suspends to a handler.
    Resume {
        eff_out: u32,
        call_sig: u32,
        cont: u32,
        eff_in: u32,
        args: Vec<u32>,
        rets: Vec<u32>,
    },
    /// Suspend to the nearest handler of `tag` with payload `args`; resumption supplies `rets`.
    Suspend {
        eff_out: u32,
        tag: u32,
        eff_in: u32,
        args: Vec<u32>,
        rets: Vec<u32>,
    },
    /// Resume `cont` by raising exception `tag` with payload `args` at its suspension point.
    ResumeThrow {
        eff_out: u32,
        call_sig: u32,
        cont: u32,
        eff_in: u32,
        tag: u32,
        args: Vec<u32>,
        rets: Vec<u32>,
    },
    /// Bind the leading inputs of `cont` to `args`, retyping it from `from_sig` to `to_sig`.
    ContBind {
        dst: u32,
        from_sig: u32,
        to_sig: u32,
        cont: u32,
        args: Vec<u32>,
    },
    /// Release `cont` without resuming it.
    ContDrop { cont: u32 },

    /// `dst = ()`.
    ConstUnit { dst: u32 },
    /// `dst = bool`.
    ConstBool { dst: u32, imm: bool },
    /// `dst = i64`.
    ConstI64 { dst: u32, imm: i64 },
    /// `dst = u64`.
    ConstU64 { dst: u32, imm: u64 },
    /// `dst = f64` encoded as raw IEEE bits.
    ConstF64 { dst: u32, bits: u64 },
    /// `dst = Decimal { mantissa, scale }`.
    ConstDecimal { dst: u32, value: Decimal },
    /// `dst = const_pool[idx]`.
    ConstPool { dst: u32, idx: ConstId },
    /// `dst = func#func_id`.
    ConstFunc { dst: u32, func_id: FuncId },

    /// `dst = a + b` ([`Decimal`]).
    DecAdd { dst: u32, a: u32, b: u32 },
    /// `dst = a - b` ([`Decimal`]).
    DecSub { dst: u32, a: u32, b: u32 },
    /// `dst = a * b` ([`Decimal`]).
    DecMul { dst: u32, a: u32, b: u32 },
    /// `dst = a / b` ([`Decimal`] at the `u64` scale in register `scale`, rounded by the
    /// `rounding` immediate; traps on a zero divisor or overflow).
    DecDiv {
        dst: u32,
        a: u32,
        b: u32,
        scale: u32,
        rounding: u8,
    },
    /// `dst = a` rescaled ([`Decimal`] at the `u64` scale in register `scale`; exact when
    /// widening, rounded by the `rounding` immediate when narrowing; traps on overflow).
    DecRescale {
        dst: u32,
        a: u32,
        scale: u32,
        rounding: u8,
    },
    /// `dst = a.scale` ([`Decimal`] to `u64`).
    DecScale { dst: u32, a: u32 },
    /// `dst = cmp(a, b)` ([`Decimal`] to `i64`: -1, 0 or 1 by numeric value, ignoring scale).
    DecCmp { dst: u32, a: u32, b: u32 },

    /// `dst = a + b` (`f64`).
    F64Add { dst: u32, a: u32, b: u32 },
    /// `dst = a - b` (`f64`).
    F64Sub { dst: u32, a: u32, b: u32 },
    /// `dst = a * b` (`f64`).
    F64Mul { dst: u32, a: u32, b: u32 },
    /// `dst = a / b` (`f64`).
    F64Div { dst: u32, a: u32, b: u32 },
    /// `dst = -a` (`f64`).
    F64Neg { dst: u32, a: u32 },
    /// `dst = abs(a)` (`f64`).
    F64Abs { dst: u32, a: u32 },
    /// `dst = min(a, b)` (`f64`, NaN-propagating).
    F64Min { dst: u32, a: u32, b: u32 },
    /// `dst = max(a, b)` (`f64`, NaN-propagating).
    F64Max { dst: u32, a: u32, b: u32 },
    /// `dst = min_num(a, b)` (`f64`, number-favoring).
    F64MinNum { dst: u32, a: u32, b: u32 },
    /// `dst = max_num(a, b)` (`f64`, number-favoring).
    F64MaxNum { dst: u32, a: u32, b: u32 },
    /// `dst = a % b` (`f64`).
    F64Rem { dst: u32, a: u32, b: u32 },
    /// `dst = f64_to_bits(a)` (`f64` -> `u64`).
    F64ToBits { dst: u32, a: u32 },
    /// `dst = f64_from_bits(a)` (`u64` -> `f64`).
    F64FromBits { dst: u32, a: u32 },
    /// `dst = copysign(a, b)` (`f64`, magnitude of `a` with the sign of `b`).
    F64CopySign { dst: u32, a: u32, b: u32 },
    /// `dst = floor(a)` (`f64`, round toward negative infinity).
    F64Floor { dst: u32, a: u32 },
    /// `dst = ceil(a)` (`f64`, round toward positive infinity).
    F64Ceil { dst: u32, a: u32 },
    /// `dst = trunc(a)` (`f64`, round toward zero).
    F64Trunc { dst: u32, a: u32 },
    /// `dst = nearest(a)` (`f64`, round to nearest integer, ties to even).
    F64Nearest { dst: u32, a: u32 },
    /// `dst = sqrt(a)` (`f64`, correctly rounded).
    F64Sqrt { dst: u32, a: u32 },
    /// `dst = round_half_away(a)` (`f64`, round to nearest integer, ties away from zero).
    F64RoundHalfAway { dst: u32, a: u32 },
    /// `dst = clamp(a, lo, hi)` (`f64`; traps unless `lo <= hi`).
    F64Clamp { dst: u32, a: u32, lo: u32, hi: u32 },
    /// `dst = pow(a, b)` (`f64`, pinned `libm::pow`).
    F64Pow { dst: u32, a: u32, b: u32 },
    /// `dst = exp(a)` (`f64`, pinned `libm::exp`).
    F64Exp { dst: u32, a: u32 },
    /// `dst = ln(a)` (`f64`, pinned `libm::log`).
    F64Ln { dst: u32, a: u32 },
    /// `dst = sin(a)` (`f64`, pinned `libm::sin`).
    F64Sin { dst: u32, a: u32 },
    /// `dst = cos(a)` (`f64`, pinned `libm::cos`).
    F64Cos { dst: u32, a: u32 },
    /// `dst = atan2(y, x)` (`f64`, pinned `libm::atan2`).
    F64Atan2 { dst: u32, y: u32, x: u32 },

    /// `dst = a + b` (`i64`).
    I64Add { dst: u32, a: u32, b: u32 },
    /// `dst = a - b` (`i64`).
    I64Sub { dst: u32, a: u32, b: u32 },
    /// `dst = a * b` (`i64`).
    I64Mul { dst: u32, a: u32, b: u32 },
    /// `(dst, overflow) = a + b` (`i64` wrapped sum; `bool` true if the signed sum overflowed).
    I64AddOvf {
        dst: u32,
        overflow: u32,
        a: u32,
        b: u32,
    },
    /// `(dst, overflow) = a - b` (`i64` wrapped difference; `bool` true if the signed difference overflowed).
    I64SubOvf {
        dst: u32,
        overflow: u32,
        a: u32,
        b: u32,
    },
    /// `(dst, overflow) = a * b` (`i64` wrapped product; `bool` true if the signed product overflowed).
    I64MulOvf {
        dst: u32,
        overflow: u32,
        a: u32,
        b: u32,
    },

    /// `dst = a + b` (`u64`).
    U64Add { dst: u32, a: u32, b: u32 },
    /// `dst = a - b` (`u64`).
    U64Sub { dst: u32, a: u32, b: u32 },
    /// `dst = a * b` (`u64`).
    U64Mul { dst: u32, a: u32, b: u32 },
    /// `(dst, overflow) = a + b` (`u64` wrapped sum; `bool` true if the sum carried out).
    U64AddOvf {
        dst: u32,
        overflow: u32,
        a: u32,
        b: u32,
    },
    /// `(dst, overflow) = a - b` (`u64` wrapped difference; `bool` true if the difference borrowed).
    U64SubOvf {
        dst: u32,
        overflow: u32,
        a: u32,
        b: u32,
    },
    /// `(dst, overflow) = a * b` (`u64` wrapped product; `bool` true if the product overflowed).
    U64MulOvf {
        dst: u32,
        overflow: u32,
        a: u32,
        b: u32,
    },
    /// `dst = a & b` (`u64`).
    U64And { dst: u32, a: u32, b: u32 },
    /// `dst = a | b` (`u64`).
    U64Or { dst: u32, a: u32, b: u32 },
    /// `dst = a ^ b` (`u64`).
    U64Xor { dst: u32, a: u32, b: u32 },
    /// `dst = a << (b & 63)` (`u64`).
    U64Shl { dst: u32, a: u32, b: u32 },
    /// `dst = a >> (b & 63)` (`u64`).
    U64Shr { dst: u32, a: u32, b: u32 },
    /// `dst = a.leading_zeros()` (`u64`; 64 when `a == 0`).
    U64Clz { dst: u32, a: u32 },
    /// `dst = a.trailing_zeros()` (`u64`; 64 when `a == 0`).
    U64Ctz { dst: u32, a: u32 },
    /// `dst = a.count_ones()` (`u64`).
    U64Popcnt { dst: u32, a: u32 },
    /// `dst = a.rotate_left(b & 63)` (`u64`).
    U64Rotl { dst: u32, a: u32, b: u32 },
    /// `dst = a.rotate_right(b & 63)` (`u64`).
    U64Rotr { dst: u32, a: u32, b: u32 },
    /// `dst = i32`.
    ConstI32 { dst: u32, imm: i32 },
    /// `dst = u32`.
    ConstU32 { dst: u32, imm: u32 },
    /// `dst = f32` encoded as raw IEEE bits.
    ConstF32 { dst: u32, bits: u32 },
    /// `dst = a + b` (`i32`, wrapping).
    I32Add { dst: u32, a: u32, b: u32 },
    /// `dst = a - b` (`i32`, wrapping).
    I32Sub { dst: u32, a: u32, b: u32 },
    /// `dst = a * b` (`i32`, wrapping).
    I32Mul { dst: u32, a: u32, b: u32 },
    /// `dst = a / b` (`i32`, truncating; traps on zero divisor or `MIN / -1`).
    I32Div { dst: u32, a: u32, b: u32 },
    /// `dst = a % b` (`i32`, sign of `a`; traps on zero divisor or `MIN % -1`).
    I32Rem { dst: u32, a: u32, b: u32 },
    /// `(dst, overflow) = a + b` (`i32` wrapped sum; `bool` true if the signed sum overflowed).
    I32AddOvf {
        dst: u32,
        overflow: u32,
        a: u32,
        b: u32,
    },
    /// `(dst, overflow) = a - b` (`i32` wrapped difference; `bool` true on signed overflow).
    I32SubOvf {
        dst: u32,
        overflow: u32,
        a: u32,
        b: u32,
    },
    /// `(dst, overflow) = a * b` (`i32` wrapped product; `bool` true on signed overflow).
    I32MulOvf {
        dst: u32,
        overflow: u32,
        a: u32,
        b: u32,
    },
    /// `dst = a & b` (`i32`).
    I32And { dst: u32, a: u32, b: u32 },
    /// `dst = a | b` (`i32`).
    I32Or { dst: u32, a: u32, b: u32 },
    /// `dst = a ^ b` (`i32`).
    I32Xor { dst: u32, a: u32, b: u32 },
    /// `dst = a << (b & 31)` (`i32`).
    I32Shl { dst: u32, a: u32, b: u32 },
    /// `dst = a >> (b & 31)` (`i32`, arithmetic).
    I32Shr { dst: u32, a: u32, b: u32 },
    /// `dst = (a == b)` (`i32` -> `bool`).
    I32Eq { dst: u32, a: u32, b: u32 },
    /// `dst = (a < b)` (`i32` -> `bool`).
    I32Lt { dst: u32, a: u32, b: u32 },
    /// `dst = (a > b)` (`i32` -> `bool`).
    I32Gt { dst: u32, a: u32, b: u32 },
    /// `dst = (a <= b)` (`i32` -> `bool`).
    I32Le { dst: u32, a: u32, b: u32 },
    /// `dst = (a >= b)` (`i32` -> `bool`).
    I32Ge { dst: u32, a: u32, b: u32 },
    /// `dst = a + b` (`u32`, wrapping).
    U32Add { dst: u32, a: u32, b: u32 },
    /// `dst = a - b` (`u32`, wrapping).
    U32Sub { dst: u32, a: u32, b: u32 },
    /// `dst = a * b` (`u32`, wrapping).
    U32Mul { dst: u32, a: u32, b: u32 },
    /// `dst = a / b` (`u32`; traps on zero divisor).
    U32Div { dst: u32, a: u32, b: u32 },
    /// `dst = a % b` (`u32`; traps on zero divisor).
    U32Rem { dst: u32, a: u32, b: u32 },
    /// `(dst, overflow) = a + b` (`u32` wrapped sum; `bool` carry).
    U32AddOvf {
        dst: u32,
        overflow: u32,
        a: u32,
        b: u32,
    },
    /// `(dst, overflow) = a - b` (`u32` wrapped difference; `bool` borrow).
    U32SubOvf {
        dst: u32,
        overflow: u32,
        a: u32,
        b: u32,
    },
    /// `(dst, overflow) = a * b` (`u32` wrapped product; `bool` true on overflow).
    U32MulOvf {
        dst: u32,
        overflow: u32,
        a: u32,
        b: u32,
    },
    /// `dst = a & b` (`u32`).
    U32And { dst: u32, a: u32, b: u32 },
    /// `dst = a | b` (`u32`).
    U32Or { dst: u32, a: u32, b: u32 },
    /// `dst = a ^ b` (`u32`).
    U32Xor { dst: u32, a: u32, b: u32 },
    /// `dst = a << (b & 31)` (`u32`).
    U32Shl { dst: u32, a: u32, b: u32 },
    /// `dst = a >> (b & 31)` (`u32`, logical).
    U32Shr { dst: u32, a: u32, b: u32 },
    /// `dst = a.leading_zeros()` (`u32`; 32 when `a == 0`).
    U32Clz { dst: u32, a: u32 },
    /// `dst = a.trailing_zeros()` (`u32`; 32 when `a == 0`).
    U32Ctz { dst: u32, a: u32 },
    /// `dst = a.count_ones()` (`u32`).
    U32Popcnt { dst: u32, a: u32 },
    /// `dst = a.rotate_left(b & 31)` (`u32`).
    U32Rotl { dst: u32, a: u32, b: u32 },
    /// `dst = a.rotate_right(b & 31)` (`u32`).
    U32Rotr { dst: u32, a: u32, b: u32 },
    /// `dst = (a == b)` (`u32` -> `bool`).
    U32Eq { dst: u32, a: u32, b: u32 },
    /// `dst = (a < b)` (`u32` -> `bool`).
    U32Lt { dst: u32, a: u32, b: u32 },
    /// `dst = (a > b)` (`u32` -> `bool`).
    U32Gt { dst: u32, a: u32, b: u32 },
    /// `dst = (a <= b)` (`u32` -> `bool`).
    U32Le { dst: u32, a: u32, b: u32 },
    /// `dst = (a >= b)` (`u32` -> `bool`).
    U32Ge { dst: u32, a: u32, b: u32 },
    /// `dst = a + b` (`f32`).
    F32Add { dst: u32, a: u32, b: u32 },
    /// `dst = a - b` (`f32`).
    F32Sub { dst: u32, a: u32, b: u32 },
    /// `dst = a * b` (`f32`).
    F32Mul { dst: u32, a: u32, b: u32 },
    /// `dst = a / b` (`f32`).
    F32Div { dst: u32, a: u32, b: u32 },
    /// `dst = a % b` (`f32`, sign of `a`).
    F32Rem { dst: u32, a: u32, b: u32 },
    /// `dst = -a` (`f32`, flips the sign bit).
    F32Neg { dst: u32, a: u32 },
    /// `dst = abs(a)` (`f32`, clears the sign bit).
    F32Abs { dst: u32, a: u32 },
    /// `dst = min(a, b)` (`f32`, NaN-propagating).
    F32Min { dst: u32, a: u32, b: u32 },
    /// `dst = max(a, b)` (`f32`, NaN-propagating).
    F32Max { dst: u32, a: u32, b: u32 },
    /// `dst = min_num(a, b)` (`f32`, number-favoring).
    F32MinNum { dst: u32, a: u32, b: u32 },
    /// `dst = max_num(a, b)` (`f32`, number-favoring).
    F32MaxNum { dst: u32, a: u32, b: u32 },
    /// `dst = copysign(a, b)` (`f32`: magnitude of `a`, sign of `b`).
    F32CopySign { dst: u32, a: u32, b: u32 },
    /// `dst = floor(a)` (`f32`, rounds toward negative infinity).
    F32Floor { dst: u32, a: u32 },
    /// `dst = ceil(a)` (`f32`, rounds toward positive infinity).
    F32Ceil { dst: u32, a: u32 },
    /// `dst = trunc(a)` (`f32`, rounds toward zero).
    F32Trunc { dst: u32, a: u32 },
    /// `dst = nearest(a)` (`f32`, rounds to nearest integer, ties to even).
    F32Nearest { dst: u32, a: u32 },
    /// `dst = sqrt(a)` (`f32`, correctly rounded).
    F32Sqrt { dst: u32, a: u32 },
    /// `dst = (a == b)` (`f32` -> `bool`; false if NaN).
    F32Eq { dst: u32, a: u32, b: u32 },
    /// `dst = (a != b)` (`f32` -> `bool`; true if NaN).
    F32Ne { dst: u32, a: u32, b: u32 },
    /// `dst = (a < b)` (`f32` -> `bool`; false if NaN).
    F32Lt { dst: u32, a: u32, b: u32 },
    /// `dst = (a > b)` (`f32` -> `bool`; false if NaN).
    F32Gt { dst: u32, a: u32, b: u32 },
    /// `dst = (a <= b)` (`f32` -> `bool`; false if NaN).
    F32Le { dst: u32, a: u32, b: u32 },
    /// `dst = (a >= b)` (`f32` -> `bool`; false if NaN).
    F32Ge { dst: u32, a: u32, b: u32 },
    /// `dst = round_half_away(a)` (`f32`, round to nearest integer, ties away from zero).
    F32RoundHalfAway { dst: u32, a: u32 },
    /// `dst = clamp(a, lo, hi)` (`f32`; traps unless `lo <= hi`).
    F32Clamp { dst: u32, a: u32, lo: u32, hi: u32 },
    /// `dst = pow(a, b)` (`f32`, pinned `libm::powf`).
    F32Pow { dst: u32, a: u32, b: u32 },
    /// `dst = exp(a)` (`f32`, pinned `libm::expf`).
    F32Exp { dst: u32, a: u32 },
    /// `dst = ln(a)` (`f32`, pinned `libm::logf`).
    F32Ln { dst: u32, a: u32 },
    /// `dst = sin(a)` (`f32`, pinned `libm::sinf`).
    F32Sin { dst: u32, a: u32 },
    /// `dst = cos(a)` (`f32`, pinned `libm::cosf`).
    F32Cos { dst: u32, a: u32 },
    /// `dst = atan2(y, x)` (`f32`, pinned `libm::atan2f`).
    F32Atan2 { dst: u32, y: u32, x: u32 },
    /// `dst = i32_to_i64(a)` (`i32` -> `i64`; sign-extends).
    I32ToI64 { dst: u32, a: u32 },
    /// `dst = i64_to_i32(a)` (`i64` -> `i32`; traps when out of range).
    I64ToI32 { dst: u32, a: u32 },
    /// `dst = i64_to_i32_wrap(a)` (`i64` -> `i32`; keeps the low 32 bits).
    I64ToI32Wrap { dst: u32, a: u32 },
    /// `dst = u32_to_u64(a)` (`u32` -> `u64`; zero-extends).
    U32ToU64 { dst: u32, a: u32 },
    /// `dst = u64_to_u32(a)` (`u64` -> `u32`; traps when out of range).
    U64ToU32 { dst: u32, a: u32 },
    /// `dst = u64_to_u32_wrap(a)` (`u64` -> `u32`; keeps the low 32 bits).
    U64ToU32Wrap { dst: u32, a: u32 },
    /// `dst = u32_to_i64(a)` (`u32` -> `i64`; zero-extends).
    U32ToI64 { dst: u32, a: u32 },
    /// `dst = i32_to_u32(a)` (`i32` -> `u32`; traps on negative).
    I32ToU32 { dst: u32, a: u32 },
    /// `dst = u32_to_i32(a)` (`u32` -> `i32`; traps above `i32::MAX`).
    U32ToI32 { dst: u32, a: u32 },
    /// `dst = i32_to_u32_wrap(a)` (`i32` -> `u32`; reinterprets the bits).
    I32ToU32Wrap { dst: u32, a: u32 },
    /// `dst = u32_to_i32_wrap(a)` (`u32` -> `i32`; reinterprets the bits).
    U32ToI32Wrap { dst: u32, a: u32 },
    /// `dst = i64_to_u64_wrap(a)` (`i64` -> `u64`; reinterprets the bits).
    I64ToU64Wrap { dst: u32, a: u32 },
    /// `dst = u64_to_i64_wrap(a)` (`u64` -> `i64`; reinterprets the bits).
    U64ToI64Wrap { dst: u32, a: u32 },
    /// `dst = f32_to_f64(a)` (`f32` -> `f64`; exact).
    F32ToF64 { dst: u32, a: u32 },
    /// `dst = f64_to_f32(a)` (`f64` -> `f32`; rounds to nearest, ties to even; overflows to infinity).
    F64ToF32 { dst: u32, a: u32 },
    /// `dst = f32_to_bits(a)` (`f32` -> `u32`; raw IEEE bits).
    F32ToBits { dst: u32, a: u32 },
    /// `dst = f32_from_bits(a)` (`u32` -> `f32`; raw IEEE bits).
    F32FromBits { dst: u32, a: u32 },
    /// `dst = f32_to_i32(a)` (`f32` -> `i32`; truncates; traps on NaN, infinities and out of range).
    F32ToI32 { dst: u32, a: u32 },
    /// `dst = f32_to_u32(a)` (`f32` -> `u32`; truncates; traps on NaN, infinities and out of range).
    F32ToU32 { dst: u32, a: u32 },
    /// `dst = f32_to_i32_sat(a)` (`f32` -> `i32`; truncates and saturates; NaN -> 0).
    F32ToI32Sat { dst: u32, a: u32 },
    /// `dst = f32_to_u32_sat(a)` (`f32` -> `u32`; truncates and saturates; NaN -> 0).
    F32ToU32Sat { dst: u32, a: u32 },
    /// `dst = f64_to_i32(a)` (`f64` -> `i32`; truncates; traps on NaN, infinities and out of range).
    F64ToI32 { dst: u32, a: u32 },
    /// `dst = f64_to_u32(a)` (`f64` -> `u32`; truncates; traps on NaN, infinities and out of range).
    F64ToU32 { dst: u32, a: u32 },
    /// `dst = f64_to_i32_sat(a)` (`f64` -> `i32`; truncates and saturates; NaN -> 0).
    F64ToI32Sat { dst: u32, a: u32 },
    /// `dst = f64_to_u32_sat(a)` (`f64` -> `u32`; truncates and saturates; NaN -> 0).
    F64ToU32Sat { dst: u32, a: u32 },
    /// `dst = i32_to_f32(a)` (`i32` -> `f32`; rounds to nearest, ties to even).
    I32ToF32 { dst: u32, a: u32 },
    /// `dst = u32_to_f32(a)` (`u32` -> `f32`; rounds to nearest, ties to even).
    U32ToF32 { dst: u32, a: u32 },
    /// `dst = i64_to_f32(a)` (`i64` -> `f32`; rounds once to nearest, ties to even).
    I64ToF32 { dst: u32, a: u32 },
    /// `dst = u64_to_f32(a)` (`u64` -> `f32`; rounds once to nearest, ties to even).
    U64ToF32 { dst: u32, a: u32 },
    /// `dst = i32_to_f64(a)` (`i32` -> `f64`; exact).
    I32ToF64 { dst: u32, a: u32 },
    /// `dst = u32_to_f64(a)` (`u32` -> `f64`; exact).
    U32ToF64 { dst: u32, a: u32 },

    /// `dst = (a == b)` (`i64` -> `bool`).
    I64Eq { dst: u32, a: u32, b: u32 },
    /// `dst = (a < b)` (`i64` -> `bool`).
    I64Lt { dst: u32, a: u32, b: u32 },
    /// `dst = (a == b)` (`u64` -> `bool`).
    U64Eq { dst: u32, a: u32, b: u32 },
    /// `dst = (a < b)` (`u64` -> `bool`).
    U64Lt { dst: u32, a: u32, b: u32 },
    /// `dst = (a > b)` (`u64` -> `bool`).
    U64Gt { dst: u32, a: u32, b: u32 },
    /// `dst = (a <= b)` (`u64` -> `bool`).
    U64Le { dst: u32, a: u32, b: u32 },
    /// `dst = (a >= b)` (`u64` -> `bool`).
    U64Ge { dst: u32, a: u32, b: u32 },

    /// `dst = !a` (`bool`).
    BoolNot { dst: u32, a: u32 },
    /// `dst = a & b` (`bool`).
    BoolAnd { dst: u32, a: u32, b: u32 },
    /// `dst = a | b` (`bool`).
    BoolOr { dst: u32, a: u32, b: u32 },
    /// `dst = a ^ b` (`bool`).
    BoolXor { dst: u32, a: u32, b: u32 },

    /// `dst = a & b` (`i64`).
    I64And { dst: u32, a: u32, b: u32 },
    /// `dst = a | b` (`i64`).
    I64Or { dst: u32, a: u32, b: u32 },
    /// `dst = a ^ b` (`i64`).
    I64Xor { dst: u32, a: u32, b: u32 },
    /// `dst = a << (b & 63)` (`i64`).
    I64Shl { dst: u32, a: u32, b: u32 },
    /// `dst = a >> (b & 63)` (`i64`).
    I64Shr { dst: u32, a: u32, b: u32 },

    /// `dst = (a > b)` (`i64` -> `bool`).
    I64Gt { dst: u32, a: u32, b: u32 },
    /// `dst = (a <= b)` (`i64` -> `bool`).
    I64Le { dst: u32, a: u32, b: u32 },
    /// `dst = (a >= b)` (`i64` -> `bool`).
    I64Ge { dst: u32, a: u32, b: u32 },

    /// `dst = (a == b)` (`f64` -> `bool`).
    F64Eq { dst: u32, a: u32, b: u32 },
    /// `dst = (a < b)` (`f64` -> `bool`).
    F64Lt { dst: u32, a: u32, b: u32 },
    /// `dst = (a > b)` (`f64` -> `bool`).
    F64Gt { dst: u32, a: u32, b: u32 },
    /// `dst = (a <= b)` (`f64` -> `bool`).
    F64Le { dst: u32, a: u32, b: u32 },
    /// `dst = (a >= b)` (`f64` -> `bool`).
    F64Ge { dst: u32, a: u32, b: u32 },
    /// `dst = (a != b)` (`f64` -> `bool`, true if either input is NaN).
    F64Ne { dst: u32, a: u32, b: u32 },

    /// `dst = u64_to_i64(a)` (traps on overflow).
    U64ToI64 { dst: u32, a: u32 },
    /// `dst = i64_to_u64(a)` (traps on negative).
    I64ToU64 { dst: u32, a: u32 },

    /// `dst = if cond { a } else { b }`.
    Select { dst: u32, cond: u32, a: u32, b: u32 },

    /// Branch based on `cond` to `pc_true` or `pc_false` (byte offsets).
    Br {
        cond: u32,
        pc_true: u32,
        pc_false: u32,
    },
    /// Jump to `pc_target` (byte offset).
    Jmp { pc_target: u32 },

    /// Call a function.
    ///
    /// Encoding includes argument and return register lists.
    Call {
        eff_out: u32,
        func_id: FuncId,
        eff_in: u32,
        args: Vec<u32>,
        rets: Vec<u32>,
    },

    /// Return from the current function.
    Ret { eff_in: u32, rets: Vec<u32> },

    /// Call a host function.
    HostCall {
        eff_out: u32,
        host_sig: HostSigId,
        eff_in: u32,
        args: Vec<u32>,
        rets: Vec<u32>,
    },

    /// Indirect call through a callee register with an expected call signature id.
    CallIndirect {
        eff_out: u32,
        call_sig: u32,
        callee: u32,
        eff_in: u32,
        args: Vec<u32>,
        rets: Vec<u32>,
    },

    /// Construct a closure value from `func` and `env`.
    ClosureNew { dst: u32, func: u32, env: u32 },

    /// Allocate a tuple aggregate.
    TupleNew { dst: u32, values: Vec<u32> },
    /// Read tuple element at an immediate index.
    TupleGet { dst: u32, tuple: u32, index: u32 },

    /// Allocate a struct aggregate.
    StructNew {
        dst: u32,
        type_id: TypeId,
        values: Vec<u32>,
    },
    /// Read struct field at an immediate index.
    StructGet { dst: u32, st: u32, field_index: u32 },

    /// Allocate an array aggregate.
    ArrayNew {
        dst: u32,
        elem_type_id: ElemTypeId,
        len: u32,
        values: Vec<u32>,
    },
    /// Read array length.
    ArrayLen { dst: u32, arr: u32 },
    /// Read array element at an index register.
    ArrayGet { dst: u32, arr: u32, index: u32 },
    /// Read array element at an immediate index.
    ArrayGetImm { dst: u32, arr: u32, index: u32 },

    /// Read tuple length.
    TupleLen { dst: u32, tuple: u32 },
    /// Read struct field count.
    StructFieldCount { dst: u32, st: u32 },

    /// Read byte-string length.
    BytesLen { dst: u32, bytes: u32 },
    /// Read UTF-8 string length (in bytes).
    StrLen { dst: u32, s: u32 },

    /// `dst = a / b` (`i64`, traps on divide-by-zero and `i64::MIN / -1`).
    I64Div { dst: u32, a: u32, b: u32 },
    /// `dst = a % b` (`i64`, traps on divide-by-zero and `i64::MIN % -1`).
    I64Rem { dst: u32, a: u32, b: u32 },
    /// `dst = a / b` (`u64`, traps on divide-by-zero).
    U64Div { dst: u32, a: u32, b: u32 },
    /// `dst = a % b` (`u64`, traps on divide-by-zero).
    U64Rem { dst: u32, a: u32, b: u32 },

    /// `dst = (a as f64)` (`i64` to `f64`).
    I64ToF64 { dst: u32, a: u32 },
    /// `dst = (a as f64)` (`u64` to `f64`).
    U64ToF64 { dst: u32, a: u32 },
    /// `dst = trunc(a)` (`f64` to `i64`, traps on NaN/inf/out-of-range).
    F64ToI64 { dst: u32, a: u32 },
    /// `dst = trunc(a)` (`f64` to `u64`, traps on NaN/inf/out-of-range; `-0.5` converts to `0`).
    F64ToU64 { dst: u32, a: u32 },
    /// `dst = trunc_sat(a)` (`f64` to `i64`, saturating; NaN -> 0, never traps).
    F64ToI64Sat { dst: u32, a: u32 },
    /// `dst = trunc_sat(a)` (`f64` to `u64`, saturating; NaN -> 0, never traps).
    F64ToU64Sat { dst: u32, a: u32 },

    /// `dst = a.mantissa` ([`Decimal`] to `i64`, traps if `scale != 0` or outside `i64`).
    DecToI64 { dst: u32, a: u32 },
    /// `dst = a.mantissa` ([`Decimal`] to `u64`, traps if `scale != 0` or outside `u64`).
    DecToU64 { dst: u32, a: u32 },
    /// `dst = Decimal { mantissa: a * 10^scale, scale }` (traps on overflow).
    I64ToDec { dst: u32, a: u32, scale: u8 },
    /// `dst = Decimal { mantissa: a * 10^scale, scale }` (traps on overflow).
    U64ToDec { dst: u32, a: u32, scale: u8 },

    /// `dst = (a == b)` (`bytes` -> `bool`).
    BytesEq { dst: u32, a: u32, b: u32 },
    /// `dst = (a == b)` (`str` -> `bool`).
    StrEq { dst: u32, a: u32, b: u32 },
    /// `dst = concat(a, b)` (`bytes`).
    BytesConcat { dst: u32, a: u32, b: u32 },
    /// `dst = concat(a, b)` (`str`).
    StrConcat { dst: u32, a: u32, b: u32 },
    /// `dst = bytes[index]` (returns `u64` in `0..=255`, traps on OOB).
    BytesGet { dst: u32, bytes: u32, index: u32 },
    /// `dst = bytes[index]` (returns `u64` in `0..=255`, traps on OOB).
    BytesGetImm { dst: u32, bytes: u32, index: u32 },
    /// `dst = bytes[start..end]` (traps on invalid range).
    BytesSlice {
        dst: u32,
        bytes: u32,
        start: u32,
        end: u32,
    },
    /// `dst = s[start..end]` (byte indices; traps on invalid range or non-boundaries).
    StrSlice {
        dst: u32,
        s: u32,
        start: u32,
        end: u32,
    },
    /// `dst = s.as_bytes().to_vec()`.
    StrToBytes { dst: u32, s: u32 },
    /// `dst = String::from_utf8(bytes)` (traps on invalid UTF-8).
    BytesToStr { dst: u32, bytes: u32 },
}

// Generated impls for `Instr::reads()`/`Instr::writes()`.
include!("bytecode_instr_gen.rs");
include!("bytecode_reads_writes_gen.rs");

/// Decodes `bytes` into a list of instructions.
pub(crate) fn decode_instructions(bytes: &[u8]) -> Result<Vec<DecodedInstr>, BytecodeError> {
    let mut r = Reader::new(bytes);
    let mut out: Vec<DecodedInstr> = Vec::new();
    while r.offset() < bytes.len() {
        let offset = u32::try_from(r.offset()).map_err(|_| DecodeError::OutOfBounds)?;
        let op = read_opcode(&mut r, offset)?;
        let instr = decode_instr(op, &mut r)?;

        #[cfg(debug_assertions)]
        {
            let schema = instr.operand_schema();
            let op_schema = op.operands();
            debug_assert_eq!(
                op_schema.len(),
                schema.len(),
                "opcode schema drift for {op:?} at pc={offset} (operand count)"
            );

            for (i, operand) in schema.iter().enumerate() {
                debug_assert_eq!(
                    op_schema[i].kind, operand.kind,
                    "opcode schema drift for {op:?} at pc={offset} (operand {i} kind)"
                );
                debug_assert_eq!(
                    op_schema[i].role, operand.role,
                    "opcode schema drift for {op:?} at pc={offset} (operand {i} role)"
                );
                debug_assert_eq!(
                    op_schema[i].encoding, operand.encoding,
                    "opcode schema drift for {op:?} at pc={offset} (operand {i} encoding)"
                );
            }
        }
        out.push(DecodedInstr {
            offset,
            opcode: op,
            instr,
        });
    }
    Ok(out)
}

/// Reads one opcode, located at `offset`, from `r`.
///
/// Single-byte opcodes take one table lookup, as before prefixes existed; only a byte that is not
/// a single-byte opcode is checked for a prefix.
#[inline]
pub(crate) fn read_opcode(r: &mut Reader<'_>, offset: u32) -> Result<Opcode, BytecodeError> {
    let lead = r.read_u8()?;
    if let Some(op) = Opcode::from_u8(lead) {
        return Ok(op);
    }
    let Some(prefix) = OpcodePrefix::from_u8(lead) else {
        return Err(BytecodeError::UnknownOpcode {
            offset,
            opcode: u16::from(lead),
        });
    };
    let byte = r.read_u8()?;
    Opcode::from_prefixed(prefix, byte).ok_or(BytecodeError::UnknownOpcode {
        offset,
        opcode: u16::from_be_bytes([lead, byte]),
    })
}

#[allow(
    dead_code,
    reason = "used by tests; will be used by public assembler/codec API"
)]
pub(crate) fn encode_instructions(instrs: &[Instr]) -> Result<Vec<u8>, EncodeError> {
    let mut out: Vec<u8> = Vec::new();
    for instr in instrs {
        encode_instr(instr, &mut out)?;
    }
    Ok(out)
}

// Generated decoder; single source of truth is `execution_tape/opcodes.json`.
include!("bytecode_decode_gen.rs");

#[allow(
    dead_code,
    reason = "generated; will be used by public assembler/codec API"
)]
mod bytecode_encode_gen {
    use super::*;
    // Generated encoder; single source of truth is `execution_tape/opcodes.json`.
    include!("bytecode_encode_gen.rs");
}

use bytecode_encode_gen::encode_instr;

impl Instr {
    /// Arm 0 is jump/true; arm 1 is false. Distinct arms keep distinct arguments even at equal targets.
    pub(crate) fn edge(&self, arm: usize) -> Option<(u32, &[u32])> {
        match (self, arm) {
            (Self::Jmp { pc_target }, 0) => Some((*pc_target, &[])),
            (Self::JmpArgs { pc_target, args }, 0) => Some((*pc_target, args)),
            (Self::Br { pc_true, .. }, 0) => Some((*pc_true, &[])),
            (Self::Br { pc_false, .. }, 1) => Some((*pc_false, &[])),
            (
                Self::BrArgs {
                    pc_true,
                    true_arg_count,
                    args,
                    ..
                },
                0,
            ) => Some((*pc_true, args.get(..*true_arg_count as usize)?)),
            (
                Self::BrArgs {
                    pc_false,
                    true_arg_count,
                    args,
                    ..
                },
                1,
            ) => Some((*pc_false, args.get(*true_arg_count as usize..)?)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Instr;
    use alloc::vec;
    use alloc::vec::Vec;

    use crate::opcode::Opcode;
    use crate::program::HostSigId;
    use crate::program::{ConstId, ElemTypeId, TypeId};
    use crate::value::FuncId;

    #[test]
    fn opcode_schema_matches_decoded_instr_shape_for_smoke_set() {
        // Chosen to cover all operand kinds, including reg lists and ids.
        let instrs: &[(Opcode, Instr)] = &[
            (Opcode::Nop, Instr::Nop),
            (Opcode::Mov, Instr::Mov { dst: 1, src: 2 }),
            (Opcode::Trap, Instr::Trap { code: 7 }),
            (
                Opcode::Throw,
                Instr::Throw {
                    eff_in: 0,
                    tag: 3,
                    args: vec![1, 2],
                },
            ),
            (Opcode::ConstBool, Instr::ConstBool { dst: 1, imm: true }),
            (Opcode::ConstI64, Instr::ConstI64 { dst: 1, imm: -3 }),
            (Opcode::ConstU64, Instr::ConstU64 { dst: 1, imm: 3 }),
            (Opcode::ConstF64, Instr::ConstF64 { dst: 1, bits: 42 }),
            (
                Opcode::ConstDecimal,
                Instr::ConstDecimal {
                    dst: 1,
                    value: crate::value::Decimal::new(12, 2),
                },
            ),
            (
                Opcode::ConstPool,
                Instr::ConstPool {
                    dst: 1,
                    idx: ConstId(0),
                },
            ),
            (
                Opcode::ConstFunc,
                Instr::ConstFunc {
                    dst: 1,
                    func_id: FuncId(0),
                },
            ),
            (Opcode::I64Add, Instr::I64Add { dst: 1, a: 2, b: 3 }),
            (Opcode::BoolNot, Instr::BoolNot { dst: 1, a: 2 }),
            (
                Opcode::Select,
                Instr::Select {
                    dst: 1,
                    cond: 2,
                    a: 3,
                    b: 4,
                },
            ),
            (
                Opcode::Br,
                Instr::Br {
                    cond: 1,
                    pc_true: 0,
                    pc_false: 10,
                },
            ),
            (Opcode::Jmp, Instr::Jmp { pc_target: 12 }),
            (
                Opcode::Call,
                Instr::Call {
                    eff_out: 0,
                    func_id: FuncId(0),
                    eff_in: 0,
                    args: vec![1, 2],
                    rets: vec![3],
                },
            ),
            (
                Opcode::HostCall,
                Instr::HostCall {
                    eff_out: 0,
                    host_sig: HostSigId(0),
                    eff_in: 0,
                    args: vec![1],
                    rets: vec![2, 3],
                },
            ),
            (
                Opcode::CallIndirect,
                Instr::CallIndirect {
                    eff_out: 0,
                    call_sig: 0,
                    callee: 1,
                    eff_in: 0,
                    args: vec![2],
                    rets: vec![3],
                },
            ),
            (
                Opcode::ClosureNew,
                Instr::ClosureNew {
                    dst: 1,
                    func: 2,
                    env: 3,
                },
            ),
            (
                Opcode::Ret,
                Instr::Ret {
                    eff_in: 0,
                    rets: vec![1],
                },
            ),
            (
                Opcode::TupleNew,
                Instr::TupleNew {
                    dst: 1,
                    values: vec![2, 3],
                },
            ),
            (
                Opcode::TupleGet,
                Instr::TupleGet {
                    dst: 1,
                    tuple: 2,
                    index: 0,
                },
            ),
            (
                Opcode::StructNew,
                Instr::StructNew {
                    dst: 1,
                    type_id: TypeId(0),
                    values: vec![2],
                },
            ),
            (
                Opcode::StructGet,
                Instr::StructGet {
                    dst: 1,
                    st: 2,
                    field_index: 0,
                },
            ),
            (
                Opcode::ArrayNew,
                Instr::ArrayNew {
                    dst: 1,
                    elem_type_id: ElemTypeId(0),
                    len: 2,
                    values: vec![2, 3],
                },
            ),
            (
                Opcode::ArrayGetImm,
                Instr::ArrayGetImm {
                    dst: 1,
                    arr: 2,
                    index: 0,
                },
            ),
            (
                Opcode::BytesGetImm,
                Instr::BytesGetImm {
                    dst: 1,
                    bytes: 2,
                    index: 0,
                },
            ),
        ];

        for (op, instr) in instrs {
            let schema = instr.operand_schema();
            let op_schema = op.operands();
            assert_eq!(
                op_schema.len(),
                schema.len(),
                "opcode operand schema drift for {op:?} (len)"
            );

            for (i, operand) in schema.iter().enumerate() {
                assert_eq!(
                    op_schema[i].kind, operand.kind,
                    "opcode operand schema drift for {op:?} (operand {i} kind)"
                );
                assert_eq!(
                    op_schema[i].role, operand.role,
                    "opcode operand schema drift for {op:?} (operand {i} role)"
                );
                assert_eq!(
                    op_schema[i].encoding, operand.encoding,
                    "opcode encoding schema drift for {op:?} (operand {i} encoding)"
                );
            }
        }
    }

    #[test]
    fn reads_and_writes_for_arithmetic() {
        let i = Instr::I64Add { dst: 3, a: 1, b: 2 };
        assert_eq!(i.reads().collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(i.writes().collect::<Vec<_>>(), vec![3]);
    }

    #[test]
    fn reads_and_writes_for_aggregate_and_call() {
        let tuple = Instr::TupleNew {
            dst: 9,
            values: vec![1, 2, 3],
        };
        assert_eq!(tuple.reads().collect::<Vec<_>>(), vec![1, 2, 3]);
        assert_eq!(tuple.writes().collect::<Vec<_>>(), vec![9]);

        let call = Instr::Call {
            eff_out: 0,
            func_id: FuncId(1),
            eff_in: 0,
            args: vec![4, 5],
            rets: vec![6, 7],
        };
        assert_eq!(call.reads().collect::<Vec<_>>(), vec![0, 4, 5]);
        assert_eq!(call.writes().collect::<Vec<_>>(), vec![0, 6, 7]);

        let host = Instr::HostCall {
            eff_out: 0,
            host_sig: HostSigId(2),
            eff_in: 0,
            args: vec![],
            rets: vec![8],
        };
        assert_eq!(host.reads().collect::<Vec<_>>(), vec![0]);
        assert_eq!(host.writes().collect::<Vec<_>>(), vec![0, 8]);

        let indirect = Instr::CallIndirect {
            eff_out: 0,
            call_sig: 1,
            callee: 4,
            eff_in: 0,
            args: vec![5, 6],
            rets: vec![7],
        };
        assert_eq!(indirect.reads().collect::<Vec<_>>(), vec![4, 0, 5, 6]);
        assert_eq!(indirect.writes().collect::<Vec<_>>(), vec![0, 7]);
    }

    #[test]
    fn reads_and_writes_for_ret() {
        let r = Instr::Ret {
            eff_in: 0,
            rets: vec![1, 2],
        };
        assert_eq!(r.reads().collect::<Vec<_>>(), vec![0, 1, 2]);
        assert_eq!(r.writes().collect::<Vec<_>>(), vec![]);
    }

    #[test]
    fn reads_and_writes_for_const_func() {
        let i = Instr::ConstFunc {
            dst: 3,
            func_id: FuncId(7),
        };
        assert_eq!(i.reads().collect::<Vec<_>>(), vec![]);
        assert_eq!(i.writes().collect::<Vec<_>>(), vec![3]);
    }

    #[test]
    fn encode_decode_roundtrip_for_smoke_set() {
        let smoke: Vec<Instr> = vec![
            Instr::Nop,
            Instr::Trap { code: 42 },
            Instr::Throw {
                eff_in: 0,
                tag: 1,
                args: vec![4],
            },
            Instr::Mov { dst: 1, src: 2 },
            Instr::ConstUnit { dst: 1 },
            Instr::ConstBool { dst: 1, imm: true },
            Instr::ConstI64 { dst: 1, imm: -5 },
            Instr::ConstU64 { dst: 1, imm: 123 },
            Instr::ConstF64 {
                dst: 1,
                bits: 0x3ff0_0000_0000_0000,
            },
            Instr::ConstDecimal {
                dst: 1,
                value: crate::value::Decimal::new(7, 2),
            },
            Instr::ConstPool {
                dst: 1,
                idx: ConstId(0),
            },
            Instr::ConstFunc {
                dst: 2,
                func_id: FuncId(1),
            },
            Instr::Br {
                cond: 1,
                pc_true: 10,
                pc_false: 20,
            },
            Instr::Jmp { pc_target: 10 },
            Instr::Call {
                eff_out: 0,
                func_id: FuncId(1),
                eff_in: 0,
                args: vec![4, 5],
                rets: vec![6, 7],
            },
            Instr::HostCall {
                eff_out: 0,
                host_sig: HostSigId(2),
                eff_in: 0,
                args: vec![1, 2, 3],
                rets: vec![],
            },
            Instr::CallIndirect {
                eff_out: 0,
                call_sig: 0,
                callee: 3,
                eff_in: 0,
                args: vec![1, 2],
                rets: vec![4],
            },
            Instr::ClosureNew {
                dst: 3,
                func: 4,
                env: 5,
            },
            Instr::Ret {
                eff_in: 0,
                rets: vec![1],
            },
            Instr::TupleNew {
                dst: 1,
                values: vec![2, 3],
            },
            Instr::TupleGet {
                dst: 1,
                tuple: 2,
                index: 0,
            },
            Instr::StructNew {
                dst: 1,
                type_id: TypeId(0),
                values: vec![2],
            },
            Instr::StructGet {
                dst: 1,
                st: 2,
                field_index: 0,
            },
            Instr::ArrayNew {
                dst: 1,
                elem_type_id: ElemTypeId(0),
                len: 2,
                values: vec![2, 3],
            },
            Instr::ArrayGetImm {
                dst: 1,
                arr: 2,
                index: 0,
            },
            Instr::BytesGetImm {
                dst: 1,
                bytes: 2,
                index: 0,
            },
        ];

        let bytes = super::encode_instructions(&smoke).expect("encode");
        let decoded = super::decode_instructions(&bytes).expect("decode");
        let roundtripped: Vec<Instr> = decoded.into_iter().map(|di| di.instr).collect();
        assert_eq!(roundtripped, smoke);
    }
}
