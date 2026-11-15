// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Checked conversions (`*.to_*_ovf`, `*.to_*_exact`) through the public API.
//!
//! `numerics_32_ops.rs` checks every opcode against a reference on edge-case inputs. This file
//! pins the edges a frontend relies on to literal results, the branch-on-flag idiom, the
//! encoding and disassembly, and the verifier's typing.

use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder, Reg};
use execution_tape::host::{Host, HostContext, HostError, SigHash, ValueRef};
use execution_tape::opcode::{Opcode, OpcodePrefix};
use execution_tape::program::{Program, ValueType};
use execution_tape::trace::TraceMask;
use execution_tape::value::{FuncId, FuncRef, Value};
use execution_tape::verifier::{
    VerifiedProgram, VerifyConfig, VerifyError, verify_program, verify_program_owned,
};
use execution_tape::vm::{Limits, Trap, Vm};

use ValueType::{Bool, F32, F64, I32, I64, U32, U64};

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

/// One checked conversion: its emitter and its source and result types.
type Emit = fn(&mut Asm, Reg, Reg, Reg) -> &mut Asm;

/// Builds `f(x) -> (value, flag)` for each conversion, in order, roundtripped through the wire
/// format.
fn program(input: ValueType, conversions: &[(Emit, ValueType)]) -> VerifiedProgram {
    let mut builder = ProgramBuilder::new();
    for &(emit, output) in conversions {
        let id = builder
            .declare_function(FunctionSig {
                arg_types: vec![input],
                ret_types: vec![output, Bool],
            })
            .unwrap();
        let mut body = builder.function_asm(id).unwrap();
        let value = body.alloc_register(output);
        let flag = body.alloc_register(Bool);
        emit(&mut body, value, flag, Reg(1));
        body.ret(Reg::EFFECT, &[value, flag]);
        builder.define_function(id, body).unwrap();
    }
    let bytes = builder.build().encode();
    verify_program_owned(Program::decode(&bytes).unwrap(), &VerifyConfig::default()).unwrap()
}

/// Runs every function of `program` on `x`, returning each `(value, flag)`.
fn run_all(program: &VerifiedProgram, count: usize, x: Value) -> Vec<(Value, bool)> {
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(program.clone()).unwrap();
    (0..count)
        .map(|f| {
            let out = vm
                .run(
                    FuncRef::new(unit, FuncId(u32::try_from(f).unwrap())),
                    std::slice::from_ref(&x),
                    TraceMask::NONE,
                    None,
                )
                .unwrap();
            let Value::Bool(flag) = out[1] else {
                panic!("{out:?}")
            };
            (out[0].clone(), flag)
        })
        .collect()
}

