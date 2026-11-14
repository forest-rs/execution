// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use super::{BlockDeclarationError, BlockEdgeError, ExceptionPlace, VerifyError};
use crate::disasm::disassemble_function;
use crate::program::Program;
use crate::value::FuncId;
use alloc::string::ToString;
use core::fmt;

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
            | Self::InvalidDecimalRounding { func, .. }
            | Self::TupleIndexOutOfBounds { func, .. }
            | Self::StructFieldIndexOutOfBounds { func, .. }
            | Self::HostSigOutOfBounds { func, .. }
            | Self::CallSigOutOfBounds { func, .. }
            | Self::ContBindSignature { func, .. }
            | Self::InvalidRegion { func, .. }
            | Self::InvalidRegionRun { func, .. }
            | Self::TagOutOfBounds { func, .. }
            | Self::ThrowArityMismatch { func, .. }
            | Self::ThrowPayloadMismatch { func, .. }
            | Self::ConstOutOfBounds { func, .. }
            | Self::HostCallSigMalformed { func, .. }
            | Self::CallIndirectContractMismatch { func, .. }
            | Self::CallIndirectSigMalformed { func, .. }
            | Self::MissingRegisterTypes { func, .. }
            | Self::RegisterTypeMismatch { func, .. }
            | Self::UnknownAggregateShape { func, .. }
            | Self::CallCalleeArgTypesOutOfBounds { func, .. }
            | Self::CallCalleeRetTypesOutOfBounds { func, .. }
            | Self::MissingTerminator { func, .. }
            | Self::InternalBlockInconsistent { func, .. }
            | Self::BytecodeDecode { func, .. }
            | Self::UnknownOpcode { func, .. } => Some(FuncId(*func)),
            Self::ExceptionReference {
                place: ExceptionPlace::Aggregate { func, .. },
            }
            | Self::ContinuationReference {
                place: ExceptionPlace::Aggregate { func, .. },
            } => Some(FuncId(*func)),
            Self::ExceptionReference { .. } | Self::ContinuationReference { .. } => None,
            Self::InvalidContract { .. }
            | Self::InvalidMetadata { .. }
            | Self::Decode(..)
            | Self::HostSigMalformed { .. }
            | Self::CallSigMalformed { .. }
            | Self::DuplicateTagName { .. }
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
            | Self::UnknownOpcode { pc, .. }
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
            | Self::InvalidDecimalRounding { pc, .. }
            | Self::TupleIndexOutOfBounds { pc, .. }
            | Self::StructFieldIndexOutOfBounds { pc, .. }
            | Self::HostSigOutOfBounds { pc, .. }
            | Self::CallSigOutOfBounds { pc, .. }
            | Self::ContBindSignature { pc, .. }
            | Self::TagOutOfBounds { pc, .. }
            | Self::ThrowArityMismatch { pc, .. }
            | Self::ThrowPayloadMismatch { pc, .. }
            | Self::ConstOutOfBounds { pc, .. }
            | Self::HostCallSigMalformed { pc, .. }
            | Self::CallIndirectContractMismatch { pc, .. }
            | Self::CallIndirectSigMalformed { pc, .. }
            | Self::UnknownAggregateShape { pc, .. }
            | Self::CallCalleeArgTypesOutOfBounds { pc, .. }
            | Self::CallCalleeRetTypesOutOfBounds { pc, .. }
            | Self::MissingTerminator { pc, .. }
            | Self::InternalBlockInconsistent { pc, .. } => Some(*pc),
            Self::RegisterTypeMismatch { pc, .. } => *pc,
            Self::ExceptionReference {
                place: ExceptionPlace::Aggregate { pc, .. },
            }
            | Self::ContinuationReference {
                place: ExceptionPlace::Aggregate { pc, .. },
            } => Some(*pc),
            Self::ExceptionReference { .. } | Self::ContinuationReference { .. } => None,
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
            | Self::DuplicateTagName { .. }
            | Self::InvalidRegion { .. }
            | Self::InvalidRegionRun { .. }
            | Self::HostSigHashMismatch { .. }
            | Self::MissingRegisterTypes { .. }
            | Self::BytecodeDecode { .. } => None,
        }
    }
}

