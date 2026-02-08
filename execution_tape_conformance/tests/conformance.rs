// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

#![allow(missing_docs, reason = "integration test crate")]

use execution_tape::aggregates::AggError;
use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
use execution_tape::host::{AccessSink, Host, HostError, HostSig, SigHash, ValueRef};
use execution_tape::opcode::Opcode;
use execution_tape::program::{
    Const, FunctionDef, HostSymbol, Program, StructTypeDef, TypeTableDef, ValueType,
};
use execution_tape::trace::TraceMask;
use execution_tape::value::Decimal;
use execution_tape::value::FuncId;
use execution_tape::value::Value;
use execution_tape::verifier::{VerifyConfig, VerifyError, verify_program, verify_program_owned};
use execution_tape::vm::{Limits, Trap, Vm};

struct TestHost;

impl Host for TestHost {
    fn call(
        &mut self,
        symbol: &str,
        _sig_hash: SigHash,
        args: &[ValueRef<'_>],
        _access: Option<&mut dyn AccessSink>,
    ) -> Result<(Vec<Value>, u64), HostError> {
        match symbol {
            "id" => Ok((args.iter().copied().map(ValueRef::to_value).collect(), 0)),
            _ => Err(HostError::UnknownSymbol),
        }
    }
}

fn verify_owned(program: Program) -> execution_tape::verifier::VerifiedProgram {
    verify_program_owned(program, &VerifyConfig::default()).unwrap()
}

#[test]
fn golden_minimal_program_bytes_v0_0_1() {
    let p = Program::new(
        vec![],
        vec![],
        vec![],
        TypeTableDef::default(),
        vec![FunctionDef {
            arg_types: vec![],
            ret_types: vec![],
            reg_count: 1,
            bytecode: vec![Opcode::Ret as u8, 0x00, 0x00], // ret r0
            spans: vec![],
        }],
    );

    // This test is intentionally strict: it locks in the container encoding for a minimal program
    // as a regression signal for format changes.
    let expected: &[u8] = &[
        // magic "EXTAPE\0\0"
        0x45,
        0x58,
        0x54,
        0x41,
        0x50,
        0x45,
        0x00,
        0x00,
        // version_major=0, version_minor=1
        0x00,
        0x00,
        0x01,
        0x00, // symbols: tag=1, len=1, payload=[0]
        0x01,
        0x01,
        0x00, // const_pool: tag=2, len=1, payload=[0]
        0x02,
        0x01,
        0x00, // types: tag=3, len=3, payload=[field_names=0, structs=0, array_elems=0]
        0x03,
        0x03,
        0x00,
        0x00,
        0x00,
        // bytecode_blobs: tag=5, len=5, payload=[n=1, len=3, ret r0]
        0x05,
        0x05,
        0x01,
        0x03,
        Opcode::Ret as u8,
        0x00,
        0x00,
        // span_tables: tag=6, len=2, payload=[n=1, span_count=0]
        0x06,
        0x02,
        0x01,
        0x00,
        // function_table: tag=4, len=6, payload=[n=1, argc=0, retc=0, regc=1, bc=0, sp=0]
        0x04,
        0x06,
        0x01,
        0x00,
        0x00,
        0x01,
        0x00,
        0x00,
        // function_sigs: tag=7, len=3, payload=[n=1, argc=0, retc=0]
        0x07,
        0x03,
        0x01,
        0x00,
        0x00, // host_sigs: tag=8, len=1, payload=[0]
        0x08,
        0x01,
        0x00,
    ];
    let bytes = p.encode();
    assert_eq!(bytes, expected);

    let back = Program::decode(&bytes).unwrap();
    assert_eq!(back, p);
    verify_program(&back, &VerifyConfig::default()).unwrap();
}

#[test]
fn roundtrip_verify_run_pure_ops() {
    let mut a = Asm::new();
    a.const_i64(1, 7);
    a.const_i64(2, 9);
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
    let p = pb.build_verified().unwrap();
    let bytes = p.program().encode();
    let back = Program::decode(&bytes).unwrap();
    let back = verify_owned(back);

    let mut vm = Vm::new(TestHost, Limits::default());
    let out = vm
        .run(&back, FuncId(0), &[], TraceMask::NONE, None)
        .unwrap();
    assert_eq!(out, vec![Value::I64(16)]);
}

#[test]
fn roundtrip_verify_run_cmp_cast_select() {
    // out = select((7 < 9), u64_to_i64(3_u64), 0_i64) + 5_i64
    let mut a = Asm::new();
    a.const_i64(1, 7);
    a.const_i64(2, 9);
    a.i64_lt(3, 1, 2);
    a.const_u64(4, 3);
    a.u64_to_i64(5, 4);
    a.const_i64(6, 0);
    a.select(7, 3, 5, 6);
    a.const_i64(8, 5);
    a.i64_add(9, 7, 8);
    a.ret(0, &[9]);

    let mut pb = ProgramBuilder::new();
    pb.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
            reg_count: 10,
        },
    )
    .unwrap();
    let p = pb.build_verified().unwrap();

    let bytes = p.program().encode();
    let back = Program::decode(&bytes).unwrap();
    let back = verify_owned(back);

    let mut vm = Vm::new(TestHost, Limits::default());
    let out = vm
        .run(&back, FuncId(0), &[], TraceMask::NONE, None)
        .unwrap();
    assert_eq!(out, vec![Value::I64(8)]);
}