#[test]
fn integer_narrowing_reports_values_that_do_not_fit() {
    let from_i64: [(Emit, ValueType); 3] = [
        (Asm::i64_to_i32_ovf, I32),
        (Asm::i64_to_u32_ovf, U32),
        (Asm::i64_to_u64_ovf, U64),
    ];
    let program = program(I64, &from_i64);
    // x => (i32, i32 flag, u32, u32 flag, u64, u64 flag)
    let cases: [(i64, Value, bool, Value, bool, Value, bool); 9] = [
        (
            0,
            Value::I32(0),
            false,
            Value::U32(0),
            false,
            Value::U64(0),
            false,
        ),
        (
            -1,
            Value::I32(-1),
            false,
            Value::U32(u32::MAX),
            true,
            Value::U64(u64::MAX),
            true,
        ),
        (
            i64::from(i32::MAX),
            Value::I32(i32::MAX),
            false,
            Value::U32(0x7FFF_FFFF),
            false,
            Value::U64(0x7FFF_FFFF),
            false,
        ),
        (
            i64::from(i32::MAX) + 1,
            Value::I32(i32::MIN),
            true,
            Value::U32(1 << 31),
            false,
            Value::U64(1 << 31),
            false,
        ),
        (
            i64::from(i32::MIN),
            Value::I32(i32::MIN),
            false,
            Value::U32(1 << 31),
            true,
            Value::U64(0xFFFF_FFFF_8000_0000),
            true,
        ),
        (
            i64::from(u32::MAX),
            Value::I32(-1),
            true,
            Value::U32(u32::MAX),
            false,
            Value::U64(0xFFFF_FFFF),
            false,
        ),
        (
            1 << 32,
            Value::I32(0),
            true,
            Value::U32(0),
            true,
            Value::U64(1 << 32),
            false,
        ),
        (
            i64::MIN,
            Value::I32(0),
            true,
            Value::U32(0),
            true,
            Value::U64(1 << 63),
            true,
        ),
        (
            i64::MAX,
            Value::I32(-1),
            true,
            Value::U32(u32::MAX),
            true,
            Value::U64(0x7FFF_FFFF_FFFF_FFFF),
            false,
        ),
    ];
    for (x, i32_value, i32_flag, u32_value, u32_flag, u64_value, u64_flag) in cases {
        assert_eq!(
            run_all(&program, 3, Value::I64(x)),
            [
                (i32_value, i32_flag),
                (u32_value, u32_flag),
                (u64_value, u64_flag)
            ],
            "x = {x}"
        );
    }

    let program = program_u64();
    for (x, u32_result, i64_result) in [
        (0, (Value::U32(0), false), (Value::I64(0), false)),
        (
            u64::from(u32::MAX),
            (Value::U32(u32::MAX), false),
            (Value::I64(0xFFFF_FFFF), false),
        ),
        (1 << 32, (Value::U32(0), true), (Value::I64(1 << 32), false)),
        (
            0x7FFF_FFFF_FFFF_FFFF,
            (Value::U32(u32::MAX), true),
            (Value::I64(i64::MAX), false),
        ),
        (1 << 63, (Value::U32(0), true), (Value::I64(i64::MIN), true)),
        (
            u64::MAX,
            (Value::U32(u32::MAX), true),
            (Value::I64(-1), true),
        ),
    ] {
        assert_eq!(
            run_all(&program, 2, Value::U64(x)),
            [u32_result, i64_result],
            "x = {x}"
        );
    }
}

fn program_u64() -> VerifiedProgram {
    program(
        U64,
        &[(Asm::u64_to_u32_ovf, U32), (Asm::u64_to_i64_ovf, I64)],
    )
}

#[test]
fn sign_changes_report_values_that_do_not_fit() {
    let signed = program(I32, &[(Asm::i32_to_u32_ovf, U32)]);
    for (x, want) in [
        (0, (Value::U32(0), false)),
        (i32::MAX, (Value::U32(0x7FFF_FFFF), false)),
        (-1, (Value::U32(u32::MAX), true)),
        (i32::MIN, (Value::U32(1 << 31), true)),
    ] {
        assert_eq!(run_all(&signed, 1, Value::I32(x)), [want], "x = {x}");
    }
    let unsigned = program(U32, &[(Asm::u32_to_i32_ovf, I32)]);
    for (x, want) in [
        (0, (Value::I32(0), false)),
        (0x7FFF_FFFF, (Value::I32(i32::MAX), false)),
        (1 << 31, (Value::I32(i32::MIN), true)),
        (u32::MAX, (Value::I32(-1), true)),
    ] {
        assert_eq!(run_all(&unsigned, 1, Value::U32(x)), [want], "x = {x}");
    }
}

