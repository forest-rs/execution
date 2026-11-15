// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Explicit collection: reclaiming garbage from flat and shared (DAG) heaps, retracing a heap with
//! none, and collecting while an invocation waits at a host request with live frames, plain or
//! holding strings, exception references and held continuations.
//!
//! Heaps and paused invocations are built outside the timed region; only `collect` is timed.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use execution_tape::asm::{Catch, FunctionAbi, FunctionSig, ProgramBuilder, Reg};
use execution_tape::contract::Contract;
use execution_tape::host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef};
use execution_tape::program::ValueType;
use execution_tape::value::{FuncId, FuncRef, Value};
use execution_tape::verifier::VerifiedProgram;
use execution_tape::vm::{Invocation, InvocationOptions, Limits, RootedValues, RunStatus, Vm};
use std::{
    hint::black_box,
    sync::Arc,
    time::{Duration, Instant},
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

/// A VM holding `count` small tuples, every `stride`th of them registered in one group.
fn heap(count: u32, stride: u32) -> (Vm<NoHost>, RootedValues) {
    let mut vm = Vm::new(NoHost, Limits::default());
    let heap = vm.aggregates_mut();
    let mut kept = Vec::new();
    for i in 0..count {
        let tuple = heap
            .tuple_new(vec![Value::I64(i64::from(i)), Value::Str("payload".into())])
            .unwrap();
        if i % stride == 0 {
            kept.push(Value::Agg(tuple));
        }
    }
    let root = vm.root(kept).unwrap();
    (vm, root)
}

/// A VM holding a DAG of `count` tuples, each pointing at its predecessor and at the node
/// halfway back, so most nodes are shared; between them sits one unreachable tuple each.
fn dag(count: u32) -> (Vm<NoHost>, RootedValues) {
    let mut vm = Vm::new(NoHost, Limits::default());
    let heap = vm.aggregates_mut();
    let mut nodes = vec![heap.tuple_new(vec![]).unwrap()];
    for i in 1..count as usize {
        heap.tuple_new(vec![Value::I64(i as i64)]).unwrap();
        let node = heap
            .tuple_new(vec![Value::Agg(nodes[i - 1]), Value::Agg(nodes[i / 2])])
            .unwrap();
        nodes.push(node);
    }
    let root = vm.root(vec![Value::Agg(*nodes.last().unwrap())]).unwrap();
    (vm, root)
}

fn idle(c: &mut Criterion) {
    let mut group = c.benchmark_group("collection");
    for count in [1_000_u32, 10_000, 100_000] {
        group.bench_function(BenchmarkId::new("reclaim_half", count), |b| {
            b.iter_custom(|iterations| {
                let mut elapsed = Duration::ZERO;
                for _ in 0..iterations {
                    let (mut vm, root) = heap(count, 2);
                    let start = Instant::now();
                    let report = black_box(vm.collect().unwrap());
                    elapsed += start.elapsed();
                    assert_eq!(report.aggregates.reclaimed, u64::from(count / 2));
                    drop(root);
                }
                elapsed
            });
        });
        group.bench_function(BenchmarkId::new("dag_half_live", count), |b| {
            b.iter_custom(|iterations| {
                let mut elapsed = Duration::ZERO;
                for _ in 0..iterations {
                    let (mut vm, root) = dag(count);
                    let start = Instant::now();
                    let report = black_box(vm.collect().unwrap());
                    elapsed += start.elapsed();
                    assert_eq!(report.aggregates.live, u64::from(count));
                    drop(root);
                }
                elapsed
            });
        });
        // Nothing to reclaim: the cost of tracing a heap that is all live, which moves nothing.
        let (mut vm, _root) = heap(count, 1);
        group.bench_function(BenchmarkId::new("all_live", count), |b| {
            b.iter(|| black_box(vm.collect().unwrap()));
        });
    }
    group.finish();
}

/// `descend(n, acc)` holds `acc`, a fresh `(n, acc)` and an overwritten garbage tuple in each
/// frame, recursing until `n` is zero, where it waits at `pause`.
fn recursion() -> (Arc<VerifiedProgram>, FuncId) {
    let mut b = ProgramBuilder::new();
    let pause = b.host_sig_for(
        "pause",
        HostSig {
            args: vec![],
            rets: vec![],
        },
    );
    let descend = b
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::I64, ValueType::Agg],
            ret_types: vec![],
        })
        .unwrap();
    let mut a = b.function_asm(descend).unwrap();
    let (n, acc) = (
        b.argument(descend, 0).unwrap(),
        b.argument(descend, 1).unwrap(),
    );
    let (zero, one, done, next, deeper, garbage) = (
        a.alloc_register(ValueType::I64),
        a.alloc_register(ValueType::I64),
        a.alloc_register(ValueType::Bool),
        a.alloc_register(ValueType::I64),
        a.alloc_register(ValueType::Agg),
        a.alloc_register(ValueType::Agg),
    );
    let (bottom, recurse) = (a.label(), a.label());
    a.const_i64(zero, 0)
        .const_i64(one, 1)
        .tuple_new(garbage, &[n])
        .tuple_new(garbage, &[])
        .i64_eq(done, n, zero)
        .br(done, bottom, recurse);
    a.place(recurse).unwrap();
    a.i64_sub(next, n, one).tuple_new(deeper, &[n, acc]);
    let [] = b.emit_call(&mut a, descend, &[next, deeper]).unwrap();
    a.ret(Reg::EFFECT, &[]);
    a.place(bottom).unwrap();
    a.host_call_sig_id(Reg::EFFECT, pause, Reg::EFFECT, &[], &[])
        .ret(Reg::EFFECT, &[]);
    b.define_function(descend, a).unwrap();
    (Arc::new(b.build_verified().unwrap()), descend)
}

