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

use crate::bytecode::{DecodedInstr, Instr, decode_instructions};
use crate::format::DecodeError;
use crate::host::sig_hash_slices;
use crate::program::{ConstEntry, ElemTypeId, Function, Program, SpanEntry, TypeId, ValueType};
use crate::value::FuncId;

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
    decoded_functions: Vec<Vec<DecodedInstr>>,
}

impl VerifiedProgram {
    /// Returns the underlying program.
    #[must_use]
    pub fn program(&self) -> &Program {
        &self.program
    }

    /// Returns the decoded instruction stream for `func`, if present.
    ///
    /// The verifier guarantees that this exists for all functions in the program.
    #[must_use]
    pub(crate) fn decoded(&self, func: FuncId) -> Option<&[DecodedInstr]> {
        self.decoded_functions
            .get(func.0 as usize)
            .map(Vec::as_slice)
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
    /// A bytecode jump target is invalid (out of bounds or not an instruction boundary).
    InvalidJumpTarget {
        /// Function index within the program.
        func: u32,
        /// Byte offset target.
        pc: u32,
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
    /// The program's host signature hash does not match the canonical hash for its types.
    HostSigHashMismatch {
        /// Host signature id.
        host_sig: u32,
    },
    /// A typed use requires a concrete type, but the value is [`ValueType::Any`].
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
    /// Bytecode decoding failed.
    BytecodeDecode {
        /// Function index within the program.
        func: u32,
    },
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decode(e) => write!(f, "decode failed: {e}"),
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
            Self::BadSpanDeltas { func } => {
                write!(f, "function {func} span table has bad pc_deltas")
            }
            Self::RegCountTooLarge { func, reg_count } => {
                write!(f, "function {func} reg_count {reg_count} is too large")
            }
            Self::ArgCountExceedsRegs { func } => {
                write!(f, "function {func} arg_count exceeds reg_count")
            }
            Self::InvalidJumpTarget { func, pc } => {
                write!(f, "function {func} invalid jump target pc={pc}")
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
            Self::TypeMismatch {
                func,
                pc,
                expected,
                actual,
            } => write!(
                f,
                "function {func} pc={pc} type mismatch (expected {expected:?}, got {actual:?})"
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

    for (i, func) in program.functions.iter().enumerate() {
        let func_id = u32::try_from(i).unwrap_or(u32::MAX);
        let _decoded = verify_function_container(program, func_id, func, cfg)?;
    }
    Ok(())
}

/// Verifies `program` and returns a [`VerifiedProgram`] wrapper on success.
pub fn verify_program_owned(
    program: Program,
    cfg: &VerifyConfig,
) -> Result<VerifiedProgram, VerifyError> {
    verify_host_sigs(&program)?;

    let mut decoded_functions: Vec<Vec<DecodedInstr>> = Vec::with_capacity(program.functions.len());
    for (i, func) in program.functions.iter().enumerate() {
        let func_id = u32::try_from(i).unwrap_or(u32::MAX);
        decoded_functions.push(verify_function_container(&program, func_id, func, cfg)?);
    }

    Ok(VerifiedProgram {
        program,
        decoded_functions,
    })
}

fn verify_host_sigs(program: &Program) -> Result<(), VerifyError> {
    for (i, hs) in program.host_sigs.iter().enumerate() {
        let host_sig = u32::try_from(i).unwrap_or(u32::MAX);
        if (hs.symbol.0 as usize) >= program.symbols.len() {
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
) -> Result<Vec<DecodedInstr>, VerifyError> {
    if func.reg_count > cfg.max_regs_per_function {
        return Err(VerifyError::RegCountTooLarge {
            func: func_id,
            reg_count: func.reg_count,
        });
    }

    let bytecode = program
        .function_bytecode(func)
        .map_err(|_| VerifyError::FunctionBytecodeOutOfBounds { func: func_id })?;
    let spans = program
        .function_spans(func)
        .map_err(|_| VerifyError::FunctionSpansOutOfBounds { func: func_id })?;
    let arg_types = program
        .function_arg_types(func)
        .map_err(|_| VerifyError::FunctionArgTypesOutOfBounds { func: func_id })?;
    let ret_types = program
        .function_ret_types(func)
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
    verify_function_bytecode(
        program, func_id, func, bytecode, arg_types, ret_types, &decoded,
    )?;

    Ok(decoded)
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

fn verify_function_bytecode(
    program: &Program,
    func_id: u32,
    func: &Function,
    bytecode: &[u8],
    arg_types: &[ValueType],
    ret_types: &[ValueType],
    decoded: &[DecodedInstr],
) -> Result<(), VerifyError> {
    if func.reg_count == 0 {
        return Err(VerifyError::ArgCountExceedsRegs { func: func_id });
    }
    // Convention: reg 0 is the effect token; value args occupy regs 1..=arg_count.
    if u64::from(func.arg_count) + 1 > u64::from(func.reg_count) {
        return Err(VerifyError::ArgCountExceedsRegs { func: func_id });
    }

    let boundaries = compute_boundaries(bytecode.len(), decoded);

    // Build CFG blocks and reachability.
    let byte_len =
        u32::try_from(bytecode.len()).map_err(|_| VerifyError::BytecodeDecode { func: func_id })?;
    let blocks = build_basic_blocks(byte_len, decoded, &boundaries)
        .map_err(|pc| VerifyError::InvalidJumpTarget { func: func_id, pc })?;
    let reachable = compute_reachable(&blocks);

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

    Ok(())
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

    match instr {
        Instr::Nop => {}
        Instr::Mov { dst, src } => {
            require_init(*src, state)?;
            write_reg(*dst, state)?;
        }
        Instr::Trap { .. } => {}

        Instr::ConstUnit { dst }
        | Instr::ConstBool { dst, .. }
        | Instr::ConstI64 { dst, .. }
        | Instr::ConstU64 { dst, .. }
        | Instr::ConstF64 { dst, .. }
        | Instr::ConstDecimal { dst, .. }
        | Instr::ConstPool { dst, .. } => {
            write_reg(*dst, state)?;
        }

        Instr::DecAdd { dst, a, b } | Instr::DecSub { dst, a, b } | Instr::DecMul { dst, a, b } => {
            require_init(*a, state)?;
            require_init(*b, state)?;
            write_reg(*dst, state)?;
        }

        Instr::F64Add { dst, a, b }
        | Instr::F64Sub { dst, a, b }
        | Instr::F64Mul { dst, a, b }
        | Instr::F64Div { dst, a, b } => {
            require_init(*a, state)?;
            require_init(*b, state)?;
            write_reg(*dst, state)?;
        }

        Instr::I64Add { dst, a, b }
        | Instr::I64Sub { dst, a, b }
        | Instr::I64Mul { dst, a, b }
        | Instr::I64Div { dst, a, b }
        | Instr::I64Rem { dst, a, b }
        | Instr::U64Add { dst, a, b }
        | Instr::U64Sub { dst, a, b }
        | Instr::U64Mul { dst, a, b }
        | Instr::U64Div { dst, a, b }
        | Instr::U64Rem { dst, a, b }
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
        | Instr::I64Eq { dst, a, b }
        | Instr::I64Lt { dst, a, b }
        | Instr::U64Eq { dst, a, b }
        | Instr::U64Lt { dst, a, b }
        | Instr::F64Eq { dst, a, b }
        | Instr::F64Lt { dst, a, b }
        | Instr::F64Gt { dst, a, b }
        | Instr::F64Le { dst, a, b }
        | Instr::F64Ge { dst, a, b }
        | Instr::BytesEq { dst, a, b }
        | Instr::StrEq { dst, a, b }
        | Instr::BytesConcat { dst, a, b }
        | Instr::StrConcat { dst, a, b } => {
            require_init(*a, state)?;
            require_init(*b, state)?;
            write_reg(*dst, state)?;
        }
        Instr::U64Gt { dst, a, b }
        | Instr::U64Le { dst, a, b }
        | Instr::U64Ge { dst, a, b }
        | Instr::I64Gt { dst, a, b }
        | Instr::I64Le { dst, a, b }
        | Instr::I64Ge { dst, a, b } => {
            require_init(*a, state)?;
            require_init(*b, state)?;
            write_reg(*dst, state)?;
        }
        Instr::BoolNot { dst, a }
        | Instr::U64ToI64 { dst, a }
        | Instr::I64ToU64 { dst, a }
        | Instr::I64ToF64 { dst, a }
        | Instr::U64ToF64 { dst, a }
        | Instr::F64ToI64 { dst, a }
        | Instr::F64ToU64 { dst, a }
        | Instr::DecToI64 { dst, a }
        | Instr::DecToU64 { dst, a }
        | Instr::StrToBytes { dst, s: a }
        | Instr::BytesToStr { dst, bytes: a } => {
            require_init(*a, state)?;
            write_reg(*dst, state)?;
        }
        Instr::I64ToDec { dst, a, .. } | Instr::U64ToDec { dst, a, .. } => {
            require_init(*a, state)?;
            write_reg(*dst, state)?;
        }
        Instr::BytesGet { dst, bytes, index } => {
            require_init(*bytes, state)?;
            require_init(*index, state)?;
            write_reg(*dst, state)?;
        }
        Instr::BytesGetImm { dst, bytes, .. } => {
            require_init(*bytes, state)?;
            write_reg(*dst, state)?;
        }
        Instr::BytesSlice {
            dst,
            bytes,
            start,
            end,
        } => {
            require_init(*bytes, state)?;
            require_init(*start, state)?;
            require_init(*end, state)?;
            write_reg(*dst, state)?;
        }
        Instr::StrSlice { dst, s, start, end } => {
            require_init(*s, state)?;
            require_init(*start, state)?;
            require_init(*end, state)?;
            write_reg(*dst, state)?;
        }
        Instr::Select { dst, cond, a, b } => {
            require_init(*cond, state)?;
            require_init(*a, state)?;
            require_init(*b, state)?;
            write_reg(*dst, state)?;
        }

        Instr::Br {
            cond,
            pc_true: _,
            pc_false: _,
        } => {
            require_init(*cond, state)?;
        }
        Instr::Jmp { .. } => {}

        Instr::Call {
            eff_out,
            func_id: callee,
            eff_in,
            args,
            rets,
        } => {
            require_init(*eff_in, state)?;
            for &a in args {
                require_init(a, state)?;
            }
            // Signature check (counts only for now).
            let callee = program
                .functions
                .get(callee.0 as usize)
                .ok_or(VerifyError::CallArityMismatch { func: func_id, pc })?;
            if u32::try_from(args.len()).ok() != Some(callee.arg_count)
                || u32::try_from(rets.len()).ok() != Some(callee.ret_count)
            {
                return Err(VerifyError::CallArityMismatch { func: func_id, pc });
            }
            write_reg(*eff_out, state)?;
            for &r in rets {
                write_reg(r, state)?;
            }
        }

        Instr::Ret { eff_in, rets } => {
            require_init(*eff_in, state)?;
            for &r in rets {
                require_init(r, state)?;
            }
        }

        Instr::HostCall {
            eff_out,
            host_sig: _,
            eff_in,
            args,
            rets,
        } => {
            require_init(*eff_in, state)?;
            for &a in args {
                require_init(a, state)?;
            }
            write_reg(*eff_out, state)?;
            for &r in rets {
                write_reg(r, state)?;
            }
        }

        Instr::TupleNew { dst, values } => {
            for &v in values {
                require_init(v, state)?;
            }
            write_reg(*dst, state)?;
        }
        Instr::TupleGet { dst, tuple, .. } => {
            require_init(*tuple, state)?;
            write_reg(*dst, state)?;
        }
        Instr::TupleLen { dst, tuple } => {
            require_init(*tuple, state)?;
            write_reg(*dst, state)?;
        }
        Instr::StructNew { dst, values, .. } => {
            for &v in values {
                require_init(v, state)?;
            }
            write_reg(*dst, state)?;
        }
        Instr::StructGet { dst, st, .. } => {
            require_init(*st, state)?;
            write_reg(*dst, state)?;
        }
        Instr::StructFieldCount { dst, st } => {
            require_init(*st, state)?;
            write_reg(*dst, state)?;
        }
        Instr::ArrayNew { dst, values, .. } => {
            for &v in values {
                require_init(v, state)?;
            }
            write_reg(*dst, state)?;
        }
        Instr::ArrayLen { dst, arr } => {
            require_init(*arr, state)?;
            write_reg(*dst, state)?;
        }
        Instr::ArrayGet { dst, arr, index } => {
            require_init(*arr, state)?;
            require_init(*index, state)?;
            write_reg(*dst, state)?;
        }
        Instr::ArrayGetImm { dst, arr, .. } => {
            require_init(*arr, state)?;
            write_reg(*dst, state)?;
        }
        Instr::BytesLen { dst, bytes } => {
            require_init(*bytes, state)?;
            write_reg(*dst, state)?;
        }
        Instr::StrLen { dst, s } => {
            require_init(*s, state)?;
            write_reg(*dst, state)?;
        }
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
    Tuple(Vec<ValueType>),
    Struct(TypeId),
    Array(ElemTypeId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TypeState {
    // `None` means "unknown/top" during fixpoint iteration (used to compute a greatest fixpoint for
    // loop-invariant types without assuming a particular type up front).
    //
    // `Some(None)` means "definitely uninitialized".
    //
    // `Some(Some(t))` means "definitely initialized with type `t`", where `t == Any` is used when
    // all incoming paths initialize the value but disagree on its concrete type.
    values: Vec<Option<Option<ValueType>>>,
    aggs: Vec<Option<AggMeta>>,
}

fn initial_types(reg_count: usize, arg_types: &[ValueType]) -> TypeState {
    let mut values: Vec<Option<Option<ValueType>>> = vec![Some(None); reg_count];
    let aggs: Vec<Option<AggMeta>> = vec![None; reg_count];
    if reg_count != 0 {
        values[0] = Some(Some(ValueType::Unit)); // effect token
        for (i, &t) in arg_types.iter().enumerate() {
            if 1 + i < reg_count {
                values[1 + i] = Some(Some(t));
            }
        }
    }
    TypeState { values, aggs }
}

fn meet_value(
    a: Option<Option<ValueType>>,
    b: Option<Option<ValueType>>,
) -> Option<Option<ValueType>> {
    match (a, b) {
        // Unknown/top (used for initialization) doesn't constrain the result.
        (None, x) | (x, None) => x,
        // Any definitely-uninitialized incoming path makes the value definitely uninitialized.
        (Some(None), _) | (_, Some(None)) => Some(None),
        (Some(Some(x)), Some(Some(y))) => {
            if x == y {
                Some(Some(x))
            } else {
                Some(Some(ValueType::Any))
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

    fn set_value(state: &mut TypeState, reg: u32, ty: ValueType) {
        if let Some(slot) = state.values.get_mut(reg as usize) {
            *slot = Some(Some(ty));
        }
        if ty != ValueType::Agg {
            clear_agg(state, reg);
        }
    }

    fn set_agg(state: &mut TypeState, reg: u32, meta: Option<AggMeta>) {
        set_value(state, reg, ValueType::Agg);
        if let Some(slot) = state.aggs.get_mut(reg as usize) {
            *slot = meta;
        }
    }

    fn copy_reg(state: &mut TypeState, dst: u32, src: u32) {
        let t = state
            .values
            .get(src as usize)
            .copied()
            .flatten()
            .flatten()
            .unwrap_or(ValueType::Any);
        set_value(state, dst, t);
        let meta = state.aggs.get(src as usize).cloned().unwrap_or(None);
        if let Some(slot) = state.aggs.get_mut(dst as usize) {
            *slot = meta;
        }
        if t != ValueType::Agg {
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
        Instr::ConstPool { dst, idx } => {
            let t = program
                .const_pool
                .get(idx.0 as usize)
                .map(const_value_type)
                .unwrap_or(ValueType::Any);
            set_value(state, *dst, t);
        }
        Instr::DecAdd { dst, .. } | Instr::DecSub { dst, .. } | Instr::DecMul { dst, .. } => {
            set_value(state, *dst, ValueType::Decimal);
        }
        Instr::F64Add { dst, .. }
        | Instr::F64Sub { dst, .. }
        | Instr::F64Mul { dst, .. }
        | Instr::F64Div { dst, .. } => {
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
        Instr::I64ToU64 { dst, .. } => set_value(state, *dst, ValueType::U64),
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
        | Instr::BoolNot { dst, .. } => set_value(state, *dst, ValueType::Bool),
        Instr::BytesEq { dst, .. } | Instr::StrEq { dst, .. } => {
            set_value(state, *dst, ValueType::Bool);
        }
        Instr::Select { dst, a, .. } => {
            let t = state
                .values
                .get(*a as usize)
                .copied()
                .flatten()
                .flatten()
                .unwrap_or(ValueType::Any);
            set_value(state, *dst, t);
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
                && let Ok(types) = program.function_ret_types(callee)
            {
                for (dst, t) in rets.iter().zip(types.iter().copied()) {
                    set_value(state, *dst, t);
                }
                return;
            }
            for dst in rets {
                set_value(state, *dst, ValueType::Any);
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
                    set_value(state, *dst, ValueType::Any);
                }
                return;
            };
            if let Ok(types) = program.host_sig_rets(hs) {
                for (dst, t) in rets.iter().zip(types.iter().copied()) {
                    set_value(state, *dst, t);
                }
            } else {
                for dst in rets {
                    set_value(state, *dst, ValueType::Any);
                }
            }
        }
        Instr::TupleNew { dst, values } => {
            let mut elems: Vec<ValueType> = Vec::with_capacity(values.len());
            for &r in values {
                elems.push(
                    state
                        .values
                        .get(r as usize)
                        .copied()
                        .flatten()
                        .flatten()
                        .unwrap_or(ValueType::Any),
                );
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
                Some(AggMeta::Tuple(elems)) => elems
                    .get(*index as usize)
                    .copied()
                    .unwrap_or(ValueType::Any),
                _ => ValueType::Any,
            };
            set_value(state, *dst, out);
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
                    .unwrap_or(ValueType::Any),
                _ => ValueType::Any,
            };
            set_value(state, *dst, out);
        }
        Instr::ArrayGet { dst, arr, .. } => {
            let out = match state.aggs.get(*arr as usize).and_then(|m| m.as_ref()) {
                Some(AggMeta::Array(elem_type_id)) => program
                    .types
                    .array_elems
                    .get(elem_type_id.0 as usize)
                    .copied()
                    .unwrap_or(ValueType::Any),
                _ => ValueType::Any,
            };
            set_value(state, *dst, out);
        }
        Instr::ArrayGetImm { dst, arr, .. } => {
            let out = match state.aggs.get(*arr as usize).and_then(|m| m.as_ref()) {
                Some(AggMeta::Array(elem_type_id)) => program
                    .types
                    .array_elems
                    .get(elem_type_id.0 as usize)
                    .copied()
                    .unwrap_or(ValueType::Any),
                _ => ValueType::Any,
            };
            set_value(state, *dst, out);
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
    // Precompute preds.
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); blocks.len()];
    for (i, b) in blocks.iter().enumerate() {
        for succ in b.succs {
            if let Some(s) = succ.filter(|&s| s != usize::MAX) {
                preds[s].push(i);
            }
        }
    }

    let empty = TypeState {
        values: vec![None; reg_count],
        aggs: vec![None; reg_count],
    };
    let mut in_sets: Vec<TypeState> = vec![empty.clone(); blocks.len()];
    let mut out_sets: Vec<TypeState> = vec![empty; blocks.len()];
    if !blocks.is_empty() {
        in_sets[0] = entry_types.clone();
    }

    let mut changed = true;
    while changed {
        changed = false;
        for i in 0..blocks.len() {
            if !reachable[i] {
                continue;
            }
            if i != 0 {
                let mut it = preds[i].iter().copied().filter(|&p| reachable[p]);
                let Some(first) = it.next() else {
                    continue;
                };
                let mut new_in = out_sets[first].clone();
                for p in it {
                    for r in 0..reg_count {
                        new_in.values[r] = meet_value(new_in.values[r], out_sets[p].values[r]);
                        if matches!(new_in.values[r], Some(Some(ValueType::Agg))) {
                            new_in.aggs[r] = meet_agg(&new_in.aggs[r], &out_sets[p].aggs[r]);
                        } else {
                            new_in.aggs[r] = None;
                        }
                    }
                }
                if new_in != in_sets[i] {
                    in_sets[i] = new_in;
                    changed = true;
                }
            }

            let mut out = in_sets[i].clone();
            for di in decoded
                .iter()
                .take(blocks[i].instr_end)
                .skip(blocks[i].instr_start)
            {
                transfer_types(program, &di.instr, &mut out);
            }
            if out != out_sets[i] {
                out_sets[i] = out;
                changed = true;
            }
        }
    }

    Ok((in_sets, out_sets))
}

fn check_expected(
    func: u32,
    pc: u32,
    reg: u32,
    actual: Option<ValueType>,
    expected: ValueType,
) -> Result<(), VerifyError> {
    if expected == ValueType::Any {
        return Ok(());
    }
    let Some(actual) = actual else {
        return Err(VerifyError::UninitializedRead { func, pc, reg });
    };
    if actual == ValueType::Any {
        return Err(VerifyError::UnknownTypeAtUse {
            func,
            pc,
            reg,
            expected,
        });
    }
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

fn validate_instr_types(
    program: &Program,
    func_id: u32,
    pc: u32,
    instr: &Instr,
    state: &TypeState,
    func_ret_types: &[ValueType],
) -> Result<(), VerifyError> {
    let t = |reg: u32| state.values.get(reg as usize).copied().flatten().flatten();
    let a = |reg: u32| state.aggs.get(reg as usize).and_then(|m| m.as_ref());

    match instr {
        Instr::Nop | Instr::Trap { .. } | Instr::Jmp { .. } => {}
        Instr::Mov { .. } => {}
        Instr::ConstUnit { .. }
        | Instr::ConstBool { .. }
        | Instr::ConstI64 { .. }
        | Instr::ConstU64 { .. }
        | Instr::ConstF64 { .. }
        | Instr::ConstDecimal { .. } => {}
        Instr::ConstPool { idx, .. } => {
            if (idx.0 as usize) >= program.const_pool.len() {
                return Err(VerifyError::ConstOutOfBounds {
                    func: func_id,
                    pc,
                    const_id: idx.0,
                });
            }
        }
        Instr::DecAdd { a, b, .. } | Instr::DecSub { a, b, .. } | Instr::DecMul { a, b, .. } => {
            check_expected(func_id, pc, *a, t(*a), ValueType::Decimal)?;
            check_expected(func_id, pc, *b, t(*b), ValueType::Decimal)?;
        }
        Instr::F64Add { a, b, .. }
        | Instr::F64Sub { a, b, .. }
        | Instr::F64Mul { a, b, .. }
        | Instr::F64Div { a, b, .. } => {
            check_expected(func_id, pc, *a, t(*a), ValueType::F64)?;
            check_expected(func_id, pc, *b, t(*b), ValueType::F64)?;
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
            let ta = t(*a).ok_or(VerifyError::UninitializedRead {
                func: func_id,
                pc,
                reg: *a,
            })?;
            let tb = t(*b).ok_or(VerifyError::UninitializedRead {
                func: func_id,
                pc,
                reg: *b,
            })?;
            if ta == ValueType::Any || tb == ValueType::Any {
                return Err(VerifyError::UnknownTypeAtUse {
                    func: func_id,
                    pc,
                    reg: if ta == ValueType::Any { *a } else { *b },
                    expected: ValueType::Any,
                });
            }
            if ta != tb {
                return Err(VerifyError::TypeMismatch {
                    func: func_id,
                    pc,
                    expected: ta,
                    actual: tb,
                });
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
            let callee_args = program
                .function_arg_types(callee_fn)
                .map_err(|_| VerifyError::FunctionArgTypesOutOfBounds { func: callee.0 })?;
            let callee_rets = program
                .function_ret_types(callee_fn)
                .map_err(|_| VerifyError::FunctionRetTypesOutOfBounds { func: callee.0 })?;
            if args.len() != callee_args.len() || rets.len() != callee_rets.len() {
                return Err(VerifyError::CallArityMismatch { func: func_id, pc });
            }
            for (&r, &expected) in args.iter().zip(callee_args.iter()) {
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
                .ok_or(VerifyError::HostSigOutOfBounds {
                    func: func_id,
                    pc,
                    host_sig: host_sig.0,
                })?;
            let hs_args = program
                .host_sig_args(hs)
                .map_err(|_| VerifyError::HostSigMalformed {
                    host_sig: host_sig.0,
                })?;
            let hs_rets = program
                .host_sig_rets(hs)
                .map_err(|_| VerifyError::HostSigMalformed {
                    host_sig: host_sig.0,
                })?;
            if args.len() != hs_args.len() || rets.len() != hs_rets.len() {
                return Err(VerifyError::HostCallArityMismatch { func: func_id, pc });
            }
            for (&r, &expected) in args.iter().zip(hs_args.iter()) {
                check_expected(func_id, pc, r, t(r), expected)?;
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
            let elem = program
                .types
                .array_elems
                .get(elem_type_id.0 as usize)
                .copied()
                .ok_or(VerifyError::ArrayElemTypeOutOfBounds {
                    func: func_id,
                    pc,
                    elem_type_id: elem_type_id.0,
                })?;
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct BasicBlock {
    start_pc: u32,
    end_pc: u32,
    instr_start: usize,
    instr_end: usize,
    succs: [Option<usize>; 2],
}

fn compute_boundaries(byte_len: usize, decoded: &[DecodedInstr]) -> Vec<bool> {
    let mut b = vec![false; byte_len + 1];
    for di in decoded {
        let o = di.offset as usize;
        if o <= byte_len {
            b[o] = true;
        }
    }
    b[byte_len] = true;
    b
}

fn build_basic_blocks(
    byte_len: u32,
    decoded: &[DecodedInstr],
    boundaries: &[bool],
) -> Result<Vec<BasicBlock>, u32> {
    // Leaders are: entry, jump targets, and the next instruction after a terminator.
    let mut leader = vec![false; (byte_len as usize) + 1];
    leader[0] = true;

    for (i, di) in decoded.iter().enumerate() {
        let end = if i + 1 < decoded.len() {
            decoded[i + 1].offset as usize
        } else {
            byte_len as usize
        };

        match &di.instr {
            Instr::Br {
                pc_true, pc_false, ..
            } => {
                if *pc_true > byte_len {
                    return Err(*pc_true);
                }
                if *pc_false > byte_len {
                    return Err(*pc_false);
                }
                leader[*pc_true as usize] = true;
                leader[*pc_false as usize] = true;
                if end <= byte_len as usize {
                    leader[end] = true;
                }
            }
            Instr::Jmp { pc_target } => {
                if *pc_target > byte_len {
                    return Err(*pc_target);
                }
                leader[*pc_target as usize] = true;
                if end <= byte_len as usize {
                    leader[end] = true;
                }
            }
            Instr::Ret { .. } | Instr::Trap { .. } => {
                if end <= byte_len as usize {
                    leader[end] = true;
                }
            }
            _ => {}
        }
    }

    // Validate that all leaders (targets) are instruction boundaries (or end).
    for (pc, &is_leader) in leader.iter().enumerate() {
        if is_leader && pc <= byte_len as usize && !boundaries[pc] {
            return Err(u32::try_from(pc).unwrap_or(byte_len));
        }
    }

    // Map from pc to instruction index.
    let mut pc_to_instr = vec![usize::MAX; (byte_len as usize) + 1];
    for (idx, di) in decoded.iter().enumerate() {
        pc_to_instr[di.offset as usize] = idx;
    }

    // Collect leader pcs in order.
    let mut leader_pcs: Vec<u32> = leader
        .iter()
        .enumerate()
        .filter_map(|(pc, &v)| v.then_some(u32::try_from(pc).unwrap_or(byte_len)))
        .collect();
    leader_pcs.sort_unstable();
    leader_pcs.dedup();

    let mut blocks: Vec<BasicBlock> = Vec::new();
    for (i, &start_pc) in leader_pcs.iter().enumerate() {
        if start_pc == byte_len {
            continue;
        }
        let end_pc = leader_pcs.get(i + 1).copied().unwrap_or(byte_len);
        let instr_start = pc_to_instr[start_pc as usize];
        let instr_end = if end_pc == byte_len {
            decoded.len()
        } else {
            pc_to_instr[end_pc as usize]
        };
        blocks.push(BasicBlock {
            start_pc,
            end_pc,
            instr_start,
            instr_end,
            succs: [None, None],
        });
    }

    // Map pc -> block index.
    let mut pc_to_block = vec![usize::MAX; (byte_len as usize) + 1];
    for (i, b) in blocks.iter().enumerate() {
        pc_to_block[b.start_pc as usize] = i;
    }

    // Fill successors.
    for i in 0..blocks.len() {
        let last = blocks[i].instr_end.saturating_sub(1);
        let Some(di) = decoded.get(last) else {
            continue;
        };
        let fallthrough = if blocks[i].end_pc < byte_len {
            Some(pc_to_block[blocks[i].end_pc as usize])
        } else {
            None
        };
        blocks[i].succs = match &di.instr {
            Instr::Br {
                pc_true, pc_false, ..
            } => [
                Some(pc_to_block[*pc_true as usize]),
                Some(pc_to_block[*pc_false as usize]),
            ],
            Instr::Jmp { pc_target } => [Some(pc_to_block[*pc_target as usize]), None],
            Instr::Ret { .. } | Instr::Trap { .. } => [None, None],
            _ => [fallthrough, None],
        };
    }

    Ok(blocks)
}

fn compute_reachable(blocks: &[BasicBlock]) -> Vec<bool> {
    let mut reachable = vec![false; blocks.len()];
    if blocks.is_empty() {
        return reachable;
    }
    let mut stack = vec![0_usize];
    reachable[0] = true;
    while let Some(b) = stack.pop() {
        for &succ in &blocks[b].succs {
            let Some(s) = succ else { continue };
            if s != usize::MAX && !reachable[s] {
                reachable[s] = true;
                stack.push(s);
            }
        }
    }
    reachable
}

fn compute_must_init(
    blocks: &[BasicBlock],
    reachable: &[bool],
    reg_count: usize,
    entry_init: &BitSet,
    decoded: &[DecodedInstr],
) -> Result<(Vec<BitSet>, Vec<BitSet>), VerifyError> {
    // Precompute preds.
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); blocks.len()];
    for (i, b) in blocks.iter().enumerate() {
        for succ in b.succs {
            if let Some(s) = succ.filter(|&s| s != usize::MAX) {
                preds[s].push(i);
            }
        }
    }

    let top = BitSet::new_full(reg_count);
    let mut in_sets = vec![top.clone(); blocks.len()];
    let mut out_sets = vec![top.clone(); blocks.len()];

    if !blocks.is_empty() {
        in_sets[0] = entry_init.clone();
    }

    let writes = compute_block_writes(blocks, reachable, reg_count, decoded)?;

    let mut changed = true;
    while changed {
        changed = false;
        for i in 0..blocks.len() {
            if !reachable[i] {
                continue;
            }
            if i == 0 {
                // Entry block IN is fixed.
            } else {
                let mut new_in = top.clone();
                for &p in &preds[i] {
                    if !reachable[p] {
                        continue;
                    }
                    new_in.intersect_with(&out_sets[p]);
                }
                if new_in != in_sets[i] {
                    in_sets[i] = new_in;
                    changed = true;
                }
            }

            // OUT = IN + writes in block (writes-only transfer).
            let mut out = in_sets[i].clone();
            out.union_with(&writes[i]);
            if out != out_sets[i] {
                out_sets[i] = out;
                changed = true;
            }
        }
    }

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
            match &di.instr {
                Instr::Mov { dst, .. }
                | Instr::ConstUnit { dst }
                | Instr::ConstBool { dst, .. }
                | Instr::ConstI64 { dst, .. }
                | Instr::ConstU64 { dst, .. }
                | Instr::ConstF64 { dst, .. }
                | Instr::ConstDecimal { dst, .. }
                | Instr::ConstPool { dst, .. }
                | Instr::DecAdd { dst, .. }
                | Instr::DecSub { dst, .. }
                | Instr::DecMul { dst, .. }
                | Instr::F64Add { dst, .. }
                | Instr::F64Sub { dst, .. }
                | Instr::F64Mul { dst, .. }
                | Instr::F64Div { dst, .. }
                | Instr::I64Add { dst, .. }
                | Instr::I64Sub { dst, .. }
                | Instr::I64Mul { dst, .. }
                | Instr::U64Add { dst, .. }
                | Instr::U64Sub { dst, .. }
                | Instr::U64Mul { dst, .. }
                | Instr::U64And { dst, .. }
                | Instr::U64Or { dst, .. }
                | Instr::U64Xor { dst, .. }
                | Instr::U64Shl { dst, .. }
                | Instr::U64Shr { dst, .. }
                | Instr::I64Eq { dst, .. }
                | Instr::I64Lt { dst, .. }
                | Instr::U64Eq { dst, .. }
                | Instr::U64Lt { dst, .. }
                | Instr::U64Gt { dst, .. }
                | Instr::U64Le { dst, .. }
                | Instr::U64Ge { dst, .. }
                | Instr::F64Eq { dst, .. }
                | Instr::F64Lt { dst, .. }
                | Instr::F64Gt { dst, .. }
                | Instr::F64Le { dst, .. }
                | Instr::F64Ge { dst, .. }
                | Instr::I64And { dst, .. }
                | Instr::I64Or { dst, .. }
                | Instr::I64Xor { dst, .. }
                | Instr::I64Shl { dst, .. }
                | Instr::I64Shr { dst, .. }
                | Instr::I64Div { dst, .. }
                | Instr::I64Rem { dst, .. }
                | Instr::I64Gt { dst, .. }
                | Instr::I64Le { dst, .. }
                | Instr::I64Ge { dst, .. }
                | Instr::BoolNot { dst, .. }
                | Instr::U64ToI64 { dst, .. }
                | Instr::I64ToU64 { dst, .. }
                | Instr::U64Div { dst, .. }
                | Instr::U64Rem { dst, .. }
                | Instr::I64ToF64 { dst, .. }
                | Instr::U64ToF64 { dst, .. }
                | Instr::F64ToI64 { dst, .. }
                | Instr::F64ToU64 { dst, .. }
                | Instr::DecToI64 { dst, .. }
                | Instr::DecToU64 { dst, .. }
                | Instr::I64ToDec { dst, .. }
                | Instr::U64ToDec { dst, .. }
                | Instr::BytesEq { dst, .. }
                | Instr::StrEq { dst, .. }
                | Instr::BytesConcat { dst, .. }
                | Instr::StrConcat { dst, .. }
                | Instr::BytesGet { dst, .. }
                | Instr::BytesGetImm { dst, .. }
                | Instr::BytesSlice { dst, .. }
                | Instr::StrSlice { dst, .. }
                | Instr::StrToBytes { dst, .. }
                | Instr::BytesToStr { dst, .. }
                | Instr::Select { dst, .. }
                | Instr::TupleNew { dst, .. }
                | Instr::TupleGet { dst, .. }
                | Instr::TupleLen { dst, .. }
                | Instr::StructNew { dst, .. }
                | Instr::StructGet { dst, .. }
                | Instr::StructFieldCount { dst, .. }
                | Instr::ArrayNew { dst, .. }
                | Instr::ArrayLen { dst, .. }
                | Instr::ArrayGet { dst, .. }
                | Instr::ArrayGetImm { dst, .. }
                | Instr::BytesLen { dst, .. }
                | Instr::StrLen { dst, .. } => {
                    if (*dst as usize) < reg_count {
                        s.set(*dst as usize);
                    }
                }
                Instr::Call {
                    eff_out,
                    func_id: _,
                    eff_in: _,
                    args: _,
                    rets,
                } => {
                    if (*eff_out as usize) < reg_count {
                        s.set(*eff_out as usize);
                    }
                    for &r in rets {
                        if (r as usize) < reg_count {
                            s.set(r as usize);
                        }
                    }
                }
                Instr::HostCall { eff_out, rets, .. } => {
                    if (*eff_out as usize) < reg_count {
                        s.set(*eff_out as usize);
                    }
                    for &r in rets {
                        if (r as usize) < reg_count {
                            s.set(r as usize);
                        }
                    }
                }
                Instr::Ret { .. }
                | Instr::Br { .. }
                | Instr::Jmp { .. }
                | Instr::Trap { .. }
                | Instr::Nop => {}
            }
        }
        out[i] = s;
    }
    Ok(out)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BitSet {
    bits: Vec<u64>,
    len: usize,
}

impl BitSet {
    fn new_empty(len: usize) -> Self {
        let words = len.div_ceil(64);
        Self {
            bits: vec![0; words],
            len,
        }
    }

    fn new_full(len: usize) -> Self {
        let mut s = Self::new_empty(len);
        for w in &mut s.bits {
            *w = !0;
        }
        // Clear unused bits in last word.
        let rem = len % 64;
        if rem != 0 {
            let mask = (1_u64 << rem) - 1;
            if let Some(last) = s.bits.last_mut() {
                *last &= mask;
            }
        }
        s
    }

    fn get(&self, idx: usize) -> bool {
        if idx >= self.len {
            return false;
        }
        let w = idx / 64;
        let b = idx % 64;
        (self.bits[w] >> b) & 1 == 1
    }

    fn set(&mut self, idx: usize) {
        if idx >= self.len {
            return;
        }
        let w = idx / 64;
        let b = idx % 64;
        self.bits[w] |= 1_u64 << b;
    }

    fn intersect_with(&mut self, other: &Self) {
        for (a, b) in self.bits.iter_mut().zip(other.bits.iter()) {
            *a &= *b;
        }
    }

    fn union_with(&mut self, other: &Self) {
        for (a, b) in self.bits.iter_mut().zip(other.bits.iter()) {
            *a |= *b;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asm::Asm;
    use crate::asm::{FunctionSig, ProgramBuilder};
    use crate::program::{Const, FunctionDef, HostSymbol, StructTypeDef, TypeTableDef};
    use crate::value::FuncId;
    use alloc::vec;

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
                // const_unit r1
                bytecode: vec![0x10, 0x01],
                spans: vec![SpanEntry {
                    pc_delta: 0,
                    span_id: 1,
                }],
            }],
        );
        verify_program(&p, &VerifyConfig::default()).unwrap();
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
                    span_id: 1,
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
            Err(VerifyError::InvalidJumpTarget { func: 0, pc: 1 })
        );
    }

    #[test]
    fn verifier_rejects_call_arity_mismatch() {
        // Caller calls callee(1 arg) with 0 args.
        let caller = FunctionDef {
            arg_types: vec![],
            ret_types: vec![],
            reg_count: 2,
            // call r0, func=1, r0, argc=0, retc=0
            bytecode: {
                let mut a = Asm::new();
                a.call(0, FuncId(1), 0, &[], &[]);
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
                reg_count: 4,
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
                reg_count: 3,
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
                reg_count: 3,
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
                reg_count: 6,
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
                ret_types: vec![ValueType::Any],
                reg_count: 4,
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
                reg_count: 6,
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
                ret_types: vec![ValueType::Any],
                reg_count: 4,
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
                ret_types: vec![ValueType::Any],
                reg_count: 4,
            },
        )
        .unwrap();
        let p = pb.build();

        let err = verify_program(&p, &VerifyConfig::default()).unwrap_err();
        assert!(matches!(err, VerifyError::AggKindMismatch { .. }));
    }
}
