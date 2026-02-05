// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Disassembler for `execution_tape` programs.
//!
//! This module provides:
//! - A structured view (`Disassembly`, `InstrView`) for tooling/tests.
//! - A stable, human-readable text format via [`core::fmt::Display`].
//!
//! The disassembly format is intentionally “assembly-like” (one instruction per line, label
//! resolution for branch targets) so it can plausibly be parsed in the future.

#![allow(clippy::module_name_repetitions, reason = "public API module")]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::bytecode::{
    BytecodeError, DecodedInstr, Instr, ReadsIter, WritesIter, decode_instructions,
};
use crate::format::DecodeError;
use crate::opcode::{Opcode, OperandKind};
use crate::program::{ConstId, ElemTypeId, HostSigId, Program, TypeId};
use crate::value::FuncId;
use crate::verifier::VerifiedProgram;

/// Disassembles `program` into a structured view.
///
/// This is best-effort: if a function fails to decode, the error is recorded in the returned
/// [`FunctionDisassembly`] and other functions may still be disassembled.
#[must_use]
pub fn disassemble(program: &Program) -> Disassembly<'_> {
    let mut functions: Vec<FunctionDisassembly<'_>> = Vec::with_capacity(program.functions.len());
    for ix in 0..program.functions.len() {
        let func_id = FuncId(u32::try_from(ix).unwrap_or(u32::MAX));
        functions.push(
            disassemble_function(program, func_id)
                .unwrap_or_else(|e| FunctionDisassembly::from_error(program, func_id, e)),
        );
    }
    Disassembly { program, functions }
}

/// Disassembles a [`VerifiedProgram`].
///
/// Currently this uses the underlying [`Program`] bytecode; verifier-enriched disassembly (e.g.
/// spans, block boundaries, typed registers) can be layered on later without changing the basic
/// output model.
#[must_use]
pub fn disassemble_verified(program: &VerifiedProgram) -> Disassembly<'_> {
    disassemble(program.program())
}

/// Disassembles a single function from `program`.
pub fn disassemble_function(
    program: &Program,
    func: FuncId,
) -> Result<FunctionDisassembly<'_>, DisasmError> {
    let func_def = program
        .functions
        .get(func.0 as usize)
        .ok_or(DisasmError::Decode(DecodeError::OutOfBounds))?;
    let bytes = func_def.bytecode(program).map_err(DisasmError::Decode)?;
    let decoded = decode_instructions(bytes).map_err(DisasmError::from_bytecode)?;
    Ok(FunctionDisassembly {
        program,
        func,
        decoded: Ok(decoded),
    })
}

impl<'a> FunctionDisassembly<'a> {
    fn from_error(program: &'a Program, func: FuncId, err: DisasmError) -> Self {
        Self {
            program,
            func,
            decoded: Err(err),
        }
    }

    /// Returns the underlying program reference.
    #[must_use]
    pub fn program(&self) -> &Program {
        self.program
    }

    /// Returns the disassembled function id.
    #[must_use]
    pub fn func(&self) -> FuncId {
        self.func
    }

    /// Returns the decode error, if the function could not be disassembled.
    #[must_use]
    pub fn error(&self) -> Option<&DisasmError> {
        self.decoded.as_ref().err()
    }

    /// Iterates the decoded instruction stream as lightweight [`InstrView`] values.
    pub fn instrs(&self) -> impl Iterator<Item = InstrView<'_>> + '_ {
        let decoded: &[DecodedInstr] = match &self.decoded {
            Ok(v) => v,
            Err(_) => &[],
        };
        decoded
            .iter()
            .map(move |di| instr_view(self.program, self.func, di))
    }

    /// Computes label indices for this function.
    ///
    /// Labels are derived from control-flow targets (`br`/`jmp`) plus the function entry (`pc=0`).
    /// If the function failed to decode, this returns an empty label set.
    #[must_use]
    pub fn labels(&self) -> Labels {
        if self.error().is_some() {
            return Labels { pcs: Vec::new() };
        }

        let mut pcs: Vec<u32> = Vec::new();
        pcs.push(0);
        for iv in self.instrs() {
            match iv.operands() {
                Operands::Br {
                    pc_true, pc_false, ..
                } => {
                    pcs.push(pc_true);
                    pcs.push(pc_false);
                }
                Operands::Jmp { pc_target } => {
                    pcs.push(pc_target);
                }
                _ => {}
            }
        }
        pcs.sort_unstable();
        pcs.dedup();
        Labels { pcs }
    }
}

/// Label indices for a function disassembly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Labels {
    pcs: Vec<u32>,
}

impl Labels {
    /// Returns the sorted label pcs.
    #[must_use]
    pub fn pcs(&self) -> &[u32] {
        &self.pcs
    }

    /// Returns the label index for `pc` if it is labeled.
    #[must_use]
    pub fn label_index(&self, pc: u32) -> Option<usize> {
        self.pcs.binary_search(&pc).ok()
    }
}

