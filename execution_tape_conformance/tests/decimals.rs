// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Decimal range, representation, codec, and ABI boundaries through the public API.

use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
use execution_tape::codec::{self, DecodedOperand, Operand};
use execution_tape::disasm::{ConstValue, InputIndex};
use execution_tape::format::DecodeError;
use execution_tape::host::{Host, HostContext, HostError, SigHash, ValueRef};
use execution_tape::opcode::Opcode;
use execution_tape::program::{ByteRange, Const, ConstEntry, ConstId, Program, ValueType};
use execution_tape::trace::TraceMask;
use execution_tape::value::FuncRef;
use execution_tape::value::{Decimal, DecimalRounding, FuncId, Value};
use execution_tape::verifier::{
    MetadataTable, VerifiedProgram, VerifyConfig, VerifyError, verify_program, verify_program_owned,
};
use execution_tape::vm::{Limits, Trap, TrapInfo, Vm};

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

fn verified(builder: ProgramBuilder) -> VerifiedProgram {
    let bytes = builder.build().encode();
    verify_program_owned(Program::decode(&bytes).unwrap(), &VerifyConfig::default()).unwrap()
}

fn function(
    args: &[ValueType],
    rets: &[ValueType],
    emit: impl FnOnce(&mut Asm, &mut ProgramBuilder),
) -> VerifiedProgram {
    let mut builder = ProgramBuilder::new();
    let id = builder
        .declare_function(FunctionSig {
            arg_types: args.into(),
            ret_types: rets.into(),
        })
        .unwrap();
    let mut body = builder.function_asm(id).unwrap();
    emit(&mut body, &mut builder);
    builder.define_function(id, body).unwrap();
    verified(builder)
}

fn run(program: &VerifiedProgram, args: &[Value]) -> Result<Vec<Value>, TrapInfo> {
    {
        let mut vm = Vm::new(NoHost, Limits::default());
        let unit = vm.install(program.clone()).unwrap();
        vm.run(FuncRef::new(unit, FuncId(0)), args, TraceMask::NONE, None)
    }
}

#[test]
fn wide_literals_roundtrip_inline_pool_and_disassembly() {
    for mantissa in [
        i128::MIN,
        i128::MIN + 1,
        -1,
        0,
        1,
        i128::from(u64::MAX),
        i128::MAX,
    ] {
        for scale in [0, 12, 38, u8::MAX] {
            let decimal = Decimal::new(mantissa, scale);
            assert_eq!((decimal.mantissa(), decimal.scale()), (mantissa, scale));
            let program = function(
                &[],
                &[ValueType::Decimal, ValueType::Decimal],
                |body, builder| {
                    let inline = body.alloc_register(ValueType::Decimal);
                    let pooled = body.alloc_register(ValueType::Decimal);
                    let id = builder.constant(Const::Decimal(decimal));
                    body.const_decimal_value(inline, decimal)
                        .const_pool(pooled, id)
                        .ret(0, &[inline, pooled]);
                },
            );
            assert_eq!(
                program.program().const_decimal(ConstId(0)).unwrap(),
                decimal
            );
            assert_eq!(program.program().const_bytes_data.len(), 17);
            assert_eq!(
                run(&program, &[]).unwrap(),
                [Value::Decimal(decimal), Value::Decimal(decimal)]
            );
            let disassembly =
                execution_tape::disasm::disassemble_function(program.program(), FuncId(0)).unwrap();
            let literals: Vec<_> = disassembly
                .instrs()
                .filter_map(|instruction| instruction.const_value())
                .collect();
            assert_eq!(
                literals,
                [ConstValue::Decimal(decimal), ConstValue::Decimal(decimal)]
            );
        }
    }
}

