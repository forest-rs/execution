// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A VM keeps saved computations between invocations and resumes each once, with a fresh
//! allowance.

use execution_tape::{
    asm::{FunctionSig, ProgramBuilder, Reg},
    host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef},
    program::ValueType,
    trace::{TraceMask, TraceSink},
    value::{FuncId, FuncRef, Value},
    verifier::VerifiedProgram,
    vm::{
        CollectError, ComputationId, InvocationOptions, Limits, ResumeError, RunStatus, Trap, Vm,
    },
};
use std::sync::Arc;

/// Answers `ask` with 5 and `make` with `(222,)`, for runs that do not yield.
struct Answers;
impl Host for Answers {
    fn call(
        &mut self,
        symbol: &str,
        _: SigHash,
        _: &[ValueRef<'_>],
        rets: &mut [Value],
        mut context: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        rets[0] = match symbol {
            "make" => Value::Agg(context.tuple_new(vec![Value::I64(222)])?),
            _ => Value::I64(5),
        };
        Ok(0)
    }
}

fn sig(args: &[ValueType], rets: &[ValueType]) -> FunctionSig {
    FunctionSig {
        arg_types: args.to_vec(),
        ret_types: rets.to_vec(),
    }
}

/// `entry(text)` boxes `text`, asks the host for a number, and returns `((text,), number)`.
fn asks() -> (Arc<VerifiedProgram>, FuncId) {
    let mut b = ProgramBuilder::new();
    let ask = b.host_sig_for(
        "ask",
        HostSig {
            args: vec![],
            rets: vec![ValueType::I64],
        },
    );
    let entry = b
        .declare_function(sig(&[ValueType::Str], &[ValueType::Agg]))
        .unwrap();
    let mut a = b.function_asm(entry).unwrap();
    let (boxed, number, result) = (
        a.alloc_register(ValueType::Agg),
        a.alloc_register(ValueType::I64),
        a.alloc_register(ValueType::Agg),
    );
    a.tuple_new(boxed, &[Reg(1)])
        .host_call_sig_id(Reg::EFFECT, ask, Reg::EFFECT, &[], &[number])
        .tuple_new(result, &[boxed, number])
        .ret(Reg::EFFECT, &[result]);
    b.define_function(entry, a).unwrap();
    (Arc::new(b.build_verified().unwrap()), entry)
}

/// Starts `asks` and saves it waiting at its host request.
fn save_asking(vm: &mut Vm<Answers>, entry: FuncRef, text: &str) -> ComputationId {
    let args = [Value::Str(text.into())];
    let mut invocation = vm
        .start(
            entry,
            &args,
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::AwaitingHost,
        "it waits at `ask`"
    );
    invocation.save().unwrap()
}

/// Resumes a saved `asks`, answers its request with `answer` and returns its text and number.
fn finish_asking(vm: &mut Vm<Answers>, id: ComputationId, answer: i64) -> (String, i64) {
    let mut invocation = vm
        .resume(id, InvocationOptions::default().yield_host_calls())
        .unwrap();
    assert_eq!(
        invocation.resume(0).unwrap(),
        RunStatus::AwaitingHost,
        "the resumed computation still waits at `ask`"
    );
    invocation
        .pending_host_call()
        .unwrap()
        .complete(&mut [Value::I64(answer)], Ok(0))
        .unwrap();
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::Complete,
        "answered, it returns"
    );
    let [Value::Agg(result)] = invocation.take_outputs().unwrap()[..] else {
        panic!("one tuple");
    };
    drop(invocation);
    let heap = vm.aggregates();
    let Value::Agg(boxed) = heap.tuple_get(result, 0).unwrap() else {
        panic!("a boxed string");
    };
    let Value::Str(text) = heap.tuple_get(boxed, 0).unwrap() else {
        panic!("a string");
    };
    let Value::I64(number) = heap.tuple_get(result, 1).unwrap() else {
        panic!("a number");
    };
    (text, number)
}