/// A program disassembly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Disassembly<'a> {
    /// The program that was disassembled.
    pub program: &'a Program,
    /// Per-function disassemblies in `FuncId` order.
    pub functions: Vec<FunctionDisassembly<'a>>,
}

/// A single-function disassembly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionDisassembly<'a> {
    program: &'a Program,
    func: FuncId,
    decoded: Result<Vec<DecodedInstr>, DisasmError>,
}

/// A disassembly error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DisasmError {
    /// The underlying data failed to decode.
    Decode(DecodeError),
    /// The opcode byte is not recognized.
    UnknownOpcode {
        /// The unrecognized opcode byte.
        opcode: u8,
    },
}

impl DisasmError {
    fn from_bytecode(e: BytecodeError) -> Self {
        match e {
            BytecodeError::Decode(e) => Self::Decode(e),
            BytecodeError::UnknownOpcode { opcode } => Self::UnknownOpcode { opcode },
        }
    }
}

/// A register-like operand split for call-like instructions.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct CallOperands<'a> {
    /// Effect output register (`eff_out`).
    pub eff_out: u32,
    /// Call target (function or host signature).
    pub callee: CallTarget<'a>,
    /// Effect input register (`eff_in`).
    pub eff_in: u32,
    /// Argument registers.
    pub args: &'a [u32],
    /// Return registers.
    pub rets: &'a [u32],
}

/// The callee for a call-like instruction.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CallTarget<'a> {
    /// A program function by id.
    Func(FuncId),
    /// A host signature table entry (id and best-effort resolved symbol).
    HostSig(HostSigId, Option<&'a str>),
}

/// Disassembled instruction operands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Operands<'a> {
    /// Most instructions: `dst` + `srcs` + optional immediate/index.
    Simple,
    /// `br cond, pc_true, pc_false`.
    Br {
        /// Condition register.
        cond: u32,
        /// Jump target if `cond` is true.
        pc_true: u32,
        /// Jump target if `cond` is false.
        pc_false: u32,
    },
    /// `jmp pc_target`.
    Jmp {
        /// Jump target pc.
        pc_target: u32,
    },
    /// `call eff_out, func_id, eff_in, args..., rets...`.
    Call(CallOperands<'a>),
    /// `ret eff_in, rets...`.
    Ret {
        /// Effect register.
        eff: u32,
        /// Return registers.
        rets: &'a [u32],
    },
}

/// A table/index-like immediate operand.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum InputIndex {
    /// Trap code for [`Opcode::Trap`].
    TrapCode(u32),
    /// Constant pool index for [`Opcode::ConstPool`].
    Const(ConstId),
    /// Host signature table index for [`Opcode::HostCall`].
    HostSig(HostSigId),
    /// Function index for [`Opcode::Call`].
    Func(FuncId),
    /// Struct type index for [`Opcode::StructNew`].
    Type(TypeId),
    /// Array element type index for [`Opcode::ArrayNew`].
    ElemType(ElemTypeId),
    /// Generic immediate index (tuple/struct/array access, decimal scale, etc.).
    Index(u32),
}

/// A single disassembled instruction view.
///
/// This is a lightweight view over the decoded instruction stream: it does not allocate, and
/// exposes read/write register iterators.
#[derive(Copy, Clone, Debug)]
pub struct InstrView<'a> {
    program: &'a Program,
    func: FuncId,
    decoded: &'a DecodedInstr,
}

/// An iterator over register operands without allocation.
#[derive(Copy, Clone, Debug)]
pub struct RegIter<'a> {
    prefix: [u32; 3],
    prefix_len: u8,
    prefix_idx: u8,
    tail: &'a [u32],
    tail_idx: usize,
}

impl<'a> RegIter<'a> {
    /// Returns an empty register iterator.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            prefix: [0, 0, 0],
            prefix_len: 0,
            prefix_idx: 0,
            tail: &[],
            tail_idx: 0,
        }
    }

    /// Returns an iterator over a single register.
    #[must_use]
    pub fn one(a: u32) -> Self {
        Self {
            prefix: [a, 0, 0],
            prefix_len: 1,
            prefix_idx: 0,
            tail: &[],
            tail_idx: 0,
        }
    }

    /// Returns an iterator over two registers.
    #[must_use]
    pub fn two(a: u32, b: u32) -> Self {
        Self {
            prefix: [a, b, 0],
            prefix_len: 2,
            prefix_idx: 0,
            tail: &[],
            tail_idx: 0,
        }
    }

    /// Returns an iterator over three registers.
    #[must_use]
    pub fn three(a: u32, b: u32, c: u32) -> Self {
        Self {
            prefix: [a, b, c],
            prefix_len: 3,
            prefix_idx: 0,
            tail: &[],
            tail_idx: 0,
        }
    }

    /// Returns an iterator over a slice of registers.
    #[must_use]
    pub fn slice(tail: &'a [u32]) -> Self {
        Self {
            prefix: [0, 0, 0],
            prefix_len: 0,
            prefix_idx: 0,
            tail,
            tail_idx: 0,
        }
    }

    /// Returns an iterator over `first`, followed by all registers in `tail`.
    #[must_use]
    pub fn one_plus_slice(first: u32, tail: &'a [u32]) -> Self {
        Self {
            prefix: [first, 0, 0],
            prefix_len: 1,
            prefix_idx: 0,
            tail,
            tail_idx: 0,
        }
    }
}