#[test]
fn wide_operand_codec_preserves_both_parts_and_canonical_bytes() {
    for mantissa in [i128::MIN, i128::from(i64::MAX) + 1, i128::MAX] {
        let value = Decimal::new(mantissa, 255);
        let mut bytes = Vec::new();
        codec::encode_instruction(
            Opcode::ConstDecimal,
            &[Operand::Reg(1), Operand::Decimal(value)],
            &mut bytes,
        )
        .unwrap();
        let decoded = codec::decode_instruction(&bytes).unwrap();
        assert_eq!(
            decoded.operands,
            [DecodedOperand::Reg(1), DecodedOperand::Decimal(value)]
        );
        assert_eq!(codec::canonicalize(&bytes).unwrap(), bytes);
    }
}

#[test]
fn decimal_arithmetic_matches_checked_i128_at_boundaries() {
    for operation in 0..3 {
        let program = function(
            &[ValueType::Decimal, ValueType::Decimal],
            &[ValueType::Decimal],
            |body, _| {
                let result = body.alloc_register(ValueType::Decimal);
                match operation {
                    0 => body.dec_add(result, 1, 2),
                    1 => body.dec_sub(result, 1, 2),
                    _ => body.dec_mul(result, 1, 2),
                };
                body.ret(0, &[result]);
            },
        );
        let values = [
            i128::MIN,
            i128::MIN + 1,
            -(1_i128 << 80),
            -1,
            0,
            1,
            1_i128 << 80,
            i128::MAX,
        ];
        for a in values {
            for b in values {
                let expected = match operation {
                    0 => a.checked_add(b),
                    1 => a.checked_sub(b),
                    _ => a.checked_mul(b),
                };
                let result = run(
                    &program,
                    &[
                        Value::Decimal(Decimal::new(a, 12)),
                        Value::Decimal(Decimal::new(b, 12)),
                    ],
                );
                match expected {
                    Some(value) => assert_eq!(
                        result.unwrap(),
                        [Value::Decimal(Decimal::new(
                            value,
                            if operation == 2 { 24 } else { 12 }
                        ))]
                    ),
                    None => assert!(matches!(result.unwrap_err().trap, Trap::DecimalOverflow)),
                }
            }
        }
    }
}

fn dec(mantissa: i128, scale: u8) -> Value {
    Value::Decimal(Decimal::new(mantissa, scale))
}

/// `f(a, b) -> (a + b, a - b)`.
fn add_sub_program() -> VerifiedProgram {
    function(
        &[ValueType::Decimal, ValueType::Decimal],
        &[ValueType::Decimal, ValueType::Decimal],
        |body, _| {
            let sum = body.alloc_register(ValueType::Decimal);
            let difference = body.alloc_register(ValueType::Decimal);
            body.dec_add(sum, 1, 2)
                .dec_sub(difference, 1, 2)
                .ret(0, &[sum, difference]);
        },
    )
}

#[test]
fn addition_aligns_differing_scales_exactly() {
    let program = add_sub_program();
    // 19.99 + 1.6491 = 21.6391; 19.99 - 1.6491 = 18.3409 (scale 4, no rounding).
    assert_eq!(
        run(&program, &[dec(1999, 2), dec(16491, 4)]).unwrap(),
        [dec(216_391, 4), dec(183_409, 4)]
    );
    assert_eq!(
        run(&program, &[dec(-5, 0), dec(25, 1)]).unwrap(),
        [dec(-25, 1), dec(-75, 1)]
    );
    // Equal scales are unchanged, and zero keeps the larger scale.
    assert_eq!(
        run(&program, &[dec(0, 0), dec(0, 255)]).unwrap(),
        [dec(0, 255), dec(0, 255)]
    );
    // Aligning `i128::MAX` to a larger scale cannot be represented.
    let error = run(&program, &[dec(i128::MAX, 0), dec(0, 1)]).unwrap_err();
    assert!(matches!(error.trap, Trap::DecimalOverflow));
}

