// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Every 32-bit numeric and conversion opcode (`0xFD` prefix) against a Rust reference.
//!
//! Each case assembles a one-instruction function through the public codec, round-trips it
//! through the wire format, verifies it and runs it on every combination of edge-case inputs.
//! Results must match the reference, including traps; float results compare by bits except that
//! any NaN matches any NaN (payload propagation is checked separately).

use std::fmt::Write as _;

use execution_tape::codec::{Operand, encode_instruction};
use execution_tape::host::{Host, HostContext, HostError, SigHash, ValueRef};
use execution_tape::opcode::{Opcode, OpcodePrefix};
use execution_tape::program::{FunctionDef, Program, TypeTableDef, ValueType};
use execution_tape::trace::TraceMask;
use execution_tape::value::{FuncId, FuncRef, Value};
use execution_tape::verifier::{VerifyConfig, VerifyError, verify_program, verify_program_owned};
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

/// A reference result: output values, or the trap the instruction must raise.
type Outcome = Result<Vec<Value>, Trap>;

struct Case {
    op: Opcode,
    ins: &'static [ValueType],
    outs: &'static [ValueType],
    reference: fn(&[Value]) -> Outcome,
}

fn i(v: &Value) -> i32 {
    let Value::I32(v) = v else { panic!("{v:?}") };
    *v
}
fn u(v: &Value) -> u32 {
    let Value::U32(v) = v else { panic!("{v:?}") };
    *v
}
fn f(v: &Value) -> f32 {
    let Value::F32(v) = v else { panic!("{v:?}") };
    *v
}
fn i64v(v: &Value) -> i64 {
    let Value::I64(v) = v else { panic!("{v:?}") };
    *v
}
fn u64v(v: &Value) -> u64 {
    let Value::U64(v) = v else { panic!("{v:?}") };
    *v
}
fn f64v(v: &Value) -> f64 {
    let Value::F64(v) = v else { panic!("{v:?}") };
    *v
}

fn one(v: Value) -> Outcome {
    Ok(vec![v])
}

fn sdiv(a: i32, b: i32, op: fn(i32, i32) -> i32) -> Outcome {
    if b == 0 {
        return Err(Trap::DivByZero);
    }
    if a == i32::MIN && b == -1 {
        return Err(Trap::IntDivOverflow);
    }
    one(Value::I32(op(a, b)))
}

fn udiv(a: u32, b: u32, op: fn(u32, u32) -> u32) -> Outcome {
    if b == 0 {
        return Err(Trap::DivByZero);
    }
    one(Value::U32(op(a, b)))
}

fn nan_min(a: f32, b: f32) -> f32 {
    if a.is_nan() || b.is_nan() {
        f32::NAN
    } else if a == b {
        // Distinguishes -0.0 from 0.0.
        if a.is_sign_negative() { a } else { b }
    } else {
        a.min(b)
    }
}
fn nan_max(a: f32, b: f32) -> f32 {
    if a.is_nan() || b.is_nan() {
        f32::NAN
    } else if a == b {
        if a.is_sign_positive() { a } else { b }
    } else {
        a.max(b)
    }
}
fn num_min(a: f32, b: f32) -> f32 {
    match (a.is_nan(), b.is_nan()) {
        (true, true) => f32::NAN,
        (true, false) => b,
        (false, true) => a,
        (false, false) => nan_min(a, b),
    }
}
fn num_max(a: f32, b: f32) -> f32 {
    match (a.is_nan(), b.is_nan()) {
        (true, true) => f32::NAN,
        (true, false) => b,
        (false, true) => a,
        (false, false) => nan_max(a, b),
    }
}