impl Iterator for RegIter<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<Self::Item> {
        if self.prefix_idx < self.prefix_len {
            let out = self.prefix[usize::from(self.prefix_idx)];
            self.prefix_idx += 1;
            return Some(out);
        }
        let out = self.tail.get(self.tail_idx).copied()?;
        self.tail_idx += 1;
        Some(out)
    }
}

impl<'a> InstrView<'a> {
    /// Function containing this instruction.
    #[must_use]
    pub fn func(&self) -> FuncId {
        self.func
    }

    /// Byte offset (pc) within the function bytecode stream.
    #[must_use]
    pub fn pc(&self) -> u32 {
        self.decoded.offset
    }

    /// Decoded opcode.
    #[must_use]
    pub fn opcode(&self) -> Opcode {
        // `DecodedInstr` is produced by the decoder, so `opcode` is known.
        Opcode::from_u8(self.decoded.opcode).expect("decoded instruction opcode must be known")
    }

    /// Returns `true` if this instruction is a terminator (e.g. `br`, `jmp`, `ret`, `trap`).
    #[must_use]
    pub fn is_terminator(&self) -> bool {
        self.opcode().is_terminator()
    }

    /// Primary destination register, when the instruction has a clear single `dst`.
    #[must_use]
    pub fn dst(&self) -> Option<u32> {
        self.decoded.instr.writes().next()
    }

    /// Optional index-like immediate operand (const pool, host sig table, type ids, etc.).
    #[must_use]
    pub fn input_index(&self) -> Option<InputIndex> {
        match &self.decoded.instr {
            Instr::Trap { code } => Some(InputIndex::TrapCode(*code)),
            Instr::ConstPool { idx, .. } => Some(InputIndex::Const(*idx)),
            Instr::HostCall { host_sig, .. } => Some(InputIndex::HostSig(*host_sig)),
            Instr::Call { func_id, .. } => Some(InputIndex::Func(*func_id)),
            Instr::StructNew { type_id, .. } => Some(InputIndex::Type(*type_id)),
            Instr::ArrayNew { elem_type_id, .. } => Some(InputIndex::ElemType(*elem_type_id)),
            Instr::TupleGet { index, .. }
            | Instr::StructGet {
                field_index: index, ..
            }
            | Instr::ArrayGetImm { index, .. }
            | Instr::BytesGetImm { index, .. } => Some(InputIndex::Index(*index)),
            Instr::I64ToDec { scale, .. } | Instr::U64ToDec { scale, .. } => {
                Some(InputIndex::Index(u32::from(*scale)))
            }
            _ => None,
        }
    }

    /// Resolved host symbol for `host_call` (best-effort).
    #[must_use]
    pub fn host_op_symbol(&self) -> Option<&'a str> {
        let Instr::HostCall { host_sig, .. } = &self.decoded.instr else {
            return None;
        };
        host_sig_symbol(self.program, *host_sig)
    }

    /// Literal constant value for `const.*` instructions (and best-effort for `const.pool`).
    #[must_use]
    pub fn const_value(&self) -> Option<ConstValue<'a>> {
        match &self.decoded.instr {
            Instr::ConstUnit { .. } => Some(ConstValue::Unit),
            Instr::ConstBool { imm, .. } => Some(ConstValue::Bool(*imm)),
            Instr::ConstI64 { imm, .. } => Some(ConstValue::I64(*imm)),
            Instr::ConstU64 { imm, .. } => Some(ConstValue::U64(*imm)),
            Instr::ConstF64 { bits, .. } => Some(ConstValue::F64Bits(*bits)),
            Instr::ConstDecimal {
                mantissa, scale, ..
            } => Some(ConstValue::Decimal {
                mantissa: *mantissa,
                scale: *scale,
            }),
            Instr::ConstPool { idx, .. } => const_pool_value(self.program, *idx),
            _ => None,
        }
    }

    /// Full-fidelity operands for instructions that don't fit the `dst/srcs/input_index` shape.
    #[must_use]
    pub fn operands(&self) -> Operands<'a> {
        match &self.decoded.instr {
            Instr::Br {
                cond,
                pc_true,
                pc_false,
            } => Operands::Br {
                cond: *cond,
                pc_true: *pc_true,
                pc_false: *pc_false,
            },
            Instr::Jmp { pc_target } => Operands::Jmp {
                pc_target: *pc_target,
            },
            Instr::Call {
                eff_out,
                func_id,
                eff_in,
                args,
                rets,
            } => Operands::Call(CallOperands {
                eff_out: *eff_out,
                callee: CallTarget::Func(*func_id),
                eff_in: *eff_in,
                args,
                rets,
            }),
            Instr::HostCall {
                eff_out,
                host_sig,
                eff_in,
                args,
                rets,
            } => Operands::Call(CallOperands {
                eff_out: *eff_out,
                callee: CallTarget::HostSig(*host_sig, self.host_op_symbol()),
                eff_in: *eff_in,
                args,
                rets,
            }),
            Instr::Ret { eff_in, rets } => Operands::Ret { eff: *eff_in, rets },
            _ => Operands::Simple,
        }
    }

    /// Registers read by this instruction (best-effort; intended for tooling/dataflow).
    #[must_use]
    pub fn reads(&self) -> RegIter<'a> {
        reg_iter_from_reads(self.decoded.instr.reads())
    }

    /// Registers written by this instruction (best-effort; intended for tooling/dataflow).
    #[must_use]
    pub fn writes(&self) -> RegIter<'a> {
        reg_iter_from_writes(self.decoded.instr.writes())
    }
}

