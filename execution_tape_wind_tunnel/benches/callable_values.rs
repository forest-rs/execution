// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Calls through a language's callable values, as a frontend that gives each value a typed
//! entry, a boxed entry and a shape lowers them, against direct calls of the same function.
//!
//! A callable value is a tuple of a typed entry closure, a boxed entry closure and a `u64`
//! shape, under a tuple contract. Each case walks a balanced call tree that passes the value
//! down to every leaf, where it calls `inc(n) = n + 1`:
//!
//! - `direct` calls `inc` directly and passes no value; it is the baseline.
//! - `typed` opens the value and calls its typed entry.
//! - `untyped` compares the value's shape with the call's, finds them equal and calls the typed
//!   entry.
//! - `mismatch` finds the shapes differ, so it boxes the argument with a host call, calls the
//!   boxed entry, which unboxes it, adds and boxes the result, and unboxes that: four host calls.
//!   `mismatch_synchronous` declares the box and unbox imports synchronous.
//!
//! Two more cases bound what a different representation could save, passing the parts as
//! separate values instead of one tuple, which then needs no contract and no opening:
//!
//! - `typed_entry_only` passes only the typed entry closure and calls it.
//! - `untyped_parts` passes the typed entry and the shape, and compares the shape first.
//!
//! Every case runs in a turn that yields host calls, whose driver dispatches each request, as
//! an embedding that handles control requests itself does.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use execution_tape::asm::{FunctionAbi, FunctionSig, ProgramBuilder, Reg};
use execution_tape::contract::Contract;
use execution_tape::host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef};
use execution_tape::program::{HostTypeId, ValueType};
use execution_tape::value::{FuncId, FuncRef, Obj, ObjHandle, Value};
use execution_tape::verifier::VerifiedProgram;
use execution_tape::vm::{Limits, RunStatus, TurnOptions, TurnOutcome, Vm};
use std::{hint::black_box, sync::Arc, time::Duration};

const DEPTH: i64 = 8;
const OBJECT: ValueType = ValueType::Obj(HostTypeId(1));
const SHAPE: u64 = 0x5eed;