/// Like [`recursion`], but every frame also holds an owned string, an exception reference with a
/// tuple payload, and an unstarted continuation over a closure whose environment is a tuple.
fn rich_recursion() -> (Arc<VerifiedProgram>, FuncId) {
    let mut b = ProgramBuilder::new();
    let pause = b.host_sig_for(
        "pause",
        HostSig {
            args: vec![],
            rets: vec![],
        },
    );
    let integer = b.contract(Contract::Kind(ValueType::I64)).unwrap();
    let boxed = b.contract(Contract::Tuple(&[Some(integer)])).unwrap();
    let failed = b.declare_tag("failed", &[boxed.into()]).unwrap();
    let thunk = b
        .call_sig(FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let text = b.const_str("payload");
    let body = b
        .declare_function(FunctionAbi {
            args: vec![boxed.into()],
            rets: vec![integer.into()],
        })
        .unwrap();
    let raise = b
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let descend = b
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![],
        })
        .unwrap();

    let mut a = b.function_asm(body).unwrap();
    let n = a.alloc_register(ValueType::I64);
    a.tuple_get(n, b.argument(body, 0).unwrap(), 0)
        .ret(Reg::EFFECT, &[n]);
    b.define_function(body, a).unwrap();

    let mut a = b.function_asm(raise).unwrap();
    let payload = a.alloc_register(ValueType::Agg);
    a.tuple_new(payload, &[b.argument(raise, 0).unwrap()]);
    b.emit_throw(&mut a, failed, &[payload]).unwrap();
    b.define_function(raise, a).unwrap();

    let mut a = b.function_asm(descend).unwrap();
    let n = b.argument(descend, 0).unwrap();
    let (literal, owned, garbage, env, function, closure, cont, caught) = (
        a.alloc_register(ValueType::Str),
        a.alloc_register(ValueType::Str),
        a.alloc_register(ValueType::Agg),
        a.alloc_register(ValueType::Agg),
        a.alloc_register(ValueType::Func),
        a.alloc_register(ValueType::Closure),
        a.alloc_register(ValueType::Cont),
        a.alloc_register(ValueType::Exn),
    );
    let (zero, one, done, next) = (
        a.alloc_register(ValueType::I64),
        a.alloc_register(ValueType::I64),
        a.alloc_register(ValueType::Bool),
        a.alloc_register(ValueType::I64),
    );
    a.const_pool(literal, text)
        .str_concat(owned, literal, literal)
        .tuple_new(garbage, &[])
        .tuple_new(garbage, &[n])
        .tuple_new(env, &[n])
        .const_func(function, body)
        .closure_new(closure, function, env)
        .cont_new(cont, thunk, closure);
    let handler = a.block(&[caught]);
    a.begin_region(&[Catch::AllRef(handler)]);
    let [_] = b.emit_call(&mut a, raise, &[n]).unwrap();
    a.end_region();
    a.trap(1);
    a.place(handler).unwrap();
    let (bottom, recurse) = (a.label(), a.label());
    a.const_i64(zero, 0)
        .const_i64(one, 1)
        .i64_eq(done, n, zero)
        .br(done, bottom, recurse);
    a.place(recurse).unwrap();
    a.i64_sub(next, n, one);
    let [] = b.emit_call(&mut a, descend, &[next]).unwrap();
    a.ret(Reg::EFFECT, &[]);
    a.place(bottom).unwrap();
    a.host_call_sig_id(Reg::EFFECT, pause, Reg::EFFECT, &[], &[])
        .ret(Reg::EFFECT, &[]);
    b.define_function(descend, a).unwrap();
    (Arc::new(b.build_verified().unwrap()), descend)
}

