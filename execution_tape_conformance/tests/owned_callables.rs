// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Consumer-style ownership, admission and replay of escaped executable closures.

use execution_tape::{
    aggregates::AggHeap,
    asm::{FunctionAbi, FunctionSig, ProgramBuilder},
    callable::{Callable, CallableError},
    contract::Contract,
    host::{Host, HostContext, HostError, SigHash, ValueRef},
    program::ValueType,
    recording::{RecordLimits, Recorder, ReplayOutcome},
    trace::TraceSink,
    value::{Closure, FuncId, Value},
    verifier::VerifiedProgram,
    vm::{DataWork, InvocationOptions, Limits, RunStatus, Trap, Vm},
};
use std::sync::Arc;

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
        panic!("no host calls");
    }
}
fn signature(args: &[ValueType], rets: &[ValueType]) -> FunctionSig {
    FunctionSig {
        arg_types: args.to_vec(),
        ret_types: rets.to_vec(),
    }
}
fn maker() -> (Arc<VerifiedProgram>, FuncId) {
    let mut b = ProgramBuilder::new();
    let integer = b.contract(Contract::Kind(ValueType::I64)).unwrap();
    let env = b.contract(Contract::Tuple(&[Some(integer)])).unwrap();
    let closure = b
        .contract(Contract::Closure {
            args: &[integer],
            rets: &[integer],
        })
        .unwrap();
    let body = b
        .declare_function(FunctionAbi {
            args: vec![env.into(), integer.into()],
            rets: vec![integer.into()],
        })
        .unwrap();
    let mut a = b.function_asm(body).unwrap();
    let capture = a.alloc_register(ValueType::I64);
    let answer = a.alloc_register(ValueType::I64);
    a.tuple_get(capture, 1, 0)
        .i64_add(answer, capture, 2)
        .ret(0, &[answer]);
    b.define_function(body, a).unwrap();
    let factory = b
        .declare_function(FunctionAbi {
            args: vec![integer.into()],
            rets: vec![closure.into()],
        })
        .unwrap();
    let mut a = b.function_asm(factory).unwrap();
    let env = a.alloc_register(ValueType::Agg);
    let function = a.alloc_register(ValueType::Func);
    let result = a.alloc_register(ValueType::Closure);
    a.tuple_new(env, &[1])
        .const_func(function, body)
        .closure_new(result, function, env)
        .ret(0, &[result]);
    b.define_function(factory, a).unwrap();
    (Arc::new(b.build_verified().unwrap()), factory)
}
fn escaped() -> Callable {
    let (program, factory) = maker();
    let factory = Callable::function(program, factory).unwrap();
    let mut vm = Vm::new(NoHost, Limits::default());
    let args = [Value::I64(40)];
    let mut invocation = factory
        .start(&mut vm, &args, InvocationOptions::default())
        .unwrap();
    invocation.resume(u64::MAX).unwrap();
    invocation.retain_callable(0).unwrap()
}

fn finish(callable: &Callable, args: &[Value]) -> Vec<Value> {
    let mut vm = Vm::new(NoHost, Limits::default());
    let mut invocation = callable
        .start(&mut vm, args, InvocationOptions::default())
        .unwrap();
    while invocation.resume(1).unwrap() == RunStatus::Suspended {}
    invocation.take_outputs().unwrap()
}

#[test]
fn escaped_closure_owns_code_and_captures_after_creator_vm_and_frames_disappear() {
    let callable = escaped();
    let weak = Arc::downgrade(callable.program());
    assert_eq!(
        callable
            .signature()
            .arg_types()
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        [ValueType::I64]
    );
    assert_eq!(
        callable
            .signature()
            .ret_types()
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        [ValueType::I64]
    );
    let clone = callable.clone();
    assert_eq!(callable, clone);
    let (env, heap) = callable.environment().unwrap();
    assert_eq!(heap.tuple_get(env, 0).unwrap(), Value::I64(40));
    assert_eq!(finish(&callable, &[Value::I64(2)]), [Value::I64(42)]);
    drop(callable);
    assert!(weak.upgrade().is_some());
    assert_eq!(finish(&clone, &[Value::I64(3)]), [Value::I64(43)]);
    drop(clone);
    assert!(weak.upgrade().is_none());
}