fn f2i(x: f64, lo: f64, hi: f64) -> Result<f64, Trap> {
    if !x.is_finite() {
        return Err(Trap::FloatToIntInvalid);
    }
    let t = x.trunc();
    if t < lo || t > hi {
        return Err(Trap::IntCastOverflow);
    }
    Ok(t)
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "reference conversions use Rust's defined casts"
)]
fn cases() -> Vec<Case> {
    macro_rules! case {
        ($op:ident, [$($in:ident),*], [$($out:ident),*], $r:expr) => {
            Case { op: Opcode::$op, ins: &[$($in),*], outs: &[$($out),*], reference: $r }
        };
    }
    vec![
        case!(I32Add, [I32, I32], [I32], |v| one(Value::I32(
            i(&v[0]).wrapping_add(i(&v[1]))
        ))),
        case!(I32Sub, [I32, I32], [I32], |v| one(Value::I32(
            i(&v[0]).wrapping_sub(i(&v[1]))
        ))),
        case!(I32Mul, [I32, I32], [I32], |v| one(Value::I32(
            i(&v[0]).wrapping_mul(i(&v[1]))
        ))),
        case!(I32Div, [I32, I32], [I32], |v| sdiv(
            i(&v[0]),
            i(&v[1]),
            |a, b| a / b
        )),
        case!(I32Rem, [I32, I32], [I32], |v| sdiv(
            i(&v[0]),
            i(&v[1]),
            |a, b| a % b
        )),
        case!(I32AddOvf, [I32, I32], [I32, Bool], |v| {
            let (r, o) = i(&v[0]).overflowing_add(i(&v[1]));
            Ok(vec![Value::I32(r), Value::Bool(o)])
        }),
        case!(I32SubOvf, [I32, I32], [I32, Bool], |v| {
            let (r, o) = i(&v[0]).overflowing_sub(i(&v[1]));
            Ok(vec![Value::I32(r), Value::Bool(o)])
        }),
        case!(I32MulOvf, [I32, I32], [I32, Bool], |v| {
            let (r, o) = i(&v[0]).overflowing_mul(i(&v[1]));
            Ok(vec![Value::I32(r), Value::Bool(o)])
        }),
        case!(I32And, [I32, I32], [I32], |v| one(Value::I32(
            i(&v[0]) & i(&v[1])
        ))),
        case!(I32Or, [I32, I32], [I32], |v| one(Value::I32(
            i(&v[0]) | i(&v[1])
        ))),
        case!(I32Xor, [I32, I32], [I32], |v| one(Value::I32(
            i(&v[0]) ^ i(&v[1])
        ))),
        case!(I32Shl, [I32, I32], [I32], |v| one(Value::I32(
            i(&v[0]).wrapping_shl(i(&v[1]).cast_unsigned())
        ))),
        case!(I32Shr, [I32, I32], [I32], |v| one(Value::I32(
            i(&v[0]).wrapping_shr(i(&v[1]).cast_unsigned())
        ))),
        case!(I32Eq, [I32, I32], [Bool], |v| one(Value::Bool(
            i(&v[0]) == i(&v[1])
        ))),
        case!(I32Lt, [I32, I32], [Bool], |v| one(Value::Bool(
            i(&v[0]) < i(&v[1])
        ))),
        case!(I32Gt, [I32, I32], [Bool], |v| one(Value::Bool(
            i(&v[0]) > i(&v[1])
        ))),
        case!(I32Le, [I32, I32], [Bool], |v| one(Value::Bool(
            i(&v[0]) <= i(&v[1])
        ))),
        case!(I32Ge, [I32, I32], [Bool], |v| one(Value::Bool(
            i(&v[0]) >= i(&v[1])
        ))),
        case!(U32Add, [U32, U32], [U32], |v| one(Value::U32(
            u(&v[0]).wrapping_add(u(&v[1]))
        ))),
        case!(U32Sub, [U32, U32], [U32], |v| one(Value::U32(
            u(&v[0]).wrapping_sub(u(&v[1]))
        ))),
        case!(U32Mul, [U32, U32], [U32], |v| one(Value::U32(
            u(&v[0]).wrapping_mul(u(&v[1]))
        ))),
        case!(U32Div, [U32, U32], [U32], |v| udiv(
            u(&v[0]),
            u(&v[1]),
            |a, b| a / b
        )),
        case!(U32Rem, [U32, U32], [U32], |v| udiv(
            u(&v[0]),
            u(&v[1]),
            |a, b| a % b
        )),
        case!(U32AddOvf, [U32, U32], [U32, Bool], |v| {
            let (r, o) = u(&v[0]).overflowing_add(u(&v[1]));
            Ok(vec![Value::U32(r), Value::Bool(o)])
        }),
        case!(U32SubOvf, [U32, U32], [U32, Bool], |v| {
            let (r, o) = u(&v[0]).overflowing_sub(u(&v[1]));
            Ok(vec![Value::U32(r), Value::Bool(o)])
        }),
        case!(U32MulOvf, [U32, U32], [U32, Bool], |v| {
            let (r, o) = u(&v[0]).overflowing_mul(u(&v[1]));
            Ok(vec![Value::U32(r), Value::Bool(o)])
        }),
        case!(U32And, [U32, U32], [U32], |v| one(Value::U32(
            u(&v[0]) & u(&v[1])
        ))),
        case!(U32Or, [U32, U32], [U32], |v| one(Value::U32(
            u(&v[0]) | u(&v[1])
        ))),
        case!(U32Xor, [U32, U32], [U32], |v| one(Value::U32(
            u(&v[0]) ^ u(&v[1])
        ))),
        case!(U32Shl, [U32, U32], [U32], |v| one(Value::U32(
            u(&v[0]).wrapping_shl(u(&v[1]))
        ))),
        case!(U32Shr, [U32, U32], [U32], |v| one(Value::U32(
            u(&v[0]).wrapping_shr(u(&v[1]))
        ))),
        case!(U32Clz, [U32], [U32], |v| one(Value::U32(
            u(&v[0]).leading_zeros()
        ))),
        case!(U32Ctz, [U32], [U32], |v| one(Value::U32(
            u(&v[0]).trailing_zeros()
        ))),
        case!(U32Popcnt, [U32], [U32], |v| one(Value::U32(
            u(&v[0]).count_ones()
        ))),
        case!(U32Rotl, [U32, U32], [U32], |v| one(Value::U32(
            u(&v[0]).rotate_left(u(&v[1]))
        ))),
        case!(U32Rotr, [U32, U32], [U32], |v| one(Value::U32(
            u(&v[0]).rotate_right(u(&v[1]))
        ))),
        case!(U32Eq, [U32, U32], [Bool], |v| one(Value::Bool(
            u(&v[0]) == u(&v[1])
        ))),
        case!(U32Lt, [U32, U32], [Bool], |v| one(Value::Bool(
            u(&v[0]) < u(&v[1])
        ))),
        case!(U32Gt, [U32, U32], [Bool], |v| one(Value::Bool(
            u(&v[0]) > u(&v[1])
        ))),
        case!(U32Le, [U32, U32], [Bool], |v| one(Value::Bool(
            u(&v[0]) <= u(&v[1])
        ))),
        case!(U32Ge, [U32, U32], [Bool], |v| one(Value::Bool(
            u(&v[0]) >= u(&v[1])
        ))),
        case!(F32Add, [F32, F32], [F32], |v| one(Value::F32(
            f(&v[0]) + f(&v[1])
        ))),
        case!(F32Sub, [F32, F32], [F32], |v| one(Value::F32(
            f(&v[0]) - f(&v[1])
        ))),
        case!(F32Mul, [F32, F32], [F32], |v| one(Value::F32(
            f(&v[0]) * f(&v[1])
        ))),
        case!(F32Div, [F32, F32], [F32], |v| one(Value::F32(
            f(&v[0]) / f(&v[1])
        ))),
        case!(F32Rem, [F32, F32], [F32], |v| one(Value::F32(
            f(&v[0]) % f(&v[1])
        ))),
        case!(F32Neg, [F32], [F32], |v| one(Value::F32(-f(&v[0])))),
        case!(F32Abs, [F32], [F32], |v| one(Value::F32(f(&v[0]).abs()))),
        case!(F32Min, [F32, F32], [F32], |v| one(Value::F32(nan_min(
            f(&v[0]),
            f(&v[1])
        )))),
        case!(F32Max, [F32, F32], [F32], |v| one(Value::F32(nan_max(
            f(&v[0]),
            f(&v[1])
        )))),
        case!(F32MinNum, [F32, F32], [F32], |v| one(Value::F32(num_min(
            f(&v[0]),
            f(&v[1])
        )))),
        case!(F32MaxNum, [F32, F32], [F32], |v| one(Value::F32(num_max(
            f(&v[0]),
            f(&v[1])
        )))),
        case!(F32CopySign, [F32, F32], [F32], |v| one(Value::F32(
            f(&v[0]).copysign(f(&v[1]))
        ))),
        case!(F32Floor, [F32], [F32], |v| one(Value::F32(
            f(&v[0]).floor()
        ))),
        case!(F32Ceil, [F32], [F32], |v| one(Value::F32(f(&v[0]).ceil()))),
        case!(F32Trunc, [F32], [F32], |v| one(Value::F32(
            f(&v[0]).trunc()
        ))),
        case!(F32Nearest, [F32], [F32], |v| one(Value::F32(
            f(&v[0]).round_ties_even()
        ))),
        case!(F32Sqrt, [F32], [F32], |v| one(Value::F32(f(&v[0]).sqrt()))),
        case!(F32Eq, [F32, F32], [Bool], |v| one(Value::Bool(
            f(&v[0]) == f(&v[1])
        ))),
        case!(F32Ne, [F32, F32], [Bool], |v| one(Value::Bool(
            f(&v[0]) != f(&v[1])
        ))),
        case!(F32Lt, [F32, F32], [Bool], |v| one(Value::Bool(
            f(&v[0]) < f(&v[1])
        ))),
        case!(F32Gt, [F32, F32], [Bool], |v| one(Value::Bool(
            f(&v[0]) > f(&v[1])
        ))),
        case!(F32Le, [F32, F32], [Bool], |v| one(Value::Bool(
            f(&v[0]) <= f(&v[1])
        ))),
        case!(F32Ge, [F32, F32], [Bool], |v| one(Value::Bool(
            f(&v[0]) >= f(&v[1])
        ))),
        case!(I32ToI64, [I32], [I64], |v| one(Value::I64(i64::from(i(
            &v[0]
        ))))),
        case!(I64ToI32, [I64], [I32], |v| i32::try_from(i64v(&v[0]))
            .map(|x| vec![Value::I32(x)])
            .map_err(|_| Trap::IntCastOverflow)),
        case!(I64ToI32Wrap, [I64], [I32], |v| one(Value::I32(
            i64v(&v[0]) as i32
        ))),
        case!(U32ToU64, [U32], [U64], |v| one(Value::U64(u64::from(u(
            &v[0]
        ))))),
        case!(U64ToU32, [U64], [U32], |v| u32::try_from(u64v(&v[0]))
            .map(|x| vec![Value::U32(x)])
            .map_err(|_| Trap::IntCastOverflow)),
        case!(U64ToU32Wrap, [U64], [U32], |v| one(Value::U32(
            u64v(&v[0]) as u32
        ))),
        case!(U32ToI64, [U32], [I64], |v| one(Value::I64(i64::from(u(
            &v[0]
        ))))),
        case!(I32ToU32, [I32], [U32], |v| u32::try_from(i(&v[0]))
            .map(|x| vec![Value::U32(x)])
            .map_err(|_| Trap::IntCastOverflow)),
        case!(U32ToI32, [U32], [I32], |v| i32::try_from(u(&v[0]))
            .map(|x| vec![Value::I32(x)])
            .map_err(|_| Trap::IntCastOverflow)),
        case!(I32ToU32Wrap, [I32], [U32], |v| one(Value::U32(
            i(&v[0]).cast_unsigned()
        ))),
        case!(U32ToI32Wrap, [U32], [I32], |v| one(Value::I32(
            u(&v[0]).cast_signed()
        ))),
        case!(I64ToU64Wrap, [I64], [U64], |v| one(Value::U64(
            i64v(&v[0]).cast_unsigned()
        ))),
        case!(U64ToI64Wrap, [U64], [I64], |v| one(Value::I64(
            u64v(&v[0]).cast_signed()
        ))),
        case!(F32ToF64, [F32], [F64], |v| one(Value::F64(f64::from(f(
            &v[0]
        ))))),
        case!(F64ToF32, [F64], [F32], |v| one(Value::F32(
            f64v(&v[0]) as f32
        ))),
        case!(F32ToBits, [F32], [U32], |v| one(Value::U32(
            f(&v[0]).to_bits()
        ))),
        case!(F32FromBits, [U32], [F32], |v| one(Value::F32(
            f32::from_bits(u(&v[0]))
        ))),
        case!(F32ToI32, [F32], [I32], |v| f2i(
            f64::from(f(&v[0])),
            -2_147_483_648.0,
            2_147_483_647.0
        )
        .map(|t| vec![Value::I32(t as i32)])),
        case!(F32ToU32, [F32], [U32], |v| f2i(
            f64::from(f(&v[0])),
            0.0,
            4_294_967_295.0
        )
        .map(|t| vec![Value::U32(t as u32)])),
        case!(F32ToI32Sat, [F32], [I32], |v| one(Value::I32(
            f(&v[0]) as i32
        ))),
        case!(F32ToU32Sat, [F32], [U32], |v| one(Value::U32(
            f(&v[0]) as u32
        ))),
        case!(F64ToI32, [F64], [I32], |v| f2i(
            f64v(&v[0]),
            -2_147_483_648.0,
            2_147_483_647.0
        )
        .map(|t| vec![Value::I32(t as i32)])),
        case!(F64ToU32, [F64], [U32], |v| f2i(
            f64v(&v[0]),
            0.0,
            4_294_967_295.0
        )
        .map(|t| vec![Value::U32(t as u32)])),
        case!(F64ToI32Sat, [F64], [I32], |v| one(Value::I32(
            f64v(&v[0]) as i32
        ))),
        case!(F64ToU32Sat, [F64], [U32], |v| one(Value::U32(
            f64v(&v[0]) as u32
        ))),
        case!(I32ToF32, [I32], [F32], |v| one(Value::F32(i(&v[0]) as f32))),
        case!(U32ToF32, [U32], [F32], |v| one(Value::F32(u(&v[0]) as f32))),
        case!(I64ToF32, [I64], [F32], |v| one(Value::F32(
            i64v(&v[0]) as f32
        ))),
        case!(U64ToF32, [U64], [F32], |v| one(Value::F32(
            u64v(&v[0]) as f32
        ))),
        case!(I32ToF64, [I32], [F64], |v| one(Value::F64(f64::from(i(
            &v[0]
        ))))),
        case!(U32ToF64, [U32], [F64], |v| one(Value::F64(f64::from(u(
            &v[0]
        ))))),
    ]
}

