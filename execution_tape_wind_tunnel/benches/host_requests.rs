// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Compare identical dependent host work through synchronous and yielded boundaries.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use execution_tape::{
    asm::{FunctionSig, ProgramBuilder},
    host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef},
    program::ValueType,
    trace::TraceMask,
    value::{FuncId, Value},
    vm::{InvocationOptions, Limits, RunStatus, Vm},
};
use std::{
    future::Future,
    hint::black_box,
    task::{Context, Poll, Waker},
    time::Duration,
};

fn reply<'a>(mut args: impl Iterator<Item = ValueRef<'a>>) -> Value {
    let (Some(ValueRef::I64(previous)), Some(ValueRef::Str(text)), None) =
        (args.next(), args.next(), args.next())
    else {
        panic!("benchmark ABI");
    };
    Value::I64(previous + black_box(text).len() as i64)
}
struct Native;
impl Host for Native {
    fn call(
        &mut self,
        symbol: &str,
        _: SigHash,
        args: &[ValueRef<'_>],
        rets: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        assert_eq!(symbol, "add_length");
        rets[0] = reply(args.iter().copied());
        Ok(0)
    }
}
#[derive(Clone, Copy)]
enum Mode {
    Ordinary,
    Scheduled,
    Dispatch,
    Direct,
    AsyncReady,
}
fn host_requests(c: &mut Criterion) {
    let mut group = c.benchmark_group("host_requests_100");
    for bytes in [16, 4096] {
        let mut builder = ProgramBuilder::new();
        let entry = builder
            .declare_function(FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
            })
            .unwrap();
        let import = builder.host_sig_for(
            "add_length",
            HostSig {
                args: vec![ValueType::I64, ValueType::Str],
                rets: vec![ValueType::I64],
            },
        );
        let text = builder.const_str(&"x".repeat(bytes));
        let mut asm = builder.function_asm(entry).unwrap();
        let sum = asm.alloc_register(ValueType::I64);
        let payload = asm.alloc_register(ValueType::Str);
        asm.const_i64(sum, 0).const_pool(payload, text);
        for _ in 0..100 {
            asm.host_call_sig_id(0, import, 0, &[sum, payload], &[sum]);
        }
        asm.ret(0, &[sum]);
        builder.define_function(entry, asm).unwrap();
        let program = builder.build_verified().unwrap();
        for (name, mode) in [
            ("ordinary", Mode::Ordinary),
            ("scheduled", Mode::Scheduled),
            ("yield_dispatch", Mode::Dispatch),
            ("yield_direct_slots", Mode::Direct),
            ("yield_async_ready", Mode::AsyncReady),
        ] {
            let mut vm = Vm::new(Native, Limits::default());
            group.bench_function(BenchmarkId::new(name, bytes), |b| {
                b.iter(|| {
                    if matches!(mode, Mode::Ordinary) {
                        let out = vm
                            .run(&program, FuncId(0), &[], TraceMask::NONE, None)
                            .unwrap();
                        assert_eq!(out, [Value::I64((100 * bytes) as i64)]);
                        black_box(out);
                        return;
                    }
                    let mut options = InvocationOptions::default();
                    if !matches!(mode, Mode::Scheduled) {
                        options = options.yield_host_calls();
                    }
                    let mut run = vm.start(&program, entry, &[], options).unwrap();
                    loop {
                        match run.resume(u64::MAX).unwrap() {
                            RunStatus::Suspended => unreachable!("unlimited quantum"),
                            RunStatus::Complete => break,
                            RunStatus::AwaitingHost => {
                                let request = run.pending_host_call().unwrap();
                                match mode {
                                    Mode::Dispatch => request.dispatch().unwrap(),
                                    Mode::Direct => {
                                        assert_eq!(request.symbol(), "add_length");
                                        let mut slots = [reply(request.arguments())];
                                        request.complete(&mut slots, Ok(0)).unwrap();
                                    }
                                    Mode::AsyncReady => {
                                        let future = request.complete_with(async |request| {
                                            assert_eq!(request.symbol(), "add_length");
                                            Ok((vec![reply(request.arguments())], 0))
                                        });
                                        let mut future = std::pin::pin!(future);
                                        let mut context = Context::from_waker(Waker::noop());
                                        assert!(matches!(
                                            future.as_mut().poll(&mut context),
                                            Poll::Ready(Ok(()))
                                        ));
                                    }
                                    _ => unreachable!("ordinary host owns delivery"),
                                }
                            }
                        }
                    }
                    let out = run.result().unwrap().unwrap();
                    assert_eq!(out, [Value::I64((100 * bytes) as i64)]);
                    black_box(out);
                });
            });
        }
    }
    group.finish();
}
criterion_group! {
    name = benches;
    config = Criterion::default().warm_up_time(Duration::from_secs(1)).measurement_time(Duration::from_secs(3)).sample_size(30);
    targets = host_requests
}
criterion_main!(benches);
