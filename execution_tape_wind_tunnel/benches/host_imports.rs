// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The cost of resolving host calls with `HostContext::import_in` as the import surface grows.
//!
//! Each case runs a guest loop of `ITERS` dependent host calls to `step(x) = x + 1`, the last of
//! `N` imports of the same signature, so a lookup that scans the surface in order does the most
//! work. `import_in/N` dispatches by matching `ctx.import_in(&SURFACE)` against the step import;
//! `host_sig_id/N` dispatches by the import's ID, with no lookup, and is the floor. Divide by
//! `ITERS` for a per-call cost; the difference between the two is the lookup.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use execution_tape::asm::{FunctionSig, ProgramBuilder, Reg};
use execution_tape::host::{Host, HostContext, HostError, SigHash, ValueRef};
use execution_tape::host_binding::HostImport;
use execution_tape::program::{HostSigId, ValueType};
use execution_tape::trace::TraceMask;
use execution_tape::value::{FuncId, FuncRef, Value};
use execution_tape::verifier::VerifiedProgram;
use execution_tape::vm::{ExecutionContext, Limits, Vm};
use std::hint::black_box;

const ITERS: u64 = 1_000;
const SIZES: [usize; 4] = [1, 8, 32, 128];

/// `N` imports `bench.op.I(i64) -> i64`, as `'static` as `const` declarations would be.
fn surface(n: usize) -> &'static [HostImport] {
    let imports: Vec<HostImport> = (0..n)
        .map(|i| {
            let symbol: &'static str = format!("bench.op.{i}").leak();
            HostImport::new(symbol, &[ValueType::I64], &[ValueType::I64])
        })
        .collect();
    imports.leak()
}

/// How the host recognizes the step import.
#[derive(Clone, Copy)]
enum Dispatch {
    ImportIn,
    HostSigId,
}

struct StepHost {
    dispatch: Dispatch,
    surface: &'static [HostImport],
    step: HostImport,
    step_id: HostSigId,
}

impl Host for StepHost {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        args: &[ValueRef<'_>],
        rets: &mut [Value],
        ctx: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        let is_step = match self.dispatch {
            Dispatch::ImportIn => ctx.import_in(self.surface) == Some(self.step),
            Dispatch::HostSigId => ctx.host_sig_id() == self.step_id,
        };
        match (is_step, args) {
            (true, [ValueRef::I64(x)]) => {
                rets[0] = Value::I64(x + 1);
                Ok(0)
            }
            _ => Err(HostError::UnknownSymbol),
        }
    }
}

/// Imports all of `surface` and loops `(iters) -> i64` over calls to its last import.
fn program(surface: &[HostImport]) -> (VerifiedProgram, HostSigId) {
    let mut b = ProgramBuilder::new();
    let ids: Vec<HostSigId> = surface.iter().map(|&i| b.host_import(i)).collect();
    let step = *ids.last().unwrap();
    let function = b
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::U64],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let mut a = b.function_asm(function).unwrap();
    let limit = Reg(1);
    let counter = a.alloc_register(ValueType::U64);
    let one = a.alloc_register(ValueType::U64);
    let done = a.alloc_register(ValueType::Bool);
    let acc = a.alloc_register(ValueType::I64);
    let next = a.alloc_register(ValueType::I64);
    let (head, body, exit) = (a.label(), a.label(), a.label());
    a.const_u64(counter, 0)
        .const_u64(one, 1)
        .const_i64(acc, 0)
        .jmp(head);
    a.place(head).unwrap();
    a.u64_eq(done, counter, limit).br(done, exit, body);
    a.place(body).unwrap();
    a.host_call_sig_id(Reg::EFFECT, step, Reg::EFFECT, &[acc], &[next]);
    a.mov(acc, next).u64_add(counter, counter, one).jmp(head);
    a.place(exit).unwrap();
    a.ret(Reg::EFFECT, &[acc]);
    b.define_function(function, a).unwrap();
    (b.build_verified().unwrap(), step)
}

fn host_imports(c: &mut Criterion) {
    let limits = Limits {
        fuel: u64::MAX,
        ..Limits::default()
    };
    let mut group = c.benchmark_group("host_imports");
    let mut ctx = ExecutionContext::new();
    let mut out = Vec::new();
    for n in SIZES {
        let surface = surface(n);
        let (program, step_id) = program(surface);
        program
            .check_host_calls(surface, |_| Ok::<(), &str>(()))
            .unwrap();
        for (name, dispatch) in [
            ("import_in", Dispatch::ImportIn),
            ("host_sig_id", Dispatch::HostSigId),
        ] {
            let host = StepHost {
                dispatch,
                surface,
                step: *surface.last().unwrap(),
                step_id,
            };
            let mut vm = Vm::new(host, limits.clone());
            let unit = vm.install(program.clone()).unwrap();
            let entry = FuncRef::new(unit, FuncId(0));
            let args = [Value::U64(ITERS)];
            group.bench_with_input(BenchmarkId::new(name, n), &n, |b, _| {
                b.iter(|| {
                    out.clear();
                    vm.run_with_ctx(
                        &mut ctx,
                        entry,
                        black_box(&args),
                        &mut out,
                        TraceMask::NONE,
                        None,
                        None,
                    )
                    .unwrap();
                    assert_eq!(out[0], Value::I64(ITERS as i64), "every call adds one");
                });
            });
        }
    }
    group.finish();
}

criterion_group!(benches, host_imports);
criterion_main!(benches);