/// Boxes an `i64` as an object and unboxes it again.
struct Boxes<const SYNC: bool>;
impl<const SYNC: bool> Host for Boxes<SYNC> {
    fn call(
        &mut self,
        symbol: &str,
        _: SigHash,
        args: &[ValueRef<'_>],
        rets: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        rets[0] = match (symbol, args) {
            ("box", [ValueRef::I64(n)]) => Value::Obj(Obj {
                host_type: HostTypeId(1),
                handle: ObjHandle(*n as u64),
            }),
            ("unbox", [ValueRef::Obj(object)]) => Value::I64(object.handle.0 as i64),
            _ => return Err(HostError::SignatureMismatch),
        };
        Ok(0)
    }
    fn is_synchronous(&self, _: &str, _: SigHash) -> bool {
        SYNC
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Case {
    Direct,
    Typed,
    Untyped,
    Mismatch,
    EntryOnly,
    Parts,
}

/// Builds the program for `case` and returns its entry, `entry(depth) -> i64`.
fn program(case: Case) -> (Arc<VerifiedProgram>, FuncId) {
    let mut b = ProgramBuilder::new();
    let integer = b.contract(Contract::Kind(ValueType::I64)).unwrap();
    let entry_kind = b.contract(Contract::Kind(ValueType::Closure)).unwrap();
    let shape_kind = b.contract(Contract::Kind(ValueType::U64)).unwrap();
    let callable = b
        .contract(Contract::Tuple(&[
            Some(entry_kind),
            Some(entry_kind),
            Some(shape_kind),
        ]))
        .unwrap();
    let box_import = b.host_sig_for(
        "box",
        HostSig {
            args: vec![ValueType::I64],
            rets: vec![OBJECT],
        },
    );
    let unbox_import = b.host_sig_for(
        "unbox",
        HostSig {
            args: vec![OBJECT],
            rets: vec![ValueType::I64],
        },
    );
    let typed_sig = b
        .call_sig(FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let boxed_sig = b
        .call_sig(FunctionSig {
            arg_types: vec![OBJECT],
            ret_types: vec![OBJECT],
        })
        .unwrap();

    // inc(n) = n + 1, directly and as a typed entry.
    let direct = b
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let mut a = b.function_asm(direct).unwrap();
    let one = a.alloc_register(ValueType::I64);
    a.const_i64(one, 1)
        .i64_add(Reg(1), Reg(1), one)
        .ret(Reg::EFFECT, &[Reg(1)]);
    b.define_function(direct, a).unwrap();
    let typed = b
        .declare_function(FunctionSig::closure_body(
            &[ValueType::I64],
            &[ValueType::I64],
        ))
        .unwrap();
    let mut a = b.function_asm(typed).unwrap();
    let one = a.alloc_register(ValueType::I64);
    a.const_i64(one, 1)
        .i64_add(Reg(2), Reg(2), one)
        .ret(Reg::EFFECT, &[Reg(2)]);
    b.define_function(typed, a).unwrap();
    // The boxed entry unboxes its argument, adds one and boxes the result.
    let boxed = b
        .declare_function(FunctionSig::closure_body(&[OBJECT], &[OBJECT]))
        .unwrap();
    let mut a = b.function_asm(boxed).unwrap();
    let n = a.alloc_register(ValueType::I64);
    let one = a.alloc_register(ValueType::I64);
    a.host_call_sig_id(Reg::EFFECT, unbox_import, Reg::EFFECT, &[Reg(2)], &[n])
        .const_i64(one, 1)
        .i64_add(n, n, one)
        .host_call_sig_id(Reg::EFFECT, box_import, Reg::EFFECT, &[n], &[Reg(2)])
        .ret(Reg::EFFECT, &[Reg(2)]);
    b.define_function(boxed, a).unwrap();

    // tree(value..., d): a leaf call at d = 0, else tree(value..., d - 1) twice, added.
    let value: Vec<execution_tape::asm::ContractRef> = match case {
        Case::Direct => vec![],
        Case::Typed | Case::Untyped | Case::Mismatch => vec![callable.into()],
        Case::EntryOnly => vec![entry_kind.into()],
        Case::Parts => vec![entry_kind.into(), shape_kind.into()],
    };
    let parts = value.len();
    let tree = b
        .declare_function(FunctionAbi {
            args: value.into_iter().chain([integer.into()]).collect(),
            rets: vec![integer.into()],
        })
        .unwrap();
    let mut a = b.function_asm(tree).unwrap();
    let parts: Vec<Reg> = (1..=parts as u32).map(Reg).collect();
    let f = parts.first().copied();
    let d = Reg(parts.len() as u32 + 1);
    let scratch = a.alloc_register(ValueType::I64);
    let leaf_test = a.alloc_register(ValueType::Bool);
    let left = a.alloc_register(ValueType::I64);
    let right = a.alloc_register(ValueType::I64);
    let entry_reg = a.alloc_register(ValueType::Closure);
    let shape = a.alloc_register(ValueType::U64);
    let expected = a.alloc_register(ValueType::U64);
    let same = a.alloc_register(ValueType::Bool);
    let boxed_entry = a.alloc_register(ValueType::Closure);
    let boxed_arg = a.alloc_register(OBJECT);
    let boxed_result = a.alloc_register(OBJECT);
    let leaf = a.label();
    let recurse = a.label();
    a.const_i64(scratch, 0)
        .i64_eq(leaf_test, d, scratch)
        .br(leaf_test, leaf, recurse);
    a.place(recurse).unwrap();
    let args: Vec<Reg> = parts.iter().copied().chain([scratch]).collect();
    a.const_i64(scratch, 1)
        .i64_sub(scratch, d, scratch)
        .call(Reg::EFFECT, tree, Reg::EFFECT, &args, &[left])
        .call(Reg::EFFECT, tree, Reg::EFFECT, &args, &[right])
        .i64_add(left, left, right)
        .ret(Reg::EFFECT, &[left]);
    a.place(leaf).unwrap();
    match (case, f) {
        (Case::Direct, _) => {
            a.call(Reg::EFFECT, direct, Reg::EFFECT, &[d], &[left])
                .ret(Reg::EFFECT, &[left]);
        }
        (Case::Typed, Some(f)) => {
            a.tuple_get(entry_reg, f, 0)
                .call_indirect(
                    Reg::EFFECT,
                    typed_sig,
                    entry_reg,
                    Reg::EFFECT,
                    &[d],
                    &[left],
                )
                .ret(Reg::EFFECT, &[left]);
        }
        (Case::Untyped | Case::Mismatch, Some(f)) => {
            let typed_call = a.label();
            let boxed_call = a.label();
            let call_shape = if case == Case::Untyped { SHAPE } else { !SHAPE };
            a.tuple_get(entry_reg, f, 0)
                .tuple_get(shape, f, 2)
                .const_u64(expected, call_shape)
                .u64_eq(same, shape, expected)
                .br(same, typed_call, boxed_call);
            a.place(typed_call).unwrap();
            a.call_indirect(
                Reg::EFFECT,
                typed_sig,
                entry_reg,
                Reg::EFFECT,
                &[d],
                &[left],
            )
            .ret(Reg::EFFECT, &[left]);
            a.place(boxed_call).unwrap();
            a.tuple_get(boxed_entry, f, 1)
                .host_call_sig_id(Reg::EFFECT, box_import, Reg::EFFECT, &[d], &[boxed_arg])
                .call_indirect(
                    Reg::EFFECT,
                    boxed_sig,
                    boxed_entry,
                    Reg::EFFECT,
                    &[boxed_arg],
                    &[boxed_result],
                )
                .host_call_sig_id(
                    Reg::EFFECT,
                    unbox_import,
                    Reg::EFFECT,
                    &[boxed_result],
                    &[left],
                )
                .ret(Reg::EFFECT, &[left]);
        }
        (Case::EntryOnly, Some(f)) => {
            a.call_indirect(Reg::EFFECT, typed_sig, f, Reg::EFFECT, &[d], &[left])
                .ret(Reg::EFFECT, &[left]);
        }
        (Case::Parts, Some(f)) => {
            let typed_call = a.label();
            a.const_u64(expected, SHAPE)
                .u64_eq(same, parts[1], expected)
                .br(same, typed_call, typed_call);
            a.place(typed_call).unwrap();
            a.call_indirect(Reg::EFFECT, typed_sig, f, Reg::EFFECT, &[d], &[left])
                .ret(Reg::EFFECT, &[left]);
        }
        _ => unreachable!("cases with a value take one"),
    }
    b.define_function(tree, a).unwrap();

    // entry(d) makes the value, as the frontend's constructor does, and walks the tree.
    let entry = b
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let mut a = b.function_asm(entry).unwrap();
    let result = a.alloc_register(ValueType::I64);
    if case != Case::Direct {
        let environment = a.alloc_register(ValueType::Agg);
        let function = a.alloc_register(ValueType::Func);
        let typed_entry = a.alloc_register(ValueType::Closure);
        let boxed_entry = a.alloc_register(ValueType::Closure);
        let shape = a.alloc_register(ValueType::U64);
        let value = a.alloc_register(ValueType::Agg);
        a.tuple_new(environment, &[])
            .const_func(function, typed)
            .closure_new(typed_entry, function, environment)
            .const_func(function, boxed)
            .closure_new(boxed_entry, function, environment)
            .const_u64(shape, SHAPE)
            .tuple_new(value, &[typed_entry, boxed_entry, shape]);
        let passed = match case {
            Case::EntryOnly => vec![typed_entry, Reg(1)],
            Case::Parts => vec![typed_entry, shape, Reg(1)],
            _ => vec![value, Reg(1)],
        };
        a.call(Reg::EFFECT, tree, Reg::EFFECT, &passed, &[result]);
    } else {
        a.call(Reg::EFFECT, tree, Reg::EFFECT, &[Reg(1)], &[result]);
    }
    a.ret(Reg::EFFECT, &[result]);
    b.define_function(entry, a).unwrap();
    (Arc::new(b.build_verified().unwrap()), entry)
}

fn walk<H: Host>(vm: &mut Vm<H>, entry: FuncRef) -> i64 {
    let args = [Value::I64(DEPTH)];
    let options = TurnOptions::default().yield_host_calls();
    let mut turn = vm.start(entry, black_box(&args), options).unwrap();
    loop {
        match turn.drive(u64::MAX).unwrap() {
            RunStatus::Complete => break,
            RunStatus::AwaitingHost => turn.pending_host_call().unwrap().dispatch().unwrap(),
            status => unreachable!("unexpected run status {status:?}"),
        }
    }
    let Ok(TurnOutcome::Returned(values)) = turn.finish() else {
        unreachable!("the walk returned");
    };
    let [Value::I64(sum)] = values[..] else {
        unreachable!("one integer");
    };
    sum
}

fn bench<H: Host>(c: &mut Criterion, name: &str, host: H, case: Case) {
    let (program, entry) = program(case);
    let mut vm = Vm::new(host, Limits::default());
    let unit = vm.install(program).unwrap();
    let entry = FuncRef::new(unit, entry);
    // Every leaf returns 1.
    assert_eq!(walk(&mut vm, entry), 1 << DEPTH);
    let mut group = c.benchmark_group("callable_values");
    group.bench_function(BenchmarkId::from_parameter(name), |b| {
        b.iter(|| black_box(walk(&mut vm, entry)));
    });
    group.finish();
}

fn callable_values(c: &mut Criterion) {
    bench(c, "direct", Boxes::<false>, Case::Direct);
    bench(c, "typed", Boxes::<false>, Case::Typed);
    bench(c, "untyped", Boxes::<false>, Case::Untyped);
    bench(c, "mismatch", Boxes::<false>, Case::Mismatch);
    bench(c, "mismatch_synchronous", Boxes::<true>, Case::Mismatch);
    bench(c, "typed_entry_only", Boxes::<false>, Case::EntryOnly);
    bench(c, "untyped_parts", Boxes::<false>, Case::Parts);
}

criterion_group! {
    name = benches;
    config = Criterion::default().warm_up_time(Duration::from_secs(1)).measurement_time(Duration::from_secs(3)).sample_size(30);
    targets = callable_values
}
criterion_main!(benches);