fn reg_iter_from_reads<'a>(it: ReadsIter<'a>) -> RegIter<'a> {
    debug_assert_eq!(it.prefix_idx, 0, "ReadsIter must be unconsumed");
    debug_assert_eq!(it.rest_idx, 0, "ReadsIter must be unconsumed");
    RegIter {
        prefix: it.prefix,
        prefix_len: it.prefix_len,
        prefix_idx: 0,
        tail: it.rest,
        tail_idx: 0,
    }
}

fn reg_iter_from_writes<'a>(it: WritesIter<'a>) -> RegIter<'a> {
    debug_assert_eq!(it.idx, 0, "WritesIter must be unconsumed");
    match it.first {
        None => RegIter::empty(),
        Some(r) => {
            if it.rest.is_empty() {
                RegIter::one(r)
            } else {
                RegIter::one_plus_slice(r, it.rest)
            }
        }
    }
}

/// A literal constant value.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ConstValue<'a> {
    /// `()`.
    Unit,
    /// Boolean literal.
    Bool(bool),
    /// Signed 64-bit integer literal.
    I64(i64),
    /// Unsigned 64-bit integer literal.
    U64(u64),
    /// IEEE 754 bits for an `f64` constant.
    F64Bits(u64),
    /// Decimal literal.
    Decimal {
        /// Integer mantissa.
        mantissa: i64,
        /// Base-10 scale.
        scale: u8,
    },
    /// Bytes constant (borrowed from the program constant pool).
    Bytes(&'a [u8]),
    /// UTF-8 string constant (borrowed from the program constant pool).
    Str(&'a str),
}

#[cfg(any())]
fn instr_view<'a>(program: &'a Program, func: FuncId, di: DecodedInstr) -> InstrView<'a> {
    let opcode = Opcode::from_u8(di.opcode).unwrap();
    let mut view = InstrView {
        func,
        pc: di.offset,
        opcode,
        dst: None,
        srcs: Vec::new(),
        input_index: None,
        host_op_symbol: None,
        operands: Operands::Simple,
        const_value: None,
    };

    match di.instr {
        Instr::Nop => {}
        Instr::Mov { dst, src } => {
            view.dst = Some(dst);
            view.srcs = vec![src];
        }
        Instr::Trap { code } => {
            view.input_index = Some(InputIndex::TrapCode(code));
        }

        Instr::ConstUnit { dst } => {
            view.dst = Some(dst);
            view.const_value = Some(ConstValue::Unit);
        }
        Instr::ConstBool { dst, imm } => {
            view.dst = Some(dst);
            view.const_value = Some(ConstValue::Bool(imm));
        }
        Instr::ConstI64 { dst, imm } => {
            view.dst = Some(dst);
            view.const_value = Some(ConstValue::I64(imm));
        }
        Instr::ConstU64 { dst, imm } => {
            view.dst = Some(dst);
            view.const_value = Some(ConstValue::U64(imm));
        }
        Instr::ConstF64 { dst, bits } => {
            view.dst = Some(dst);
            view.const_value = Some(ConstValue::F64Bits(bits));
        }
        Instr::ConstDecimal {
            dst,
            mantissa,
            scale,
        } => {
            view.dst = Some(dst);
            view.const_value = Some(ConstValue::Decimal { mantissa, scale });
        }
        Instr::ConstPool { dst, idx } => {
            view.dst = Some(dst);
            view.input_index = Some(InputIndex::Const(idx));
            view.const_value = const_pool_value(program, idx);
        }

        Instr::DecAdd { dst, a, b }
        | Instr::DecSub { dst, a, b }
        | Instr::DecMul { dst, a, b }
        | Instr::F64Add { dst, a, b }
        | Instr::F64Sub { dst, a, b }
        | Instr::F64Mul { dst, a, b }
        | Instr::F64Div { dst, a, b }
        | Instr::F64Min { dst, a, b }
        | Instr::F64Max { dst, a, b }
        | Instr::F64MinNum { dst, a, b }
        | Instr::F64MaxNum { dst, a, b }
        | Instr::F64Rem { dst, a, b }
        | Instr::I64Add { dst, a, b }
        | Instr::I64Sub { dst, a, b }
        | Instr::I64Mul { dst, a, b }
        | Instr::U64Add { dst, a, b }
        | Instr::U64Sub { dst, a, b }
        | Instr::U64Mul { dst, a, b }
        | Instr::U64And { dst, a, b }
        | Instr::U64Or { dst, a, b }
        | Instr::U64Xor { dst, a, b }
        | Instr::U64Shl { dst, a, b }
        | Instr::U64Shr { dst, a, b }
        | Instr::I64And { dst, a, b }
        | Instr::I64Or { dst, a, b }
        | Instr::I64Xor { dst, a, b }
        | Instr::I64Shl { dst, a, b }
        | Instr::I64Shr { dst, a, b }
        | Instr::U64Div { dst, a, b }
        | Instr::U64Rem { dst, a, b }
        | Instr::I64Div { dst, a, b }
        | Instr::I64Rem { dst, a, b }
        | Instr::U64Eq { dst, a, b }
        | Instr::U64Lt { dst, a, b }
        | Instr::U64Gt { dst, a, b }
        | Instr::U64Le { dst, a, b }
        | Instr::U64Ge { dst, a, b }
        | Instr::I64Eq { dst, a, b }
        | Instr::I64Lt { dst, a, b }
        | Instr::I64Gt { dst, a, b }
        | Instr::I64Le { dst, a, b }
        | Instr::I64Ge { dst, a, b }
        | Instr::F64Eq { dst, a, b }
        | Instr::F64Lt { dst, a, b }
        | Instr::F64Gt { dst, a, b }
        | Instr::F64Le { dst, a, b }
        | Instr::F64Ge { dst, a, b }
        | Instr::BoolAnd { dst, a, b }
        | Instr::BoolOr { dst, a, b }
        | Instr::BoolXor { dst, a, b } => {
            view.dst = Some(dst);
            view.srcs = vec![a, b];
        }

        Instr::F64Neg { dst, a } | Instr::F64Abs { dst, a } | Instr::BoolNot { dst, a } => {
            view.dst = Some(dst);
            view.srcs = vec![a];
        }

        Instr::F64ToBits { dst, a }
        | Instr::F64FromBits { dst, a }
        | Instr::I64ToU64 { dst, a }
        | Instr::U64ToI64 { dst, a }
        | Instr::I64ToF64 { dst, a }
        | Instr::U64ToF64 { dst, a }
        | Instr::F64ToI64 { dst, a }
        | Instr::F64ToU64 { dst, a }
        | Instr::DecToI64 { dst, a }
        | Instr::DecToU64 { dst, a } => {
            view.dst = Some(dst);
            view.srcs = vec![a];
        }

        Instr::I64ToDec { dst, a, scale } | Instr::U64ToDec { dst, a, scale } => {
            view.dst = Some(dst);
            view.srcs = vec![a];
            view.input_index = Some(InputIndex::Index(u32::from(scale)));
        }

        Instr::Select { dst, cond, a, b } => {
            view.dst = Some(dst);
            view.srcs = vec![cond, a, b];
        }

        Instr::Br {
            cond,
            pc_true,
            pc_false,
        } => {
            view.srcs = vec![cond];
            view.operands = Operands::Br {
                cond,
                pc_true,
                pc_false,
            };
        }
        Instr::Jmp { pc_target } => {
            view.operands = Operands::Jmp { pc_target };
        }

        Instr::Call {
            eff_out,
            func_id,
            eff_in,
            args,
            rets,
        } => {
            view.dst = Some(eff_out);
            view.input_index = Some(InputIndex::Func(func_id));
            view.srcs = Vec::with_capacity(1 + args.len() + rets.len());
            view.srcs.push(eff_in);
            view.srcs.extend(args.iter().copied());
            view.srcs.extend(rets.iter().copied());
            view.operands = Operands::Call(CallLikeOperands {
                eff_out,
                callee: CallTarget::Func(func_id),
                eff_in,
                args,
                rets,
            });
        }
        Instr::Ret { eff_in, rets } => {
            view.srcs = Vec::with_capacity(1 + rets.len());
            view.srcs.push(eff_in);
            view.srcs.extend(rets.iter().copied());
            view.operands = Operands::Ret { eff_in, rets };
        }
        Instr::HostCall {
            eff_out,
            host_sig,
            eff_in,
            args,
            rets,
        } => {
            view.dst = Some(eff_out);
            view.input_index = Some(InputIndex::HostSig(host_sig));
            view.host_op_symbol = host_sig_symbol(program, host_sig);
            view.srcs = Vec::with_capacity(1 + args.len() + rets.len());
            view.srcs.push(eff_in);
            view.srcs.extend(args.iter().copied());
            view.srcs.extend(rets.iter().copied());
            view.operands = Operands::Call(CallLikeOperands {
                eff_out,
                callee: CallTarget::HostSig(host_sig, view.host_op_symbol),
                eff_in,
                args,
                rets,
            });
        }

        Instr::TupleNew { dst, values } => {
            view.dst = Some(dst);
            view.srcs = values;
        }
        Instr::TupleGet { dst, tuple, index } => {
            view.dst = Some(dst);
            view.srcs = vec![tuple];
            view.input_index = Some(InputIndex::Index(index));
        }
        Instr::StructNew {
            dst,
            type_id,
            values,
        } => {
            view.dst = Some(dst);
            view.srcs = values;
            view.input_index = Some(InputIndex::Type(type_id));
        }
        Instr::StructGet {
            dst,
            st,
            field_index,
        } => {
            view.dst = Some(dst);
            view.srcs = vec![st];
            view.input_index = Some(InputIndex::Index(field_index));
        }
        Instr::ArrayNew {
            dst,
            elem_type_id,
            len,
            values,
        } => {
            view.dst = Some(dst);
            view.srcs = values;
            // `len` is redundant with `values.len()` but is part of the encoding.
            view.input_index = Some(InputIndex::ElemType(elem_type_id));
            let _ = len;
        }
        Instr::ArrayLen { dst, arr } => {
            view.dst = Some(dst);
            view.srcs = vec![arr];
        }
        Instr::ArrayGet { dst, arr, index } => {
            view.dst = Some(dst);
            view.srcs = vec![arr, index];
        }
        Instr::ArrayGetImm { dst, arr, index } => {
            view.dst = Some(dst);
            view.srcs = vec![arr];
            view.input_index = Some(InputIndex::Index(index));
        }
        Instr::TupleLen { dst, tuple } => {
            view.dst = Some(dst);
            view.srcs = vec![tuple];
        }
        Instr::StructFieldCount { dst, st } => {
            view.dst = Some(dst);
            view.srcs = vec![st];
        }
        Instr::BytesLen { dst, bytes } => {
            view.dst = Some(dst);
            view.srcs = vec![bytes];
        }
        Instr::StrLen { dst, s } => {
            view.dst = Some(dst);
            view.srcs = vec![s];
        }
        Instr::BytesEq { dst, a, b } | Instr::StrEq { dst, a, b } => {
            view.dst = Some(dst);
            view.srcs = vec![a, b];
        }
        Instr::BytesConcat { dst, a, b } | Instr::StrConcat { dst, a, b } => {
            view.dst = Some(dst);
            view.srcs = vec![a, b];
        }
        Instr::BytesGet { dst, bytes, index } => {
            view.dst = Some(dst);
            view.srcs = vec![bytes, index];
        }
        Instr::BytesGetImm { dst, bytes, index } => {
            view.dst = Some(dst);
            view.srcs = vec![bytes];
            view.input_index = Some(InputIndex::Index(index));
        }
        Instr::BytesSlice {
            dst,
            bytes,
            start,
            end,
        }
        | Instr::StrSlice {
            dst,
            s: bytes,
            start,
            end,
        } => {
            view.dst = Some(dst);
            view.srcs = vec![bytes, start, end];
        }
        Instr::StrToBytes { dst, s } | Instr::BytesToStr { dst, bytes: s } => {
            view.dst = Some(dst);
            view.srcs = vec![s];
        }
    }

    view
}

