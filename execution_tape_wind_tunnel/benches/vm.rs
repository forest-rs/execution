// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use execution_tape::value::FuncRef;
use std::{hint::black_box, sync::Arc};

use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder, Reg};
use execution_tape::host::{
    Host, HostContext, HostError, HostSig, SigHash, ValueRef, sig_hash as sig_hash_fn,
};
use execution_tape::program::Pc;
use execution_tape::program::Program;
use execution_tape::program::{Const, SpanId, ValueType};
use execution_tape::trace::{ScopeKind, TraceMask, TraceOutcome, TraceSink};
use execution_tape::value::{FuncId, Value};
use execution_tape::vm::{Limits, Vm};

fn bench_vm(c: &mut Criterion) {
    bench_run_memory(c);
    bench_payloads(c);
    bench_run_results(c);
    bench_i64_add_chain(c);
    bench_i64_add_chain_traced_instr(c);
    bench_i64_add_chain_traced_instr_span_dense(c);
    bench_i64_add_chain_traced_instr_span_sparse(c);
    bench_i64_add_chain_no_trace_span_dense(c);
    bench_bytes_const_len(c);
    bench_str_const_len(c);
    bench_call_overhead(c);
    bench_call_loop(c);
    bench_call_indirect_loop(c);
    bench_branch_hot_loop(c);
    bench_host_call(c);
    bench_host_call_loop(c);
    bench_host_call_traced_run(c);
    bench_host_call_traced_instr(c);
    bench_host_call_traced_host(c);
}

fn bench_run_memory(c: &mut Criterion) {
    for complete in [false, true] {
        let mut group = c.benchmark_group(if complete {
            "run_memory_complete"
        } else {
            "run_memory"
        });
        for mode in ["scalar", "retained_input", "trap", "new_root", "new_chain"] {
            let mut pb = ProgramBuilder::new();
            let mut asm = Asm::new();
            asm.set_register_types(vec![
                ValueType::Unit,
                ValueType::Agg,
                ValueType::I64,
                ValueType::Agg,
            ]);
            asm.const_i64(Reg(2), 42);
            for i in 0..32 {
                let field = if mode == "new_chain" {
                    if i == 0 { 1 } else { 3 }
                } else {
                    2
                };
                asm.tuple_new(Reg(3), &[Reg(field)]);
            }
            if mode == "trap" {
                asm.trap(7);
            } else {
                asm.ret(
                    Reg::EFFECT,
                    &[Reg(match mode {
                        "scalar" => 2,
                        "new_root" | "new_chain" => 3,
                        _ => 1,
                    })],
                );
            }
            let entry = pb
                .push_function_checked(
                    asm,
                    FunctionSig {
                        arg_types: vec![ValueType::Agg],
                        ret_types: vec![if mode == "scalar" {
                            ValueType::I64
                        } else {
                            ValueType::Agg
                        }],
                    },
                )
                .unwrap();
            let program = Arc::new(pb.build_verified().unwrap());
            group.bench_function(mode, |b| {
                b.iter_batched(
                    || {
                        let mut vm = Vm::new(NopHost, wide_open_limits());
                        let unit = vm.install(Arc::clone(&program)).unwrap();
                        let root = vm.aggregates_mut().tuple_new(vec![Value::I64(1)]).unwrap();
                        ((vm, unit), vec![Value::Agg(root)], Vec::with_capacity(1))
                    },
                    |((mut vm, unit), args, mut outputs)| {
                        for _ in 0..100 {
                            outputs.clear();
                            let result = vm.run_into(
                                FuncRef::new(unit, entry),
                                &args,
                                &mut outputs,
                                TraceMask::NONE,
                                None,
                                None,
                            );
                            assert_eq!(result.is_err(), mode == "trap");
                        }
                        black_box(vm.aggregates().len_u32());
                        if complete {
                            // Include reclamation and final retained-state destruction in this window.
                            drop((vm, args, outputs));
                            None
                        } else {
                            // Keep the execution-only window comparable with existing measurements.
                            Some((vm, args, outputs))
                        }
                    },
                    criterion::BatchSize::SmallInput,
                )
            });
        }
        group.finish();
    }
}

