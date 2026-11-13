// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! 32-bit value types (`i32`, `u32`, `f32`) through the public API.

use std::sync::Arc;

use execution_tape::asm::{FunctionSig, ProgramBuilder};
use execution_tape::host::{
    Host, HostContext, HostError, HostSig, SigHash, ValueRef, sig_hash_slices,
};
use execution_tape::program::{Program, ValueType};
use execution_tape::recording::{
    HostOutcome, RecordLimits, RecordedValue, Recorder, ReplayOutcome,
};
use execution_tape::trace::TraceSink;
use execution_tape::value::{FuncId, FuncRef, Value};
use execution_tape::verifier::{VerifiedProgram, VerifyConfig, verify_program_owned};
use execution_tape::vm::{Limits, Vm};

const W32: [ValueType; 3] = [ValueType::I32, ValueType::U32, ValueType::F32];

/// Echoes its arguments, checking that they arrive with their 32-bit kinds.
struct Echo;
impl Host for Echo {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        args: &[ValueRef<'_>],
        rets: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        let kinds: Vec<_> = args.iter().map(|a| a.value_type()).collect();
        assert_eq!(kinds, W32, "host arguments keep their 32-bit kinds");
        for (ret, arg) in rets.iter_mut().zip(args) {
            *ret = arg.to_value();
        }
        Ok(0)
    }
}

/// `f(cond, i, u, f)` passes the 32-bit values through a host call, a tuple, `select` and `mov`,
/// and returns them in their original order. Round-tripped through the wire format.
fn program() -> Arc<VerifiedProgram> {
    let mut builder = ProgramBuilder::new();
    let echo = builder.host_sig_for(
        "echo",
        HostSig {
            args: W32.to_vec(),
            rets: W32.to_vec(),
        },
    );
    let mut args = vec![ValueType::Bool];
    args.extend(W32);
    let id = builder
        .declare_function(FunctionSig {
            arg_types: args,
            ret_types: W32.to_vec(),
        })
        .unwrap();
    let mut body = builder.function_asm(id).unwrap();
    let echoed: Vec<u32> = W32.iter().map(|&t| body.alloc_register(t)).collect();
    let tuple = body.alloc_register(ValueType::Agg);
    let projected: Vec<u32> = W32.iter().map(|&t| body.alloc_register(t)).collect();
    let selected: Vec<u32> = W32.iter().map(|&t| body.alloc_register(t)).collect();
    let moved: Vec<u32> = W32.iter().map(|&t| body.alloc_register(t)).collect();
    body.host_call_sig_id(0, echo, 0, &[2, 3, 4], &echoed);
    body.tuple_new(tuple, &echoed);
    for (index, &dst) in (0..).zip(&projected) {
        body.tuple_get(dst, tuple, index);
    }
    for i in 0..3 {
        // Both arms hold the same value, so either choice of `cond` must preserve it.
        body.select(selected[i], 1, projected[i], echoed[i]);
        body.mov(moved[i], selected[i]);
    }
    body.ret(0, &moved);
    builder.define_function(id, body).unwrap();
    let bytes = builder.build().encode();
    let program = Program::decode(&bytes).unwrap();
    Arc::new(verify_program_owned(program, &VerifyConfig::default()).unwrap())
}

fn args(cond: bool, i: i32, u: u32, f: f32) -> Vec<Value> {
    vec![
        Value::Bool(cond),
        Value::I32(i),
        Value::U32(u),
        Value::F32(f),
    ]
}

fn bits(values: &[Value]) -> Vec<(ValueType, u64)> {
    values
        .iter()
        .map(|v| {
            let bits = match *v {
                Value::I32(i) => u64::from(i.cast_unsigned()),
                Value::U32(u) => u64::from(u),
                Value::F32(f) => u64::from(f.to_bits()),
                ref other => panic!("unexpected {other:?}"),
            };
            (v.value_type(), bits)
        })
        .collect()
}

#[test]
fn values_keep_kind_and_bits_across_calls_aggregates_and_moves() {
    let program = program();
    let mut vm = Vm::new(Echo, Limits::default());
    let unit = vm.install(Arc::clone(&program)).unwrap();
    let nan = f32::from_bits(0x7FC0_1234);
    for (i, u, f) in [
        (0, 0, 0.0),
        (i32::MIN, u32::MAX, -0.0),
        (i32::MAX, 1 << 31, nan),
        (-1, 7, f32::INFINITY),
        (12345, 0xDEAD_BEEF, f32::from_bits(1)),
    ] {
        for cond in [false, true] {
            let input = args(cond, i, u, f);
            let out = vm
                .run(
                    FuncRef::new(unit, FuncId(0)),
                    &input,
                    execution_tape::trace::TraceMask::NONE,
                    None,
                )
                .unwrap();
            assert_eq!(bits(&out), bits(&input[1..]), "{input:?}");
        }
    }
}

#[test]
fn entry_arguments_are_checked_against_their_32_bit_kinds() {
    let program = program();
    let mut vm = Vm::new(Echo, Limits::default());
    let unit = vm.install(program).unwrap();
    let wrong = vec![
        Value::Bool(true),
        Value::I64(1),
        Value::U32(2),
        Value::F32(3.0),
    ];
    let trap = vm
        .run(
            FuncRef::new(unit, FuncId(0)),
            &wrong,
            execution_tape::trace::TraceMask::NONE,
            None,
        )
        .unwrap_err();
    assert!(
        matches!(
            trap.trap,
            execution_tape::vm::Trap::TypeMismatch {
                expected: ValueType::I32,
                actual: ValueType::I64
            }
        ),
        "{trap:?}"
    );
}

#[test]
fn host_signatures_distinguish_width_and_signedness() {
    let hashes: Vec<SigHash> = [
        ValueType::I32,
        ValueType::U32,
        ValueType::F32,
        ValueType::I64,
        ValueType::U64,
        ValueType::F64,
    ]
    .iter()
    .map(|&t| sig_hash_slices(&[t], &[t]))
    .collect();
    for (i, a) in hashes.iter().enumerate() {
        for b in &hashes[i + 1..] {
            assert_ne!(a, b);
        }
    }
}

#[test]
fn recordings_keep_32_bit_kinds_and_float_bits() {
    let program = program();
    let mut vm = Vm::new(Echo, Limits::default());
    let unit = vm.install(Arc::clone(&program)).unwrap();
    let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
    let nan = f32::from_bits(0x7FA0_0001);
    vm.run(
        FuncRef::new(unit, FuncId(0)),
        &args(true, -5, 9, nan),
        recorder.mask(),
        Some(&mut recorder),
    )
    .unwrap();
    let record = recorder.finish();
    record.status().unwrap();
    let call = record.host_calls().next().expect("one host call");
    let HostOutcome::Returned { values, .. } = call.outcome else {
        panic!("host returned");
    };
    let returned: Vec<_> = values
        .iter()
        .map(|v| match v {
            RecordedValue::I32(i) => (ValueType::I32, u64::from(i.cast_unsigned())),
            RecordedValue::U32(u) => (ValueType::U32, u64::from(*u)),
            RecordedValue::F32(bits) => (ValueType::F32, u64::from(*bits)),
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    assert_eq!(
        returned,
        [
            (ValueType::I32, u64::from((-5_i32).cast_unsigned())),
            (ValueType::U32, 9),
            (ValueType::F32, u64::from(nan.to_bits())),
        ]
    );
    let ReplayOutcome::Returned(snapshot) = record.replay().unwrap() else {
        panic!("expected replay return");
    };
    assert_eq!(bits(snapshot.values()), bits(&args(true, -5, 9, nan)[1..]));
}