#[test]
fn a_saved_request_outlives_other_runs_and_a_moving_collection() {
    let (program, entry) = asks();
    let mut vm = Vm::new(Answers, Limits::default());
    let entry = FuncRef::new(vm.install(program).unwrap(), entry);
    // Garbage below the saved computation's own aggregates, so collection moves them.
    vm.aggregates_mut().tuple_new(vec![]).unwrap();
    let id = save_asking(&mut vm, entry, "kept");
    assert_eq!(vm.heap_stats().saved_computations, 1);
    assert!(vm.heap_stats().saved_bytes > 0);

    // Other runs share the heap meanwhile.
    for _ in 0..3 {
        vm.run(entry, &[Value::Str("other".into())], TraceMask::NONE, None)
            .unwrap();
    }
    let report = vm.collect().unwrap();
    assert!(report.moved > 0, "{report:?}");
    assert_eq!(report.aggregates.live, 1, "only the saved box survives");

    assert_eq!(finish_asking(&mut vm, id, 9), ("kept".into(), 9));
    assert_eq!(vm.heap_stats().saved_computations, 0);
    assert_eq!(vm.heap_stats().saved_bytes, 0);
}

#[test]
fn saved_computations_resume_independently_and_each_id_once() {
    let (program, entry) = asks();
    let mut vm = Vm::new(Answers, Limits::default());
    let entry = FuncRef::new(vm.install(program).unwrap(), entry);
    let first = save_asking(&mut vm, entry, "first");
    let second = save_asking(&mut vm, entry, "second");
    assert_ne!(first, second);

    assert_eq!(finish_asking(&mut vm, second, 2), ("second".into(), 2));
    assert!(matches!(
        vm.resume(second, InvocationOptions::default()),
        Err(ResumeError::Unknown)
    ));
    // A later computation may reuse the slot; the old ID still names nothing.
    let third = save_asking(&mut vm, entry, "third");
    assert!(matches!(
        vm.resume(second, InvocationOptions::default()),
        Err(ResumeError::Unknown)
    ));
    assert_eq!(finish_asking(&mut vm, first, 1), ("first".into(), 1));
    assert_eq!(finish_asking(&mut vm, third, 3), ("third".into(), 3));
}

#[test]
fn dropping_a_resumed_invocation_cancels_it_and_keeps_its_older_sibling() {
    let (program, entry) = asks();
    let mut vm = Vm::new(Answers, Limits::default());
    let entry = FuncRef::new(vm.install(program).unwrap(), entry);
    let older = save_asking(&mut vm, entry, "older");
    let doomed = save_asking(&mut vm, entry, "doomed");
    let before = vm.aggregates().len_u32();
    {
        let mut invocation = vm
            .resume(doomed, InvocationOptions::default().yield_host_calls())
            .unwrap();
        invocation
            .pending_host_call()
            .unwrap()
            .complete(&mut [Value::I64(0)], Ok(0))
            .unwrap();
        // The result tuple is allocated before the invocation is dropped unfinished.
        assert_eq!(invocation.resume(1).unwrap(), RunStatus::Suspended);
        assert_eq!(invocation.aggregates().len_u32(), before + 1);
    }
    assert_eq!(
        vm.aggregates().len_u32(),
        before,
        "only its own lease rolled back"
    );
    assert!(matches!(
        vm.resume(doomed, InvocationOptions::default()),
        Err(ResumeError::Unknown)
    ));
    assert_eq!(finish_asking(&mut vm, older, 4), ("older".into(), 4));
}

#[test]
fn a_forgotten_resumed_invocation_never_frees_its_id() {
    let (program, entry) = asks();
    let mut vm = Vm::new(Answers, Limits::default());
    let entry = FuncRef::new(vm.install(program).unwrap(), entry);
    let lost = save_asking(&mut vm, entry, "lost");
    core::mem::forget(
        vm.resume(lost, InvocationOptions::default().yield_host_calls())
            .unwrap(),
    );
    let next = save_asking(&mut vm, entry, "next");
    assert!(matches!(
        vm.resume(lost, InvocationOptions::default()),
        Err(ResumeError::Unknown)
    ));
    assert_eq!(finish_asking(&mut vm, next, 6), ("next".into(), 6));
}