#[test]
fn float_conversions_report_every_float_that_is_not_an_integer_in_range() {
    let to_ints: [(Emit, ValueType); 4] = [
        (Asm::f64_to_i64_exact, I64),
        (Asm::f64_to_u64_exact, U64),
        (Asm::f64_to_i32_exact, I32),
        (Asm::f64_to_u32_exact, U32),
    ];
    let program = program(F64, &to_ints);
    let two63 = 9_223_372_036_854_775_808.0_f64;
    let two64 = 18_446_744_073_709_551_616.0_f64;
    let below = |x: f64| f64::from_bits(x.to_bits() - 1);
    let beyond_negative = |x: f64| f64::from_bits(x.to_bits() + 1);
    // x => [(i64, inexact), (u64, inexact), (i32, inexact), (u32, inexact)]; values are the
    // saturating truncation.
    let cases: Vec<(f64, [(Value, bool); 4])> = vec![
        (
            0.0,
            [
                (Value::I64(0), false),
                (Value::U64(0), false),
                (Value::I32(0), false),
                (Value::U32(0), false),
            ],
        ),
        // -0.0 is the integer zero, for unsigned targets too.
        (
            -0.0,
            [
                (Value::I64(0), false),
                (Value::U64(0), false),
                (Value::I32(0), false),
                (Value::U32(0), false),
            ],
        ),
        (
            -3.0,
            [
                (Value::I64(-3), false),
                (Value::U64(0), true),
                (Value::I32(-3), false),
                (Value::U32(0), true),
            ],
        ),
        // A fraction is inexact even when its truncation fits; `-0.5` truncates to zero.
        (
            2.5,
            [
                (Value::I64(2), true),
                (Value::U64(2), true),
                (Value::I32(2), true),
                (Value::U32(2), true),
            ],
        ),
        (
            -0.5,
            [
                (Value::I64(0), true),
                (Value::U64(0), true),
                (Value::I32(0), true),
                (Value::U32(0), true),
            ],
        ),
        (
            f64::from_bits(1),
            [
                (Value::I64(0), true),
                (Value::U64(0), true),
                (Value::I32(0), true),
                (Value::U32(0), true),
            ],
        ),
        (
            f64::NAN,
            [
                (Value::I64(0), true),
                (Value::U64(0), true),
                (Value::I32(0), true),
                (Value::U32(0), true),
            ],
        ),
        (
            f64::INFINITY,
            [
                (Value::I64(i64::MAX), true),
                (Value::U64(u64::MAX), true),
                (Value::I32(i32::MAX), true),
                (Value::U32(u32::MAX), true),
            ],
        ),
        (
            f64::NEG_INFINITY,
            [
                (Value::I64(i64::MIN), true),
                (Value::U64(0), true),
                (Value::I32(i32::MIN), true),
                (Value::U32(0), true),
            ],
        ),
        // -2^63 is `i64::MIN` exactly; 2^63 saturates to `i64::MAX`, which is not it.
        (
            -two63,
            [
                (Value::I64(i64::MIN), false),
                (Value::U64(0), true),
                (Value::I32(i32::MIN), true),
                (Value::U32(0), true),
            ],
        ),
        (
            beyond_negative(-two63),
            [
                (Value::I64(i64::MIN), true),
                (Value::U64(0), true),
                (Value::I32(i32::MIN), true),
                (Value::U32(0), true),
            ],
        ),
        (
            two63,
            [
                (Value::I64(i64::MAX), true),
                (Value::U64(1 << 63), false),
                (Value::I32(i32::MAX), true),
                (Value::U32(u32::MAX), true),
            ],
        ),
        (
            below(two63),
            [
                (Value::I64(0x7FFF_FFFF_FFFF_FC00), false),
                (Value::U64(0x7FFF_FFFF_FFFF_FC00), false),
                (Value::I32(i32::MAX), true),
                (Value::U32(u32::MAX), true),
            ],
        ),
        (
            two64,
            [
                (Value::I64(i64::MAX), true),
                (Value::U64(u64::MAX), true),
                (Value::I32(i32::MAX), true),
                (Value::U32(u32::MAX), true),
            ],
        ),
        (
            below(two64),
            [
                (Value::I64(i64::MAX), true),
                (Value::U64(0xFFFF_FFFF_FFFF_F800), false),
                (Value::I32(i32::MAX), true),
                (Value::U32(u32::MAX), true),
            ],
        ),
        // The 32-bit edges: 2^31 and 2^32 are just outside, -2^31 and 2^32 - 1 just inside.
        (
            2_147_483_648.0,
            [
                (Value::I64(1 << 31), false),
                (Value::U64(1 << 31), false),
                (Value::I32(i32::MAX), true),
                (Value::U32(1 << 31), false),
            ],
        ),
        (
            -2_147_483_648.0,
            [
                (Value::I64(-(1 << 31)), false),
                (Value::U64(0), true),
                (Value::I32(i32::MIN), false),
                (Value::U32(0), true),
            ],
        ),
        (
            4_294_967_295.0,
            [
                (Value::I64(0xFFFF_FFFF), false),
                (Value::U64(0xFFFF_FFFF), false),
                (Value::I32(i32::MAX), true),
                (Value::U32(u32::MAX), false),
            ],
        ),
        (
            4_294_967_296.0,
            [
                (Value::I64(1 << 32), false),
                (Value::U64(1 << 32), false),
                (Value::I32(i32::MAX), true),
                (Value::U32(u32::MAX), true),
            ],
        ),
    ];
    for (x, want) in cases {
        assert_eq!(run_all(&program, 4, Value::F64(x)), want, "x = {x:e}");
    }

    let single = program_f32();
    for (x, want) in [
        (-0.0_f32, [(Value::I32(0), false), (Value::U32(0), false)]),
        (1.5, [(Value::I32(1), true), (Value::U32(1), true)]),
        (-7.0, [(Value::I32(-7), false), (Value::U32(0), true)]),
        (
            -2_147_483_648.0,
            [(Value::I32(i32::MIN), false), (Value::U32(0), true)],
        ),
        (
            2_147_483_648.0,
            [(Value::I32(i32::MAX), true), (Value::U32(1 << 31), false)],
        ),
        (
            4_294_967_296.0,
            [(Value::I32(i32::MAX), true), (Value::U32(u32::MAX), true)],
        ),
        (f32::NAN, [(Value::I32(0), true), (Value::U32(0), true)]),
        (
            f32::NEG_INFINITY,
            [(Value::I32(i32::MIN), true), (Value::U32(0), true)],
        ),
    ] {
        assert_eq!(run_all(&single, 2, Value::F32(x)), want, "x = {x:e}");
    }
}

