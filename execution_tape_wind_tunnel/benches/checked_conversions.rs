// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The cost of a checked conversion and its branch against the trapping conversion and the
//! instruction sequences a frontend needs without one.
//!
//! Each case runs a guest loop of `ITERS` conversions of a counter that every input passes, so
//! the branch to the failure path is never taken. `*/loop_only` runs the same loop with no
//! conversion; subtract it and divide by `ITERS` for a per-conversion cost.
//!
//! - `i32`: `i64` to `i32`. `sequence` is wrap, widen, `i64.eq`, `br`.
//! - `u32`: `i64` to `u32`. `trapping` is `i64.to_u64` then `u64.to_u32`; `sequence` is two
//!   wraps, widen, `i64.eq`, `br`.
//! - `f64_i64`: an integral `f64` to `i64`. `trapping` is the truncating `f64.to_i64`, which does
//!   not detect a fraction; `sequence` is saturating truncation, convert back, `f64.eq`, `br`,
//!   then `const.f64 2^63`, `f64.lt`, `br`, because 2^63 saturates to a value that converts
//!   back to it.
//! - `f64_i32`: an integral `f64` to `i32`; `sequence` is saturating truncation, convert back,
//!   `f64.eq`, `br`.

use criterion::{Criterion, criterion_group, criterion_main};
use execution_tape::asm::{Asm, FunctionSig, Label, ProgramBuilder, Reg};
use execution_tape::host::{Host, HostContext, HostError, SigHash, ValueRef};
use execution_tape::program::ValueType;
use execution_tape::trace::TraceMask;
use execution_tape::value::{FuncId, FuncRef, Value};
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

/// What one iteration does with its input `x`.
#[derive(Copy, Clone)]
enum Body {
    Nothing,
    I32Trapping,
    I32Sequence,
    I32Checked,
    U32Trapping,
    U32Sequence,
    U32Checked,
    F64I64Trapping,
    F64I64Sequence,
    F64I64Checked,
    F64I32Trapping,
    F64I32Sequence,
    F64I32Checked,
}

/// Emits `body` converting `x`, branching to `fail` when a check fails.
fn emit(a: &mut Asm, body: Body, x: Reg, fail: Label) {
    let pass = a.label();
    match body {
        Body::Nothing => {}
        Body::I32Trapping => {
            let n = a.alloc_register(ValueType::I32);
            a.i64_to_i32(n, x);
        }
        Body::I32Sequence => {
            let n = a.alloc_register(ValueType::I32);
            let back = a.alloc_register(ValueType::I64);
            let fits = a.alloc_register(ValueType::Bool);
            a.i64_to_i32_wrap(n, x)
                .i32_to_i64(back, n)
                .i64_eq(fits, back, x)
                .br(fits, pass, fail);
        }
        Body::I32Checked => {
            let n = a.alloc_register(ValueType::I32);
            let overflow = a.alloc_register(ValueType::Bool);
            a.i64_to_i32_ovf(n, overflow, x).br(overflow, fail, pass);
        }
        Body::U32Trapping => {
            let bits = a.alloc_register(ValueType::U64);
            let n = a.alloc_register(ValueType::U32);
            a.i64_to_u64(bits, x).u64_to_u32(n, bits);
        }
        Body::U32Sequence => {
            let bits = a.alloc_register(ValueType::U64);
            let n = a.alloc_register(ValueType::U32);
            let back = a.alloc_register(ValueType::I64);
            let fits = a.alloc_register(ValueType::Bool);
            a.i64_to_u64_wrap(bits, x)
                .u64_to_u32_wrap(n, bits)
                .u32_to_i64(back, n)
                .i64_eq(fits, back, x)
                .br(fits, pass, fail);
        }
        Body::U32Checked => {
            let n = a.alloc_register(ValueType::U32);
            let overflow = a.alloc_register(ValueType::Bool);
            a.i64_to_u32_ovf(n, overflow, x).br(overflow, fail, pass);
        }
        Body::F64I64Trapping => {
            let n = a.alloc_register(ValueType::I64);
            a.f64_to_i64(n, x);
        }
        Body::F64I64Sequence => {
            let n = a.alloc_register(ValueType::I64);
            let back = a.alloc_register(ValueType::F64);
            let ok = a.alloc_register(ValueType::Bool);
            let top = a.alloc_register(ValueType::F64);
            let below_top = a.label();
            a.f64_to_i64_sat(n, x)
                .i64_to_f64(back, n)
                .f64_eq(ok, back, x)
                .br(ok, below_top, fail);
            a.place(below_top).unwrap();
            a.const_f64(top, 9_223_372_036_854_775_808.0)
                .f64_lt(ok, x, top)
                .br(ok, pass, fail);
        }
        Body::F64I64Checked => {
            let n = a.alloc_register(ValueType::I64);
            let inexact = a.alloc_register(ValueType::Bool);
            a.f64_to_i64_exact(n, inexact, x).br(inexact, fail, pass);
        }
        Body::F64I32Trapping => {
            let n = a.alloc_register(ValueType::I32);
            a.f64_to_i32(n, x);
        }
        Body::F64I32Sequence => {
            let n = a.alloc_register(ValueType::I32);
            let back = a.alloc_register(ValueType::F64);
            let ok = a.alloc_register(ValueType::Bool);
            a.f64_to_i32_sat(n, x)
                .i32_to_f64(back, n)
                .f64_eq(ok, back, x)
                .br(ok, pass, fail);
        }
        Body::F64I32Checked => {
            let n = a.alloc_register(ValueType::I32);
            let inexact = a.alloc_register(ValueType::Bool);
            a.f64_to_i32_exact(n, inexact, x).br(inexact, fail, pass);
        }
    }
    a.place(pass).unwrap();
}