/// `entry()` adds one twenty times and returns the sum: 22 instructions.
fn counts() -> (Arc<VerifiedProgram>, FuncId) {
    let mut b = ProgramBuilder::new();
    let entry = b.declare_function(sig(&[], &[ValueType::I64])).unwrap();
    let mut a = b.function_asm(entry).unwrap();
    let (sum, one) = (
        a.alloc_register(ValueType::I64),
        a.alloc_register(ValueType::I64),
    );
    a.const_i64(one, 1).const_i64(sum, 0);
    for _ in 0..20 {
        a.i64_add(sum, sum, one);
    }
    a.ret(Reg::EFFECT, &[sum]);
    b.define_function(entry, a).unwrap();
    (Arc::new(b.build_verified().unwrap()), entry)
}

#[test]
fn each_resumed_invocation_starts_a_fresh_allowance() {
    let (program, entry) = counts();
    let limits = Limits {
        fuel: 12,
        ..Limits::default()
    };
    let mut vm = Vm::new(Answers, limits);
    let entry = FuncRef::new(vm.install(program).unwrap(), entry);
    let error = vm.run(entry, &[], TraceMask::NONE, None).unwrap_err();
    assert!(matches!(error.trap, Trap::FuelExceeded), "{error:?}");

    let mut invocation = vm.start(entry, &[], InvocationOptions::default()).unwrap();
    assert_eq!(invocation.resume(11).unwrap(), RunStatus::Suspended);
    let id = invocation.save().unwrap();
    let mut invocation = vm.resume(id, InvocationOptions::default()).unwrap();
    assert_eq!(invocation.remaining_fuel(), 12);
    assert_eq!(invocation.resume(u64::MAX).unwrap(), RunStatus::Complete);
    assert_eq!(invocation.result().unwrap().unwrap(), [Value::I64(20)]);
}

/// `entry()` calls `mid`, which asks the host and then calls `leaf`.
fn nested() -> (Arc<VerifiedProgram>, FuncId) {
    let mut b = ProgramBuilder::new();
    let ask = b.host_sig_for(
        "ask",
        HostSig {
            args: vec![],
            rets: vec![ValueType::I64],
        },
    );
    let entry = b.declare_function(sig(&[], &[ValueType::I64])).unwrap();
    let mid = b.declare_function(sig(&[], &[ValueType::I64])).unwrap();
    let leaf = b.declare_function(sig(&[], &[ValueType::I64])).unwrap();
    let mut a = b.function_asm(leaf).unwrap();
    let n = a.alloc_register(ValueType::I64);
    a.const_i64(n, 1).ret(Reg::EFFECT, &[n]);
    b.define_function(leaf, a).unwrap();
    let mut a = b.function_asm(mid).unwrap();
    let n = a.alloc_register(ValueType::I64);
    a.host_call_sig_id(Reg::EFFECT, ask, Reg::EFFECT, &[], &[n])
        .call(Reg::EFFECT, leaf, Reg::EFFECT, &[], &[n])
        .ret(Reg::EFFECT, &[n]);
    b.define_function(mid, a).unwrap();
    let mut a = b.function_asm(entry).unwrap();
    let n = a.alloc_register(ValueType::I64);
    a.call(Reg::EFFECT, mid, Reg::EFFECT, &[], &[n])
        .ret(Reg::EFFECT, &[n]);
    b.define_function(entry, a).unwrap();
    (Arc::new(b.build_verified().unwrap()), entry)
}

#[test]
fn a_resumed_computation_keeps_its_frames_and_activation_ids() {
    let (program, entry) = nested();
    let limits = Limits {
        max_call_depth: 2,
        ..Limits::default()
    };
    let mut vm = Vm::new(Answers, limits);
    let entry = FuncRef::new(vm.install(program).unwrap(), entry);
    let mut invocation = vm
        .start(entry, &[], InvocationOptions::default().yield_host_calls())
        .unwrap();
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::AwaitingHost
    );
    let before: Vec<_> = invocation
        .inspect()
        .frames()
        .map(|frame| frame.activation)
        .collect();
    let id = invocation.save().unwrap();

    let mut invocation = vm
        .resume(id, InvocationOptions::default().yield_host_calls())
        .unwrap();
    let after: Vec<_> = invocation
        .inspect()
        .frames()
        .map(|frame| frame.activation)
        .collect();
    assert_eq!(before, after);
    invocation
        .pending_host_call()
        .unwrap()
        .complete(&mut [Value::I64(0)], Ok(0))
        .unwrap();
    // The two held frames still count: `leaf` would be the third.
    let error = invocation.resume(u64::MAX).unwrap_err();
    assert!(matches!(error.trap, Trap::CallDepthExceeded), "{error:?}");
}

