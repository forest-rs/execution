// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Bytecode decoding for `execution_tape` (draft).
//!
//! This module defines a minimal opcode set and a decoder that parses an instruction stream into
//! typed instructions with byte offsets. The encoding is currently draft and will evolve alongside
//! the verifier and interpreter.

use alloc::vec::Vec;

use crate::format::{DecodeError, Reader};
use crate::opcode::{Opcode, OperandKind};
use crate::program::{ConstId, HostSigId};
use crate::program::{ElemTypeId, TypeId};
use crate::value::FuncId;

#[cfg(doc)]
use crate::value::Decimal;

/// A bytecode decoding error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BytecodeError {
    /// The byte stream was malformed.
    Decode(DecodeError),
    /// The opcode byte is not recognized.
    UnknownOpcode {
        /// The unrecognized opcode byte.
        opcode: u8,
    },
}

impl From<DecodeError> for BytecodeError {
    fn from(e: DecodeError) -> Self {
        Self::Decode(e)
    }
}

/// A decoded instruction with its byte offset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DecodedInstr {
    /// Byte offset within the function bytecode stream.
    pub offset: u32,
    /// Opcode byte.
    pub opcode: u8,
    /// The instruction.
    pub instr: Instr,
}

/// Instruction set (draft).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Instr {
    /// No-op.
    Nop,
    /// `dst = src`.
    Mov { dst: u32, src: u32 },
    /// Trap unconditionally with a trap code.
    Trap { code: u32 },

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
    ConstDecimal { dst: u32, mantissa: i64, scale: u8 },
    /// `dst = const_pool[idx]`.
    ConstPool { dst: u32, idx: ConstId },

    /// `dst = a + b` ([`Decimal`]).
    DecAdd { dst: u32, a: u32, b: u32 },
    /// `dst = a - b` ([`Decimal`]).
    DecSub { dst: u32, a: u32, b: u32 },
    /// `dst = a * b` ([`Decimal`]).
    DecMul { dst: u32, a: u32, b: u32 },

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

    /// `dst = a + b` (`i64`).
    I64Add { dst: u32, a: u32, b: u32 },
    /// `dst = a - b` (`i64`).
    I64Sub { dst: u32, a: u32, b: u32 },
    /// `dst = a * b` (`i64`).
    I64Mul { dst: u32, a: u32, b: u32 },

    /// `dst = a + b` (`u64`).
    U64Add { dst: u32, a: u32, b: u32 },
    /// `dst = a - b` (`u64`).
    U64Sub { dst: u32, a: u32, b: u32 },
    /// `dst = a * b` (`u64`).
    U64Mul { dst: u32, a: u32, b: u32 },
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
    /// `dst = trunc(a)` (`f64` to `u64`, traps on NaN/inf/out-of-range/negative).
    F64ToU64 { dst: u32, a: u32 },

    /// `dst = a.mantissa` ([`Decimal`] to `i64`, traps if `scale != 0`).
    DecToI64 { dst: u32, a: u32 },
    /// `dst = a.mantissa` ([`Decimal`] to `u64`, traps if `scale != 0` or mantissa < 0).
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

const OPERANDS_NONE: &[OperandKind] = &[];
const OPERANDS_REG: &[OperandKind] = &[OperandKind::Reg];
const OPERANDS_REG_REG: &[OperandKind] = &[OperandKind::Reg, OperandKind::Reg];
const OPERANDS_REG_REG_REG: &[OperandKind] =
    &[OperandKind::Reg, OperandKind::Reg, OperandKind::Reg];
const OPERANDS_REG_REG_REG_REG: &[OperandKind] = &[
    OperandKind::Reg,
    OperandKind::Reg,
    OperandKind::Reg,
    OperandKind::Reg,
];
const OPERANDS_IMM_U32: &[OperandKind] = &[OperandKind::ImmU32];
const OPERANDS_REG_IMM_BOOL: &[OperandKind] = &[OperandKind::Reg, OperandKind::ImmBool];
const OPERANDS_REG_IMM_I64: &[OperandKind] = &[OperandKind::Reg, OperandKind::ImmI64];
const OPERANDS_REG_IMM_U64: &[OperandKind] = &[OperandKind::Reg, OperandKind::ImmU64];
const OPERANDS_REG_IMM_I64_IMM_U8: &[OperandKind] =
    &[OperandKind::Reg, OperandKind::ImmI64, OperandKind::ImmU8];
const OPERANDS_REG_CONST_ID: &[OperandKind] = &[OperandKind::Reg, OperandKind::ConstId];
const OPERANDS_REG_REG_IMM_U32: &[OperandKind] =
    &[OperandKind::Reg, OperandKind::Reg, OperandKind::ImmU32];
const OPERANDS_REG_PC_PC: &[OperandKind] = &[OperandKind::Reg, OperandKind::Pc, OperandKind::Pc];
const OPERANDS_PC: &[OperandKind] = &[OperandKind::Pc];
const OPERANDS_REG_REG_IMM_U8: &[OperandKind] =
    &[OperandKind::Reg, OperandKind::Reg, OperandKind::ImmU8];
const OPERANDS_REG_FUNC_ID_REG_REG_LIST_REG_LIST: &[OperandKind] = &[
    OperandKind::Reg,
    OperandKind::FuncId,
    OperandKind::Reg,
    OperandKind::RegList,
    OperandKind::RegList,
];
const OPERANDS_REG_HOST_SIG_ID_REG_REG_LIST_REG_LIST: &[OperandKind] = &[
    OperandKind::Reg,
    OperandKind::HostSigId,
    OperandKind::Reg,
    OperandKind::RegList,
    OperandKind::RegList,
];
const OPERANDS_REG_REG_LIST: &[OperandKind] = &[OperandKind::Reg, OperandKind::RegList];
const OPERANDS_REG_TYPE_ID_REG_LIST: &[OperandKind] =
    &[OperandKind::Reg, OperandKind::TypeId, OperandKind::RegList];