fn bench_payloads(c: &mut Criterion) {
    let mut group = c.benchmark_group("payload");
    for size in [32, 65_536] {
        for text in [false, true] {
            let ty = if text {
                ValueType::Str
            } else {
                ValueType::Bytes
            };
            let kind = if text { "str" } else { "bytes" };
            let value = if text {
                Value::Str("x".repeat(size))
            } else {
                Value::Bytes(vec![42; size])
            };
            for source in ["constant", "input", "tuple", "host"] {
                for length_only in [false, true] {
                    let mut pb = ProgramBuilder::new();
                    let mut asm = Asm::new();
                    let mut inputs = Vec::new();
                    let mut arg_types = Vec::new();
                    if source == "input" {
                        inputs.push(value.clone());
                        arg_types.push(ty);
                    } else if source == "host" {
                        let hs = pb.host_sig_for(
                            "payload",
                            HostSig {
                                args: vec![],
                                rets: vec![ty],
                            },
                        );
                        asm.host_call_sig_id(Reg::EFFECT, hs, Reg::EFFECT, &[], &[Reg(1)]);
                    } else {
                        let id = match &value {
                            Value::Bytes(b) => pb.const_bytes(b),
                            Value::Str(s) => pb.const_str(s),
                            _ => unreachable!(),
                        };
                        asm.const_pool(Reg(1), id);
                    }
                    let reg = if source == "tuple" {
                        asm.tuple_new(Reg(2), &[Reg(1)]);
                        asm.tuple_get(Reg(3), Reg(2), 0);
                        3
                    } else {
                        1
                    };
                    let result = if length_only {
                        if text {
                            asm.str_len(Reg(4), Reg(reg));
                        } else {
                            asm.bytes_len(Reg(4), Reg(reg));
                        }
                        4
                    } else {
                        reg
                    };
                    let mut kinds = vec![ValueType::Unit; result as usize + 1];
                    kinds[1] = ty;
                    if source == "tuple" {
                        kinds[2] = ValueType::Agg;
                        kinds[3] = ty;
                    }
                    if length_only {
                        kinds[4] = ValueType::U64;
                    }
                    asm.set_register_types(kinds);
                    asm.ret(Reg::EFFECT, &[Reg(result)]);
                    let entry = pb
                        .push_function_checked(
                            asm,
                            FunctionSig {
                                arg_types,
                                ret_types: vec![if length_only { ValueType::U64 } else { ty }],
                            },
                        )
                        .unwrap();
                    let p = pb.build_verified().unwrap();
                    let mut vm = Vm::new(PayloadHost { size, text }, wide_open_limits());
                    let unit = vm.install(p.clone()).unwrap();
                    let mut outputs = Vec::with_capacity(1);
                    let operation = if length_only { "len" } else { "return" };
                    group.bench_function(
                        BenchmarkId::new(format!("{kind}_{source}_{operation}"), size),
                        |b| {
                            b.iter(|| {
                                outputs.clear();
                                vm.run_into(
                                    FuncRef::new(unit, entry),
                                    &inputs,
                                    &mut outputs,
                                    TraceMask::NONE,
                                    None,
                                    None,
                                )
                                .unwrap();
                                black_box(&outputs);
                                // These fixtures return owned payloads/scalars, never heap handles.
                                vm.aggregates_mut().clear();
                            })
                        },
                    );
                }
            }
        }
    }
    group.finish();
}

struct PayloadHost {
    size: usize,
    text: bool,
}
impl Host for PayloadHost {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        _: &[ValueRef<'_>],
        rets: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        rets[0] = if self.text {
            Value::Str("x".repeat(self.size))
        } else {
            Value::Bytes(vec![42; self.size])
        };
        Ok(0)
    }
}

fn bench_run_results(c: &mut Criterion) {
    let mut group = c.benchmark_group("run_results");
    for count in [1_u32, 16, 64] {
        let mut asm = Asm::new();
        let mut kinds = vec![ValueType::I64; count as usize + 1];
        kinds[0] = ValueType::Unit;
        asm.set_register_types(kinds);
        for reg in 1..=count {
            asm.const_i64(Reg(reg), i64::from(reg));
        }
        let regs: Vec<_> = (1..=count).map(Reg).collect();
        asm.ret(Reg::EFFECT, &regs);
        let mut pb = ProgramBuilder::new();
        pb.push_function_checked(
            asm,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64; count as usize],
            },
        )
        .unwrap();
        let p = pb.build_verified().unwrap();
        let mut vm = Vm::new(NopHost, wide_open_limits());
        let unit = vm.install(p.clone()).unwrap();
        group.bench_function(BenchmarkId::new("fresh", count), |b| {
            b.iter(|| {
                black_box(
                    vm.run(FuncRef::new(unit, FuncId(0)), &[], TraceMask::NONE, None)
                        .unwrap(),
                );
            })
        });
        group.bench_function(BenchmarkId::new("context", count), |b| {
            b.iter(|| {
                let mut outputs = Vec::new();
                vm.run_into(
                    FuncRef::new(unit, FuncId(0)),
                    &[],
                    &mut outputs,
                    TraceMask::NONE,
                    None,
                    None,
                )
                .unwrap();
                black_box(outputs);
            })
        });
        let mut outputs = Vec::with_capacity(count as usize);
        group.bench_function(BenchmarkId::new("context_and_results", count), |b| {
            b.iter(|| {
                outputs.clear();
                vm.run_into(
                    FuncRef::new(unit, FuncId(0)),
                    &[],
                    &mut outputs,
                    TraceMask::NONE,
                    None,
                    None,
                )
                .unwrap();
                black_box(&outputs);
            })
        });
    }
    group.finish();
}

