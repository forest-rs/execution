// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The prefixed opcode space: two-byte opcodes behind a reserved lead byte.
//!
//! Prefixed opcodes round-trip through assembly, the wire format, the operand codec,
//! verification, disassembly, execution and tracing like single-byte ones. Unknown single bytes,
//! unknown bytes after an assigned prefix and a prefix cut off by the end of the function are all
//! rejected, by every decoder, with the offending code and location.

use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
use execution_tape::codec::{
    DecodeError as CodecError, Operand, canonicalize, decode_all, decode_instruction,
    encode_instruction,
};
use execution_tape::disasm::{DisasmError, disassemble_function};
use execution_tape::format::DecodeError;
use execution_tape::host::{Host, HostContext, HostError, SigHash, ValueRef};
use execution_tape::opcode::{Opcode, OpcodePrefix};
use execution_tape::program::{FunctionDef, Program, TypeTableDef, ValueType};
use execution_tape::trace::{TraceLocation, TraceMask, TraceSink};
use execution_tape::value::{FuncId, FuncRef, Value};
use execution_tape::verifier::{
    VerifiedProgram, VerifyConfig, VerifyError, verify_program, verify_program_owned,
};
use execution_tape::vm::{Limits, Vm};

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
            format!("function #0, pc 1: unknown opcode {shown}")
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

/// The first occupants of the extended space: `u64` bit counting and rotation.
const BIT_OPS: [(Opcode, u8, &str); 5] = [
    (Opcode::U64Clz, 0x00, "u64.clz"),
    (Opcode::U64Ctz, 0x01, "u64.ctz"),
    (Opcode::U64Popcnt, 0x02, "u64.popcnt"),
    (Opcode::U64Rotl, 0x03, "u64.rotl"),
    (Opcode::U64Rotr, 0x04, "u64.rotr"),
];

#[test]
fn bit_ops_occupy_the_extended_space() {
    for (opcode, byte, mnemonic) in BIT_OPS {
        assert_eq!(opcode.prefix(), Some(OpcodePrefix::Extended), "{mnemonic}");
        assert_eq!(opcode.byte(), byte, "{mnemonic}");
        assert_eq!(opcode.code(), u16::from_be_bytes([EXTENDED, byte]));
        assert_eq!(opcode.encoded_len(), 2);
        assert_eq!(Opcode::from_code(opcode.code()), Some(opcode));
        assert_eq!(
            Opcode::from_prefixed(OpcodePrefix::Extended, byte),
            Some(opcode)
        );
        assert_eq!(opcode.mnemonic(), mnemonic);
        let mut bytes = Vec::new();
        opcode.encode(&mut bytes);
        assert_eq!(bytes, [EXTENDED, byte]);
    }
}

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

/// Opcode codes reported to the instruction trace hook.
#[derive(Default)]
struct Codes(Vec<u16>);
impl TraceSink for Codes {
    fn instr(&mut self, _: &Program, _: TraceLocation, _: u32, _: Option<u64>, opcode: u16) {
        self.0.push(opcode);
    }
}

/// `f(a, b) -> (clz a, ctz a, popcnt a, rotl a b, rotr a b)`, round-tripped through the wire.
fn bit_program() -> VerifiedProgram {
    let mut builder = ProgramBuilder::new();
    let id = builder
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::U64, ValueType::U64],
            ret_types: vec![ValueType::U64; 5],
        })
        .unwrap();
    let mut body = builder.function_asm(id).unwrap();
    let rets: Vec<u32> = (0..5)
        .map(|_| body.alloc_register(ValueType::U64))
        .collect();
    body.u64_clz(rets[0], 1)
        .u64_ctz(rets[1], 1)
        .u64_popcnt(rets[2], 1)
        .u64_rotl(rets[3], 1, 2)
        .u64_rotr(rets[4], 1, 2)
        .ret(0, &rets);
    builder.define_function(id, body).unwrap();
    let bytes = builder.build().encode();
    verify_program_owned(Program::decode(&bytes).unwrap(), &VerifyConfig::default()).unwrap()
}

