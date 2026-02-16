// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Program verification for `execution_tape`.
//!
//! v1 verification is responsible for rejecting malformed or unsafe-to-execute programs before
//! they reach the interpreter. It is intentionally split into:
//! - container-level checks (bounds, consistency, monotonic tables)
//! - bytecode-level checks (CFG, init-before-use, type discipline) (future tickets)

use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use crate::analysis::cfg::BasicBlock;
use crate::analysis::dataflow;
use crate::analysis::liveness;
use crate::bytecode::{DecodedInstr, Instr, decode_instructions};
use crate::format::DecodeError;
use crate::host::sig_hash_slices;
use crate::instr_operands;
use crate::opcode::Opcode;
use crate::program::{
    ConstEntry, ElemTypeId, Function, Program, SpanEntry, SpanId, SymbolId, TypeId, ValueType,
};
use crate::typed::{
    AggReg, BoolReg, BytesReg, DecimalReg, ExecDecoded, ExecFunc, ExecInstr, F64Reg, FuncReg,
    I64Reg, ObjReg, RegClass, RegCounts, RegLayout, StrReg, U64Reg, UnitReg, VReg, VRegSlice,
};
use crate::value::FuncId;
use crate::{analysis::bitset::BitSet, analysis::cfg};

#[cfg(doc)]
use crate::vm::Vm;

/// A program that has been verified under a particular verifier configuration.
///
/// This is an API affordance for embedders: it allows a VM to expose
/// [`Vm::run`], which can assume
/// (and potentially optimize around) verifier-enforced invariants from [`VerifyConfig`], while
/// still validating host ABI conformance at runtime.
///
/// Internally, a [`VerifiedProgram`] also carries a decoded instruction stream so the VM does not
/// need to decode bytecode at runtime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedProgram {
    program: Program,
    verified_functions: Vec<ExecFunc>,
}

/// A non-fatal verifier lint warning.
///
/// Lints are best-effort: they are intended to help frontend authors diagnose low-quality
/// bytecode, without changing the acceptance criteria of verification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VerifyLint {
    /// A basic block is unreachable from function entry.
    UnreachableBlock {
        /// Function index within the program.
        func: u32,
        /// Byte offset of the basic block start.
        pc: u32,
    },
    /// An instruction writes a register whose value is never read.
    DeadStore {
        /// Function index within the program.
        func: u32,
        /// Byte offset of the instruction.
        pc: u32,
        /// Written register.
        reg: u32,
    },
    /// A `mov` is redundant (e.g. `mov rX, rX`).
    RedundantMove {
        /// Function index within the program.
        func: u32,
        /// Byte offset of the instruction.
        pc: u32,
        /// Destination register.
        dst: u32,
        /// Source register.
        src: u32,
    },
    /// A `call`/`host_call` return register is never read.
    UnusedCallReturn {
        /// Function index within the program.
        func: u32,
        /// Byte offset of the instruction.
        pc: u32,
        /// Return register.
        reg: u32,
    },
}

impl VerifiedProgram {
    /// Returns the underlying program.
    #[must_use]
    pub fn program(&self) -> &Program {
        &self.program
    }

    #[must_use]
    pub(crate) fn verified(&self, func: FuncId) -> Option<&ExecFunc> {
        self.verified_functions.get(func.0 as usize)
    }

    /// Consumes `self` and returns the underlying program.
    #[must_use]
    pub fn into_program(self) -> Program {
        self.program
    }
}

/// A verification error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VerifyError {
    /// The program failed to decode.
    Decode(DecodeError),
    /// Internal error: opcode schema does not match the decoded instruction shape.
    ///
    /// This should never occur for valid `execution_tape` builds, and indicates drift between
    /// opcode metadata and the bytecode decoder.
    InternalOpcodeSchemaMismatch {
        /// Function index within the program.
        func: u32,
        /// Byte offset of the instruction.
        pc: u32,
        /// Opcode byte.
        opcode: u8,
    },
    /// A function references an out-of-bounds byte range.
    FunctionBytecodeOutOfBounds {
        /// Function index within the program.
        func: u32,
    },
    /// A function references an out-of-bounds span range.
    FunctionSpansOutOfBounds {
        /// Function index within the program.
        func: u32,
    },
    /// A function references an out-of-bounds argument type range.
    FunctionArgTypesOutOfBounds {
        /// Function index within the program.
        func: u32,
    },
    /// A function references an out-of-bounds return type range.
    FunctionRetTypesOutOfBounds {
        /// Function index within the program.
        func: u32,
    },
    /// A function signature does not match its declared counts.
    FunctionSigCountMismatch {
        /// Function index within the program.
        func: u32,
    },
    /// A function input-name entry references an invalid argument index.
    FunctionInputNameArgOutOfBounds {
        /// Function index within the program.
        func: u32,
        /// Argument index within the function signature.
        arg: u32,
    },
    /// A function input name is empty.
    FunctionInputNameEmpty {
        /// Function index within the program.
        func: u32,
        /// Argument index within the function signature.
        arg: u32,
    },
    /// A function input name is duplicated within the same function signature.
    FunctionInputNameDuplicate {
        /// Function index within the program.
        func: u32,
        /// Symbol id naming the duplicated input.
        name: SymbolId,
    },
    /// A function declares multiple names for the same argument index.
    FunctionInputNameDuplicateArgIndex {
        /// Function index within the program.
        func: u32,
        /// Argument index within the function signature.
        arg: u32,
    },
    /// A function output-name entry references an invalid return index.
    FunctionOutputNameRetOutOfBounds {
        /// Function index within the program.
        func: u32,
        /// Return index within the function signature.
        ret: u32,
    },
    /// A function output name is empty.
    FunctionOutputNameEmpty {
        /// Function index within the program.
        func: u32,
        /// Return index within the function signature.
        ret: u32,
    },
    /// A function output name is duplicated within the same function signature.
    FunctionOutputNameDuplicate {
        /// Function index within the program.
        func: u32,
        /// Symbol id naming the duplicated output.
        name: SymbolId,
    },
    /// A function declares multiple names for the same return index.
    FunctionOutputNameDuplicateRetIndex {
        /// Function index within the program.
        func: u32,
        /// Return index within the function signature.
        ret: u32,
    },
    /// A function span table has a bad `pc_delta` sequence.
    ///
    /// v1 requires `pc_delta` values to be non-zero after the first entry (to ensure progress)
    /// and for the cumulative pc to stay within the function bytecode length.
    BadSpanDeltas {
        /// Function index within the program.
        func: u32,
    },
    /// A function declares an absurd register count (implementation-defined upper bound).
    RegCountTooLarge {
        /// Function index within the program.
        func: u32,
        /// Declared total register count.
        reg_count: u32,
    },
    /// A function declares more value arguments than it has registers for.
    ArgCountExceedsRegs {
        /// Function index within the program.
        func: u32,
    },
    /// A bytecode jump target is invalid.
    InvalidJumpTarget {
        /// Function index within the program.
        func: u32,
        /// Byte offset of the jump instruction.
        pc: u32,
        /// Byte offset target.
        target: u32,
        /// Why the jump target is invalid.
        reason: InvalidJumpTargetReason,
    },
    /// An effect-token input register was not `r0`.
    EffectInNotR0 {
        /// Function index within the program.
        func: u32,
        /// Byte offset of the instruction.
        pc: u32,
        /// The offending register.
        reg: u32,
    },
    /// An effect-token output register was not `r0`.
    EffectOutNotR0 {
        /// Function index within the program.
        func: u32,
        /// Byte offset of the instruction.
        pc: u32,
        /// The offending register.
        reg: u32,
    },
    /// A register index is out of bounds for the function's `reg_count`.
    RegOutOfBounds {
        /// Function index within the program.
        func: u32,
        /// Byte offset of the instruction.
        pc: u32,
        /// Register index.
        reg: u32,
    },
    /// A register was read before being initialized along some reachable path.
    UninitializedRead {
        /// Function index within the program.
        func: u32,
        /// Byte offset of the instruction.
        pc: u32,
        /// Register index.
        reg: u32,
    },
    /// A `call` instruction does not match the callee function's signature counts.
    CallArityMismatch {
        /// Function index within the program.
        func: u32,
        /// Byte offset of the instruction.
        pc: u32,
    },
    /// A `ret` instruction does not match the function's return type count.
    ReturnArityMismatch {
        /// Function index within the program.
        func: u32,
        /// Byte offset of the instruction.
        pc: u32,
    },
    /// A `host_call` instruction does not match the host signature's counts.
    HostCallArityMismatch {
        /// Function index within the program.
        func: u32,
        /// Byte offset of the instruction.
        pc: u32,
    },
    /// An instruction references an out-of-bounds function id.
    ///
    /// This applies to any instruction carrying a `FuncId` immediate (e.g. `call`, `const.func`).
    FuncIdOutOfBounds {
        /// Function index within the program.
        func: u32,
        /// Byte offset of the instruction.
        pc: u32,
        /// Referenced function id.
        func_id: u32,
    },
    /// A `struct_new` references an unknown struct `type_id`.
    StructTypeOutOfBounds {
        /// Function index within the program.
        func: u32,
        /// Byte offset of the instruction.
        pc: u32,
        /// Struct type id.
        type_id: u32,
    },
    /// A `struct_new` value count does not match the struct field count.
    StructArityMismatch {
        /// Function index within the program.
        func: u32,
        /// Byte offset of the instruction.
        pc: u32,
        /// Struct type id.
        type_id: u32,
    },
    /// An `array_new` references an unknown `elem_type_id`.
    ArrayElemTypeOutOfBounds {
        /// Function index within the program.
        func: u32,
        /// Byte offset of the instruction.
        pc: u32,
        /// Element type id.
        elem_type_id: u32,
    },
    /// An `array_new` length does not match the provided values.
    ArrayLenMismatch {
        /// Function index within the program.
        func: u32,
        /// Byte offset of the instruction.
        pc: u32,
    },
    /// A projection opcode referenced an aggregate of the wrong kind (when the verifier can
    /// statically determine the aggregate kind).
    AggKindMismatch {
        /// Function index within the program.
        func: u32,
        /// Byte offset of the instruction.
        pc: u32,
        /// Expected aggregate kind.
        expected: AggKind,
        /// Actual aggregate kind.
        actual: AggKind,
    },
    /// A `tuple_get` index is out of bounds (when the verifier can statically determine arity).
    TupleIndexOutOfBounds {
        /// Function index within the program.
        func: u32,
        /// Byte offset of the instruction.
        pc: u32,
        /// Tuple arity.
        arity: u32,
        /// Index immediate.
        index: u32,
    },
    /// A `struct_get` field index is out of bounds (when the verifier can statically determine the struct type).
    StructFieldIndexOutOfBounds {
        /// Function index within the program.
        func: u32,
        /// Byte offset of the instruction.
        pc: u32,
        /// Struct type id.
        type_id: u32,
        /// Field index immediate.
        field_index: u32,
    },
    /// A `host_call` references an out-of-bounds host signature id.
    HostSigOutOfBounds {
        /// Function index within the program.
        func: u32,
        /// Byte offset of the instruction.
        pc: u32,
        /// Host signature id.
        host_sig: u32,
    },
    /// A `const_pool` instruction references an out-of-bounds constant.
    ConstOutOfBounds {
        /// Function index within the program.
        func: u32,
        /// Byte offset of the instruction.
        pc: u32,
        /// Constant id.
        const_id: u32,
    },
    /// A host signature table entry is malformed.
    HostSigMalformed {
        /// Host signature id.
        host_sig: u32,
    },
    /// A `host_call` referenced a host signature, but that entry was malformed.
    HostCallSigMalformed {
        /// Function index within the program.
        func: u32,
        /// Byte offset of the instruction.
        pc: u32,
        /// Host signature id.
        host_sig: u32,
    },
    /// The program's host signature hash does not match the canonical hash for its types.
    HostSigHashMismatch {
        /// Host signature id.
        host_sig: u32,
    },
    /// A typed use requires a concrete type, but the value's type is not known.
    UnknownTypeAtUse {
        /// Function index within the program.
        func: u32,
        /// Byte offset of the instruction.
        pc: u32,
        /// Register index.
        reg: u32,
        /// Expected type.
        expected: ValueType,
    },
    /// A `select` operand has no stable concrete type.
    UnknownTypeAtSelect {
        /// Function index within the program.
        func: u32,
        /// Byte offset of the instruction.
        pc: u32,
        /// Register index.
        reg: u32,
    },
    /// A register has no stable concrete type across reachable paths.
    UnstableRegType {
        /// Function index within the program.
        func: u32,
        /// Register index.
        reg: u32,
    },
    /// A register has no stable concrete type (with instruction context).
    UnstableRegTypeAt {
        /// Function index within the program.
        func: u32,
        /// Byte offset of the instruction.
        pc: u32,
        /// Register index.
        reg: u32,
    },
    /// A `call` referenced a callee arg type slice that was out of bounds.
    CallCalleeArgTypesOutOfBounds {
        /// Function index within the program.
        func: u32,
        /// Byte offset of the instruction.
        pc: u32,
        /// Callee function id.
        callee: u32,
    },
    /// A `call` referenced a callee ret type slice that was out of bounds.
    CallCalleeRetTypesOutOfBounds {
        /// Function index within the program.
        func: u32,
        /// Byte offset of the instruction.
        pc: u32,
        /// Callee function id.
        callee: u32,
    },
    /// A typed use saw a concrete type that does not match what was expected.
    TypeMismatch {
        /// Function index within the program.
        func: u32,
        /// Byte offset of the instruction.
        pc: u32,
        /// Expected type.
        expected: ValueType,
        /// Actual type.
        actual: ValueType,
    },
    /// A basic block can fall through to the next block without an explicit terminator.
    MissingTerminator {
        /// Function index within the program.
        func: u32,
        /// Byte offset of the last instruction in the block.
        pc: u32,
    },
    /// Internal inconsistency between decoded instructions and verifier-computed basic blocks.
    ///
    /// This should be impossible for well-formed verifier code; treat as a verifier bug.
    InternalBlockInconsistent {
        /// Function index within the program.
        func: u32,
        /// Byte offset of the inconsistent block start.
        pc: u32,
        /// Instruction index range start.
        instr_start: usize,
        /// Instruction index range end.
        instr_end: usize,
    },
    /// Bytecode decoding failed.
    BytecodeDecode {
        /// Function index within the program.
        func: u32,
    },
}

