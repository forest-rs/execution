// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The fixed cost of one short, pure run on a parked VM.
//!
//! Shaped like an embedder that evaluates small scalar expressions many times: one installed
//! function of three `f32` inputs that executes 14 instructions (13 arithmetic and constant
//! instructions, then `ret`) and returns three `f32`s, on a VM kept between runs with reused
//! argument and result buffers. `ret_only` runs a function that only returns its inputs, which
//! isolates per-run overhead from per-instruction cost. `native` is the same arithmetic in Rust.
//! Program construction and installation are outside timing.

use std::hint::black_box;
use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use execution_tape::asm::{FunctionSig, ProgramBuilder, Reg};
use execution_tape::host::{Host, HostContext, HostError, SigHash, ValueRef};
use execution_tape::program::ValueType;
use execution_tape::trace::TraceMask;
use execution_tape::value::{FuncId, FuncRef, Value};
use execution_tape::verifier::VerifiedProgram;
use execution_tape::vm::{ExecutionContext, Limits, Vm};

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

const F32: ValueType = ValueType::F32;

/// `(age, chipping, fade) -> (chip, fade, gloss)`: 14 instructions.
fn expressions() -> Arc<VerifiedProgram> {
    let mut builder = ProgramBuilder::new();
    let function = builder
        .declare_function(FunctionSig {
            arg_types: vec![F32, F32, F32],
            ret_types: vec![F32, F32, F32],
        })
        .unwrap();
    let mut a = builder.function_asm(function).unwrap();
    let (age, chipping, fade) = (Reg(1), Reg(2), Reg(3));
    let k = a.alloc_register(F32);
    let t = a.alloc_register(F32);
    let chip = a.alloc_register(F32);
    let faded = a.alloc_register(F32);
    let gloss = a.alloc_register(F32);
    a.const_f32(k, 0.6).f32_mul(t, k, age).f32_mul(t, t, age);
    a.f32_max_num(chip, chipping, t);
    a.const_f32(k, 0.9).f32_mul(t, k, age).f32_add(t, fade, t);
    a.const_f32(k, 1.0).f32_min_num(faded, t, k);
    a.const_f32(k, 0.5)
        .f32_mul(gloss, chip, k)
        .f32_add(gloss, gloss, faded)
        .f32_min_num(gloss, gloss, k);
    a.ret(Reg::EFFECT, &[chip, faded, gloss]);
    builder.define_function(function, a).unwrap();
    Arc::new(builder.build_verified().unwrap())
}

/// `(a, b, c) -> (a, b, c)`: one instruction.
fn ret_only() -> Arc<VerifiedProgram> {
    let mut builder = ProgramBuilder::new();
    let function = builder
        .declare_function(FunctionSig {
            arg_types: vec![F32, F32, F32],
            ret_types: vec![F32, F32, F32],
        })
        .unwrap();
    let mut a = builder.function_asm(function).unwrap();
    a.ret(Reg::EFFECT, &[Reg(1), Reg(2), Reg(3)]);
    builder.define_function(function, a).unwrap();
    Arc::new(builder.build_verified().unwrap())
}

fn native(age: f32, chipping: f32, fade: f32) -> [f32; 3] {
    let chip = chipping.max(0.6 * age * age);
    let faded = (fade + 0.9 * age).min(1.0);
    let gloss = (chip * 0.5 + faded).min(0.5);
    [chip, faded, gloss]
}

fn parked(program: Arc<VerifiedProgram>, fuel: u64) -> (Vm<NoHost>, FuncRef) {
    let mut vm = Vm::new(
        NoHost,
        Limits {
            fuel,
            max_call_depth: 1,
            max_host_calls: 0,
            ..Limits::default()
        },
    );
    let unit = vm.install(program).unwrap();
    (vm, FuncRef::new(unit, FuncId(0)))
}

fn bench(c: &mut Criterion) {
    let mut group = c.benchmark_group("short_runs");
    let inputs = [0.078_125_f32, 0.25, 0.5];
    let expected = native(inputs[0], inputs[1], inputs[2]);

    for (name, program, fuel, check) in [
        ("pure14", expressions(), 14, true),
        ("ret_only", ret_only(), 1, false),
    ] {
        let (mut vm, entry) = parked(program, fuel);
        let mut ctx = ExecutionContext::new();
        let mut args = Vec::with_capacity(3);
        let mut results = Vec::with_capacity(3);
        let mut run = |vm: &mut Vm<NoHost>, x: [f32; 3]| {
            args.clear();
            args.extend(x.map(Value::F32));
            results.clear();
            vm.run_with_ctx(
                &mut ctx,
                entry,
                &args,
                &mut results,
                TraceMask::NONE,
                None,
                None,
            )
            .unwrap();
            match results.as_slice() {
                [Value::F32(a), Value::F32(b), Value::F32(c)] => [*a, *b, *c],
                other => panic!("unexpected results {other:?}"),
            }
        };
        if check {
            assert_eq!(run(&mut vm, inputs), expected);
        }
        group.bench_function(format!("{name}/parked"), |b| {
            b.iter(|| run(&mut vm, black_box(inputs)));
        });
    }
    group.bench_function("pure14/native", |b| {
        b.iter(|| {
            let [a, x, y] = black_box(inputs);
            native(a, x, y)
        });
    });
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