const OPERANDS_REG_ELEM_TYPE_ID_REG_LIST: &[OperandKind] = &[
    OperandKind::Reg,
    OperandKind::ElemTypeId,
    OperandKind::RegList,
];

impl Instr {
    /// Returns operand kind descriptors for this decoded instruction.
    ///
    /// This is intended for tooling and debug assertions (e.g. checking the opcode JSON schema
    /// stays consistent with the decoder). It is not used to drive execution.
    #[must_use]
    pub(crate) fn operand_kinds(&self) -> &'static [OperandKind] {
        match self {
            Self::Nop => OPERANDS_NONE,
            Self::Mov { .. } => OPERANDS_REG_REG,
            Self::Trap { .. } => OPERANDS_IMM_U32,

            Self::ConstUnit { .. } => OPERANDS_REG,
            Self::ConstBool { .. } => OPERANDS_REG_IMM_BOOL,
            Self::ConstI64 { .. } => OPERANDS_REG_IMM_I64,
            Self::ConstU64 { .. } | Self::ConstF64 { .. } => OPERANDS_REG_IMM_U64,
            Self::ConstDecimal { .. } => OPERANDS_REG_IMM_I64_IMM_U8,
            Self::ConstPool { .. } => OPERANDS_REG_CONST_ID,

            Self::DecAdd { .. }
            | Self::DecSub { .. }
            | Self::DecMul { .. }
            | Self::F64Add { .. }
            | Self::F64Sub { .. }
            | Self::F64Mul { .. }
            | Self::F64Div { .. }
            | Self::F64Min { .. }
            | Self::F64Max { .. }
            | Self::F64MinNum { .. }
            | Self::F64MaxNum { .. }
            | Self::F64Rem { .. }
            | Self::I64Add { .. }
            | Self::I64Sub { .. }
            | Self::I64Mul { .. }
            | Self::U64Add { .. }
            | Self::U64Sub { .. }
            | Self::U64Mul { .. }
            | Self::U64And { .. }
            | Self::U64Or { .. }
            | Self::U64Xor { .. }
            | Self::U64Shl { .. }
            | Self::U64Shr { .. }
            | Self::U64Div { .. }
            | Self::U64Rem { .. }
            | Self::I64Div { .. }
            | Self::I64Rem { .. }
            | Self::I64Eq { .. }
            | Self::I64Lt { .. }
            | Self::I64Gt { .. }
            | Self::I64Le { .. }
            | Self::I64Ge { .. }
            | Self::U64Eq { .. }
            | Self::U64Lt { .. }
            | Self::U64Gt { .. }
            | Self::U64Le { .. }
            | Self::U64Ge { .. }
            | Self::F64Eq { .. }
            | Self::F64Lt { .. }
            | Self::F64Gt { .. }
            | Self::F64Le { .. }
            | Self::F64Ge { .. }
            | Self::BoolAnd { .. }
            | Self::BoolOr { .. }
            | Self::BoolXor { .. }
            | Self::I64And { .. }
            | Self::I64Or { .. }
            | Self::I64Xor { .. }
            | Self::I64Shl { .. }
            | Self::I64Shr { .. }
            | Self::BytesEq { .. }
            | Self::StrEq { .. }
            | Self::BytesConcat { .. }
            | Self::StrConcat { .. }
            | Self::BytesGet { .. }
            | Self::ArrayGet { .. } => OPERANDS_REG_REG_REG,

            Self::F64Neg { .. }
            | Self::F64Abs { .. }
            | Self::F64ToBits { .. }
            | Self::F64FromBits { .. }
            | Self::U64ToI64 { .. }
            | Self::I64ToU64 { .. }
            | Self::I64ToF64 { .. }
            | Self::U64ToF64 { .. }
            | Self::F64ToI64 { .. }
            | Self::F64ToU64 { .. }
            | Self::DecToI64 { .. }
            | Self::DecToU64 { .. }
            | Self::BoolNot { .. }
            | Self::StrToBytes { .. }
            | Self::BytesToStr { .. }
            | Self::TupleLen { .. }
            | Self::StructFieldCount { .. }
            | Self::ArrayLen { .. }
            | Self::BytesLen { .. }
            | Self::StrLen { .. } => OPERANDS_REG_REG,

            Self::I64ToDec { .. } | Self::U64ToDec { .. } => OPERANDS_REG_REG_IMM_U8,
            Self::Select { .. } | Self::BytesSlice { .. } | Self::StrSlice { .. } => {
                OPERANDS_REG_REG_REG_REG
            }

            Self::BytesGetImm { .. } | Self::ArrayGetImm { .. } | Self::TupleGet { .. } => {
                OPERANDS_REG_REG_IMM_U32
            }
            Self::StructGet { .. } => OPERANDS_REG_REG_IMM_U32,

            Self::Br { .. } => OPERANDS_REG_PC_PC,
            Self::Jmp { .. } => OPERANDS_PC,

            Self::Call { .. } => OPERANDS_REG_FUNC_ID_REG_REG_LIST_REG_LIST,
            Self::HostCall { .. } => OPERANDS_REG_HOST_SIG_ID_REG_REG_LIST_REG_LIST,
            Self::Ret { .. } => OPERANDS_REG_REG_LIST,

            Self::TupleNew { .. } => OPERANDS_REG_REG_LIST,
            Self::StructNew { .. } => OPERANDS_REG_TYPE_ID_REG_LIST,
            Self::ArrayNew { .. } => OPERANDS_REG_ELEM_TYPE_ID_REG_LIST,
        }
    }

    /// Iterates the virtual registers read by this instruction (allocation-free).
    #[must_use]
    pub(crate) fn reads(&self) -> ReadsIter<'_> {
        match self {
            Self::Nop
            | Self::Trap { .. }
            | Self::ConstUnit { .. }
            | Self::ConstBool { .. }
            | Self::ConstI64 { .. }
            | Self::ConstU64 { .. }
            | Self::ConstF64 { .. }
            | Self::ConstDecimal { .. }
            | Self::ConstPool { .. }
            | Self::Jmp { .. } => ReadsIter::none(),

            Self::Mov { src, .. }
            | Self::F64Neg { a: src, .. }
            | Self::F64Abs { a: src, .. }
            | Self::F64ToBits { a: src, .. }
            | Self::F64FromBits { a: src, .. }
            | Self::U64ToI64 { a: src, .. }
            | Self::I64ToU64 { a: src, .. }
            | Self::I64ToF64 { a: src, .. }
            | Self::U64ToF64 { a: src, .. }
            | Self::F64ToI64 { a: src, .. }
            | Self::F64ToU64 { a: src, .. }
            | Self::DecToI64 { a: src, .. }
            | Self::DecToU64 { a: src, .. }
            | Self::BoolNot { a: src, .. }
            | Self::ArrayLen { arr: src, .. }
            | Self::TupleLen { tuple: src, .. }
            | Self::StructFieldCount { st: src, .. }
            | Self::BytesLen { bytes: src, .. }
            | Self::StrLen { s: src, .. }
            | Self::StrToBytes { s: src, .. }
            | Self::BytesToStr { bytes: src, .. }
            | Self::TupleGet { tuple: src, .. }
            | Self::StructGet { st: src, .. }
            | Self::ArrayGetImm { arr: src, .. }
            | Self::BytesGetImm { bytes: src, .. } => ReadsIter::one(*src),

            Self::Br { cond, .. } => ReadsIter::one(*cond),

            Self::I64ToDec { a, .. } | Self::U64ToDec { a, .. } => ReadsIter::one(*a),

            Self::Select { cond, a, b, .. } => ReadsIter::three(*cond, *a, *b),

            Self::DecAdd { a, b, .. }
            | Self::DecSub { a, b, .. }
            | Self::DecMul { a, b, .. }
            | Self::F64Add { a, b, .. }
            | Self::F64Sub { a, b, .. }
            | Self::F64Mul { a, b, .. }
            | Self::F64Div { a, b, .. }
            | Self::F64Min { a, b, .. }
            | Self::F64Max { a, b, .. }
            | Self::F64MinNum { a, b, .. }
            | Self::F64MaxNum { a, b, .. }
            | Self::F64Rem { a, b, .. }
            | Self::I64Add { a, b, .. }
            | Self::I64Sub { a, b, .. }
            | Self::I64Mul { a, b, .. }
            | Self::U64Add { a, b, .. }
            | Self::U64Sub { a, b, .. }
            | Self::U64Mul { a, b, .. }
            | Self::U64And { a, b, .. }
            | Self::U64Or { a, b, .. }
            | Self::U64Xor { a, b, .. }
            | Self::U64Shl { a, b, .. }
            | Self::U64Shr { a, b, .. }
            | Self::I64And { a, b, .. }
            | Self::I64Or { a, b, .. }
            | Self::I64Xor { a, b, .. }
            | Self::I64Shl { a, b, .. }
            | Self::I64Shr { a, b, .. }
            | Self::U64Div { a, b, .. }
            | Self::U64Rem { a, b, .. }
            | Self::I64Div { a, b, .. }
            | Self::I64Rem { a, b, .. }
            | Self::U64Eq { a, b, .. }
            | Self::U64Lt { a, b, .. }
            | Self::U64Gt { a, b, .. }
            | Self::U64Le { a, b, .. }
            | Self::U64Ge { a, b, .. }
            | Self::I64Eq { a, b, .. }
            | Self::I64Lt { a, b, .. }
            | Self::I64Gt { a, b, .. }
            | Self::I64Le { a, b, .. }
            | Self::I64Ge { a, b, .. }
            | Self::F64Eq { a, b, .. }
            | Self::F64Lt { a, b, .. }
            | Self::F64Gt { a, b, .. }
            | Self::F64Le { a, b, .. }
            | Self::F64Ge { a, b, .. }
            | Self::BoolAnd { a, b, .. }
            | Self::BoolOr { a, b, .. }
            | Self::BoolXor { a, b, .. }
            | Self::BytesEq { a, b, .. }
            | Self::StrEq { a, b, .. }
            | Self::BytesConcat { a, b, .. }
            | Self::StrConcat { a, b, .. }
            | Self::ArrayGet {
                arr: a, index: b, ..
            }
            | Self::BytesGet {
                bytes: a, index: b, ..
            } => ReadsIter::two(*a, *b),

            Self::BytesSlice {
                bytes, start, end, ..
            } => ReadsIter::three(*bytes, *start, *end),
            Self::StrSlice { s, start, end, .. } => ReadsIter::three(*s, *start, *end),

            Self::TupleNew { values, .. }
            | Self::StructNew { values, .. }
            | Self::ArrayNew { values, .. } => ReadsIter::slice(values.as_slice()),

            Self::Call { eff_in, args, .. } | Self::HostCall { eff_in, args, .. } => {
                ReadsIter::one_plus_slice(*eff_in, args.as_slice())
            }
            Self::Ret { eff_in, rets } => ReadsIter::one_plus_slice(*eff_in, rets.as_slice()),
        }
    }

    /// Iterates the virtual registers written by this instruction (allocation-free).
    #[must_use]
    pub(crate) fn writes(&self) -> WritesIter<'_> {
        match self {
            Self::Nop
            | Self::Trap { .. }
            | Self::Br { .. }
            | Self::Jmp { .. }
            | Self::Ret { .. } => WritesIter::none(),

            Self::Call { eff_out, rets, .. } | Self::HostCall { eff_out, rets, .. } => {
                WritesIter::one_plus_slice(*eff_out, rets.as_slice())
            }

            Self::Mov { dst, .. }
            | Self::ConstUnit { dst }
            | Self::ConstBool { dst, .. }
            | Self::ConstI64 { dst, .. }
            | Self::ConstU64 { dst, .. }
            | Self::ConstF64 { dst, .. }
            | Self::ConstDecimal { dst, .. }
            | Self::ConstPool { dst, .. }
            | Self::DecAdd { dst, .. }
            | Self::DecSub { dst, .. }
            | Self::DecMul { dst, .. }
            | Self::F64Add { dst, .. }
            | Self::F64Sub { dst, .. }
            | Self::F64Mul { dst, .. }
            | Self::F64Div { dst, .. }
            | Self::F64Neg { dst, .. }
            | Self::F64Abs { dst, .. }
            | Self::F64Min { dst, .. }
            | Self::F64Max { dst, .. }
            | Self::F64MinNum { dst, .. }
            | Self::F64MaxNum { dst, .. }
            | Self::F64Rem { dst, .. }
            | Self::F64ToBits { dst, .. }
            | Self::F64FromBits { dst, .. }
            | Self::I64Add { dst, .. }
            | Self::I64Sub { dst, .. }
            | Self::I64Mul { dst, .. }
            | Self::U64Add { dst, .. }
            | Self::U64Sub { dst, .. }
            | Self::U64Mul { dst, .. }
            | Self::U64And { dst, .. }
            | Self::U64Or { dst, .. }
            | Self::U64Xor { dst, .. }
            | Self::U64Shl { dst, .. }
            | Self::U64Shr { dst, .. }
            | Self::I64Eq { dst, .. }
            | Self::I64Lt { dst, .. }
            | Self::U64Eq { dst, .. }
            | Self::U64Lt { dst, .. }
            | Self::U64Gt { dst, .. }
            | Self::U64Le { dst, .. }
            | Self::U64Ge { dst, .. }
            | Self::BoolNot { dst, .. }
            | Self::BoolAnd { dst, .. }
            | Self::BoolOr { dst, .. }
            | Self::BoolXor { dst, .. }
            | Self::I64And { dst, .. }
            | Self::I64Or { dst, .. }
            | Self::I64Xor { dst, .. }
            | Self::I64Shl { dst, .. }
            | Self::I64Shr { dst, .. }
            | Self::I64Gt { dst, .. }
            | Self::I64Le { dst, .. }
            | Self::I64Ge { dst, .. }
            | Self::F64Eq { dst, .. }
            | Self::F64Lt { dst, .. }
            | Self::F64Gt { dst, .. }
            | Self::F64Le { dst, .. }
            | Self::F64Ge { dst, .. }
            | Self::U64ToI64 { dst, .. }
            | Self::I64ToU64 { dst, .. }
            | Self::Select { dst, .. }
            | Self::TupleNew { dst, .. }
            | Self::TupleGet { dst, .. }
            | Self::StructNew { dst, .. }
            | Self::StructGet { dst, .. }
            | Self::ArrayNew { dst, .. }
            | Self::ArrayLen { dst, .. }
            | Self::ArrayGet { dst, .. }
            | Self::ArrayGetImm { dst, .. }
            | Self::TupleLen { dst, .. }
            | Self::StructFieldCount { dst, .. }
            | Self::BytesLen { dst, .. }
            | Self::StrLen { dst, .. }
            | Self::I64Div { dst, .. }
            | Self::I64Rem { dst, .. }
            | Self::U64Div { dst, .. }
            | Self::U64Rem { dst, .. }
            | Self::I64ToF64 { dst, .. }
            | Self::U64ToF64 { dst, .. }
            | Self::F64ToI64 { dst, .. }
            | Self::F64ToU64 { dst, .. }
            | Self::DecToI64 { dst, .. }
            | Self::DecToU64 { dst, .. }
            | Self::I64ToDec { dst, .. }
            | Self::U64ToDec { dst, .. }
            | Self::BytesEq { dst, .. }
            | Self::StrEq { dst, .. }
            | Self::BytesConcat { dst, .. }
            | Self::StrConcat { dst, .. }
            | Self::BytesGet { dst, .. }
            | Self::BytesGetImm { dst, .. }
            | Self::BytesSlice { dst, .. }
            | Self::StrSlice { dst, .. }
            | Self::StrToBytes { dst, .. }
            | Self::BytesToStr { dst, .. } => WritesIter::one(*dst),
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub(crate) struct ReadsIter<'a> {
    pub(crate) prefix: [u32; 3],
    pub(crate) prefix_len: u8,
    pub(crate) prefix_idx: u8,
    pub(crate) rest: &'a [u32],
    pub(crate) rest_idx: usize,
}

