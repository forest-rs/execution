// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Keep scheduling, metadata, verifier-backed register inspection and object traversal distinct.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use execution_tape::value::FuncRef;
use execution_tape::{
    asm::{FunctionSig, ProgramBuilder, Reg},
    callable::Callable,
    host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef},
    program::{HostTypeId, ValueType},
    trace::TraceMask,
    value::{FuncId, Obj, ObjHandle, Value},
    vm::{Limits, RunStatus, TurnOptions, Vm},
};
use std::{hint::black_box, sync::Arc, time::Duration};

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
    a.ret(Reg::EFFECT, &[value]);
    b.define_function(entry, a).unwrap();
    let p = b.build_verified().unwrap();
    let object = Value::Obj(Obj {
        host_type: HostTypeId(1),
        handle: ObjHandle(42),
    });
    let args = [object.clone()];
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(p.clone()).unwrap();
    let mut group = c.benchmark_group("live_execution");
    group.bench_function("ordinary_run_1000", |b| {
        b.iter(|| {
            black_box(
                vm.run(
                    FuncRef::new(unit, entry),
                    black_box(&args),
                    TraceMask::NONE,
                    None,
                )
                .unwrap(),
            );
        })
    });
    let mut outputs = Vec::new();
    group.bench_function("ordinary_reused_context_1000", |b| {
        b.iter(|| {
            outputs.clear();
            vm.run_into(
                FuncRef::new(unit, entry),
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
                        .start(
                            FuncRef::new(unit, entry),
                            black_box(&args),
                            TurnOptions::default(),
                        )
                        .unwrap();
                    while run.drive(quantum).unwrap() == RunStatus::Suspended {}
                    black_box(run.result().unwrap().unwrap());
                })
            },
        );
    }
    group.finish();

    // Both sides create a fresh VM and own their inputs/code. `saved` also saves the
    // computation after its first instruction and resumes it, which isolates that round trip.
    let callable = Callable::function(Arc::new(p.clone()), entry).unwrap();
    let mut group = c.benchmark_group("saving");
    for saved in [false, true] {
        group.bench_function(if saved { "saved_once" } else { "unsaved" }, |b| {
            b.iter(|| {
                let mut fresh = Vm::new(NoHost, Limits::default());
                let unit = fresh.install(Arc::clone(callable.program())).unwrap();
                let mut invocation = fresh
                    .start(
                        callable.at(unit),
                        black_box(args.to_vec()),
                        TurnOptions::default(),
                    )
                    .unwrap();
                if saved {
                    assert_eq!(invocation.drive(1).unwrap(), RunStatus::Suspended);
                    let id = saved_id(invocation);
                    invocation = fresh.resume(id, TurnOptions::default()).unwrap();
                }
                assert_eq!(invocation.drive(u64::MAX).unwrap(), RunStatus::Complete);
                black_box(invocation.result().unwrap().unwrap());
            });
        });
    }
    group.finish();

    // These retained nodes make object inventory cost visible without growing the heap per sample.
    vm.aggregates_mut().tuple_new(vec![object; 1024]).unwrap();
    let mut run = vm
        .start(FuncRef::new(unit, entry), &args, TurnOptions::default())
        .unwrap();
    run.drive(100).unwrap();
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

fn callback_free_calls(c: &mut Criterion) {
    let mut b = ProgramBuilder::new();
    let caller = b
        .declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let callee = b
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let mut a = b.function_asm(caller).unwrap();
    let value = a.alloc_register(ValueType::I64);
    a.const_i64(value, 42);
    for _ in 0..1000 {
        a.call(Reg::EFFECT, callee, Reg::EFFECT, &[value], &[value]);
    }
    a.ret(Reg::EFFECT, &[value]);
    b.define_function(caller, a).unwrap();
    let mut a = b.function_asm(callee).unwrap();
    a.ret(Reg::EFFECT, &[Reg(1)]);
    b.define_function(callee, a).unwrap();
    let program = b.build_verified().unwrap();
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(program.clone()).unwrap();
    let mut group = c.benchmark_group("callback_free_calls");
    group.bench_function("ordinary_1000", |b| {
        b.iter(|| {
            black_box(
                vm.run(FuncRef::new(unit, caller), &[], TraceMask::NONE, None)
                    .unwrap(),
            );
        })
    });
    group.bench_function("scheduled_1000", |b| {
        b.iter(|| {
            let mut invocation = vm
                .start(FuncRef::new(unit, caller), &[], TurnOptions::default())
                .unwrap();
            assert_eq!(invocation.drive(u64::MAX).unwrap(), RunStatus::Complete);
            black_box(invocation.result().unwrap().unwrap());
        })
    });
    group.finish();
}

