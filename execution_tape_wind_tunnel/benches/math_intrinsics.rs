// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Float math as an intrinsic, as a host call computing the same function, and natively, in both
//! widths.
//!
//! Each case chains 128 dependent calls (`x = pow(x, 1.01)`, `x = sin(x)` or, for `f64`,
//! `x = exp(-x)`), so per-call cost is the reported time divided by 128. Program preparation and
//! result checks are outside timing; the VM cases reuse context and output storage.

use std::{hint::black_box, time::Duration};

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use execution_tape::{
    asm::{Asm, FunctionSig, ProgramBuilder},
    host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef},
    math,
    program::{
        HostSigId,
        ValueType::{self, F32, F64, Unit},
    },
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
        rets[0] = match (symbol, args) {
            ("pow", [ValueRef::F32(x), ValueRef::F32(y)]) => Value::F32(math::pow_f32(*x, *y)),
            ("sin", [ValueRef::F32(x)]) => Value::F32(math::sin_f32(*x)),
            ("pow", [ValueRef::F64(x), ValueRef::F64(y)]) => Value::F64(math::pow_f64(*x, *y)),
            ("sin", [ValueRef::F64(x)]) => Value::F64(math::sin_f64(*x)),
            ("exp", [ValueRef::F64(x)]) => Value::F64(math::exp_f64(*x)),
            _ => return Err(HostError::UnknownSymbol),
        };
        Ok(0)
    }
}

#[derive(Clone, Copy, Debug)]
enum Op {
    Pow,
    Sin,
    /// `exp(-x)`, which keeps the chain bounded; every path, native included, negates first.
    ExpNeg,
}

#[derive(Clone, Copy, Debug)]
enum Width {
    F32,
    F64,
}

impl Op {
    fn native32(self, x: f32) -> f32 {
        match self {
            Self::Pow => math::pow_f32(x, 1.01),
            Self::Sin => math::sin_f32(x),
            Self::ExpNeg => math::exp_f32(-x),
        }
    }

    fn native64(self, x: f64) -> f64 {
        match self {
            Self::Pow => math::pow_f64(x, 1.01),
            Self::Sin => math::sin_f64(x),
            Self::ExpNeg => math::exp_f64(-x),
        }
    }

    fn symbol(self) -> &'static str {
        match self {
            Self::Pow => "pow",
            Self::Sin => "sin",
            Self::ExpNeg => "exp",
        }
    }

    /// Emits one link of the chain on `r1`, with `r2` holding `1.01` and `r3` scratch.
    fn emit(self, body: &mut Asm, width: Width, host: Option<HostSigId>) {
        match (self, width) {
            (Self::ExpNeg, Width::F32) => _ = body.f32_neg(3, 1),
            (Self::ExpNeg, Width::F64) => _ = body.f64_neg(3, 1),
            _ => {}
        };
        let Some(host) = host else {
            match (self, width) {
                (Self::Pow, Width::F32) => _ = body.f32_pow(1, 1, 2),
                (Self::Sin, Width::F32) => _ = body.f32_sin(1, 1),
                (Self::ExpNeg, Width::F32) => _ = body.f32_exp(1, 3),
                (Self::Pow, Width::F64) => _ = body.f64_pow(1, 1, 2),
                (Self::Sin, Width::F64) => _ = body.f64_sin(1, 1),
                (Self::ExpNeg, Width::F64) => _ = body.f64_exp(1, 3),
            }
            return;
        };
        let args: &[u32] = match self {
            Self::Pow => &[1, 2],
            Self::Sin => &[1],
            Self::ExpNeg => &[3],
        };
        body.host_call_sig_id(0, host, 0, args, &[1]);
    }
}

fn chain(c: &mut Criterion) {
    let cases = [
        (Width::F32, Op::Pow),
        (Width::F32, Op::Sin),
        (Width::F64, Op::Pow),
        (Width::F64, Op::Sin),
        (Width::F64, Op::ExpNeg),
    ];
    for (width, op) in cases {
        let (group_name, ty, start, expected): (_, ValueType, _, _) = match width {
            Width::F32 => {
                let mut x = 0.7_f32;
                for _ in 0..CHAIN {
                    x = op.native32(x);
                }
                ("f32_math_chain_128", F32, Value::F32(0.7), Value::F32(x))
            }
            Width::F64 => {
                let mut x = 0.7_f64;
                for _ in 0..CHAIN {
                    x = op.native64(x);
                }
                ("f64_math_chain_128", F64, Value::F64(0.7), Value::F64(x))
            }
        };
        let mut group = c.benchmark_group(group_name);
        group.bench_function(BenchmarkId::new(format!("{op:?}"), "native"), |b| {
            b.iter(|| match width {
                Width::F32 => {
                    let mut x = black_box(0.7_f32);
                    for _ in 0..CHAIN {
                        x = op.native32(x);
                    }
                    black_box(x);
                }
                Width::F64 => {
                    let mut x = black_box(0.7_f64);
                    for _ in 0..CHAIN {
                        x = op.native64(x);
                    }
                    black_box(x);
                }
            });
        });
        for path in ["intrinsic", "host"] {
            let mut builder = ProgramBuilder::new();
            let host_args = match op {
                Op::Pow => vec![ty, ty],
                Op::Sin | Op::ExpNeg => vec![ty],
            };
            let host = builder.host_sig_for(
                op.symbol(),
                HostSig {
                    args: host_args,
                    rets: vec![ty],
                },
            );
            let mut body = Asm::new();
            body.set_register_types(vec![Unit, ty, ty, ty]);
            match width {
                Width::F32 => _ = body.const_f32(2, 1.01),
                Width::F64 => _ = body.const_f64(2, 1.01),
            }
            for _ in 0..CHAIN {
                op.emit(&mut body, width, (path == "host").then_some(host));
            }
            body.ret(0, &[1]);
            let entry = builder
                .push_function_checked(
                    body,
                    FunctionSig {
                        arg_types: vec![ty],
                        ret_types: vec![ty],
                    },
                )
                .unwrap();
            let program = builder.build_verified().unwrap();
            let mut vm = Vm::new(MathHost, Limits::default());
            let mut context = ExecutionContext::new();
            let mut outputs = Vec::new();
            let unit = vm.install(program).unwrap();
            let args = [start.clone()];
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
            assert_eq!(
                outputs,
                std::slice::from_ref(&expected),
                "{width:?} {op:?} {path}"
            );
            group.bench_function(BenchmarkId::new(format!("{op:?}"), path), |b| {
                b.iter(|| {
                    run(&mut outputs);
                    black_box(&outputs);
                });
            });
        }
        group.finish();
    }
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