#[test]
fn only_unobserved_waiting_invocations_on_a_borrowed_vm_are_saved() {
    let (program, entry) = asks();
    let mut vm = Vm::new(Answers, Limits::default());
    let entry = FuncRef::new(vm.install(program).unwrap(), entry);
    let args = [Value::Str("text".into())];

    // Finished: its result is taken instead.
    let mut invocation = vm
        .start(entry, &args, InvocationOptions::default())
        .unwrap();
    assert_eq!(invocation.resume(u64::MAX).unwrap(), RunStatus::Complete);
    let invocation = invocation.save().unwrap_err();
    assert!(invocation.result().unwrap().is_ok());
    drop(invocation);

    // Observed: observers belong to one invocation.
    let mut sink = Silent;
    let mut invocation = vm
        .start(
            entry,
            &args,
            InvocationOptions::default()
                .yield_host_calls()
                .trace(TraceMask::RUN, &mut sink),
        )
        .unwrap();
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::AwaitingHost
    );
    drop(invocation.save().unwrap_err());

    // Owned: saving would drop the VM with it.
    let (program, function) = asks();
    let callable = execution_tape::callable::Callable::function(program, function).unwrap();
    let mut owned = Vm::new(Answers, Limits::default());
    let unit = owned.install(Arc::clone(callable.program())).unwrap();
    let mut invocation = callable
        .start_owned(
            owned,
            unit,
            vec![Value::Str("text".into())],
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::AwaitingHost
    );
    drop(invocation.save().unwrap_err());

    // A resumed computation cannot be observed yet, and refusing keeps it saved.
    let id = save_asking(&mut vm, entry, "kept");
    let mut sink = Silent;
    assert!(matches!(
        vm.resume(
            id,
            InvocationOptions::default().trace(TraceMask::RUN, &mut sink)
        ),
        Err(ResumeError::Observed)
    ));
    assert_eq!(finish_asking(&mut vm, id, 7), ("kept".into(), 7));
}

/// `entry()` asks the host to `make` a tuple and returns it; `spin()` returns 4, for the host
/// to call back.
fn makes() -> (Arc<VerifiedProgram>, FuncId, FuncId) {
    let mut b = ProgramBuilder::new();
    let make = b.host_sig_for(
        "make",
        HostSig {
            args: vec![],
            rets: vec![ValueType::Agg],
        },
    );
    let entry = b.declare_function(sig(&[], &[ValueType::Agg])).unwrap();
    let spin = b.declare_function(sig(&[], &[ValueType::I64])).unwrap();
    let mut a = b.function_asm(spin).unwrap();
    let n = a.alloc_register(ValueType::I64);
    a.const_i64(n, 1).i64_add(n, n, n).i64_add(n, n, n);
    a.ret(Reg::EFFECT, &[n]);
    b.define_function(spin, a).unwrap();
    let mut a = b.function_asm(entry).unwrap();
    let made = a.alloc_register(ValueType::Agg);
    a.host_call_sig_id(Reg::EFFECT, make, Reg::EFFECT, &[], &[made])
        .ret(Reg::EFFECT, &[made]);
    b.define_function(entry, a).unwrap();
    (Arc::new(b.build_verified().unwrap()), entry, spin)
}