impl VerifyError {
    /// Returns the register this diagnostic is about, when it names one.
    ///
    /// Like [`Self::function`], the index belongs to the rejected program and may be out of
    /// bounds.
    #[must_use]
    pub fn register(&self) -> Option<u32> {
        match self {
            Self::EffectInNotR0 { reg, .. }
            | Self::EffectOutNotR0 { reg, .. }
            | Self::RegOutOfBounds { reg, .. }
            | Self::UninitializedRead { reg, .. }
            | Self::ThrowPayloadMismatch { reg, .. }
            | Self::RegisterTypeMismatch { reg, .. }
            | Self::UnknownAggregateShape { reg, .. }
            | Self::BlockDeclaration {
                error:
                    BlockDeclarationError::InvalidRegister { reg, .. }
                    | BlockDeclarationError::DuplicateRegister { reg, .. },
                ..
            }
            | Self::BlockEdge {
                error: BlockEdgeError::Type { reg, .. },
                ..
            }
            | Self::ExceptionReference {
                place: ExceptionPlace::Aggregate { reg, .. },
            }
            | Self::ContinuationReference {
                place: ExceptionPlace::Aggregate { reg, .. },
            } => Some(*reg),
            _ => None,
        }
    }

    /// Renders this diagnostic with what `program`'s debug metadata knows about its location.
    ///
    /// `program` must be the program that was rejected. Beyond [`fmt::Display`], this names the
    /// function, the nearest label at or before the pc (as `label "name" + offset`), the span,
    /// the instruction, and the input name of an argument register:
    ///
    /// ```text
    /// function #0 "shade", pc 12 (label "weathered" + 4, span 7, f32.mul r6, [r3, r5]): r3 declared U32, required F32 (r3 is input "age")
    /// ```
    ///
    /// Metadata the program lacks is left out. Source text is not part of a program; map
    /// [`Self::function`] and [`Self::pc`] through a [`crate::source::SourceMap`] for it.
    #[must_use]
    pub fn display_in<'a>(&'a self, program: &'a Program) -> impl fmt::Display + 'a {
        InProgram {
            error: self,
            program,
        }
    }
}

/// A [`VerifyError`] rendered with names from the rejected program.
struct InProgram<'a> {
    error: &'a VerifyError,
    program: &'a Program,
}

impl fmt::Display for InProgram<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { error, program } = *self;
        let Some(FuncId(func)) = error.function() else {
            return error.fmt_message(f);
        };
        write!(f, "function #{func}")?;
        if let Some(name) = program.function_name(func) {
            write!(f, " {name:?}")?;
        }
        if let Some(pc) = error.pc() {
            write!(f, ", pc {pc}")?;
            let mut details = Details::new(f);
            if let Some((label_pc, name)) = label_before(program, func, pc) {
                match pc - label_pc {
                    0 => details.item(format_args!("label {name:?}"))?,
                    offset => details.item(format_args!("label {name:?} + {offset}"))?,
                }
            }
            if let Some(span) = span_at(program, func, pc) {
                details.item(format_args!("span {span}"))?;
            }
            if let Ok(disassembly) = disassemble_function(program, FuncId(func))
                && let Some(instr) = disassembly.instrs().find(|instr| instr.pc() == pc)
            {
                // The disassembler prefixes each line with its pc, which is already shown.
                let text = instr.to_string();
                let text = text
                    .split_once(": ")
                    .map_or(text.as_str(), |(_, rest)| rest);
                details.item(format_args!("{text}"))?;
            }
            details.finish()?;
        }
        f.write_str(": ")?;
        error.fmt_message(f)?;
        if let Some(reg) = error.register()
            && let Some(arg) = reg.checked_sub(1)
            && let Some(name) = program.function_input_name(func, arg)
        {
            write!(f, " (r{reg} is input {name:?})")?;
        }
        Ok(())
    }
}

/// Writes ` (a, b, c)`, or nothing when no item is written.
struct Details<'a, 'f> {
    f: &'a mut fmt::Formatter<'f>,
    open: bool,
}

impl<'a, 'f> Details<'a, 'f> {
    fn new(f: &'a mut fmt::Formatter<'f>) -> Self {
        Self { f, open: false }
    }

    fn item(&mut self, item: fmt::Arguments<'_>) -> fmt::Result {
        self.f.write_str(if self.open { ", " } else { " (" })?;
        self.open = true;
        self.f.write_fmt(item)
    }

    fn finish(self) -> fmt::Result {
        if self.open {
            self.f.write_str(")")?;
        }
        Ok(())
    }
}

