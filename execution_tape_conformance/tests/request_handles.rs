// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Handles read from a yielded request stay valid until the invocation commits or rolls back.

use execution_tape::{
    asm::{FunctionAbi, FunctionSig, ProgramBuilder},
    contract::Contract,
    host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef},
    program::ValueType,
    value::{FuncId, FuncRef, Value},
    vm::{InvocationOptions, Limits, RunStatus, Vm},
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
        panic!("the embedding completes every yielded request");
    }
}

fn sig(args: &[ValueType], rets: &[ValueType]) -> FunctionSig {
    FunctionSig {
        arg_types: args.to_vec(),
        ret_types: rets.to_vec(),
    }
}

struct Fixture {
    program: execution_tape::verifier::VerifiedProgram,
    entry: FuncId,
    make: FuncId,
    nested: FuncId,
}

/// `entry` passes a fresh tuple and a function to `ask`; `make` allocates a closure and `nested`
/// yields an `inner` request from inside a callback. `wrap` boxes an aggregate.
fn fixture() -> Fixture {
    let mut b = ProgramBuilder::new();
    let ask = b.host_sig_for(
        "ask",
        HostSig {
            args: vec![ValueType::Agg, ValueType::Func],
            rets: vec![ValueType::Agg],
        },
    );
    let inner = b.host_sig_for(
        "inner",
        HostSig {
            args: vec![ValueType::Agg],
            rets: vec![ValueType::Agg],
        },
    );
    let integer = b.contract(Contract::Kind(ValueType::I64)).unwrap();
    let captured = b.contract(Contract::Tuple(&[Some(integer)])).unwrap();
    let add = b
        .declare_function(FunctionAbi {
            args: vec![captured.into(), integer.into()],
            rets: vec![integer.into()],
        })
        .unwrap();
    let wrap = b
        .declare_function(sig(&[ValueType::Agg], &[ValueType::Agg]))
        .unwrap();
    let make = b
        .declare_function(sig(&[ValueType::I64], &[ValueType::Closure]))
        .unwrap();
    let nested = b
        .declare_function(sig(&[ValueType::Agg], &[ValueType::Agg]))
        .unwrap();
    let entry = b.declare_function(sig(&[], &[ValueType::Agg])).unwrap();

    let mut a = b.function_asm(add).unwrap();
    let (env, x) = (b.argument(add, 0).unwrap(), b.argument(add, 1).unwrap());
    let k = a.alloc_register(ValueType::I64);
    a.tuple_get(k, env, 0).i64_add(x, x, k).ret(0, &[x]);
    b.define_function(add, a).unwrap();

    let mut a = b.function_asm(wrap).unwrap();
    let boxed = a.alloc_register(ValueType::Agg);
    a.tuple_new(boxed, &[b.argument(wrap, 0).unwrap()])
        .ret(0, &[boxed]);
    b.define_function(wrap, a).unwrap();

    let mut a = b.function_asm(make).unwrap();
    let (function, env, closure) = (
        a.alloc_register(ValueType::Func),
        a.alloc_register(ValueType::Agg),
        a.alloc_register(ValueType::Closure),
    );
    a.const_func(function, add)
        .tuple_new(env, &[b.argument(make, 0).unwrap()])
        .closure_new(closure, function, env)
        .ret(0, &[closure]);
    b.define_function(make, a).unwrap();

    let mut a = b.function_asm(nested).unwrap();
    let answer = a.alloc_register(ValueType::Agg);
    a.host_call_sig_id(0, inner, 0, &[b.argument(nested, 0).unwrap()], &[answer])
        .ret(0, &[answer]);
    b.define_function(nested, a).unwrap();

    let mut a = b.function_asm(entry).unwrap();
    let (seven, data, function, answer) = (
        a.alloc_register(ValueType::I64),
        a.alloc_register(ValueType::Agg),
        a.alloc_register(ValueType::Func),
        a.alloc_register(ValueType::Agg),
    );
    a.const_i64(seven, 7)
        .tuple_new(data, &[seven])
        .const_func(function, wrap)
        .host_call_sig_id(0, ask, 0, &[data, function], &[answer])
        .ret(0, &[answer]);
    b.define_function(entry, a).unwrap();

    Fixture {
        program: b.build_verified().unwrap(),
        entry,
        make,
        nested,
    }
}