/// A chain of `depth` distinct functions, each paused at its call, under a leaf host call.
///
/// Every function has `padding + 9` registers of mixed kinds, a branch diamond before its call and
/// one register first written after it, so frames have both initialized and unavailable values.
fn call_chain(depth: u32, padding: usize) -> execution_tape::verifier::VerifiedProgram {
    let mut b = ProgramBuilder::new();
    let sig = FunctionSig {
        arg_types: vec![ValueType::I64],
        ret_types: vec![ValueType::I64],
    };
    let functions: Vec<FuncId> = (0..depth)
        .map(|_| b.declare_function(sig.clone()).unwrap())
        .collect();
    let probe = b.host_sig_for(
        "probe",
        HostSig {
            args: vec![ValueType::I64],
            rets: vec![ValueType::I64],
        },
    );
    let text = b.const_str("frame");
    for (index, &function) in functions.iter().enumerate() {
        let mut a = b.function_asm(function).unwrap();
        let x = Reg(1);
        let k = a.alloc_register(ValueType::I64);
        let y = a.alloc_register(ValueType::I64);
        let flag = a.alloc_register(ValueType::Bool);
        let real = a.alloc_register(ValueType::F64);
        let name = a.alloc_register(ValueType::Str);
        let result = a.alloc_register(ValueType::I64);
        let after = a.alloc_register(ValueType::I64);
        let pad: Vec<Reg> = (0..padding)
            .map(|_| a.alloc_register(ValueType::I64))
            .collect();
        let (left, right, join) = (a.label(), a.label(), a.label());
        a.const_i64(k, index as i64)
            .i64_add(y, x, k)
            .const_f64(real, 0.5)
            .const_pool(name, text);
        for &p in &pad {
            a.i64_add(p, y, k);
        }
        a.i64_lt(flag, x, k).br(flag, left, right);
        a.place(left).unwrap();
        a.i64_add(y, y, k).jmp(join);
        a.place(right).unwrap();
        a.i64_add(y, y, x).jmp(join);
        a.place(join).unwrap();
        match functions.get(index + 1) {
            Some(&next) => a.call(Reg::EFFECT, next, Reg::EFFECT, &[y], &[result]),
            None => a.host_call(Reg::EFFECT, probe, Reg::EFFECT, &[y], &[result]),
        };
        a.i64_add(after, result, k).ret(Reg::EFFECT, &[after]);
        b.define_function(function, a).unwrap();
    }
    b.build_verified().unwrap()
}

/// Snapshots every frame and its available registers, as a debugger or condition chooser does.
fn frame_snapshots(c: &mut Criterion) {
    let mut group = c.benchmark_group("frame_snapshot");
    for (depth, padding) in [(1, 0), (10, 0), (10, 32)] {
        let program = call_chain(depth, padding);
        let mut vm = Vm::new(NoHost, Limits::default());
        let unit = vm.install(program).unwrap();
        let mut run = vm
            .start(
                FuncRef::new(unit, FuncId(0)),
                &[Value::I64(1)],
                TurnOptions::default().yield_host_calls(),
            )
            .unwrap();
        assert_eq!(run.drive(u64::MAX).unwrap(), RunStatus::AwaitingHost);
        let snapshot = |inspection: &mut execution_tape::vm::ComputationInspection<'_>| {
            let frames: Vec<_> = inspection.frames().collect();
            frames
                .into_iter()
                .map(|frame| {
                    inspection
                        .registers(&frame.activation)
                        .unwrap()
                        .filter_map(|register| register.value.map(|value| value.to_value()))
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>()
        };
        let expected = snapshot(&mut run.inspect());
        assert_eq!(expected.len(), depth as usize);
        // r0, x, k, y, flag, real, name and the padding; `result` and `after` are not yet written.
        assert!(expected.iter().all(|frame| frame.len() == 7 + padding));
        let case = format!("{depth}x{}", padding + 9);
        group.bench_function(BenchmarkId::new("fresh_view", &case), |b| {
            b.iter(|| black_box(snapshot(&mut run.inspect())));
        });
        let mut inspection = run.inspect();
        group.bench_function(BenchmarkId::new("reused_view", &case), |b| {
            b.iter(|| black_box(snapshot(&mut inspection)));
        });
    }
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default().warm_up_time(Duration::from_millis(300)).measurement_time(Duration::from_millis(1200)).sample_size(30);
    targets = stepping, callback_free_calls, frame_snapshots
}
criterion_main!(benches);

/// Finishes `turn`, whose computation must still be running or waiting, and takes its ID.
#[track_caller]
fn saved_id<H: execution_tape::host::Host>(
    turn: execution_tape::vm::Turn<'_, H>,
) -> execution_tape::vm::ComputationId {
    match turn.finish() {
        Ok(execution_tape::vm::TurnOutcome::Saved(id)) => id,
        other => panic!("the computation was not saved: {other:?}"),
    }
}