fn instr_view<'a>(program: &'a Program, func: FuncId, di: &'a DecodedInstr) -> InstrView<'a> {
    InstrView {
        program,
        func,
        decoded: di,
    }
}

fn host_sig_symbol(program: &Program, id: HostSigId) -> Option<&str> {
    let entry = program.host_sigs.get(id.0 as usize)?;
    program.symbol_str(entry.symbol).ok()
}

fn const_pool_value<'a>(program: &'a Program, id: ConstId) -> Option<ConstValue<'a>> {
    let entry = program.const_pool.get(id.0 as usize)?;
    match entry {
        crate::program::ConstEntry::Unit => Some(ConstValue::Unit),
        crate::program::ConstEntry::Bool(b) => Some(ConstValue::Bool(*b)),
        crate::program::ConstEntry::I64(i) => Some(ConstValue::I64(*i)),
        crate::program::ConstEntry::U64(u) => Some(ConstValue::U64(*u)),
        crate::program::ConstEntry::F64(bits) => Some(ConstValue::F64Bits(*bits)),
        crate::program::ConstEntry::Decimal { mantissa, scale } => Some(ConstValue::Decimal {
            mantissa: *mantissa,
            scale: *scale,
        }),
        crate::program::ConstEntry::Bytes(_) => program.const_bytes(id).ok().map(ConstValue::Bytes),
        crate::program::ConstEntry::Str(_) => program.const_str(id).ok().map(ConstValue::Str),
    }
}