#[test]
fn handles_from_any_request_stay_valid_until_the_root_commits() {
    let f = fixture();
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(f.program).unwrap();
    let options = InvocationOptions::default().yield_host_calls();
    let mut run = vm.start(FuncRef::new(unit, f.entry), &[], options).unwrap();

    // The outer request's arguments: a guest tuple and a function value.
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    let request = run.pending_host_call().unwrap();
    let (data, wrap) = match request.arguments().collect::<Vec<_>>()[..] {
        [ValueRef::Agg(data), ValueRef::Func(wrap)] => (data, wrap),
        ref other => panic!("ask arguments: {other:?}"),
    };
    // A callback allocates a closure; its environment lives in the same heap.
    request
        .call_guest(Value::Func(FuncRef::new(unit, f.make)), vec![Value::I64(5)])
        .unwrap();
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::CallbackReturned);
    let mut request = run.pending_host_call().unwrap();
    let [Value::Closure(closure)] = request.take_callback_results().unwrap()[..] else {
        panic!("closure callback result");
    };

    // A second callback yields a nested request, which reads the outer argument again and uses
    // handles from the outer request and the earlier callback.
    request
        .call_guest(
            Value::Func(FuncRef::new(unit, f.nested)),
            vec![Value::Agg(data)],
        )
        .unwrap();
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    let request = run.pending_host_call().unwrap();
    assert_eq!(request.symbol(), "inner");
    assert_eq!(
        request.arguments().collect::<Vec<_>>(),
        [ValueRef::Agg(data)]
    );
    request
        .call_guest(Value::Closure(closure), vec![Value::I64(1)])
        .unwrap();
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::CallbackReturned);
    let mut request = run.pending_host_call().unwrap();
    assert_eq!(request.take_callback_results(), Some(vec![Value::I64(6)]));
    request
        .call_guest(Value::Func(wrap), vec![Value::Agg(data)])
        .unwrap();
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::CallbackReturned);
    let mut request = run.pending_host_call().unwrap();
    let mut boxed = request.take_callback_results().unwrap();
    let [Value::Agg(boxed_handle)] = boxed[..] else {
        panic!("wrapped tuple");
    };
    request.complete(&mut boxed, Ok(0)).unwrap();

    // Back at the outer request: every handle read so far still names the same node.
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::CallbackReturned);
    let mut request = run.pending_host_call().unwrap();
    assert_eq!(request.symbol(), "ask");
    assert_eq!(
        request.take_callback_results(),
        Some(vec![Value::Agg(boxed_handle)])
    );
    let heap = request.aggregates();
    assert_eq!(heap.tuple_get(data, 0).unwrap(), Value::I64(7));
    assert_eq!(heap.tuple_get(closure.env, 0).unwrap(), Value::I64(5));
    assert_eq!(heap.tuple_get(boxed_handle, 0).unwrap(), Value::Agg(data));
    request
        .call_guest(Value::Closure(closure), vec![Value::I64(2)])
        .unwrap();
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::CallbackReturned);
    let mut request = run.pending_host_call().unwrap();
    assert_eq!(request.take_callback_results(), Some(vec![Value::I64(7)]));
    let before_commit = request.aggregates().len_u32();
    request
        .complete(&mut [Value::Agg(boxed_handle)], Ok(0))
        .unwrap();

    // Root commit is the only compaction: unreachable nodes go and the output may be renamed.
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::Complete);
    let [Value::Agg(output)] = run.result().unwrap().unwrap()[..] else {
        panic!("tuple output");
    };
    let heap = run.aggregates();
    assert!(
        heap.len_u32() < before_commit,
        "commit retains reachable nodes only"
    );
    assert_eq!(heap.len_u32(), 2, "the boxed tuple and the data it reaches");
    let Value::Agg(inner) = heap.tuple_get(output, 0).unwrap() else {
        panic!("boxed aggregate");
    };
    assert_eq!(heap.tuple_get(inner, 0).unwrap(), Value::I64(7));
}

#[test]
fn rolling_back_discards_every_handle_the_requests_exposed() {
    let f = fixture();
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(f.program).unwrap();
    let options = InvocationOptions::default().yield_host_calls();
    let mut run = vm.start(FuncRef::new(unit, f.entry), &[], options).unwrap();
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    run.pending_host_call()
        .unwrap()
        .call_guest(Value::Func(FuncRef::new(unit, f.make)), vec![Value::I64(5)])
        .unwrap();
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::CallbackReturned);
    assert!(
        run.aggregates().len_u32() >= 2,
        "data and closure environment"
    );
    drop(run);
    assert_eq!(
        vm.aggregates().len_u32(),
        0,
        "cancellation truncates the heap"
    );
}
