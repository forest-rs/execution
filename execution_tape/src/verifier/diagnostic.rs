// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use super::VerifyError;
use crate::value::FuncId;

impl VerifyError {
    /// Returns the function referenced by this diagnostic, when it has one.
    ///
    /// The index belongs to the rejected program and may itself be out of bounds.
    /// Program-wide metadata and decoding errors have no function location.
    #[must_use]
    pub fn function(&self) -> Option<FuncId> {
        match self {
            Self::BlockDeclaration { func, .. }
            | Self::BlockEdge { func, .. }
            | Self::InternalOpcodeSchemaMismatch { func, .. }
            | Self::UnsupportedInstruction { func, .. }
            | Self::FunctionBytecodeOutOfBounds { func, .. }
            | Self::FunctionSpansOutOfBounds { func, .. }
            | Self::FunctionArgTypesOutOfBounds { func, .. }
            | Self::FunctionRetTypesOutOfBounds { func, .. }
            | Self::FunctionSigCountMismatch { func, .. }
            | Self::UnsupportedValueType { func, .. }
            | Self::FunctionInputNameArgOutOfBounds { func, .. }
            | Self::FunctionInputNameEmpty { func, .. }
            | Self::FunctionInputNameDuplicate { func, .. }
            | Self::FunctionInputNameDuplicateArgIndex { func, .. }
            | Self::FunctionOutputNameRetOutOfBounds { func, .. }
            | Self::FunctionOutputNameEmpty { func, .. }
            | Self::FunctionOutputNameDuplicate { func, .. }
            | Self::FunctionOutputNameDuplicateRetIndex { func, .. }
            | Self::BadSpanDeltas { func, .. }
            | Self::RegCountTooLarge { func, .. }
            | Self::ArgCountExceedsRegs { func, .. }
            | Self::InvalidJumpTarget { func, .. }
            | Self::EffectInNotR0 { func, .. }
            | Self::EffectOutNotR0 { func, .. }
            | Self::RegOutOfBounds { func, .. }
            | Self::UninitializedRead { func, .. }
            | Self::CallArityMismatch { func, .. }
            | Self::ContractMismatch { func, .. }
            | Self::ReturnArityMismatch { func, .. }
            | Self::HostCallArityMismatch { func, .. }
            | Self::FuncIdOutOfBounds { func, .. }
            | Self::StructTypeOutOfBounds { func, .. }
            | Self::StructArityMismatch { func, .. }
            | Self::ArrayElemTypeOutOfBounds { func, .. }
            | Self::ArrayLenMismatch { func, .. }
            | Self::AggKindMismatch { func, .. }
            | Self::TupleIndexOutOfBounds { func, .. }
            | Self::StructFieldIndexOutOfBounds { func, .. }
            | Self::HostSigOutOfBounds { func, .. }
            | Self::CallSigOutOfBounds { func, .. }
            | Self::ConstOutOfBounds { func, .. }
            | Self::HostCallSigMalformed { func, .. }
            | Self::CallIndirectSigMalformed { func, .. }
            | Self::MissingRegisterTypes { func, .. }
            | Self::RegisterTypeMismatch { func, .. }
            | Self::UnknownAggregateShape { func, .. }
            | Self::CallCalleeArgTypesOutOfBounds { func, .. }
            | Self::CallCalleeRetTypesOutOfBounds { func, .. }
            | Self::MissingTerminator { func, .. }
            | Self::InternalBlockInconsistent { func, .. }
            | Self::BytecodeDecode { func, .. } => Some(FuncId(*func)),
            Self::InvalidContract { .. }
            | Self::InvalidMetadata { .. }
            | Self::Decode(..)
            | Self::HostSigMalformed { .. }
            | Self::CallSigMalformed { .. }
            | Self::HostSigHashMismatch { .. } => None,
        }
    }