/// `f(a, b, scale) -> (cmp(a, b), a.scale, rescale(a, scale), a / b at scale)` for one mode.
fn decimal_ops_program(rounding: DecimalRounding) -> VerifiedProgram {
    let program = function(
        &[ValueType::Decimal, ValueType::Decimal, ValueType::U64],
        &[
            ValueType::I64,
            ValueType::U64,
            ValueType::Decimal,
            ValueType::Decimal,
        ],
        |body, _| {
            let order = body.alloc_register(ValueType::I64);
            let scale = body.alloc_register(ValueType::U64);
            let rescaled = body.alloc_register(ValueType::Decimal);
            let quotient = body.alloc_register(ValueType::Decimal);
            body.dec_cmp(order, 1, 2)
                .dec_scale(scale, 1)
                .dec_rescale(rescaled, 1, 3, rounding)
                .dec_div(quotient, 1, 2, 3, rounding)
                .ret(0, &[order, scale, rescaled, quotient]);
        },
    );
    // Roundtrip through the wire format.
    let bytes = program.program().encode();
    verify_program_owned(Program::decode(&bytes).unwrap(), &VerifyConfig::default()).unwrap()
}

#[test]
fn comparison_and_scale_ignore_and_report_representation() {
    let program = function(
        &[ValueType::Decimal, ValueType::Decimal],
        &[ValueType::I64, ValueType::U64],
        |body, _| {
            let order = body.alloc_register(ValueType::I64);
            let scale = body.alloc_register(ValueType::U64);
            body.dec_cmp(order, 1, 2)
                .dec_scale(scale, 1)
                .ret(0, &[order, scale]);
        },
    );
    for (a, b, order) in [
        ((10, 1), (100, 2), 0),
        ((0, 0), (0, 255), 0),
        ((-5, 1), (1, 3), -1),
        ((1, 0), (i128::MAX, 38), -1),
        ((2, 0), (i128::MAX, 38), 1),
        ((i128::MIN, 0), (i128::MIN, 1), -1),
    ] {
        let out = run(&program, &[dec(a.0, a.1), dec(b.0, b.1)]).unwrap();
        assert_eq!(
            out,
            [Value::I64(order), Value::U64(u64::from(a.1))],
            "{a:?} cmp {b:?}"
        );
    }
}

/// The canonical rounding table: each value (in tenths) rounded to an integer by each mode,
/// through both `dec.rescale` and `dec.div`.
#[test]
fn rescale_and_division_round_by_mode() {
    let tenths = [55, 25, 16, 11, 10, -10, -11, -16, -25, -55];
    let table: [(DecimalRounding, [i128; 10]); 5] = [
        (
            DecimalRounding::HalfEven,
            [6, 2, 2, 1, 1, -1, -1, -2, -2, -6],
        ),
        (
            DecimalRounding::HalfAwayFromZero,
            [6, 3, 2, 1, 1, -1, -1, -2, -3, -6],
        ),
        (
            DecimalRounding::TowardZero,
            [5, 2, 1, 1, 1, -1, -1, -1, -2, -5],
        ),
        (DecimalRounding::Floor, [5, 2, 1, 1, 1, -1, -2, -2, -3, -6]),
        (
            DecimalRounding::Ceiling,
            [6, 3, 2, 2, 1, -1, -1, -1, -2, -5],
        ),
    ];
    for (mode, expected) in table {
        let program = decimal_ops_program(mode);
        for (value, want) in tenths.into_iter().zip(expected) {
            let out = run(&program, &[dec(value, 1), dec(1, 0), Value::U64(0)]).unwrap();
            assert_eq!(out[2], dec(want, 0), "rescale {value}/10 {mode:?}");
            assert_eq!(out[3], dec(want, 0), "{value}/10 / 1 {mode:?}");
        }
    }
}