/// Starts `entry` with `args` and returns once it waits at its host request.
fn waiting<'vm>(
    vm: &'vm mut Vm<NoHost>,
    program: &Arc<VerifiedProgram>,
    entry: FuncId,
    args: &'vm [Value],
) -> Invocation<'vm, NoHost> {
    let unit = vm.install(Arc::clone(program)).unwrap();
    let options = InvocationOptions::default().yield_host_calls();
    let mut run = vm.start(FuncRef::new(unit, entry), args, options).unwrap();
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    run
}

fn paused(c: &mut Criterion) {
    let (rich, rich_entry) = rich_recursion();
    let mut group = c.benchmark_group("collection_paused");
    for depth in [16_i64, 256] {
        group.bench_function(BenchmarkId::new("rich_frames", depth), |b| {
            b.iter_custom(|iterations| {
                let mut elapsed = Duration::ZERO;
                for _ in 0..iterations {
                    let mut vm = Vm::new(
                        NoHost,
                        Limits {
                            max_call_depth: 4096,
                            ..Limits::default()
                        },
                    );
                    let args = [Value::I64(depth)];
                    let mut run = waiting(&mut vm, &rich, rich_entry, &args);
                    let start = Instant::now();
                    let report = black_box(run.collect().unwrap());
                    elapsed += start.elapsed();
                    assert_eq!(report.aggregates.reclaimed, depth as u64 + 1);
                }
                elapsed
            });
        });
    }
    group.finish();
    let (program, descend) = recursion();
    let mut group = c.benchmark_group("collection_paused");
    for depth in [16_i64, 256] {
        group.bench_function(BenchmarkId::new("frames", depth), |b| {
            b.iter_custom(|iterations| {
                let mut elapsed = Duration::ZERO;
                for _ in 0..iterations {
                    let mut vm = Vm::new(
                        NoHost,
                        Limits {
                            max_call_depth: 1024,
                            ..Limits::default()
                        },
                    );
                    let unit = vm.install(Arc::clone(&program)).unwrap();
                    let empty = vm.aggregates_mut().tuple_new(vec![]).unwrap();
                    let args = [Value::I64(depth), Value::Agg(empty)];
                    let options = InvocationOptions::default().yield_host_calls();
                    let mut run = vm
                        .start(FuncRef::new(unit, descend), &args, options)
                        .unwrap();
                    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
                    let start = Instant::now();
                    let report = black_box(run.collect().unwrap());
                    elapsed += start.elapsed();
                    assert_eq!(report.aggregates.reclaimed, depth as u64 + 1);
                }
                elapsed
            });
        });
    }
    group.finish();
}

criterion_group! {
    name=benches;
    config=Criterion::default().warm_up_time(Duration::from_millis(300)).measurement_time(Duration::from_millis(1500)).sample_size(30);
    targets=idle, paused
}
criterion_main!(benches);