/// The nearest named label of `func` at or before `pc`.
fn label_before(program: &Program, func: u32, pc: u32) -> Option<(u32, &str)> {
    let entry = program
        .labels
        .iter()
        .filter(|entry| entry.func == func && entry.pc <= pc)
        .max_by_key(|entry| entry.pc)?;
    Some((entry.pc, program.symbol_str(entry.name).ok()?))
}

/// The span covering `pc` in `func`'s span table, as the VM reports it for traps.
fn span_at(program: &Program, func: u32, pc: u32) -> Option<u64> {
    let spans = program.functions.get(func as usize)?.spans(program).ok()?;
    let mut at = 0_u64;
    let mut span = None;
    for entry in spans {
        at = at.saturating_add(entry.pc_delta);
        if at > u64::from(pc) {
            break;
        }
        span = Some(entry.span_id.get());
    }
    span
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::{Instr, encode_instructions};
    use crate::program::{FunctionDef, Program, TypeTableDef, ValueType};
    use crate::verifier::{VerifyConfig, verify_program};
    use alloc::string::ToString;
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

    #[test]
    fn display_prints_a_present_pc_and_omits_an_absent_one() {
        let at = VerifyError::UninitializedRead {
            func: 1,
            pc: 0,
            reg: 3,
        };
        assert_eq!(at.to_string(), "function #1, pc 0: uninitialized read: r3");
        let signature = VerifyError::RegisterTypeMismatch {
            func: 0,
            pc: None,
            reg: 1,
            declared: ValueType::U32,
            required: ValueType::F32,
        };
        assert_eq!(
            signature.to_string(),
            "function #0: r1 declared U32, required F32"
        );
        let program_wide = VerifyError::HostSigMalformed { host_sig: 2 };
        assert_eq!(program_wide.to_string(), "host_sig 2 malformed");
    }

    /// `shade(age: f32, wet: bool) -> f32` multiplies by `wet` after a named label, inside span 7.
    fn mistyped_program() -> Program {
        use crate::asm::{FunctionSig, ProgramBuilder};
        use crate::program::SpanId;

        let mut builder = ProgramBuilder::new();
        let shade = builder
            .declare_function(FunctionSig {
                arg_types: vec![ValueType::F32, ValueType::Bool],
                ret_types: vec![ValueType::F32],
            })
            .unwrap();
        builder.set_function_name(shade, "shade").unwrap();
        builder.set_function_input_name(shade, 0, "age").unwrap();
        builder.set_function_input_name(shade, 1, "wet").unwrap();
        let mut asm = builder.function_asm(shade).unwrap();
        let out = asm.alloc_register(ValueType::F32);
        asm.const_f32(out, 0.5);
        let weathered = asm.label_named("weathered");
        asm.place(weathered).unwrap();
        asm.span(SpanId::try_from(7_u64).unwrap())
            .f32_mul(out, 1, out)
            .f32_mul(out, 2, out)
            .ret(0, &[out]);
        builder.define_function(shade, asm).unwrap();
        builder.build()
    }

    #[test]
    fn display_in_names_the_location_from_program_metadata() {
        let program = mistyped_program();
        let error = verify_program(&program, &VerifyConfig::default()).unwrap_err();
        assert_eq!(error.register(), Some(2));
        assert_eq!(
            error.to_string(),
            "function #0, pc 12: r2 declared Bool, required F32"
        );
        assert_eq!(
            error.display_in(&program).to_string(),
            "function #0 \"shade\", pc 12 (label \"weathered\" + 5, span 7, f32.mul r3, [r2, r3]): \
             r2 declared Bool, required F32 (r2 is input \"wet\")"
        );
    }

    #[test]
    fn display_in_leaves_out_metadata_the_program_lacks() {
        let mut program = mistyped_program();
        program.function_names.clear();
        program.labels.clear();
        program.value_name_ids.iter_mut().for_each(|id| *id = 0);
        let error = verify_program(&program, &VerifyConfig::default()).unwrap_err();
        let shown = error.display_in(&program).to_string();
        assert!(
            shown.starts_with("function #0, pc 12 (span 7, f32.mul r3, [r2, r3]): r2 declared"),
            "{shown}"
        );
        assert!(!shown.contains("input"), "{shown}");
        let program_wide = VerifyError::HostSigMalformed { host_sig: 2 };
        assert_eq!(
            program_wide.display_in(&program).to_string(),
            "host_sig 2 malformed"
        );
    }
}