#[test]
fn roundtrip_verify_run_u64_ops() {
    // (((5 + 7) - 5) * 7) = 49
    let mut a = Asm::new();
    a.const_u64(1, 5);
    a.const_u64(2, 7);
    a.u64_add(3, 1, 2);
    a.u64_sub(4, 3, 1);
    a.u64_mul(5, 4, 2);
    a.ret(0, &[5]);

    let mut pb = ProgramBuilder::new();
    pb.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::U64],
            reg_count: 6,
        },
    )
    .unwrap();
    let p = pb.build_verified().unwrap();
    let bytes = p.program().encode();
    let back = Program::decode(&bytes).unwrap();
    let back = verify_owned(back);

    let mut vm = Vm::new(TestHost, Limits::default());
    let out = vm
        .run(&back, FuncId(0), &[], TraceMask::NONE, None)
        .unwrap();
    assert_eq!(out, vec![Value::U64(49)]);
}

#[test]
fn roundtrip_verify_run_f64_ext_ops() {
    let mut a = Asm::new();
    a.const_f64(1, 1.25);
    a.f64_neg(2, 1);
    a.f64_abs(3, 2);

    a.const_f64(4, f64::NAN);
    a.const_f64(5, 3.0);
    a.f64_min(6, 4, 5);
    a.f64_min_num(7, 4, 5);
    a.f64_max_num(8, 4, 5);

    a.const_f64(9, 5.5);
    a.const_f64(10, 2.0);
    a.f64_rem(11, 9, 10);

    a.const_f64(12, -0.0_f64);
    a.const_f64(13, 0.0_f64);
    a.f64_min(14, 12, 13);
    a.f64_max(15, 12, 13);
    a.f64_to_bits(16, 14);
    a.f64_to_bits(17, 15);
    a.f64_from_bits(18, 16);

    a.ret(0, &[2, 3, 7, 8, 11, 6, 16, 17, 18]);

    let mut pb = ProgramBuilder::new();
    pb.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![
                ValueType::F64,
                ValueType::F64,
                ValueType::F64,
                ValueType::F64,
                ValueType::F64,
                ValueType::F64,
                ValueType::U64,
                ValueType::U64,
                ValueType::F64,
            ],
            reg_count: 19,
        },
    )
    .unwrap();
    let p = pb.build_verified().unwrap();

    let bytes = p.program().encode();
    let back = Program::decode(&bytes).unwrap();
    let back = verify_owned(back);

    let mut vm = Vm::new(TestHost, Limits::default());
    let out = vm
        .run(&back, FuncId(0), &[], TraceMask::NONE, None)
        .unwrap();

    assert_eq!(out.len(), 9);
    assert!(matches!(out[0], Value::F64(v) if v == -1.25));
    assert!(matches!(out[1], Value::F64(v) if v == 1.25));
    assert!(matches!(out[2], Value::F64(v) if v == 3.0));
    assert!(matches!(out[3], Value::F64(v) if v == 3.0));
    assert!(matches!(out[4], Value::F64(v) if v == 1.5));
    assert!(matches!(out[5], Value::F64(v) if v.is_nan()));
    assert!(matches!(out[6], Value::U64(v) if v == (-0.0_f64).to_bits()));
    assert!(matches!(out[7], Value::U64(v) if v == 0.0_f64.to_bits()));
    assert!(matches!(out[8], Value::F64(v) if v.to_bits() == (-0.0_f64).to_bits()));
}

#[test]
fn roundtrip_verify_run_u64_ops_wrap() {
    // u64 wraps on overflow.
    let mut a = Asm::new();
    a.const_u64(1, u64::MAX);
    a.const_u64(2, 1);
    a.u64_add(3, 1, 2);
    a.ret(0, &[3]);

    let mut pb = ProgramBuilder::new();
    pb.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::U64],
            reg_count: 4,
        },
    )
    .unwrap();
    let p = pb.build_verified().unwrap();
    let bytes = p.program().encode();
    let back = Program::decode(&bytes).unwrap();
    let back = verify_owned(back);

    let mut vm = Vm::new(TestHost, Limits::default());
    let out = vm
        .run(&back, FuncId(0), &[], TraceMask::NONE, None)
        .unwrap();
    assert_eq!(out, vec![Value::U64(0)]);
}

#[test]
fn roundtrip_verify_run_u64_bitwise_and_shifts() {
    // ((0b1010 & 0b1100) | 0b0001) ^ (1 << 3) = 0b0001
    let mut a = Asm::new();
    a.const_u64(1, 0b1010);
    a.const_u64(2, 0b1100);
    a.u64_and(3, 1, 2);
    a.const_u64(4, 0b0001);
    a.u64_or(5, 3, 4);
    a.const_u64(6, 1);
    a.const_u64(7, 3);
    a.u64_shl(8, 6, 7);
    a.u64_xor(9, 5, 8);
    a.u64_shr(10, 9, 7);
    a.u64_shl(11, 10, 7);
    a.ret(0, &[9, 11]);

    let mut pb = ProgramBuilder::new();
    pb.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::U64, ValueType::U64],
            reg_count: 12,
        },
    )
    .unwrap();
    let p = pb.build_verified().unwrap();
    let bytes = p.program().encode();
    let back = Program::decode(&bytes).unwrap();
    let back = verify_owned(back);

    let mut vm = Vm::new(TestHost, Limits::default());
    let out = vm
        .run(&back, FuncId(0), &[], TraceMask::NONE, None)
        .unwrap();
    assert_eq!(out, vec![Value::U64(0b0001), Value::U64(0b0000)]);
}