/// Why a jump target is invalid.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum InvalidJumpTargetReason {
    /// The target is greater than or equal to the function bytecode length.
    OutOfRange,
    /// The target does not land on an instruction boundary.
    NotInstructionBoundary,
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decode(e) => write!(f, "decode failed: {e}"),
            Self::InternalOpcodeSchemaMismatch { func, pc, opcode } => write!(
                f,
                "internal opcode schema mismatch: function {func} pc={pc} opcode=0x{opcode:02X}"
            ),
            Self::FunctionBytecodeOutOfBounds { func } => {
                write!(f, "function {func} bytecode out of bounds")
            }
            Self::FunctionSpansOutOfBounds { func } => {
                write!(f, "function {func} spans out of bounds")
            }
            Self::FunctionArgTypesOutOfBounds { func } => {
                write!(f, "function {func} arg types out of bounds")
            }
            Self::FunctionRetTypesOutOfBounds { func } => {
                write!(f, "function {func} ret types out of bounds")
            }
            Self::FunctionSigCountMismatch { func } => {
                write!(f, "function {func} signature count mismatch")
            }
            Self::FunctionInputNameArgOutOfBounds { func, arg } => {
                write!(
                    f,
                    "function {func} input name arg index out of bounds: {arg}"
                )
            }
            Self::FunctionInputNameEmpty { func, arg } => {
                write!(f, "function {func} input name is empty for arg {arg}")
            }
            Self::FunctionInputNameDuplicate { func, name } => {
                write!(
                    f,
                    "function {func} input name is duplicated (symbol_id={})",
                    name.0
                )
            }
            Self::FunctionInputNameDuplicateArgIndex { func, arg } => {
                write!(
                    f,
                    "function {func} input name arg index is duplicated: {arg}"
                )
            }
            Self::FunctionOutputNameRetOutOfBounds { func, ret } => {
                write!(
                    f,
                    "function {func} output name ret index out of bounds: {ret}"
                )
            }
            Self::FunctionOutputNameEmpty { func, ret } => {
                write!(f, "function {func} output name is empty for ret {ret}")
            }
            Self::FunctionOutputNameDuplicate { func, name } => {
                write!(
                    f,
                    "function {func} output name is duplicated (symbol_id={})",
                    name.0
                )
            }
            Self::FunctionOutputNameDuplicateRetIndex { func, ret } => {
                write!(
                    f,
                    "function {func} output name ret index is duplicated: {ret}"
                )
            }
            Self::BadSpanDeltas { func } => {
                write!(f, "function {func} span table has bad pc_deltas")
            }
            Self::RegCountTooLarge { func, reg_count } => {
                write!(f, "function {func} reg_count {reg_count} is too large")
            }
            Self::ArgCountExceedsRegs { func } => {
                write!(f, "function {func} arg_count exceeds reg_count")
            }
            Self::InvalidJumpTarget {
                func,
                pc,
                target,
                reason,
            } => write!(
                f,
                "function {func} pc={pc} invalid jump target pc={target} ({reason:?})"
            ),
            Self::EffectInNotR0 { func, pc, reg } => {
                write!(
                    f,
                    "function {func} pc={pc} effect input must be r0 (got r{reg})"
                )
            }
            Self::EffectOutNotR0 { func, pc, reg } => {
                write!(
                    f,
                    "function {func} pc={pc} effect output must be r0 (got r{reg})"
                )
            }
            Self::RegOutOfBounds { func, pc, reg } => {
                write!(f, "function {func} pc={pc} register out of bounds: r{reg}")
            }
            Self::UninitializedRead { func, pc, reg } => {
                write!(f, "function {func} pc={pc} uninitialized read: r{reg}")
            }
            Self::CallArityMismatch { func, pc } => {
                write!(f, "function {func} pc={pc} call arity mismatch")
            }
            Self::ReturnArityMismatch { func, pc } => {
                write!(f, "function {func} pc={pc} return arity mismatch")
            }
            Self::HostCallArityMismatch { func, pc } => {
                write!(f, "function {func} pc={pc} host_call arity mismatch")
            }
            Self::FuncIdOutOfBounds { func, pc, func_id } => {
                write!(
                    f,
                    "function {func} pc={pc} function id out of bounds: func#{func_id}"
                )
            }
            Self::StructTypeOutOfBounds { func, pc, type_id } => {
                write!(
                    f,
                    "function {func} pc={pc} struct type_id out of bounds: {type_id}"
                )
            }
            Self::StructArityMismatch { func, pc, type_id } => {
                write!(
                    f,
                    "function {func} pc={pc} struct arity mismatch for type_id={type_id}"
                )
            }
            Self::ArrayElemTypeOutOfBounds {
                func,
                pc,
                elem_type_id,
            } => write!(
                f,
                "function {func} pc={pc} array elem_type_id out of bounds: {elem_type_id}"
            ),
            Self::ArrayLenMismatch { func, pc } => {
                write!(f, "function {func} pc={pc} array len mismatch")
            }
            Self::StructFieldIndexOutOfBounds {
                func,
                pc,
                type_id,
                field_index,
            } => write!(
                f,
                "function {func} pc={pc} struct field_index out of bounds (type_id={type_id}, field_index={field_index})"
            ),
            Self::TupleIndexOutOfBounds {
                func,
                pc,
                arity,
                index,
            } => write!(
                f,
                "function {func} pc={pc} tuple index out of bounds (arity {arity}, index {index})"
            ),
            Self::ConstOutOfBounds { func, pc, const_id } => {
                write!(
                    f,
                    "function {func} pc={pc} const id out of bounds: {const_id}"
                )
            }
            Self::HostSigOutOfBounds { func, pc, host_sig } => {
                write!(
                    f,
                    "function {func} pc={pc} host_sig out of bounds: {host_sig}"
                )
            }
            Self::HostSigMalformed { host_sig } => write!(f, "host_sig {host_sig} malformed"),
            Self::HostCallSigMalformed { func, pc, host_sig } => write!(
                f,
                "function {func} pc={pc} host_sig {host_sig} malformed (via host_call)"
            ),
            Self::HostSigHashMismatch { host_sig } => {
                write!(f, "host_sig {host_sig} sig_hash mismatch")
            }
            Self::UnknownTypeAtUse {
                func,
                pc,
                reg,
                expected,
            } => write!(
                f,
                "function {func} pc={pc} unknown type at use (r{reg}, expected {expected:?})"
            ),
            Self::UnknownTypeAtSelect { func, pc, reg } => write!(
                f,
                "function {func} pc={pc} unknown type at select operand (r{reg})"
            ),
            Self::UnstableRegType { func, reg } => write!(
                f,
                "function {func}: reg {reg} has no stable concrete type across reachable paths"
            ),
            Self::UnstableRegTypeAt { func, pc, reg } => write!(
                f,
                "function {func} pc={pc}: reg {reg} has no stable concrete type across reachable paths"
            ),
            Self::CallCalleeArgTypesOutOfBounds { func, pc, callee } => write!(
                f,
                "function {func} pc={pc} callee f{callee} arg types out of bounds"
            ),
            Self::CallCalleeRetTypesOutOfBounds { func, pc, callee } => write!(
                f,
                "function {func} pc={pc} callee f{callee} ret types out of bounds"
            ),
            Self::TypeMismatch {
                func,
                pc,
                expected,
                actual,
            } => write!(
                f,
                "function {func} pc={pc} type mismatch (expected {expected:?}, got {actual:?})"
            ),
            Self::MissingTerminator { func, pc } => write!(
                f,
                "function {func} pc={pc} block can fall through without a terminator"
            ),
            Self::InternalBlockInconsistent {
                func,
                pc,
                instr_start,
                instr_end,
            } => write!(
                f,
                "function {func} pc={pc} internal verifier error: inconsistent basic blocks (instrs {instr_start}..{instr_end})"
            ),
            Self::BytecodeDecode { func } => write!(f, "function {func} bytecode decode failed"),
            Self::AggKindMismatch {
                func,
                pc,
                expected,
                actual,
            } => write!(
                f,
                "function {func} pc={pc} aggregate kind mismatch (expected {expected:?}, got {actual:?})"
            ),
        }
    }
}

impl core::error::Error for VerifyError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Decode(e) => Some(e),
            _ => None,
        }
    }
}

/// Aggregate kind (tuple/struct/array) tracked by the verifier.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AggKind {
    /// Tuple aggregate.
    Tuple,
    /// Struct aggregate.
    Struct,
    /// Array aggregate.
    Array,
}

impl From<DecodeError> for VerifyError {
    fn from(e: DecodeError) -> Self {
        Self::Decode(e)
    }
}

/// Verifier configuration and limits.
#[derive(Clone, Debug)]
pub struct VerifyConfig {
    /// Maximum allowed registers per function.
    pub max_regs_per_function: u32,
}

impl Default for VerifyConfig {
    fn default() -> Self {
        Self {
            max_regs_per_function: 65_536,
        }
    }
}

/// Verifies `program` according to v1 container-level rules.
pub fn verify_program(program: &Program, cfg: &VerifyConfig) -> Result<(), VerifyError> {
    verify_host_sigs(program)?;
    verify_function_value_names(program)?;

    for (i, func) in program.functions.iter().enumerate() {
        let func_id = u32::try_from(i).unwrap_or(u32::MAX);
        let _ = verify_function_container(program, func_id, func, cfg)?.exec;
    }
    Ok(())
}

/// Verifies `program` and also computes non-fatal lint warnings.
pub fn verify_program_with_lints(
    program: &Program,
    cfg: &VerifyConfig,
) -> Result<Vec<VerifyLint>, VerifyError> {
    verify_host_sigs(program)?;
    verify_function_value_names(program)?;

    let mut lints: Vec<VerifyLint> = Vec::new();
    for (i, func) in program.functions.iter().enumerate() {
        let func_id = u32::try_from(i).unwrap_or(u32::MAX);
        let out = verify_function_container(program, func_id, func, cfg)?;
        lints.extend(out.lints);
    }
    Ok(lints)
}

/// Verifies `program` and returns a [`VerifiedProgram`] wrapper on success.
pub fn verify_program_owned(
    program: Program,
    cfg: &VerifyConfig,
) -> Result<VerifiedProgram, VerifyError> {
    verify_host_sigs(&program)?;
    verify_function_value_names(&program)?;

    let mut verified_functions: Vec<ExecFunc> = Vec::with_capacity(program.functions.len());
    for (i, func) in program.functions.iter().enumerate() {
        let func_id = u32::try_from(i).unwrap_or(u32::MAX);
        verified_functions.push(verify_function_container(&program, func_id, func, cfg)?.exec);
    }

    Ok(VerifiedProgram {
        program,
        verified_functions,
    })
}

/// Verifies `program`, returning the [`VerifiedProgram`] plus non-fatal lint warnings.
pub fn verify_program_owned_with_lints(
    program: Program,
    cfg: &VerifyConfig,
) -> Result<(VerifiedProgram, Vec<VerifyLint>), VerifyError> {
    verify_host_sigs(&program)?;
    verify_function_value_names(&program)?;

    let mut verified_functions: Vec<ExecFunc> = Vec::with_capacity(program.functions.len());
    let mut lints: Vec<VerifyLint> = Vec::new();
    for (i, func) in program.functions.iter().enumerate() {
        let func_id = u32::try_from(i).unwrap_or(u32::MAX);
        let out = verify_function_container(&program, func_id, func, cfg)?;
        verified_functions.push(out.exec);
        lints.extend(out.lints);
    }

    Ok((
        VerifiedProgram {
            program,
            verified_functions,
        },
        lints,
    ))
}

struct ExecFuncContainer {
    exec: ExecFunc,
    lints: Vec<VerifyLint>,
}

fn verify_function_value_names(program: &Program) -> Result<(), VerifyError> {
    for (func_id, func) in program.functions.iter().enumerate() {
        let func_id = u32::try_from(func_id).unwrap_or(u32::MAX);

        let arg_names = program
            .function_arg_name_ids(func)
            .map_err(VerifyError::Decode)?;
        for (arg, &name_id) in arg_names.iter().enumerate() {
            let arg = u32::try_from(arg).unwrap_or(u32::MAX);
            if name_id == 0 {
                continue;
            }
            let sym = SymbolId(
                core::num::NonZeroU32::new(name_id)
                    .ok_or(VerifyError::Decode(DecodeError::OutOfBounds))?,
            );
            let name = program.symbol_str(sym).map_err(VerifyError::Decode)?;
            if name.is_empty() {
                return Err(VerifyError::FunctionInputNameEmpty { func: func_id, arg });
            }
        }
        for (i, &a) in arg_names.iter().enumerate() {
            if a == 0 {
                continue;
            }
            for &b in &arg_names[..i] {
                if b == a {
                    return Err(VerifyError::FunctionInputNameDuplicate {
                        func: func_id,
                        name: SymbolId(
                            core::num::NonZeroU32::new(a)
                                .ok_or(VerifyError::Decode(DecodeError::OutOfBounds))?,
                        ),
                    });
                }
            }
        }

        let ret_names = program
            .function_ret_name_ids(func)
            .map_err(VerifyError::Decode)?;
        for (ret, &name_id) in ret_names.iter().enumerate() {
            let ret = u32::try_from(ret).unwrap_or(u32::MAX);
            if name_id == 0 {
                continue;
            }
            let sym = SymbolId(
                core::num::NonZeroU32::new(name_id)
                    .ok_or(VerifyError::Decode(DecodeError::OutOfBounds))?,
            );
            let name = program.symbol_str(sym).map_err(VerifyError::Decode)?;
            if name.is_empty() {
                return Err(VerifyError::FunctionOutputNameEmpty { func: func_id, ret });
            }
        }
        for (i, &a) in ret_names.iter().enumerate() {
            if a == 0 {
                continue;
            }
            for &b in &ret_names[..i] {
                if b == a {
                    return Err(VerifyError::FunctionOutputNameDuplicate {
                        func: func_id,
                        name: SymbolId(
                            core::num::NonZeroU32::new(a)
                                .ok_or(VerifyError::Decode(DecodeError::OutOfBounds))?,
                        ),
                    });
                }
            }
        }
    }

    Ok(())
}

fn verify_host_sigs(program: &Program) -> Result<(), VerifyError> {
    for (i, hs) in program.host_sigs.iter().enumerate() {
        let host_sig = u32::try_from(i).unwrap_or(u32::MAX);
        if (hs.symbol.index() as usize) >= program.symbols.len() {
            return Err(VerifyError::HostSigMalformed { host_sig });
        }
        let args = program
            .host_sig_args(hs)
            .map_err(|_| VerifyError::HostSigMalformed { host_sig })?;
        let rets = program
            .host_sig_rets(hs)
            .map_err(|_| VerifyError::HostSigMalformed { host_sig })?;
        if hs.sig_hash != sig_hash_slices(args, rets) {
            return Err(VerifyError::HostSigHashMismatch { host_sig });
        }
    }
    Ok(())
}

fn verify_function_container(
    program: &Program,
    func_id: u32,
    func: &Function,
    cfg: &VerifyConfig,
) -> Result<ExecFuncContainer, VerifyError> {
    if func.reg_count > cfg.max_regs_per_function {
        return Err(VerifyError::RegCountTooLarge {
            func: func_id,
            reg_count: func.reg_count,
        });
    }

    let bytecode = func
        .bytecode(program)
        .map_err(|_| VerifyError::FunctionBytecodeOutOfBounds { func: func_id })?;
    let spans = func
        .spans(program)
        .map_err(|_| VerifyError::FunctionSpansOutOfBounds { func: func_id })?;
    let arg_types = func
        .arg_types(program)
        .map_err(|_| VerifyError::FunctionArgTypesOutOfBounds { func: func_id })?;
    let ret_types = func
        .ret_types(program)
        .map_err(|_| VerifyError::FunctionRetTypesOutOfBounds { func: func_id })?;

    if u32::try_from(arg_types.len()).ok() != Some(func.arg_count)
        || u32::try_from(ret_types.len()).ok() != Some(func.ret_count)
    {
        return Err(VerifyError::FunctionSigCountMismatch { func: func_id });
    }

    verify_span_table(bytecode.len() as u64, spans)
        .map_err(|_| VerifyError::BadSpanDeltas { func: func_id })?;

    let decoded =
        decode_instructions(bytecode).map_err(|_| VerifyError::BytecodeDecode { func: func_id })?;
    for di in &decoded {
        let op = Opcode::from_u8(di.opcode).expect("decoder only emits known opcodes");
        let schema = di.instr.operand_schema();
        let op_schema = op.operands();
        let mut ok = schema.len() == op_schema.len();
        if ok {
            for (i, operand) in schema.iter().enumerate() {
                if op_schema[i].kind != operand.kind
                    || op_schema[i].role != operand.role
                    || op_schema[i].encoding != operand.encoding
                {
                    ok = false;
                    break;
                }
            }
        }
        if !ok {
            return Err(VerifyError::InternalOpcodeSchemaMismatch {
                func: func_id,
                pc: di.offset,
                opcode: di.opcode,
            });
        }
    }
    verify_id_operands_in_bounds(program, func_id, &decoded)?;
    verify_function_bytecode(
        program, func_id, func, bytecode, spans, arg_types, ret_types, &decoded,
    )
}

fn verify_id_operands_in_bounds(
    program: &Program,
    func: u32,
    decoded: &[DecodedInstr],
) -> Result<(), VerifyError> {
    for di in decoded {
        let pc = di.offset;

        let mut err: Option<VerifyError> = None;

        instr_operands::visit_const_ids(&di.instr, |id| {
            if err.is_some() {
                return;
            }
            if program.const_pool.get(id.0 as usize).is_none() {
                err = Some(VerifyError::ConstOutOfBounds {
                    func,
                    pc,
                    const_id: id.0,
                });
            }
        });

        instr_operands::visit_host_sig_ids(&di.instr, |id| {
            if err.is_some() {
                return;
            }
            if program.host_sig(id).is_none() {
                err = Some(VerifyError::HostSigOutOfBounds {
                    func,
                    pc,
                    host_sig: id.0,
                });
            }
        });

        instr_operands::visit_type_ids(&di.instr, |id| {
            if err.is_some() {
                return;
            }
            if program.types.structs.get(id.0 as usize).is_none() {
                err = Some(VerifyError::StructTypeOutOfBounds {
                    func,
                    pc,
                    type_id: id.0,
                });
            }
        });

        instr_operands::visit_elem_type_ids(&di.instr, |id| {
            if err.is_some() {
                return;
            }
            if program.types.array_elems.get(id.0 as usize).is_none() {
                err = Some(VerifyError::ArrayElemTypeOutOfBounds {
                    func,
                    pc,
                    elem_type_id: id.0,
                });
            }
        });

        instr_operands::visit_func_ids(&di.instr, |id| {
            if err.is_some() {
                return;
            }
            if program.functions.get(id.0 as usize).is_none() {
                err = Some(VerifyError::FuncIdOutOfBounds {
                    func,
                    pc,
                    func_id: id.0,
                });
            }
        });

        if let Some(err) = err {
            return Err(err);
        }
    }

    Ok(())
}

fn verify_span_table(bytecode_len: u64, spans: &[SpanEntry]) -> Result<(), ()> {
    let mut pc: u64 = 0;
    for (i, s) in spans.iter().enumerate() {
        if i != 0 && s.pc_delta == 0 {
            return Err(());
        }
        pc = pc.checked_add(s.pc_delta).ok_or(())?;
        if pc > bytecode_len {
            return Err(());
        }
    }
    Ok(())
}

fn build_span_by_instr_ix(decoded: &[DecodedInstr], spans: &[SpanEntry]) -> Vec<Option<SpanId>> {
    let mut by_ix: Vec<Option<SpanId>> = Vec::with_capacity(decoded.len());
    let mut span_ix: usize = 0;
    let mut span_pc: u64 = 0;
    let mut cur_span: Option<SpanId> = None;

    for di in decoded {
        let instr_pc = u64::from(di.offset);
        while let Some(s) = spans.get(span_ix) {
            let next_pc = span_pc.saturating_add(s.pc_delta);
            if next_pc > instr_pc {
                break;
            }
            span_pc = next_pc;
            cur_span = Some(s.span_id);
            span_ix += 1;
        }
        by_ix.push(cur_span);
    }

    by_ix
}

