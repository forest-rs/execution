// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The cost of a call through a link against a direct `call` and a `call.indirect`.
//!
//! Each case runs a guest loop of `ITERS` calls to `step(x) = x + 1`, threading the result
//! through the next call, so every call's result is used. `loop_only` runs the same loop with
//! the add inline and no call; subtract it and divide by `ITERS` for a per-call cost.
//! `link`/`indirect` reach `step` in the calling unit (`same_unit`) or in another install of the
//! program (`other_unit`), the second after `Vm::relink`.

use criterion::{Criterion, criterion_group, criterion_main};
use execution_tape::asm::{FunctionSig, ProgramBuilder, Reg};
use execution_tape::host::{Host, HostContext, HostError, SigHash, ValueRef};
use execution_tape::program::{LinkId, ValueType};
use execution_tape::trace::TraceMask;
use execution_tape::value::{FuncId, FuncRef, UnitId, Value};
use execution_tape::verifier::VerifiedProgram;
use execution_tape::vm::{Limits, Vm};
use std::hint::black_box;
use std::sync::Arc;

const ITERS: u64 = 1_000;

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

/// How a loop reaches `step`.
#[derive(Copy, Clone)]
enum Call {
    Inline,
    Direct,
    Link,
    Indirect,
}

const STEP: FuncId = FuncId(0);
const LINK: LinkId = LinkId(0);

/// `step`, then one loop function per [`Call`], in that order, each `(iters, callee) -> i64`.
fn program() -> Arc<VerifiedProgram> {
    let mut b = ProgramBuilder::new();
    let step = b
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    assert_eq!(step, STEP, "step is the first function");
    let mut body = b.function_asm(step).unwrap();
    let x = b.argument(step, 0).unwrap();
    let one = body.alloc_register(ValueType::I64);
    let y = body.alloc_register(ValueType::I64);
    body.const_i64(one, 1)
        .i64_add(y, x, one)
        .ret(Reg::EFFECT, &[y]);
    b.define_function(step, body).unwrap();
    let link = b.declare_link("step", step).unwrap();
    assert_eq!(link, LINK, "the only link");
    let signature = b
        .call_sig(FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();

    for call in [Call::Inline, Call::Direct, Call::Link, Call::Indirect] {
        let function = b
            .declare_function(FunctionSig {
                arg_types: vec![ValueType::U64, ValueType::Func],
                ret_types: vec![ValueType::I64],
            })
            .unwrap();
        let mut a = b.function_asm(function).unwrap();
        let (limit, callee) = (Reg(1), Reg(2));
        let counter = a.alloc_register(ValueType::U64);
        let one = a.alloc_register(ValueType::U64);
        let done = a.alloc_register(ValueType::Bool);
        let acc = a.alloc_register(ValueType::I64);
        let next = a.alloc_register(ValueType::I64);
        let unit_step = a.alloc_register(ValueType::I64);
        let (head, body, exit) = (a.label(), a.label(), a.label());
        a.const_u64(counter, 0)
            .const_u64(one, 1)
            .const_i64(acc, 0)
            .const_i64(unit_step, 1)
            .jmp(head);
        a.place(head).unwrap();
        a.u64_eq(done, counter, limit).br(done, exit, body);
        a.place(body).unwrap();
        match call {
            Call::Inline => a.i64_add(next, acc, unit_step),
            Call::Direct => a.call(Reg::EFFECT, step, Reg::EFFECT, &[acc], &[next]),
            Call::Link => a.call_link(Reg::EFFECT, link, Reg::EFFECT, &[acc], &[next]),
            Call::Indirect => {
                a.call_indirect(Reg::EFFECT, signature, callee, Reg::EFFECT, &[acc], &[next])
            }
        };
        a.mov(acc, next).u64_add(counter, counter, one).jmp(head);
        a.place(exit).unwrap();
        a.ret(Reg::EFFECT, &[acc]);
        b.define_function(function, a).unwrap();
    }
    Arc::new(b.build_verified().unwrap())
}

fn loop_of(call: Call) -> FuncId {
    FuncId(1 + call as u32)
}

fn linked_calls(c: &mut Criterion) {
    let program = program();
    let limits = Limits {
        fuel: u64::MAX,
        ..Limits::default()
    };
    let mut vm = Vm::new(NoHost, limits);
    let caller = vm.install(Arc::clone(&program)).unwrap();
    let other = vm.install(Arc::clone(&program)).unwrap();
    // A second caller unit whose link names `step` of `other`.
    let relinked = vm.install(program).unwrap();
    vm.relink(relinked, LINK, FuncRef::new(other, STEP))
        .unwrap();

    let cases: [(&str, UnitId, Call, UnitId); 6] = [
        ("loop_only", caller, Call::Inline, caller),
        ("direct", caller, Call::Direct, caller),
        ("link/same_unit", caller, Call::Link, caller),
        ("link/other_unit", relinked, Call::Link, caller),
        ("indirect/same_unit", caller, Call::Indirect, caller),
        ("indirect/other_unit", caller, Call::Indirect, other),
    ];
    let mut group = c.benchmark_group("linked_calls");
    let mut out = Vec::new();
    for (name, unit, call, callee) in cases {
        let args = [Value::U64(ITERS), Value::Func(FuncRef::new(callee, STEP))];
        let entry = FuncRef::new(unit, loop_of(call));
        group.bench_function(name, |b| {
            b.iter(|| {
                out.clear();
                vm.run_into(
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
    group.finish();
}

criterion_group!(benches, linked_calls);
criterion_main!(benches);