fn bench_i64_add_chain(c: &mut Criterion) {
    let mut group = c.benchmark_group("i64_add_chain");
    for &chain_len in &[10_u32, 50, 200, 1000] {
        let p = build_i64_add_chain(chain_len);
        let mut vm = Vm::new(NopHost, wide_open_limits());
        let unit = vm.install(p.clone()).unwrap();
        group.bench_with_input(BenchmarkId::from_parameter(chain_len), &p, |b, _| {
            b.iter(|| {
                let out = vm
                    .run(FuncRef::new(unit, FuncId(0)), &[], TraceMask::NONE, None)
                    .unwrap();
                black_box(out);
            });
        });
    }
    group.finish();
}

fn bench_i64_add_chain_traced_instr(c: &mut Criterion) {
    let mut group = c.benchmark_group("i64_add_chain_traced_instr");
    for &chain_len in &[10_u32, 50, 200] {
        let p = build_i64_add_chain(chain_len);
        let mut vm = Vm::new(NopHost, wide_open_limits());
        let unit = vm.install(p.clone()).unwrap();
        let mut sink = CountingInstr::default();
        let mask = sink.mask();
        group.bench_with_input(BenchmarkId::from_parameter(chain_len), &p, |b, _| {
            b.iter(|| {
                let out = vm
                    .run(FuncRef::new(unit, FuncId(0)), &[], mask, Some(&mut sink))
                    .unwrap();
                black_box(out);
            });
        });
    }
    group.finish();
}

fn bench_i64_add_chain_traced_instr_span_dense(c: &mut Criterion) {
    let mut group = c.benchmark_group("i64_add_chain_traced_instr_span_dense");
    for &chain_len in &[10_u32, 50, 200] {
        let p = build_i64_add_chain_with_spans(chain_len, 1);
        let mut vm = Vm::new(NopHost, wide_open_limits());
        let unit = vm.install(p.clone()).unwrap();
        let mut sink = CountingInstr::default();
        let mask = sink.mask();
        group.bench_with_input(BenchmarkId::from_parameter(chain_len), &p, |b, _| {
            b.iter(|| {
                let out = vm
                    .run(FuncRef::new(unit, FuncId(0)), &[], mask, Some(&mut sink))
                    .unwrap();
                black_box(out);
            });
        });
    }
    group.finish();
}

fn bench_i64_add_chain_traced_instr_span_sparse(c: &mut Criterion) {
    let mut group = c.benchmark_group("i64_add_chain_traced_instr_span_sparse");
    for &chain_len in &[10_u32, 50, 200] {
        let p = build_i64_add_chain_with_spans(chain_len, 16);
        let mut vm = Vm::new(NopHost, wide_open_limits());
        let unit = vm.install(p.clone()).unwrap();
        let mut sink = CountingInstr::default();
        let mask = sink.mask();
        group.bench_with_input(BenchmarkId::from_parameter(chain_len), &p, |b, _| {
            b.iter(|| {
                let out = vm
                    .run(FuncRef::new(unit, FuncId(0)), &[], mask, Some(&mut sink))
                    .unwrap();
                black_box(out);
            });
        });
    }
    group.finish();
}

fn bench_i64_add_chain_no_trace_span_dense(c: &mut Criterion) {
    let mut group = c.benchmark_group("i64_add_chain_no_trace_span_dense");
    for &chain_len in &[10_u32, 50, 200, 1000] {
        let p = build_i64_add_chain_with_spans(chain_len, 1);
        let mut vm = Vm::new(NopHost, wide_open_limits());
        let unit = vm.install(p.clone()).unwrap();
        group.bench_with_input(BenchmarkId::from_parameter(chain_len), &p, |b, _| {
            b.iter(|| {
                let out = vm
                    .run(FuncRef::new(unit, FuncId(0)), &[], TraceMask::NONE, None)
                    .unwrap();
                black_box(out);
            });
        });
    }
    group.finish();
}

fn bench_bytes_const_len(c: &mut Criterion) {
    let mut group = c.benchmark_group("bytes_const_len");
    for &n in &[0_usize, 32, 1024, 4096] {
        let p = build_bytes_const_len(n);
        let mut vm = Vm::new(NopHost, wide_open_limits());
        let unit = vm.install(p.clone()).unwrap();
        group.bench_with_input(BenchmarkId::from_parameter(n), &p, |b, _| {
            b.iter(|| {
                let out = vm
                    .run(FuncRef::new(unit, FuncId(0)), &[], TraceMask::NONE, None)
                    .unwrap();
                black_box(out);
            });
        });
    }
    group.finish();
}