fn samples(t: ValueType) -> Vec<Value> {
    match t {
        I32 => [
            0,
            1,
            -1,
            2,
            -7,
            31,
            32,
            33,
            -32,
            12_345_678,
            -98_765,
            i32::MIN,
            i32::MIN + 1,
            i32::MAX,
        ]
        .map(Value::I32)
        .to_vec(),
        U32 => [
            0,
            1,
            2,
            7,
            31,
            32,
            33,
            1_000_000,
            0xDEAD_BEEF,
            1 << 31,
            u32::MAX,
        ]
        .map(Value::U32)
        .to_vec(),
        F32 => [
            0.0,
            -0.0,
            1.0,
            -1.0,
            0.5,
            -0.5,
            1.5,
            2.5,
            -2.5,
            3.7,
            -1e10,
            1e30,
            f32::MAX,
            f32::MIN_POSITIVE,
            f32::from_bits(1),
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NAN,
            f32::from_bits(0xFFC0_1234),
            2_147_483_520.0,
            2_147_483_648.0,
            -2_147_483_648.0,
            -2_147_483_904.0,
            4_294_967_040.0,
            4_294_967_296.0,
        ]
        .map(Value::F32)
        .to_vec(),
        I64 => [
            0,
            1,
            -1,
            i64::from(i32::MAX),
            i64::from(i32::MAX) + 1,
            i64::from(i32::MIN),
            i64::from(i32::MIN) - 1,
            (1 << 53) + 1,
            i64::MAX,
            i64::MIN,
        ]
        .map(Value::I64)
        .to_vec(),
        U64 => [
            0,
            1,
            u64::from(u32::MAX),
            u64::from(u32::MAX) + 1,
            (1 << 53) + 1,
            (1 << 63) + (1 << 39) + 1,
            u64::MAX,
        ]
        .map(Value::U64)
        .to_vec(),
        F64 => [
            0.0,
            -0.0,
            1.5,
            -1.5,
            -0.9,
            -1.0,
            2_147_483_647.9,
            2_147_483_648.0,
            -2_147_483_648.9,
            -2_147_483_649.0,
            4_294_967_295.5,
            4_294_967_296.0,
            1e300,
            1.0 + f64::from(f32::EPSILON) / 2.0,
            f64::from(f32::MAX) * 1.000_000_1,
            f64::INFINITY,
            f64::NAN,
            f64::from_bits(0x7FF0_0000_2000_0001),
        ]
        .map(Value::F64)
        .to_vec(),
        other => panic!("no samples for {other:?}"),
    }
}

fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::F32(x), Value::F32(y)) => (x.is_nan() && y.is_nan()) || x.to_bits() == y.to_bits(),
        (Value::F64(x), Value::F64(y)) => (x.is_nan() && y.is_nan()) || x.to_bits() == y.to_bits(),
        _ => a == b,
    }
}

/// Builds `f(ins...) -> outs` whose body is the one instruction, via the wire format.
fn program(case: &Case) -> Program {
    let regs = |start: u32, n: usize| (start..).take(n).collect::<Vec<u32>>();
    let args = regs(1, case.ins.len());
    let outs = regs(1 + u32::try_from(case.ins.len()).unwrap(), case.outs.len());
    let operands: Vec<Operand<'_>> = outs.iter().chain(&args).map(|&r| Operand::Reg(r)).collect();
    let mut bytecode = Vec::new();
    encode_instruction(case.op, &operands, &mut bytecode).unwrap();
    encode_instruction(
        Opcode::Ret,
        &[Operand::Reg(0), Operand::RegList(&outs)],
        &mut bytecode,
    )
    .unwrap();
    let mut register_types = vec![ValueType::Unit];
    register_types.extend(case.ins);
    register_types.extend(case.outs);
    let program = Program::new(
        vec![],
        vec![],
        vec![],
        TypeTableDef::default(),
        vec![FunctionDef {
            arg_types: case.ins.to_vec(),
            ret_types: case.outs.to_vec(),
            register_types,
            bytecode,
            spans: vec![],
        }],
    );
    Program::decode(&program.encode()).unwrap()
}

