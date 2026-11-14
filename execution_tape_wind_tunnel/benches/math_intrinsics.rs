// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `f32` math as an intrinsic, as a host call computing the same function, and natively.
//!
//! Each case chains 128 dependent calls (`x = pow(x, 1.01)` or `x = sin(x)`), so per-call cost
//! is the reported time divided by 128. Program preparation and result checks are outside timing;
//! the VM cases reuse context and output storage.

use std::{hint::black_box, time::Duration};

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use execution_tape::{
    asm::{Asm, FunctionSig, ProgramBuilder},
    host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef},
    math,
    program::ValueType::{F32, Unit},
    trace::TraceMask,
    value::{FuncRef, Value},
    vm::{ExecutionContext, Limits, Vm},
};

const CHAIN: usize = 128;

/// Computes the same `math` function a tape intrinsic would.
struct MathHost;
impl Host for MathHost {
    fn call(
        &mut self,
        symbol: &str,
        _: SigHash,
        args: &[ValueRef<'_>],
        rets: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        let arg = |i: usize| match args[i] {
            ValueRef::F32(x) => x,
            _ => unreachable!(),
        };
        rets[0] = Value::F32(match symbol {
            "pow" => math::pow_f32(arg(0), arg(1)),
            "sin" => math::sin_f32(arg(0)),
            _ => return Err(HostError::UnknownSymbol),
        });
        Ok(0)
    }
}

#[derive(Clone, Copy, Debug)]
enum Op {
    Pow,
    Sin,
}

impl Op {
    fn native(self, x: f32) -> f32 {
        match self {
            Self::Pow => math::pow_f32(x, 1.01),
            Self::Sin => math::sin_f32(x),
        }
    }
}

fn chain(c: &mut Criterion) {
    let mut group = c.benchmark_group("f32_math_chain_128");
    for op in [Op::Pow, Op::Sin] {
        let mut expected = 0.7_f32;
        for _ in 0..CHAIN {
            expected = op.native(expected);
        }
        group.bench_function(BenchmarkId::new(format!("{op:?}"), "native"), |b| {
            b.iter(|| {
                let mut x = black_box(0.7_f32);
                for _ in 0..CHAIN {
                    x = op.native(x);
                }
                black_box(x)
            });
        });
        for path in ["intrinsic", "host"] {
            let mut builder = ProgramBuilder::new();
            let (symbol, args) = match op {
                Op::Pow => ("pow", vec![F32, F32]),
                Op::Sin => ("sin", vec![F32]),
            };
            let host = builder.host_sig_for(
                symbol,
                HostSig {
                    args,
                    rets: vec![F32],
                },
            );
            let mut body = Asm::new();
            body.set_register_types(vec![Unit, F32, F32]);
            body.const_f32(2, 1.01);
            for _ in 0..CHAIN {
                match (op, path) {
                    (Op::Pow, "intrinsic") => _ = body.f32_pow(1, 1, 2),
                    (Op::Sin, "intrinsic") => _ = body.f32_sin(1, 1),
                    (Op::Pow, _) => _ = body.host_call_sig_id(0, host, 0, &[1, 2], &[1]),
                    (Op::Sin, _) => _ = body.host_call_sig_id(0, host, 0, &[1], &[1]),
                }
            }
            body.ret(0, &[1]);
            let entry = builder
                .push_function_checked(
                    body,
                    FunctionSig {
                        arg_types: vec![F32],
                        ret_types: vec![F32],
                    },
                )
                .unwrap();
            let program = builder.build_verified().unwrap();
            let mut vm = Vm::new(MathHost, Limits::default());
            let mut context = ExecutionContext::new();
            let mut outputs = Vec::new();
            let unit = vm.install(program).unwrap();
            let args = [Value::F32(0.7)];
            let mut run = |outputs: &mut Vec<Value>| {
                outputs.clear();
                vm.run_with_ctx(
                    &mut context,
                    FuncRef::new(unit, entry),
                    black_box(&args),
                    outputs,
                    TraceMask::NONE,
                    None,
                    None,
                )
                .unwrap();
            };
            run(&mut outputs);
            assert_eq!(outputs, [Value::F32(expected)], "{op:?} {path}");
            group.bench_function(BenchmarkId::new(format!("{op:?}"), path), |b| {
                b.iter(|| {
                    run(&mut outputs);
                    black_box(&outputs);
                });
            });
        }
    }
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(2))
        .sample_size(40);
    targets = chain
}
criterion_main!(benches);
