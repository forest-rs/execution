// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};

use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
use execution_tape::host::{
    AccessSink, Host, HostError, HostSig, SigHash, ValueRef, sig_hash as sig_hash_fn,
};
use execution_tape::program::Program;
use execution_tape::program::{Const, ValueType};
use execution_tape::trace::{ScopeKind, TraceMask, TraceOutcome, TraceSink};
use execution_tape::value::{FuncId, Value};
use execution_tape::vm::{Limits, Vm};

fn bench_vm(c: &mut Criterion) {
    bench_i64_add_chain(c);
    bench_i64_add_chain_traced_instr(c);
    bench_i64_add_chain_traced_instr_span_dense(c);
    bench_i64_add_chain_traced_instr_span_sparse(c);
    bench_i64_add_chain_no_trace_span_dense(c);
    bench_bytes_const_len(c);
    bench_str_const_len(c);
    bench_call_overhead(c);
    bench_call_loop(c);
    bench_branch_hot_loop(c);
    bench_host_call(c);
    bench_host_call_loop(c);
    bench_host_call_traced_run(c);
    bench_host_call_traced_instr(c);
    bench_host_call_traced_host(c);
}

fn bench_i64_add_chain(c: &mut Criterion) {
    let mut group = c.benchmark_group("i64_add_chain");
    for &chain_len in &[10_u32, 50, 200, 1000] {
        let p = build_i64_add_chain(chain_len);
        let mut vm = Vm::new(NopHost, wide_open_limits());
        group.bench_with_input(BenchmarkId::from_parameter(chain_len), &p, |b, p| {
            b.iter(|| {
                let out = vm.run(p, FuncId(0), &[], TraceMask::NONE, None).unwrap();
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
        let mut sink = CountingInstr::default();
        let mask = sink.mask();
        group.bench_with_input(BenchmarkId::from_parameter(chain_len), &p, |b, p| {
            b.iter(|| {
                let out = vm.run(p, FuncId(0), &[], mask, Some(&mut sink)).unwrap();
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
        let mut sink = CountingInstr::default();
        let mask = sink.mask();
        group.bench_with_input(BenchmarkId::from_parameter(chain_len), &p, |b, p| {
            b.iter(|| {
                let out = vm.run(p, FuncId(0), &[], mask, Some(&mut sink)).unwrap();
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
        let mut sink = CountingInstr::default();
        let mask = sink.mask();
        group.bench_with_input(BenchmarkId::from_parameter(chain_len), &p, |b, p| {
            b.iter(|| {
                let out = vm.run(p, FuncId(0), &[], mask, Some(&mut sink)).unwrap();
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
        group.bench_with_input(BenchmarkId::from_parameter(chain_len), &p, |b, p| {
            b.iter(|| {
                let out = vm.run(p, FuncId(0), &[], TraceMask::NONE, None).unwrap();
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
        group.bench_with_input(BenchmarkId::from_parameter(n), &p, |b, p| {
            b.iter(|| {
                let out = vm.run(p, FuncId(0), &[], TraceMask::NONE, None).unwrap();
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
        group.bench_with_input(BenchmarkId::from_parameter(n), &p, |b, p| {
            b.iter(|| {
                let out = vm.run(p, FuncId(0), &[], TraceMask::NONE, None).unwrap();
                black_box(out);
            });
        });
    }
    group.finish();
}

fn bench_call_overhead(c: &mut Criterion) {
    let p = build_call_overhead();
    let mut vm = Vm::new(NopHost, wide_open_limits());

    c.bench_function("call_overhead_one_call", |b| {
        b.iter(|| {
            let out = vm.run(&p, FuncId(0), &[], TraceMask::NONE, None).unwrap();
            black_box(out);
        });
    });
}

fn bench_call_loop(c: &mut Criterion) {
    let mut group = c.benchmark_group("call_loop");
    for &iters in &[10_u64, 100, 1000] {
        let p = build_call_loop(iters);
        let mut vm = Vm::new(NopHost, wide_open_limits());
        group.bench_with_input(BenchmarkId::from_parameter(iters), &p, |b, p| {
            b.iter(|| {
                let out = vm.run(p, FuncId(0), &[], TraceMask::NONE, None).unwrap();
                black_box(out);
            });
        });
    }
    group.finish();
}

fn bench_branch_hot_loop(c: &mut Criterion) {
    let mut group = c.benchmark_group("branch_hot_loop");
    for &iters in &[100_u64, 1000, 10_000] {
        let p = build_branch_hot_loop(iters);
        let mut vm = Vm::new(NopHost, wide_open_limits());
        group.bench_with_input(BenchmarkId::from_parameter(iters), &p, |b, p| {
            b.iter(|| {
                let out = vm.run(p, FuncId(0), &[], TraceMask::NONE, None).unwrap();
                black_box(out);
            });
        });
    }
    group.finish();
}

fn bench_host_call(c: &mut Criterion) {
    let p = build_host_call_overhead();
    let mut vm = Vm::new(IdentityHost, wide_open_limits());

    c.bench_function("host_call_overhead_one_call", |b| {
        b.iter(|| {
            let out = vm.run(&p, FuncId(0), &[], TraceMask::NONE, None).unwrap();
            black_box(out);
        });
    });
}

fn bench_host_call_loop(c: &mut Criterion) {
    let mut group = c.benchmark_group("host_call_loop");
    for &iters in &[10_u64, 100, 1000] {
        let p = build_host_call_loop(iters);
        let mut vm = Vm::new(IdentityHost, wide_open_limits());
        group.bench_with_input(BenchmarkId::from_parameter(iters), &p, |b, p| {
            b.iter(|| {
                let out = vm.run(p, FuncId(0), &[], TraceMask::NONE, None).unwrap();
                black_box(out);
            });
        });
    }
    group.finish();
}

fn bench_host_call_traced_run(c: &mut Criterion) {
    let p = build_host_call_overhead();
    let mut vm = Vm::new(IdentityHost, wide_open_limits());
    let mut sink = CountingTrace::default();
    let mask = sink.mask();

    c.bench_function("host_call_traced_run_mask_run", |b| {
        b.iter(|| {
            let out = vm.run(&p, FuncId(0), &[], mask, Some(&mut sink)).unwrap();
            black_box(out);
        });
    });
}

fn bench_host_call_traced_instr(c: &mut Criterion) {
    let p = build_host_call_overhead();
    let mut vm = Vm::new(IdentityHost, wide_open_limits());
    let mut sink = CountingInstr::default();
    let mask = sink.mask();

    c.bench_function("host_call_traced_instr_mask_instr", |b| {
        b.iter(|| {
            let out = vm.run(&p, FuncId(0), &[], mask, Some(&mut sink)).unwrap();
            black_box(out);
        });
    });
}

fn bench_host_call_traced_host(c: &mut Criterion) {
    let p = build_host_call_overhead();
    let mut vm = Vm::new(IdentityHost, wide_open_limits());
    let mut sink = CountingHostScopes::default();
    let mask = sink.mask();

    c.bench_function("host_call_traced_host_mask_host", |b| {
        b.iter(|| {
            let out = vm.run(&p, FuncId(0), &[], mask, Some(&mut sink)).unwrap();
            black_box(out);
        });
    });
}

fn build_i64_add_chain(chain_len: u32) -> execution_tape::verifier::VerifiedProgram {
    let mut a = Asm::new();
    a.const_i64(1, 1);
    a.const_i64(2, 2);
    let mut cur = 3;
    for _ in 0..chain_len {
        a.i64_add(cur, cur - 1, 2);
        cur += 1;
    }
    a.ret(0, &[cur - 1]);

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
    a.const_i64(1, 1);
    a.const_i64(2, 2);
    let mut cur = 3;
    let mut next_span_id = 1_u64;
    for i in 0..chain_len {
        if i % span_every == 0 {
            a.span(next_span_id);
            next_span_id = next_span_id.saturating_add(1);
        }
        a.i64_add(cur, cur - 1, 2);
        cur += 1;
    }
    a.ret(0, &[cur - 1]);

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
    a.const_pool(1, bytes);
    a.bytes_len(2, 1);
    a.ret(0, &[2]);

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
    a.const_pool(1, s);
    a.str_len(2, 1);
    a.ret(0, &[2]);

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
    let f0 = pb.declare_function(FunctionSig {
        arg_types: vec![],
        ret_types: vec![ValueType::I64],
    });
    let f1 = pb.declare_function(FunctionSig {
        arg_types: vec![],
        ret_types: vec![ValueType::I64],
    });

    let mut a1 = Asm::new();
    a1.const_i64(1, 1);
    a1.const_i64(2, 2);
    a1.i64_add(3, 1, 2);
    a1.ret(0, &[3]);
    pb.define_function(f1, a1).unwrap();

    let mut a0 = Asm::new();
    a0.call(0, f1, 0, &[], &[1]);
    a0.ret(0, &[1]);
    pb.define_function(f0, a0).unwrap();

    pb.build_verified().unwrap()
}

fn build_call_loop(iters: u64) -> execution_tape::verifier::VerifiedProgram {
    let mut pb = ProgramBuilder::new();
    let f0 = pb.declare_function(FunctionSig {
        arg_types: vec![],
        ret_types: vec![ValueType::I64],
    });
    let f1 = pb.declare_function(FunctionSig {
        arg_types: vec![],
        ret_types: vec![ValueType::I64],
    });

    let mut a1 = Asm::new();
    a1.const_i64(1, 1);
    a1.const_i64(2, 2);
    a1.i64_add(3, 1, 2);
    a1.ret(0, &[3]);
    pb.define_function(f1, a1).unwrap();

    // r1: counter, r2: limit, r3: one, r4: sum, r5: cond, r6: tmp (call result)
    let mut a0 = Asm::new();
    let l_loop = a0.label();
    let l_body = a0.label();
    let l_done = a0.label();

    a0.const_u64(1, 0);
    a0.const_u64(2, iters);
    a0.const_u64(3, 1);
    a0.const_i64(4, 0);

    a0.jmp(l_loop);
    a0.place(l_loop).unwrap();
    a0.u64_eq(5, 1, 2);
    a0.br(5, l_done, l_body);

    // body
    a0.place(l_body).unwrap();
    a0.call(0, f1, 0, &[], &[6]);
    a0.i64_add(4, 4, 6);
    a0.u64_add(1, 1, 3);
    a0.jmp(l_loop);

    a0.place(l_done).unwrap();
    a0.ret(0, &[4]);

    pb.define_function(f0, a0).unwrap();
    pb.build_verified().unwrap()
}

fn build_branch_hot_loop(iters: u64) -> execution_tape::verifier::VerifiedProgram {
    let mut pb = ProgramBuilder::new();

    // r1: counter, r2: limit, r3: one, r4: zero
    // r5: done?, r6: parity, r7: even?, r8: accumulator
    let mut a = Asm::new();
    let l_loop = a.label();
    let l_check = a.label();
    let l_even = a.label();
    let l_odd = a.label();
    let l_done = a.label();

    a.const_u64(1, 0);
    a.const_u64(2, iters);
    a.const_u64(3, 1);
    a.const_u64(4, 0);
    a.const_i64(8, 0);

    a.jmp(l_loop);
    a.place(l_loop).unwrap();
    a.u64_eq(5, 1, 2);
    a.br(5, l_done, l_check);

    a.place(l_check).unwrap();
    a.u64_and(6, 1, 3);
    a.u64_eq(7, 6, 4);
    a.br(7, l_even, l_odd);

    // Alternate branches hit this on even iterations.
    a.place(l_even).unwrap();
    a.i64_add(8, 8, 8);
    a.u64_add(1, 1, 3);
    a.jmp(l_loop);

    // Alternate branches hit this on odd iterations.
    a.place(l_odd).unwrap();
    a.i64_sub(8, 8, 8);
    a.u64_add(1, 1, 3);
    a.jmp(l_loop);

    a.place(l_done).unwrap();
    a.ret(0, &[8]);

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
    a.const_i64(1, 9);
    a.host_call(0, host_sig, 0, &[1], &[2]);
    a.ret(0, &[2]);
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
    let l_loop = a.label();
    let l_body = a.label();
    let l_done = a.label();

    a.const_u64(1, 0);
    a.const_u64(2, iters);
    a.const_u64(3, 1);
    a.const_i64(4, 9);
    a.const_i64(5, 0);

    a.jmp(l_loop);
    a.place(l_loop).unwrap();
    a.u64_eq(6, 1, 2);
    a.br(6, l_done, l_body);

    // body
    a.place(l_body).unwrap();
    a.host_call(0, host_sig, 0, &[4], &[5]);
    a.u64_add(1, 1, 3);
    a.jmp(l_loop);

    a.place(l_done).unwrap();
    a.ret(0, &[5]);

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
        max_host_calls: u64::MAX,
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
        _access: Option<&mut dyn AccessSink>,
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
        _access: Option<&mut dyn AccessSink>,
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

    fn run_start(&mut self, _program: &Program, _entry: FuncId, _arg_count: usize) {
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
        _func: FuncId,
        _pc: u32,
        _span_id: Option<u64>,
    ) {
        self._count = self._count.wrapping_add(1);
    }

    fn scope_exit(
        &mut self,
        _program: &Program,
        _kind: ScopeKind,
        _depth: usize,
        _func: FuncId,
        _pc: u32,
        _span_id: Option<u64>,
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
        _func: FuncId,
        _pc: u32,
        _next_pc: u32,
        _span_id: Option<u64>,
        _opcode: u8,
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
