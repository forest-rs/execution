// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use super::*;
use crate::asm::{Catch, FunctionSig, ProgramBuilder, Reg};
use crate::contract::Contract;
use crate::host::{HostError, HostSig, SigHash, ValueRef};
use alloc::vec;

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
        panic!("requests are yielded")
    }
}

fn sig(args: &[ValueType], rets: &[ValueType]) -> FunctionSig {
    FunctionSig {
        arg_types: args.to_vec(),
        ret_types: rets.to_vec(),
    }
}

/// `entry()` catches `failed((7,))` as a reference, copies the reference into a second
/// register, overwrites the tuple register, and waits at `pause(text)`. It then rethrows the
/// copy into a handler that returns the payload's integer.
fn shared_exception() -> VerifiedProgram {
    let mut b = ProgramBuilder::new();
    let pause = b.host_sig_for(
        "pause",
        HostSig {
            args: vec![ValueType::Str],
            rets: vec![],
        },
    );
    let integer = b.contract(Contract::Kind(ValueType::I64)).unwrap();
    let boxed = b.contract(Contract::Tuple(&[Some(integer)])).unwrap();
    let failed = b.declare_tag("failed", &[boxed.into()]).unwrap();
    let text = b.const_str("text");
    let entry = b.declare_function(sig(&[], &[ValueType::I64])).unwrap();
    let mut a = b.function_asm(entry).unwrap();
    let (seven, tuple, caught, copy, label, unboxed, n) = (
        a.alloc_register(ValueType::I64),
        a.alloc_register(ValueType::Agg),
        a.alloc_register(ValueType::Exn),
        a.alloc_register(ValueType::Exn),
        a.alloc_register(ValueType::Str),
        a.alloc_register(ValueType::Agg),
        a.alloc_register(ValueType::I64),
    );
    a.const_i64(seven, 7).tuple_new(tuple, &[seven]);
    let handler = a.block(&[caught]);
    a.begin_region(&[Catch::AllRef(handler)]);
    b.emit_throw(&mut a, failed, &[tuple]).unwrap();
    a.end_region();
    a.place(handler).unwrap();
    a.mov(copy, caught)
        .tuple_new(tuple, &[])
        .const_pool(label, text)
        .host_call_sig_id(Reg::EFFECT, pause, Reg::EFFECT, &[label], &[]);
    let unbox = a.block(&[unboxed]);
    a.begin_region(&[Catch::Tag(failed, unbox)]);
    a.throw_ref(Reg::EFFECT, copy);
    a.end_region();
    a.place(unbox).unwrap();
    a.tuple_get(n, unboxed, 0).ret(Reg::EFFECT, &[n]);
    b.define_function(entry, a).unwrap();
    b.build_verified().unwrap()
}

fn paused(vm: &mut Vm<NoHost>) -> Invocation<'_, NoHost> {
    let unit = vm.install(shared_exception()).unwrap();
    vm.aggregates_mut().tuple_new(vec![]).unwrap();
    let mut run = vm
        .start(
            FuncRef::new(unit, FuncId(0)),
            &[],
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    run
}

/// The active frame's exception reference registers, in register order.
fn exceptions(run: &Invocation<'_, NoHost>) -> Vec<Arc<Exception>> {
    run.continuation
        .ctx
        .exns
        .iter()
        .flatten()
        .cloned()
        .collect()
}

/// Whether two registers share one exception, and the integer its payload tuple holds.
///
/// The clones taken to look are dropped before returning, so they cannot keep the exception
/// shared during a later collection.
fn shared_payload(exceptions: Vec<Arc<Exception>>, heap: &AggHeap) -> (bool, Value) {
    let [first, second] = &exceptions[..] else {
        panic!("two exception registers, got {}", exceptions.len())
    };
    let [Value::Agg(payload)] = first.payload() else {
        panic!("one tuple payload")
    };
    (
        Arc::ptr_eq(first, second),
        heap.tuple_get(*payload, 0).unwrap(),
    )
}

#[test]
fn registers_that_shared_an_exception_still_share_its_relocated_copy() {
    let mut vm = Vm::new(NoHost, Limits::default());
    let mut run = paused(&mut vm);
    assert_eq!(
        shared_payload(exceptions(&run), run.aggregates()),
        (true, Value::I64(7))
    );
    // Only the two registers hold the exception now.
    let report = run.collect().unwrap();
    assert_eq!(report.aggregates.reclaimed, 1, "only the filler goes");
    assert_eq!(
        shared_payload(exceptions(&run), run.aggregates()),
        (true, Value::I64(7)),
        "sharing survives relocation"
    );
    let request = run.pending_host_call().unwrap();
    request.complete(&mut [], Ok(0)).unwrap();
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::Complete);
    assert_eq!(run.result().unwrap().unwrap(), [Value::I64(7)]);
}