fn bench_str_const_len(c: &mut Criterion) {
    let mut group = c.benchmark_group("str_const_len");
    for &n in &[0_usize, 32, 1024, 4096] {
        let p = build_str_const_len(n);
        let mut vm = Vm::new(NopHost, wide_open_limits());
        let unit = vm.install(p.clone()).unwrap();
        group.bench_with_input(BenchmarkId::from_parameter(n), &p, |b, _| {
            b.iter(|| {
                let out = vm
                    .run(FuncRef::new(unit, FuncId(0)), &[], TraceMask::NONE, None)
                    .unwrap();
                black_box(out);
            });
        });
    }
    group.finish();
}

fn bench_call_overhead(c: &mut Criterion) {
    let p = build_call_overhead();
    let mut vm = Vm::new(NopHost, wide_open_limits());
    let unit = vm.install(p.clone()).unwrap();

    c.bench_function("call_overhead_one_call", |b| {
        b.iter(|| {
            let out = vm
                .run(FuncRef::new(unit, FuncId(0)), &[], TraceMask::NONE, None)
                .unwrap();
            black_box(out);
        });
    });
}

fn bench_call_loop(c: &mut Criterion) {
    let mut group = c.benchmark_group("call_loop");
    for &iters in &[10_u64, 100, 1000] {
        let p = build_call_loop(iters);
        let mut vm = Vm::new(NopHost, wide_open_limits());
        let unit = vm.install(p.clone()).unwrap();
        group.bench_with_input(BenchmarkId::from_parameter(iters), &p, |b, _| {
            b.iter(|| {
                let out = vm
                    .run(FuncRef::new(unit, FuncId(0)), &[], TraceMask::NONE, None)
                    .unwrap();
                black_box(out);
            });
        });
    }
    group.finish();
}

fn bench_call_indirect_loop(c: &mut Criterion) {
    let mut group = c.benchmark_group("call_indirect_loop");
    for &iters in &[100_u64, 1_000, 10_000] {
        let func_program = build_call_indirect_loop(iters, false);
        let closure_program = build_call_indirect_loop(iters, true);
        let mut func_vm = Vm::new(NopHost, wide_open_limits());
        let func_unit = func_vm.install(func_program.clone()).unwrap();
        let mut closure_vm = Vm::new(NopHost, wide_open_limits());
        let closure_unit = closure_vm.install(closure_program.clone()).unwrap();

        group.bench_with_input(BenchmarkId::new("func", iters), &func_program, |b, _| {
            b.iter(|| {
                let out = func_vm
                    .run(
                        FuncRef::new(func_unit, FuncId(0)),
                        &[],
                        TraceMask::NONE,
                        None,
                    )
                    .unwrap();
                black_box(out);
            });
        });
        group.bench_with_input(
            BenchmarkId::new("closure", iters),
            &closure_program,
            |b, _| {
                b.iter(|| {
                    let out = closure_vm
                        .run(
                            FuncRef::new(closure_unit, FuncId(0)),
                            &[],
                            TraceMask::NONE,
                            None,
                        )
                        .unwrap();
                    black_box(out);
                });
            },
        );
    }
    group.finish();
}

fn bench_branch_hot_loop(c: &mut Criterion) {
    let mut group = c.benchmark_group("branch_hot_loop");
    for &iters in &[100_u64, 1000, 10_000] {
        let p = build_branch_hot_loop(iters);
        let mut vm = Vm::new(NopHost, wide_open_limits());
        let unit = vm.install(p.clone()).unwrap();
        group.bench_with_input(BenchmarkId::from_parameter(iters), &p, |b, _| {
            b.iter(|| {
                let out = vm
                    .run(FuncRef::new(unit, FuncId(0)), &[], TraceMask::NONE, None)
                    .unwrap();
                black_box(out);
            });
        });
    }
    group.finish();
}

fn bench_host_call(c: &mut Criterion) {
    let p = build_host_call_overhead();
    let mut vm = Vm::new(IdentityHost, wide_open_limits());
    let unit = vm.install(p.clone()).unwrap();

    c.bench_function("host_call_overhead_one_call", |b| {
        b.iter(|| {
            let out = vm
                .run(FuncRef::new(unit, FuncId(0)), &[], TraceMask::NONE, None)
                .unwrap();
            black_box(out);
        });
    });
}

fn bench_host_call_loop(c: &mut Criterion) {
    let mut group = c.benchmark_group("host_call_loop");
    for &iters in &[10_u64, 100, 1000] {
        let p = build_host_call_loop(iters);
        let mut vm = Vm::new(IdentityHost, wide_open_limits());
        let unit = vm.install(p.clone()).unwrap();
        group.bench_with_input(BenchmarkId::from_parameter(iters), &p, |b, _| {
            b.iter(|| {
                let out = vm
                    .run(FuncRef::new(unit, FuncId(0)), &[], TraceMask::NONE, None)
                    .unwrap();
                black_box(out);
            });
        });
    }
    group.finish();
}

