// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Producer-declared register kinds through the public assembly, codec and preparation APIs.

use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
use execution_tape::format::{DecodeError, Reader};
use execution_tape::host::{Host, HostContext, HostError, SigHash, ValueRef};
use execution_tape::program::{HostTypeId, Program, ValueType};
use execution_tape::trace::TraceMask;
use execution_tape::value::{FuncId, Value};
use execution_tape::vm::{Limits, Vm};

#[derive(Debug)]
struct NoHost;
impl Host for NoHost {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        _: &[ValueRef<'_>],
        _: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        Err(HostError::UnknownSymbol)
    }
}
use execution_tape::verifier::{
    MetadataTable, VerifyConfig, VerifyError, verify_program, verify_program_owned,
    verify_program_owned_with_lints, verify_program_with_lints,
};

fn program(mut asm: Asm, kinds: &[ValueType], args: &[ValueType], rets: &[ValueType]) -> Program {
    asm.set_register_types(kinds.to_vec());
    let mut builder = ProgramBuilder::new();
    builder
        .push_function_checked(
            asm,
            FunctionSig {
                arg_types: args.into(),
                ret_types: rets.into(),
            },
        )
        .unwrap();
    builder.build()
}

fn error(program: &Program) -> VerifyError {
    let config = VerifyConfig::default();
    let expected = verify_program(program, &config).unwrap_err();
    assert_eq!(
        verify_program_with_lints(program, &config).unwrap_err(),
        expected,
        "verification entrypoints must diagnose the same invalid contract"
    );
    assert_eq!(
        verify_program_owned(program.clone(), &config).unwrap_err(),
        expected,
        "verification entrypoints must diagnose the same invalid contract"
    );
    assert_eq!(
        verify_program_owned_with_lints(program.clone(), &config).unwrap_err(),
        expected,
        "verification entrypoints must diagnose the same invalid contract"
    );
    expected
}

#[test]
fn declared_kinds_roundtrip_share_storage_and_ignore_section_order() {
    let mut asm = Asm::new();
    asm.ret(0, &[1]);
    let kinds = [
        ValueType::Unit,
        ValueType::Obj(HostTypeId(u64::MAX)),
        ValueType::I64,
        ValueType::I64,
        ValueType::Obj(HostTypeId(u64::MAX)),
    ];
    let p = program(asm, &kinds, &kinds[1..2], &kinds[1..2]);
    assert_eq!(p.register_type_ids[1], p.register_type_ids[4]);
    assert_eq!(p.register_type_ids[2], p.register_type_ids[3]);
    let function = &p.functions[0];
    assert_eq!(
        function
            .register_types(&p)
            .unwrap()
            .iter()
            .collect::<Vec<_>>(),
        kinds
    );
    assert_eq!(function.register_type(&p, 4).unwrap(), kinds[4]);
    verify_program(&p, &VerifyConfig::default()).unwrap();
    let encoded = p.encode();
    assert_eq!(Program::decode(&encoded).unwrap(), p);
    // Container header is magic[8], major[2], minor[2]. Reverse complete sections.
    let mut reader = Reader::new(&encoded[12..]);
    let mut sections = Vec::new();
    while reader.offset() < encoded.len() - 12 {
        let start = reader.offset();
        reader.read_u8().unwrap();
        let len = usize::try_from(reader.read_uleb128_u64().unwrap()).unwrap();
        reader.read_bytes(len).unwrap();
        sections.push(&encoded[12 + start..12 + reader.offset()]);
    }
    let mut reordered = encoded[..12].to_vec();
    for section in sections.into_iter().rev() {
        reordered.extend_from_slice(section);
    }
    assert_eq!(Program::decode(&reordered).unwrap(), p);
}

#[test]
fn declarations_do_not_initialize_registers() {
    let mut asm = Asm::new();
    asm.ret(0, &[1]);
    let p = program(
        asm,
        &[ValueType::Unit, ValueType::I64],
        &[],
        &[ValueType::I64],
    );
    assert!(matches!(
        error(&p),
        VerifyError::UninitializedRead { reg: 1, .. }
    ));
}

#[test]
fn every_definition_must_match_even_without_a_typed_use() {
    let mut asm = Asm::new();
    asm.const_bool(1, true).ret(0, &[]);
    let p = program(asm, &[ValueType::Unit, ValueType::I64], &[], &[]);
    assert!(matches!(
        error(&p),
        VerifyError::RegisterTypeMismatch {
            func: 0,
            pc: Some(0),
            reg: 1,
            declared: ValueType::I64,
            required: ValueType::Bool,
        }
    ));
}

#[test]
fn argument_prefix_and_effect_register_are_checked() {
    for kinds in [
        [ValueType::Bool, ValueType::I64],
        [ValueType::Unit, ValueType::Bool],
    ] {
        let mut asm = Asm::new();
        asm.ret(0, &[]);
        let p = program(asm, &kinds, &[ValueType::I64], &[]);
        assert!(matches!(
            error(&p),
            VerifyError::RegisterTypeMismatch { pc: None, .. }
        ));
    }
}