fn fmt_reg(w: &mut fmt::Formatter<'_>, r: u32) -> fmt::Result {
    write!(w, "r{}", r)
}

fn fmt_reg_list(w: &mut fmt::Formatter<'_>, regs: &[u32]) -> fmt::Result {
    write!(w, "[")?;
    for (i, r) in regs.iter().enumerate() {
        if i != 0 {
            write!(w, ", ")?;
        }
        fmt_reg(w, *r)?;
    }
    write!(w, "]")
}

fn fmt_reg_iter(w: &mut fmt::Formatter<'_>, mut regs: RegIter<'_>) -> fmt::Result {
    write!(w, "[")?;
    if let Some(first) = regs.next() {
        fmt_reg(w, first)?;
        for r in regs {
            write!(w, ", ")?;
            fmt_reg(w, r)?;
        }
    }
    write!(w, "]")
}

impl fmt::Display for Disassembly<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, fd) in self.functions.iter().enumerate() {
            if i != 0 {
                writeln!(f)?;
            }
            writeln!(f, "func f{}:", fd.func().0)?;
            if let Some(e) = fd.error() {
                writeln!(f, "  <decode error: {e:?}>")?;
                continue;
            }

            let labels = fd.labels();

            for iv in fd.instrs() {
                let pc = iv.pc();
                if let Some(label_ix) = labels.label_index(pc) {
                    writeln!(f, "  @L{label_ix}:")?;
                }
                fmt_instr_with_labels(f, &iv, labels.pcs())?;
                writeln!(f)?;
            }
        }
        Ok(())
    }
}