#[test]
fn invocation_imports_are_transactional_and_do_not_accumulate_in_a_long_lived_vm() {
    let callable = escaped();
    let mut vm = Vm::new(NoHost, Limits::default());
    let retained = vm.aggregates_mut().tuple_new(vec![Value::I64(99)]).unwrap();
    for cancel in [true, false, false, true] {
        let args = [Value::I64(2)];
        let mut invocation = callable
            .start(&mut vm, &args, InvocationOptions::default())
            .unwrap();
        assert_eq!(invocation.aggregates().len_u32(), 2);
        if !cancel {
            assert_eq!(invocation.resume(u64::MAX).unwrap(), RunStatus::Complete);
            assert_eq!(invocation.result().unwrap().unwrap(), [Value::I64(42)]);
        }
        drop(invocation);
        assert_eq!(vm.aggregates().len_u32(), 1);
        assert_eq!(
            vm.aggregates().tuple_get(retained, 0).unwrap(),
            Value::I64(99)
        );
    }
}

#[test]
fn capture_import_work_is_admitted_before_allocation_and_preserved_in_replay() {
    let callable = escaped(); // one tuple + one field = two aggregate-work units
    for limit in [1, 2] {
        let mut vm = Vm::new(
            NoHost,
            Limits {
                max_aggregate_work: limit,
                ..Limits::default()
            },
        );
        let mut recorder = Recorder::new(Arc::clone(callable.program()), RecordLimits::default());
        let args = [Value::I64(2)];
        let result = callable.start(
            &mut vm,
            &args,
            InvocationOptions::default().trace(recorder.mask(), &mut recorder),
        );
        if limit == 1 {
            let error = result.unwrap_err();
            assert!(matches!(
                error.trap,
                Trap::AggregateWorkExceeded {
                    limit: 1,
                    used: 0,
                    requested: 2
                }
            ));
            assert_eq!(vm.aggregates().len_u32(), 0);
            assert!(recorder.finish().invocation().is_none());
        } else {
            let mut invocation = result.unwrap();
            invocation.resume(u64::MAX).unwrap();
            drop(invocation);
            assert_eq!(vm.aggregates().len_u32(), 0);
            let record = recorder.finish();
            assert_eq!(
                record.invocation().unwrap().work,
                DataWork {
                    aggregate: 2,
                    payload: 0
                }
            );
            // The recorded body receives the hidden environment as its first argument.
            assert_eq!(record.invocation().unwrap().args.len(), 2);
            let ReplayOutcome::Returned(result) = record.replay().unwrap() else {
                panic!("expected return");
            };
            assert_eq!(result.values(), [Value::I64(42)]);
        }
    }
}

