// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Keep scheduling, metadata, verifier-backed register inspection and object traversal distinct.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use execution_tape::{
    asm::{FunctionSig, ProgramBuilder},
    host::{Host, HostContext, HostError, SigHash, ValueRef},
    program::{HostTypeId, ValueType},
    trace::TraceMask,
    value::{Obj, ObjHandle, Value},
    vm::{ExecutionContext, InvocationOptions, Limits, RunStatus, Vm},
};
use std::{hint::black_box, time::Duration};

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
        panic!("no host calls");
    }
}
fn stepping(c: &mut Criterion) {
    let mut b = ProgramBuilder::new();
    let entry = b
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::Obj(HostTypeId(1))],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let mut a = b.function_asm(entry).unwrap();
    let value = a.alloc_register(ValueType::I64);
    let one = a.alloc_register(ValueType::I64);
    a.const_i64(value, 0).const_i64(one, 1);
    for _ in 0..1000 {
        a.i64_add(value, value, one);
    }
    a.ret(0, &[value]);
    b.define_function(entry, a).unwrap();
    let p = b.build_verified().unwrap();
    let object = Value::Obj(Obj {
        host_type: HostTypeId(1),
        handle: ObjHandle(42),
    });
    let args = [object.clone()];
    let mut vm = Vm::new(NoHost, Limits::default());
    let mut group = c.benchmark_group("live_execution");
    group.bench_function("ordinary_run_1000", |b| {
        b.iter(|| {
            black_box(
                vm.run(&p, entry, black_box(&args), TraceMask::NONE, None)
                    .unwrap(),
            );
        })
    });
    let mut ctx = ExecutionContext::new();
    let mut outputs = Vec::new();
    group.bench_function("ordinary_reused_context_1000", |b| {
        b.iter(|| {
            outputs.clear();
            vm.run_with_ctx(
                &mut ctx,
                &p,
                entry,
                black_box(&args),
                &mut outputs,
                TraceMask::NONE,
                None,
                None,
            )
            .unwrap();
            black_box(&outputs);
        })
    });
    for quantum in [1, 64, u64::MAX] {
        group.bench_with_input(
            BenchmarkId::new("scheduled_1000", quantum),
            &quantum,
            |b, &quantum| {
                b.iter(|| {
                    let mut run = vm
                        .start(&p, entry, black_box(&args), InvocationOptions::default())
                        .unwrap();
                    while run.resume(quantum).unwrap() == RunStatus::Suspended {}
                    black_box(run.result().unwrap().unwrap());
                })
            },
        );
    }
    group.finish();

    // These retained nodes make object inventory cost visible without growing the heap per sample.
    vm.aggregates_mut().tuple_new(vec![object; 1024]).unwrap();
    let mut run = vm
        .start(&p, entry, &args, InvocationOptions::default())
        .unwrap();
    run.resume(100).unwrap();
    let mut group = c.benchmark_group("live_inspection");
    group.bench_function("frame_metadata", |b| {
        b.iter(|| {
            black_box(run.inspect().frames().next().unwrap());
        })
    });
    let id = run.inspect().frames().next().unwrap().activation;
    group.bench_function("fresh_register_query", |b| {
        b.iter(|| {
            black_box(run.inspect().register(&id, value).unwrap());
        })
    });
    let mut inspection = run.inspect();
    inspection.register(&id, value).unwrap();
    group.bench_function("cached_register_query", |b| {
        b.iter(|| {
            black_box(inspection.register(&id, value).unwrap());
        })
    });
    group.bench_function("cached_object_inventory_1024", |b| {
        b.iter(|| {
            let mut count = 0_usize;
            inspection.visit_objects(|root| {
                black_box(root);
                count += 1;
            });
            assert_eq!(count, 1026); // initialized receiver register + argument + retained fields
            black_box(count);
        })
    });
    group.finish();
}
criterion_group! {
    name = benches;
    config = Criterion::default().warm_up_time(Duration::from_millis(300)).measurement_time(Duration::from_millis(1200)).sample_size(30);
    targets = stepping
}
criterion_main!(benches);