#[test]
fn roundtrip_verify_run_u64_ordering() {
    // Verify gt/le/ge all agree with expected ordering.
    let mut a = Asm::new();
    a.const_u64(1, 5);
    a.const_u64(2, 7);
    a.u64_gt(3, 2, 1);
    a.u64_le(4, 1, 2);
    a.u64_ge(5, 2, 2);
    a.ret(0, &[3, 4, 5]);

    let mut pb = ProgramBuilder::new();
    pb.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::Bool, ValueType::Bool, ValueType::Bool],
            reg_count: 6,
        },
    )
    .unwrap();
    let p = pb.build_verified().unwrap();
    let bytes = p.program().encode();
    let back = Program::decode(&bytes).unwrap();
    let back = verify_owned(back);

    let mut vm = Vm::new(TestHost, Limits::default());
    let out = vm
        .run(&back, FuncId(0), &[], TraceMask::NONE, None)
        .unwrap();
    assert_eq!(
        out,
        vec![Value::Bool(true), Value::Bool(true), Value::Bool(true)]
    );
}

#[test]
fn roundtrip_verify_run_bool_bitwise() {
    let mut a = Asm::new();
    a.const_bool(1, true);
    a.const_bool(2, false);
    a.bool_and(3, 1, 2);
    a.bool_or(4, 1, 2);
    a.bool_xor(5, 1, 2);
    a.ret(0, &[3, 4, 5]);

    let mut pb = ProgramBuilder::new();
    pb.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::Bool, ValueType::Bool, ValueType::Bool],
            reg_count: 6,
        },
    )
    .unwrap();
    let p = pb.build_verified().unwrap();
    let bytes = p.program().encode();
    let back = Program::decode(&bytes).unwrap();
    let back = verify_owned(back);

    let mut vm = Vm::new(TestHost, Limits::default());
    let out = vm
        .run(&back, FuncId(0), &[], TraceMask::NONE, None)
        .unwrap();
    assert_eq!(
        out,
        vec![Value::Bool(false), Value::Bool(true), Value::Bool(true)]
    );
}

#[test]
fn roundtrip_verify_run_i64_bitwise_and_shifts() {
    // (0b0110 & 0b1010) | 0b0001 = 0b0011
    // 0b0011 ^ (1 << 3) = 0b1011
    let mut a = Asm::new();
    a.const_i64(1, 0b0110);
    a.const_i64(2, 0b1010);
    a.i64_and(3, 1, 2);
    a.const_i64(4, 0b0001);
    a.i64_or(5, 3, 4);
    a.const_i64(6, 1);
    a.const_i64(7, 3);
    a.i64_shl(8, 6, 7);
    a.i64_xor(9, 5, 8);
    a.i64_shr(10, 9, 7);
    a.i64_shl(11, 10, 7);
    a.ret(0, &[9, 11]);

    let mut pb = ProgramBuilder::new();
    pb.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64, ValueType::I64],
            reg_count: 12,
        },
    )
    .unwrap();
    let p = pb.build_verified().unwrap();
    let bytes = p.program().encode();
    let back = Program::decode(&bytes).unwrap();
    let back = verify_owned(back);

    let mut vm = Vm::new(TestHost, Limits::default());
    let out = vm
        .run(&back, FuncId(0), &[], TraceMask::NONE, None)
        .unwrap();
    assert_eq!(out, vec![Value::I64(0b1011), Value::I64(0b1000)]);
}

#[test]
fn roundtrip_verify_run_i64_ordering() {
    // Verify gt/le/ge all agree with expected ordering.
    let mut a = Asm::new();
    a.const_i64(1, -5);
    a.const_i64(2, 7);
    a.i64_gt(3, 2, 1);
    a.i64_le(4, 1, 2);
    a.i64_ge(5, 1, 1);
    a.ret(0, &[3, 4, 5]);

    let mut pb = ProgramBuilder::new();
    pb.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::Bool, ValueType::Bool, ValueType::Bool],
            reg_count: 6,
        },
    )
    .unwrap();
    let p = pb.build_verified().unwrap();
    let bytes = p.program().encode();
    let back = Program::decode(&bytes).unwrap();
    let back = verify_owned(back);

    let mut vm = Vm::new(TestHost, Limits::default());
    let out = vm
        .run(&back, FuncId(0), &[], TraceMask::NONE, None)
        .unwrap();
    assert_eq!(
        out,
        vec![Value::Bool(true), Value::Bool(true), Value::Bool(true)]
    );
}

#[test]
fn roundtrip_verify_run_decimal_ops() {
    // (1.25 + 2.00 - 0.50) * 2 = 5.50
    let mut a = Asm::new();
    a.const_decimal(1, 125, 2);
    a.const_decimal(2, 200, 2);
    a.dec_add(3, 1, 2);
    a.const_decimal(4, 50, 2);
    a.dec_sub(5, 3, 4);
    a.const_decimal(6, 2, 0);
    a.dec_mul(7, 5, 6);
    a.ret(0, &[7]);

    let mut pb = ProgramBuilder::new();
    pb.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::Decimal],
            reg_count: 8,
        },
    )
    .unwrap();
    let p = pb.build_verified().unwrap();
    let bytes = p.program().encode();
    let back = Program::decode(&bytes).unwrap();
    let back = verify_owned(back);

    let mut vm = Vm::new(TestHost, Limits::default());
    let out = vm
        .run(&back, FuncId(0), &[], TraceMask::NONE, None)
        .unwrap();
    assert_eq!(
        out,
        vec![Value::Decimal(Decimal {
            mantissa: 550,
            scale: 2,
        })]
    );
}