fn verify_function_bytecode(
    program: &Program,
    func_id: u32,
    func: &Function,
    bytecode: &[u8],
    spans: &[SpanEntry],
    arg_types: &[ValueType],
    ret_types: &[ValueType],
    decoded: &[DecodedInstr],
) -> Result<ExecFuncContainer, VerifyError> {
    if func.reg_count == 0 {
        return Err(VerifyError::ArgCountExceedsRegs { func: func_id });
    }
    // Convention: reg 0 is the effect token; value args occupy regs 1..=arg_count.
    if u64::from(func.arg_count) + 1 > u64::from(func.reg_count) {
        return Err(VerifyError::ArgCountExceedsRegs { func: func_id });
    }

    let boundaries = cfg::compute_boundaries(bytecode.len(), decoded);

    // Build CFG blocks and reachability.
    let byte_len =
        u32::try_from(bytecode.len()).map_err(|_| VerifyError::BytecodeDecode { func: func_id })?;
    let blocks = cfg::build_basic_blocks(byte_len, decoded, &boundaries).map_err(|e| {
        let reason = if e.out_of_range {
            InvalidJumpTargetReason::OutOfRange
        } else {
            InvalidJumpTargetReason::NotInstructionBoundary
        };
        VerifyError::InvalidJumpTarget {
            func: func_id,
            pc: e.src_pc,
            target: e.target_pc,
            reason,
        }
    })?;
    let reachable = cfg::compute_reachable(&blocks);

    // Reject implicit fallthrough between basic blocks: every reachable block must end in an
    // explicit terminator.
    for block in &blocks {
        if block.instr_end == 0 || block.instr_end <= block.instr_start {
            return Err(VerifyError::InternalBlockInconsistent {
                func: func_id,
                pc: block.start_pc,
                instr_start: block.instr_start,
                instr_end: block.instr_end,
            });
        }
        let Some(last) = decoded.get(block.instr_end - 1) else {
            return Err(VerifyError::InternalBlockInconsistent {
                func: func_id,
                pc: block.start_pc,
                instr_start: block.instr_start,
                instr_end: block.instr_end,
            });
        };
        let is_terminator = Opcode::from_u8(last.opcode).is_some_and(Opcode::is_terminator);
        if !is_terminator {
            return Err(VerifyError::MissingTerminator {
                func: func_id,
                pc: last.offset,
            });
        }
    }

    let lints = lint_function_bytecode(func_id, func, decoded, &blocks, &reachable);

    // Must-init analysis (writes-only transfer).
    let reg_count = func.reg_count as usize;
    let entry_init = initial_init(reg_count, arg_types.len());
    let (in_sets, out_sets) =
        compute_must_init(&blocks, &reachable, reg_count, &entry_init, decoded)?;

    // Validate reads.
    for (b_idx, block) in blocks.iter().enumerate() {
        if !reachable[b_idx] {
            continue;
        }
        let mut state = in_sets[b_idx].clone();
        for di in decoded.iter().take(block.instr_end).skip(block.instr_start) {
            validate_instr_reads_writes(program, func_id, di.offset, &di.instr, &mut state, func)?;
        }
        debug_assert_eq!(state, out_sets[b_idx], "must-init OUT mismatch");
    }

    // Type analysis + validation.
    let entry_types = initial_types(reg_count, arg_types);
    let (type_in, type_out) = compute_must_types(
        program,
        &blocks,
        &reachable,
        reg_count,
        &entry_types,
        decoded,
    )?;
    for (b_idx, block) in blocks.iter().enumerate() {
        if !reachable[b_idx] {
            continue;
        }
        let mut state = type_in[b_idx].clone();
        for di in decoded.iter().take(block.instr_end).skip(block.instr_start) {
            validate_instr_types(program, func_id, di.offset, &di.instr, &state, ret_types)?;
            transfer_types(program, &di.instr, &mut state);
        }
        debug_assert_eq!(state, type_out[b_idx], "type OUT mismatch");
    }

    for b_idx in 0..blocks.len() {
        if !reachable[b_idx] {
            continue;
        }
        for (r, t) in type_in[b_idx].values.iter().enumerate() {
            if matches!(t, Some(RegType::Ambiguous)) {
                return Err(VerifyError::UnstableRegType {
                    func: func_id,
                    reg: u32::try_from(r).unwrap_or(u32::MAX),
                });
            }
        }
        for (r, t) in type_out[b_idx].values.iter().enumerate() {
            if matches!(t, Some(RegType::Ambiguous)) {
                return Err(VerifyError::UnstableRegType {
                    func: func_id,
                    reg: u32::try_from(r).unwrap_or(u32::MAX),
                });
            }
        }
    }

    // Compute a stable virtual-register type assignment. Unstable (merged) regs have already been
    // rejected above (`UnstableRegType`), so this should converge to a single concrete `ValueType`
    // for every virtual register that is ever written.
    let mut reg_types: Vec<Option<ValueType>> = vec![None; reg_count];
    if reg_count != 0 {
        reg_types[0] = Some(ValueType::Unit); // effect token
        for (i, &t) in arg_types.iter().enumerate() {
            if 1 + i < reg_count {
                reg_types[1 + i] = Some(t);
            }
        }
    }

    for (b_idx, block) in blocks.iter().enumerate() {
        if !reachable[b_idx] {
            continue;
        }
        let mut state = type_in[b_idx].clone();
        for di in decoded.iter().take(block.instr_end).skip(block.instr_start) {
            transfer_types(program, &di.instr, &mut state);
            for w in di.instr.writes() {
                let Some(t) = state.values.get(w as usize).copied().flatten() else {
                    continue;
                };
                let t = match t {
                    RegType::Concrete(t) => t,
                    RegType::Uninit => {
                        return Err(VerifyError::UnstableRegTypeAt {
                            func: func_id,
                            pc: di.offset,
                            reg: w,
                        });
                    }
                    RegType::Ambiguous => {
                        return Err(VerifyError::UnstableRegTypeAt {
                            func: func_id,
                            pc: di.offset,
                            reg: w,
                        });
                    }
                };
                match reg_types.get_mut(w as usize) {
                    Some(slot @ None) => *slot = Some(t),
                    Some(Some(prev)) if *prev == t => {}
                    Some(Some(_prev)) => {
                        return Err(VerifyError::UnstableRegTypeAt {
                            func: func_id,
                            pc: di.offset,
                            reg: w,
                        });
                    }
                    None => {}
                }
            }
        }
    }

    // Build a per-function register layout: each virtual register maps to exactly one class-local
    // index.
    let mut counts = RegCounts::default();
    let mut reg_map: Vec<VReg> = Vec::with_capacity(reg_count);
    for t in reg_types.iter().copied() {
        let t = t.unwrap_or(ValueType::Unit);
        let class = RegClass::of(t);
        let idx_u32 = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
        let v = match class {
            RegClass::Unit => {
                let r = UnitReg(idx_u32(counts.unit));
                counts.unit += 1;
                VReg::Unit(r)
            }
            RegClass::Bool => {
                let r = BoolReg(idx_u32(counts.bools));
                counts.bools += 1;
                VReg::Bool(r)
            }
            RegClass::I64 => {
                let r = I64Reg(idx_u32(counts.i64s));
                counts.i64s += 1;
                VReg::I64(r)
            }
            RegClass::U64 => {
                let r = U64Reg(idx_u32(counts.u64s));
                counts.u64s += 1;
                VReg::U64(r)
            }
            RegClass::F64 => {
                let r = F64Reg(idx_u32(counts.f64s));
                counts.f64s += 1;
                VReg::F64(r)
            }
            RegClass::Decimal => {
                let r = DecimalReg(idx_u32(counts.decimals));
                counts.decimals += 1;
                VReg::Decimal(r)
            }
            RegClass::Bytes => {
                let r = BytesReg(idx_u32(counts.bytes));
                counts.bytes += 1;
                VReg::Bytes(r)
            }
            RegClass::Str => {
                let r = StrReg(idx_u32(counts.strs));
                counts.strs += 1;
                VReg::Str(r)
            }
            RegClass::Obj => {
                let r = ObjReg(idx_u32(counts.objs));
                counts.objs += 1;
                VReg::Obj(r)
            }
            RegClass::Agg => {
                let r = AggReg(idx_u32(counts.aggs));
                counts.aggs += 1;
                VReg::Agg(r)
            }
            RegClass::Func => {
                let r = FuncReg(idx_u32(counts.funcs));
                counts.funcs += 1;
                VReg::Func(r)
            }
        };
        reg_map.push(v);
    }

    let mut arg_regs: Vec<VReg> = Vec::with_capacity(arg_types.len());
    for i in 0..arg_types.len() {
        let r = reg_map
            .get(1 + i)
            .copied()
            .unwrap_or(VReg::Unit(UnitReg(0)));
        arg_regs.push(r);
    }
    let reg_layout = RegLayout {
        reg_map,
        counts,
        arg_regs,
    };

    let map_pc_to_ix = |src_pc: u32, target_pc: u32| -> Result<u32, VerifyError> {
        let ix = decoded
            .binary_search_by_key(&target_pc, |d| d.offset)
            .map_err(|_| {
                let reason = if target_pc >= byte_len {
                    InvalidJumpTargetReason::OutOfRange
                } else {
                    InvalidJumpTargetReason::NotInstructionBoundary
                };
                VerifyError::InvalidJumpTarget {
                    func: func_id,
                    pc: src_pc,
                    target: target_pc,
                    reason,
                }
            })?;
        u32::try_from(ix).map_err(|_| VerifyError::BytecodeDecode { func: func_id })
    };

    let mut operands: Vec<VReg> = Vec::new();
    let mut execution_instrs: Vec<ExecDecoded> = Vec::with_capacity(decoded.len());
    for di in decoded {
        let pc = di.offset;
        let map = |reg: u32| -> Result<VReg, VerifyError> {
            reg_layout
                .reg_map
                .get(reg as usize)
                .copied()
                .ok_or(VerifyError::RegOutOfBounds {
                    func: func_id,
                    pc,
                    reg,
                })
        };
        let unstable = |reg: u32| VerifyError::UnstableRegTypeAt {
            func: func_id,
            pc,
            reg,
        };
        let map_unit = |reg: u32| -> Result<UnitReg, VerifyError> {
            match map(reg)? {
                VReg::Unit(r) => Ok(r),
                _ => Err(unstable(reg)),
            }
        };
        let map_bool = |reg: u32| -> Result<BoolReg, VerifyError> {
            match map(reg)? {
                VReg::Bool(r) => Ok(r),
                _ => Err(unstable(reg)),
            }
        };
        let map_i64 = |reg: u32| -> Result<I64Reg, VerifyError> {
            match map(reg)? {
                VReg::I64(r) => Ok(r),
                _ => Err(unstable(reg)),
            }
        };
        let map_u64 = |reg: u32| -> Result<U64Reg, VerifyError> {
            match map(reg)? {
                VReg::U64(r) => Ok(r),
                _ => Err(unstable(reg)),
            }
        };
        let map_f64 = |reg: u32| -> Result<F64Reg, VerifyError> {
            match map(reg)? {
                VReg::F64(r) => Ok(r),
                _ => Err(unstable(reg)),
            }
        };
        let map_decimal = |reg: u32| -> Result<DecimalReg, VerifyError> {
            match map(reg)? {
                VReg::Decimal(r) => Ok(r),
                _ => Err(unstable(reg)),
            }
        };
        let map_bytes = |reg: u32| -> Result<BytesReg, VerifyError> {
            match map(reg)? {
                VReg::Bytes(r) => Ok(r),
                _ => Err(unstable(reg)),
            }
        };
        let map_str = |reg: u32| -> Result<StrReg, VerifyError> {
            match map(reg)? {
                VReg::Str(r) => Ok(r),
                _ => Err(unstable(reg)),
            }
        };
        let map_agg = |reg: u32| -> Result<AggReg, VerifyError> {
            match map(reg)? {
                VReg::Agg(r) => Ok(r),
                _ => Err(unstable(reg)),
            }
        };
        let map_func = |reg: u32| -> Result<FuncReg, VerifyError> {
            match map(reg)? {
                VReg::Func(r) => Ok(r),
                _ => Err(unstable(reg)),
            }
        };

        let mut push_vregs = |regs: &[u32]| -> Result<VRegSlice, VerifyError> {
            let start = operands.len();
            operands.reserve(regs.len());
            for &r in regs {
                operands.push(map(r)?);
            }
            Ok(VRegSlice {
                start,
                len: regs.len(),
            })
        };

        let vi = match &di.instr {
            Instr::Nop => ExecInstr::Nop,
            Instr::Trap { code } => ExecInstr::Trap { code: *code },

            Instr::Mov { dst, src } => match (map(*dst)?, map(*src)?) {
                (VReg::Unit(d), VReg::Unit(s)) => ExecInstr::MovUnit { dst: d, src: s },
                (VReg::Bool(d), VReg::Bool(s)) => ExecInstr::MovBool { dst: d, src: s },
                (VReg::I64(d), VReg::I64(s)) => ExecInstr::MovI64 { dst: d, src: s },
                (VReg::U64(d), VReg::U64(s)) => ExecInstr::MovU64 { dst: d, src: s },
                (VReg::F64(d), VReg::F64(s)) => ExecInstr::MovF64 { dst: d, src: s },
                (VReg::Decimal(d), VReg::Decimal(s)) => ExecInstr::MovDecimal { dst: d, src: s },
                (VReg::Bytes(d), VReg::Bytes(s)) => ExecInstr::MovBytes { dst: d, src: s },
                (VReg::Str(d), VReg::Str(s)) => ExecInstr::MovStr { dst: d, src: s },
                (VReg::Obj(d), VReg::Obj(s)) => ExecInstr::MovObj { dst: d, src: s },
                (VReg::Agg(d), VReg::Agg(s)) => ExecInstr::MovAgg { dst: d, src: s },
                (VReg::Func(d), VReg::Func(s)) => ExecInstr::MovFunc { dst: d, src: s },
                _ => {
                    return Err(unstable(*dst));
                }
            },

            Instr::ConstUnit { dst } => ExecInstr::ConstUnit {
                dst: map_unit(*dst)?,
            },
            Instr::ConstBool { dst, imm } => ExecInstr::ConstBool {
                dst: map_bool(*dst)?,
                imm: *imm,
            },
            Instr::ConstI64 { dst, imm } => ExecInstr::ConstI64 {
                dst: map_i64(*dst)?,
                imm: *imm,
            },
            Instr::ConstU64 { dst, imm } => ExecInstr::ConstU64 {
                dst: map_u64(*dst)?,
                imm: *imm,
            },
            Instr::ConstF64 { dst, bits } => ExecInstr::ConstF64 {
                dst: map_f64(*dst)?,
                bits: *bits,
            },
            Instr::ConstDecimal {
                dst,
                mantissa,
                scale,
            } => ExecInstr::ConstDecimal {
                dst: map_decimal(*dst)?,
                mantissa: *mantissa,
                scale: *scale,
            },
            Instr::ConstFunc { dst, func_id } => ExecInstr::ConstFunc {
                dst: map_func(*dst)?,
                func_id: *func_id,
            },

            Instr::ConstPool { dst, idx } => {
                let c = &program.const_pool[idx.0 as usize];
                match const_value_type(c) {
                    ValueType::Unit => ExecInstr::ConstPoolUnit {
                        dst: map_unit(*dst)?,
                        idx: *idx,
                    },
                    ValueType::Bool => ExecInstr::ConstPoolBool {
                        dst: map_bool(*dst)?,
                        idx: *idx,
                    },
                    ValueType::I64 => ExecInstr::ConstPoolI64 {
                        dst: map_i64(*dst)?,
                        idx: *idx,
                    },
                    ValueType::U64 => ExecInstr::ConstPoolU64 {
                        dst: map_u64(*dst)?,
                        idx: *idx,
                    },
                    ValueType::F64 => ExecInstr::ConstPoolF64 {
                        dst: map_f64(*dst)?,
                        idx: *idx,
                    },
                    ValueType::Decimal => ExecInstr::ConstPoolDecimal {
                        dst: map_decimal(*dst)?,
                        idx: *idx,
                    },
                    ValueType::Bytes => ExecInstr::ConstPoolBytes {
                        dst: map_bytes(*dst)?,
                        idx: *idx,
                    },
                    ValueType::Str => ExecInstr::ConstPoolStr {
                        dst: map_str(*dst)?,
                        idx: *idx,
                    },
                    _ => {
                        return Err(VerifyError::ConstOutOfBounds {
                            func: func_id,
                            pc: di.offset,
                            const_id: idx.0,
                        });
                    }
                }
            }

            Instr::DecAdd { dst, a, b } => ExecInstr::DecAdd {
                dst: map_decimal(*dst)?,
                a: map_decimal(*a)?,
                b: map_decimal(*b)?,
            },
            Instr::DecSub { dst, a, b } => ExecInstr::DecSub {
                dst: map_decimal(*dst)?,
                a: map_decimal(*a)?,
                b: map_decimal(*b)?,
            },
            Instr::DecMul { dst, a, b } => ExecInstr::DecMul {
                dst: map_decimal(*dst)?,
                a: map_decimal(*a)?,
                b: map_decimal(*b)?,
            },

            Instr::F64Add { dst, a, b } => ExecInstr::F64Add {
                dst: map_f64(*dst)?,
                a: map_f64(*a)?,
                b: map_f64(*b)?,
            },
            Instr::F64Sub { dst, a, b } => ExecInstr::F64Sub {
                dst: map_f64(*dst)?,
                a: map_f64(*a)?,
                b: map_f64(*b)?,
            },
            Instr::F64Mul { dst, a, b } => ExecInstr::F64Mul {
                dst: map_f64(*dst)?,
                a: map_f64(*a)?,
                b: map_f64(*b)?,
            },
            Instr::F64Div { dst, a, b } => ExecInstr::F64Div {
                dst: map_f64(*dst)?,
                a: map_f64(*a)?,
                b: map_f64(*b)?,
            },
            Instr::F64Neg { dst, a } => ExecInstr::F64Neg {
                dst: map_f64(*dst)?,
                a: map_f64(*a)?,
            },
            Instr::F64Abs { dst, a } => ExecInstr::F64Abs {
                dst: map_f64(*dst)?,
                a: map_f64(*a)?,
            },
            Instr::F64Min { dst, a, b } => ExecInstr::F64Min {
                dst: map_f64(*dst)?,
                a: map_f64(*a)?,
                b: map_f64(*b)?,
            },
            Instr::F64Max { dst, a, b } => ExecInstr::F64Max {
                dst: map_f64(*dst)?,
                a: map_f64(*a)?,
                b: map_f64(*b)?,
            },
            Instr::F64MinNum { dst, a, b } => ExecInstr::F64MinNum {
                dst: map_f64(*dst)?,
                a: map_f64(*a)?,
                b: map_f64(*b)?,
            },
            Instr::F64MaxNum { dst, a, b } => ExecInstr::F64MaxNum {
                dst: map_f64(*dst)?,
                a: map_f64(*a)?,
                b: map_f64(*b)?,
            },
            Instr::F64Rem { dst, a, b } => ExecInstr::F64Rem {
                dst: map_f64(*dst)?,
                a: map_f64(*a)?,
                b: map_f64(*b)?,
            },
            Instr::F64ToBits { dst, a } => ExecInstr::F64ToBits {
                dst: map_u64(*dst)?,
                a: map_f64(*a)?,
            },
            Instr::F64FromBits { dst, a } => ExecInstr::F64FromBits {
                dst: map_f64(*dst)?,
                a: map_u64(*a)?,
            },

            Instr::I64Add { dst, a, b } => ExecInstr::I64Add {
                dst: map_i64(*dst)?,
                a: map_i64(*a)?,
                b: map_i64(*b)?,
            },
            Instr::I64Sub { dst, a, b } => ExecInstr::I64Sub {
                dst: map_i64(*dst)?,
                a: map_i64(*a)?,
                b: map_i64(*b)?,
            },
            Instr::I64Mul { dst, a, b } => ExecInstr::I64Mul {
                dst: map_i64(*dst)?,
                a: map_i64(*a)?,
                b: map_i64(*b)?,
            },

            Instr::U64Add { dst, a, b } => ExecInstr::U64Add {
                dst: map_u64(*dst)?,
                a: map_u64(*a)?,
                b: map_u64(*b)?,
            },
            Instr::U64Sub { dst, a, b } => ExecInstr::U64Sub {
                dst: map_u64(*dst)?,
                a: map_u64(*a)?,
                b: map_u64(*b)?,
            },
            Instr::U64Mul { dst, a, b } => ExecInstr::U64Mul {
                dst: map_u64(*dst)?,
                a: map_u64(*a)?,
                b: map_u64(*b)?,
            },
            Instr::U64And { dst, a, b } => ExecInstr::U64And {
                dst: map_u64(*dst)?,
                a: map_u64(*a)?,
                b: map_u64(*b)?,
            },
            Instr::U64Or { dst, a, b } => ExecInstr::U64Or {
                dst: map_u64(*dst)?,
                a: map_u64(*a)?,
                b: map_u64(*b)?,
            },
            Instr::U64Xor { dst, a, b } => ExecInstr::U64Xor {
                dst: map_u64(*dst)?,
                a: map_u64(*a)?,
                b: map_u64(*b)?,
            },
            Instr::U64Shl { dst, a, b } => ExecInstr::U64Shl {
                dst: map_u64(*dst)?,
                a: map_u64(*a)?,
                b: map_u64(*b)?,
            },
            Instr::U64Shr { dst, a, b } => ExecInstr::U64Shr {
                dst: map_u64(*dst)?,
                a: map_u64(*a)?,
                b: map_u64(*b)?,
            },

            Instr::I64Eq { dst, a, b } => ExecInstr::I64Eq {
                dst: map_bool(*dst)?,
                a: map_i64(*a)?,
                b: map_i64(*b)?,
            },
            Instr::I64Lt { dst, a, b } => ExecInstr::I64Lt {
                dst: map_bool(*dst)?,
                a: map_i64(*a)?,
                b: map_i64(*b)?,
            },
            Instr::I64Gt { dst, a, b } => ExecInstr::I64Gt {
                dst: map_bool(*dst)?,
                a: map_i64(*a)?,
                b: map_i64(*b)?,
            },
            Instr::I64Le { dst, a, b } => ExecInstr::I64Le {
                dst: map_bool(*dst)?,
                a: map_i64(*a)?,
                b: map_i64(*b)?,
            },
            Instr::I64Ge { dst, a, b } => ExecInstr::I64Ge {
                dst: map_bool(*dst)?,
                a: map_i64(*a)?,
                b: map_i64(*b)?,
            },

            Instr::U64Eq { dst, a, b } => ExecInstr::U64Eq {
                dst: map_bool(*dst)?,
                a: map_u64(*a)?,
                b: map_u64(*b)?,
            },
            Instr::U64Lt { dst, a, b } => ExecInstr::U64Lt {
                dst: map_bool(*dst)?,
                a: map_u64(*a)?,
                b: map_u64(*b)?,
            },
            Instr::U64Gt { dst, a, b } => ExecInstr::U64Gt {
                dst: map_bool(*dst)?,
                a: map_u64(*a)?,
                b: map_u64(*b)?,
            },
            Instr::U64Le { dst, a, b } => ExecInstr::U64Le {
                dst: map_bool(*dst)?,
                a: map_u64(*a)?,
                b: map_u64(*b)?,
            },
            Instr::U64Ge { dst, a, b } => ExecInstr::U64Ge {
                dst: map_bool(*dst)?,
                a: map_u64(*a)?,
                b: map_u64(*b)?,
            },

            Instr::F64Eq { dst, a, b } => ExecInstr::F64Eq {
                dst: map_bool(*dst)?,
                a: map_f64(*a)?,
                b: map_f64(*b)?,
            },
            Instr::F64Lt { dst, a, b } => ExecInstr::F64Lt {
                dst: map_bool(*dst)?,
                a: map_f64(*a)?,
                b: map_f64(*b)?,
            },
            Instr::F64Gt { dst, a, b } => ExecInstr::F64Gt {
                dst: map_bool(*dst)?,
                a: map_f64(*a)?,
                b: map_f64(*b)?,
            },
            Instr::F64Le { dst, a, b } => ExecInstr::F64Le {
                dst: map_bool(*dst)?,
                a: map_f64(*a)?,
                b: map_f64(*b)?,
            },
            Instr::F64Ge { dst, a, b } => ExecInstr::F64Ge {
                dst: map_bool(*dst)?,
                a: map_f64(*a)?,
                b: map_f64(*b)?,
            },

            Instr::BoolNot { dst, a } => ExecInstr::BoolNot {
                dst: map_bool(*dst)?,
                a: map_bool(*a)?,
            },
            Instr::BoolAnd { dst, a, b } => ExecInstr::BoolAnd {
                dst: map_bool(*dst)?,
                a: map_bool(*a)?,
                b: map_bool(*b)?,
            },
            Instr::BoolOr { dst, a, b } => ExecInstr::BoolOr {
                dst: map_bool(*dst)?,
                a: map_bool(*a)?,
                b: map_bool(*b)?,
            },
            Instr::BoolXor { dst, a, b } => ExecInstr::BoolXor {
                dst: map_bool(*dst)?,
                a: map_bool(*a)?,
                b: map_bool(*b)?,
            },

            Instr::I64And { dst, a, b } => ExecInstr::I64And {
                dst: map_i64(*dst)?,
                a: map_i64(*a)?,
                b: map_i64(*b)?,
            },
            Instr::I64Or { dst, a, b } => ExecInstr::I64Or {
                dst: map_i64(*dst)?,
                a: map_i64(*a)?,
                b: map_i64(*b)?,
            },
            Instr::I64Xor { dst, a, b } => ExecInstr::I64Xor {
                dst: map_i64(*dst)?,
                a: map_i64(*a)?,
                b: map_i64(*b)?,
            },
            Instr::I64Shl { dst, a, b } => ExecInstr::I64Shl {
                dst: map_i64(*dst)?,
                a: map_i64(*a)?,
                b: map_i64(*b)?,
            },
            Instr::I64Shr { dst, a, b } => ExecInstr::I64Shr {
                dst: map_i64(*dst)?,
                a: map_i64(*a)?,
                b: map_i64(*b)?,
            },

            Instr::U64ToI64 { dst, a } => ExecInstr::U64ToI64 {
                dst: map_i64(*dst)?,
                a: map_u64(*a)?,
            },
            Instr::I64ToU64 { dst, a } => ExecInstr::I64ToU64 {
                dst: map_u64(*dst)?,
                a: map_i64(*a)?,
            },

            Instr::Select { dst, cond, a, b } => match (map(*dst)?, map(*a)?, map(*b)?) {
                (VReg::Unit(d), VReg::Unit(aa), VReg::Unit(bb)) => ExecInstr::SelectUnit {
                    dst: d,
                    cond: map_bool(*cond)?,
                    a: aa,
                    b: bb,
                },
                (VReg::Bool(d), VReg::Bool(aa), VReg::Bool(bb)) => ExecInstr::SelectBool {
                    dst: d,
                    cond: map_bool(*cond)?,
                    a: aa,
                    b: bb,
                },
                (VReg::I64(d), VReg::I64(aa), VReg::I64(bb)) => ExecInstr::SelectI64 {
                    dst: d,
                    cond: map_bool(*cond)?,
                    a: aa,
                    b: bb,
                },
                (VReg::U64(d), VReg::U64(aa), VReg::U64(bb)) => ExecInstr::SelectU64 {
                    dst: d,
                    cond: map_bool(*cond)?,
                    a: aa,
                    b: bb,
                },
                (VReg::F64(d), VReg::F64(aa), VReg::F64(bb)) => ExecInstr::SelectF64 {
                    dst: d,
                    cond: map_bool(*cond)?,
                    a: aa,
                    b: bb,
                },
                (VReg::Decimal(d), VReg::Decimal(aa), VReg::Decimal(bb)) => {
                    ExecInstr::SelectDecimal {
                        dst: d,
                        cond: map_bool(*cond)?,
                        a: aa,
                        b: bb,
                    }
                }
                (VReg::Bytes(d), VReg::Bytes(aa), VReg::Bytes(bb)) => ExecInstr::SelectBytes {
                    dst: d,
                    cond: map_bool(*cond)?,
                    a: aa,
                    b: bb,
                },
                (VReg::Str(d), VReg::Str(aa), VReg::Str(bb)) => ExecInstr::SelectStr {
                    dst: d,
                    cond: map_bool(*cond)?,
                    a: aa,
                    b: bb,
                },
                (VReg::Obj(d), VReg::Obj(aa), VReg::Obj(bb)) => ExecInstr::SelectObj {
                    dst: d,
                    cond: map_bool(*cond)?,
                    a: aa,
                    b: bb,
                },
                (VReg::Agg(d), VReg::Agg(aa), VReg::Agg(bb)) => ExecInstr::SelectAgg {
                    dst: d,
                    cond: map_bool(*cond)?,
                    a: aa,
                    b: bb,
                },
                (VReg::Func(d), VReg::Func(aa), VReg::Func(bb)) => ExecInstr::SelectFunc {
                    dst: d,
                    cond: map_bool(*cond)?,
                    a: aa,
                    b: bb,
                },
                _ => {
                    return Err(unstable(*dst));
                }
            },

            Instr::Br {
                cond,
                pc_true,
                pc_false,
            } => ExecInstr::Br {
                cond: map_bool(*cond)?,
                true_ix: map_pc_to_ix(pc, *pc_true)?,
                false_ix: map_pc_to_ix(pc, *pc_false)?,
            },
            Instr::Jmp { pc_target } => ExecInstr::Jmp {
                target_ix: map_pc_to_ix(pc, *pc_target)?,
            },

            Instr::Call {
                eff_out,
                func_id: callee,
                eff_in,
                args,
                rets,
            } => ExecInstr::Call {
                eff_out: map_unit(*eff_out)?,
                func_id: *callee,
                eff_in: map_unit(*eff_in)?,
                args: push_vregs(args)?,
                rets: push_vregs(rets)?,
            },
            Instr::Ret { eff_in, rets } => ExecInstr::Ret {
                eff_in: map_unit(*eff_in)?,
                rets: push_vregs(rets)?,
            },
            Instr::HostCall {
                eff_out,
                host_sig,
                eff_in,
                args,
                rets,
            } => ExecInstr::HostCall {
                eff_out: map_unit(*eff_out)?,
                host_sig: *host_sig,
                eff_in: map_unit(*eff_in)?,
                args: push_vregs(args)?,
                rets: push_vregs(rets)?,
            },

            Instr::TupleNew { dst, values } => ExecInstr::TupleNew {
                dst: map_agg(*dst)?,
                values: push_vregs(values)?,
            },
            Instr::TupleGet { dst, tuple, index } => ExecInstr::TupleGet {
                dst: map(*dst)?,
                tuple: map_agg(*tuple)?,
                index: *index,
            },

            Instr::StructNew {
                dst,
                type_id,
                values,
            } => ExecInstr::StructNew {
                dst: map_agg(*dst)?,
                type_id: *type_id,
                values: push_vregs(values)?,
            },
            Instr::StructGet {
                dst,
                st,
                field_index,
            } => ExecInstr::StructGet {
                dst: map(*dst)?,
                st: map_agg(*st)?,
                field_index: *field_index,
            },

            Instr::ArrayNew {
                dst,
                elem_type_id,
                len,
                values,
            } => ExecInstr::ArrayNew {
                dst: map_agg(*dst)?,
                elem_type_id: *elem_type_id,
                len: *len,
                values: push_vregs(values)?,
            },
            Instr::ArrayLen { dst, arr } => ExecInstr::ArrayLen {
                dst: map_u64(*dst)?,
                arr: map_agg(*arr)?,
            },
            Instr::ArrayGet { dst, arr, index } => ExecInstr::ArrayGet {
                dst: map(*dst)?,
                arr: map_agg(*arr)?,
                index: map_u64(*index)?,
            },
            Instr::ArrayGetImm { dst, arr, index } => ExecInstr::ArrayGetImm {
                dst: map(*dst)?,
                arr: map_agg(*arr)?,
                index: *index,
            },

            Instr::TupleLen { dst, tuple } => ExecInstr::TupleLen {
                dst: map_u64(*dst)?,
                tuple: map_agg(*tuple)?,
            },
            Instr::StructFieldCount { dst, st } => ExecInstr::StructFieldCount {
                dst: map_u64(*dst)?,
                st: map_agg(*st)?,
            },

            Instr::BytesLen { dst, bytes } => ExecInstr::BytesLen {
                dst: map_u64(*dst)?,
                bytes: map_bytes(*bytes)?,
            },
            Instr::StrLen { dst, s } => ExecInstr::StrLen {
                dst: map_u64(*dst)?,
                s: map_str(*s)?,
            },

            Instr::I64Div { dst, a, b } => ExecInstr::I64Div {
                dst: map_i64(*dst)?,
                a: map_i64(*a)?,
                b: map_i64(*b)?,
            },
            Instr::I64Rem { dst, a, b } => ExecInstr::I64Rem {
                dst: map_i64(*dst)?,
                a: map_i64(*a)?,
                b: map_i64(*b)?,
            },
            Instr::U64Div { dst, a, b } => ExecInstr::U64Div {
                dst: map_u64(*dst)?,
                a: map_u64(*a)?,
                b: map_u64(*b)?,
            },
            Instr::U64Rem { dst, a, b } => ExecInstr::U64Rem {
                dst: map_u64(*dst)?,
                a: map_u64(*a)?,
                b: map_u64(*b)?,
            },

            Instr::I64ToF64 { dst, a } => ExecInstr::I64ToF64 {
                dst: map_f64(*dst)?,
                a: map_i64(*a)?,
            },
            Instr::U64ToF64 { dst, a } => ExecInstr::U64ToF64 {
                dst: map_f64(*dst)?,
                a: map_u64(*a)?,
            },
            Instr::F64ToI64 { dst, a } => ExecInstr::F64ToI64 {
                dst: map_i64(*dst)?,
                a: map_f64(*a)?,
            },
            Instr::F64ToU64 { dst, a } => ExecInstr::F64ToU64 {
                dst: map_u64(*dst)?,
                a: map_f64(*a)?,
            },

            Instr::DecToI64 { dst, a } => ExecInstr::DecToI64 {
                dst: map_i64(*dst)?,
                a: map_decimal(*a)?,
            },
            Instr::DecToU64 { dst, a } => ExecInstr::DecToU64 {
                dst: map_u64(*dst)?,
                a: map_decimal(*a)?,
            },
            Instr::I64ToDec { dst, a, scale } => ExecInstr::I64ToDec {
                dst: map_decimal(*dst)?,
                a: map_i64(*a)?,
                scale: *scale,
            },
            Instr::U64ToDec { dst, a, scale } => ExecInstr::U64ToDec {
                dst: map_decimal(*dst)?,
                a: map_u64(*a)?,
                scale: *scale,
            },

            Instr::BytesEq { dst, a, b } => ExecInstr::BytesEq {
                dst: map_bool(*dst)?,
                a: map_bytes(*a)?,
                b: map_bytes(*b)?,
            },
            Instr::StrEq { dst, a, b } => ExecInstr::StrEq {
                dst: map_bool(*dst)?,
                a: map_str(*a)?,
                b: map_str(*b)?,
            },
            Instr::BytesConcat { dst, a, b } => ExecInstr::BytesConcat {
                dst: map_bytes(*dst)?,
                a: map_bytes(*a)?,
                b: map_bytes(*b)?,
            },
            Instr::StrConcat { dst, a, b } => ExecInstr::StrConcat {
                dst: map_str(*dst)?,
                a: map_str(*a)?,
                b: map_str(*b)?,
            },
            Instr::BytesGet { dst, bytes, index } => ExecInstr::BytesGet {
                dst: map_u64(*dst)?,
                bytes: map_bytes(*bytes)?,
                index: map_u64(*index)?,
            },
            Instr::BytesGetImm { dst, bytes, index } => ExecInstr::BytesGetImm {
                dst: map_u64(*dst)?,
                bytes: map_bytes(*bytes)?,
                index: *index,
            },
            Instr::BytesSlice {
                dst,
                bytes,
                start,
                end,
            } => ExecInstr::BytesSlice {
                dst: map_bytes(*dst)?,
                bytes: map_bytes(*bytes)?,
                start: map_u64(*start)?,
                end: map_u64(*end)?,
            },
            Instr::StrSlice { dst, s, start, end } => ExecInstr::StrSlice {
                dst: map_str(*dst)?,
                s: map_str(*s)?,
                start: map_u64(*start)?,
                end: map_u64(*end)?,
            },
            Instr::StrToBytes { dst, s } => ExecInstr::StrToBytes {
                dst: map_bytes(*dst)?,
                s: map_str(*s)?,
            },
            Instr::BytesToStr { dst, bytes } => ExecInstr::BytesToStr {
                dst: map_str(*dst)?,
                bytes: map_bytes(*bytes)?,
            },
        };

        execution_instrs.push(ExecDecoded {
            offset: di.offset,
            opcode: di.opcode,
            instr: vi,
        });
    }
    let span_by_instr_ix = build_span_by_instr_ix(decoded, spans);

    Ok(ExecFuncContainer {
        exec: ExecFunc {
            byte_len,
            reg_layout,
            operands,
            instrs: execution_instrs,
            span_by_instr_ix,
        },
        lints,
    })
}