#[test]
fn division_produces_the_requested_scale() {
    let program = decimal_ops_program(DecimalRounding::HalfEven);
    let quotient = |a: Value, b: Value, scale: u64| {
        run(&program, &[a, b, Value::U64(scale)]).map(|out| out[3].clone())
    };
    // 10 / 3 = 3.3333 at scale 4; 2 / 3 = 0.67 at scale 2.
    assert_eq!(quotient(dec(10, 0), dec(3, 0), 4).unwrap(), dec(33_333, 4));
    assert_eq!(quotient(dec(2, 0), dec(3, 0), 2).unwrap(), dec(67, 2));
    // A price split three ways: 100.00 / 3 = 33.33, rounded half-even.
    assert_eq!(
        quotient(dec(10_000, 2), dec(3, 0), 2).unwrap(),
        dec(3333, 2)
    );
    // Operand scales do not constrain the result scale: 1.5 / 0.25 = 6.
    assert_eq!(quotient(dec(15, 1), dec(25, 2), 0).unwrap(), dec(6, 0));
    // Exact tie at the last kept digit: 0.125 / 1 at scale 2 = 0.12 (half-even).
    assert_eq!(quotient(dec(125, 3), dec(1, 0), 2).unwrap(), dec(12, 2));
    assert_eq!(
        quotient(dec(i128::MIN, 0), dec(1, 0), 0).unwrap(),
        dec(i128::MIN, 0)
    );
}

#[test]
fn decimal_rescale_and_division_trap_on_zero_divisor_scale_and_overflow() {
    let program = decimal_ops_program(DecimalRounding::Floor);
    let trap = |a: Value, b: Value, scale: u64| {
        run(&program, &[a, b, Value::U64(scale)]).unwrap_err().trap
    };
    assert!(matches!(trap(dec(1, 0), dec(0, 5), 0), Trap::DivByZero));
    // A scale register beyond `u8` is out of range for both operations.
    assert!(matches!(
        trap(dec(1, 0), dec(1, 0), 256),
        Trap::DecimalOverflow
    ));
    assert!(matches!(
        trap(dec(0, 0), dec(1, 0), u64::MAX),
        Trap::DecimalOverflow
    ));
    // Rescaling up overflows before division runs.
    assert!(matches!(
        trap(dec(i128::MAX, 0), dec(1, 0), 1),
        Trap::DecimalOverflow
    ));
    // The quotient `2^127` fits only when negative.
    assert!(matches!(
        trap(dec(i128::MIN, 0), dec(-1, 0), 0),
        Trap::DecimalOverflow
    ));
    let quotient = |a: Value, b: Value, scale: u64| {
        run(&program, &[a, b, Value::U64(scale)]).unwrap()[3].clone()
    };
    assert_eq!(
        quotient(dec(i128::MIN, 0), dec(10, 1), 0),
        dec(i128::MIN, 0)
    );
}

#[test]
fn decimal_rounding_bytes_names_and_disassembly_are_stable() {
    for (opcode, byte, mnemonic) in [
        (Opcode::DecCmp, 0x9D, "dec.cmp"),
        (Opcode::DecScale, 0x9E, "dec.scale"),
        (Opcode::DecRescale, 0x9F, "dec.rescale"),
        (Opcode::DecDiv, 0xA0, "dec.div"),
    ] {
        assert_eq!(opcode.byte(), byte, "{mnemonic} byte");
        assert_eq!(Opcode::from_byte(byte), Some(opcode), "{mnemonic} decode");
        assert_eq!(opcode.mnemonic(), mnemonic);
    }
    for (index, mode) in DecimalRounding::ALL.into_iter().enumerate() {
        assert_eq!(usize::from(mode.byte()), index);
        assert_eq!(DecimalRounding::from_byte(mode.byte()), Some(mode));
    }
    assert_eq!(DecimalRounding::from_byte(5), None);
    let names: Vec<_> = DecimalRounding::ALL.iter().map(|m| m.name()).collect();
    assert_eq!(
        names,
        [
            "half_even",
            "half_away_from_zero",
            "toward_zero",
            "floor",
            "ceiling"
        ]
    );

    let program = decimal_ops_program(DecimalRounding::HalfAwayFromZero);
    let disassembly =
        execution_tape::disasm::disassemble_function(program.program(), FuncId(0)).unwrap();
    let lines: Vec<_> = disassembly
        .instrs()
        .map(|instruction| {
            (
                instruction.to_string(),
                instruction.input_index(),
                instruction.reads().count(),
            )
        })
        .collect();
    assert!(lines[0].0.ends_with("dec.cmp r4, [r1, r2]"), "{lines:?}");
    assert!(lines[1].0.ends_with("dec.scale r5, [r1]"), "{lines:?}");
    assert!(
        lines[2]
            .0
            .ends_with("dec.rescale r6, [r1, r3] ; rounding=half_away_from_zero"),
        "{lines:?}"
    );
    assert!(
        lines[3]
            .0
            .ends_with("dec.div r7, [r1, r2, r3] ; rounding=half_away_from_zero"),
        "{lines:?}"
    );
    let rounding = Some(InputIndex::Rounding(
        DecimalRounding::HalfAwayFromZero.byte(),
    ));
    assert_eq!((lines[2].1, lines[2].2), (rounding, 2));
    assert_eq!((lines[3].1, lines[3].2), (rounding, 3));
}