#[test]
fn vm_traps_decimal_scale_mismatch() {
    let mut a = Asm::new();
    a.const_decimal(1, 1, 0);
    a.const_decimal(2, 1, 1);
    a.dec_add(3, 1, 2);
    a.ret(0, &[3]);

    let mut pb = ProgramBuilder::new();
    pb.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::Decimal],
            reg_count: 4,
        },
    )
    .unwrap();
    let p = pb.build_verified().unwrap();

    let mut vm = Vm::new(TestHost, Limits::default());
    let err = vm
        .run(&p, FuncId(0), &[], TraceMask::NONE, None)
        .unwrap_err();
    assert_eq!(err.trap, Trap::DecimalScaleMismatch);
}

#[test]
fn vm_traps_decimal_overflow_on_scale_add() {
    let mut a = Asm::new();
    a.const_decimal(1, 1, 250);
    a.const_decimal(2, 1, 10);
    a.dec_mul(3, 1, 2);
    a.ret(0, &[3]);

    let mut pb = ProgramBuilder::new();
    pb.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::Decimal],
            reg_count: 4,
        },
    )
    .unwrap();
    let p = pb.build_verified().unwrap();

    let mut vm = Vm::new(TestHost, Limits::default());
    let err = vm
        .run(&p, FuncId(0), &[], TraceMask::NONE, None)
        .unwrap_err();
    assert_eq!(err.trap, Trap::DecimalOverflow);
}

#[test]
fn roundtrip_verify_run_f64_ops() {
    // (1.5 + 2.25 - 0.5) * 2.0 = 6.5
    let mut a = Asm::new();
    a.const_f64(1, 1.5);
    a.const_f64(2, 2.25);
    a.f64_add(3, 1, 2);
    a.const_f64(4, 0.5);
    a.f64_sub(5, 3, 4);
    a.const_f64(6, 2.0);
    a.f64_mul(7, 5, 6);
    a.ret(0, &[7]);

    let mut pb = ProgramBuilder::new();
    pb.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::F64],
            reg_count: 8,
        },
    )
    .unwrap();
    let p = pb.build_verified().unwrap();
    let bytes = p.program().encode();
    let back = Program::decode(&bytes).unwrap();
    let back = verify_owned(back);

    let mut vm = Vm::new(TestHost, Limits::default());
    let out = vm
        .run(&back, FuncId(0), &[], TraceMask::NONE, None)
        .unwrap();
    assert_eq!(out, vec![Value::F64(6.5)]);
}

#[test]
fn vm_traps_on_cast_overflow() {
    let mut a = Asm::new();
    a.const_u64(1, u64::MAX);
    a.u64_to_i64(2, 1);
    a.ret(0, &[2]);

    let mut pb = ProgramBuilder::new();
    pb.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
            reg_count: 3,
        },
    )
    .unwrap();
    let p = pb.build_verified().unwrap();

    let mut vm = Vm::new(TestHost, Limits::default());
    let err = vm
        .run(&p, FuncId(0), &[], TraceMask::NONE, None)
        .unwrap_err();
    assert_eq!(err.trap, Trap::IntCastOverflow);
}

#[test]
fn roundtrip_verify_run_host_call() {
    let sig = HostSig {
        args: vec![ValueType::I64],
        rets: vec![ValueType::I64],
    };

    let mut pb = ProgramBuilder::new();
    let host_sig = pb.host_sig_for("id", sig);

    let mut a = Asm::new();
    a.const_i64(1, 9);
    a.host_call(0, host_sig, 0, &[1], &[2]);
    a.ret(0, &[2]);
    pb.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
            reg_count: 3,
        },
    )
    .unwrap();

    let p = pb.build_verified().unwrap();
    let bytes = p.program().encode();
    let back = Program::decode(&bytes).unwrap();
    let back = verify_owned(back);

    let mut vm = Vm::new(TestHost, Limits::default());
    let out = vm
        .run(&back, FuncId(0), &[], TraceMask::NONE, None)
        .unwrap();
    assert_eq!(out, vec![Value::I64(9)]);
}

#[test]
fn verifier_rejects_unknown_opcode() {
    let p = Program::new(
        vec![],
        vec![],
        vec![],
        TypeTableDef::default(),
        vec![FunctionDef {
            arg_types: vec![],
            ret_types: vec![],
            reg_count: 1,
            bytecode: vec![0xFF],
            spans: vec![],
        }],
    );
    let err = verify_program(&p, &VerifyConfig::default()).unwrap_err();
    assert!(matches!(err, VerifyError::BytecodeDecode { .. }));
}

#[test]
fn vm_traps_call_depth_on_unbounded_recursion() {
    // A function that calls itself unconditionally.
    let mut pb = ProgramBuilder::new();
    let f = pb.declare_function(FunctionSig {
        arg_types: vec![],
        ret_types: vec![],
        reg_count: 1,
    });

    let mut a = Asm::new();
    a.call(0, f, 0, &[], &[]);
    a.ret(0, &[]);
    pb.define_function(f, a).unwrap();
    let p = pb.build_verified().unwrap();

    let limits = Limits {
        fuel: 1_000_000,
        max_call_depth: 8,
        max_host_calls: 1_000_000,
    };
    let mut vm = Vm::new(TestHost, limits);
    let err = vm.run(&p, f, &[], TraceMask::NONE, None).unwrap_err();
    assert_eq!(err.trap, Trap::CallDepthExceeded);
}