/// `body()` catches `failed((7,))` as a reference, copies it into a second register and parks
/// to `entry`, which handles `park` and then waits at `pause` holding the parked continuation.
/// Resumed, `body` rethrows the copy into a handler returning the payload's integer.
fn held_shared_exception() -> VerifiedProgram {
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
    let park = b.declare_tag_with_results("park", &[], &[]).unwrap();
    let thunk = b.call_sig(sig(&[], &[ValueType::I64])).unwrap();
    let entry = b.declare_function(sig(&[], &[ValueType::I64])).unwrap();
    let body = b.declare_function(sig(&[], &[ValueType::I64])).unwrap();

    let mut a = b.function_asm(body).unwrap();
    let (seven, tuple, caught, copy, unboxed, n) = (
        a.alloc_register(ValueType::I64),
        a.alloc_register(ValueType::Agg),
        a.alloc_register(ValueType::Exn),
        a.alloc_register(ValueType::Exn),
        a.alloc_register(ValueType::Agg),
        a.alloc_register(ValueType::I64),
    );
    a.const_i64(seven, 7).tuple_new(tuple, &[seven]);
    let handler = a.block(&[caught]);
    a.begin_region(&[Catch::AllRef(handler)]);
    b.emit_throw(&mut a, failed, &[tuple]).unwrap();
    a.end_region();
    a.place(handler).unwrap();
    a.mov(copy, caught)
        .tuple_new(tuple, &[])
        .suspend(Reg::EFFECT, park, Reg::EFFECT, &[], &[]);
    let unbox = a.block(&[unboxed]);
    a.begin_region(&[Catch::Tag(failed, unbox)]);
    a.throw_ref(Reg::EFFECT, copy);
    a.end_region();
    a.place(unbox).unwrap();
    a.tuple_get(n, unboxed, 0).ret(Reg::EFFECT, &[n]);
    b.define_function(body, a).unwrap();

    let mut a = b.function_asm(entry).unwrap();
    let (function, cont, parked, result) = (
        a.alloc_register(ValueType::Func),
        a.alloc_register(ValueType::Cont),
        a.alloc_register(ValueType::Cont),
        a.alloc_register(ValueType::I64),
    );
    a.const_func(function, body).cont_new(cont, thunk, function);
    let handle = a.block(&[parked]);
    a.begin_region(&[Catch::On(park, handle)]);
    a.resume(Reg::EFFECT, thunk, cont, Reg::EFFECT, &[], &[result]);
    a.end_region();
    a.ret(Reg::EFFECT, &[result]);
    a.place(handle).unwrap();
    a.host_call_sig_id(Reg::EFFECT, pause, Reg::EFFECT, &[], &[])
        .resume(Reg::EFFECT, thunk, parked, Reg::EFFECT, &[], &[result])
        .ret(Reg::EFFECT, &[result]);
    b.define_function(entry, a).unwrap();
    b.build_verified().unwrap()
}

#[test]
fn held_continuations_that_shared_an_exception_still_share_its_relocated_copy() {
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(held_shared_exception()).unwrap();
    vm.aggregates_mut().tuple_new(vec![]).unwrap();
    let mut run = vm
        .start(
            FuncRef::new(unit, FuncId(0)),
            &[],
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    let held = |run: &Invocation<'_, NoHost>| run.continuation.ctx.continuations.held_exceptions();
    assert_eq!(
        shared_payload(held(&run), run.aggregates()),
        (true, Value::I64(7))
    );
    let report = run.collect().unwrap();
    assert_eq!(report.aggregates.reclaimed, 1, "only the filler goes");
    assert_eq!(
        shared_payload(held(&run), run.aggregates()),
        (true, Value::I64(7)),
        "sharing survives relocation in a held continuation"
    );
    let request = run.pending_host_call().unwrap();
    request.complete(&mut [], Ok(0)).unwrap();
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::Complete);
    assert_eq!(run.result().unwrap().unwrap(), [Value::I64(7)]);
}

#[test]
fn a_dangling_register_refuses_collection_without_moving_anything() {
    let mut vm = Vm::new(NoHost, Limits::default());
    let mut run = paused(&mut vm);
    let aggs = run.continuation.ctx.aggs.clone();
    let strs = run.continuation.ctx.strs.clone();
    let length = run.aggregates().len_u32();
    let label = strs.last().copied().expect("the label register");
    run.continuation.ctx.strs[strs.len() - 1] = StrHandle(77);
    assert_eq!(run.collect(), Err(CollectError::DanglingPayload));
    assert_eq!(run.aggregates().len_u32(), length);
    assert_eq!(run.continuation.ctx.aggs, aggs);
    assert_eq!(exceptions(&run)[0].payload(), [Value::Agg(AggHandle(1))]);
    assert_eq!(run.heap_stats().collections, 0);

    run.continuation.ctx.strs[strs.len() - 1] = label;
    run.continuation.ctx.aggs[0] = AggHandle(99);
    assert_eq!(
        run.collect(),
        Err(CollectError::DanglingHandle {
            handle: AggHandle(99)
        })
    );
    assert_eq!(run.aggregates().len_u32(), length);
    run.continuation.ctx.aggs[0] = aggs[0];

    // The handler's payload register is not written yet: whatever its slot holds is ignored.
    run.continuation.ctx.aggs[1] = AggHandle(99);
    assert_eq!(run.collect().unwrap().aggregates.live, 2);
    assert_eq!(run.continuation.ctx.aggs[1], AggHandle(99));
    let request = run.pending_host_call().unwrap();
    request.complete(&mut [], Ok(0)).unwrap();
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::Complete);
    assert_eq!(run.result().unwrap().unwrap(), [Value::I64(7)]);
}