#[test]
fn imported_capture_work_cannot_be_replenished_by_resume_or_checked_replay() {
    let mut b = ProgramBuilder::new();
    let body = b
        .declare_function(signature(&[ValueType::Agg], &[ValueType::Agg]))
        .unwrap();
    let mut a = b.function_asm(body).unwrap();
    let result = a.alloc_register(ValueType::Agg);
    a.tuple_new(result, &[1]).ret(0, &[result]);
    b.define_function(body, a).unwrap();
    let program = Arc::new(b.build_verified().unwrap());
    let mut heap = AggHeap::new();
    let env = heap.tuple_new(vec![Value::I64(7)]).unwrap();
    let callable = Callable::retain(
        Arc::clone(&program),
        &heap,
        ValueRef::Closure(Closure { func: body, env }),
    )
    .unwrap();
    for limit in [3, 4] {
        let mut vm = Vm::new(
            NoHost,
            Limits {
                max_aggregate_work: limit,
                ..Limits::default()
            },
        );
        let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
        let mut invocation = callable
            .start(
                &mut vm,
                &[],
                InvocationOptions::default().trace(recorder.mask(), &mut recorder),
            )
            .unwrap();
        let result = invocation.resume(1);
        if limit == 3 {
            assert!(matches!(
                result.unwrap_err().trap,
                Trap::AggregateWorkExceeded {
                    limit: 3,
                    used: 2,
                    requested: 2
                }
            ));
            assert!(invocation.resume(100).is_err());
        } else {
            assert_eq!(result.unwrap(), RunStatus::Suspended);
            invocation.resume(1).unwrap();
            assert_eq!(invocation.aggregates().len_u32(), 2);
        }
        drop(invocation);
        let record = recorder.finish();
        match (limit, record.replay().unwrap()) {
            (3, ReplayOutcome::Trapped(error)) => assert!(matches!(
                error.trap,
                Trap::AggregateWorkExceeded { used: 2, .. }
            )),
            (4, ReplayOutcome::Returned(values)) => assert_eq!(values.aggregates().len_u32(), 2),
            _ => panic!("wrong checked replay outcome"),
        }
    }
}

#[test]
fn invalid_targets_and_environment_contracts_fail_without_retaining_vm_allocations() {
    let (p, _) = maker();
    assert!(matches!(
        Callable::function(Arc::clone(&p), FuncId(u32::MAX)),
        Err(CallableError::MissingFunction { .. })
    ));
    assert!(matches!(
        Callable::retain(Arc::clone(&p), &AggHeap::new(), ValueRef::I64(4)),
        Err(CallableError::NotCallable {
            actual: ValueType::I64
        })
    ));
    let mut heap = AggHeap::new();
    let env = heap.tuple_new(vec![Value::Bool(true)]).unwrap();
    let bad = Callable::retain(
        p,
        &heap,
        ValueRef::Closure(Closure {
            func: FuncId(0),
            env,
        }),
    )
    .unwrap();
    let args = [Value::I64(2)];
    let mut vm = Vm::new(NoHost, Limits::default());
    let error = bad
        .start(&mut vm, &args, InvocationOptions::default())
        .unwrap_err();
    assert!(matches!(error.trap, Trap::ContractMismatch { .. }));
    assert_eq!(vm.aggregates().len_u32(), 0);
}

#[test]
fn copied_payload_arguments_and_captures_share_the_guest_budget_and_replay_debit() {
    let mut b = ProgramBuilder::new();
    let body = b
        .declare_function(signature(
            &[ValueType::Agg, ValueType::Str],
            &[ValueType::Str],
        ))
        .unwrap();
    let mut a = b.function_asm(body).unwrap();
    a.ret(0, &[2]);
    b.define_function(body, a).unwrap();
    let program = Arc::new(b.build_verified().unwrap());
    let mut heap = AggHeap::new();
    let env = heap.tuple_new(vec![Value::Str("xy".into())]).unwrap();
    let callable = Callable::retain(
        Arc::clone(&program),
        &heap,
        ValueRef::Closure(Closure { func: body, env }),
    )
    .unwrap();
    drop(heap);
    let args = [Value::Str("abcde".into())];
    for limit in [6, 11, 12] {
        let mut vm = Vm::new(
            NoHost,
            Limits {
                max_payload_work: limit,
                ..Limits::default()
            },
        );
        let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
        let started = callable.start(
            &mut vm,
            &args,
            InvocationOptions::default().trace(recorder.mask(), &mut recorder),
        );
        if limit == 6 {
            assert!(matches!(
                started.unwrap_err().trap,
                Trap::PayloadWorkExceeded {
                    used: 0,
                    requested: 7,
                    ..
                }
            ));
            assert_eq!(vm.aggregates().len_u32(), 0);
            assert!(recorder.finish().invocation().is_none());
            continue;
        }
        let mut invocation = started.unwrap();
        assert_eq!(invocation.resume(0).unwrap(), RunStatus::Suspended);
        let result = invocation.resume(1);
        if limit == 11 {
            assert!(matches!(
                result.unwrap_err().trap,
                Trap::PayloadWorkExceeded {
                    used: 7,
                    requested: 5,
                    ..
                }
            ));
        } else {
            assert_eq!(result.unwrap(), RunStatus::Complete);
            assert_eq!(invocation.result().unwrap().unwrap(), args);
        }
        drop(invocation);
        let record = recorder.finish();
        assert_eq!(record.invocation().unwrap().work.payload, 7);
        match (limit, record.replay().unwrap()) {
            (11, ReplayOutcome::Trapped(error)) => assert!(matches!(
                error.trap,
                Trap::PayloadWorkExceeded {
                    used: 7,
                    requested: 5,
                    ..
                }
            )),
            (12, ReplayOutcome::Returned(values)) => assert_eq!(values.values(), args),
            _ => panic!("wrong payload replay outcome"),
        }
    }
}

