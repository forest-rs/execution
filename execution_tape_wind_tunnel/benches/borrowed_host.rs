// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Running installed code against a host that borrows per-call state.
//!
//! `reused` is the floor: one VM whose host owns its state, with a reused context. `fresh` builds
//! a VM around the borrowing host and installs the program for every call, which is what such a
//! host needs without `Vm::replace_host`. `replace_host` parks one VM and swaps the borrowing host
//! in and out around each call. Each case runs an empty function and one with a host call.

use std::hint::black_box;
use std::sync::Arc;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use execution_tape::asm::{FunctionSig, ProgramBuilder};
use execution_tape::host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef};
use execution_tape::program::ValueType;
use execution_tape::trace::TraceMask;
use execution_tape::value::{FuncId, FuncRef, Value};
use execution_tape::verifier::VerifiedProgram;
use execution_tape::vm::{ExecutionContext, Limits, Vm};

/// Per-call state that only the caller owns.
struct Context {
    scale: f32,
    calls: u64,
}

fn scale(cx: &mut Context, args: &[ValueRef<'_>], rets: &mut [Value]) -> Result<u64, HostError> {
    let [ValueRef::F32(x)] = args else {
        return Err(HostError::Failed);
    };
    cx.calls += 1;
    rets[0] = Value::F32(x * cx.scale);
    Ok(0)
}

/// Owns its context: the reused-VM floor.
struct Owned(Context);
impl Host for Owned {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        args: &[ValueRef<'_>],
        rets: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        scale(&mut self.0, args, rets)
    }
}

/// Borrows the caller's context for one call.
struct Borrowed<'c>(&'c mut Context);
impl Host for Borrowed<'_> {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        args: &[ValueRef<'_>],
        rets: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        scale(self.0, args, rets)
    }
}

/// Holds the VM between calls.
struct Parked;
impl Host for Parked {
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

/// `f(x: f32) -> f32`, either returning `x` or `scale(x)`.
fn program(host_call: bool) -> Arc<VerifiedProgram> {
    let mut builder = ProgramBuilder::new();
    let entry = builder
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::F32],
            ret_types: vec![ValueType::F32],
        })
        .unwrap();
    let import = builder.host_sig_for(
        "scale",
        HostSig {
            args: vec![ValueType::F32],
            rets: vec![ValueType::F32],
        },
    );
    let mut asm = builder.function_asm(entry).unwrap();
    if host_call {
        let out = asm.alloc_register(ValueType::F32);
        asm.host_call_sig_id(0, import, 0, &[1], &[out])
            .ret(0, &[out]);
    } else {
        asm.ret(0, &[1]);
    }
    builder.define_function(entry, asm).unwrap();
    Arc::new(builder.build_verified().unwrap())
}

fn borrowed_host(c: &mut Criterion) {
    let mut group = c.benchmark_group("borrowed_host");
    let args = [Value::F32(1.5)];
    for (name, host_call) in [("empty", false), ("one_host_call", true)] {
        let program = program(host_call);
        let entry = FuncId(0);

        let mut vm = Vm::new(
            Owned(Context {
                scale: 2.0,
                calls: 0,
            }),
            Limits::default(),
        );
        let unit = vm.install(Arc::clone(&program)).unwrap();
        let mut scratch = ExecutionContext::new();
        let mut out = Vec::with_capacity(1);
        group.bench_function(BenchmarkId::new("reused", name), |b| {
            b.iter(|| {
                out.clear();
                vm.run_with_ctx(
                    &mut scratch,
                    FuncRef::new(unit, entry),
                    black_box(&args),
                    &mut out,
                    TraceMask::NONE,
                    None,
                    None,
                )
                .unwrap();
                black_box(&out);
            });
        });

        let mut cx = Context {
            scale: 2.0,
            calls: 0,
        };
        group.bench_function(BenchmarkId::new("fresh", name), |b| {
            b.iter(|| {
                let mut vm = Vm::new(Borrowed(&mut cx), Limits::default());
                let unit = vm.install(Arc::clone(&program)).unwrap();
                let out = vm
                    .run(
                        FuncRef::new(unit, entry),
                        black_box(&args),
                        TraceMask::NONE,
                        None,
                    )
                    .unwrap();
                black_box(out);
            });
        });

        let mut parked = Some(Vm::new(Parked, Limits::default()));
        let unit = parked
            .as_mut()
            .unwrap()
            .install(Arc::clone(&program))
            .unwrap();
        let mut scratch = ExecutionContext::new();
        let mut out = Vec::with_capacity(1);
        group.bench_function(BenchmarkId::new("replace_host", name), |b| {
            b.iter(|| {
                let (mut vm, idle) = parked.take().unwrap().replace_host(Borrowed(&mut cx));
                out.clear();
                vm.run_with_ctx(
                    &mut scratch,
                    FuncRef::new(unit, entry),
                    black_box(&args),
                    &mut out,
                    TraceMask::NONE,
                    None,
                    None,
                )
                .unwrap();
                parked = Some(vm.replace_host(idle).0);
                black_box(&out);
            });
        });
        assert!(!host_call || cx.calls > 0);
    }
    group.finish();
}

criterion_group!(benches, borrowed_host);
criterion_main!(benches);