fn fmt_label_ref(f: &mut fmt::Formatter<'_>, pc: u32, label_pcs: &[u32]) -> fmt::Result {
    match label_pcs.binary_search(&pc) {
        Ok(ix) => write!(f, "@L{ix}"),
        Err(_) => write!(f, "@{:06}", pc),
    }
}

fn fmt_instr_with_labels(
    f: &mut fmt::Formatter<'_>,
    iv: &InstrView<'_>,
    label_pcs: &[u32],
) -> fmt::Result {
    write!(f, "  {:06}: {}", iv.pc(), iv.opcode().mnemonic())?;
    match iv.operands() {
        Operands::Simple => {
            if let Some(dst) = iv.dst() {
                write!(f, " ")?;
                fmt_reg(f, dst)?;
            }
            let reads = iv.reads();
            if reads.clone().next().is_some() {
                write!(f, ", ")?;
                fmt_reg_iter(f, reads)?;
            }
            if let Some(ix) = iv.input_index() {
                if iv
                    .opcode()
                    .operand_kinds()
                    .iter()
                    .any(|k| matches!(k, OperandKind::ConstId))
                {
                    write!(f, ", {ix}")?;
                } else {
                    write!(f, " ; {ix}")?;
                }
            }
            if let Some(sym) = iv.host_op_symbol() {
                write!(f, " ; host=\"{sym}\"")?;
            }
            if let Some(v) = iv.const_value() {
                write!(f, " ; ")?;
                fmt_const_value(f, v)?;
            }
        }
        Operands::Br {
            cond,
            pc_true,
            pc_false,
        } => {
            write!(f, " ")?;
            fmt_reg(f, cond)?;
            write!(f, ", ")?;
            fmt_label_ref(f, pc_true, label_pcs)?;
            write!(f, ", ")?;
            fmt_label_ref(f, pc_false, label_pcs)?;
        }
        Operands::Jmp { pc_target } => {
            write!(f, " ")?;
            fmt_label_ref(f, pc_target, label_pcs)?;
        }
        Operands::Call(call) => {
            write!(f, " eff_out=")?;
            fmt_reg(f, call.eff_out)?;
            write!(f, ", ")?;
            match call.callee {
                CallTarget::Func(id) => write!(f, "f{}", id.0)?,
                CallTarget::HostSig(id, sym) => {
                    write!(f, "host_sig#{}", id.0)?;
                    if let Some(s) = sym {
                        write!(f, "(\"{s}\")")?;
                    }
                }
            }
            write!(f, ", eff_in=")?;
            fmt_reg(f, call.eff_in)?;
            write!(f, ", args=")?;
            fmt_reg_list(f, call.args)?;
            write!(f, ", rets=")?;
            fmt_reg_list(f, call.rets)?;
        }
        Operands::Ret { eff, rets } => {
            write!(f, " eff=")?;
            fmt_reg(f, eff)?;
            write!(f, ", rets=")?;
            fmt_reg_list(f, rets)?;
        }
    }
    Ok(())
}

fn fmt_const_value(f: &mut fmt::Formatter<'_>, v: ConstValue<'_>) -> fmt::Result {
    match v {
        ConstValue::Unit => write!(f, "()"),
        ConstValue::Bool(b) => write!(f, "{b}"),
        ConstValue::I64(i) => write!(f, "{i}"),
        ConstValue::U64(u) => write!(f, "{u}"),
        ConstValue::F64Bits(bits) => {
            let as_f = f64::from_bits(bits);
            write!(f, "{as_f} (bits=0x{bits:016X})")
        }
        ConstValue::Decimal { mantissa, scale } => {
            write!(f, "dec(mantissa={mantissa}, scale={scale})")
        }
        ConstValue::Bytes(b) => {
            let max = 32_usize;
            write!(f, "bytes[len={}]", b.len())?;
            write!(f, " 0x")?;
            for (i, byte) in b.iter().take(max).enumerate() {
                let _ = i;
                write!(f, "{byte:02X}")?;
            }
            if b.len() > max {
                write!(f, "…")?;
            }
            Ok(())
        }
        ConstValue::Str(s) => {
            let max = 64_usize;
            let shown: String = s.chars().take(max).collect();
            if s.chars().nth(max).is_some() {
                write!(f, "str[len={}] \"{}…\"", s.len(), shown.escape_default())
            } else {
                write!(f, "str[len={}] \"{}\"", s.len(), shown.escape_default())
            }
        }
    }
}

