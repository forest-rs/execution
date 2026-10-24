// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Raw containers must satisfy the same executable-body invariant as assembled functions.

use execution_tape::{
    program::{FunctionDef, Program, TypeTableDef, ValueType},
    verifier::{
        ProgramAnalysis, VerifyConfig, VerifyError, verify_program, verify_program_owned,
        verify_program_owned_with_lints, verify_program_with_lints,
    },
};
#[test]
fn empty_function_is_rejected_at_every_preparation_entrypoint() {
    let program = Program::new(
        vec![],
        vec![],
        vec![],
        TypeTableDef::default(),
        vec![FunctionDef {
            arg_types: vec![],
            ret_types: vec![],
            register_types: vec![ValueType::Unit],
            bytecode: vec![],
            spans: vec![],
        }],
    );
    let config = VerifyConfig::default();
    let expected = VerifyError::MissingTerminator { func: 0, pc: 0 };
    assert_eq!(verify_program(&program, &config), Err(expected.clone()));
    assert!(
        matches!(verify_program_owned(program.clone(), &config), Err(error) if error == expected)
    );
    assert!(
        matches!(verify_program_with_lints(&program, &config), Err(error) if error == expected)
    );
    assert!(
        matches!(verify_program_owned_with_lints(program.clone(), &config), Err(error) if error == expected)
    );
    assert!(matches!(ProgramAnalysis::new(&program, &config), Err(error) if error == expected));
    let roundtrip = Program::decode(&program.encode()).unwrap();
    assert_eq!(verify_program(&roundtrip, &config), Err(expected));
}