impl<'a> ReadsIter<'a> {
    const fn none() -> Self {
        Self {
            prefix: [0, 0, 0],
            prefix_len: 0,
            prefix_idx: 0,
            rest: &[],
            rest_idx: 0,
        }
    }

    const fn one(a: u32) -> Self {
        Self {
            prefix: [a, 0, 0],
            prefix_len: 1,
            prefix_idx: 0,
            rest: &[],
            rest_idx: 0,
        }
    }

    const fn two(a: u32, b: u32) -> Self {
        Self {
            prefix: [a, b, 0],
            prefix_len: 2,
            prefix_idx: 0,
            rest: &[],
            rest_idx: 0,
        }
    }

    const fn three(a: u32, b: u32, c: u32) -> Self {
        Self {
            prefix: [a, b, c],
            prefix_len: 3,
            prefix_idx: 0,
            rest: &[],
            rest_idx: 0,
        }
    }

    const fn slice(rest: &'a [u32]) -> Self {
        Self {
            prefix: [0, 0, 0],
            prefix_len: 0,
            prefix_idx: 0,
            rest,
            rest_idx: 0,
        }
    }

    const fn one_plus_slice(first: u32, rest: &'a [u32]) -> Self {
        Self {
            prefix: [first, 0, 0],
            prefix_len: 1,
            prefix_idx: 0,
            rest,
            rest_idx: 0,
        }
    }
}