#[test]
fn prefixed_instructions_round_trip_through_every_decoder() {
    let program = bit_program();
    let source = program.program();
    let bytecode = source.functions[0].bytecode(source).unwrap();

    let decoded = decode_all(bytecode).unwrap();
    let opcodes: Vec<_> = decoded.iter().map(|d| d.opcode).collect();
    let mut expected: Vec<_> = BIT_OPS.iter().map(|(op, ..)| *op).collect();
    expected.push(Opcode::Ret);
    assert_eq!(opcodes, expected);
    // `u64.clz r3, r1`: prefix, byte, then two one-byte register operands.
    assert_eq!(decoded[0].byte_len, 4);
    assert_eq!(&bytecode[..4], [EXTENDED, 0x00, 3, 1]);
    assert_eq!(canonicalize(bytecode).unwrap(), bytecode);

    let mut encoded = Vec::new();
    encode_instruction(
        Opcode::U64Rotl,
        &[Operand::Reg(6), Operand::Reg(1), Operand::Reg(2)],
        &mut encoded,
    )
    .unwrap();
    assert_eq!(encoded, [EXTENDED, 0x03, 6, 1, 2]);
    assert_eq!(
        decode_instruction(&encoded).unwrap().opcode,
        Opcode::U64Rotl
    );

    let disassembly = disassemble_function(source, FuncId(0)).unwrap();
    let views: Vec<_> = disassembly.instrs().collect();
    let mut pc = 0;
    for (view, (opcode, _, mnemonic)) in views.iter().zip(BIT_OPS) {
        assert_eq!(view.opcode(), opcode);
        assert_eq!(view.pc(), pc, "{mnemonic}");
        assert!(view.to_string().contains(mnemonic), "{view}");
        pc += if view.reads().count() == 1 { 4 } else { 5 };
    }

    let pcs: Vec<_> = program
        .function(FuncId(0))
        .unwrap()
        .instructions()
        .map(|i| (i.pc, i.opcode))
        .collect();
    assert_eq!(
        pcs,
        [0, 4, 8, 12, 17, 22]
            .into_iter()
            .zip(expected.iter().copied())
            .collect::<Vec<_>>()
    );
}

#[test]
fn bit_ops_match_rust_and_trace_by_code() {
    let program = bit_program();
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(program).unwrap();
    let values = [
        0,
        1,
        2,
        0x8000_0000_0000_0000,
        u64::MAX,
        0x0123_4567_89AB_CDEF,
        0xF0F0_0000_0000_0F0F,
    ];
    // Rotation amounts are taken modulo 64.
    let amounts = [0, 1, 7, 63, 64, 65, 200, u64::MAX];
    for a in values {
        for b in amounts {
            let mut codes = Codes::default();
            let got = vm
                .run(
                    FuncRef::new(unit, FuncId(0)),
                    &[Value::U64(a), Value::U64(b)],
                    TraceMask::INSTR,
                    Some(&mut codes),
                )
                .unwrap();
            let sh = u32::try_from(b & 63).unwrap();
            assert_eq!(
                got,
                [
                    Value::U64(u64::from(a.leading_zeros())),
                    Value::U64(u64::from(a.trailing_zeros())),
                    Value::U64(u64::from(a.count_ones())),
                    Value::U64(a.rotate_left(sh)),
                    Value::U64(a.rotate_right(sh)),
                ],
                "a = {a:#x}, b = {b}"
            );
            let mut expected: Vec<u16> = BIT_OPS.iter().map(|(op, ..)| op.code()).collect();
            expected.push(Opcode::Ret.code());
            assert_eq!(codes.0, expected);
        }
    }
}

#[test]
fn verifier_requires_u64_operands_for_bit_ops() {
    for emit in [
        (|a: &mut Asm| {
            a.u64_popcnt(2, 1);
        }) as fn(&mut Asm),
        |a| {
            a.u64_rotl(2, 1, 1);
        },
    ] {
        let mut a = Asm::new();
        a.set_register_types(vec![ValueType::Unit, ValueType::I64, ValueType::U64]);
        emit(&mut a);
        a.ret(0, &[]);
        let mut pb = ProgramBuilder::new();
        pb.push_function_checked(
            a,
            FunctionSig {
                arg_types: vec![ValueType::I64],
                ret_types: vec![],
            },
        )
        .unwrap();
        let error = verify_program(&pb.build(), &VerifyConfig::default()).unwrap_err();
        assert!(
            matches!(error, VerifyError::RegisterTypeMismatch { pc: Some(0), .. }),
            "{error:?}"
        );
    }
}
