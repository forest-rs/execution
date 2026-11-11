// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Typed exception tags.

use execution_tape::{
    asm::{ContractRef, ProgramBuilder, TagBuildError},
    contract::Contract,
    disasm::disassemble,
    program::{Program, ValueType},
    verifier::{MetadataTable, VerifyConfig, VerifyError, verify_program},
};

fn tagged() -> Program {
    let mut b = ProgramBuilder::new();
    let integer = b.contract(Contract::Kind(ValueType::I64)).unwrap();
    let pair = b.contract(Contract::Tuple(&[Some(integer)])).unwrap();
    b.declare_tag(
        "failed",
        &[
            ValueType::I64.into(),
            ValueType::Str.into(),
            ContractRef::Id(pair),
        ],
    )
    .unwrap();
    b.build_checked().unwrap()
}

#[test]
fn tags_round_trip_and_disassemble_with_their_names() {
    let program = tagged();
    assert_eq!(Program::decode(&program.encode()).unwrap(), program);
    let tag = execution_tape::program::TagId(0);
    assert_eq!(program.tag_name(tag), Some("failed"));
    let text = disassemble(&program).to_string();
    assert!(
        text.starts_with("tag#0 \"failed\" ; payload=[I64, Str, Agg]\n"),
        "{text}"
    );
}

#[test]
fn builder_rejects_duplicate_tag_names() {
    let mut b = ProgramBuilder::new();
    let failed = b.declare_tag("failed", &[ValueType::I64.into()]).unwrap();
    assert_eq!(
        b.declare_tag("failed", &[ValueType::Bool.into()]),
        Err(TagBuildError::DuplicateName {
            name: "failed".into(),
            existing: failed,
        })
    );
    // Equal payloads still declare distinct tags; an empty payload needs no annotation.
    assert_ne!(
        b.declare_tag("other", &[ValueType::I64.into()]).unwrap(),
        failed
    );
    assert_eq!(b.declare_tag("abort", &[]).unwrap().0, 2);
}

#[test]
fn verifier_checks_every_tag_table_entry() {
    let base = tagged();
    let cfg = VerifyConfig::default();
    let mut duplicate = base.clone();
    let mut copy = duplicate.tags[0].clone();
    duplicate.tags.push(copy.clone());
    assert_eq!(
        verify_program(&duplicate, &cfg),
        Err(VerifyError::DuplicateTagName {
            tag: 1,
            first: 0,
            name: "failed".into(),
        })
    );
    copy.payload.offset = u32::MAX;
    let mut malformed = base;
    malformed.tags.push(copy);
    assert!(matches!(
        verify_program(&malformed, &cfg),
        Err(VerifyError::InvalidMetadata {
            table: MetadataTable::Tags,
            entry: 1,
            ..
        })
    ));
}