#[test]
fn roundtrip_decode_preserves_symbols_and_consts_shape() {
    // This test exercises the various arenas (symbols, const blobs, type table packing) without
    // asserting exact bytes.
    let p = Program::new(
        vec![HostSymbol {
            symbol: "price.lookup".into(),
        }],
        vec![Const::Bytes(vec![1, 2, 3]), Const::Str("hi".into())],
        vec![],
        TypeTableDef::default(),
        vec![],
    );

    let bytes = p.encode();
    let back = Program::decode(&bytes).unwrap();
    assert_eq!(back, p);
}

#[test]
fn vm_loop_sum_0_to_n_minus_1() {
    // Sum i = 0..n-1 using a u64 loop counter.
    //
    // sum: i64, i: u64
    // while (i < n) { sum += u64_to_i64(i); i = i64_to_u64(u64_to_i64(i) + 1); }
    let mut a = Asm::new();
    let l_loop = a.label();
    let l_body = a.label();
    let l_done = a.label();

    a.const_i64(1, 0); // sum
    a.const_u64(2, 0); // i
    a.const_u64(3, 5); // n
    a.const_i64(4, 1); // one (i64)

    a.jmp(l_loop);
    a.place(l_loop).unwrap();
    a.u64_lt(5, 2, 3); // i < n
    a.br(5, l_body, l_done);

    a.place(l_body).unwrap();
    a.u64_to_i64(6, 2); // i_i64
    a.i64_add(1, 1, 6); // sum += i
    a.i64_add(7, 6, 4); // next_i_i64 = i + 1
    a.i64_to_u64(2, 7); // i = next_i
    a.jmp(l_loop);

    a.place(l_done).unwrap();
    a.ret(0, &[1]);

    let mut pb = ProgramBuilder::new();
    pb.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
            reg_count: 8,
        },
    )
    .unwrap();
    let p = pb.build_verified().unwrap();

    let mut vm = Vm::new(TestHost, Limits::default());
    let out = vm.run(&p, FuncId(0), &[], TraceMask::NONE, None).unwrap();
    assert_eq!(out, vec![Value::I64(10)]);
}

#[test]
fn vm_traps_fuel_in_tight_loop() {
    let mut a = Asm::new();
    let l0 = a.label();
    a.place(l0).unwrap();
    a.jmp(l0);

    let mut pb = ProgramBuilder::new();
    pb.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![],
            reg_count: 1,
        },
    )
    .unwrap();
    let p = pb.build_verified().unwrap();

    let limits = Limits {
        fuel: 3,
        ..Limits::default()
    };
    let mut vm = Vm::new(TestHost, limits);
    let err = vm
        .run(&p, FuncId(0), &[], TraceMask::NONE, None)
        .unwrap_err();
    assert_eq!(err.trap, Trap::FuelExceeded);
}

#[test]
fn vm_traps_host_call_limit_in_loop() {
    struct CountingHost {
        calls: u64,
    }

    impl Host for CountingHost {
        fn call(
            &mut self,
            _symbol: &str,
            _sig_hash: SigHash,
            _args: &[ValueRef<'_>],
            _access: Option<&mut dyn AccessSink>,
        ) -> Result<(Vec<Value>, u64), HostError> {
            self.calls += 1;
            Ok((Vec::new(), 0))
        }
    }

    let sig = HostSig {
        args: vec![],
        rets: vec![],
    };

    let mut pb = ProgramBuilder::new();
    let host_sig = pb.host_sig_for("noop", sig);

    let mut a = Asm::new();
    let l0 = a.label();
    a.place(l0).unwrap();
    a.host_call(0, host_sig, 0, &[], &[]);
    a.jmp(l0);
    pb.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![],
            reg_count: 1,
        },
    )
    .unwrap();
    let p = pb.build_verified().unwrap();

    let limits = Limits {
        fuel: 100,
        max_host_calls: 2,
        ..Limits::default()
    };
    let mut vm = Vm::new(CountingHost { calls: 0 }, limits);
    let err = vm
        .run(&p, FuncId(0), &[], TraceMask::NONE, None)
        .unwrap_err();
    assert_eq!(err.trap, Trap::HostCallLimitExceeded);
}

#[test]
fn verifier_rejects_select_type_mismatch() {
    // cond is bool, but a/b are different concrete types (i64 vs u64).
    let mut a = Asm::new();
    a.const_bool(1, true);
    a.const_i64(2, 1);
    a.const_u64(3, 1);
    a.select(4, 1, 2, 3);
    a.ret(0, &[4]);

    let mut pb = ProgramBuilder::new();
    pb.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::U64],
            reg_count: 5,
        },
    )
    .unwrap();
    let p = pb.build();

    let err = verify_program(&p, &VerifyConfig::default()).unwrap_err();
    assert!(matches!(err, VerifyError::TypeMismatch { .. }));
}

#[test]
fn vm_traps_on_negative_i64_to_u64() {
    let mut a = Asm::new();
    a.const_i64(1, -1);
    a.i64_to_u64(2, 1);
    a.ret(0, &[2]);

    let mut pb = ProgramBuilder::new();
    pb.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::U64],
            reg_count: 3,
        },
    )
    .unwrap();
    let p = pb.build_verified().unwrap();

    let mut vm = Vm::new(TestHost, Limits::default());
    let err = vm
        .run(&p, FuncId(0), &[], TraceMask::NONE, None)
        .unwrap_err();
    assert_eq!(err.trap, Trap::IntCastOverflow);
}