/// `entry()` returns what `outer()` returns. `ticking()` waits at `tick` three times, keeping
/// one more tuple and leaving one unreachable before each, and returns `(42,)`.
fn ticking() -> (VerifiedProgram, FuncId) {
    let mut b = ProgramBuilder::new();
    let outer = b.host_sig_for(
        "outer",
        HostSig {
            args: vec![],
            rets: vec![ValueType::Agg],
        },
    );
    let tick = b.host_sig_for(
        "tick",
        HostSig {
            args: vec![],
            rets: vec![],
        },
    );
    let entry = b.declare_function(sig(&[], &[ValueType::Agg])).unwrap();
    let ticking = b.declare_function(sig(&[], &[ValueType::Agg])).unwrap();
    let mut a = b.function_asm(entry).unwrap();
    let result = a.alloc_register(ValueType::Agg);
    a.host_call_sig_id(Reg::EFFECT, outer, Reg::EFFECT, &[], &[result])
        .ret(Reg::EFFECT, &[result]);
    b.define_function(entry, a).unwrap();
    let mut a = b.function_asm(ticking).unwrap();
    let (spare, first, second, n, result) = (
        a.alloc_register(ValueType::Agg),
        a.alloc_register(ValueType::Agg),
        a.alloc_register(ValueType::Agg),
        a.alloc_register(ValueType::I64),
        a.alloc_register(ValueType::Agg),
    );
    // Each overwritten `spare` leaves one tuple for the next collection to reclaim.
    a.tuple_new(spare, &[])
        .tuple_new(spare, &[])
        .host_call_sig_id(Reg::EFFECT, tick, Reg::EFFECT, &[], &[])
        .tuple_new(first, &[])
        .tuple_new(spare, &[])
        .host_call_sig_id(Reg::EFFECT, tick, Reg::EFFECT, &[], &[])
        .tuple_new(second, &[first])
        .tuple_new(spare, &[])
        .host_call_sig_id(Reg::EFFECT, tick, Reg::EFFECT, &[], &[])
        .const_i64(n, 42)
        .tuple_new(result, &[n])
        .ret(Reg::EFFECT, &[result]);
    b.define_function(ticking, a).unwrap();
    (b.build_verified().unwrap(), ticking)
}

/// Runs `ticking` as a callback of `outer` in a VM whose handle windows wrap at 8, collecting
/// at every `tick`, and returns `outer`'s result and the collections' outcomes.
fn tick_through_a_wrap(build: bool) -> (Value, Vec<Result<u32, CollectError>>) {
    let (program, ticking) = ticking();
    let mut vm = Vm::new(NoHost, Limits::default());
    vm.memory.set_window_limit(8);
    let unit = vm.install(program).unwrap();
    for _ in 0..4 {
        vm.aggregates_mut().tuple_new(vec![]).unwrap();
    }
    let mut run = vm
        .start(
            FuncRef::new(unit, FuncId(0)),
            &[],
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    let mut request = run.pending_host_call().unwrap();
    let mine = build.then(|| request.context().tuple_new(vec![Value::I64(7)]).unwrap());
    request
        .call_guest(Value::Func(FuncRef::new(unit, ticking)), vec![])
        .unwrap();
    let mut windows = Vec::new();
    for _ in 0..3 {
        assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
        windows.push(run.collect().map(|_| run.aggregates().handle_end()));
        let request = run.pending_host_call().unwrap();
        request.complete(&mut [], Ok(0)).unwrap();
    }
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::CallbackReturned);
    let mut request = run.pending_host_call().unwrap();
    let returned = request.take_callback_results().unwrap();
    let mut answer = match mine {
        Some(mine) => vec![Value::Agg(mine)],
        None => returned,
    };
    request.complete(&mut answer, Ok(0)).unwrap();
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::Complete);
    let Value::Agg(result) = run.result().unwrap().unwrap()[0] else {
        panic!("tuple result")
    };
    (run.aggregates().tuple_get(result, 0).unwrap(), windows)
}

#[test]
fn a_host_built_aggregate_is_never_moved_so_a_window_wrap_cannot_alias_it() {
    let (value, windows) = tick_through_a_wrap(true);
    assert_eq!(value, Value::I64(7));
    assert!(
        windows
            .iter()
            .all(|outcome| *outcome == Err(CollectError::HostHandles))
    );
}

#[test]
fn collections_wrap_their_window_without_changing_what_the_run_returns() {
    let (value, windows) = tick_through_a_wrap(false);
    assert_eq!(value, Value::I64(42));
    // Windows advance past every earlier handle, then wrap below the old one.
    let ends: Vec<u32> = windows.into_iter().map(Result::unwrap).collect();
    assert!(ends.windows(2).any(|pair| pair[1] < pair[0]), "{ends:?}");
}