    /// Returns the diagnostic's source bytecode offset within its function, when known.
    ///
    /// Function-level and program-wide errors have no instruction offset. An offset may
    /// identify a typed block header or malformed instruction, so it need not be executable.
    #[must_use]
    pub fn pc(&self) -> Option<u32> {
        match self {
            Self::BlockDeclaration { pc, .. }
            | Self::BlockEdge { pc, .. }
            | Self::InternalOpcodeSchemaMismatch { pc, .. }
            | Self::UnsupportedInstruction { pc, .. }
            | Self::InvalidJumpTarget { pc, .. }
            | Self::EffectInNotR0 { pc, .. }
            | Self::EffectOutNotR0 { pc, .. }
            | Self::RegOutOfBounds { pc, .. }
            | Self::UninitializedRead { pc, .. }
            | Self::CallArityMismatch { pc, .. }
            | Self::ContractMismatch { pc, .. }
            | Self::ReturnArityMismatch { pc, .. }
            | Self::HostCallArityMismatch { pc, .. }
            | Self::FuncIdOutOfBounds { pc, .. }
            | Self::StructTypeOutOfBounds { pc, .. }
            | Self::StructArityMismatch { pc, .. }
            | Self::ArrayElemTypeOutOfBounds { pc, .. }
            | Self::ArrayLenMismatch { pc, .. }
            | Self::AggKindMismatch { pc, .. }
            | Self::TupleIndexOutOfBounds { pc, .. }
            | Self::StructFieldIndexOutOfBounds { pc, .. }
            | Self::HostSigOutOfBounds { pc, .. }
            | Self::CallSigOutOfBounds { pc, .. }
            | Self::ConstOutOfBounds { pc, .. }
            | Self::HostCallSigMalformed { pc, .. }
            | Self::CallIndirectSigMalformed { pc, .. }
            | Self::UnknownAggregateShape { pc, .. }
            | Self::CallCalleeArgTypesOutOfBounds { pc, .. }
            | Self::CallCalleeRetTypesOutOfBounds { pc, .. }
            | Self::MissingTerminator { pc, .. }
            | Self::InternalBlockInconsistent { pc, .. } => Some(*pc),
            Self::RegisterTypeMismatch { pc, .. } => *pc,
            Self::InvalidContract { .. }
            | Self::InvalidMetadata { .. }
            | Self::Decode(..)
            | Self::FunctionBytecodeOutOfBounds { .. }
            | Self::FunctionSpansOutOfBounds { .. }
            | Self::FunctionArgTypesOutOfBounds { .. }
            | Self::FunctionRetTypesOutOfBounds { .. }
            | Self::FunctionSigCountMismatch { .. }
            | Self::UnsupportedValueType { .. }
            | Self::FunctionInputNameArgOutOfBounds { .. }
            | Self::FunctionInputNameEmpty { .. }
            | Self::FunctionInputNameDuplicate { .. }
            | Self::FunctionInputNameDuplicateArgIndex { .. }
            | Self::FunctionOutputNameRetOutOfBounds { .. }
            | Self::FunctionOutputNameEmpty { .. }
            | Self::FunctionOutputNameDuplicate { .. }
            | Self::FunctionOutputNameDuplicateRetIndex { .. }
            | Self::BadSpanDeltas { .. }
            | Self::RegCountTooLarge { .. }
            | Self::ArgCountExceedsRegs { .. }
            | Self::HostSigMalformed { .. }
            | Self::CallSigMalformed { .. }
            | Self::HostSigHashMismatch { .. }
            | Self::MissingRegisterTypes { .. }
            | Self::BytecodeDecode { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::{Instr, encode_instructions};
    use crate::program::{FunctionDef, Program, TypeTableDef, ValueType};
    use crate::verifier::{VerifyConfig, verify_program};
    use alloc::vec;

    #[test]
    fn rejected_program_locations_distinguish_instructions_signatures_and_metadata() {
        let mut program = Program::new(
            vec![],
            vec![],
            vec![],
            TypeTableDef::default(),
            vec![FunctionDef {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
                register_types: vec![ValueType::Unit, ValueType::I64],
                bytecode: encode_instructions(&[Instr::Ret {
                    eff_in: 0,
                    rets: vec![1],
                }])
                .unwrap(),
                spans: vec![],
            }],
        );
        let error = verify_program(&program, &VerifyConfig::default()).unwrap_err();
        assert!(matches!(error, VerifyError::UninitializedRead { .. }));
        assert_eq!(error.function(), Some(FuncId(0)));
        assert_eq!(error.pc(), Some(0));

        program.functions[0].arg_count = 1;
        // Reuse the return kind arena for this deliberately inconsistent argument declaration.
        program.functions[0].arg_types = program.functions[0].ret_types;
        program.register_type_ids[1] = u32::try_from(program.value_types.len()).unwrap();
        program.value_types.push(ValueType::Bool);
        let error = verify_program(&program, &VerifyConfig::default()).unwrap_err();
        assert!(matches!(error, VerifyError::RegisterTypeMismatch { .. }));
        assert_eq!(error.function(), Some(FuncId(0)));
        assert_eq!(error.pc(), None);

        program.symbols.clear();
        let error = verify_program(&program, &VerifyConfig::default()).unwrap_err();
        assert!(matches!(error, VerifyError::InvalidMetadata { .. }));
        assert_eq!(error.function(), None);
        assert_eq!(error.pc(), None);
    }
}