#[test]
fn aliased_and_equal_but_distinct_captures_remain_distinct_after_import() {
    let mut b = ProgramBuilder::new();
    let integer = b.contract(Contract::Kind(ValueType::I64)).unwrap();
    let inner = b.contract(Contract::Tuple(&[Some(integer)])).unwrap();
    let env = b.contract(Contract::Tuple(&[Some(inner); 3])).unwrap();
    let body = b
        .declare_function(FunctionAbi {
            args: vec![env.into()],
            rets: vec![integer.into()],
        })
        .unwrap();
    let mut a = b.function_asm(body).unwrap();
    let tuple = a.alloc_register(ValueType::Agg);
    let answer = a.alloc_register(ValueType::I64);
    a.tuple_get(tuple, 1, 0)
        .tuple_get(answer, tuple, 0)
        .ret(0, &[answer]);
    b.define_function(body, a).unwrap();
    let program = Arc::new(b.build_verified().unwrap());
    let mut heap = AggHeap::new();
    let first = heap.tuple_new(vec![Value::I64(42)]).unwrap();
    let equal = heap.tuple_new(vec![Value::I64(42)]).unwrap();
    let env = heap
        .tuple_new(vec![
            Value::Agg(first),
            Value::Agg(first),
            Value::Agg(equal),
        ])
        .unwrap();
    let callable = Callable::retain(
        program,
        &heap,
        ValueRef::Closure(Closure { func: body, env }),
    )
    .unwrap();
    drop(heap);
    let mut vm = Vm::new(NoHost, Limits::default());
    let mut run = callable
        .start(&mut vm, &[], InvocationOptions::default())
        .unwrap();
    let mut inspection = run.inspect();
    let root = inspection.frames().next().unwrap().activation;
    let Some(ValueRef::Agg(env)) = inspection.register(&root, 1).unwrap().value else {
        panic!("missing imported environment");
    };
    let first = run.aggregates().tuple_get(env, 0).unwrap();
    assert_eq!(first, run.aggregates().tuple_get(env, 1).unwrap());
    assert_ne!(first, run.aggregates().tuple_get(env, 2).unwrap());
    drop(inspection);
    run.resume(u64::MAX).unwrap();
    assert_eq!(run.result().unwrap().unwrap(), [Value::I64(42)]);
}