#[test]
fn roundtrip_verify_run_tuple_len() {
    let mut a = Asm::new();
    a.const_i64(1, 10);
    a.const_i64(2, 20);
    a.tuple_new(3, &[1, 2]);
    a.tuple_len(4, 3);
    a.tuple_get(5, 3, 1);
    a.ret(0, &[4, 5]);

    let mut pb = ProgramBuilder::new();
    pb.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::U64, ValueType::I64],
            reg_count: 6,
        },
    )
    .unwrap();
    let p = pb.build_verified().unwrap();

    let mut vm = Vm::new(TestHost, Limits::default());
    let out = vm.run(&p, FuncId(0), &[], TraceMask::NONE, None).unwrap();
    assert_eq!(out, vec![Value::U64(2), Value::I64(20)]);
}

#[test]
fn roundtrip_verify_run_struct_field_count() {
    let mut pb = ProgramBuilder::new();
    let type_id = pb.struct_type(StructTypeDef {
        field_names: vec!["a".into(), "b".into(), "c".into()],
        field_types: vec![ValueType::I64, ValueType::U64, ValueType::Bool],
    });

    let mut a = Asm::new();
    a.const_i64(1, 1);
    a.const_u64(2, 2);
    a.const_bool(3, true);
    a.struct_new(4, type_id, &[1, 2, 3]);
    a.struct_field_count(5, 4);
    a.struct_get(6, 4, 1);
    a.ret(0, &[5, 6]);

    pb.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::U64, ValueType::U64],
            reg_count: 7,
        },
    )
    .unwrap();
    let p = pb.build_verified().unwrap();

    let mut vm = Vm::new(TestHost, Limits::default());
    let out = vm.run(&p, FuncId(0), &[], TraceMask::NONE, None).unwrap();
    assert_eq!(out, vec![Value::U64(3), Value::U64(2)]);
}

#[test]
fn roundtrip_verify_run_array_get_imm() {
    let mut pb = ProgramBuilder::new();
    let elem = pb.array_elem(ValueType::I64);

    let mut a = Asm::new();
    a.const_i64(1, 7);
    a.const_i64(2, 8);
    a.const_i64(3, 9);
    a.array_new(4, elem, &[1, 2, 3]);
    a.array_get_imm(5, 4, 2);
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
    let p = pb.build_verified().unwrap();

    let mut vm = Vm::new(TestHost, Limits::default());
    let out = vm.run(&p, FuncId(0), &[], TraceMask::NONE, None).unwrap();
    assert_eq!(out, vec![Value::I64(9)]);
}

#[test]
fn vm_traps_array_get_imm_oob() {
    let mut pb = ProgramBuilder::new();
    let elem = pb.array_elem(ValueType::U64);

    let mut a = Asm::new();
    a.const_u64(1, 1);
    a.array_new(2, elem, &[1]);
    a.array_get_imm(3, 2, 9);
    a.ret(0, &[3]);

    pb.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::U64],
            reg_count: 4,
        },
    )
    .unwrap();
    let p = pb.build_verified().unwrap();

    let mut vm = Vm::new(TestHost, Limits::default());
    let err = vm
        .run(&p, FuncId(0), &[], TraceMask::NONE, None)
        .unwrap_err();
    assert_eq!(err.trap, Trap::AggError(AggError::OutOfBounds));
}

#[test]
fn roundtrip_verify_run_bytes_len_and_str_len() {
    let mut pb = ProgramBuilder::new();
    let bytes = pb.constant(Const::Bytes(vec![1, 2, 3, 4]));
    let s = pb.constant(Const::Str("aé".into())); // 3 bytes in UTF-8

    let mut a = Asm::new();
    a.const_pool(1, bytes);
    a.const_pool(2, s);
    a.bytes_len(3, 1);
    a.str_len(4, 2);
    a.ret(0, &[3, 4]);

    pb.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::U64, ValueType::U64],
            reg_count: 5,
        },
    )
    .unwrap();
    let p = pb.build_verified().unwrap();

    let mut vm = Vm::new(TestHost, Limits::default());
    let out = vm.run(&p, FuncId(0), &[], TraceMask::NONE, None).unwrap();
    assert_eq!(out, vec![Value::U64(4), Value::U64(3)]);
}

#[test]
fn roundtrip_verify_run_div_rem() {
    // (25 / 4, 25 % 4, 25_u64 / 4_u64, 25_u64 % 4_u64) = (6, 1, 6, 1)
    let mut a = Asm::new();
    a.const_i64(1, 25);
    a.const_i64(2, 4);
    a.i64_div(3, 1, 2);
    a.i64_rem(4, 1, 2);
    a.const_u64(5, 25);
    a.const_u64(6, 4);
    a.u64_div(7, 5, 6);
    a.u64_rem(8, 5, 6);
    a.ret(0, &[3, 4, 7, 8]);

    let mut pb = ProgramBuilder::new();
    pb.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![
                ValueType::I64,
                ValueType::I64,
                ValueType::U64,
                ValueType::U64,
            ],
            reg_count: 9,
        },
    )
    .unwrap();
    let p = pb.build_verified().unwrap();

    let mut vm = Vm::new(TestHost, Limits::default());
    let out = vm.run(&p, FuncId(0), &[], TraceMask::NONE, None).unwrap();
    assert_eq!(
        out,
        vec![Value::I64(6), Value::I64(1), Value::U64(6), Value::U64(1)]
    );
}

#[test]
fn vm_traps_div_by_zero() {
    let mut a = Asm::new();
    a.const_u64(1, 1);
    a.const_u64(2, 0);
    a.u64_div(3, 1, 2);
    a.ret(0, &[3]);

    let mut pb = ProgramBuilder::new();
    pb.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::U64],
            reg_count: 4,
        },
    )
    .unwrap();
    let p = pb.build_verified().unwrap();

    let mut vm = Vm::new(TestHost, Limits::default());
    let err = vm
        .run(&p, FuncId(0), &[], TraceMask::NONE, None)
        .unwrap_err();
    assert_eq!(err.trap, Trap::DivByZero);
}