#[test]
fn a_request_with_staged_host_aggregates_is_not_saved() {
    let (program, entry, _) = makes();
    let mut vm = Vm::new(Answers, Limits::default());
    let entry = FuncRef::new(vm.install(program).unwrap(), entry);
    let mut invocation = vm
        .start(entry, &[], InvocationOptions::default().yield_host_calls())
        .unwrap();
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::AwaitingHost
    );
    let staged = invocation
        .pending_host_call()
        .unwrap()
        .context()
        .tuple_new(vec![Value::I64(111)])
        .unwrap();
    // Only the host's handle reaches the staged tuple, and it would alias what later runs
    // allocate.
    let mut invocation = invocation.save().unwrap_err();
    let mut request = invocation.pending_host_call().unwrap();
    assert_eq!(request.context().tuple_len(staged), Ok(1));
    request.complete(&mut [Value::Agg(staged)], Ok(0)).unwrap();
    assert_eq!(invocation.resume(u64::MAX).unwrap(), RunStatus::Complete);
    let [Value::Agg(made)] = invocation.take_outputs().unwrap()[..] else {
        panic!("one tuple");
    };
    drop(invocation);
    assert_eq!(vm.aggregates().tuple_get(made, 0).unwrap(), Value::I64(111));
}

#[test]
fn promoted_host_aggregates_survive_saving_and_pin_collection_to_their_computation() {
    let (program, entry, spin) = makes();
    let mut vm = Vm::new(Answers, Limits::default());
    let unit = vm.install(program).unwrap();
    let entry = FuncRef::new(unit, entry);
    // Garbage below, so a collection would move the promoted tuple.
    vm.aggregates_mut().tuple_new(vec![]).unwrap();
    let mut invocation = vm
        .start(entry, &[], InvocationOptions::default().yield_host_calls())
        .unwrap();
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::AwaitingHost
    );
    let staged = {
        let mut request = invocation.pending_host_call().unwrap();
        let staged = request.context().tuple_new(vec![Value::I64(111)]).unwrap();
        // Calling back promotes the staged tuple into the heap.
        request
            .call_guest(Value::Func(FuncRef::new(unit, spin)), vec![])
            .unwrap();
        staged
    };
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::CallbackReturned
    );
    let id = invocation.save().unwrap();

    // Another run commits its own tuple meanwhile; nothing aliases.
    let [Value::Agg(other)] = vm.run(entry, &[], TraceMask::NONE, None).unwrap()[..] else {
        panic!("one tuple");
    };
    assert_ne!(other, staged);
    assert_eq!(
        vm.collect(),
        Err(CollectError::SavedHostHandles { computation: id })
    );

    let mut invocation = vm
        .resume(id, InvocationOptions::default().yield_host_calls())
        .unwrap();
    let mut request = invocation.pending_host_call().unwrap();
    assert_eq!(request.take_callback_results().unwrap(), [Value::I64(4)]);
    // The host keeps building on the same request after resuming.
    let wrapped = request
        .context()
        .tuple_new(vec![Value::Agg(staged)])
        .unwrap();
    request.complete(&mut [Value::Agg(wrapped)], Ok(0)).unwrap();
    assert_eq!(invocation.resume(u64::MAX).unwrap(), RunStatus::Complete);
    let [Value::Agg(made)] = invocation.take_outputs().unwrap()[..] else {
        panic!("one tuple");
    };
    drop(invocation);
    let heap = vm.aggregates();
    let Value::Agg(inner) = heap.tuple_get(made, 0).unwrap() else {
        panic!("the promoted tuple");
    };
    assert_eq!(heap.tuple_get(inner, 0).unwrap(), Value::I64(111));
    assert!(vm.collect().is_ok(), "the request completed");
}

#[test]
// Panic-abort targets, including default WASI, cannot catch the panic.
#[cfg(panic = "unwind")]
fn clearing_the_heap_of_saved_computations_panics_instead_of_aliasing() {
    let (program, entry) = asks();
    let mut vm = Vm::new(Answers, Limits::default());
    let entry = FuncRef::new(vm.install(program).unwrap(), entry);
    let id = save_asking(&mut vm, entry, "kept");
    for clear in [
        |vm: &mut Vm<Answers>| vm.aggregates_mut().clear(),
        |vm: &mut Vm<Answers>| drop(vm.aggregates_mut().take_nodes()),
    ] {
        let cleared = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| clear(&mut vm)));
        assert!(cleared.is_err());
    }
    // Replacing the heap outright is caught before the saved handles are used.
    let fresh = execution_tape::aggregates::AggHeap::new();
    let previous = core::mem::replace(vm.aggregates_mut(), fresh);
    let resumed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drop(vm.resume(id, InvocationOptions::default()));
    }));
    assert!(resumed.is_err());
    *vm.aggregates_mut() = previous;
    assert_eq!(finish_asking(&mut vm, id, 3), ("kept".into(), 3));
    // With nothing saved, clearing is allowed again.
    vm.aggregates_mut().clear();
}