#[test]
fn declarations_cannot_forge_projection_proofs() {
    let mut opaque = Asm::new();
    opaque.tuple_get(2, 1, 0).ret(0, &[2]);
    let p = program(
        opaque,
        &[ValueType::Unit, ValueType::Agg, ValueType::I64],
        &[ValueType::Agg],
        &[ValueType::I64],
    );
    assert!(matches!(
        error(&p),
        VerifyError::UnknownAggregateShape { reg: 1, .. }
    ));

    let mut known = Asm::new();
    known
        .const_bool(1, true)
        .tuple_new(2, &[1])
        .tuple_get(3, 2, 0)
        .ret(0, &[3]);
    let p = program(
        known,
        &[
            ValueType::Unit,
            ValueType::Bool,
            ValueType::Agg,
            ValueType::I64,
        ],
        &[],
        &[ValueType::I64],
    );
    assert!(matches!(
        error(&p),
        VerifyError::RegisterTypeMismatch {
            reg: 3,
            declared: ValueType::I64,
            required: ValueType::Bool,
            ..
        }
    ));
}

#[test]
fn malformed_ledger_ranges_counts_and_unused_ids_are_rejected() {
    let mut asm = Asm::new();
    asm.ret(0, &[]);
    let base = program(asm, &[ValueType::Unit], &[], &[]);
    for case in 0..4 {
        let mut p = base.clone();
        match case {
            0 => p.functions[0].register_types.offset = u32::MAX,
            1 => p.functions[0].register_types.len = 2,
            2 => p.register_type_ids[0] = u32::MAX,
            _ => p.register_type_ids.push(u32::MAX),
        }
        assert!(matches!(
            error(&p),
            VerifyError::InvalidMetadata {
                table: MetadataTable::RegisterTypes,
                ..
            }
        ));
    }
}

#[test]
fn block_parameter_kinds_come_from_the_function() {
    let mut asm = Asm::new();
    let join = asm.block(&[2]);
    asm.const_i64(1, 7).jmp(join.with_args(&[1]));
    asm.place(join).unwrap();
    asm.ret(0, &[2]);
    let p = program(
        asm,
        &[ValueType::Unit, ValueType::I64, ValueType::I64],
        &[],
        &[ValueType::I64],
    );
    let verified = verify_program_owned(
        Program::decode(&p.encode()).unwrap(),
        &VerifyConfig::default(),
    )
    .unwrap();
    assert!(verified.function(FuncId(0)).is_some());
    let outputs = Vm::new(NoHost, Limits::default())
        .run(&verified, FuncId(0), &[], TraceMask::NONE, None)
        .unwrap();
    assert!(matches!(outputs.as_slice(), [Value::I64(7)]));
}

#[test]
fn prior_wire_version_is_rejected_explicitly() {
    let mut asm = Asm::new();
    asm.ret(0, &[]);
    let mut bytes = program(asm, &[ValueType::Unit], &[], &[]).encode();
    bytes[10..12].copy_from_slice(&1_u16.to_le_bytes());
    assert!(matches!(
        Program::decode(&bytes),
        Err(DecodeError::UnsupportedVersion { minor: 1, .. })
    ));
}

#[test]
fn invalid_definition_is_reported_before_dependent_shape_facts() {
    let mut asm = Asm::new();
    asm.const_i64(1, 7)
        .tuple_new(2, &[1])
        .tuple_get(3, 2, 0)
        .ret(0, &[3]);
    let p = program(
        asm,
        &[
            ValueType::Unit,
            ValueType::Bool,
            ValueType::Agg,
            ValueType::I64,
        ],
        &[],
        &[ValueType::I64],
    );
    assert_eq!(
        error(&p),
        VerifyError::RegisterTypeMismatch {
            func: 0,
            pc: Some(0),
            reg: 1,
            declared: ValueType::Bool,
            required: ValueType::I64,
        }
    );
}

#[test]
fn nested_projection_overwriting_its_source_keeps_the_inner_proof() {
    let mut asm = Asm::new();
    asm.const_i64(1, 7)
        .tuple_new(2, &[1])
        .tuple_new(3, &[2])
        .tuple_get(3, 3, 0);
    asm.tuple_get(4, 3, 0).ret(0, &[4]);
    let p = program(
        asm,
        &[
            ValueType::Unit,
            ValueType::I64,
            ValueType::Agg,
            ValueType::Agg,
            ValueType::I64,
        ],
        &[],
        &[ValueType::I64],
    );
    let decoded = Program::decode(&p.encode()).unwrap();
    let config = VerifyConfig::default();
    verify_program(&decoded, &config).unwrap();
    verify_program_with_lints(&decoded, &config).unwrap();
    let prepared = verify_program_owned(decoded.clone(), &config).unwrap();
    verify_program_owned_with_lints(decoded, &config).unwrap();
    assert_eq!(
        Vm::new(NoHost, Limits::default())
            .run(&prepared, FuncId(0), &[], TraceMask::NONE, None)
            .unwrap(),
        vec![Value::I64(7)]
    );
}