#[test]
fn vm_traps_i64_div_overflow_min_over_minus_one() {
    let mut a = Asm::new();
    a.const_i64(1, i64::MIN);
    a.const_i64(2, -1);
    a.i64_div(3, 1, 2);
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
    let p = pb.build_verified().unwrap();

    let mut vm = Vm::new(TestHost, Limits::default());
    let err = vm
        .run(&p, FuncId(0), &[], TraceMask::NONE, None)
        .unwrap_err();
    assert_eq!(err.trap, Trap::IntDivOverflow);
}

#[test]
fn roundtrip_verify_run_int_float_conversions() {
    let mut a = Asm::new();
    a.const_i64(1, -3);
    a.i64_to_f64(2, 1);
    a.const_u64(3, 7);
    a.u64_to_f64(4, 3);
    a.const_f64(5, 3.9);
    a.f64_to_i64(6, 5);
    a.const_f64(7, 3.9);
    a.f64_to_u64(8, 7);
    a.ret(0, &[2, 4, 6, 8]);

    let mut pb = ProgramBuilder::new();
    pb.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![
                ValueType::F64,
                ValueType::F64,
                ValueType::I64,
                ValueType::U64,
            ],
            reg_count: 9,
        },
    )
    .unwrap();
    let p = pb.build_verified().unwrap();

    let mut vm = Vm::new(TestHost, Limits::default());
    let out = vm.run(&p, FuncId(0), &[], TraceMask::NONE, None).unwrap();

    match (&out[0], &out[1]) {
        (Value::F64(a), Value::F64(b)) => {
            assert_eq!(*a, -3.0);
            assert_eq!(*b, 7.0);
        }
        _ => panic!("unexpected return types: {out:?}"),
    }
    assert_eq!(out[2], Value::I64(3));
    assert_eq!(out[3], Value::U64(3));
}

#[test]
fn roundtrip_verify_run_f64_div_and_comparisons() {
    let mut a = Asm::new();
    a.const_f64(1, 9.0);
    a.const_f64(2, 2.0);
    a.f64_div(3, 1, 2); // 4.5
    a.f64_eq(4, 1, 2);
    a.f64_lt(5, 2, 1);
    a.f64_gt(6, 1, 2);
    a.f64_le(7, 2, 2);
    a.f64_ge(8, 1, 2);
    a.ret(0, &[3, 4, 5, 6, 7, 8]);

    let mut pb = ProgramBuilder::new();
    pb.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![
                ValueType::F64,
                ValueType::Bool,
                ValueType::Bool,
                ValueType::Bool,
                ValueType::Bool,
                ValueType::Bool,
            ],
            reg_count: 9,
        },
    )
    .unwrap();
    let p = pb.build_verified().unwrap();

    let mut vm = Vm::new(TestHost, Limits::default());
    let out = vm.run(&p, FuncId(0), &[], TraceMask::NONE, None).unwrap();
    assert_eq!(out[0], Value::F64(4.5));
    assert_eq!(out[1], Value::Bool(false));
    assert_eq!(out[2], Value::Bool(true));
    assert_eq!(out[3], Value::Bool(true));
    assert_eq!(out[4], Value::Bool(true));
    assert_eq!(out[5], Value::Bool(true));
}

#[test]
fn roundtrip_verify_run_f64_comparisons_nan_are_false() {
    let mut a = Asm::new();
    a.const_f64(1, f64::NAN);
    a.const_f64(2, 1.0);
    a.f64_eq(3, 1, 1);
    a.f64_lt(4, 1, 2);
    a.f64_gt(5, 1, 2);
    a.f64_le(6, 1, 2);
    a.f64_ge(7, 1, 2);
    a.ret(0, &[3, 4, 5, 6, 7]);

    let mut pb = ProgramBuilder::new();
    pb.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![
                ValueType::Bool,
                ValueType::Bool,
                ValueType::Bool,
                ValueType::Bool,
                ValueType::Bool,
            ],
            reg_count: 8,
        },
    )
    .unwrap();
    let p = pb.build_verified().unwrap();

    let mut vm = Vm::new(TestHost, Limits::default());
    let out = vm.run(&p, FuncId(0), &[], TraceMask::NONE, None).unwrap();
    assert_eq!(
        out,
        vec![
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(false),
            Value::Bool(false),
        ]
    );
}

#[test]
fn vm_traps_f64_to_int_on_nan() {
    let mut a = Asm::new();
    a.const_f64(1, f64::NAN);
    a.f64_to_i64(2, 1);
    a.ret(0, &[2]);

    let mut pb = ProgramBuilder::new();
    pb.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
            reg_count: 3,
        },
    )
    .unwrap();
    let p = pb.build_verified().unwrap();

    let mut vm = Vm::new(TestHost, Limits::default());
    let err = vm
        .run(&p, FuncId(0), &[], TraceMask::NONE, None)
        .unwrap_err();
    assert_eq!(err.trap, Trap::FloatToIntInvalid);
}

