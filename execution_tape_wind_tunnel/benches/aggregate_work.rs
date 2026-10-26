// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Guest construction work, including empty nodes and repeated fields.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use execution_tape::{
    asm::{FunctionSig, ProgramBuilder},
    host::{Host, HostContext, HostError, SigHash, ValueRef},
    program::{StructTypeDef, ValueType},
    trace::TraceMask,
    value::Value,
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

fn construction(c: &mut Criterion) {
    let mut group = c.benchmark_group("aggregate_construction");
    for width in [0, 1024] {
        for kind in ["tuple", "struct", "array"] {
            let mut builder = ProgramBuilder::new();
            let st = builder.struct_type(StructTypeDef {
                field_names: (0..width).map(|i| format!("f{i}")).collect(),
                field_types: vec![ValueType::I64; width],
            });
            let elem = builder.array_elem(ValueType::I64);
            let entry = builder
                .declare_function(FunctionSig {
                    arg_types: vec![ValueType::I64],
                    ret_types: vec![ValueType::U64],
                })
                .unwrap();
            let mut body = builder.function_asm(entry).unwrap();
            let aggregate = body.alloc_register(ValueType::Agg);
            let len = body.alloc_register(ValueType::U64);
            let fields = vec![1; width];
            for _ in 0..128 {
                match kind {
                    "tuple" => body.tuple_new(aggregate, &fields),
                    "struct" => body.struct_new(aggregate, st, &fields),
                    "array" => body.array_new(aggregate, elem, &fields),
                    _ => unreachable!(),
                };
            }
            match kind {
                "tuple" => body.tuple_len(len, aggregate),
                "struct" => body.struct_field_count(len, aggregate),
                "array" => body.array_len(len, aggregate),
                _ => unreachable!(),
            };
            body.ret(0, &[len]);
            builder.define_function(entry, body).unwrap();
            let program = builder.build_verified().unwrap();
            let mut vm = Vm::new(NoHost, Limits::default());
            let mut context = ExecutionContext::new();
            let args = [Value::I64(7)];
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
            assert_eq!(outputs, [Value::U64(width as u64)]);
            assert_eq!(vm.aggregates().len_u32(), 0);
            group.bench_function(BenchmarkId::new(kind, width), |b| {
                b.iter(|| {
                    outputs.clear();
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

criterion_group!(benches, construction);
criterion_main!(benches);
