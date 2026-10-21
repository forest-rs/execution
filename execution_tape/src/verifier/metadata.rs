// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Validation of immutable program metadata, including unused entries.

use super::VerifyError;
use crate::format::DecodeError;
use crate::program::{ConstEntry, ConstId, FieldNameId, Program};

/// Program metadata table containing an invalid entry.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum MetadataTable {
    /// Constant pool.
    Constants,
    /// Symbol table, including its reserved empty entry at index zero.
    Symbols,
    /// Interned struct field names.
    FieldNames,
    /// Struct definitions and their field ranges/counts.
    Structs,
    /// Required function register kind ranges and type IDs.
    RegisterTypes,
    /// Packed optional function value names.
    ValueNames,
    /// Optional program name (entry zero).
    ProgramName,
    /// Function name entries.
    FunctionNames,
    /// Label name entries.
    LabelNames,
    /// Function argument name ranges, indexed by function.
    ArgumentNames,
    /// Function return name ranges, indexed by function.
    ReturnNames,
}

fn invalid(table: MetadataTable, entry: usize, error: DecodeError) -> VerifyError {
    VerifyError::InvalidMetadata {
        table,
        entry: u32::try_from(entry).unwrap_or(u32::MAX),
        error,
    }
}

pub(super) fn verify(program: &Program) -> Result<(), VerifyError> {
    use MetadataTable as T;
    for (index, constant) in program.const_pool.iter().enumerate() {
        let id = ConstId(
            u32::try_from(index)
                .map_err(|_| invalid(T::Constants, index, DecodeError::OutOfBounds))?,
        );
        match constant {
            ConstEntry::Bytes(_) => {
                program
                    .const_bytes(id)
                    .map_err(|e| invalid(T::Constants, index, e))?;
            }
            ConstEntry::Str(_) => {
                program
                    .const_str(id)
                    .map_err(|e| invalid(T::Constants, index, e))?;
            }
            _ => {}
        }
    }
    let reserved = program
        .symbols
        .first()
        .ok_or_else(|| invalid(T::Symbols, 0, DecodeError::OutOfBounds))?;
    if reserved.bytes.len != 0 {
        return Err(invalid(T::Symbols, 0, DecodeError::OutOfBounds));
    }
    for (index, symbol) in program.symbols.iter().enumerate() {
        let end = symbol
            .bytes
            .end()
            .map_err(|e| invalid(T::Symbols, index, e))?;
        program
            .symbol_data
            .get(symbol.bytes.offset as usize..end as usize)
            .ok_or_else(|| invalid(T::Symbols, index, DecodeError::InvalidUtf8))?;
    }
    for (index, _) in program.types.field_name_ranges.iter().enumerate() {
        let id = FieldNameId(
            u32::try_from(index)
                .map_err(|_| invalid(T::FieldNames, index, DecodeError::OutOfBounds))?,
        );
        let bytes = program
            .types
            .field_name_bytes(id)
            .ok_or_else(|| invalid(T::FieldNames, index, DecodeError::OutOfBounds))?;
        core::str::from_utf8(bytes)
            .map_err(|_| invalid(T::FieldNames, index, DecodeError::InvalidUtf8))?;
    }
    for (index, st) in program.types.structs.iter().enumerate() {
        let names = program
            .types
            .struct_field_name_ids(st)
            .map_err(|e| invalid(T::Structs, index, e))?;
        let types = program
            .types
            .struct_field_types(st)
            .map_err(|e| invalid(T::Structs, index, e))?;
        if names.len() != types.len()
            || names
                .iter()
                .any(|id| id.0 as usize >= program.types.field_name_ranges.len())
        {
            return Err(invalid(T::Structs, index, DecodeError::OutOfBounds));
        }
    }
    for (index, &id) in program.register_type_ids.iter().enumerate() {
        if id as usize >= program.value_types.len() {
            return Err(invalid(T::RegisterTypes, index, DecodeError::OutOfBounds));
        }
    }
    let valid_name = |id: u32| id == 0 || (id as usize) < program.symbols.len();
    for (index, &name) in program.value_name_ids.iter().enumerate() {
        if !valid_name(name) {
            return Err(invalid(T::ValueNames, index, DecodeError::OutOfBounds));
        }
    }
    if let Some(name) = program.program_name
        && !valid_name(name.index())
    {
        return Err(invalid(T::ProgramName, 0, DecodeError::OutOfBounds));
    }
    for (index, name) in program.function_names.iter().enumerate() {
        if name.func as usize >= program.functions.len() || !valid_name(name.name.index()) {
            return Err(invalid(T::FunctionNames, index, DecodeError::OutOfBounds));
        }
    }
    for (index, label) in program.labels.iter().enumerate() {
        if !valid_name(label.name.index())
            || program
                .functions
                .get(label.func as usize)
                .is_none_or(|f| label.pc > f.bytecode.len)
        {
            return Err(invalid(T::LabelNames, index, DecodeError::OutOfBounds));
        }
    }
    for (index, func) in program.functions.iter().enumerate() {
        func.register_type_ids(program)
            .map_err(|error| invalid(T::RegisterTypes, index, error))?;
        for (range, count, table) in [
            (func.arg_name_ids, func.arg_count, T::ArgumentNames),
            (func.ret_name_ids, func.ret_count, T::ReturnNames),
        ] {
            let end = range.end().map_err(|e| invalid(table, index, e))?;
            if (range.len != 0 && range.len != count)
                || program
                    .value_name_ids
                    .get(range.offset as usize..end as usize)
                    .is_none()
            {
                return Err(invalid(table, index, DecodeError::OutOfBounds));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asm::{Asm, FunctionSig, ProgramBuilder};
    use crate::program::{
        ByteRange, Const, FunctionNameEntry, HostSymbol, LabelNameEntry, StructTypeDef,
        TypeTableDef, ValueType,
    };
    use crate::verifier::{
        VerifyConfig, verify_program, verify_program_owned, verify_program_owned_with_lints,
        verify_program_with_lints,
    };
    use alloc::vec;
    use core::num::NonZeroU32;

    fn assert_invalid(p: Program, table: MetadataTable) {
        let cfg = VerifyConfig::default();
        for result in [
            verify_program(&p, &cfg),
            verify_program_with_lints(&p, &cfg).map(|_| ()),
            verify_program_owned(p.clone(), &cfg).map(|_| ()),
            verify_program_owned_with_lints(p.clone(), &cfg).map(|_| ()),
        ] {
            assert!(
                matches!(result, Err(VerifyError::InvalidMetadata { table: actual, .. }) if actual == table),
                "expected {table:?}: {result:?}"
            );
        }
    }

    #[test]
    fn rejects_unused_invalid_constant_ranges_and_utf8_boundaries() {
        for text in [false, true] {
            let base = Program::new(
                vec![],
                vec![if text {
                    Const::Str("é".into())
                } else {
                    Const::Bytes(vec![1])
                }],
                vec![],
                TypeTableDef::default(),
                vec![],
            );
            for range in [
                ByteRange { offset: 10, len: 1 },
                ByteRange {
                    offset: u32::MAX,
                    len: 1,
                },
                ByteRange { offset: 1, len: 1 },
            ] {
                let mut p = base.clone();
                p.const_pool[0] = if text {
                    ConstEntry::Str(range)
                } else {
                    ConstEntry::Bytes(range)
                };
                assert_invalid(p, MetadataTable::Constants);
            }
        }
    }

    #[test]
    fn rejects_malformed_symbols_structs_and_names_without_executing_functions() {
        let mut base = Program::new(
            vec![HostSymbol {
                symbol: "é".into()
            }],
            vec![],
            vec![],
            TypeTableDef {
                structs: vec![StructTypeDef {
                    field_names: vec!["x".into()],
                    field_types: vec![ValueType::I64],
                }],
                array_elems: vec![],
            },
            vec![],
        );
        let mut p = base.clone();
        p.symbols[1].bytes.offset = 1;
        assert_invalid(p, MetadataTable::Symbols);
        let mut p = base.clone();
        p.symbols.clear();
        assert_invalid(p, MetadataTable::Symbols);
        let mut p = base.clone();
        p.types.name_data[0] = 0xff;
        assert_invalid(p, MetadataTable::FieldNames);
        let mut p = base.clone();
        p.types.field_name_ranges[0].len = 9;
        assert_invalid(p, MetadataTable::FieldNames);
        let mut p = base.clone();
        p.types.structs[0].field_types.len = 0;
        assert_invalid(p, MetadataTable::Structs);
        let mut p = base.clone();
        p.types.field_name_ids[0] = FieldNameId(9);
        assert_invalid(p, MetadataTable::Structs);
        let mut p = base.clone();
        p.types.structs[0].field_types.offset = 9;
        assert_invalid(p, MetadataTable::Structs);
        let invalid_symbol = crate::program::SymbolId(NonZeroU32::new(9).unwrap());
        let mut p = base.clone();
        p.program_name = Some(invalid_symbol);
        assert_invalid(p, MetadataTable::ProgramName);
        let mut p = base.clone();
        p.function_names.push(FunctionNameEntry {
            func: 0,
            name: invalid_symbol,
        });
        assert_invalid(p, MetadataTable::FunctionNames);
        let mut p = base.clone();
        p.labels.push(LabelNameEntry {
            func: 0,
            pc: 0,
            name: invalid_symbol,
        });
        assert_invalid(p, MetadataTable::LabelNames);
        base.value_name_ids.push(9);
        assert_invalid(base, MetadataTable::ValueNames);
    }

    #[test]
    fn function_name_ranges_have_signature_arity() {
        let mut pb = ProgramBuilder::new();
        let mut a = Asm::new();
        a.set_register_types(vec![ValueType::Unit, ValueType::I64]);
        a.ret(0, &[1]);
        pb.push_function_checked(
            a,
            FunctionSig {
                arg_types: vec![ValueType::I64],
                ret_types: vec![ValueType::I64],
            },
        )
        .unwrap();
        let base = pb.build();
        let mut p = base.clone();
        p.value_name_ids = vec![0, 0];
        p.functions[0].arg_name_ids.len = 2;
        assert_invalid(p, MetadataTable::ArgumentNames);
        let mut p = base.clone();
        p.functions[0].ret_name_ids.offset = 9;
        assert_invalid(p, MetadataTable::ReturnNames);
        verify_program(&base, &VerifyConfig::default()).unwrap();
    }
}
