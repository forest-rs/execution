// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Primitive addition on bounded inputs, with preparation and output checking outside timing.

use std::{hint::black_box, time::Duration};

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use execution_tape::{
    asm::{Asm, FunctionSig, ProgramBuilder},
    host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef},
    program::{Const, ValueType},
    trace::TraceMask,
    value::{Decimal, Value},
    vm::{ExecutionContext, Limits, Vm},
};

struct EchoHost;
impl Host for EchoHost {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        args: &[ValueRef<'_>],
        rets: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        rets[0] = args[0].to_value();
        Ok(0)
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

fn numeric(c: &mut Criterion) {
    let mut group = c.benchmark_group("numeric_add");
    for count in [32, 1024] {
        for kind in [ValueType::I64, ValueType::F64, ValueType::Decimal] {
            let mut body = Asm::new();
            body.set_register_types(vec![ValueType::Unit, kind, kind]);
            let input = match kind {
                ValueType::I64 => {
                    body.const_i64(2, 1);
                    Value::I64(12345)
                }
                ValueType::F64 => {
                    body.const_f64(2, 0.01);
                    Value::F64(123.45)
                }
                ValueType::Decimal => {
                    body.const_decimal(2, 1, 2);
                    Value::Decimal(Decimal::new(12345, 2))
                }
                _ => unreachable!(),
            };
            for _ in 0..count {
                match kind {
                    ValueType::I64 => {
                        body.i64_add(1, 1, 2);
                    }
                    ValueType::F64 => {
                        body.f64_add(1, 1, 2);
                    }
                    ValueType::Decimal => {
                        body.dec_add(1, 1, 2);
                    }
                    _ => unreachable!(),
                }
            }
            body.ret(0, &[1]);
            let mut builder = ProgramBuilder::new();
            let entry = builder
                .push_function_checked(
                    body,
                    FunctionSig {
                        arg_types: vec![kind],
                        ret_types: vec![kind],
                    },
                )
                .unwrap();
            let program = builder.build_verified().unwrap();
            let mut context = ExecutionContext::new();
            let mut vm = Vm::new(NoHost, Limits::default());
            let args = [input];
            let mut outputs = Vec::new();
            vm.run_with_ctx(
                &mut context,
                &program,
                entry,
                &args,
                &mut outputs,
                TraceMask::NONE,
                None,
                None,
            )
            .unwrap();
            match &outputs[0] {
                Value::I64(value) => assert_eq!(*value, 12345 + count),
                Value::Decimal(value) => {
                    assert_eq!(*value, Decimal::new(i128::from(12345 + count), 2))
                }
                Value::F64(value) => {
                    assert!((*value - (123.45 + count as f64 * 0.01)).abs() < 1e-9)
                }
                _ => unreachable!(),
            }
            group.bench_function(BenchmarkId::new(format!("{kind:?}"), count), |b| {
                b.iter(|| {
                    vm.run_with_ctx(
                        &mut context,
                        black_box(&program),
                        entry,
                        black_box(&args),
                        &mut outputs,
                        TraceMask::NONE,
                        None,
                        None,
                    )
                    .unwrap();
                    black_box(&outputs);
                });
            });
        }
    }
    group.finish();
}

// Same values and instruction counts across inline, pool, and host paths. The
// timed run reuses context/output storage; construction and result checks are outside timing.
fn boundaries(c: &mut Criterion) {
    let mut group = c.benchmark_group("numeric_boundaries");
    for kind in [ValueType::I64, ValueType::Decimal] {
        for path in ["inline", "pool", "host"] {
            let decimal = Decimal::new(12345, 2);
            let expected = if kind == ValueType::I64 {
                Value::I64(12345)
            } else {
                Value::Decimal(decimal)
            };
            let mut builder = ProgramBuilder::new();
            let literal = builder.constant(if kind == ValueType::I64 {
                Const::I64(12345)
            } else {
                Const::Decimal(decimal)
            });
            let host = builder.host_sig_for(
                "echo",
                HostSig {
                    args: vec![kind],
                    rets: vec![kind],
                },
            );
            let mut body = Asm::new();
            body.set_register_types(vec![ValueType::Unit, kind]);
            body.const_pool(1, literal);
            for _ in 0..128 {
                match path {
                    "inline" if kind == ValueType::I64 => {
                        body.const_i64(1, 12345);
                    }
                    "inline" => {
                        body.const_decimal_value(1, decimal);
                    }
                    "pool" => {
                        body.const_pool(1, literal);
                    }
                    _ => {
                        body.host_call_sig_id(0, host, 0, &[1], &[1]);
                    }
                }
            }
            body.ret(0, &[1]);
            let entry = builder
                .push_function_checked(
                    body,
                    FunctionSig {
                        arg_types: vec![],
                        ret_types: vec![kind],
                    },
                )
                .unwrap();
            let program = builder.build_verified().unwrap();
            let mut vm = Vm::new(EchoHost, Limits::default());
            let mut context = ExecutionContext::new();
            let mut outputs = Vec::new();
            vm.run_with_ctx(
                &mut context,
                &program,
                entry,
                &[],
                &mut outputs,
                TraceMask::NONE,
                None,
                None,
            )
            .unwrap();
            assert_eq!(outputs, [expected]);
            group.bench_function(BenchmarkId::new(format!("{kind:?}"), path), |b| {
                b.iter(|| {
                    vm.run_with_ctx(
                        &mut context,
                        black_box(&program),
                        entry,
                        &[],
                        &mut outputs,
                        TraceMask::NONE,
                        None,
                        None,
                    )
                    .unwrap();
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
        .warm_up_time(Duration::from_millis(300))
        .measurement_time(Duration::from_millis(1200))
        .sample_size(40);
    targets = numeric, boundaries
}
criterion_main!(benches);