impl Iterator for ReadsIter<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<Self::Item> {
        if self.prefix_idx < self.prefix_len {
            let out = self.prefix[usize::from(self.prefix_idx)];
            self.prefix_idx += 1;
            return Some(out);
        }
        let out = self.rest.get(self.rest_idx).copied()?;
        self.rest_idx += 1;
        Some(out)
    }
}

#[derive(Copy, Clone, Debug)]
pub(crate) struct WritesIter<'a> {
    pub(crate) first: Option<u32>,
    pub(crate) rest: &'a [u32],
    pub(crate) idx: usize,
}

impl<'a> WritesIter<'a> {
    const fn none() -> Self {
        Self {
            first: None,
            rest: &[],
            idx: 0,
        }
    }

    const fn one(r: u32) -> Self {
        Self {
            first: Some(r),
            rest: &[],
            idx: 0,
        }
    }

    const fn one_plus_slice(first: u32, rest: &'a [u32]) -> Self {
        Self {
            first: Some(first),
            rest,
            idx: 0,
        }
    }
}

impl Iterator for WritesIter<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(r) = self.first.take() {
            return Some(r);
        }
        let r = self.rest.get(self.idx).copied()?;
        self.idx += 1;
        Some(r)
    }
}

/// Decodes `bytes` into a list of instructions.
pub(crate) fn decode_instructions(bytes: &[u8]) -> Result<Vec<DecodedInstr>, BytecodeError> {
    let mut r = Reader::new(bytes);
    let mut out: Vec<DecodedInstr> = Vec::new();
    while r.offset() < bytes.len() {
        let offset = u32::try_from(r.offset()).map_err(|_| DecodeError::OutOfBounds)?;
        let opcode = r.read_u8()?;
        let op = Opcode::from_u8(opcode).ok_or(BytecodeError::UnknownOpcode { opcode })?;
        let instr = match op {
            Opcode::Nop => Instr::Nop,
            Opcode::Mov => Instr::Mov {
                dst: read_reg(&mut r)?,
                src: read_reg(&mut r)?,
            },
            Opcode::Trap => Instr::Trap {
                code: read_u32_uleb(&mut r)?,
            },

            Opcode::ConstUnit => Instr::ConstUnit {
                dst: read_reg(&mut r)?,
            },
            Opcode::ConstBool => Instr::ConstBool {
                dst: read_reg(&mut r)?,
                imm: r.read_u8()? != 0,
            },
            Opcode::ConstI64 => Instr::ConstI64 {
                dst: read_reg(&mut r)?,
                imm: r.read_sleb128_i64()?,
            },
            Opcode::ConstU64 => Instr::ConstU64 {
                dst: read_reg(&mut r)?,
                imm: r.read_uleb128_u64()?,
            },
            Opcode::ConstF64 => Instr::ConstF64 {
                dst: read_reg(&mut r)?,
                bits: r.read_u64_le()?,
            },
            Opcode::ConstDecimal => Instr::ConstDecimal {
                dst: read_reg(&mut r)?,
                mantissa: r.read_sleb128_i64()?,
                scale: r.read_u8()?,
            },
            Opcode::ConstPool => Instr::ConstPool {
                dst: read_reg(&mut r)?,
                idx: ConstId(read_u32_uleb(&mut r)?),
            },
            Opcode::DecAdd => Instr::DecAdd {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::DecSub => Instr::DecSub {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::DecMul => Instr::DecMul {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::F64Add => Instr::F64Add {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::F64Sub => Instr::F64Sub {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::F64Mul => Instr::F64Mul {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::I64Add => Instr::I64Add {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::I64Sub => Instr::I64Sub {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::I64Mul => Instr::I64Mul {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::U64Add => Instr::U64Add {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::U64Sub => Instr::U64Sub {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::U64Mul => Instr::U64Mul {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::U64And => Instr::U64And {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::U64Or => Instr::U64Or {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },

            Opcode::I64Eq => Instr::I64Eq {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::I64Lt => Instr::I64Lt {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::U64Eq => Instr::U64Eq {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::U64Lt => Instr::U64Lt {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::U64Xor => Instr::U64Xor {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::U64Shl => Instr::U64Shl {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::U64Shr => Instr::U64Shr {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::U64Gt => Instr::U64Gt {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },

            Opcode::BoolNot => Instr::BoolNot {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
            },
            Opcode::BoolAnd => Instr::BoolAnd {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::BoolOr => Instr::BoolOr {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::BoolXor => Instr::BoolXor {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::U64Le => Instr::U64Le {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::U64Ge => Instr::U64Ge {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::I64And => Instr::I64And {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },

            Opcode::U64ToI64 => Instr::U64ToI64 {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
            },
            Opcode::I64ToU64 => Instr::I64ToU64 {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
            },
            Opcode::I64Or => Instr::I64Or {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::I64Xor => Instr::I64Xor {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },

            Opcode::Select => Instr::Select {
                dst: read_reg(&mut r)?,
                cond: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::I64Gt => Instr::I64Gt {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::I64Le => Instr::I64Le {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::I64Ge => Instr::I64Ge {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::I64Shl => Instr::I64Shl {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::I64Shr => Instr::I64Shr {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },

            Opcode::Br => Instr::Br {
                cond: read_reg(&mut r)?,
                pc_true: read_u32_uleb(&mut r)?,
                pc_false: read_u32_uleb(&mut r)?,
            },
            Opcode::Jmp => Instr::Jmp {
                pc_target: read_u32_uleb(&mut r)?,
            },

            Opcode::Call => {
                let eff_out = read_reg(&mut r)?;
                let func_id = FuncId(read_u32_uleb(&mut r)?);
                let eff_in = read_reg(&mut r)?;
                let argc = read_u32_uleb(&mut r)? as usize;
                let mut args = Vec::with_capacity(argc);
                for _ in 0..argc {
                    args.push(read_reg(&mut r)?);
                }
                let retc = read_u32_uleb(&mut r)? as usize;
                let mut rets = Vec::with_capacity(retc);
                for _ in 0..retc {
                    rets.push(read_reg(&mut r)?);
                }
                Instr::Call {
                    eff_out,
                    func_id,
                    eff_in,
                    args,
                    rets,
                }
            }
            Opcode::Ret => {
                let eff_in = read_reg(&mut r)?;
                let retc = read_u32_uleb(&mut r)? as usize;
                let mut rets = Vec::with_capacity(retc);
                for _ in 0..retc {
                    rets.push(read_reg(&mut r)?);
                }
                Instr::Ret { eff_in, rets }
            }
            Opcode::HostCall => {
                let eff_out = read_reg(&mut r)?;
                let host_sig = HostSigId(read_u32_uleb(&mut r)?);
                let eff_in = read_reg(&mut r)?;
                let argc = read_u32_uleb(&mut r)? as usize;
                let mut args = Vec::with_capacity(argc);
                for _ in 0..argc {
                    args.push(read_reg(&mut r)?);
                }
                let retc = read_u32_uleb(&mut r)? as usize;
                let mut rets = Vec::with_capacity(retc);
                for _ in 0..retc {
                    rets.push(read_reg(&mut r)?);
                }
                Instr::HostCall {
                    eff_out,
                    host_sig,
                    eff_in,
                    args,
                    rets,
                }
            }
            Opcode::TupleNew => {
                let dst = read_reg(&mut r)?;
                let arity = read_u32_uleb(&mut r)? as usize;
                let mut values = Vec::with_capacity(arity);
                for _ in 0..arity {
                    values.push(read_reg(&mut r)?);
                }
                Instr::TupleNew { dst, values }
            }
            Opcode::TupleGet => Instr::TupleGet {
                dst: read_reg(&mut r)?,
                tuple: read_reg(&mut r)?,
                index: read_u32_uleb(&mut r)?,
            },
            Opcode::StructNew => {
                let dst = read_reg(&mut r)?;
                let type_id = TypeId(read_u32_uleb(&mut r)?);
                let field_count = read_u32_uleb(&mut r)? as usize;
                let mut values = Vec::with_capacity(field_count);
                for _ in 0..field_count {
                    values.push(read_reg(&mut r)?);
                }
                Instr::StructNew {
                    dst,
                    type_id,
                    values,
                }
            }
            Opcode::StructGet => Instr::StructGet {
                dst: read_reg(&mut r)?,
                st: read_reg(&mut r)?,
                field_index: read_u32_uleb(&mut r)?,
            },
            Opcode::ArrayNew => {
                let dst = read_reg(&mut r)?;
                let elem_type_id = ElemTypeId(read_u32_uleb(&mut r)?);
                let len = read_u32_uleb(&mut r)?;
                let mut values = Vec::with_capacity(len as usize);
                for _ in 0..(len as usize) {
                    values.push(read_reg(&mut r)?);
                }
                Instr::ArrayNew {
                    dst,
                    elem_type_id,
                    len,
                    values,
                }
            }
            Opcode::ArrayLen => Instr::ArrayLen {
                dst: read_reg(&mut r)?,
                arr: read_reg(&mut r)?,
            },
            Opcode::ArrayGet => Instr::ArrayGet {
                dst: read_reg(&mut r)?,
                arr: read_reg(&mut r)?,
                index: read_reg(&mut r)?,
            },
            Opcode::TupleLen => Instr::TupleLen {
                dst: read_reg(&mut r)?,
                tuple: read_reg(&mut r)?,
            },
            Opcode::StructFieldCount => Instr::StructFieldCount {
                dst: read_reg(&mut r)?,
                st: read_reg(&mut r)?,
            },
            Opcode::ArrayGetImm => Instr::ArrayGetImm {
                dst: read_reg(&mut r)?,
                arr: read_reg(&mut r)?,
                index: read_u32_uleb(&mut r)?,
            },
            Opcode::BytesLen => Instr::BytesLen {
                dst: read_reg(&mut r)?,
                bytes: read_reg(&mut r)?,
            },
            Opcode::StrLen => Instr::StrLen {
                dst: read_reg(&mut r)?,
                s: read_reg(&mut r)?,
            },
            Opcode::I64Div => Instr::I64Div {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::I64Rem => Instr::I64Rem {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::U64Div => Instr::U64Div {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::U64Rem => Instr::U64Rem {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::I64ToF64 => Instr::I64ToF64 {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
            },
            Opcode::U64ToF64 => Instr::U64ToF64 {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
            },
            Opcode::F64ToI64 => Instr::F64ToI64 {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
            },
            Opcode::F64ToU64 => Instr::F64ToU64 {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
            },
            Opcode::DecToI64 => Instr::DecToI64 {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
            },
            Opcode::DecToU64 => Instr::DecToU64 {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
            },
            Opcode::I64ToDec => Instr::I64ToDec {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                scale: r.read_u8()?,
            },
            Opcode::U64ToDec => Instr::U64ToDec {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                scale: r.read_u8()?,
            },
            Opcode::BytesEq => Instr::BytesEq {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::StrEq => Instr::StrEq {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::BytesConcat => Instr::BytesConcat {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::StrConcat => Instr::StrConcat {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::BytesGet => Instr::BytesGet {
                dst: read_reg(&mut r)?,
                bytes: read_reg(&mut r)?,
                index: read_reg(&mut r)?,
            },
            Opcode::BytesGetImm => Instr::BytesGetImm {
                dst: read_reg(&mut r)?,
                bytes: read_reg(&mut r)?,
                index: read_u32_uleb(&mut r)?,
            },
            Opcode::BytesSlice => Instr::BytesSlice {
                dst: read_reg(&mut r)?,
                bytes: read_reg(&mut r)?,
                start: read_reg(&mut r)?,
                end: read_reg(&mut r)?,
            },
            Opcode::StrSlice => Instr::StrSlice {
                dst: read_reg(&mut r)?,
                s: read_reg(&mut r)?,
                start: read_reg(&mut r)?,
                end: read_reg(&mut r)?,
            },
            Opcode::StrToBytes => Instr::StrToBytes {
                dst: read_reg(&mut r)?,
                s: read_reg(&mut r)?,
            },
            Opcode::BytesToStr => Instr::BytesToStr {
                dst: read_reg(&mut r)?,
                bytes: read_reg(&mut r)?,
            },
            Opcode::F64Div => Instr::F64Div {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::F64Neg => Instr::F64Neg {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
            },
            Opcode::F64Abs => Instr::F64Abs {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
            },
            Opcode::F64Min => Instr::F64Min {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::F64Max => Instr::F64Max {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::F64MinNum => Instr::F64MinNum {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::F64MaxNum => Instr::F64MaxNum {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::F64Rem => Instr::F64Rem {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::F64ToBits => Instr::F64ToBits {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
            },
            Opcode::F64FromBits => Instr::F64FromBits {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
            },
            Opcode::F64Eq => Instr::F64Eq {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::F64Lt => Instr::F64Lt {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::F64Gt => Instr::F64Gt {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::F64Le => Instr::F64Le {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
            Opcode::F64Ge => Instr::F64Ge {
                dst: read_reg(&mut r)?,
                a: read_reg(&mut r)?,
                b: read_reg(&mut r)?,
            },
        };

        #[cfg(debug_assertions)]
        debug_assert_eq!(
            op.operand_kinds(),
            instr.operand_kinds(),
            "opcode schema drift for {op:?} at pc={offset}"
        );

        out.push(DecodedInstr {
            offset,
            opcode,
            instr,
        });
    }
    Ok(out)
}

fn read_u32_uleb(r: &mut Reader<'_>) -> Result<u32, DecodeError> {
    let v = r.read_uleb128_u64()?;
    u32::try_from(v).map_err(|_| DecodeError::OutOfBounds)
}

fn read_reg(r: &mut Reader<'_>) -> Result<u32, DecodeError> {
    read_u32_uleb(r)
}

#[cfg(test)]
mod tests {
    use super::Instr;
    use alloc::vec;
    use alloc::vec::Vec;

    use crate::opcode::{Opcode, OperandKind};
    use crate::program::HostSigId;
    use crate::program::{ConstId, ElemTypeId, TypeId};
    use crate::value::FuncId;

    #[allow(
        dead_code,
        reason = "kept for now; may be useful when adding role drift checks"
    )]
    fn inferred_operand_kinds_for_instr(instr: &Instr) -> Vec<OperandKind> {
        match instr {
            Instr::Nop => vec![],
            Instr::Mov { .. } => vec![OperandKind::Reg, OperandKind::Reg],
            Instr::Trap { .. } => vec![OperandKind::ImmU32],

            Instr::ConstUnit { .. } => vec![OperandKind::Reg],
            Instr::ConstBool { .. } => vec![OperandKind::Reg, OperandKind::ImmBool],
            Instr::ConstI64 { .. } => vec![OperandKind::Reg, OperandKind::ImmI64],
            Instr::ConstU64 { .. } => vec![OperandKind::Reg, OperandKind::ImmU64],
            Instr::ConstF64 { .. } => vec![OperandKind::Reg, OperandKind::ImmU64],
            Instr::ConstDecimal { .. } => {
                vec![OperandKind::Reg, OperandKind::ImmI64, OperandKind::ImmU8]
            }
            Instr::ConstPool { .. } => vec![OperandKind::Reg, OperandKind::ConstId],

            Instr::DecAdd { .. }
            | Instr::DecSub { .. }
            | Instr::DecMul { .. }
            | Instr::F64Add { .. }
            | Instr::F64Sub { .. }
            | Instr::F64Mul { .. }
            | Instr::F64Div { .. }
            | Instr::F64Min { .. }
            | Instr::F64Max { .. }
            | Instr::F64MinNum { .. }
            | Instr::F64MaxNum { .. }
            | Instr::F64Rem { .. }
            | Instr::I64Add { .. }
            | Instr::I64Sub { .. }
            | Instr::I64Mul { .. }
            | Instr::U64Add { .. }
            | Instr::U64Sub { .. }
            | Instr::U64Mul { .. }
            | Instr::U64And { .. }
            | Instr::U64Or { .. }
            | Instr::U64Xor { .. }
            | Instr::U64Shl { .. }
            | Instr::U64Shr { .. }
            | Instr::U64Div { .. }
            | Instr::U64Rem { .. }
            | Instr::I64Div { .. }
            | Instr::I64Rem { .. }
            | Instr::I64Eq { .. }
            | Instr::I64Lt { .. }
            | Instr::I64Gt { .. }
            | Instr::I64Le { .. }
            | Instr::I64Ge { .. }
            | Instr::U64Eq { .. }
            | Instr::U64Lt { .. }
            | Instr::U64Gt { .. }
            | Instr::U64Le { .. }
            | Instr::U64Ge { .. }
            | Instr::F64Eq { .. }
            | Instr::F64Lt { .. }
            | Instr::F64Gt { .. }
            | Instr::F64Le { .. }
            | Instr::F64Ge { .. }
            | Instr::BoolAnd { .. }
            | Instr::BoolOr { .. }
            | Instr::BoolXor { .. }
            | Instr::I64And { .. }
            | Instr::I64Or { .. }
            | Instr::I64Xor { .. }
            | Instr::I64Shl { .. }
            | Instr::I64Shr { .. }
            | Instr::BytesEq { .. }
            | Instr::StrEq { .. }
            | Instr::BytesConcat { .. }
            | Instr::StrConcat { .. }
            | Instr::BytesGet { .. }
            | Instr::ArrayGet { .. } => vec![OperandKind::Reg, OperandKind::Reg, OperandKind::Reg],

            Instr::F64Neg { .. }
            | Instr::F64Abs { .. }
            | Instr::F64ToBits { .. }
            | Instr::F64FromBits { .. }
            | Instr::U64ToI64 { .. }
            | Instr::I64ToU64 { .. }
            | Instr::I64ToF64 { .. }
            | Instr::U64ToF64 { .. }
            | Instr::F64ToI64 { .. }
            | Instr::F64ToU64 { .. }
            | Instr::DecToI64 { .. }
            | Instr::DecToU64 { .. }
            | Instr::BoolNot { .. }
            | Instr::StrToBytes { .. }
            | Instr::BytesToStr { .. }
            | Instr::TupleLen { .. }
            | Instr::StructFieldCount { .. }
            | Instr::ArrayLen { .. }
            | Instr::BytesLen { .. }
            | Instr::StrLen { .. } => vec![OperandKind::Reg, OperandKind::Reg],

            Instr::I64ToDec { .. } | Instr::U64ToDec { .. } => {
                vec![OperandKind::Reg, OperandKind::Reg, OperandKind::ImmU8]
            }

            Instr::Select { .. } | Instr::BytesSlice { .. } | Instr::StrSlice { .. } => vec![
                OperandKind::Reg,
                OperandKind::Reg,
                OperandKind::Reg,
                OperandKind::Reg,
            ],

            Instr::BytesGetImm { .. } | Instr::ArrayGetImm { .. } | Instr::TupleGet { .. } => {
                vec![OperandKind::Reg, OperandKind::Reg, OperandKind::ImmU32]
            }

            Instr::StructGet { .. } => {
                vec![OperandKind::Reg, OperandKind::Reg, OperandKind::ImmU32]
            }

            Instr::Br { .. } => vec![OperandKind::Reg, OperandKind::Pc, OperandKind::Pc],
            Instr::Jmp { .. } => vec![OperandKind::Pc],

            Instr::Call { .. } => vec![
                OperandKind::Reg,
                OperandKind::FuncId,
                OperandKind::Reg,
                OperandKind::RegList,
                OperandKind::RegList,
            ],
            Instr::HostCall { .. } => vec![
                OperandKind::Reg,
                OperandKind::HostSigId,
                OperandKind::Reg,
                OperandKind::RegList,
                OperandKind::RegList,
            ],
            Instr::Ret { .. } => vec![OperandKind::Reg, OperandKind::RegList],

            Instr::TupleNew { .. } => vec![OperandKind::Reg, OperandKind::RegList],
            Instr::StructNew { .. } => {
                vec![OperandKind::Reg, OperandKind::TypeId, OperandKind::RegList]
            }
            Instr::ArrayNew { .. } => {
                vec![
                    OperandKind::Reg,
                    OperandKind::ElemTypeId,
                    OperandKind::RegList,
                ]
            }
        }
    }

    #[test]
    fn opcode_schema_matches_decoded_instr_shape_for_smoke_set() {
        // Chosen to cover all operand kinds, including reg lists and ids.
        let instrs: &[(Opcode, Instr)] = &[
            (Opcode::Nop, Instr::Nop),
            (Opcode::Mov, Instr::Mov { dst: 1, src: 2 }),
            (Opcode::Trap, Instr::Trap { code: 7 }),
            (Opcode::ConstBool, Instr::ConstBool { dst: 1, imm: true }),
            (Opcode::ConstI64, Instr::ConstI64 { dst: 1, imm: -3 }),
            (Opcode::ConstU64, Instr::ConstU64 { dst: 1, imm: 3 }),
            (Opcode::ConstF64, Instr::ConstF64 { dst: 1, bits: 42 }),
            (
                Opcode::ConstDecimal,
                Instr::ConstDecimal {
                    dst: 1,
                    mantissa: 12,
                    scale: 2,
                },
            ),
            (
                Opcode::ConstPool,
                Instr::ConstPool {
                    dst: 1,
                    idx: ConstId(0),
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
            assert_eq!(
                op.operand_kinds().len(),
                op.operand_roles().len(),
                "schema mismatch (kinds vs roles) for {op:?}"
            );
            assert_eq!(
                op.operand_kinds(),
                instr.operand_kinds(),
                "opcode operand schema drift for {op:?}"
            );
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
}