#[test]
fn another_vms_id_is_refused() {
    let (program, entry) = asks();
    let mut a = Vm::new(Answers, Limits::default());
    let mut b = Vm::new(Answers, Limits::default());
    let entry_a = FuncRef::new(a.install(Arc::clone(&program)).unwrap(), entry);
    let entry_b = FuncRef::new(b.install(program).unwrap(), entry);
    let from_a = save_asking(&mut a, entry_a, "a");
    let from_b = save_asking(&mut b, entry_b, "b");
    assert!(matches!(
        b.resume(from_a, InvocationOptions::default()),
        Err(ResumeError::ForeignVm)
    ));
    // The ID's VM checks come before the observer check.
    let mut sink = Silent;
    assert!(matches!(
        b.resume(
            from_a,
            InvocationOptions::default().trace(TraceMask::RUN, &mut sink)
        ),
        Err(ResumeError::ForeignVm)
    ));
    assert_eq!(finish_asking(&mut b, from_b, 2), ("b".into(), 2));
    assert_eq!(finish_asking(&mut a, from_a, 1), ("a".into(), 1));
}

/// `entry()` suspends with `need` to its embedding and returns the answer plus one.
fn suspends() -> (Arc<VerifiedProgram>, FuncId) {
    let mut b = ProgramBuilder::new();
    let need = b
        .declare_tag_with_results("need", &[], &[ValueType::I64.into()])
        .unwrap();
    let entry = b.declare_function(sig(&[], &[ValueType::I64])).unwrap();
    let mut a = b.function_asm(entry).unwrap();
    let (answer, one) = (
        a.alloc_register(ValueType::I64),
        a.alloc_register(ValueType::I64),
    );
    a.suspend(Reg::EFFECT, need, Reg::EFFECT, &[], &[answer])
        .const_i64(one, 1)
        .i64_add(answer, answer, one)
        .ret(Reg::EFFECT, &[answer]);
    b.define_function(entry, a).unwrap();
    (Arc::new(b.build_verified().unwrap()), entry)
}

#[test]
fn a_computation_saved_at_a_yielded_suspension_is_answered_later() {
    let (program, entry) = suspends();
    let mut vm = Vm::new(Answers, Limits::default());
    let entry = FuncRef::new(vm.install(program).unwrap(), entry);
    let mut invocation = vm
        .start(entry, &[], InvocationOptions::default().yield_suspensions())
        .unwrap();
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::AwaitingHandler
    );
    let id = invocation.save().unwrap();
    let (asking, ask) = asks();
    let ask = FuncRef::new(vm.install(asking).unwrap(), ask);
    vm.run(ask, &[Value::Str("other".into())], TraceMask::NONE, None)
        .unwrap();

    let mut invocation = vm
        .resume(id, InvocationOptions::default().yield_suspensions())
        .unwrap();
    assert_eq!(
        invocation.resume(0).unwrap(),
        RunStatus::AwaitingHandler,
        "still waiting"
    );
    invocation
        .pending_suspension()
        .unwrap()
        .resume(vec![Value::I64(41)])
        .unwrap();
    assert_eq!(invocation.resume(u64::MAX).unwrap(), RunStatus::Complete);
    assert_eq!(invocation.result().unwrap().unwrap(), [Value::I64(42)]);
}