#[test]
fn verifier_rejects_unknown_rounding_modes_and_mistyped_scales() {
    let build = |scale_type: ValueType| {
        let mut builder = ProgramBuilder::new();
        let id = builder
            .declare_function(FunctionSig {
                arg_types: vec![ValueType::Decimal, scale_type],
                ret_types: vec![ValueType::Decimal],
            })
            .unwrap();
        let mut body = builder.function_asm(id).unwrap();
        let out = body.alloc_register(ValueType::Decimal);
        body.dec_rescale(out, 1, 2, DecimalRounding::Ceiling)
            .ret(0, &[out]);
        builder.define_function(id, body).unwrap();
        builder.build()
    };
    let mut program = build(ValueType::U64);
    // `dec.rescale r3, r1, r2, mode` is the first instruction; its last byte is the mode.
    let at = program.functions[0].bytecode.offset as usize;
    assert_eq!(program.bytecode_data[at], Opcode::DecRescale.byte());
    assert_eq!(
        program.bytecode_data[at + 4],
        DecimalRounding::Ceiling.byte()
    );
    program.bytecode_data[at + 4] = 5;
    let error = verify_program(&program, &VerifyConfig::default()).unwrap_err();
    assert_eq!(
        error,
        VerifyError::InvalidDecimalRounding {
            func: 0,
            pc: 0,
            rounding: 5
        }
    );
    assert!(
        error
            .to_string()
            .contains("invalid decimal rounding mode 5")
    );
    let text = execution_tape::disasm::disassemble(&program).to_string();
    assert!(text.contains("rounding=<invalid 5>"), "{text}");

    let error = verify_program(&build(ValueType::I64), &VerifyConfig::default()).unwrap_err();
    assert!(
        matches!(
            error,
            VerifyError::RegisterTypeMismatch {
                required: ValueType::U64,
                ..
            }
        ),
        "{error:?}"
    );
}

#[test]
fn decimal_integer_narrowing_is_checked_and_requires_zero_scale() {
    for unsigned in [false, true] {
        let kind = if unsigned {
            ValueType::U64
        } else {
            ValueType::I64
        };
        let program = function(&[ValueType::Decimal], &[kind], |body, _| {
            let result = body.alloc_register(kind);
            if unsigned {
                body.dec_to_u64(result, 1);
            } else {
                body.dec_to_i64(result, 1);
            }
            body.ret(0, &[result]);
        });
        for mantissa in [
            i128::MIN,
            i128::from(i64::MIN),
            -1,
            0,
            i128::from(i64::MAX),
            i128::from(u64::MAX),
            i128::from(u64::MAX) + 1,
            i128::MAX,
        ] {
            let expected = if unsigned {
                u64::try_from(mantissa).ok().map(Value::U64)
            } else {
                i64::try_from(mantissa).ok().map(Value::I64)
            };
            let actual = run(&program, &[Value::Decimal(Decimal::new(mantissa, 0))]);
            match expected {
                Some(value) => assert_eq!(actual.unwrap(), [value]),
                None => assert!(matches!(actual.unwrap_err().trap, Trap::IntCastOverflow)),
            }
            assert!(matches!(
                run(&program, &[Value::Decimal(Decimal::new(mantissa, 1))])
                    .unwrap_err()
                    .trap,
                Trap::DecimalScaleMismatch
            ));
        }
    }
}