fn bench_host_call_traced_run(c: &mut Criterion) {
    let p = build_host_call_overhead();
    let mut vm = Vm::new(IdentityHost, wide_open_limits());
    let unit = vm.install(p.clone()).unwrap();
    let mut sink = CountingTrace::default();
    let mask = sink.mask();

    c.bench_function("host_call_traced_run_mask_run", |b| {
        b.iter(|| {
            let out = vm
                .run(FuncRef::new(unit, FuncId(0)), &[], mask, Some(&mut sink))
                .unwrap();
            black_box(out);
        });
    });
}

fn bench_host_call_traced_instr(c: &mut Criterion) {
    let p = build_host_call_overhead();
    let mut vm = Vm::new(IdentityHost, wide_open_limits());
    let unit = vm.install(p.clone()).unwrap();
    let mut sink = CountingInstr::default();
    let mask = sink.mask();

    c.bench_function("host_call_traced_instr_mask_instr", |b| {
        b.iter(|| {
            let out = vm
                .run(FuncRef::new(unit, FuncId(0)), &[], mask, Some(&mut sink))
                .unwrap();
            black_box(out);
        });
    });
}

fn bench_host_call_traced_host(c: &mut Criterion) {
    let p = build_host_call_overhead();
    let mut vm = Vm::new(IdentityHost, wide_open_limits());
    let unit = vm.install(p.clone()).unwrap();
    let mut sink = CountingHostScopes::default();
    let mask = sink.mask();

    c.bench_function("host_call_traced_host_mask_host", |b| {
        b.iter(|| {
            let out = vm
                .run(FuncRef::new(unit, FuncId(0)), &[], mask, Some(&mut sink))
                .unwrap();
            black_box(out);
        });
    });
}

fn build_i64_add_chain(chain_len: u32) -> execution_tape::verifier::VerifiedProgram {
    let mut a = Asm::new();
    let mut kinds = vec![ValueType::I64; chain_len as usize + 3];
    kinds[0] = ValueType::Unit;
    a.set_register_types(kinds);
    a.const_i64(Reg(1), 1);
    a.const_i64(Reg(2), 2);
    let mut cur = 3;
    for _ in 0..chain_len {
        a.i64_add(Reg(cur), Reg(cur - 1), Reg(2));
        cur += 1;
    }
    a.ret(Reg::EFFECT, &[Reg(cur - 1)]);

    let mut pb = ProgramBuilder::new();
    pb.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
        },
    )
    .unwrap();
    pb.build_verified().unwrap()
}

fn build_i64_add_chain_with_spans(
    chain_len: u32,
    span_every: u32,
) -> execution_tape::verifier::VerifiedProgram {
    assert_ne!(span_every, 0, "span_every must be non-zero");

    let mut a = Asm::new();
    let mut kinds = vec![ValueType::I64; chain_len as usize + 3];
    kinds[0] = ValueType::Unit;
    a.set_register_types(kinds);
    a.const_i64(Reg(1), 1);
    a.const_i64(Reg(2), 2);
    let mut cur = 3;
    let mut next_span_id = 1_u64;
    for i in 0..chain_len {
        if i % span_every == 0 {
            a.span(SpanId::new(next_span_id));
            next_span_id = next_span_id.saturating_add(1);
        }
        a.i64_add(Reg(cur), Reg(cur - 1), Reg(2));
        cur += 1;
    }
    a.ret(Reg::EFFECT, &[Reg(cur - 1)]);

    let mut pb = ProgramBuilder::new();
    pb.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
        },
    )
    .unwrap();
    pb.build_verified().unwrap()
}

fn build_bytes_const_len(n: usize) -> execution_tape::verifier::VerifiedProgram {
    let mut pb = ProgramBuilder::new();
    let bytes = pb.constant(Const::Bytes(vec![0_u8; n]));

    let mut a = Asm::new();
    a.set_register_types(vec![ValueType::Unit, ValueType::Bytes, ValueType::U64]);
    a.const_pool(Reg(1), bytes);
    a.bytes_len(Reg(2), Reg(1));
    a.ret(Reg::EFFECT, &[Reg(2)]);

    pb.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::U64],
        },
    )
    .unwrap();
    pb.build_verified().unwrap()
}

fn build_str_const_len(n: usize) -> execution_tape::verifier::VerifiedProgram {
    let mut pb = ProgramBuilder::new();
    let s = pb.constant(Const::Str("a".repeat(n)));

    let mut a = Asm::new();
    a.set_register_types(vec![ValueType::Unit, ValueType::Str, ValueType::U64]);
    a.const_pool(Reg(1), s);
    a.str_len(Reg(2), Reg(1));
    a.ret(Reg::EFFECT, &[Reg(2)]);

    pb.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::U64],
        },
    )
    .unwrap();
    pb.build_verified().unwrap()
}