const CASES: [(&str, Body, ValueType); 14] = [
    ("i32/loop_only", Body::Nothing, ValueType::I64),
    ("i32/trapping", Body::I32Trapping, ValueType::I64),
    ("i32/sequence", Body::I32Sequence, ValueType::I64),
    ("i32/checked", Body::I32Checked, ValueType::I64),
    ("u32/trapping", Body::U32Trapping, ValueType::I64),
    ("u32/sequence", Body::U32Sequence, ValueType::I64),
    ("u32/checked", Body::U32Checked, ValueType::I64),
    ("f64/loop_only", Body::Nothing, ValueType::F64),
    ("f64_i64/trapping", Body::F64I64Trapping, ValueType::F64),
    ("f64_i64/sequence", Body::F64I64Sequence, ValueType::F64),
    ("f64_i64/checked", Body::F64I64Checked, ValueType::F64),
    ("f64_i32/trapping", Body::F64I32Trapping, ValueType::F64),
    ("f64_i32/sequence", Body::F64I32Sequence, ValueType::F64),
    ("f64_i32/checked", Body::F64I32Checked, ValueType::F64),
];

/// One function per case, `(iters) -> bool`: true when every conversion passed.
fn program() -> Arc<VerifiedProgram> {
    let mut b = ProgramBuilder::new();
    for (_, body, input) in CASES {
        let function = b
            .declare_function(FunctionSig {
                arg_types: vec![ValueType::U64],
                ret_types: vec![ValueType::Bool],
            })
            .unwrap();
        let mut a = b.function_asm(function).unwrap();
        let limit = Reg(1);
        let counter = a.alloc_register(ValueType::U64);
        let one = a.alloc_register(ValueType::U64);
        let done = a.alloc_register(ValueType::Bool);
        let x = a.alloc_register(input);
        let step = a.alloc_register(input);
        let result = a.alloc_register(ValueType::Bool);
        let (head, body_label, exit, fail) = (a.label(), a.label(), a.label(), a.label());
        a.const_u64(counter, 0).const_u64(one, 1);
        if input == ValueType::I64 {
            a.const_i64(x, 0).const_i64(step, 1);
        } else {
            a.const_f64(x, 0.0).const_f64(step, 1.0);
        }
        a.jmp(head);
        a.place(head).unwrap();
        a.u64_eq(done, counter, limit).br(done, exit, body_label);
        a.place(body_label).unwrap();
        emit(&mut a, body, x, fail);
        if input == ValueType::I64 {
            a.i64_add(x, x, step);
        } else {
            a.f64_add(x, x, step);
        }
        a.u64_add(counter, counter, one).jmp(head);
        a.place(exit).unwrap();
        a.const_bool(result, true).ret(Reg::EFFECT, &[result]);
        a.place(fail).unwrap();
        a.const_bool(result, false).ret(Reg::EFFECT, &[result]);
        b.define_function(function, a).unwrap();
    }
    Arc::new(b.build_verified().unwrap())
}

fn checked_conversions(c: &mut Criterion) {
    let limits = Limits {
        fuel: u64::MAX,
        ..Limits::default()
    };
    let mut vm = Vm::new(NoHost, limits);
    let unit = vm.install(program()).unwrap();
    let mut group = c.benchmark_group("checked_conversions");
    let mut out = Vec::new();
    let args = [Value::U64(ITERS)];
    for (index, (name, _, _)) in CASES.iter().enumerate() {
        let entry = FuncRef::new(unit, FuncId(u32::try_from(index).unwrap()));
        group.bench_function(*name, |b| {
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
                assert_eq!(out[0], Value::Bool(true), "every conversion passes");
            });
        });
    }
    group.finish();
}

criterion_group!(benches, checked_conversions);
criterion_main!(benches);