#[test]
fn integer_to_decimal_supports_full_unsigned_range_and_zero_at_any_scale() {
    for unsigned in [false, true] {
        for scale in [0, 12, 38, 39, 255] {
            let kind = if unsigned {
                ValueType::U64
            } else {
                ValueType::I64
            };
            let program = function(&[kind], &[ValueType::Decimal], |body, _| {
                let result = body.alloc_register(ValueType::Decimal);
                if unsigned {
                    body.u64_to_dec(result, 1, scale);
                } else {
                    body.i64_to_dec(result, 1, scale);
                }
                body.ret(0, &[result]);
            });
            for integer in [
                0,
                1,
                -1,
                i128::from(i64::MIN),
                i128::from(i64::MAX),
                i128::from(u64::MAX),
            ] {
                let value = if unsigned {
                    u64::try_from(integer).ok().map(Value::U64)
                } else {
                    i64::try_from(integer).ok().map(Value::I64)
                };
                let Some(value) = value else {
                    continue;
                };
                let expected = (0..scale).try_fold(integer, |value, _| value.checked_mul(10));
                let actual = run(&program, &[value]);
                match expected {
                    Some(value) => assert_eq!(
                        actual.unwrap(),
                        [Value::Decimal(Decimal::new(value, scale))]
                    ),
                    None => assert!(matches!(actual.unwrap_err().trap, Trap::DecimalOverflow)),
                }
            }
        }
    }
}

#[test]
fn borrowed_decimal_views_preserve_identity_without_normalizing_scale() {
    let value = Value::Decimal(Decimal::new(i128::MIN, 12));
    let Value::Decimal(owned) = &value else {
        unreachable!()
    };
    let ValueRef::Decimal(borrowed) = ValueRef::from_value(&value) else {
        unreachable!()
    };
    assert!(core::ptr::eq(owned, borrowed));
    assert_eq!(ValueRef::from_value(&value).to_value(), value);
    assert_ne!(Decimal::new(10, 1), Decimal::new(100, 2));
    assert_ne!(Decimal::new(0, 0), Decimal::new(0, 255));
}

#[test]
fn malformed_unused_decimal_ranges_are_rejected_before_preparation() {
    for range in [
        ByteRange { offset: 0, len: 16 },
        ByteRange { offset: 0, len: 18 },
        ByteRange { offset: 1, len: 17 },
        ByteRange {
            offset: u32::MAX,
            len: 17,
        },
    ] {
        let mut builder = ProgramBuilder::new();
        builder.constant(Const::Decimal(Decimal::new(i128::MAX, 12)));
        let mut program = builder.build();
        program.const_pool[0] = ConstEntry::Decimal(range);
        assert_eq!(
            program.const_decimal(ConstId(0)),
            Err(DecodeError::OutOfBounds)
        );
        assert!(matches!(
            verify_program(&program, &VerifyConfig::default()),
            Err(VerifyError::InvalidMetadata {
                table: MetadataTable::Constants,
                entry: 0,
                ..
            })
        ));
    }
}

#[test]
fn previous_wire_version_is_rejected_explicitly() {
    let mut bytes = ProgramBuilder::new().build().encode();
    for minor in [4_u16, 5] {
        bytes[10..12].copy_from_slice(&minor.to_le_bytes());
        assert!(
            matches!(
                Program::decode(&bytes),
                Err(DecodeError::UnsupportedVersion { major: 0, minor: m }) if m == minor
            ),
            "wire version 0.{minor} must be rejected"
        );
    }
}