fn inputs(types: &[ValueType]) -> Vec<Vec<Value>> {
    types.iter().fold(vec![vec![]], |acc, &t| {
        acc.iter()
            .flat_map(|prefix| {
                samples(t).into_iter().map(move |v| {
                    let mut next = prefix.clone();
                    next.push(v);
                    next
                })
            })
            .collect()
    })
}

#[test]
fn every_numeric32_opcode_matches_its_reference() {
    let cases = cases();
    let mut failures = String::new();
    let mut checked = 0;
    for case in &cases {
        let program = verify_program_owned(program(case), &VerifyConfig::default())
            .unwrap_or_else(|e| panic!("{:?}: {e}", case.op));
        let mut vm = Vm::new(NoHost, Limits::default());
        let unit = vm.install(program).unwrap();
        for input in inputs(case.ins) {
            let got = vm
                .run(FuncRef::new(unit, FuncId(0)), &input, TraceMask::NONE, None)
                .map_err(|t| t.trap);
            let want = (case.reference)(&input);
            let ok = match (&got, &want) {
                (Ok(g), Ok(w)) => g.len() == w.len() && g.iter().zip(w).all(|(g, w)| same(g, w)),
                (Err(g), Err(w)) => format!("{g:?}") == format!("{w:?}"),
                _ => false,
            };
            if !ok {
                writeln!(
                    failures,
                    "{} {input:?}: got {got:?}, want {want:?}",
                    case.op.mnemonic()
                )
                .unwrap();
            }
            checked += 1;
        }
    }
    assert!(failures.is_empty(), "{failures}");
    assert!(checked > 5_000, "{checked}");
}