#[test]
fn logical_register_views_ignore_backing_pool_ids_and_unrelated_entries() {
    use execution_tape::host::HostSig;
    use std::hash::{Hash, Hasher};
    let kinds = [
        ValueType::Unit,
        ValueType::I64,
        ValueType::Obj(HostTypeId(u64::MAX)),
    ];
    let mut asm = Asm::new();
    asm.ret(0, &[]);
    let first = program(asm, &kinds, &[], &[]);
    let mut builder = ProgramBuilder::new();
    builder.host_sig_for(
        "unused",
        HostSig {
            args: vec![ValueType::Bool],
            rets: vec![ValueType::Bytes],
        },
    );
    builder.call_sig(&[ValueType::Closure], &[ValueType::Agg]);
    for _ in 0..2 {
        let mut asm = Asm::new();
        asm.set_register_types(kinds.to_vec()).ret(0, &[]);
        builder
            .push_function_checked(
                asm,
                FunctionSig {
                    arg_types: vec![],
                    ret_types: vec![],
                },
            )
            .unwrap();
    }
    let second = builder.build();
    assert_ne!(first.register_type_ids[0], second.register_type_ids[0]);
    let left = first.functions[0].register_types(&first).unwrap();
    let right = second.functions[1].register_types(&second).unwrap();
    assert_eq!(left, right);
    assert_eq!(format!("{left:?}"), format!("{right:?}"));
    let mut left_hash = std::collections::hash_map::DefaultHasher::new();
    let mut right_hash = std::collections::hash_map::DefaultHasher::new();
    left.hash(&mut left_hash);
    right.hash(&mut right_hash);
    assert_eq!(left_hash.finish(), right_hash.finish());
    assert_eq!(Program::decode(&second.encode()).unwrap(), second);
    verify_program(&second, &VerifyConfig::default()).unwrap();
}

#[test]
fn inactive_functions_and_instructions_have_static_contracts_checked() {
    let mut asm = Asm::new();
    asm.trap(7).const_bool(1, true).ret(0, &[]);
    let p = program(asm, &[ValueType::Unit, ValueType::I64], &[], &[]);
    assert!(matches!(
        error(&p),
        VerifyError::RegisterTypeMismatch {
            reg: 1,
            required: ValueType::Bool,
            ..
        }
    ));
    let mut builder = ProgramBuilder::new();
    for declared in [ValueType::I64, ValueType::Bool] {
        let mut asm = Asm::new();
        asm.set_register_types(vec![ValueType::Unit, declared])
            .ret(0, &[]);
        builder
            .push_function_checked(
                asm,
                FunctionSig {
                    arg_types: vec![ValueType::I64],
                    ret_types: vec![],
                },
            )
            .unwrap();
    }
    assert!(matches!(
        error(&builder.build()),
        VerifyError::RegisterTypeMismatch {
            func: 1,
            pc: None,
            reg: 1,
            ..
        }
    ));
}

#[test]
fn missing_declarations_are_not_inferred() {
    let mut asm = Asm::new();
    asm.ret(0, &[]);
    let p = program(asm, &[], &[], &[]);
    assert_eq!(error(&p), VerifyError::MissingRegisterTypes { func: 0 });
}

#[test]
fn malformed_encoded_ledgers_are_rejected_before_allocation_or_field_reinterpretation() {
    use execution_tape::format::Writer;
    let mut asm = Asm::new();
    asm.ret(0, &[]);
    let encoded = program(asm, &[ValueType::Unit], &[], &[]).encode();
    for table in [
        // One function, no arguments/results, two kinds: Unit then truncated Obj(u64).
        vec![1, 0, 0, 2, 0, 8, 255, 255],
        // One function, one declaration with an unknown kind tag.
        vec![1, 0, 0, 1, 255, 0, 0],
        // Hostile register count, with no matching encoded kinds.
        vec![1, 0, 0, 255, 255, 255, 255, 15, 0, 0],
        // Old table shape cannot silently be reinterpreted as declarations and references.
        vec![1, 0, 0, 1, 0, 0],
    ] {
        let mut reader = Reader::new(&encoded[12..]);
        let mut writer = Writer::new();
        writer.write_bytes(&encoded[..12]);
        while reader.offset() < encoded.len() - 12 {
            let tag = reader.read_u8().unwrap();
            let len = usize::try_from(reader.read_uleb128_u64().unwrap()).unwrap();
            let original = reader.read_bytes(len).unwrap();
            let payload = if tag == 4 { table.as_slice() } else { original };
            writer.write_u8(tag);
            writer.write_uleb128_u64(u64::try_from(payload.len()).unwrap());
            writer.write_bytes(payload);
        }
        assert!(Program::decode(writer.as_slice()).is_err());
    }
}

#[test]
#[should_panic(expected = "cannot encode malformed register declarations")]
fn encoder_never_silently_omits_required_declarations() {
    let mut asm = Asm::new();
    asm.ret(0, &[]);
    let mut p = program(asm, &[ValueType::Unit], &[], &[]);
    p.register_type_ids[0] = u32::MAX;
    let _ = p.encode();
}