impl fmt::Display for InstrView<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:06}: {}", self.pc(), self.opcode().mnemonic())?;
        match self.operands() {
            Operands::Simple => {
                if let Some(dst) = self.dst() {
                    write!(f, " ")?;
                    fmt_reg(f, dst)?;
                }
                let reads = self.reads();
                if reads.clone().next().is_some() {
                    write!(f, ", ")?;
                    fmt_reg_iter(f, reads)?;
                }
                if let Some(ix) = self.input_index() {
                    if self
                        .opcode()
                        .operand_kinds()
                        .iter()
                        .any(|k| matches!(k, OperandKind::ConstId))
                    {
                        write!(f, ", {ix}")?;
                    } else {
                        write!(f, " ; {ix}")?;
                    }
                }
                if let Some(sym) = self.host_op_symbol() {
                    write!(f, " ; host=\"{sym}\"")?;
                }
                if let Some(v) = self.const_value() {
                    write!(f, " ; ")?;
                    fmt_const_value(f, v)?;
                }
            }
            Operands::Br {
                cond,
                pc_true,
                pc_false,
            } => {
                write!(f, " ")?;
                fmt_reg(f, cond)?;
                write!(f, ", @{:06}, @{:06}", pc_true, pc_false)?;
            }
            Operands::Jmp { pc_target } => {
                write!(f, " @{:06}", pc_target)?;
            }
            Operands::Call(call) => {
                write!(f, " eff_out=")?;
                fmt_reg(f, call.eff_out)?;
                write!(f, ", ")?;
                match call.callee {
                    CallTarget::Func(id) => write!(f, "f{}", id.0)?,
                    CallTarget::HostSig(id, sym) => {
                        write!(f, "host_sig#{}", id.0)?;
                        if let Some(s) = sym {
                            write!(f, "(\"{s}\")")?;
                        }
                    }
                }
                write!(f, ", eff_in=")?;
                fmt_reg(f, call.eff_in)?;
                write!(f, ", args=")?;
                fmt_reg_list(f, call.args)?;
                write!(f, ", rets=")?;
                fmt_reg_list(f, call.rets)?;
            }
            Operands::Ret { eff, rets } => {
                write!(f, " eff=")?;
                fmt_reg(f, eff)?;
                write!(f, ", rets=")?;
                fmt_reg_list(f, rets)?;
            }
        }
        Ok(())
    }
}

impl fmt::Display for InputIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::TrapCode(code) => write!(f, "trap_code={code}"),
            Self::Const(id) => write!(f, "const#{}", id.0),
            Self::HostSig(id) => write!(f, "host_sig#{}", id.0),
            Self::Func(id) => write!(f, "func#{}", id.0),
            Self::Type(id) => write!(f, "type#{}", id.0),
            Self::ElemType(id) => write!(f, "elem_type#{}", id.0),
            Self::Index(ix) => write!(f, "index={ix}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asm::{Asm, FunctionSig, ProgramBuilder};
    use crate::program::ValueType;
    use alloc::string::ToString;
    use alloc::vec;

    #[test]
    fn disasm_formats_labels_for_branches() {
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
        let vp = pb.build_verified().unwrap();

        let dis = disassemble(vp.program());
        let text = dis.to_string();
        assert!(text.contains("@L"));
        assert!(text.contains("br"));
    }

    #[test]
    fn function_labels_map_pcs_to_indices() {
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
        let vp = pb.build_verified().unwrap();

        let fd = disassemble_function(vp.program(), FuncId(0)).unwrap();
        let labels = fd.labels();
        assert_eq!(labels.label_index(0), Some(0));

        let mut targets = None;
        for iv in fd.instrs() {
            if let Operands::Br {
                pc_true, pc_false, ..
            } = iv.operands()
            {
                targets = Some((pc_true, pc_false));
                break;
            }
        }
        let (pc_true, pc_false) = targets.expect("expected br");

        assert!(labels.label_index(pc_true).is_some());
        assert!(labels.label_index(pc_false).is_some());
    }

    #[test]
    fn instr_view_is_terminator_matches_opcode() {
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
        let vp = pb.build_verified().unwrap();

        let fd = disassemble_function(vp.program(), FuncId(0)).unwrap();
        let mut it = fd.instrs();
        let first = it.next().expect("expected const");
        assert!(!first.is_terminator());
        let second = it.next().expect("expected ret");
        assert!(second.is_terminator());
    }
}