/// `entry(text)` asks the host twice, boxing as it goes: `((((text,), x),), y)`.
fn asks_twice() -> (Arc<VerifiedProgram>, FuncId) {
    let mut b = ProgramBuilder::new();
    let ask = b.host_sig_for(
        "ask",
        HostSig {
            args: vec![],
            rets: vec![ValueType::I64],
        },
    );
    let entry = b
        .declare_function(sig(&[ValueType::Str], &[ValueType::Agg]))
        .unwrap();
    let mut a = b.function_asm(entry).unwrap();
    let (first, x, second, y, third) = (
        a.alloc_register(ValueType::Agg),
        a.alloc_register(ValueType::I64),
        a.alloc_register(ValueType::Agg),
        a.alloc_register(ValueType::I64),
        a.alloc_register(ValueType::Agg),
    );
    a.tuple_new(first, &[Reg(1)])
        .host_call_sig_id(Reg::EFFECT, ask, Reg::EFFECT, &[], &[x])
        .tuple_new(second, &[first, x])
        .host_call_sig_id(Reg::EFFECT, ask, Reg::EFFECT, &[], &[y])
        .tuple_new(third, &[second, y])
        .ret(Reg::EFFECT, &[third]);
    b.define_function(entry, a).unwrap();
    (Arc::new(b.build_verified().unwrap()), entry)
}

/// Resumes `id`, answers one request with `answer`, and saves it again or returns its result.
fn answer_once(
    vm: &mut Vm<Answers>,
    id: ComputationId,
    answer: i64,
) -> Result<Value, ComputationId> {
    let mut invocation = vm
        .resume(id, InvocationOptions::default().yield_host_calls())
        .unwrap();
    invocation
        .pending_host_call()
        .unwrap()
        .complete(&mut [Value::I64(answer)], Ok(0))
        .unwrap();
    match invocation.resume(u64::MAX).unwrap() {
        RunStatus::AwaitingHost => Err(invocation.save().unwrap()),
        RunStatus::Complete => Ok(invocation.take_outputs().unwrap().remove(0)),
        status => panic!("unexpected {status:?}"),
    }
}

#[test]
fn interleaved_computations_allocate_across_leases_and_collections() {
    let (program, entry) = asks_twice();
    let mut vm = Vm::new(Answers, Limits::default());
    let entry = FuncRef::new(vm.install(program).unwrap(), entry);
    let start = |vm: &mut Vm<Answers>, text: &str| {
        let args = [Value::Str(text.into())];
        let mut invocation = vm
            .start(
                entry,
                &args,
                InvocationOptions::default().yield_host_calls(),
            )
            .unwrap();
        assert_eq!(
            invocation.resume(u64::MAX).unwrap(),
            RunStatus::AwaitingHost
        );
        invocation.save().unwrap()
    };
    let a = start(&mut vm, "a");
    let b = start(&mut vm, "b");
    let a = answer_once(&mut vm, a, 1).unwrap_err();
    vm.aggregates_mut().tuple_new(vec![]).unwrap();
    assert!(vm.collect().unwrap().moved > 0);
    let b = answer_once(&mut vm, b, 2).unwrap_err();
    vm.aggregates_mut().tuple_new(vec![]).unwrap();
    assert!(vm.collect().unwrap().moved > 0);
    let Value::Agg(a) = answer_once(&mut vm, a, 3).unwrap() else {
        panic!("a tuple");
    };
    let a = vm.root(vec![Value::Agg(a)]).unwrap();
    vm.collect().unwrap();
    let Value::Agg(b) = answer_once(&mut vm, b, 4).unwrap() else {
        panic!("a tuple");
    };
    let read = |vm: &Vm<Answers>, outer: execution_tape::value::AggHandle| {
        let heap = vm.aggregates();
        let Value::Agg(second) = heap.tuple_get(outer, 0).unwrap() else {
            panic!("second");
        };
        let Value::Agg(first) = heap.tuple_get(second, 0).unwrap() else {
            panic!("first");
        };
        (
            heap.tuple_get(first, 0).unwrap(),
            heap.tuple_get(second, 1).unwrap(),
            heap.tuple_get(outer, 1).unwrap(),
        )
    };
    assert_eq!(
        read(&vm, b),
        (Value::Str("b".into()), Value::I64(2), Value::I64(4))
    );
    let [Value::Agg(a)] = vm.resolve(&a).unwrap()[..] else {
        panic!("rooted tuple");
    };
    assert_eq!(
        read(&vm, a),
        (Value::Str("a".into()), Value::I64(1), Value::I64(3))
    );
}

#[derive(Default)]
struct Silent;
impl TraceSink for Silent {}
