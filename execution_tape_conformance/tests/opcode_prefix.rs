// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The prefixed opcode space: two-byte opcodes behind a reserved lead byte.
//!
//! Unknown single bytes, unknown bytes after an assigned prefix and a prefix cut off by the end
//! of the function are all rejected, by every decoder, with the offending code and location.

use execution_tape::codec::{DecodeError as CodecError, decode_instruction};
use execution_tape::disasm::{DisasmError, disassemble_function};
use execution_tape::format::DecodeError;
use execution_tape::opcode::{Opcode, OpcodePrefix};
use execution_tape::program::{FunctionDef, Program, TypeTableDef, ValueType};
use execution_tape::value::FuncId;
use execution_tape::verifier::{VerifyConfig, VerifyError, verify_program};

/// A one-function program whose body is `nop` followed by `tail`.
fn program_with(tail: &[u8]) -> Program {
    let mut bytecode = vec![Opcode::Nop.byte()];
    bytecode.extend_from_slice(tail);
    Program::new(
        vec![],
        vec![],
        vec![],
        TypeTableDef::default(),
        vec![FunctionDef {
            arg_types: vec![],
            ret_types: vec![],
            register_types: vec![ValueType::Unit],
            bytecode,
            spans: vec![],
        }],
    )
}

const EXTENDED: u8 = OpcodePrefix::Extended.byte();

/// A byte after the extended prefix that names no opcode.
const UNASSIGNED_EXTENDED: u8 = 0xFF;

#[test]
fn extended_prefix_is_reserved_lead_byte() {
    assert_eq!(EXTENDED, 0xFC);
    assert_eq!(
        OpcodePrefix::from_u8(EXTENDED),
        Some(OpcodePrefix::Extended)
    );
    assert_eq!(Opcode::from_byte(EXTENDED), None);
    assert_eq!(
        Opcode::from_prefixed(OpcodePrefix::Extended, UNASSIGNED_EXTENDED),
        None
    );
    for b in 0xF0..=0xFF {
        assert_eq!(Opcode::from_byte(b), None, "{b:#04X} is prefix space");
    }
    // Existing single-byte opcodes keep their bytes and codes.
    assert_eq!(Opcode::Call.code(), 0x50);
    assert_eq!(Opcode::Call.prefix(), None);
    assert_eq!(Opcode::Call.encoded_len(), 1);
    assert_eq!(Opcode::from_code(0x50), Some(Opcode::Call));
}

#[test]
fn unknown_opcodes_are_reported_with_code_and_pc() {
    for (tail, opcode, shown) in [
        // Unassigned single byte.
        (vec![0xEF], 0x00EF, "0xEF"),
        // Reserved prefix byte with no prefix assigned.
        (vec![0xFF, 0x00], 0x00FF, "0xFF"),
        // Assigned prefix followed by an unassigned byte.
        (
            vec![EXTENDED, UNASSIGNED_EXTENDED],
            u16::from_be_bytes([EXTENDED, UNASSIGNED_EXTENDED]),
            "0xFC 0xFF",
        ),
    ] {
        let program = program_with(&tail);
        let err = verify_program(&program, &VerifyConfig::default()).unwrap_err();
        assert_eq!(
            err,
            VerifyError::UnknownOpcode {
                func: 0,
                pc: 1,
                opcode
            }
        );
        assert_eq!(
            err.to_string(),
            format!("function 0 pc=1: unknown opcode {shown}")
        );

        assert!(matches!(
            disassemble_function(&program, FuncId(0)),
            Err(DisasmError::UnknownOpcode { pc: 1, opcode: o }) if o == opcode
        ));

        let err = decode_instruction(&tail).unwrap_err();
        assert_eq!(err, CodecError::UnknownOpcode { opcode });
        assert_eq!(err.to_string(), format!("unknown opcode {shown}"));
    }
}

#[test]
fn a_prefix_at_the_end_of_a_function_is_truncated() {
    let program = program_with(&[EXTENDED]);
    assert_eq!(
        verify_program(&program, &VerifyConfig::default()).unwrap_err(),
        VerifyError::BytecodeDecode { func: 0 }
    );
    assert!(matches!(
        disassemble_function(&program, FuncId(0)),
        Err(DisasmError::Decode(DecodeError::UnexpectedEof))
    ));
    assert_eq!(
        decode_instruction(&[EXTENDED]).unwrap_err(),
        CodecError::Format(DecodeError::UnexpectedEof)
    );
}