fn lint_function_bytecode(
    func_id: u32,
    func: &Function,
    decoded: &[DecodedInstr],
    blocks: &[BasicBlock],
    reachable: &[bool],
) -> Vec<VerifyLint> {
    let reg_count = func.reg_count as usize;
    if reg_count == 0 {
        return Vec::new();
    }

    let mut lints: Vec<VerifyLint> = Vec::new();
    for (b_idx, b) in blocks.iter().enumerate() {
        if !reachable.get(b_idx).copied().unwrap_or(false) {
            lints.push(VerifyLint::UnreachableBlock {
                func: func_id,
                pc: b.start_pc,
            });
        }
    }

    let live = liveness::compute_liveness(reg_count, decoded, blocks, reachable);

    // Report dead stores / unused call returns / redundant moves.
    for (b_idx, b) in blocks.iter().enumerate() {
        if !reachable.get(b_idx).copied().unwrap_or(false) {
            continue;
        }
        let mut live = live.live_out[b_idx].clone();

        for di in decoded.iter().take(b.instr_end).skip(b.instr_start).rev() {
            if let Instr::Mov { dst, src } = di.instr
                && dst == src
                && dst != 0
            {
                lints.push(VerifyLint::RedundantMove {
                    func: func_id,
                    pc: di.offset,
                    dst,
                    src,
                });
            }

            // Writes.
            let opcode = Opcode::from_u8(di.opcode).expect("decoder only emits known opcodes");
            let is_call_like = opcode.is_call_like();
            let mut call_rets: Option<&[u32]> = None;
            if let Instr::Call { rets, .. } = &di.instr {
                call_rets = Some(rets.as_slice());
            } else if let Instr::HostCall { rets, .. } = &di.instr {
                call_rets = Some(rets.as_slice());
            }

            for w in di.instr.writes() {
                if w == 0 {
                    continue;
                }
                if !live.get(w as usize) {
                    if is_call_like
                        && call_rets.is_some_and(|rs| rs.iter().copied().any(|r| r == w))
                    {
                        lints.push(VerifyLint::UnusedCallReturn {
                            func: func_id,
                            pc: di.offset,
                            reg: w,
                        });
                    } else {
                        lints.push(VerifyLint::DeadStore {
                            func: func_id,
                            pc: di.offset,
                            reg: w,
                        });
                    }
                }
                live.clear(w as usize);
            }

            // Reads.
            for r in di.instr.reads() {
                if r == 0 {
                    continue;
                }
                live.set(r as usize);
            }
        }
    }

    lints
}