fn program_f32() -> VerifiedProgram {
    program(
        F32,
        &[(Asm::f32_to_i32_exact, I32), (Asm::f32_to_u32_exact, U32)],
    )
}

/// The idiom the ops exist for: convert, then branch on the flag to the language's failure
/// path instead of trapping. `narrow(x)` returns `x` as `i32`, or `-1` when it does not fit.
#[test]
fn a_branch_on_the_flag_replaces_the_trap() {
    let mut builder = ProgramBuilder::new();
    let id = builder
        .declare_function(FunctionSig {
            arg_types: vec![I64],
            ret_types: vec![I32],
        })
        .unwrap();
    let mut a = builder.function_asm(id).unwrap();
    let value = a.alloc_register(I32);
    let overflow = a.alloc_register(Bool);
    let fallback = a.alloc_register(I32);
    let (fail, done) = (a.label(), a.label());
    a.i64_to_i32_ovf(value, overflow, Reg(1))
        .br(overflow, fail, done);
    a.place(fail).unwrap();
    a.const_i32(fallback, -1).ret(Reg::EFFECT, &[fallback]);
    a.place(done).unwrap();
    a.ret(Reg::EFFECT, &[value]);
    builder.define_function(id, a).unwrap();
    let checked = builder.build_verified().unwrap();

    let mut builder = ProgramBuilder::new();
    let id = builder
        .declare_function(FunctionSig {
            arg_types: vec![I64],
            ret_types: vec![I32],
        })
        .unwrap();
    let mut a = builder.function_asm(id).unwrap();
    let value = a.alloc_register(I32);
    a.i64_to_i32(value, Reg(1)).ret(Reg::EFFECT, &[value]);
    builder.define_function(id, a).unwrap();
    let trapping = builder.build_verified().unwrap();

    let mut vm = Vm::new(NoHost, Limits::default());
    let checked = vm.install(checked).unwrap();
    let trapping = vm.install(trapping).unwrap();
    for x in [0, 7, -7, i64::from(i32::MIN), i64::from(i32::MAX)] {
        let entry = FuncRef::new(checked, id);
        let got = vm.run(entry, &[Value::I64(x)], TraceMask::NONE, None);
        let want = i32::try_from(x).unwrap();
        assert_eq!(got.unwrap(), [Value::I32(want)], "x = {x}");
    }
    for x in [i64::from(i32::MAX) + 1, i64::MIN, 1 << 40] {
        let got = vm.run(
            FuncRef::new(checked, id),
            &[Value::I64(x)],
            TraceMask::NONE,
            None,
        );
        assert_eq!(got.unwrap(), [Value::I32(-1)], "x = {x}");
        let trap = vm
            .run(
                FuncRef::new(trapping, id),
                &[Value::I64(x)],
                TraceMask::NONE,
                None,
            )
            .unwrap_err();
        assert!(matches!(trap.trap, Trap::IntCastOverflow), "x = {x}");
    }
}

