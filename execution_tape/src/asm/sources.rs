// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Optional authoring-time source attribution, separate from executable program storage.

use super::*;
use crate::source::{SourceId, SourceMapping};
use core::ops::Range;

#[derive(Clone, Debug, Default)]
pub(super) struct Sources {
    current: Option<(SourceId, Range<usize>)>,
    instructions: Vec<(u32, SourceId, Range<usize>)>,
}
impl Sources {
    pub(super) fn instruction(&mut self, pc: u32) {
        if let Some((document, range)) = &self.current {
            self.instructions.push((pc, *document, range.clone()));
        }
    }
}
impl Asm {
    /// Attributes subsequent emitted instructions to an exact authored document range.
    ///
    /// Attribution lasts until another `source` call or [`Self::clear_source`]. The source text
    /// remains with the frontend; this stores only document IDs and half-open byte ranges.
    /// [`crate::source::SourceMap::new`] validates ranges against the exact document snapshot.
    ///
    /// Call [`Self::source_mappings`] before consuming the assembler. Labels and branch fixups
    /// preserve instruction offsets. Source capture is authoring work only: mappings are not
    /// encoded in bytecode, retained by verification, or consulted during execution.
    pub fn source(&mut self, document: SourceId, range: Range<usize>) -> &mut Self {
        self.sources.current = Some((document, range));
        self
    }

    /// Stops attributing new instructions. Previously recorded mappings remain available.
    pub fn clear_source(&mut self) -> &mut Self {
        self.sources.current = None;
        self
    }

    /// Iterates exact mappings for attributed instructions in this function's body.
    ///
    /// Supply the function whose body this assembler defines. Unattributed instructions have
    /// no entry; no declaration/source range is inferred. Repeated queries do not change the
    /// body. Collect these mappings into a separate [`crate::source::SourceMap`] after building
    /// the program. Finalization consumes this authoring data, so query before `define_function`.
    ///
    /// ```
    /// use execution_tape::{asm::{FunctionSig, ProgramBuilder}, program::ValueType,
    ///     source::{SourceDocument, SourceId, SourceLimits, SourceMap}};
    /// let mut program = ProgramBuilder::new();
    /// let entry = program.declare_function(FunctionSig { arg_types: vec![], ret_types: vec![ValueType::I64] })?;
    /// let mut body = program.function_asm(entry)?;
    /// let value = body.alloc_register(ValueType::I64);
    /// body.source(SourceId(0), 0..2).const_i64(value, 42).ret(0, &[value]);
    /// let mappings = body.source_mappings(entry).collect();
    /// program.define_function(entry, body)?;
    /// let program = program.build_verified()?;
    /// let sources = SourceMap::new(program.program(), vec![SourceDocument {
    ///     name: "answer.expr".into(), text: "42".into(),
    /// }], mappings, SourceLimits::default())?;
    /// sources.bind(program.program())?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn source_mappings(
        &self,
        function: FuncId,
    ) -> impl ExactSizeIterator<Item = SourceMapping> + '_ {
        self.sources
            .instructions
            .iter()
            .map(move |(pc, document, range)| SourceMapping {
                location: crate::trace::TraceLocation {
                    func: function,
                    pc: *pc,
                },
                document: *document,
                range: range.clone(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        disasm::disassemble_function,
        source::{SourceDocument, SourceError, SourceLimits, SourceMap},
    };

    #[test]
    fn source_ranges_follow_actual_instructions_through_labels_and_branch_fixups() {
        let mut program = ProgramBuilder::new();
        let entry = program
            .declare_function(FunctionSig {
                arg_types: vec![ValueType::Bool],
                ret_types: vec![ValueType::I64],
            })
            .unwrap();
        let mut body = program.function_asm(entry).unwrap();
        let condition = program.argument(entry, 0).unwrap();
        let answer = body.alloc_register(ValueType::I64);
        let yes = body.label();
        let no = body.label();
        let join = body.block(&[answer]);
        body.source(SourceId(0), 0..2).br(condition, yes, no);
        body.place(yes).unwrap();
        let value = body.alloc_register(ValueType::I64);
        body.source(SourceId(0), 3..5)
            .const_i64(value, 42)
            .jmp(join.with_args(&[value]));
        body.place(no).unwrap();
        body.source(SourceId(0), 6..7)
            .const_i64(value, 7)
            .jmp(join.with_args(&[value]));
        body.source(SourceId(0), 0..7);
        body.place(join).unwrap();
        body.ret(0, &[answer]);
        let mappings: Vec<_> = body.source_mappings(entry).collect();
        assert_eq!(mappings.len(), body.source_mappings(entry).len());
        program.define_function(entry, body).unwrap();
        let program = program.build_verified().unwrap();
        let sources = SourceMap::new(
            program.program(),
            vec![SourceDocument {
                name: "choice.expr".into(),
                text: "if 42 7".into(),
            }],
            mappings,
            SourceLimits::default(),
        )
        .unwrap();
        let bound = sources.bind(program.program()).unwrap();
        let code = disassemble_function(program.program(), entry).unwrap();
        for instruction in code.instrs() {
            let location = crate::trace::TraceLocation {
                func: entry,
                pc: instruction.pc(),
            };
            assert!(
                bound.lookup(location).is_some(),
                "every emitted instruction, including the join declaration"
            );
        }
    }

    #[test]
    fn optional_attribution_changes_neither_bytecode_nor_registers_and_preserves_gaps() {
        let mut plain = Asm::new();
        let mut attributed = Asm::new();
        let value = plain.alloc_register(ValueType::I64);
        assert_eq!(value, attributed.alloc_register(ValueType::I64));
        attributed.source(SourceId(3), 10..12);
        for body in [&mut plain, &mut attributed] {
            body.const_i64(value, 42);
        }
        attributed.clear_source();
        for body in [&mut plain, &mut attributed] {
            body.ret(0, &[value]);
        }
        assert_eq!(attributed.source_mappings(FuncId(8)).count(), 1);
        assert_eq!(plain.source_mappings(FuncId(8)).count(), 0);
        let left = plain.finish_parts().unwrap();
        let right = attributed.finish_parts().unwrap();
        assert_eq!(left.bytecode, right.bytecode);
        assert_eq!(left.register_types, right.register_types);
        assert_eq!(left.spans, right.spans);
    }

    #[test]
    fn source_sidecar_still_rejects_frontend_ranges_outside_the_snapshot() {
        let mut program = ProgramBuilder::new();
        let entry = program
            .declare_function(FunctionSig {
                arg_types: vec![],
                ret_types: vec![],
            })
            .unwrap();
        let mut body = program.function_asm(entry).unwrap();
        body.source(SourceId(0), 0..99).ret(0, &[]);
        let mappings = body.source_mappings(entry).collect();
        program.define_function(entry, body).unwrap();
        let program = program.build_verified().unwrap();
        assert!(matches!(
            SourceMap::new(
                program.program(),
                vec![SourceDocument {
                    name: "empty.expr".into(),
                    text: String::new(),
                }],
                mappings,
                SourceLimits::default()
            ),
            Err(SourceError::InvalidRange { .. })
        ));
    }
}
