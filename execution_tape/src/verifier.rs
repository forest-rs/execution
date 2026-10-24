// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Program verification for `execution_tape`.
//!
//! Verification rejects malformed programs before they reach the interpreter. It checks container
//! bounds and consistency, control flow, initialization, and types, then lowers bytecode to typed
//! execution artifacts. The `_with_lints` entry points additionally run liveness-based diagnostics.

mod blocks;
mod contracts;
use contracts::RefinementState;
mod diagnostic;
mod inspection;
mod metadata;
pub use blocks::{BlockDeclarationError, BlockEdgeError};

pub use contracts::inspection::{
    EdgeBinding, FactOrigin, FunctionAnalysis, IncomingEdge, InstructionFacts, ProgramAnalysis,
    RegisterFact, ValueFact,
};
pub use inspection::{FunctionStats, InstructionInfo, VerifiedFunction};
pub use metadata::MetadataTable;

use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use crate::analysis::cfg::BasicBlock;
use crate::analysis::dataflow;
use crate::analysis::liveness;
use crate::bytecode::{DecodedInstr, Instr, decode_instructions};
use crate::contract::{Contract, ContractKinds, ContractPosition, ContractViolation};
use crate::format::DecodeError;
use crate::host::{SigHash, sig_hash_slices};
use crate::instr_operands;
use crate::opcode::Opcode;
use crate::program::{
    CallSigId, ConstEntry, ElemTypeId, Function, Program, RegisterTypes, SpanEntry, SpanId,
    SymbolId, TypeId, ValueType,
};
use crate::typed::{
    AggReg, BoolReg, BytesReg, ClosureReg, DecimalReg, ExecDecoded, ExecFunc, ExecInstr, F64Reg,
    FuncReg, I64Reg, ObjReg, RegClass, RegCounts, RegLayout, StrReg, U64Reg, UnitReg, VReg,
    VRegSlice,
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
    signature_cache: SignatureCache,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct SigFingerprint {
    pub(crate) hash: SigHash,
    pub(crate) key: u32,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct FunctionSigFingerprints {
    direct: SigFingerprint,
    closure: Option<SigFingerprint>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SignatureCache {
    call_sigs: Vec<SigFingerprint>,
    functions: Vec<FunctionSigFingerprints>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CanonicalSignature {
    args: Vec<crate::contract::ContractId>,
    rets: Vec<crate::contract::ContractId>,
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

    /// Borrows the prepared execution facts for `func`, or returns `None` for an unknown id.
    /// No bytecode decoding or allocation is needed.
    #[must_use]
    pub fn function(&self, func: FuncId) -> Option<VerifiedFunction<'_>> {
        Some(VerifiedFunction {
            program: self,
            id: func,
            definition: self.program.functions.get(func.0 as usize)?,
            executable: self.verified(func)?,
        })
    }

    /// Iterates prepared functions in function-id order without allocation or decoding.
    pub fn functions(&self) -> impl ExactSizeIterator<Item = VerifiedFunction<'_>> + '_ {
        self.verified_functions
            .iter()
            .enumerate()
            .map(|(index, executable)| VerifiedFunction {
                program: self,
                id: FuncId(u32::try_from(index).expect("verified function index")),
                definition: &self.program.functions[index],
                executable,
            })
    }

    #[must_use]
    pub(crate) fn verified(&self, func: FuncId) -> Option<&ExecFunc> {
        self.verified_functions.get(func.0 as usize)
    }

    #[must_use]
    pub(crate) fn call_sig_fingerprint(&self, call_sig: CallSigId) -> Option<SigFingerprint> {
        self.signature_cache
            .call_sigs
            .get(call_sig.0 as usize)
            .copied()
    }

    #[must_use]
    pub(crate) fn function_direct_sig_fingerprint(&self, func: FuncId) -> Option<SigFingerprint> {
        self.signature_cache
            .functions
            .get(func.0 as usize)
            .map(|s| s.direct)
    }

    #[must_use]
    pub(crate) fn function_closure_sig_fingerprint(&self, func: FuncId) -> Option<SigFingerprint> {
        self.signature_cache
            .functions
            .get(func.0 as usize)
            .and_then(|s| s.closure)
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
    /// A statically known callable contradicts an indirect call's exact signature.
    CallIndirectContractMismatch {
        /// Containing function.
        func: u32,
        /// Call instruction byte offset.
        pc: u32,
        /// Required indirect signature.
        call_sig: u32,
    },
    /// A compositional contract contradicts its nominal schema, even if it is unused.
    InvalidContract {
        /// Program-local contract definition.
        contract: crate::contract::ContractId,
        /// Invalid nominal identity, field count or member kind.
        error: crate::contract::ContractDefinitionError,
    },

    /// A statically known argument or result contradicts its compositional contract.
    ContractMismatch {
        /// Function containing the call or return instruction.
        func: u32,
        /// Source instruction byte offset.
        pc: u32,
        /// Argument/result obligation.
        position: ContractPosition,
        /// Nested disagreement with the originating program's contract.
        error: ContractViolation,
    },

    /// A typed block declaration is invalid.
    BlockDeclaration {
        /// Function index.
        func: u32,
        /// Header byte offset.
        pc: u32,
        /// Invalid declaration component.
        error: BlockDeclarationError,
    },
    /// A source edge does not satisfy its target's block contract.
    BlockEdge {
        /// Function index.
        func: u32,
        /// Source branch byte offset.
        pc: u32,
        /// Zero for jump/true; one for false.
        arm: usize,
        /// Source target header byte offset.
        target: u32,
        /// Failed contract component.
        error: BlockEdgeError,
    },
    /// Immutable program metadata is malformed, including entries unused by bytecode.
    InvalidMetadata {
        /// Table containing the invalid entry.
        table: MetadataTable,
        /// Zero-based table entry (function index for signature name ranges).
        entry: u32,
        /// Invalid range, count, identifier, or UTF-8 boundary.
        error: DecodeError,
    },
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
    /// A decoded opcode is recognized but not yet supported by verifier lowering.
    UnsupportedInstruction {
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
    /// A function uses a value type not yet supported by the verifier/runtime register model.
    UnsupportedValueType {
        /// Function index within the program.
        func: u32,
        /// Unsupported value type.
        value_type: ValueType,
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
    /// A `call.indirect` references an out-of-bounds call signature id.
    CallSigOutOfBounds {
        /// Function index within the program.
        func: u32,
        /// Byte offset of the instruction.
        pc: u32,
        /// Call signature id.
        call_sig: u32,
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
    /// A call signature table entry is malformed.
    CallSigMalformed {
        /// Call signature id.
        call_sig: u32,
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
    /// A `call.indirect` referenced a call signature, but that entry was malformed.
    CallIndirectSigMalformed {
        /// Function index within the program.
        func: u32,
        /// Byte offset of the instruction.
        pc: u32,
        /// Call signature id.
        call_sig: u32,
    },
    /// The program's host signature hash does not match the canonical hash for its types.
    HostSigHashMismatch {
        /// Host signature id.
        host_sig: u32,
    },
    /// A function has no register declarations, including the required r0 effect token.
    MissingRegisterTypes {
        /// Function index.
        func: u32,
    },
    /// A register declaration disagrees with a signature or instruction requirement.
    RegisterTypeMismatch {
        /// Function index.
        func: u32,
        /// Instruction PC; `None` for the effect/argument prefix contract.
        pc: Option<u32>,
        /// Virtual register.
        reg: u32,
        /// Declared kind.
        declared: ValueType,
        /// Required kind.
        required: ValueType,
    },
    /// A projection has no proven element kind. A destination declaration is not a shape proof.
    UnknownAggregateShape {
        /// Function index.
        func: u32,
        /// Projection PC.
        pc: u32,
        /// Aggregate source register.
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
            Self::CallIndirectContractMismatch { func, pc, call_sig } => write!(
                f,
                "function #{func} at {pc}: callable does not match indirect signature #{call_sig}"
            ),
            Self::InvalidContract { contract, error } => {
                write!(f, "invalid contract {}: {error:?}", contract.0)
            }
            Self::ContractMismatch {
                func,
                pc,
                position,
                error,
            } => {
                write!(f, "function #{func} at byte {pc}: ")?;
                error.fmt_at(f, *position)
            }

            Self::BlockDeclaration { func, pc, error } => {
                write!(f, "invalid block: function {func} pc={pc}: {error:?}")
            }
            Self::BlockEdge {
                func,
                pc,
                arm,
                target,
                error,
            } => write!(
                f,
                "invalid block edge: function {func} pc={pc} arm={arm} target={target}: {error:?}"
            ),
            Self::InvalidMetadata {
                table,
                entry,
                error,
            } => write!(f, "invalid {table:?} metadata entry {entry}: {error}"),
            Self::Decode(e) => write!(f, "decode failed: {e}"),
            Self::InternalOpcodeSchemaMismatch { func, pc, opcode } => write!(
                f,
                "internal opcode schema mismatch: function {func} pc={pc} opcode=0x{opcode:02X}"
            ),
            Self::UnsupportedInstruction { func, pc, opcode } => write!(
                f,
                "unsupported instruction at this verifier/runtime stage: function {func} pc={pc} opcode=0x{opcode:02X}"
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
            Self::UnsupportedValueType { func, value_type } => {
                write!(
                    f,
                    "function {func} uses unsupported value type in this verifier/runtime stage: {value_type:?}"
                )
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
            Self::CallSigOutOfBounds { func, pc, call_sig } => {
                write!(
                    f,
                    "function {func} pc={pc} call_sig out of bounds: {call_sig}"
                )
            }
            Self::HostSigMalformed { host_sig } => write!(f, "host_sig {host_sig} malformed"),
            Self::CallSigMalformed { call_sig } => {
                write!(f, "call_sig {call_sig} malformed")
            }
            Self::HostCallSigMalformed { func, pc, host_sig } => write!(
                f,
                "function {func} pc={pc} host_sig {host_sig} malformed (via host_call)"
            ),
            Self::CallIndirectSigMalformed { func, pc, call_sig } => write!(
                f,
                "function {func} pc={pc} call_sig {call_sig} malformed (via call.indirect)"
            ),
            Self::HostSigHashMismatch { host_sig } => {
                write!(f, "host_sig {host_sig} sig_hash mismatch")
            }
            Self::MissingRegisterTypes { func } => write!(
                f,
                "function {func} has no register declarations (r0 must be Unit)"
            ),
            Self::RegisterTypeMismatch {
                func,
                pc,
                reg,
                declared,
                required,
            } => write!(
                f,
                "function {func} pc={pc:?} r{reg} declared {declared:?}, required {required:?}"
            ),
            Self::UnknownAggregateShape { func, pc, reg } => write!(
                f,
                "function {func} pc={pc} r{reg} has no proven projection kind"
            ),
            Self::CallCalleeArgTypesOutOfBounds { func, pc, callee } => write!(
                f,
                "function {func} pc={pc} callee f{callee} arg types out of bounds"
            ),
            Self::CallCalleeRetTypesOutOfBounds { func, pc, callee } => write!(
                f,
                "function {func} pc={pc} callee f{callee} ret types out of bounds"
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
            Self::Decode(e) | Self::InvalidMetadata { error: e, .. } => Some(e),
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

/// Verifies container, control-flow, initialization, type, and lowering rules for `program`.
///
/// Use [`verify_program_with_lints`] to additionally compute non-fatal diagnostics.
pub fn verify_program(program: &Program, cfg: &VerifyConfig) -> Result<(), VerifyError> {
    metadata::verify(program)?;
    verify_host_sigs(program)?;
    verify_call_sigs(program)?;
    verify_function_value_names(program)?;

    for (i, func) in program.functions.iter().enumerate() {
        let func_id = u32::try_from(i).unwrap_or(u32::MAX);
        let _ = verify_function_container(program, func_id, func, cfg, false)?.exec;
    }
    Ok(())
}

/// Verifies `program` and also computes non-fatal lint warnings.
pub fn verify_program_with_lints(
    program: &Program,
    cfg: &VerifyConfig,
) -> Result<Vec<VerifyLint>, VerifyError> {
    metadata::verify(program)?;
    verify_host_sigs(program)?;
    verify_call_sigs(program)?;
    verify_function_value_names(program)?;

    let mut lints: Vec<VerifyLint> = Vec::new();
    for (i, func) in program.functions.iter().enumerate() {
        let func_id = u32::try_from(i).unwrap_or(u32::MAX);
        let out = verify_function_container(program, func_id, func, cfg, true)?;
        lints.extend(out.lints);
    }
    Ok(lints)
}

/// Verifies `program` and returns a [`VerifiedProgram`] wrapper on success.
pub fn verify_program_owned(
    program: Program,
    cfg: &VerifyConfig,
) -> Result<VerifiedProgram, VerifyError> {
    metadata::verify(&program)?;
    verify_host_sigs(&program)?;
    verify_call_sigs(&program)?;
    verify_function_value_names(&program)?;
    let signature_cache = build_signature_cache(&program)?;

    let mut verified_functions: Vec<ExecFunc> = Vec::with_capacity(program.functions.len());
    for (i, func) in program.functions.iter().enumerate() {
        let func_id = u32::try_from(i).unwrap_or(u32::MAX);
        verified_functions
            .push(verify_function_container(&program, func_id, func, cfg, false)?.exec);
    }

    Ok(VerifiedProgram {
        program,
        verified_functions,
        signature_cache,
    })
}

/// Verifies `program`, returning the [`VerifiedProgram`] plus non-fatal lint warnings.
pub fn verify_program_owned_with_lints(
    program: Program,
    cfg: &VerifyConfig,
) -> Result<(VerifiedProgram, Vec<VerifyLint>), VerifyError> {
    metadata::verify(&program)?;
    verify_host_sigs(&program)?;
    verify_call_sigs(&program)?;
    verify_function_value_names(&program)?;
    let signature_cache = build_signature_cache(&program)?;

    let mut verified_functions: Vec<ExecFunc> = Vec::with_capacity(program.functions.len());
    let mut lints: Vec<VerifyLint> = Vec::new();
    for (i, func) in program.functions.iter().enumerate() {
        let func_id = u32::try_from(i).unwrap_or(u32::MAX);
        let out = verify_function_container(&program, func_id, func, cfg, true)?;
        verified_functions.push(out.exec);
        lints.extend(out.lints);
    }

    Ok((
        VerifiedProgram {
            program,
            verified_functions,
            signature_cache,
        },
        lints,
    ))
}

fn build_signature_cache(program: &Program) -> Result<SignatureCache, VerifyError> {
    let mut canonical = Vec::new();
    let mut call_sigs = Vec::with_capacity(program.call_sigs.len());
    for (i, entry) in program.call_sigs.iter().enumerate() {
        let error = |_| VerifyError::CallSigMalformed {
            call_sig: u32::try_from(i).expect("verified function/signature count"),
        };
        let args = program.call_sig_arg_contracts(entry).map_err(error)?;
        let rets = program.call_sig_ret_contracts(entry).map_err(error)?;
        call_sigs.push(SigFingerprint {
            key: intern_signature(&mut canonical, args, rets),
            hash: program.call_sig_hash(entry).map_err(error)?,
        });
    }
    let mut functions = Vec::with_capacity(program.functions.len());
    for (i, func) in program.functions.iter().enumerate() {
        let args =
            func.arg_contracts(program)
                .map_err(|_| VerifyError::FunctionArgTypesOutOfBounds {
                    func: u32::try_from(i).expect("verified function/signature count"),
                })?;
        let rets =
            func.ret_contracts(program)
                .map_err(|_| VerifyError::FunctionRetTypesOutOfBounds {
                    func: u32::try_from(i).expect("verified function/signature count"),
                })?;
        let hash = |args: &[crate::contract::ContractId]| {
            crate::host::sig_hash_iter(
                program.contracts.kinds(args).iter(),
                program.contracts.kinds(rets).iter(),
            )
        };
        let direct = SigFingerprint {
            key: intern_signature(&mut canonical, args, rets),
            hash: hash(args),
        };
        let closure = args
            .split_first()
            .filter(|(first, _)| program.contracts.kind(**first) == Some(ValueType::Agg))
            .map(|(_, rest)| SigFingerprint {
                key: intern_signature(&mut canonical, rest, rets),
                hash: hash(rest),
            });
        functions.push(FunctionSigFingerprints { direct, closure });
    }
    Ok(SignatureCache {
        call_sigs,
        functions,
    })
}
fn intern_signature(
    canonical: &mut Vec<CanonicalSignature>,
    args: &[crate::contract::ContractId],
    rets: &[crate::contract::ContractId],
) -> u32 {
    if let Some(index) = canonical
        .iter()
        .position(|sig| sig.args == args && sig.rets == rets)
    {
        return u32::try_from(index).expect("signature capacity");
    }
    let key = u32::try_from(canonical.len()).expect("signature capacity");
    canonical.push(CanonicalSignature {
        args: args.to_vec(),
        rets: rets.to_vec(),
    });
    key
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

fn verify_call_sigs(program: &Program) -> Result<(), VerifyError> {
    for (i, cs) in program.call_sigs.iter().enumerate() {
        let call_sig = u32::try_from(i).unwrap_or(u32::MAX);
        let _ = program
            .call_sig_args(cs)
            .map_err(|_| VerifyError::CallSigMalformed { call_sig })?;
        let _ = program
            .call_sig_rets(cs)
            .map_err(|_| VerifyError::CallSigMalformed { call_sig })?;
    }
    Ok(())
}

fn verify_function_container(
    program: &Program,
    func_id: u32,
    func: &Function,
    cfg: &VerifyConfig,
    collect_lints: bool,
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

    let register_types = func.verified_register_types(program);
    if register_types.is_empty() {
        return Err(VerifyError::MissingRegisterTypes { func: func_id });
    }
    if arg_types.len() >= register_types.len() {
        return Err(VerifyError::ArgCountExceedsRegs { func: func_id });
    }
    for (reg, required) in core::iter::once(ValueType::Unit)
        .chain(arg_types.iter().copied())
        .enumerate()
    {
        let reg = u32::try_from(reg).expect("register count bounded by u32");
        let declared = register_types
            .get(reg)
            .expect("checked argument prefix bounds");
        if declared != required {
            return Err(VerifyError::RegisterTypeMismatch {
                func: func_id,
                pc: None,
                reg,
                declared,
                required,
            });
        }
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
        program,
        func_id,
        func,
        bytecode,
        spans,
        arg_types,
        ret_types,
        &decoded,
        collect_lints,
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

        if let Instr::CallIndirect { call_sig, .. } = &di.instr
            && program.call_sig(CallSigId(*call_sig)).is_none()
        {
            err = Some(VerifyError::CallSigOutOfBounds {
                func,
                pc,
                call_sig: *call_sig,
            });
        }

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

fn build_span_by_instr_ix(
    decoded: &[DecodedInstr],
    spans: &[SpanEntry],
    executable_count: usize,
) -> Vec<Option<SpanId>> {
    let mut by_ix: Vec<Option<SpanId>> = Vec::with_capacity(executable_count);
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
        if !matches!(di.instr, Instr::Block { .. }) {
            by_ix.push(cur_span);
        }
    }

    by_ix
}

fn verify_function_bytecode(
    program: &Program,
    func_id: u32,
    func: &Function,
    bytecode: &[u8],
    spans: &[SpanEntry],
    arg_types: ContractKinds<'_>,
    ret_types: ContractKinds<'_>,
    decoded: &[DecodedInstr],
    collect_lints: bool,
) -> Result<ExecFuncContainer, VerifyError> {
    if func.reg_count == 0 {
        return Err(VerifyError::ArgCountExceedsRegs { func: func_id });
    }
    // Convention: reg 0 is the effect token; value args occupy regs 1..=arg_count.
    if u64::from(func.arg_count) + 1 > u64::from(func.reg_count) {
        return Err(VerifyError::ArgCountExceedsRegs { func: func_id });
    }

    let register_types = func.verified_register_types(program);
    blocks::validate_structure(func_id, func.reg_count, decoded)?;
    let has_blocks = decoded
        .iter()
        .any(|di| matches!(di.instr, Instr::Block { .. }));
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

    let lints = if collect_lints {
        lint_function_bytecode(func_id, func, decoded, &blocks, &reachable)
    } else {
        Vec::new()
    };

    // Must-init analysis (writes-only transfer).
    let reg_count = func.reg_count as usize;
    let entry_init = initial_init(reg_count, arg_types.len());
    let (in_sets, out_sets) =
        compute_must_init(&blocks, &reachable, reg_count, &entry_init, decoded)?;

    // Validate reads, consuming the block states now that the fixpoint is complete.
    for (b_idx, (block, mut state)) in blocks.iter().zip(in_sets).enumerate() {
        if !reachable[b_idx] {
            state = BitSet::new_full(reg_count);
        }
        for di in decoded.iter().take(block.instr_end).skip(block.instr_start) {
            if has_blocks && reachable[b_idx] {
                blocks::validate_initialized(
                    func_id,
                    di.offset,
                    &di.instr,
                    &state,
                    register_types,
                    decoded,
                )?;
            }
            validate_instr_reads_writes(program, func_id, di.offset, &di.instr, &mut state, func)?;
        }
        if reachable[b_idx] {
            debug_assert_eq!(state, out_sets[b_idx], "must-init OUT mismatch");
        }
    }

    // Map each virtual register to one class-local index while lowering. Only frame counts and
    // argument locations need to survive in the executable layout.
    let mut counts = RegCounts::default();
    let mut reg_map: Vec<VReg> = Vec::with_capacity(reg_count);
    for t in register_types.iter() {
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
            RegClass::Closure => {
                let r = ClosureReg(idx_u32(counts.closures));
                counts.closures += 1;
                VReg::Closure(r)
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
    let mut reg_layout = RegLayout { counts, arg_regs };
    let mut source_to_exec = Vec::new();
    let mut execution_count = decoded.len();
    if has_blocks {
        source_to_exec.reserve(decoded.len());
        let mut exec_ix = 0_u32;
        for di in decoded {
            source_to_exec.push(exec_ix);
            if !matches!(di.instr, Instr::Block { .. }) {
                exec_ix += 1;
            }
        }
        execution_count = exec_ix as usize;
    }

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
        Ok(source_to_exec
            .get(ix)
            .copied()
            .unwrap_or(u32::try_from(ix).expect("instruction index bounded by bytecode length")))
    };

    let mut operands: Vec<VReg> = Vec::new();
    let mut execution_instrs: Vec<ExecDecoded> = Vec::with_capacity(execution_count);
    let mut scratch_regs = Vec::new();
    let mut copy_schedule = blocks::CopyScheduler::default();
    for (source_ix, di) in decoded.iter().enumerate() {
        if has_blocks {
            blocks::validate_types(func_id, di.offset, &di.instr, register_types, decoded)?;
        }
        validate_instr_types(
            program,
            func_id,
            di.offset,
            &di.instr,
            register_types,
            ret_types,
        )?;
        if matches!(di.instr, Instr::Block { .. }) {
            continue;
        }
        let pc = di.offset;
        let map = |reg: u32| -> Result<VReg, VerifyError> {
            reg_map
                .get(reg as usize)
                .copied()
                .ok_or(VerifyError::RegOutOfBounds {
                    func: func_id,
                    pc,
                    reg,
                })
        };
        let mismatch = |reg: u32, required: ValueType| VerifyError::RegisterTypeMismatch {
            func: func_id,
            pc: Some(pc),
            reg,
            declared: register_types
                .get(reg)
                .expect("mapped register has a declared kind"),
            required,
        };
        let map_unit = |reg: u32| -> Result<UnitReg, VerifyError> {
            match map(reg)? {
                VReg::Unit(r) => Ok(r),
                _ => Err(mismatch(reg, ValueType::Unit)),
            }
        };
        let map_bool = |reg: u32| -> Result<BoolReg, VerifyError> {
            match map(reg)? {
                VReg::Bool(r) => Ok(r),
                _ => Err(mismatch(reg, ValueType::Bool)),
            }
        };
        let map_i64 = |reg: u32| -> Result<I64Reg, VerifyError> {
            match map(reg)? {
                VReg::I64(r) => Ok(r),
                _ => Err(mismatch(reg, ValueType::I64)),
            }
        };
        let map_u64 = |reg: u32| -> Result<U64Reg, VerifyError> {
            match map(reg)? {
                VReg::U64(r) => Ok(r),
                _ => Err(mismatch(reg, ValueType::U64)),
            }
        };
        let map_f64 = |reg: u32| -> Result<F64Reg, VerifyError> {
            match map(reg)? {
                VReg::F64(r) => Ok(r),
                _ => Err(mismatch(reg, ValueType::F64)),
            }
        };
        let map_decimal = |reg: u32| -> Result<DecimalReg, VerifyError> {
            match map(reg)? {
                VReg::Decimal(r) => Ok(r),
                _ => Err(mismatch(reg, ValueType::Decimal)),
            }
        };
        let map_bytes = |reg: u32| -> Result<BytesReg, VerifyError> {
            match map(reg)? {
                VReg::Bytes(r) => Ok(r),
                _ => Err(mismatch(reg, ValueType::Bytes)),
            }
        };
        let map_str = |reg: u32| -> Result<StrReg, VerifyError> {
            match map(reg)? {
                VReg::Str(r) => Ok(r),
                _ => Err(mismatch(reg, ValueType::Str)),
            }
        };
        let map_agg = |reg: u32| -> Result<AggReg, VerifyError> {
            match map(reg)? {
                VReg::Agg(r) => Ok(r),
                _ => Err(mismatch(reg, ValueType::Agg)),
            }
        };
        let map_func = |reg: u32| -> Result<FuncReg, VerifyError> {
            match map(reg)? {
                VReg::Func(r) => Ok(r),
                _ => Err(mismatch(reg, ValueType::Func)),
            }
        };
        let map_closure = |reg: u32| -> Result<ClosureReg, VerifyError> {
            match map(reg)? {
                VReg::Closure(r) => Ok(r),
                _ => Err(mismatch(reg, ValueType::Closure)),
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
            Instr::Block { .. } => unreachable!("declarations have no execution step"),
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
                (VReg::Closure(d), VReg::Closure(s)) => ExecInstr::MovClosure { dst: d, src: s },
                _ => {
                    return Err(mismatch(
                        *dst,
                        register_types.get(*src).expect("mapped source"),
                    ));
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
                (VReg::Closure(d), VReg::Closure(aa), VReg::Closure(bb)) => {
                    ExecInstr::SelectClosure {
                        dst: d,
                        cond: map_bool(*cond)?,
                        a: aa,
                        b: bb,
                    }
                }
                _ => {
                    return Err(mismatch(
                        *dst,
                        register_types.get(*a).expect("mapped select source"),
                    ));
                }
            },

            Instr::Br {
                cond,
                pc_true,
                pc_false,
            } => ExecInstr::Br {
                true_copies: VRegSlice { start: 0, len: 0 },
                false_copies: VRegSlice { start: 0, len: 0 },
                cond: map_bool(*cond)?,
                true_ix: map_pc_to_ix(pc, *pc_true)?,
                false_ix: map_pc_to_ix(pc, *pc_false)?,
            },
            Instr::Jmp { pc_target } => ExecInstr::Jmp {
                copies: VRegSlice { start: 0, len: 0 },
                target_ix: map_pc_to_ix(pc, *pc_target)?,
            },

            Instr::JmpArgs { pc_target, args } => {
                let params = blocks::declaration(decoded, *pc_target).unwrap_or(&[]);
                copy_schedule.clear();
                for (&dst, &src) in params.iter().zip(args) {
                    copy_schedule.push(map(dst)?, map(src)?);
                }
                let copies =
                    copy_schedule.lower(&mut operands, &mut reg_layout.counts, &mut scratch_regs);
                ExecInstr::Jmp {
                    target_ix: map_pc_to_ix(pc, *pc_target)?,
                    copies,
                }
            }
            Instr::BrArgs {
                cond,
                pc_true,
                pc_false,
                true_arg_count,
                args,
            } => {
                let (t, f) = args.split_at(*true_arg_count as usize);
                let mut lower_edge = |target, args: &[u32]| -> Result<VRegSlice, VerifyError> {
                    let params = blocks::declaration(decoded, target).unwrap_or(&[]);
                    copy_schedule.clear();
                    for (&dst, &src) in params.iter().zip(args) {
                        copy_schedule.push(map(dst)?, map(src)?);
                    }
                    Ok(copy_schedule.lower(
                        &mut operands,
                        &mut reg_layout.counts,
                        &mut scratch_regs,
                    ))
                };
                ExecInstr::Br {
                    cond: map_bool(*cond)?,
                    true_ix: map_pc_to_ix(pc, *pc_true)?,
                    false_ix: map_pc_to_ix(pc, *pc_false)?,
                    true_copies: lower_edge(*pc_true, t)?,
                    false_copies: lower_edge(*pc_false, f)?,
                }
            }
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
            Instr::CallIndirect {
                eff_out,
                call_sig,
                callee: callee_reg,
                eff_in,
                args,
                rets,
            } => {
                let call_sig = CallSigId(*call_sig);
                let callee = map(*callee_reg)?;
                let args = push_vregs(args)?;
                let rets = push_vregs(rets)?;
                match callee {
                    VReg::Func(callee) => ExecInstr::CallIndirectFunc {
                        eff_out: map_unit(*eff_out)?,
                        call_sig,
                        callee,
                        eff_in: map_unit(*eff_in)?,
                        args,
                        rets,
                    },
                    VReg::Closure(callee) => ExecInstr::CallIndirectClosure {
                        eff_out: map_unit(*eff_out)?,
                        call_sig,
                        callee,
                        eff_in: map_unit(*eff_in)?,
                        args,
                        rets,
                    },
                    _ => {
                        return Err(mismatch(*callee_reg, ValueType::Func));
                    }
                }
            }
            Instr::ClosureNew { dst, func, env } => ExecInstr::ClosureNew {
                dst: map_closure(*dst)?,
                func: map_func(*func)?,
                env: map_agg(*env)?,
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
            end: decoded
                .get(source_ix + 1)
                .map_or(byte_len, |next| next.offset),
            instr: vi,
        });
    }
    contracts::verify_flow(
        program,
        func_id,
        &blocks,
        &reachable,
        register_types,
        decoded,
    )?;
    let mut contract_checks = Vec::new();
    for (index, &contract) in func
        .arg_contracts(program)
        .expect("verified signature")
        .iter()
        .enumerate()
    {
        if !matches!(program.contracts.get(contract), Some(Contract::Kind(_))) {
            contract_checks.push(crate::typed::ContractCheck {
                index: u32::try_from(index).expect("verified signature arity"),
                contract,
            });
        }
    }
    let arg_check_count = u32::try_from(contract_checks.len()).expect("verified argument arity");
    for (index, &contract) in func
        .ret_contracts(program)
        .expect("verified signature")
        .iter()
        .enumerate()
    {
        if !matches!(program.contracts.get(contract), Some(Contract::Kind(_))) {
            contract_checks.push(crate::typed::ContractCheck {
                index: u32::try_from(index).expect("verified signature arity"),
                contract,
            });
        }
    }
    drop(reg_map);
    let span_by_instr_ix = build_span_by_instr_ix(decoded, spans, execution_count);

    Ok(ExecFuncContainer {
        exec: ExecFunc {
            contract_checks,
            arg_check_count,
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
        Instr::CallIndirect {
            eff_out,
            call_sig,
            eff_in,
            args,
            rets,
            ..
        } => {
            require_eff_in_r0(*eff_in)?;
            require_eff_out_r0(*eff_out)?;

            let cs = program
                .call_sig(CallSigId(*call_sig))
                .expect("validated by verify_id_operands_in_bounds");
            let cs_args =
                program
                    .call_sig_args(cs)
                    .map_err(|_| VerifyError::CallIndirectSigMalformed {
                        func: func_id,
                        pc,
                        call_sig: *call_sig,
                    })?;
            let cs_rets =
                program
                    .call_sig_rets(cs)
                    .map_err(|_| VerifyError::CallIndirectSigMalformed {
                        func: func_id,
                        pc,
                        call_sig: *call_sig,
                    })?;
            if args.len() != cs_args.len() || rets.len() != cs_rets.len() {
                return Err(VerifyError::CallArityMismatch { func: func_id, pc });
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

fn check_expected(
    func: u32,
    pc: u32,
    reg: u32,
    actual: Option<ValueType>,
    required: ValueType,
) -> Result<(), VerifyError> {
    let declared = actual.ok_or(VerifyError::RegOutOfBounds { func, pc, reg })?;
    if declared != required {
        return Err(VerifyError::RegisterTypeMismatch {
            func,
            pc: Some(pc),
            reg,
            declared,
            required,
        });
    }
    Ok(())
}

fn validate_instr_types(
    program: &Program,
    func_id: u32,
    pc: u32,
    instr: &Instr,
    types: RegisterTypes<'_>,
    func_ret_types: ContractKinds<'_>,
) -> Result<(), VerifyError> {
    let t = |reg: u32| types.get(reg);

    match instr {
        Instr::Nop
        | Instr::Trap { .. }
        | Instr::Jmp { .. }
        | Instr::JmpArgs { .. }
        | Instr::Block { .. } => {}
        Instr::Mov { dst, src } => {
            let required = t(*src).ok_or(VerifyError::RegOutOfBounds {
                func: func_id,
                pc,
                reg: *src,
            })?;
            check_expected(func_id, pc, *dst, t(*dst), required)?;
        }
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
        Instr::Select { dst, cond, a, b } => {
            check_expected(func_id, pc, *cond, t(*cond), ValueType::Bool)?;
            let required = t(*a).ok_or(VerifyError::RegOutOfBounds {
                func: func_id,
                pc,
                reg: *a,
            })?;
            check_expected(func_id, pc, *b, t(*b), required)?;
            check_expected(func_id, pc, *dst, t(*dst), required)?;
        }
        Instr::Br { cond, .. } | Instr::BrArgs { cond, .. } => {
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
                check_expected(func_id, pc, r, t(r), expected)?;
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
                check_expected(func_id, pc, r, t(r), expected)?;
            }
        }
        Instr::CallIndirect {
            call_sig,
            callee,
            args,
            rets,
            ..
        } => {
            let cs = program
                .call_sig(CallSigId(*call_sig))
                .expect("validated by verify_id_operands_in_bounds");
            let cs_args =
                program
                    .call_sig_args(cs)
                    .map_err(|_| VerifyError::CallIndirectSigMalformed {
                        func: func_id,
                        pc,
                        call_sig: *call_sig,
                    })?;
            let cs_rets =
                program
                    .call_sig_rets(cs)
                    .map_err(|_| VerifyError::CallIndirectSigMalformed {
                        func: func_id,
                        pc,
                        call_sig: *call_sig,
                    })?;
            if args.len() != cs_args.len() || rets.len() != cs_rets.len() {
                return Err(VerifyError::CallArityMismatch { func: func_id, pc });
            }

            if !matches!(t(*callee), Some(ValueType::Func | ValueType::Closure)) {
                check_expected(func_id, pc, *callee, t(*callee), ValueType::Func)?;
            }
            for (&r, &expected) in args.iter().zip(cs_args.iter()) {
                check_expected(func_id, pc, r, t(r), expected)?;
            }
            for (&r, &expected) in rets.iter().zip(cs_rets.iter()) {
                check_expected(func_id, pc, r, t(r), expected)?;
            }
        }
        Instr::ClosureNew { func, env, .. } => {
            check_expected(func_id, pc, *func, t(*func), ValueType::Func)?;
            check_expected(func_id, pc, *env, t(*env), ValueType::Agg)?;
        }
        Instr::TupleNew { .. } => {}
        Instr::TupleGet { tuple, .. } => {
            check_expected(func_id, pc, *tuple, t(*tuple), ValueType::Agg)?;
        }
        Instr::TupleLen { tuple, .. } => {
            check_expected(func_id, pc, *tuple, t(*tuple), ValueType::Agg)?;
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
        Instr::StructGet { st, .. } => {
            check_expected(func_id, pc, *st, t(*st), ValueType::Agg)?;
        }
        Instr::StructFieldCount { st, .. } => {
            check_expected(func_id, pc, *st, t(*st), ValueType::Agg)?;
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
        }
        Instr::ArrayGet { arr, index, .. } => {
            check_expected(func_id, pc, *arr, t(*arr), ValueType::Agg)?;
            check_expected(func_id, pc, *index, t(*index), ValueType::U64)?;
        }
        Instr::ArrayGetImm { arr, .. } => {
            check_expected(func_id, pc, *arr, t(*arr), ValueType::Agg)?;
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
        |_, _, _, acc, incoming| acc.intersect_with(incoming),
        |b_idx, _b, state| {
            state.union_with(&writes[b_idx]);
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
        ByteRange, CallSigEntry, Const, FunctionDef, HostSymbol, Program, SpanId, StructTypeDef,
        TypeTableDef, ValueType,
    };
    use crate::value::FuncId;
    use alloc::vec;

    fn tuple_contract(
        pb: &mut ProgramBuilder,
        kinds: &[Option<ValueType>],
    ) -> crate::contract::ContractId {
        let members: Vec<_> = kinds
            .iter()
            .map(|kind| kind.map(|kind| pb.contract(Contract::Kind(kind)).unwrap()))
            .collect();
        pb.contract(Contract::Tuple(&members)).unwrap()
    }

    fn sid(v: u64) -> SpanId {
        SpanId::try_from(v).unwrap()
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
    fn verifier_accepts_minimal_program() {
        let p = Program::new(
            vec![HostSymbol { symbol: "x".into() }],
            vec![],
            vec![],
            TypeTableDef::default(),
            vec![FunctionDef {
                arg_types: vec![],
                ret_types: vec![],
                register_types: vec![ValueType::Unit, ValueType::Unit],
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
    fn select_replaces_the_destinations_previous_aggregate_shape() {
        for ret_type in [ValueType::Bool, ValueType::I64] {
            let mut asm = Asm::new();
            asm.set_register_types(vec![
                ValueType::Unit,
                ValueType::Bool,
                ValueType::I64,
                ValueType::Agg,
                ValueType::Agg,
                ValueType::Bool,
            ]);
            asm.const_bool(1, true);
            asm.const_i64(2, 7);
            asm.tuple_new(3, &[2]);
            asm.tuple_new(4, &[1]);
            asm.select(3, 1, 4, 4);
            asm.tuple_get(5, 3, 0);
            asm.ret(0, &[5]);
            let mut builder = ProgramBuilder::new();
            builder
                .push_function_checked(
                    asm,
                    FunctionSig {
                        arg_types: vec![],
                        ret_types: vec![ret_type],
                    },
                )
                .unwrap();
            let result = verify_program_owned(builder.build(), &VerifyConfig::default());
            if ret_type == ValueType::Bool {
                result.unwrap();
            } else {
                assert!(matches!(
                    result,
                    Err(VerifyError::RegisterTypeMismatch {
                        required: ValueType::I64,
                        declared: ValueType::Bool,
                        ..
                    })
                ));
            }
        }
    }

    #[test]
    fn verifier_checks_types_and_initialization_at_branch_joins() {
        for right_type in [Some(ValueType::I64), Some(ValueType::Bool), None] {
            let mut asm = Asm::new();
            asm.set_register_types(vec![
                ValueType::Unit,
                ValueType::Bool,
                ValueType::I64,
                ValueType::I64,
            ]);
            let left = asm.label();
            let right = asm.label();
            let join = asm.label();
            asm.br(1, left, right);
            asm.place(left).unwrap();
            asm.const_i64(2, 1);
            asm.jmp(join);
            asm.place(right).unwrap();
            match right_type {
                Some(ValueType::I64) => {
                    asm.const_i64(2, 2);
                }
                Some(ValueType::Bool) => {
                    asm.const_bool(2, true);
                }
                _ => {}
            }
            asm.jmp(join);
            asm.place(join).unwrap();
            asm.i64_add(3, 2, 2);
            asm.ret(0, &[3]);

            let mut builder = ProgramBuilder::new();
            builder
                .push_function_checked(
                    asm,
                    FunctionSig {
                        arg_types: vec![ValueType::Bool],
                        ret_types: vec![ValueType::I64],
                    },
                )
                .unwrap();
            let result = verify_program_owned(builder.build(), &VerifyConfig::default());
            match right_type {
                Some(ValueType::I64) => {
                    result.unwrap();
                }
                Some(ValueType::Bool) => assert!(matches!(
                    result,
                    Err(VerifyError::RegisterTypeMismatch {
                        reg: 2,
                        required: ValueType::Bool,
                        declared: ValueType::I64,
                        ..
                    })
                )),
                _ => assert!(matches!(
                    result,
                    Err(VerifyError::UninitializedRead { reg: 2, .. })
                )),
            }
        }
    }

    #[test]
    fn verifier_accepts_closure_in_function_signature() {
        let p = Program::new(
            vec![],
            vec![],
            vec![],
            TypeTableDef::default(),
            vec![FunctionDef {
                arg_types: vec![ValueType::Closure],
                ret_types: vec![],
                register_types: vec![ValueType::Unit, ValueType::Closure],
                bytecode: vec![Opcode::Ret as u8, 0x00, 0x00],
                spans: vec![],
            }],
        );
        verify_program(&p, &VerifyConfig::default()).unwrap();
    }

    #[test]
    fn verifier_lowers_closure_new() {
        let bytecode = crate::bytecode::encode_instructions(&[
            Instr::ClosureNew {
                dst: 3,
                func: 1,
                env: 2,
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
                arg_types: vec![ValueType::Func, ValueType::Agg],
                ret_types: vec![ValueType::Closure],
                register_types: vec![
                    ValueType::Unit,
                    ValueType::Func,
                    ValueType::Agg,
                    ValueType::Closure,
                ],
                bytecode,
                spans: vec![],
            }],
        );

        let verified = verify_program_owned(p, &VerifyConfig::default()).unwrap();
        let f0 = verified.verified(FuncId(0)).unwrap();
        match &f0.instrs[0].instr {
            ExecInstr::ClosureNew { dst, func, env } => {
                assert_eq!(dst, &ClosureReg(0));
                assert_eq!(func, &FuncReg(0));
                assert_eq!(env, &AggReg(0));
            }
            other => panic!("expected ClosureNew lowering, got {other:?}"),
        }
    }

    #[test]
    fn verifier_rejects_closure_new_with_non_func_operand() {
        let bytecode = crate::bytecode::encode_instructions(&[
            Instr::ClosureNew {
                dst: 3,
                func: 1,
                env: 2,
            },
            Instr::Ret {
                eff_in: 0,
                rets: vec![],
            },
        ])
        .unwrap();
        let p = Program::new(
            vec![],
            vec![],
            vec![],
            TypeTableDef::default(),
            vec![FunctionDef {
                arg_types: vec![ValueType::I64, ValueType::Agg],
                ret_types: vec![],
                register_types: vec![
                    ValueType::Unit,
                    ValueType::I64,
                    ValueType::Agg,
                    ValueType::Unit,
                ],
                bytecode,
                spans: vec![],
            }],
        );

        let err = verify_program(&p, &VerifyConfig::default()).unwrap_err();
        assert!(matches!(
            err,
            VerifyError::RegisterTypeMismatch {
                func: 0,
                pc: Some(0),
                required: ValueType::Func,
                declared: ValueType::I64,
                ..
            }
        ));
    }

    #[test]
    fn verifier_rejects_closure_new_with_non_agg_env() {
        let bytecode = crate::bytecode::encode_instructions(&[
            Instr::ClosureNew {
                dst: 3,
                func: 1,
                env: 2,
            },
            Instr::Ret {
                eff_in: 0,
                rets: vec![],
            },
        ])
        .unwrap();
        let p = Program::new(
            vec![],
            vec![],
            vec![],
            TypeTableDef::default(),
            vec![FunctionDef {
                arg_types: vec![ValueType::Func, ValueType::I64],
                ret_types: vec![],
                register_types: vec![
                    ValueType::Unit,
                    ValueType::Func,
                    ValueType::I64,
                    ValueType::Unit,
                ],
                bytecode,
                spans: vec![],
            }],
        );

        let err = verify_program(&p, &VerifyConfig::default()).unwrap_err();
        assert!(matches!(
            err,
            VerifyError::RegisterTypeMismatch {
                func: 0,
                pc: Some(0),
                required: ValueType::Agg,
                declared: ValueType::I64,
                ..
            }
        ));
    }

    #[test]
    fn verifier_rejects_closure_new_dst_type_conflict() {
        let bytecode = crate::bytecode::encode_instructions(&[
            Instr::ClosureNew {
                dst: 3,
                func: 1,
                env: 2,
            },
            Instr::Ret {
                eff_in: 0,
                rets: vec![],
            },
        ])
        .unwrap();
        let p = Program::new(
            vec![],
            vec![],
            vec![],
            TypeTableDef::default(),
            vec![FunctionDef {
                arg_types: vec![ValueType::Func, ValueType::Agg, ValueType::I64],
                ret_types: vec![],
                register_types: vec![
                    ValueType::Unit,
                    ValueType::Func,
                    ValueType::Agg,
                    ValueType::I64,
                ],
                bytecode,
                spans: vec![],
            }],
        );

        let err = verify_program(&p, &VerifyConfig::default()).unwrap_err();
        assert!(matches!(
            err,
            VerifyError::RegisterTypeMismatch {
                func: 0,
                pc: Some(0),
                reg: 3,
                declared: ValueType::I64,
                required: ValueType::Closure,
            }
        ));
    }

    #[test]
    fn verifier_lowers_call_indirect_func() {
        let caller_bytecode = crate::bytecode::encode_instructions(&[
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
                    arg_types: vec![ValueType::Func, ValueType::I64],
                    ret_types: vec![ValueType::I64],
                    register_types: vec![
                        ValueType::Unit,
                        ValueType::Func,
                        ValueType::I64,
                        ValueType::I64,
                    ],
                    bytecode: caller_bytecode,
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

        let verified = verify_program_owned(p, &VerifyConfig::default()).unwrap();
        let f0 = verified.verified(FuncId(0)).unwrap();
        match &f0.instrs[0].instr {
            ExecInstr::CallIndirectFunc {
                call_sig, callee, ..
            } => {
                assert_eq!(*call_sig, CallSigId(0));
                assert_eq!(*callee, FuncReg(0));
            }
            other => panic!("expected CallIndirectFunc lowering, got {other:?}"),
        }
    }

    #[test]
    fn verifier_lowers_call_indirect_closure() {
        let bytecode = crate::bytecode::encode_instructions(&[
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
        let mut p = Program::new(
            vec![],
            vec![],
            vec![],
            TypeTableDef::default(),
            vec![FunctionDef {
                arg_types: vec![ValueType::Closure, ValueType::I64],
                ret_types: vec![ValueType::I64],
                register_types: vec![
                    ValueType::Unit,
                    ValueType::Closure,
                    ValueType::I64,
                    ValueType::I64,
                ],
                bytecode,
                spans: vec![],
            }],
        );
        assert_eq!(
            add_call_sig(&mut p, &[ValueType::I64], &[ValueType::I64]),
            0
        );

        let verified = verify_program_owned(p, &VerifyConfig::default()).unwrap();
        let f0 = verified.verified(FuncId(0)).unwrap();
        match &f0.instrs[0].instr {
            ExecInstr::CallIndirectClosure {
                call_sig, callee, ..
            } => {
                assert_eq!(*call_sig, CallSigId(0));
                assert_eq!(*callee, ClosureReg(0));
            }
            other => panic!("expected CallIndirectClosure lowering, got {other:?}"),
        }
    }

    #[test]
    fn verifier_rejects_call_indirect_call_sig_out_of_bounds() {
        let bytecode = crate::bytecode::encode_instructions(&[
            Instr::CallIndirect {
                eff_out: 0,
                call_sig: 7,
                callee: 1,
                eff_in: 0,
                args: vec![],
                rets: vec![],
            },
            Instr::Ret {
                eff_in: 0,
                rets: vec![],
            },
        ])
        .unwrap();
        let p = Program::new(
            vec![],
            vec![],
            vec![],
            TypeTableDef::default(),
            vec![FunctionDef {
                arg_types: vec![ValueType::Func],
                ret_types: vec![],
                register_types: vec![ValueType::Unit, ValueType::Func],
                bytecode,
                spans: vec![],
            }],
        );
        assert_eq!(
            verify_program(&p, &VerifyConfig::default()),
            Err(VerifyError::CallSigOutOfBounds {
                func: 0,
                pc: 0,
                call_sig: 7,
            })
        );
    }

    #[test]
    fn verifier_rejects_call_indirect_arity_mismatch() {
        let bytecode = crate::bytecode::encode_instructions(&[
            Instr::CallIndirect {
                eff_out: 0,
                call_sig: 0,
                callee: 1,
                eff_in: 0,
                args: vec![],
                rets: vec![],
            },
            Instr::Ret {
                eff_in: 0,
                rets: vec![],
            },
        ])
        .unwrap();
        let mut p = Program::new(
            vec![],
            vec![],
            vec![],
            TypeTableDef::default(),
            vec![FunctionDef {
                arg_types: vec![ValueType::Func],
                ret_types: vec![],
                register_types: vec![ValueType::Unit, ValueType::Func],
                bytecode,
                spans: vec![],
            }],
        );
        assert_eq!(add_call_sig(&mut p, &[ValueType::I64], &[]), 0);
        assert_eq!(
            verify_program(&p, &VerifyConfig::default()),
            Err(VerifyError::CallArityMismatch { func: 0, pc: 0 })
        );
    }

    #[test]
    fn verifier_rejects_call_indirect_arg_type_mismatch() {
        let bytecode = crate::bytecode::encode_instructions(&[
            Instr::CallIndirect {
                eff_out: 0,
                call_sig: 0,
                callee: 1,
                eff_in: 0,
                args: vec![2],
                rets: vec![],
            },
            Instr::Ret {
                eff_in: 0,
                rets: vec![],
            },
        ])
        .unwrap();
        let mut p = Program::new(
            vec![],
            vec![],
            vec![],
            TypeTableDef::default(),
            vec![FunctionDef {
                arg_types: vec![ValueType::Func, ValueType::Bool],
                ret_types: vec![],
                register_types: vec![ValueType::Unit, ValueType::Func, ValueType::Bool],
                bytecode,
                spans: vec![],
            }],
        );
        assert_eq!(add_call_sig(&mut p, &[ValueType::I64], &[]), 0);

        let err = verify_program(&p, &VerifyConfig::default()).unwrap_err();
        assert!(matches!(
            err,
            VerifyError::RegisterTypeMismatch {
                func: 0,
                pc: Some(0),
                required: ValueType::I64,
                declared: ValueType::Bool,
                ..
            }
        ));
    }

    #[test]
    fn verifier_rejects_duplicate_function_output_names() {
        let mut pb = ProgramBuilder::new();

        let mut a = Asm::new();
        a.set_register_types(vec![ValueType::Unit, ValueType::I64, ValueType::I64]);
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
        a.set_register_types(vec![ValueType::Unit, ValueType::I64, ValueType::I64]);
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
                register_types: vec![ValueType::Unit],
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
        a.set_register_types(vec![ValueType::Unit, ValueType::Bool, ValueType::I64]);
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
        a.set_register_types(vec![ValueType::Unit, ValueType::Bool, ValueType::I64]);
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
                register_types: vec![ValueType::Unit],
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
                register_types: vec![
                    ValueType::Unit,
                    ValueType::Unit,
                    ValueType::Unit,
                    ValueType::Unit,
                    ValueType::Unit,
                    ValueType::Unit,
                    ValueType::Unit,
                    ValueType::Unit,
                    ValueType::Unit,
                ],
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
        a.set_register_types(vec![ValueType::Unit, ValueType::Unit]);
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
                register_types: vec![ValueType::Unit, ValueType::Unit],
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
                register_types: vec![ValueType::Unit, ValueType::Unit],
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
                register_types: vec![ValueType::Unit, ValueType::Unit],
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
                callee.set_register_types(vec![ValueType::Unit]);
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
                caller.set_register_types(vec![ValueType::Unit, ValueType::Unit]);
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
            register_types: vec![ValueType::Unit, ValueType::Unit],
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
            register_types: vec![ValueType::Unit, ValueType::I64],
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
            register_types: vec![ValueType::Unit, ValueType::Unit],
            bytecode: {
                let mut a = Asm::new();
                a.set_register_types(vec![ValueType::Unit, ValueType::Unit]);
                a.call(0, FuncId(99), 0, &[], &[]);
                a.ret(0, &[]);
                a.finish().unwrap()
            },
            spans: vec![],
        };
        let callee0 = FunctionDef {
            arg_types: vec![],
            ret_types: vec![],
            register_types: vec![ValueType::Unit],
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
        a.set_register_types(vec![
            ValueType::Unit,
            ValueType::Bool,
            ValueType::I64,
            ValueType::I64,
        ]);
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
            VerifyError::RegisterTypeMismatch {
                required: expected,
                declared: actual,
                ..
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
        a.set_register_types(vec![ValueType::Unit, ValueType::I64, ValueType::Agg]);
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
            VerifyError::RegisterTypeMismatch {
                required: expected,
                declared: actual,
                ..
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
        a.set_register_types(vec![ValueType::Unit, ValueType::I64, ValueType::Agg]);
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
        a.set_register_types(vec![
            ValueType::Unit,
            ValueType::I64,
            ValueType::I64,
            ValueType::Agg,
            ValueType::I64,
            ValueType::I64,
        ]);
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
    fn verifier_types_tuple_get_from_argument_contract() {
        let mut pb = ProgramBuilder::new();
        let tuple = tuple_contract(&mut pb, &[Some(ValueType::I64)]);
        let mut a = Asm::new();
        a.set_register_types(vec![
            ValueType::Unit,
            ValueType::Agg,
            ValueType::I64,
            ValueType::I64,
        ]);
        a.tuple_get(2, 1, 0).i64_add(3, 2, 2).ret(0, &[3]);
        pb.push_function_checked(
            a,
            crate::asm::FunctionAbi {
                args: vec![tuple.into()],
                rets: vec![ValueType::I64.into()],
            },
        )
        .unwrap();
        pb.build_checked().unwrap();
    }

    #[test]
    fn direct_calls_check_known_contracts_but_allow_unresolved_obligations() {
        for (actual, expected, accepted) in [
            (
                Some(vec![Some(ValueType::Bool)]),
                vec![Some(ValueType::I64)],
                false,
            ),
            (Some(vec![]), vec![None], false),
            (
                Some(vec![Some(ValueType::I64)]),
                vec![Some(ValueType::I64)],
                true,
            ),
            (Some(vec![Some(ValueType::Bool)]), vec![None], true),
            (Some(vec![None]), vec![Some(ValueType::I64)], true),
            (None, vec![Some(ValueType::I64)], true),
        ] {
            let mut pb = ProgramBuilder::new();
            let required = tuple_contract(&mut pb, &expected);
            let mut callee = Asm::new();
            callee.set_register_types(vec![ValueType::Unit, ValueType::Agg]);
            callee.ret(0, &[]);
            let target = pb
                .push_function_checked(
                    callee,
                    crate::asm::FunctionAbi {
                        args: vec![required.into()],
                        rets: vec![],
                    },
                )
                .unwrap();
            let actual = actual.as_ref().map(|kinds| tuple_contract(&mut pb, kinds));
            let mut caller = Asm::new();
            caller.set_register_types(vec![ValueType::Unit, ValueType::Agg]);
            caller.call(0, target, 0, &[1], &[]).ret(0, &[]);
            let arg = actual.map_or(
                crate::asm::ContractRef::Kind(ValueType::Agg),
                crate::asm::ContractRef::Id,
            );
            pb.push_function_checked(
                caller,
                crate::asm::FunctionAbi {
                    args: vec![arg],
                    rets: vec![],
                },
            )
            .unwrap();
            let result = verify_program_owned(pb.build(), &VerifyConfig::default());
            if accepted {
                result.unwrap();
            } else {
                assert!(
                    matches!(
                        result,
                        Err(VerifyError::ContractMismatch {
                            func: 1,
                            position: ContractPosition::Argument { index: 0, .. },
                            ..
                        })
                    ),
                    "{result:?}"
                );
            }
        }
    }

    #[test]
    fn verifier_rejects_contract_kind_disagreeing_with_argument_ledger() {
        let mut pb = ProgramBuilder::new();
        let tuple = tuple_contract(&mut pb, &[]);
        let mut a = Asm::new();
        a.set_register_types(vec![ValueType::Unit, ValueType::I64]);
        a.ret(0, &[]);
        pb.push_function_checked(
            a,
            crate::asm::FunctionAbi {
                args: vec![tuple.into()],
                rets: vec![],
            },
        )
        .unwrap();
        assert!(matches!(
            pb.build_verified(),
            Err(BuildError::Verify(VerifyError::RegisterTypeMismatch {
                pc: None,
                declared: ValueType::I64,
                required: ValueType::Agg,
                ..
            }))
        ));
    }

    #[test]
    fn verifier_rejects_tuple_get_oob_index() {
        let mut a = Asm::new();
        a.set_register_types(vec![
            ValueType::Unit,
            ValueType::I64,
            ValueType::Agg,
            ValueType::I64,
        ]);
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
        a.set_register_types(vec![
            ValueType::Unit,
            ValueType::Bool,
            ValueType::I64,
            ValueType::Agg,
            ValueType::Bool,
            ValueType::I64,
        ]);
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
        a.set_register_types(vec![
            ValueType::Unit,
            ValueType::I64,
            ValueType::Agg,
            ValueType::I64,
        ]);
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
        a.set_register_types(vec![
            ValueType::Unit,
            ValueType::I64,
            ValueType::Agg,
            ValueType::I64,
        ]);
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
    fn optional_lints_preserve_type_and_initialization_errors() {
        for initialized in [false, true] {
            let mut asm = Asm::new();
            asm.set_register_types(vec![
                ValueType::Unit,
                if initialized {
                    ValueType::Bool
                } else {
                    ValueType::I64
                },
                ValueType::I64,
                ValueType::I64,
            ]);
            if initialized {
                asm.const_bool(1, true);
            }
            asm.const_i64(2, 7);
            asm.i64_add(3, 1, 2);
            asm.ret(0, &[3]);
            let mut builder = ProgramBuilder::new();
            builder
                .push_function_checked(
                    asm,
                    FunctionSig {
                        arg_types: vec![],
                        ret_types: vec![ValueType::I64],
                    },
                )
                .unwrap();
            let program = builder.build();
            let cfg = VerifyConfig::default();
            let expected = verify_program(&program, &cfg).unwrap_err();
            if initialized {
                assert!(matches!(expected, VerifyError::RegisterTypeMismatch { .. }));
            } else {
                assert!(matches!(expected, VerifyError::UninitializedRead { .. }));
            }
            assert_eq!(
                verify_program_with_lints(&program, &cfg).unwrap_err(),
                expected
            );
            assert_eq!(
                verify_program_owned(program.clone(), &cfg).unwrap_err(),
                expected
            );
            assert_eq!(
                verify_program_owned_with_lints(program, &cfg).unwrap_err(),
                expected
            );
        }
    }

    #[test]
    fn verifier_lints_unreachable_dead_store_redundant_mov_and_unused_call_ret() {
        let mut pb = ProgramBuilder::new();

        // f0: returns i64
        pb.push_function_checked(
            {
                let mut a = Asm::new();
                a.set_register_types(vec![ValueType::Unit, ValueType::I64]);
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
                a.set_register_types(vec![
                    ValueType::Unit,
                    ValueType::I64,
                    ValueType::I64,
                    ValueType::I64,
                    ValueType::I64,
                ]);
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
                a.ret(0, &[]);

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
        let cfg = VerifyConfig::default();
        verify_program(&p, &cfg).unwrap();
        let lints = verify_program_with_lints(&p, &cfg).unwrap();
        let verified = verify_program_owned(p.clone(), &cfg).unwrap();
        let (verified_with_lints, owned_lints) = verify_program_owned_with_lints(p, &cfg).unwrap();
        assert_eq!(verified, verified_with_lints);
        assert_eq!(lints, owned_lints);

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