#[test]
fn vm_traps_f64_to_u64_on_negative() {
    let mut a = Asm::new();
    a.const_f64(1, -1.0);
    a.f64_to_u64(2, 1);
    a.ret(0, &[2]);

    let mut pb = ProgramBuilder::new();
    pb.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::U64],
            reg_count: 3,
        },
    )
    .unwrap();
    let p = pb.build_verified().unwrap();

    let mut vm = Vm::new(TestHost, Limits::default());
    let err = vm
        .run(&p, FuncId(0), &[], TraceMask::NONE, None)
        .unwrap_err();
    assert_eq!(err.trap, Trap::IntCastOverflow);
}

#[test]
fn roundtrip_verify_run_decimal_conversions_scale_0_only() {
    let mut a = Asm::new();
    a.const_decimal(1, 42, 0);
    a.dec_to_i64(2, 1);
    a.i64_to_dec(3, 2, 2); // 42 -> 42.00 (mantissa=4200, scale=2)
    a.ret(0, &[2, 3]);

    let mut pb = ProgramBuilder::new();
    pb.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64, ValueType::Decimal],
            reg_count: 4,
        },
    )
    .unwrap();
    let p = pb.build_verified().unwrap();

    let mut vm = Vm::new(TestHost, Limits::default());
    let out = vm.run(&p, FuncId(0), &[], TraceMask::NONE, None).unwrap();
    assert_eq!(out[0], Value::I64(42));
    assert_eq!(
        out[1],
        Value::Decimal(Decimal {
            mantissa: 4200,
            scale: 2
        })
    );
}

#[test]
fn vm_traps_dec_to_i64_on_nonzero_scale() {
    let mut a = Asm::new();
    a.const_decimal(1, 1, 2);
    a.dec_to_i64(2, 1);
    a.ret(0, &[2]);

    let mut pb = ProgramBuilder::new();
    pb.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
            reg_count: 3,
        },
    )
    .unwrap();
    let p = pb.build_verified().unwrap();

    let mut vm = Vm::new(TestHost, Limits::default());
    let err = vm
        .run(&p, FuncId(0), &[], TraceMask::NONE, None)
        .unwrap_err();
    assert_eq!(err.trap, Trap::DecimalScaleMismatch);
}

#[test]
fn roundtrip_verify_run_bytes_and_string_ops() {
    let mut pb = ProgramBuilder::new();
    let a_bytes = pb.constant(Const::Bytes(vec![1, 2, 3]));
    let b_bytes = pb.constant(Const::Bytes(vec![4, 5]));
    let a_str = pb.constant(Const::Str("hi".into()));
    let b_str = pb.constant(Const::Str("é".into()));

    let mut a = Asm::new();
    a.const_pool(1, a_bytes);
    a.const_pool(2, b_bytes);
    a.bytes_concat(3, 1, 2); // [1,2,3,4,5]
    a.bytes_get_imm(4, 3, 3); // 4
    a.const_u64(5, 1);
    a.const_u64(6, 4);
    a.bytes_slice(7, 3, 5, 6); // [2,3,4]
    a.bytes_eq(8, 7, 7);

    a.const_pool(9, a_str);
    a.const_pool(10, b_str);
    a.str_concat(11, 9, 10); // "hié"
    a.str_len(12, 11); // "hi"=2 bytes, "é"=2 bytes => 4
    a.str_eq(13, 11, 11);

    a.str_to_bytes(14, 11);
    a.bytes_to_str(15, 14);
    a.ret(0, &[4, 7, 8, 12, 13, 15]);

    pb.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![
                ValueType::U64,
                ValueType::Bytes,
                ValueType::Bool,
                ValueType::U64,
                ValueType::Bool,
                ValueType::Str,
            ],
            reg_count: 16,
        },
    )
    .unwrap();
    let p = pb.build_verified().unwrap();

    let mut vm = Vm::new(TestHost, Limits::default());
    let out = vm.run(&p, FuncId(0), &[], TraceMask::NONE, None).unwrap();
    assert_eq!(out[0], Value::U64(4));
    assert_eq!(out[1], Value::Bytes(vec![2, 3, 4]));
    assert_eq!(out[2], Value::Bool(true));
    assert_eq!(out[3], Value::U64(4));
    assert_eq!(out[4], Value::Bool(true));
    assert_eq!(out[5], Value::Str("hié".into()));
}

#[test]
fn vm_traps_str_slice_on_non_boundary() {
    let mut pb = ProgramBuilder::new();
    let s = pb.constant(Const::Str("é".into())); // 2 bytes

    let mut a = Asm::new();
    a.const_pool(1, s);
    a.const_u64(2, 1);
    a.const_u64(3, 2);
    a.str_slice(4, 1, 2, 3);
    a.ret(0, &[4]);

    pb.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::Str],
            reg_count: 5,
        },
    )
    .unwrap();
    let p = pb.build_verified().unwrap();

    let mut vm = Vm::new(TestHost, Limits::default());
    let err = vm
        .run(&p, FuncId(0), &[], TraceMask::NONE, None)
        .unwrap_err();
    assert_eq!(err.trap, Trap::StrNotCharBoundary);
}

#[test]
fn vm_traps_bytes_to_str_on_invalid_utf8() {
    let mut pb = ProgramBuilder::new();
    let b = pb.constant(Const::Bytes(vec![0xFF]));

    let mut a = Asm::new();
    a.const_pool(1, b);
    a.bytes_to_str(2, 1);
    a.ret(0, &[2]);

    pb.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::Str],
            reg_count: 3,
        },
    )
    .unwrap();
    let p = pb.build_verified().unwrap();

    let mut vm = Vm::new(TestHost, Limits::default());
    let err = vm
        .run(&p, FuncId(0), &[], TraceMask::NONE, None)
        .unwrap_err();
    assert_eq!(err.trap, Trap::InvalidUtf8);
}