fn validate_instr_reads_writes(
    program: &Program,
    func_id: u32,
    pc: u32,
    instr: &Instr,
    state: &mut BitSet,
    func: &Function,
) -> Result<(), VerifyError> {
    let reg_limit = func.reg_count;
    let check_reg = |reg: u32| -> Result<(), VerifyError> {
        if reg >= reg_limit {
            Err(VerifyError::RegOutOfBounds {
                func: func_id,
                pc,
                reg,
            })
        } else {
            Ok(())
        }
    };
    let require_init = |reg: u32, state: &BitSet| -> Result<(), VerifyError> {
        check_reg(reg)?;
        if !state.get(reg as usize) {
            Err(VerifyError::UninitializedRead {
                func: func_id,
                pc,
                reg,
            })
        } else {
            Ok(())
        }
    };
    let write_reg = |reg: u32, state: &mut BitSet| -> Result<(), VerifyError> {
        check_reg(reg)?;
        state.set(reg as usize);
        Ok(())
    };
    let require_eff_in_r0 = |reg: u32| -> Result<(), VerifyError> {
        if reg != 0 {
            return Err(VerifyError::EffectInNotR0 {
                func: func_id,
                pc,
                reg,
            });
        }
        Ok(())
    };
    let require_eff_out_r0 = |reg: u32| -> Result<(), VerifyError> {
        if reg != 0 {
            return Err(VerifyError::EffectOutNotR0 {
                func: func_id,
                pc,
                reg,
            });
        }
        Ok(())
    };

    // Instruction-specific invariants that are *not* captured by the generic read/write sets.
    match instr {
        Instr::Call {
            eff_out,
            func_id: callee,
            eff_in,
            args,
            rets,
        } => {
            require_eff_in_r0(*eff_in)?;
            require_eff_out_r0(*eff_out)?;

            // Signature check (counts only for now).
            let callee = &program.functions[callee.0 as usize];
            if u32::try_from(args.len()).ok() != Some(callee.arg_count)
                || u32::try_from(rets.len()).ok() != Some(callee.ret_count)
            {
                return Err(VerifyError::CallArityMismatch { func: func_id, pc });
            }
        }
        Instr::Ret { eff_in, rets } => {
            require_eff_in_r0(*eff_in)?;
            if u32::try_from(rets.len()).ok() != Some(func.ret_count) {
                return Err(VerifyError::ReturnArityMismatch { func: func_id, pc });
            }
        }
        Instr::HostCall {
            eff_out,
            host_sig,
            eff_in,
            args,
            rets,
        } => {
            require_eff_in_r0(*eff_in)?;
            require_eff_out_r0(*eff_out)?;

            let hs = program
                .host_sig(*host_sig)
                .expect("validated by verify_id_operands_in_bounds");
            let hs_args =
                program
                    .host_sig_args(hs)
                    .map_err(|_| VerifyError::HostCallSigMalformed {
                        func: func_id,
                        pc,
                        host_sig: host_sig.0,
                    })?;
            let hs_rets =
                program
                    .host_sig_rets(hs)
                    .map_err(|_| VerifyError::HostCallSigMalformed {
                        func: func_id,
                        pc,
                        host_sig: host_sig.0,
                    })?;
            if args.len() != hs_args.len() || rets.len() != hs_rets.len() {
                return Err(VerifyError::HostCallArityMismatch { func: func_id, pc });
            }
        }
        _ => {}
    }

    // Generic init-before-use + writes-only transfer.
    for r in instr.reads() {
        require_init(r, state)?;
    }
    for w in instr.writes() {
        write_reg(w, state)?;
    }

    Ok(())
}