/// Every assigned `0xFD` opcode is covered above, in its family's aligned block.
#[test]
fn numeric32_layout_is_covered_and_blocked_by_family() {
    let mut tested: Vec<Opcode> = cases().iter().map(|c| c.op).collect();
    tested.extend([Opcode::ConstI32, Opcode::ConstU32, Opcode::ConstF32]);
    for byte in 0..=u8::MAX {
        let Some(op) = Opcode::from_prefixed(OpcodePrefix::Numeric32, byte) else {
            continue;
        };
        assert!(tested.contains(&op), "{} untested", op.mnemonic());
        let family = op.mnemonic().split('.').next().unwrap();
        let block = match byte >> 6 {
            0 => "i32",
            1 => "u32",
            2 => "f32",
            _ => "conversion",
        };
        let consistent = match block {
            "conversion" => op.mnemonic().contains(".to_") || op.mnemonic().contains("_bits"),
            _ => family == block || op.mnemonic() == format!("const.{block}"),
        };
        assert!(consistent, "{} at {byte:#04X}", op.mnemonic());
    }
}

#[test]
fn constants_round_trip_with_exact_bits() {
    let mut asm = execution_tape::asm::Asm::new();
    asm.set_register_types(vec![ValueType::Unit, I32, U32, F32, F32]);
    asm.const_i32(1, i32::MIN)
        .const_u32(2, u32::MAX)
        .const_f32(3, -0.0)
        .const_f32_bits(4, 0x7FA0_0001)
        .ret(0, &[1, 2, 3, 4]);
    let mut builder = execution_tape::asm::ProgramBuilder::new();
    builder
        .push_function_checked(
            asm,
            execution_tape::asm::FunctionSig {
                arg_types: vec![],
                ret_types: vec![I32, U32, F32, F32],
            },
        )
        .unwrap();
    let program = Program::decode(&builder.build().encode()).unwrap();
    let disassembly = execution_tape::disasm::disassemble_function(&program, FuncId(0)).unwrap();
    let text: Vec<String> = disassembly.instrs().map(|i| i.to_string()).collect();
    assert!(
        text[0].contains("const.i32") && text[0].contains("-2147483648"),
        "{text:?}"
    );
    assert!(
        text[1].contains("const.u32") && text[1].contains("4294967295"),
        "{text:?}"
    );
    assert!(text[3].contains("0x7FA00001"), "{text:?}");
    let program = verify_program_owned(program, &VerifyConfig::default()).unwrap();
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(program).unwrap();
    let out = vm
        .run(FuncRef::new(unit, FuncId(0)), &[], TraceMask::NONE, None)
        .unwrap();
    assert_eq!(out[0], Value::I32(i32::MIN));
    assert_eq!(out[1], Value::U32(u32::MAX));
    let [Value::F32(z), Value::F32(n)] = out[2..] else {
        panic!("{out:?}");
    };
    // A signaling NaN constant is loaded unchanged, not quieted.
    assert_eq!((z.to_bits(), n.to_bits()), (0x8000_0000, 0x7FA0_0001));
}

