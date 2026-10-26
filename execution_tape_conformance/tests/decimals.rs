// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Decimal range, representation, codec, and ABI boundaries through the public API.

use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
use execution_tape::codec::{self, DecodedOperand, Operand};
use execution_tape::disasm::ConstValue;
use execution_tape::format::DecodeError;
use execution_tape::host::{Host, HostContext, HostError, SigHash, ValueRef};
use execution_tape::opcode::Opcode;
use execution_tape::program::{ByteRange, Const, ConstEntry, ConstId, Program, ValueType};
use execution_tape::trace::TraceMask;
use execution_tape::value::{Decimal, FuncId, Value};
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
    Vm::new(NoHost, Limits::default()).run(program, FuncId(0), args, TraceMask::NONE, None)
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
    bytes[10..12].copy_from_slice(&4_u16.to_le_bytes());
    assert!(matches!(
        Program::decode(&bytes),
        Err(DecodeError::UnsupportedVersion { major: 0, minor: 4 })
    ));
}