#[test]
fn checked_conversion_codes_and_disassembly_are_stable() {
    let table = [
        (Opcode::I64ToI32Ovf, 0xE0, "i64.to_i32_ovf"),
        (Opcode::I64ToU32Ovf, 0xE1, "i64.to_u32_ovf"),
        (Opcode::U64ToU32Ovf, 0xE2, "u64.to_u32_ovf"),
        (Opcode::I32ToU32Ovf, 0xE3, "i32.to_u32_ovf"),
        (Opcode::U32ToI32Ovf, 0xE4, "u32.to_i32_ovf"),
        (Opcode::I64ToU64Ovf, 0xE5, "i64.to_u64_ovf"),
        (Opcode::U64ToI64Ovf, 0xE6, "u64.to_i64_ovf"),
        (Opcode::F32ToI32Exact, 0xE7, "f32.to_i32_exact"),
        (Opcode::F32ToU32Exact, 0xE8, "f32.to_u32_exact"),
        (Opcode::F64ToI32Exact, 0xE9, "f64.to_i32_exact"),
        (Opcode::F64ToU32Exact, 0xEA, "f64.to_u32_exact"),
        (Opcode::F64ToI64Exact, 0xEB, "f64.to_i64_exact"),
        (Opcode::F64ToU64Exact, 0xEC, "f64.to_u64_exact"),
    ];
    for (opcode, byte, mnemonic) in table {
        assert_eq!(
            Opcode::from_prefixed(OpcodePrefix::Numeric32, byte),
            Some(opcode),
            "{mnemonic}"
        );
        assert_eq!(opcode.code(), 0xFD00 | u16::from(byte), "{mnemonic}");
        assert_eq!(opcode.mnemonic(), mnemonic);
    }

    let program = program_u64();
    let disassembly =
        execution_tape::disasm::disassemble_function(program.program(), FuncId(1)).unwrap();
    let first = disassembly.instrs().next().unwrap();
    assert!(
        first.to_string().ends_with("u64.to_i64_ovf r2, r3, [r1]"),
        "{first}"
    );
    assert_eq!(first.dst(), Some(Reg(2)));
    assert_eq!(first.writes().collect::<Vec<_>>(), [Reg(2), Reg(3)]);

    let program = program_f32();
    let disassembly =
        execution_tape::disasm::disassemble_function(program.program(), FuncId(0)).unwrap();
    let first = disassembly.instrs().next().unwrap();
    assert!(
        first.to_string().ends_with("f32.to_i32_exact r2, r3, [r1]"),
        "{first}"
    );
}

#[test]
fn verifier_requires_a_bool_flag_and_matching_types() {
    // (result, flag, operand) register kinds for `f64.to_i64_exact` and `i64.to_i32_ovf`.
    let cases: [(Emit, [ValueType; 3]); 6] = [
        (Asm::f64_to_i64_exact, [I64, I64, F64]),
        (Asm::f64_to_i64_exact, [U64, Bool, F64]),
        (Asm::f64_to_i64_exact, [I64, Bool, F32]),
        (Asm::i64_to_i32_ovf, [I32, I32, I64]),
        (Asm::i64_to_i32_ovf, [I64, Bool, I64]),
        (Asm::i64_to_i32_ovf, [I32, Bool, U64]),
    ];
    for (emit, [result, flag, operand]) in cases {
        let mut a = Asm::new();
        a.set_register_types(vec![ValueType::Unit, operand, result, flag]);
        emit(&mut a, Reg(2), Reg(3), Reg(1));
        a.ret(Reg::EFFECT, &[]);
        let mut pb = ProgramBuilder::new();
        pb.push_function_checked(
            a,
            FunctionSig {
                arg_types: vec![operand],
                ret_types: vec![],
            },
        )
        .unwrap();
        let error = verify_program(&pb.build(), &VerifyConfig::default()).unwrap_err();
        assert!(
            matches!(error, VerifyError::RegisterTypeMismatch { .. }),
            "{result:?}/{flag:?}/{operand:?}: {error:?}"
        );
    }
}