#[test]
fn float_width_conversions_keep_nan_sign_and_payload() {
    let case = |op| cases().into_iter().find(|c| c.op == op).unwrap();
    let run = |op, input: Value| {
        let case = case(op);
        let program = verify_program_owned(program(&case), &VerifyConfig::default()).unwrap();
        let mut vm = Vm::new(NoHost, Limits::default());
        let unit = vm.install(program).unwrap();
        vm.run(
            FuncRef::new(unit, FuncId(0)),
            &[input],
            TraceMask::NONE,
            None,
        )
        .unwrap()
        .remove(0)
    };
    let Value::F64(wide) = run(Opcode::F32ToF64, Value::F32(f32::from_bits(0xFFA0_1234))) else {
        panic!()
    };
    assert_eq!(wide.to_bits(), 0xFFFC_0246_8000_0000);
    let Value::F32(narrow) = run(Opcode::F64ToF32, Value::F64(wide)) else {
        panic!()
    };
    assert_eq!(narrow.to_bits(), 0xFFE0_1234);
    let Value::U32(bits) = run(Opcode::F32ToBits, Value::F32(f32::from_bits(0x7F80_0001))) else {
        panic!()
    };
    assert_eq!(bits, 0x7F80_0001);
}

#[test]
fn verifier_rejects_mixed_widths_and_signedness() {
    for (op, ins, outs) in [
        (Opcode::I32Add, [I32, I64], [I32]),
        (Opcode::I32Add, [U32, U32], [I32]),
        (Opcode::U32Add, [U32, U32], [U64]),
        (Opcode::F32Add, [F32, F64], [F32]),
    ] {
        let case = Case {
            op,
            ins: Box::leak(Box::new(ins)),
            outs: Box::leak(Box::new(outs)),
            reference: |_| Ok(vec![]),
        };
        let err = verify_program(&program(&case), &VerifyConfig::default()).unwrap_err();
        assert!(
            matches!(err, VerifyError::RegisterTypeMismatch { .. }),
            "{}: {err:?}",
            op.mnemonic()
        );
    }
}