#[test]
fn a_returned_nested_closure_can_escape_another_vm_and_keep_its_original_code() {
    let mut b = ProgramBuilder::new();
    let integer = b.contract(Contract::Kind(ValueType::I64)).unwrap();
    let env = b.contract(Contract::Tuple(&[Some(integer)])).unwrap();
    let leaf = b
        .declare_function(FunctionAbi {
            args: vec![env.into(), integer.into()],
            rets: vec![integer.into()],
        })
        .unwrap();
    let mut a = b.function_asm(leaf).unwrap();
    let captured = a.alloc_register(ValueType::I64);
    let answer = a.alloc_register(ValueType::I64);
    a.tuple_get(captured, 1, 0)
        .i64_add(answer, captured, 2)
        .ret(0, &[answer]);
    b.define_function(leaf, a).unwrap();
    let outer = b
        .declare_function(FunctionAbi {
            args: vec![env.into()],
            rets: vec![ValueType::Closure.into()],
        })
        .unwrap();
    let mut a = b.function_asm(outer).unwrap();
    let function = a.alloc_register(ValueType::Func);
    let result = a.alloc_register(ValueType::Closure);
    a.const_func(function, leaf)
        .closure_new(result, function, 1)
        .ret(0, &[result]);
    b.define_function(outer, a).unwrap();
    let program = Arc::new(b.build_verified().unwrap());
    let mut heap = AggHeap::new();
    let env = heap.tuple_new(vec![Value::I64(40)]).unwrap();
    let outer = Callable::retain(
        Arc::clone(&program),
        &heap,
        ValueRef::Closure(Closure { func: outer, env }),
    )
    .unwrap();
    drop(heap);
    let mut vm = Vm::new(NoHost, Limits::default());
    let mut run = outer
        .start(&mut vm, &[], InvocationOptions::default())
        .unwrap();
    run.resume(u64::MAX).unwrap();
    let inner = run.retain_callable(0).unwrap();
    drop(run);
    drop(vm);
    drop(outer);
    assert_eq!(finish(&inner, &[Value::I64(2)]), [Value::I64(42)]);
}

#[test]
fn retaining_results_requires_a_successful_callable_and_an_exact_code_owner() {
    let (program, factory) = maker();
    let bound = Callable::function(Arc::clone(&program), factory).unwrap();
    let args = [Value::I64(40)];
    let mut vm = Vm::new(NoHost, Limits::default());
    let mut run = bound
        .start(&mut vm, &args, InvocationOptions::default())
        .unwrap();
    assert_eq!(
        run.retain_callable(0).unwrap_err(),
        CallableError::NoSuccessfulResult
    );
    run.resume(u64::MAX).unwrap();
    assert_eq!(
        run.retain_callable(1).unwrap_err(),
        CallableError::OutputIndex { index: 1, count: 1 }
    );
    let closure = run.retain_callable(0).unwrap();
    drop(run);
    let args = [Value::I64(2)];
    let mut run = closure
        .start(&mut vm, &args, InvocationOptions::default())
        .unwrap();
    run.resume(u64::MAX).unwrap();
    assert_eq!(
        run.retain_callable(0).unwrap_err(),
        CallableError::NotCallable {
            actual: ValueType::I64
        }
    );
    drop(run);
    let mut run = vm
        .start(&program, factory, &args, InvocationOptions::default())
        .unwrap();
    run.resume(u64::MAX).unwrap();
    assert_eq!(
        run.retain_callable(0).unwrap_err(),
        CallableError::MissingCodeOwner
    );
}

#[test]
fn snapshot_adoption_rejects_ambiguous_roots_and_non_environment_bodies() {
    let (program, factory) = maker();
    let heap = AggHeap::new();
    let snapshot = heap
        .snapshot(&[Value::Func(factory), Value::Func(factory)])
        .unwrap();
    assert!(matches!(
        Callable::from_snapshot(Arc::clone(&program), snapshot),
        Err(CallableError::RootCount { actual: 2 })
    ));
    let mut heap = AggHeap::new();
    let env = heap.tuple_new(vec![]).unwrap();
    assert!(matches!(
        Callable::retain(
            program,
            &heap,
            ValueRef::Closure(Closure { func: factory, env })
        ),
        Err(CallableError::EnvironmentArgument {
            actual: Some(ValueType::I64),
            ..
        })
    ));
}
