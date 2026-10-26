// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Primitive addition on bounded inputs, with preparation and output checking outside timing.

use std::{hint::black_box, time::Duration};

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use execution_tape::{
    asm::{Asm, FunctionSig, ProgramBuilder},
    host::{Host, HostContext, HostError, SigHash, ValueRef},
    program::ValueType,
    trace::TraceMask,
    value::{Decimal, Value},
    vm::{ExecutionContext, Limits, Vm},
};

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
                    Value::Decimal(Decimal {
                        mantissa: 12345,
                        scale: 2,
                    })
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
                Value::Decimal(value) => assert_eq!(
                    *value,
                    Decimal {
                        mantissa: 12345 + count,
                        scale: 2
                    }
                ),
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

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_millis(300))
        .measurement_time(Duration::from_millis(1200))
        .sample_size(40);
    targets = numeric
}
criterion_main!(benches);