fn build_call_overhead() -> execution_tape::verifier::VerifiedProgram {
    let mut pb = ProgramBuilder::new();
    let f0 = pb
        .declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let f1 = pb
        .declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();

    let mut a1 = Asm::new();
    a1.set_register_types(vec![
        ValueType::Unit,
        ValueType::I64,
        ValueType::I64,
        ValueType::I64,
    ]);
    a1.const_i64(Reg(1), 1);
    a1.const_i64(Reg(2), 2);
    a1.i64_add(Reg(3), Reg(1), Reg(2));
    a1.ret(Reg::EFFECT, &[Reg(3)]);
    pb.define_function(f1, a1).unwrap();

    let mut a0 = Asm::new();
    a0.set_register_types(vec![ValueType::Unit, ValueType::I64]);
    a0.call(Reg::EFFECT, f1, Reg::EFFECT, &[], &[Reg(1)]);
    a0.ret(Reg::EFFECT, &[Reg(1)]);
    pb.define_function(f0, a0).unwrap();

    pb.build_verified().unwrap()
}

fn build_call_loop(iters: u64) -> execution_tape::verifier::VerifiedProgram {
    let mut pb = ProgramBuilder::new();
    let f0 = pb
        .declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let f1 = pb
        .declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();

    let mut a1 = Asm::new();
    a1.set_register_types(vec![
        ValueType::Unit,
        ValueType::I64,
        ValueType::I64,
        ValueType::I64,
    ]);
    a1.const_i64(Reg(1), 1);
    a1.const_i64(Reg(2), 2);
    a1.i64_add(Reg(3), Reg(1), Reg(2));
    a1.ret(Reg::EFFECT, &[Reg(3)]);
    pb.define_function(f1, a1).unwrap();

    // r1: counter, r2: limit, r3: one, r4: sum, r5: cond, r6: tmp (call result)
    let mut a0 = Asm::new();
    a0.set_register_types(vec![
        ValueType::Unit,
        ValueType::U64,
        ValueType::U64,
        ValueType::U64,
        ValueType::I64,
        ValueType::Bool,
        ValueType::I64,
    ]);
    let l_loop = a0.label();
    let l_body = a0.label();
    let l_done = a0.label();

    a0.const_u64(Reg(1), 0);
    a0.const_u64(Reg(2), iters);
    a0.const_u64(Reg(3), 1);
    a0.const_i64(Reg(4), 0);

    a0.jmp(l_loop);
    a0.place(l_loop).unwrap();
    a0.u64_eq(Reg(5), Reg(1), Reg(2));
    a0.br(Reg(5), l_done, l_body);

    // body
    a0.place(l_body).unwrap();
    a0.call(Reg::EFFECT, f1, Reg::EFFECT, &[], &[Reg(6)]);
    a0.i64_add(Reg(4), Reg(4), Reg(6));
    a0.u64_add(Reg(1), Reg(1), Reg(3));
    a0.jmp(l_loop);

    a0.place(l_done).unwrap();
    a0.ret(Reg::EFFECT, &[Reg(4)]);

    pb.define_function(f0, a0).unwrap();
    pb.build_verified().unwrap()
}