fn initial_init(reg_count: usize, arg_count: usize) -> BitSet {
    let mut s = BitSet::new_empty(reg_count);
    if reg_count != 0 {
        s.set(0); // effect token
        for i in 0..arg_count {
            s.set(1 + i);
        }
    }
    s
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum AggMeta {
    Tuple(Vec<Option<ValueType>>),
    Struct(TypeId),
    Array(ElemTypeId),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum RegType {
    Uninit,
    Concrete(ValueType),
    Ambiguous,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TypeState {
    // `None` means "unknown/top" during fixpoint iteration (used to compute a greatest fixpoint for
    // loop-invariant types without assuming a particular type up front).
    //
    // `Some(RegType::Uninit)` means "definitely uninitialized".
    // `Some(RegType::Concrete(t))` means "definitely initialized with concrete type `t`".
    // `Some(RegType::Ambiguous)` means "definitely initialized, but the type is not stable".
    values: Vec<Option<RegType>>,
    aggs: Vec<Option<AggMeta>>,
}

fn initial_types(reg_count: usize, arg_types: &[ValueType]) -> TypeState {
    let mut values: Vec<Option<RegType>> = vec![Some(RegType::Uninit); reg_count];
    let aggs: Vec<Option<AggMeta>> = vec![None; reg_count];
    if reg_count != 0 {
        values[0] = Some(RegType::Concrete(ValueType::Unit)); // effect token
        for (i, &t) in arg_types.iter().enumerate() {
            if 1 + i < reg_count {
                values[1 + i] = Some(RegType::Concrete(t));
            }
        }
    }
    TypeState { values, aggs }
}

fn meet_value(a: Option<RegType>, b: Option<RegType>) -> Option<RegType> {
    match (a, b) {
        // Unknown/top (used for initialization) doesn't constrain the result.
        (None, x) | (x, None) => x,
        // Any definitely-uninitialized incoming path makes the value definitely uninitialized.
        (Some(RegType::Uninit), _) | (_, Some(RegType::Uninit)) => Some(RegType::Uninit),
        (Some(RegType::Ambiguous), _) | (_, Some(RegType::Ambiguous)) => Some(RegType::Ambiguous),
        (Some(RegType::Concrete(x)), Some(RegType::Concrete(y))) => {
            if x == y {
                Some(RegType::Concrete(x))
            } else {
                Some(RegType::Ambiguous)
            }
        }
    }
}

fn meet_agg(a: &Option<AggMeta>, b: &Option<AggMeta>) -> Option<AggMeta> {
    match (a, b) {
        (Some(x), Some(y)) if x == y => Some(x.clone()),
        _ => None,
    }
}

fn const_value_type(c: &ConstEntry) -> ValueType {
    match c {
        ConstEntry::Unit => ValueType::Unit,
        ConstEntry::Bool(_) => ValueType::Bool,
        ConstEntry::I64(_) => ValueType::I64,
        ConstEntry::U64(_) => ValueType::U64,
        ConstEntry::F64(_) => ValueType::F64,
        ConstEntry::Decimal { .. } => ValueType::Decimal,
        ConstEntry::Bytes(_) => ValueType::Bytes,
        ConstEntry::Str(_) => ValueType::Str,
    }
}

fn transfer_types(program: &Program, instr: &Instr, state: &mut TypeState) {
    fn clear_agg(state: &mut TypeState, reg: u32) {
        if let Some(slot) = state.aggs.get_mut(reg as usize) {
            *slot = None;
        }
    }

    fn set_reg_type(state: &mut TypeState, reg: u32, ty: RegType) {
        if let Some(slot) = state.values.get_mut(reg as usize) {
            *slot = Some(ty);
        }
        if ty != RegType::Concrete(ValueType::Agg) {
            clear_agg(state, reg);
        }
    }

    fn set_value(state: &mut TypeState, reg: u32, ty: ValueType) {
        set_reg_type(state, reg, RegType::Concrete(ty));
    }

    fn set_ambiguous(state: &mut TypeState, reg: u32) {
        set_reg_type(state, reg, RegType::Ambiguous);
    }

    fn set_agg(state: &mut TypeState, reg: u32, meta: Option<AggMeta>) {
        set_value(state, reg, ValueType::Agg);
        if let Some(slot) = state.aggs.get_mut(reg as usize) {
            *slot = meta;
        }
    }

    fn copy_reg(state: &mut TypeState, dst: u32, src: u32) {
        let t = state.values.get(src as usize).copied().unwrap_or(None);
        if let Some(slot) = state.values.get_mut(dst as usize) {
            *slot = t;
        }
        let meta = state.aggs.get(src as usize).cloned().unwrap_or(None);
        if let Some(slot) = state.aggs.get_mut(dst as usize) {
            *slot = meta;
        }
        if t != Some(RegType::Concrete(ValueType::Agg)) {
            clear_agg(state, dst);
        }
    }

    match instr {
        Instr::Nop
        | Instr::Trap { .. }
        | Instr::Jmp { .. }
        | Instr::Br { .. }
        | Instr::Ret { .. } => {}
        Instr::Mov { dst, src } => copy_reg(state, *dst, *src),
        Instr::ConstUnit { dst } => set_value(state, *dst, ValueType::Unit),
        Instr::ConstBool { dst, .. } => set_value(state, *dst, ValueType::Bool),
        Instr::ConstI64 { dst, .. } => set_value(state, *dst, ValueType::I64),
        Instr::ConstU64 { dst, .. } => set_value(state, *dst, ValueType::U64),
        Instr::ConstF64 { dst, .. } => set_value(state, *dst, ValueType::F64),
        Instr::ConstDecimal { dst, .. } => set_value(state, *dst, ValueType::Decimal),
        Instr::ConstFunc { dst, .. } => set_value(state, *dst, ValueType::Func),
        Instr::ConstPool { dst, idx } => {
            let t = program
                .const_pool
                .get(idx.0 as usize)
                .map(const_value_type)
                .map(RegType::Concrete)
                .unwrap_or(RegType::Ambiguous);
            set_reg_type(state, *dst, t);
        }
        Instr::DecAdd { dst, .. } | Instr::DecSub { dst, .. } | Instr::DecMul { dst, .. } => {
            set_value(state, *dst, ValueType::Decimal);
        }
        Instr::F64Add { dst, .. }
        | Instr::F64Sub { dst, .. }
        | Instr::F64Mul { dst, .. }
        | Instr::F64Div { dst, .. }
        | Instr::F64Neg { dst, .. }
        | Instr::F64Abs { dst, .. }
        | Instr::F64Min { dst, .. }
        | Instr::F64Max { dst, .. }
        | Instr::F64MinNum { dst, .. }
        | Instr::F64MaxNum { dst, .. }
        | Instr::F64Rem { dst, .. }
        | Instr::F64FromBits { dst, .. } => {
            set_value(state, *dst, ValueType::F64);
        }
        Instr::I64Add { dst, .. }
        | Instr::I64Sub { dst, .. }
        | Instr::I64Mul { dst, .. }
        | Instr::I64Div { dst, .. }
        | Instr::I64Rem { dst, .. }
        | Instr::U64ToI64 { dst, .. } => set_value(state, *dst, ValueType::I64),
        Instr::U64Add { dst, .. } | Instr::U64Sub { dst, .. } | Instr::U64Mul { dst, .. } => {
            set_value(state, *dst, ValueType::U64);
        }
        Instr::U64Div { dst, .. } | Instr::U64Rem { dst, .. } => {
            set_value(state, *dst, ValueType::U64);
        }
        Instr::U64And { dst, .. }
        | Instr::U64Or { dst, .. }
        | Instr::U64Xor { dst, .. }
        | Instr::U64Shl { dst, .. }
        | Instr::U64Shr { dst, .. } => {
            set_value(state, *dst, ValueType::U64);
        }
        Instr::I64And { dst, .. }
        | Instr::I64Or { dst, .. }
        | Instr::I64Xor { dst, .. }
        | Instr::I64Shl { dst, .. }
        | Instr::I64Shr { dst, .. } => {
            set_value(state, *dst, ValueType::I64);
        }
        Instr::I64ToU64 { dst, .. } | Instr::F64ToBits { dst, .. } => {
            set_value(state, *dst, ValueType::U64);
        }
        Instr::I64ToF64 { dst, .. } | Instr::U64ToF64 { dst, .. } => {
            set_value(state, *dst, ValueType::F64);
        }
        Instr::F64ToI64 { dst, .. } | Instr::DecToI64 { dst, .. } => {
            set_value(state, *dst, ValueType::I64);
        }
        Instr::F64ToU64 { dst, .. } | Instr::DecToU64 { dst, .. } => {
            set_value(state, *dst, ValueType::U64);
        }
        Instr::I64ToDec { dst, .. } | Instr::U64ToDec { dst, .. } => {
            set_value(state, *dst, ValueType::Decimal);
        }
        Instr::I64Eq { dst, .. }
        | Instr::I64Lt { dst, .. }
        | Instr::U64Eq { dst, .. }
        | Instr::U64Lt { dst, .. }
        | Instr::U64Gt { dst, .. }
        | Instr::U64Le { dst, .. }
        | Instr::U64Ge { dst, .. }
        | Instr::I64Gt { dst, .. }
        | Instr::I64Le { dst, .. }
        | Instr::I64Ge { dst, .. }
        | Instr::F64Eq { dst, .. }
        | Instr::F64Lt { dst, .. }
        | Instr::F64Gt { dst, .. }
        | Instr::F64Le { dst, .. }
        | Instr::F64Ge { dst, .. }
        | Instr::BoolNot { dst, .. }
        | Instr::BoolAnd { dst, .. }
        | Instr::BoolOr { dst, .. }
        | Instr::BoolXor { dst, .. } => set_value(state, *dst, ValueType::Bool),
        Instr::BytesEq { dst, .. } | Instr::StrEq { dst, .. } => {
            set_value(state, *dst, ValueType::Bool);
        }
        Instr::Select { dst, a, .. } => {
            let t = state.values.get(*a as usize).copied().unwrap_or(None);
            match t {
                Some(t) => set_reg_type(state, *dst, t),
                None => {
                    if let Some(slot) = state.values.get_mut(*dst as usize) {
                        *slot = None;
                    }
                    clear_agg(state, *dst);
                }
            }
        }
        Instr::BytesConcat { dst, .. }
        | Instr::BytesSlice { dst, .. }
        | Instr::StrToBytes { dst, .. } => set_value(state, *dst, ValueType::Bytes),
        Instr::StrConcat { dst, .. }
        | Instr::StrSlice { dst, .. }
        | Instr::BytesToStr { dst, .. } => {
            set_value(state, *dst, ValueType::Str);
        }
        Instr::BytesGet { dst, .. } | Instr::BytesGetImm { dst, .. } => {
            set_value(state, *dst, ValueType::U64);
        }
        Instr::Call {
            eff_out,
            func_id,
            rets,
            ..
        } => {
            set_value(state, *eff_out, ValueType::Unit);
            if let Some(callee) = program.functions.get(func_id.0 as usize)
                && let Ok(types) = callee.ret_types(program)
            {
                for (dst, t) in rets.iter().zip(types.iter().copied()) {
                    set_value(state, *dst, t);
                }
                return;
            }
            for dst in rets {
                set_ambiguous(state, *dst);
            }
        }
        Instr::HostCall {
            eff_out,
            host_sig,
            rets,
            ..
        } => {
            set_value(state, *eff_out, ValueType::Unit);
            let Some(hs) = program.host_sig(*host_sig) else {
                for dst in rets {
                    set_ambiguous(state, *dst);
                }
                return;
            };
            if let Ok(types) = program.host_sig_rets(hs) {
                for (dst, t) in rets.iter().zip(types.iter().copied()) {
                    set_value(state, *dst, t);
                }
            } else {
                for dst in rets {
                    set_ambiguous(state, *dst);
                }
            }
        }
        Instr::TupleNew { dst, values } => {
            let mut elems: Vec<Option<ValueType>> = Vec::with_capacity(values.len());
            for &r in values {
                let t = state.values.get(r as usize).copied().flatten();
                elems.push(match t {
                    Some(RegType::Concrete(ty)) => Some(ty),
                    _ => None,
                });
            }
            set_agg(state, *dst, Some(AggMeta::Tuple(elems)));
        }
        Instr::StructNew { dst, type_id, .. } => {
            set_agg(state, *dst, Some(AggMeta::Struct(*type_id)));
        }
        Instr::ArrayNew {
            dst, elem_type_id, ..
        } => {
            set_agg(state, *dst, Some(AggMeta::Array(*elem_type_id)));
        }
        Instr::TupleGet { dst, tuple, index } => {
            let out = match state.aggs.get(*tuple as usize).and_then(|m| m.as_ref()) {
                Some(AggMeta::Tuple(elems)) => elems.get(*index as usize).and_then(|t| *t),
                _ => None,
            };
            match out {
                Some(t) => set_value(state, *dst, t),
                None => set_ambiguous(state, *dst),
            }
        }
        Instr::StructGet {
            dst,
            st,
            field_index,
        } => {
            let out = match state.aggs.get(*st as usize).and_then(|m| m.as_ref()) {
                Some(AggMeta::Struct(type_id)) => program
                    .types
                    .structs
                    .get(type_id.0 as usize)
                    .and_then(|st| program.types.struct_field_types(st).ok())
                    .and_then(|tys| tys.get(*field_index as usize))
                    .copied()
                    .map(RegType::Concrete)
                    .unwrap_or(RegType::Ambiguous),
                _ => RegType::Ambiguous,
            };
            set_reg_type(state, *dst, out);
        }
        Instr::ArrayGet { dst, arr, .. } => {
            let out = match state.aggs.get(*arr as usize).and_then(|m| m.as_ref()) {
                Some(AggMeta::Array(elem_type_id)) => program
                    .types
                    .array_elems
                    .get(elem_type_id.0 as usize)
                    .copied()
                    .map(RegType::Concrete)
                    .unwrap_or(RegType::Ambiguous),
                _ => RegType::Ambiguous,
            };
            set_reg_type(state, *dst, out);
        }
        Instr::ArrayGetImm { dst, arr, .. } => {
            let out = match state.aggs.get(*arr as usize).and_then(|m| m.as_ref()) {
                Some(AggMeta::Array(elem_type_id)) => program
                    .types
                    .array_elems
                    .get(elem_type_id.0 as usize)
                    .copied()
                    .map(RegType::Concrete)
                    .unwrap_or(RegType::Ambiguous),
                _ => RegType::Ambiguous,
            };
            set_reg_type(state, *dst, out);
        }
        Instr::ArrayLen { dst, .. } => set_value(state, *dst, ValueType::U64),
        Instr::TupleLen { dst, .. }
        | Instr::StructFieldCount { dst, .. }
        | Instr::BytesLen { dst, .. }
        | Instr::StrLen { dst, .. } => set_value(state, *dst, ValueType::U64),
    }
}

fn compute_must_types(
    program: &Program,
    blocks: &[BasicBlock],
    reachable: &[bool],
    reg_count: usize,
    entry_types: &TypeState,
    decoded: &[DecodedInstr],
) -> Result<(Vec<TypeState>, Vec<TypeState>), VerifyError> {
    // Must-type analysis: forward, meet-at-joins, transfer through each block. We compute a stable
    // type assignment for each register (or reject instability later).
    let bottom = TypeState {
        values: vec![None; reg_count],
        aggs: vec![None; reg_count],
    };

    let (in_sets, out_sets) = dataflow::solve_forward(
        blocks,
        reachable,
        entry_types.clone(),
        bottom,
        |acc, incoming| {
            for r in 0..reg_count {
                acc.values[r] = meet_value(acc.values[r], incoming.values[r]);
                if matches!(acc.values[r], Some(RegType::Concrete(ValueType::Agg))) {
                    acc.aggs[r] = meet_agg(&acc.aggs[r], &incoming.aggs[r]);
                } else {
                    acc.aggs[r] = None;
                }
            }
        },
        |_b_idx, b, in_state| {
            let mut out = in_state.clone();
            for di in decoded.iter().take(b.instr_end).skip(b.instr_start) {
                transfer_types(program, &di.instr, &mut out);
            }
            out
        },
    );

    Ok((in_sets, out_sets))
}

fn check_expected(
    func: u32,
    pc: u32,
    reg: u32,
    actual: Option<RegType>,
    expected: ValueType,
) -> Result<(), VerifyError> {
    let Some(actual) = actual else {
        return Err(VerifyError::UninitializedRead { func, pc, reg });
    };
    let actual = match actual {
        RegType::Uninit => {
            return Err(VerifyError::UninitializedRead { func, pc, reg });
        }
        RegType::Concrete(t) => t,
        RegType::Ambiguous => {
            return Err(VerifyError::UnknownTypeAtUse {
                func,
                pc,
                reg,
                expected,
            });
        }
    };
    if actual != expected {
        return Err(VerifyError::TypeMismatch {
            func,
            pc,
            expected,
            actual,
        });
    }
    Ok(())
}

fn check_assignable(
    func: u32,
    pc: u32,
    reg: u32,
    actual: Option<RegType>,
    expected: ValueType,
) -> Result<(), VerifyError> {
    let Some(actual) = actual else {
        return Ok(());
    };
    match actual {
        RegType::Uninit => Ok(()),
        RegType::Concrete(t) => {
            if t != expected {
                Err(VerifyError::TypeMismatch {
                    func,
                    pc,
                    expected,
                    actual: t,
                })
            } else {
                Ok(())
            }
        }
        RegType::Ambiguous => Err(VerifyError::UnknownTypeAtUse {
            func,
            pc,
            reg,
            expected,
        }),
    }
}

fn validate_instr_types(
    program: &Program,
    func_id: u32,
    pc: u32,
    instr: &Instr,
    state: &TypeState,
    func_ret_types: &[ValueType],
) -> Result<(), VerifyError> {
    let t = |reg: u32| state.values.get(reg as usize).copied().flatten();
    let a = |reg: u32| state.aggs.get(reg as usize).and_then(|m| m.as_ref());

    match instr {
        Instr::Nop | Instr::Trap { .. } | Instr::Jmp { .. } => {}
        Instr::Mov { .. } => {}
        Instr::ConstUnit { .. }
        | Instr::ConstBool { .. }
        | Instr::ConstI64 { .. }
        | Instr::ConstU64 { .. }
        | Instr::ConstF64 { .. }
        | Instr::ConstDecimal { .. }
        | Instr::ConstFunc { .. } => {}
        Instr::ConstPool { .. } => {}
        Instr::DecAdd { a, b, .. } | Instr::DecSub { a, b, .. } | Instr::DecMul { a, b, .. } => {
            check_expected(func_id, pc, *a, t(*a), ValueType::Decimal)?;
            check_expected(func_id, pc, *b, t(*b), ValueType::Decimal)?;
        }
        Instr::F64Add { a, b, .. }
        | Instr::F64Sub { a, b, .. }
        | Instr::F64Mul { a, b, .. }
        | Instr::F64Div { a, b, .. }
        | Instr::F64Min { a, b, .. }
        | Instr::F64Max { a, b, .. }
        | Instr::F64MinNum { a, b, .. }
        | Instr::F64MaxNum { a, b, .. }
        | Instr::F64Rem { a, b, .. } => {
            check_expected(func_id, pc, *a, t(*a), ValueType::F64)?;
            check_expected(func_id, pc, *b, t(*b), ValueType::F64)?;
        }
        Instr::F64Neg { a, .. } | Instr::F64Abs { a, .. } | Instr::F64ToBits { a, .. } => {
            check_expected(func_id, pc, *a, t(*a), ValueType::F64)?;
        }
        Instr::F64FromBits { a, .. } => {
            check_expected(func_id, pc, *a, t(*a), ValueType::U64)?;
        }
        Instr::I64Add { a, b, .. } | Instr::I64Sub { a, b, .. } | Instr::I64Mul { a, b, .. } => {
            check_expected(func_id, pc, *a, t(*a), ValueType::I64)?;
            check_expected(func_id, pc, *b, t(*b), ValueType::I64)?;
        }
        Instr::I64And { a, b, .. }
        | Instr::I64Or { a, b, .. }
        | Instr::I64Xor { a, b, .. }
        | Instr::I64Shl { a, b, .. }
        | Instr::I64Shr { a, b, .. } => {
            check_expected(func_id, pc, *a, t(*a), ValueType::I64)?;
            check_expected(func_id, pc, *b, t(*b), ValueType::I64)?;
        }
        Instr::U64Add { a, b, .. } | Instr::U64Sub { a, b, .. } | Instr::U64Mul { a, b, .. } => {
            check_expected(func_id, pc, *a, t(*a), ValueType::U64)?;
            check_expected(func_id, pc, *b, t(*b), ValueType::U64)?;
        }
        Instr::U64And { a, b, .. }
        | Instr::U64Or { a, b, .. }
        | Instr::U64Xor { a, b, .. }
        | Instr::U64Shl { a, b, .. }
        | Instr::U64Shr { a, b, .. } => {
            check_expected(func_id, pc, *a, t(*a), ValueType::U64)?;
            check_expected(func_id, pc, *b, t(*b), ValueType::U64)?;
        }
        Instr::I64Eq { a, b, .. } | Instr::I64Lt { a, b, .. } => {
            check_expected(func_id, pc, *a, t(*a), ValueType::I64)?;
            check_expected(func_id, pc, *b, t(*b), ValueType::I64)?;
        }
        Instr::I64Gt { a, b, .. } | Instr::I64Le { a, b, .. } | Instr::I64Ge { a, b, .. } => {
            check_expected(func_id, pc, *a, t(*a), ValueType::I64)?;
            check_expected(func_id, pc, *b, t(*b), ValueType::I64)?;
        }
        Instr::U64Eq { a, b, .. }
        | Instr::U64Lt { a, b, .. }
        | Instr::U64Gt { a, b, .. }
        | Instr::U64Le { a, b, .. }
        | Instr::U64Ge { a, b, .. } => {
            check_expected(func_id, pc, *a, t(*a), ValueType::U64)?;
            check_expected(func_id, pc, *b, t(*b), ValueType::U64)?;
        }
        Instr::BoolNot { a, .. } => {
            check_expected(func_id, pc, *a, t(*a), ValueType::Bool)?;
        }
        Instr::BoolAnd { a, b, .. } | Instr::BoolOr { a, b, .. } | Instr::BoolXor { a, b, .. } => {
            check_expected(func_id, pc, *a, t(*a), ValueType::Bool)?;
            check_expected(func_id, pc, *b, t(*b), ValueType::Bool)?;
        }
        Instr::F64Eq { a, b, .. }
        | Instr::F64Lt { a, b, .. }
        | Instr::F64Gt { a, b, .. }
        | Instr::F64Le { a, b, .. }
        | Instr::F64Ge { a, b, .. } => {
            check_expected(func_id, pc, *a, t(*a), ValueType::F64)?;
            check_expected(func_id, pc, *b, t(*b), ValueType::F64)?;
        }
        Instr::U64ToI64 { a, .. } => {
            check_expected(func_id, pc, *a, t(*a), ValueType::U64)?;
        }
        Instr::I64ToU64 { a, .. } => {
            check_expected(func_id, pc, *a, t(*a), ValueType::I64)?;
        }
        Instr::I64Div { a, b, .. } | Instr::I64Rem { a, b, .. } => {
            check_expected(func_id, pc, *a, t(*a), ValueType::I64)?;
            check_expected(func_id, pc, *b, t(*b), ValueType::I64)?;
        }
        Instr::U64Div { a, b, .. } | Instr::U64Rem { a, b, .. } => {
            check_expected(func_id, pc, *a, t(*a), ValueType::U64)?;
            check_expected(func_id, pc, *b, t(*b), ValueType::U64)?;
        }
        Instr::I64ToF64 { a, .. } => {
            check_expected(func_id, pc, *a, t(*a), ValueType::I64)?;
        }
        Instr::U64ToF64 { a, .. } => {
            check_expected(func_id, pc, *a, t(*a), ValueType::U64)?;
        }
        Instr::F64ToI64 { a, .. } | Instr::F64ToU64 { a, .. } => {
            check_expected(func_id, pc, *a, t(*a), ValueType::F64)?;
        }
        Instr::DecToI64 { a, .. } | Instr::DecToU64 { a, .. } => {
            check_expected(func_id, pc, *a, t(*a), ValueType::Decimal)?;
        }
        Instr::I64ToDec { a, .. } => {
            check_expected(func_id, pc, *a, t(*a), ValueType::I64)?;
        }
        Instr::U64ToDec { a, .. } => {
            check_expected(func_id, pc, *a, t(*a), ValueType::U64)?;
        }
        Instr::BytesEq { a, b, .. } | Instr::BytesConcat { a, b, .. } => {
            check_expected(func_id, pc, *a, t(*a), ValueType::Bytes)?;
            check_expected(func_id, pc, *b, t(*b), ValueType::Bytes)?;
        }
        Instr::StrEq { a, b, .. } | Instr::StrConcat { a, b, .. } => {
            check_expected(func_id, pc, *a, t(*a), ValueType::Str)?;
            check_expected(func_id, pc, *b, t(*b), ValueType::Str)?;
        }
        Instr::BytesGet { bytes, index, .. } => {
            check_expected(func_id, pc, *bytes, t(*bytes), ValueType::Bytes)?;
            check_expected(func_id, pc, *index, t(*index), ValueType::U64)?;
        }
        Instr::BytesGetImm { bytes, .. } => {
            check_expected(func_id, pc, *bytes, t(*bytes), ValueType::Bytes)?;
        }
        Instr::BytesSlice {
            bytes, start, end, ..
        } => {
            check_expected(func_id, pc, *bytes, t(*bytes), ValueType::Bytes)?;
            check_expected(func_id, pc, *start, t(*start), ValueType::U64)?;
            check_expected(func_id, pc, *end, t(*end), ValueType::U64)?;
        }
        Instr::StrSlice { s, start, end, .. } => {
            check_expected(func_id, pc, *s, t(*s), ValueType::Str)?;
            check_expected(func_id, pc, *start, t(*start), ValueType::U64)?;
            check_expected(func_id, pc, *end, t(*end), ValueType::U64)?;
        }
        Instr::StrToBytes { s, .. } => {
            check_expected(func_id, pc, *s, t(*s), ValueType::Str)?;
        }
        Instr::BytesToStr { bytes, .. } => {
            check_expected(func_id, pc, *bytes, t(*bytes), ValueType::Bytes)?;
        }
        Instr::Select { cond, a, b, .. } => {
            check_expected(func_id, pc, *cond, t(*cond), ValueType::Bool)?;
            let ta = t(*a).filter(|t| !matches!(t, RegType::Uninit)).ok_or(
                VerifyError::UninitializedRead {
                    func: func_id,
                    pc,
                    reg: *a,
                },
            )?;
            let tb = t(*b).filter(|t| !matches!(t, RegType::Uninit)).ok_or(
                VerifyError::UninitializedRead {
                    func: func_id,
                    pc,
                    reg: *b,
                },
            )?;
            match (ta, tb) {
                (RegType::Uninit, _) => {
                    return Err(VerifyError::UninitializedRead {
                        func: func_id,
                        pc,
                        reg: *a,
                    });
                }
                (_, RegType::Uninit) => {
                    return Err(VerifyError::UninitializedRead {
                        func: func_id,
                        pc,
                        reg: *b,
                    });
                }
                (RegType::Ambiguous, RegType::Concrete(expected)) => {
                    return Err(VerifyError::UnknownTypeAtUse {
                        func: func_id,
                        pc,
                        reg: *a,
                        expected,
                    });
                }
                (RegType::Concrete(expected), RegType::Ambiguous) => {
                    return Err(VerifyError::UnknownTypeAtUse {
                        func: func_id,
                        pc,
                        reg: *b,
                        expected,
                    });
                }
                (RegType::Ambiguous, RegType::Ambiguous) => {
                    return Err(VerifyError::UnknownTypeAtSelect {
                        func: func_id,
                        pc,
                        reg: *a,
                    });
                }
                (RegType::Concrete(ta), RegType::Concrete(tb)) => {
                    if ta != tb {
                        return Err(VerifyError::TypeMismatch {
                            func: func_id,
                            pc,
                            expected: ta,
                            actual: tb,
                        });
                    }
                }
            }
        }
        Instr::Br { cond, .. } => {
            check_expected(func_id, pc, *cond, t(*cond), ValueType::Bool)?;
        }
        Instr::Call {
            func_id: callee,
            args,
            rets,
            ..
        } => {
            let callee_fn = program
                .functions
                .get(callee.0 as usize)
                .ok_or(VerifyError::CallArityMismatch { func: func_id, pc })?;
            let callee_args = callee_fn.arg_types(program).map_err(|_| {
                VerifyError::CallCalleeArgTypesOutOfBounds {
                    func: func_id,
                    pc,
                    callee: callee.0,
                }
            })?;
            let callee_rets = callee_fn.ret_types(program).map_err(|_| {
                VerifyError::CallCalleeRetTypesOutOfBounds {
                    func: func_id,
                    pc,
                    callee: callee.0,
                }
            })?;
            if args.len() != callee_args.len() || rets.len() != callee_rets.len() {
                return Err(VerifyError::CallArityMismatch { func: func_id, pc });
            }
            for (&r, &expected) in args.iter().zip(callee_args.iter()) {
                check_expected(func_id, pc, r, t(r), expected)?;
            }
            for (&r, &expected) in rets.iter().zip(callee_rets.iter()) {
                check_assignable(func_id, pc, r, t(r), expected)?;
            }
        }
        Instr::Ret { rets, .. } => {
            if rets.len() != func_ret_types.len() {
                return Err(VerifyError::ReturnArityMismatch { func: func_id, pc });
            }
            for (&r, &expected) in rets.iter().zip(func_ret_types.iter()) {
                check_expected(func_id, pc, r, t(r), expected)?;
            }
        }
        Instr::HostCall {
            host_sig,
            args,
            rets,
            ..
        } => {
            let hs = program
                .host_sig(*host_sig)
                .expect("validated by verify_id_operands_in_bounds");
            let hs_args =
                program
                    .host_sig_args(hs)
                    .map_err(|_| VerifyError::HostCallSigMalformed {
                        func: func_id,
                        pc,
                        host_sig: host_sig.0,
                    })?;
            let hs_rets =
                program
                    .host_sig_rets(hs)
                    .map_err(|_| VerifyError::HostCallSigMalformed {
                        func: func_id,
                        pc,
                        host_sig: host_sig.0,
                    })?;
            if args.len() != hs_args.len() || rets.len() != hs_rets.len() {
                return Err(VerifyError::HostCallArityMismatch { func: func_id, pc });
            }
            for (&r, &expected) in args.iter().zip(hs_args.iter()) {
                check_expected(func_id, pc, r, t(r), expected)?;
            }
            for (&r, &expected) in rets.iter().zip(hs_rets.iter()) {
                check_assignable(func_id, pc, r, t(r), expected)?;
            }
        }
        Instr::TupleNew { .. } => {}
        Instr::TupleGet { tuple, index, .. } => {
            check_expected(func_id, pc, *tuple, t(*tuple), ValueType::Agg)?;
            if let Some(meta) = a(*tuple) {
                match meta {
                    AggMeta::Tuple(elems) => {
                        let arity = u32::try_from(elems.len()).unwrap_or(u32::MAX);
                        if *index >= arity {
                            return Err(VerifyError::TupleIndexOutOfBounds {
                                func: func_id,
                                pc,
                                arity,
                                index: *index,
                            });
                        }
                    }
                    AggMeta::Struct(_) => {
                        return Err(VerifyError::AggKindMismatch {
                            func: func_id,
                            pc,
                            expected: AggKind::Tuple,
                            actual: AggKind::Struct,
                        });
                    }
                    AggMeta::Array(_) => {
                        return Err(VerifyError::AggKindMismatch {
                            func: func_id,
                            pc,
                            expected: AggKind::Tuple,
                            actual: AggKind::Array,
                        });
                    }
                }
            }
        }
        Instr::TupleLen { tuple, .. } => {
            check_expected(func_id, pc, *tuple, t(*tuple), ValueType::Agg)?;
            if let Some(meta) = a(*tuple)
                && !matches!(meta, AggMeta::Tuple(_))
            {
                let actual = match meta {
                    AggMeta::Tuple(_) => AggKind::Tuple,
                    AggMeta::Struct(_) => AggKind::Struct,
                    AggMeta::Array(_) => AggKind::Array,
                };
                return Err(VerifyError::AggKindMismatch {
                    func: func_id,
                    pc,
                    expected: AggKind::Tuple,
                    actual,
                });
            }
        }
        Instr::StructNew {
            type_id, values, ..
        } => {
            let st = program.types.structs.get(type_id.0 as usize).ok_or(
                VerifyError::StructTypeOutOfBounds {
                    func: func_id,
                    pc,
                    type_id: type_id.0,
                },
            )?;
            let field_types = program.types.struct_field_types(st).map_err(|_| {
                VerifyError::StructTypeOutOfBounds {
                    func: func_id,
                    pc,
                    type_id: type_id.0,
                }
            })?;
            if values.len() != field_types.len() {
                return Err(VerifyError::StructArityMismatch {
                    func: func_id,
                    pc,
                    type_id: type_id.0,
                });
            }
            for (&r, &expected) in values.iter().zip(field_types.iter()) {
                check_expected(func_id, pc, r, t(r), expected)?;
            }
        }
        Instr::StructGet {
            st, field_index, ..
        } => {
            check_expected(func_id, pc, *st, t(*st), ValueType::Agg)?;
            if let Some(meta) = a(*st) {
                match meta {
                    AggMeta::Struct(type_id) => {
                        let st = program.types.structs.get(type_id.0 as usize).ok_or(
                            VerifyError::StructTypeOutOfBounds {
                                func: func_id,
                                pc,
                                type_id: type_id.0,
                            },
                        )?;
                        let field_types = program.types.struct_field_types(st).map_err(|_| {
                            VerifyError::StructTypeOutOfBounds {
                                func: func_id,
                                pc,
                                type_id: type_id.0,
                            }
                        })?;
                        let fields = u32::try_from(field_types.len()).unwrap_or(u32::MAX);
                        if *field_index >= fields {
                            return Err(VerifyError::StructFieldIndexOutOfBounds {
                                func: func_id,
                                pc,
                                type_id: type_id.0,
                                field_index: *field_index,
                            });
                        }
                    }
                    AggMeta::Tuple(_) => {
                        return Err(VerifyError::AggKindMismatch {
                            func: func_id,
                            pc,
                            expected: AggKind::Struct,
                            actual: AggKind::Tuple,
                        });
                    }
                    AggMeta::Array(_) => {
                        return Err(VerifyError::AggKindMismatch {
                            func: func_id,
                            pc,
                            expected: AggKind::Struct,
                            actual: AggKind::Array,
                        });
                    }
                }
            }
        }
        Instr::StructFieldCount { st, .. } => {
            check_expected(func_id, pc, *st, t(*st), ValueType::Agg)?;
            if let Some(meta) = a(*st)
                && !matches!(meta, AggMeta::Struct(_))
            {
                let actual = match meta {
                    AggMeta::Tuple(_) => AggKind::Tuple,
                    AggMeta::Struct(_) => AggKind::Struct,
                    AggMeta::Array(_) => AggKind::Array,
                };
                return Err(VerifyError::AggKindMismatch {
                    func: func_id,
                    pc,
                    expected: AggKind::Struct,
                    actual,
                });
            }
        }
        Instr::ArrayNew {
            elem_type_id,
            len,
            values,
            ..
        } => {
            if *len as usize != values.len() {
                return Err(VerifyError::ArrayLenMismatch { func: func_id, pc });
            }
            let elem = *program
                .types
                .array_elems
                .get(elem_type_id.0 as usize)
                .expect("validated by verify_id_operands_in_bounds");
            for &r in values {
                check_expected(func_id, pc, r, t(r), elem)?;
            }
        }
        Instr::ArrayLen { arr, .. } => {
            check_expected(func_id, pc, *arr, t(*arr), ValueType::Agg)?;
            if let Some(meta) = a(*arr)
                && !matches!(meta, AggMeta::Array(_))
            {
                let actual = match meta {
                    AggMeta::Tuple(_) => AggKind::Tuple,
                    AggMeta::Struct(_) => AggKind::Struct,
                    AggMeta::Array(_) => AggKind::Array,
                };
                return Err(VerifyError::AggKindMismatch {
                    func: func_id,
                    pc,
                    expected: AggKind::Array,
                    actual,
                });
            }
        }
        Instr::ArrayGet { arr, index, .. } => {
            check_expected(func_id, pc, *arr, t(*arr), ValueType::Agg)?;
            check_expected(func_id, pc, *index, t(*index), ValueType::U64)?;
            if let Some(meta) = a(*arr)
                && !matches!(meta, AggMeta::Array(_))
            {
                let actual = match meta {
                    AggMeta::Tuple(_) => AggKind::Tuple,
                    AggMeta::Struct(_) => AggKind::Struct,
                    AggMeta::Array(_) => AggKind::Array,
                };
                return Err(VerifyError::AggKindMismatch {
                    func: func_id,
                    pc,
                    expected: AggKind::Array,
                    actual,
                });
            }
        }
        Instr::ArrayGetImm { arr, .. } => {
            check_expected(func_id, pc, *arr, t(*arr), ValueType::Agg)?;
            if let Some(meta) = a(*arr)
                && !matches!(meta, AggMeta::Array(_))
            {
                let actual = match meta {
                    AggMeta::Tuple(_) => AggKind::Tuple,
                    AggMeta::Struct(_) => AggKind::Struct,
                    AggMeta::Array(_) => AggKind::Array,
                };
                return Err(VerifyError::AggKindMismatch {
                    func: func_id,
                    pc,
                    expected: AggKind::Array,
                    actual,
                });
            }
        }
        Instr::BytesLen { bytes, .. } => {
            check_expected(func_id, pc, *bytes, t(*bytes), ValueType::Bytes)?;
        }
        Instr::StrLen { s, .. } => {
            check_expected(func_id, pc, *s, t(*s), ValueType::Str)?;
        }
    }
    Ok(())
}

fn compute_must_init(
    blocks: &[BasicBlock],
    reachable: &[bool],
    reg_count: usize,
    entry_init: &BitSet,
    decoded: &[DecodedInstr],
) -> Result<(Vec<BitSet>, Vec<BitSet>), VerifyError> {
    // Must-init analysis: forward, meet-at-joins via intersection. OUT is a "writes-only" transfer:
    // OUT = IN ∪ WRITES(block).
    let top = BitSet::new_full(reg_count);
    let writes = compute_block_writes(blocks, reachable, reg_count, decoded)?;

    let (in_sets, out_sets) = dataflow::solve_forward(
        blocks,
        reachable,
        entry_init.clone(),
        top,
        |acc, incoming| acc.intersect_with(incoming),
        |b_idx, _b, in_state| {
            let mut out = in_state.clone();
            out.union_with(&writes[b_idx]);
            out
        },
    );

    Ok((in_sets, out_sets))
}

fn compute_block_writes(
    blocks: &[BasicBlock],
    reachable: &[bool],
    reg_count: usize,
    decoded: &[DecodedInstr],
) -> Result<Vec<BitSet>, VerifyError> {
    let mut out = vec![BitSet::new_empty(reg_count); blocks.len()];
    for (i, b) in blocks.iter().enumerate() {
        if !reachable[i] {
            continue;
        }
        let mut s = BitSet::new_empty(reg_count);
        for di in decoded.iter().take(b.instr_end).skip(b.instr_start) {
            for w in di.instr.writes() {
                if (w as usize) < reg_count {
                    s.set(w as usize);
                }
            }
        }
        out[i] = s;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asm::Asm;
    use crate::asm::{BuildError, FunctionSig, ProgramBuilder};
    use crate::opcode::Opcode;
    use crate::program::{
        Const, FunctionDef, HostSymbol, SpanId, StructTypeDef, TypeTableDef, ValueType,
    };
    use crate::value::FuncId;
    use alloc::vec;

    fn sid(v: u64) -> SpanId {
        SpanId::try_from(v).unwrap()
    }

    #[test]
    fn verifier_accepts_minimal_program() {
        let p = Program::new(
            vec![HostSymbol { symbol: "x".into() }],
            vec![],
            vec![],
            TypeTableDef::default(),
            vec![FunctionDef {
                arg_types: vec![],
                ret_types: vec![],
                reg_count: 2,
                // const_unit r1; ret r0
                bytecode: vec![Opcode::ConstUnit as u8, 0x01, Opcode::Ret as u8, 0x00, 0x00],
                spans: vec![SpanEntry {
                    pc_delta: 0,
                    span_id: sid(1),
                }],
            }],
        );
        verify_program(&p, &VerifyConfig::default()).unwrap();
    }

    #[test]
    fn verifier_rejects_duplicate_function_output_names() {
        let mut pb = ProgramBuilder::new();

        let mut a = Asm::new();
        a.const_i64(1, 1);
        a.const_i64(2, 2);
        a.ret(0, &[1, 2]);
        let f = pb
            .push_function_checked(
                a,
                FunctionSig {
                    arg_types: vec![],
                    ret_types: vec![ValueType::I64, ValueType::I64],
                },
            )
            .unwrap();

        pb.set_function_output_name(f, 0, "x").unwrap();
        pb.set_function_output_name(f, 1, "x").unwrap();

        let err = pb.build_verified().unwrap_err();
        assert!(matches!(
            err,
            BuildError::Verify(VerifyError::FunctionOutputNameDuplicate { func: 0, .. })
        ));
    }

    #[test]
    fn verifier_rejects_duplicate_function_input_names() {
        let mut pb = ProgramBuilder::new();

        let mut a = Asm::new();
        a.ret(0, &[1]);
        let f = pb
            .push_function_checked(
                a,
                FunctionSig {
                    arg_types: vec![ValueType::I64, ValueType::I64],
                    ret_types: vec![ValueType::I64],
                },
            )
            .unwrap();

        pb.set_function_input_name(f, 0, "x").unwrap();
        pb.set_function_input_name(f, 1, "x").unwrap();

        let err = pb.build_verified().unwrap_err();
        assert!(matches!(
            err,
            BuildError::Verify(VerifyError::FunctionInputNameDuplicate { func: 0, .. })
        ));
    }

    #[test]
    fn verifier_rejects_missing_terminator() {
        let p = Program::new(
            vec![],
            vec![],
            vec![],
            TypeTableDef::default(),
            vec![FunctionDef {
                arg_types: vec![],
                ret_types: vec![],
                reg_count: 1,
                // const_bool r1, 1 (non-terminator); block falls through to end (no terminator)
                bytecode: vec![Opcode::ConstBool as u8, 0x01, 0x01],
                spans: vec![],
            }],
        );
        assert_eq!(
            verify_program(&p, &VerifyConfig::default()),
            Err(VerifyError::MissingTerminator { func: 0, pc: 0 })
        );
    }

    #[test]
    fn verifier_rejects_fallthrough_between_blocks() {
        // Entry branches to either l1 or l2. l1 contains a non-terminator and then falls through
        // into l2 (since l2 is also a leader). This is now forbidden.
        let mut a = Asm::new();
        let l1 = a.label();
        let l2 = a.label();
        a.const_bool(1, true);
        a.br(1, l1, l2);

        a.place(l1).unwrap();
        let pc_l1_last = a.pc();
        a.const_i64(2, 1);
        // No terminator here; block falls through to l2.

        a.place(l2).unwrap();
        a.ret(0, &[]);

        let mut pb = ProgramBuilder::new();
        pb.push_function_checked(
            a,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![],
            },
        )
        .unwrap();
        let p = pb.build();

        assert_eq!(
            verify_program(&p, &VerifyConfig::default()),
            Err(VerifyError::MissingTerminator {
                func: 0,
                pc: pc_l1_last
            })
        );
    }

    #[test]
    fn verifier_rejects_missing_terminators_in_unreachable_blocks() {
        // Entry jumps to l_ret, making the subsequent region unreachable.
        //
        // The unreachable region contains multiple blocks and includes an implicit fallthrough
        // between blocks (missing terminator). This is now rejected: we require explicit
        // terminators for all blocks, reachable or not.
        let mut a = Asm::new();
        let l_ret = a.label();
        let l_unreach_entry = a.label();
        let l_bad = a.label();
        let l_next = a.label();

        // Ensure regs used in unreachable blocks still have stable concrete types for the whole
        // function (the verifier still typechecks and lowers unreachable bytecode).
        a.const_bool(1, true);
        a.const_i64(2, 0);
        a.jmp(l_ret);

        a.place(l_unreach_entry).unwrap();
        a.br(1, l_bad, l_next);

        a.place(l_bad).unwrap();
        let pc_bad_last = a.pc();
        a.const_i64(2, 1);
        // Missing terminator: falls through to l_next.

        a.place(l_next).unwrap();
        a.ret(0, &[]);

        a.place(l_ret).unwrap();
        a.ret(0, &[]);

        let mut pb = ProgramBuilder::new();
        pb.push_function_checked(
            a,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![],
            },
        )
        .unwrap();
        let p = pb.build();
        assert_eq!(
            verify_program(&p, &VerifyConfig::default()),
            Err(VerifyError::MissingTerminator {
                func: 0,
                pc: pc_bad_last
            })
        );
    }

    #[test]
    fn verifier_rejects_span_pc_past_end() {
        let p = Program::new(
            vec![],
            vec![Const::Unit],
            vec![],
            TypeTableDef::default(),
            vec![FunctionDef {
                arg_types: vec![],
                ret_types: vec![],
                reg_count: 1,
                bytecode: vec![0, 1, 2],
                spans: vec![SpanEntry {
                    pc_delta: 999,
                    span_id: sid(1),
                }],
            }],
        );
        assert_eq!(
            verify_program(&p, &VerifyConfig::default()),
            Err(VerifyError::BadSpanDeltas { func: 0 })
        );
    }

    #[test]
    fn verifier_rejects_huge_reg_count() {
        let cfg = VerifyConfig {
            max_regs_per_function: 8,
        };
        let p = Program::new(
            vec![],
            vec![],
            vec![],
            TypeTableDef::default(),
            vec![FunctionDef {
                arg_types: vec![],
                ret_types: vec![],
                reg_count: 9,
                bytecode: vec![0],
                spans: vec![],
            }],
        );
        assert_eq!(
            verify_program(&p, &cfg),
            Err(VerifyError::RegCountTooLarge {
                func: 0,
                reg_count: 9
            })
        );
    }

    #[test]
    fn verifier_rejects_uninitialized_read() {
        // br r1, 0, 0  (reads r1, which is uninitialized at entry)
        let mut a = Asm::new();
        let l0 = a.label();
        a.place(l0).unwrap();
        a.br(1, l0, l0);
        let bytecode = a.finish().unwrap();
        let p = Program::new(
            vec![],
            vec![],
            vec![],
            TypeTableDef::default(),
            vec![FunctionDef {
                arg_types: vec![],
                ret_types: vec![],
                reg_count: 2,
                bytecode,
                spans: vec![],
            }],
        );
        assert_eq!(
            verify_program(&p, &VerifyConfig::default()),
            Err(VerifyError::UninitializedRead {
                func: 0,
                pc: 0,
                reg: 1
            })
        );
    }

    #[test]
    fn verifier_rejects_invalid_jump_target() {
        // const_unit r1; jmp 1 (target is not an instruction boundary)
        let mut a = Asm::new();
        let l_good = a.label();
        a.place(l_good).unwrap();
        a.const_unit(1);
        // Insert an intentionally invalid jump target: byte offset 1 is not an instruction boundary.
        // We build the bytes manually here to keep the test focused on boundary rejection.
        let mut bytecode = a.finish().unwrap();
        bytecode.extend_from_slice(&[0x41, 0x01]);
        let p = Program::new(
            vec![],
            vec![],
            vec![],
            TypeTableDef::default(),
            vec![FunctionDef {
                arg_types: vec![],
                ret_types: vec![],
                reg_count: 2,
                bytecode,
                spans: vec![],
            }],
        );
        assert_eq!(
            verify_program(&p, &VerifyConfig::default()),
            Err(VerifyError::InvalidJumpTarget {
                func: 0,
                pc: 2,
                target: 1,
                reason: InvalidJumpTargetReason::NotInstructionBoundary,
            })
        );
    }

    #[test]
    fn verifier_rejects_out_of_range_jump_target() {
        // const_unit r1; jmp 255 (target is out of range)
        let mut a = Asm::new();
        a.const_unit(1);

        let mut bytecode = a.finish().unwrap();
        bytecode.extend_from_slice(&[0x41, 0xFF, 0x01]); // jmp 255 (uleb128)

        let p = Program::new(
            vec![],
            vec![],
            vec![],
            TypeTableDef::default(),
            vec![FunctionDef {
                arg_types: vec![],
                ret_types: vec![],
                reg_count: 2,
                bytecode,
                spans: vec![],
            }],
        );

        assert_eq!(
            verify_program(&p, &VerifyConfig::default()),
            Err(VerifyError::InvalidJumpTarget {
                func: 0,
                pc: 2,
                target: 255,
                reason: InvalidJumpTargetReason::OutOfRange,
            })
        );
    }

    #[test]
    fn verifier_rejects_call_with_nonzero_effect_reg() {
        // Enforce the v1 convention: effect token must be `r0`.
        let mut pb = ProgramBuilder::new();
        pb.push_function_checked(
            {
                let mut callee = Asm::new();
                callee.ret(0, &[]);
                callee
            },
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![],
            },
        )
        .unwrap();

        pb.push_function_checked(
            {
                let mut caller = Asm::new();
                // eff_in = r1 (invalid), eff_out = r0
                caller.call(0, FuncId(0), 1, &[], &[]);
                caller.ret(0, &[]);
                caller
            },
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![],
            },
        )
        .unwrap();
        let p = pb.build();

        assert_eq!(
            verify_program(&p, &VerifyConfig::default()),
            Err(VerifyError::EffectInNotR0 {
                func: 1,
                pc: 0,
                reg: 1
            })
        );
    }

    #[test]
    fn verifier_rejects_call_arity_mismatch() {
        // Caller calls callee(1 arg) with 0 args.
        let caller = FunctionDef {
            arg_types: vec![],
            ret_types: vec![],
            reg_count: 2,
            // call r0, func=1, r0, argc=0, retc=0; ret r0
            bytecode: {
                let mut a = Asm::new();
                a.call(0, FuncId(1), 0, &[], &[]);
                a.ret(0, &[]);
                a.finish().unwrap()
            },
            spans: vec![],
        };
        let callee = FunctionDef {
            arg_types: vec![ValueType::I64],
            ret_types: vec![],
            reg_count: 2,
            bytecode: vec![],
            spans: vec![],
        };
        let p = Program::new(
            vec![],
            vec![Const::Unit],
            vec![],
            TypeTableDef::default(),
            vec![caller, callee],
        );
        assert_eq!(
            verify_program(&p, &VerifyConfig::default()),
            Err(VerifyError::CallArityMismatch { func: 0, pc: 0 })
        );
    }

    #[test]
    fn verifier_rejects_func_id_out_of_bounds() {
        let caller = FunctionDef {
            arg_types: vec![],
            ret_types: vec![],
            reg_count: 2,
            bytecode: {
                let mut a = Asm::new();
                a.call(0, FuncId(99), 0, &[], &[]);
                a.ret(0, &[]);
                a.finish().unwrap()
            },
            spans: vec![],
        };
        let callee0 = FunctionDef {
            arg_types: vec![],
            ret_types: vec![],
            reg_count: 1,
            bytecode: vec![],
            spans: vec![],
        };
        let p = Program::new(
            vec![],
            vec![Const::Unit],
            vec![],
            TypeTableDef::default(),
            vec![caller, callee0],
        );
        assert_eq!(
            verify_program(&p, &VerifyConfig::default()),
            Err(VerifyError::FuncIdOutOfBounds {
                func: 0,
                pc: 0,
                func_id: 99
            })
        );
    }

    #[test]
    fn verifier_rejects_i64_add_type_mismatch() {
        let mut a = Asm::new();
        a.const_bool(1, true);
        a.const_i64(2, 1);
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
        let p = pb.build();

        let err = verify_program(&p, &VerifyConfig::default()).unwrap_err();
        match err {
            VerifyError::TypeMismatch {
                expected, actual, ..
            } => {
                assert_eq!(expected, ValueType::I64);
                assert_eq!(actual, ValueType::Bool);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn verifier_rejects_array_new_wrong_elem_type() {
        let mut pb = ProgramBuilder::new();
        let elem = pb.array_elem(ValueType::U64);

        let mut a = Asm::new();
        a.const_i64(1, 7);
        a.array_new(2, elem, &[1]);
        a.ret(0, &[2]);

        pb.push_function_checked(
            a,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::Agg],
            },
        )
        .unwrap();
        let p = pb.build();

        let err = verify_program(&p, &VerifyConfig::default()).unwrap_err();
        match err {
            VerifyError::TypeMismatch {
                expected, actual, ..
            } => {
                assert_eq!(expected, ValueType::U64);
                assert_eq!(actual, ValueType::I64);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn verifier_rejects_struct_new_wrong_arity() {
        let mut pb = ProgramBuilder::new();
        let st = pb.struct_type(StructTypeDef {
            field_names: vec!["a".into(), "b".into()],
            field_types: vec![ValueType::I64, ValueType::I64],
        });

        let mut a = Asm::new();
        a.const_i64(1, 1);
        a.struct_new(2, st, &[1]);
        a.ret(0, &[2]);

        pb.push_function_checked(
            a,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::Agg],
            },
        )
        .unwrap();
        let p = pb.build();

        let err = verify_program(&p, &VerifyConfig::default()).unwrap_err();
        assert!(matches!(err, VerifyError::StructArityMismatch { .. }));
    }

    #[test]
    fn verifier_types_tuple_get_element() {
        // tuple_new [i64, i64]; tuple_get -> i64; i64_add uses it.
        let mut a = Asm::new();
        a.const_i64(1, 3);
        a.const_i64(2, 4);
        a.tuple_new(3, &[1, 2]);
        a.tuple_get(4, 3, 0);
        a.i64_add(5, 4, 2);
        a.ret(0, &[5]);

        let mut pb = ProgramBuilder::new();
        pb.push_function_checked(
            a,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
            },
        )
        .unwrap();
        pb.build_checked().unwrap();
    }

    #[test]
    fn verifier_rejects_tuple_get_oob_index() {
        let mut a = Asm::new();
        a.const_i64(1, 3);
        a.tuple_new(2, &[1]);
        a.tuple_get(3, 2, 9);
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
        let p = pb.build();

        let err = verify_program(&p, &VerifyConfig::default()).unwrap_err();
        assert!(matches!(err, VerifyError::TupleIndexOutOfBounds { .. }));
    }

    #[test]
    fn verifier_types_struct_get_field() {
        let mut pb = ProgramBuilder::new();
        let st = pb.struct_type(StructTypeDef {
            field_names: vec!["cond".into(), "x".into()],
            field_types: vec![ValueType::Bool, ValueType::I64],
        });

        let mut a = Asm::new();
        let l_then = a.label();
        let l_else = a.label();
        a.const_bool(1, true);
        a.const_i64(2, 1);
        a.struct_new(3, st, &[1, 2]);
        a.struct_get(4, 3, 0);
        a.br(4, l_then, l_else);
        a.place(l_then).unwrap();
        a.ret(0, &[2]);
        a.place(l_else).unwrap();
        a.const_i64(5, 2);
        a.ret(0, &[5]);

        pb.push_function_checked(
            a,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
            },
        )
        .unwrap();
        pb.build_checked().unwrap();
    }

    #[test]
    fn verifier_rejects_struct_get_oob_field() {
        let mut pb = ProgramBuilder::new();
        let st = pb.struct_type(StructTypeDef {
            field_names: vec!["x".into()],
            field_types: vec![ValueType::I64],
        });

        let mut a = Asm::new();
        a.const_i64(1, 0);
        a.struct_new(2, st, &[1]);
        a.struct_get(3, 2, 7);
        a.ret(0, &[3]);

        pb.push_function_checked(
            a,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
            },
        )
        .unwrap();
        let p = pb.build();

        let err = verify_program(&p, &VerifyConfig::default()).unwrap_err();
        assert!(matches!(
            err,
            VerifyError::StructFieldIndexOutOfBounds { .. }
        ));
    }

    #[test]
    fn verifier_rejects_agg_kind_mismatch() {
        let mut a = Asm::new();
        a.const_i64(1, 3);
        a.tuple_new(2, &[1]);
        a.struct_get(3, 2, 0);
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
        let p = pb.build();

        let err = verify_program(&p, &VerifyConfig::default()).unwrap_err();
        assert!(matches!(err, VerifyError::AggKindMismatch { .. }));
    }

    #[test]
    fn verifier_lints_unreachable_dead_store_redundant_mov_and_unused_call_ret() {
        let mut pb = ProgramBuilder::new();

        // f0: returns i64
        pb.push_function_checked(
            {
                let mut a = Asm::new();
                a.const_i64(1, 5);
                a.ret(0, &[1]);
                a
            },
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
            },
        )
        .unwrap();

        // f1: has redundant mov, dead store, unused call ret, and unreachable block.
        pb.push_function_checked(
            {
                let mut a = Asm::new();
                let l_ret = a.label();
                let l_unreach = a.label();

                a.const_i64(1, 1); // dead store
                a.const_i64(2, 0);
                a.const_i64(4, 0);
                a.mov(2, 2); // redundant
                a.call(0, FuncId(0), 0, &[], &[3]); // unused call ret r3
                a.jmp(l_ret);

                a.place(l_unreach).unwrap();
                a.const_i64(4, 99);
                a.ret(0, &[4]);

                a.place(l_ret).unwrap();
                a.ret(0, &[]);
                a
            },
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![],
            },
        )
        .unwrap();

        let p = pb.build();
        let lints = verify_program_with_lints(&p, &VerifyConfig::default()).unwrap();

        assert!(
            lints
                .iter()
                .any(|l| matches!(l, VerifyLint::UnreachableBlock { func: 1, .. }))
        );
        assert!(lints.iter().any(|l| matches!(
            l,
            VerifyLint::DeadStore {
                func: 1,
                reg: 1,
                ..
            }
        )));
        assert!(lints.iter().any(|l| matches!(
            l,
            VerifyLint::RedundantMove {
                func: 1,
                dst: 2,
                src: 2,
                ..
            }
        )));
        assert!(lints.iter().any(|l| matches!(
            l,
            VerifyLint::UnusedCallReturn {
                func: 1,
                reg: 3,
                ..
            }
        )));
    }
}
