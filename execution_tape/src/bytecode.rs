// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Bytecode decoding for `execution_tape` (draft).
//!
//! This module defines a minimal opcode set and a decoder that parses an instruction stream into
//! typed instructions with byte offsets. The encoding is currently draft and will evolve alongside
//! the verifier and interpreter.

use alloc::vec::Vec;

use crate::format::{DecodeError, Reader};
use crate::opcode::Opcode;
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