fn build_call_indirect_loop(
    iters: u64,
    closure_target: bool,
) -> execution_tape::verifier::VerifiedProgram {
    let mut pb = ProgramBuilder::new();
    let call_sig = pb
        .call_sig(execution_tape::asm::FunctionSig {
            arg_types: vec![],
            ret_types: vec![],
        })
        .unwrap();

    let f0 = pb
        .declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::U64],
        })
        .unwrap();
    let f1 = pb
        .declare_function(FunctionSig {
            arg_types: if closure_target {
                vec![ValueType::Agg]
            } else {
                vec![]
            },
            ret_types: vec![],
        })
        .unwrap();

    let mut callee = Asm::new();
    let callee_kinds = if closure_target {
        &[ValueType::Unit, ValueType::Agg][..]
    } else {
        &[ValueType::Unit][..]
    };
    callee.set_register_types(callee_kinds.to_vec());
    callee.ret(Reg::EFFECT, &[]);
    pb.define_function(f1, callee).unwrap();

    // r1: counter, r2: limit, r3: one, r4: done?
    // r5: callee func register, r6: env aggregate, r7: callee closure register
    // r8: env payload constant
    let mut entry = Asm::new();
    let entry_kinds = if closure_target {
        &[
            ValueType::Unit,
            ValueType::U64,
            ValueType::U64,
            ValueType::U64,
            ValueType::Bool,
            ValueType::Func,
            ValueType::Agg,
            ValueType::Closure,
            ValueType::I64,
        ][..]
    } else {
        &[
            ValueType::Unit,
            ValueType::U64,
            ValueType::U64,
            ValueType::U64,
            ValueType::Bool,
            ValueType::Func,
        ][..]
    };
    entry.set_register_types(entry_kinds.to_vec());
    let l_loop = entry.label();
    let l_body = entry.label();
    let l_done = entry.label();

    entry.const_u64(Reg(1), 0);
    entry.const_u64(Reg(2), iters);
    entry.const_u64(Reg(3), 1);
    entry.const_func(Reg(5), f1);
    if closure_target {
        entry.const_i64(Reg(8), 7);
        entry.tuple_new(Reg(6), &[Reg(8)]);
        entry.closure_new(Reg(7), Reg(5), Reg(6));
    }

    entry.jmp(l_loop);
    entry.place(l_loop).unwrap();
    entry.u64_eq(Reg(4), Reg(1), Reg(2));
    entry.br(Reg(4), l_done, l_body);

    entry.place(l_body).unwrap();
    entry.call_indirect(
        Reg::EFFECT,
        call_sig,
        if closure_target { Reg(7) } else { Reg(5) },
        Reg::EFFECT,
        &[],
        &[],
    );
    entry.u64_add(Reg(1), Reg(1), Reg(3));
    entry.jmp(l_loop);

    entry.place(l_done).unwrap();
    entry.ret(Reg::EFFECT, &[Reg(1)]);
    pb.define_function(f0, entry).unwrap();

    pb.build_verified().unwrap()
}

fn build_branch_hot_loop(iters: u64) -> execution_tape::verifier::VerifiedProgram {
    let mut pb = ProgramBuilder::new();

    // r1: counter, r2: limit, r3: one, r4: zero
    // r5: done?, r6: parity, r7: even?, r8: accumulator
    let mut a = Asm::new();
    a.set_register_types(vec![
        ValueType::Unit,
        ValueType::U64,
        ValueType::U64,
        ValueType::U64,
        ValueType::U64,
        ValueType::Bool,
        ValueType::U64,
        ValueType::Bool,
        ValueType::I64,
    ]);
    let l_loop = a.label();
    let l_check = a.label();
    let l_even = a.label();
    let l_odd = a.label();
    let l_done = a.label();

    a.const_u64(Reg(1), 0);
    a.const_u64(Reg(2), iters);
    a.const_u64(Reg(3), 1);
    a.const_u64(Reg(4), 0);
    a.const_i64(Reg(8), 0);

    a.jmp(l_loop);
    a.place(l_loop).unwrap();
    a.u64_eq(Reg(5), Reg(1), Reg(2));
    a.br(Reg(5), l_done, l_check);

    a.place(l_check).unwrap();
    a.u64_and(Reg(6), Reg(1), Reg(3));
    a.u64_eq(Reg(7), Reg(6), Reg(4));
    a.br(Reg(7), l_even, l_odd);

    // Alternate branches hit this on even iterations.
    a.place(l_even).unwrap();
    a.i64_add(Reg(8), Reg(8), Reg(8));
    a.u64_add(Reg(1), Reg(1), Reg(3));
    a.jmp(l_loop);

    // Alternate branches hit this on odd iterations.
    a.place(l_odd).unwrap();
    a.i64_sub(Reg(8), Reg(8), Reg(8));
    a.u64_add(Reg(1), Reg(1), Reg(3));
    a.jmp(l_loop);

    a.place(l_done).unwrap();
    a.ret(Reg::EFFECT, &[Reg(8)]);

    pb.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
        },
    )
    .unwrap();
    pb.build_verified().unwrap()
}

fn build_host_call_overhead() -> execution_tape::verifier::VerifiedProgram {
    let sig = HostSig {
        args: vec![ValueType::I64],
        rets: vec![ValueType::I64],
    };
    let mut pb = ProgramBuilder::new();
    let host_sig = pb.host_sig_for("id", sig);

    let mut a = Asm::new();
    a.set_register_types(vec![ValueType::Unit, ValueType::I64, ValueType::I64]);
    a.const_i64(Reg(1), 9);
    a.host_call(Reg::EFFECT, host_sig, Reg::EFFECT, &[Reg(1)], &[Reg(2)]);
    a.ret(Reg::EFFECT, &[Reg(2)]);
    pb.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
        },
    )
    .unwrap();
    pb.build_verified().unwrap()
}

fn build_host_call_loop(iters: u64) -> execution_tape::verifier::VerifiedProgram {
    let sig = HostSig {
        args: vec![ValueType::I64],
        rets: vec![ValueType::I64],
    };
    let mut pb = ProgramBuilder::new();
    let host_sig = pb.host_sig_for("id", sig);

    // r1: counter, r2: limit, r3: one, r4: arg, r5: ret, r6: cond
    let mut a = Asm::new();
    a.set_register_types(vec![
        ValueType::Unit,
        ValueType::U64,
        ValueType::U64,
        ValueType::U64,
        ValueType::I64,
        ValueType::I64,
        ValueType::Bool,
    ]);
    let l_loop = a.label();
    let l_body = a.label();
    let l_done = a.label();

    a.const_u64(Reg(1), 0);
    a.const_u64(Reg(2), iters);
    a.const_u64(Reg(3), 1);
    a.const_i64(Reg(4), 9);
    a.const_i64(Reg(5), 0);

    a.jmp(l_loop);
    a.place(l_loop).unwrap();
    a.u64_eq(Reg(6), Reg(1), Reg(2));
    a.br(Reg(6), l_done, l_body);

    // body
    a.place(l_body).unwrap();
    a.host_call(Reg::EFFECT, host_sig, Reg::EFFECT, &[Reg(4)], &[Reg(5)]);
    a.u64_add(Reg(1), Reg(1), Reg(3));
    a.jmp(l_loop);

    a.place(l_done).unwrap();
    a.ret(Reg::EFFECT, &[Reg(5)]);

    pb.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
        },
    )
    .unwrap();
    pb.build_verified().unwrap()
}

fn wide_open_limits() -> Limits {
    Limits {
        fuel: u64::MAX,
        max_call_depth: 1024,
        max_frames: usize::MAX,
        max_saved_computations: usize::MAX,
        max_host_calls: u64::MAX,
        max_contract_work: u64::MAX,
        max_aggregate_work: u64::MAX,
        max_payload_work: u64::MAX,
    }
}

#[derive(Copy, Clone, Debug)]
struct NopHost;

impl Host for NopHost {
    fn call(
        &mut self,
        _symbol: &str,
        _sig_hash: SigHash,
        _args: &[ValueRef<'_>],
        _rets: &mut [Value],
        _ctx: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        Err(HostError::UnknownSymbol)
    }
}

#[derive(Copy, Clone, Debug)]
struct IdentityHost;

impl Host for IdentityHost {
    fn call(
        &mut self,
        symbol: &str,
        sig_hash: SigHash,
        args: &[ValueRef<'_>],
        rets: &mut [Value],
        _ctx: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        if symbol != "id" {
            return Err(HostError::UnknownSymbol);
        }
        let expected = sig_hash_fn(&HostSig {
            args: vec![ValueType::I64],
            rets: vec![ValueType::I64],
        });
        if sig_hash != expected {
            return Err(HostError::SignatureMismatch);
        }
        let Some(ValueRef::I64(x)) = args.first().copied() else {
            return Err(HostError::Failed);
        };
        rets[0] = Value::I64(x);
        Ok(0)
    }
}

#[derive(Default)]
struct CountingTrace {
    _count: u64,
}

impl TraceSink for CountingTrace {
    fn mask(&self) -> TraceMask {
        TraceMask::RUN
    }

    fn run_start(&mut self, _program: &Program, _entry: FuncRef, _arg_count: usize) {
        // Intentionally minimal: we want to measure VM overhead, not sink work.
        self._count = self._count.wrapping_add(1);
    }

    fn run_end(&mut self, _program: &Program, _outcome: TraceOutcome<'_>) {
        self._count = self._count.wrapping_add(1);
    }
}

#[derive(Default)]
struct CountingHostScopes {
    _count: u64,
}

impl TraceSink for CountingHostScopes {
    fn mask(&self) -> TraceMask {
        TraceMask::HOST
    }

    fn scope_enter(
        &mut self,
        _program: &Program,
        _kind: ScopeKind,
        _depth: usize,
        _location: execution_tape::trace::TraceLocation,
        _span_id: Option<SpanId>,
    ) {
        self._count = self._count.wrapping_add(1);
    }

    fn scope_exit(
        &mut self,
        _program: &Program,
        _kind: ScopeKind,
        _depth: usize,
        _location: execution_tape::trace::TraceLocation,
        _span_id: Option<SpanId>,
        _outcome: execution_tape::trace::TraceOutcome<'_>,
    ) {
        self._count = self._count.wrapping_add(1);
    }
}

#[derive(Default)]
struct CountingInstr {
    _count: u64,
}

impl TraceSink for CountingInstr {
    fn mask(&self) -> TraceMask {
        TraceMask::INSTR
    }

    fn instr(
        &mut self,
        _program: &Program,
        _location: execution_tape::trace::TraceLocation,
        _next_pc: Pc,
        _span_id: Option<SpanId>,
        _opcode: u16,
    ) {
        self._count = self._count.wrapping_add(1);
    }
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(std::time::Duration::from_millis(300))
        .measurement_time(std::time::Duration::from_millis(1200))
        .sample_size(60);
    targets = bench_vm
}
criterion_main!(benches);
