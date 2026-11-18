// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Consumer-style ownership, admission and replay of escaped executable closures.

use execution_tape::value::FuncRef;
use execution_tape::value::UnitId;
use execution_tape::{
    aggregates::AggHeap,
    asm::{FunctionAbi, FunctionSig, ProgramBuilder, Reg},
    callable::{Callable, CallableError},
    contract::Contract,
    host::{Host, HostContext, HostError, SigHash, ValueRef},
    program::ValueType,
    recording::{RecordLimits, Recorder, ReplayOutcome},
    trace::TraceSink,
    value::{Closure, FuncId, Value},
    verifier::VerifiedProgram,
    vm::{DataWork, Limits, RunStatus, Trap, TurnOptions, Vm},
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
    a.tuple_get(capture, Reg(1), 0)
        .i64_add(answer, capture, Reg(2))
        .ret(Reg::EFFECT, &[answer]);
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
    a.tuple_new(env, &[Reg(1)])
        .const_func(function, body)
        .closure_new(result, function, env)
        .ret(Reg::EFFECT, &[result]);
    b.define_function(factory, a).unwrap();
    (Arc::new(b.build_verified().unwrap()), factory)
}
fn escaped() -> Callable {
    let (program, factory) = maker();
    let factory = Callable::function(program, factory).unwrap();
    let mut vm = Vm::new(NoHost, Limits::default());
    let args = [Value::I64(40)];
    let unit = vm.install(Arc::clone(factory.program())).unwrap();
    let mut invocation = vm
        .start(factory.at(unit), args.to_vec(), TurnOptions::default())
        .unwrap();
    invocation.drive(u64::MAX).unwrap();
    let returned = returned_values(invocation);
    vm.retain_callable(&returned[0]).unwrap()
}

fn finish(callable: &Callable, args: &[Value]) -> Vec<Value> {
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(Arc::clone(callable.program())).unwrap();
    let mut invocation = vm
        .start(callable.at(unit), args.to_vec(), TurnOptions::default())
        .unwrap();
    while invocation.drive(1).unwrap() == RunStatus::Suspended {}
    returned_values(invocation)
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
        let unit = vm.install(Arc::clone(callable.program())).unwrap();
        let mut invocation = vm
            .start(callable.at(unit), args.to_vec(), TurnOptions::default())
            .unwrap();
        assert_eq!(invocation.aggregates().len_u32(), 2);
        if !cancel {
            assert_eq!(invocation.drive(u64::MAX).unwrap(), RunStatus::Complete);
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
        let unit = vm.install(Arc::clone(callable.program())).unwrap();
        let result = vm.start(
            callable.at(unit),
            args.to_vec(),
            TurnOptions::default().trace(recorder.mask(), &mut recorder),
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
            assert!(recorder.finish().entry().is_none());
        } else {
            let mut invocation = result.unwrap();
            invocation.drive(u64::MAX).unwrap();
            drop(invocation);
            assert_eq!(vm.aggregates().len_u32(), 0);
            let record = recorder.finish();
            assert_eq!(
                record.entry().unwrap().work,
                DataWork {
                    aggregate: 2,
                    payload: 0
                }
            );
            // The recorded body receives the hidden environment as its first argument.
            assert_eq!(record.entry().unwrap().args.len(), 2);
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
    a.tuple_new(result, &[Reg(1)]).ret(Reg::EFFECT, &[result]);
    b.define_function(body, a).unwrap();
    let program = Arc::new(b.build_verified().unwrap());
    let mut heap = AggHeap::new();
    let env = heap.tuple_new(vec![Value::I64(7)]).unwrap();
    let callable = Callable::retain(
        Arc::clone(&program),
        &heap,
        ValueRef::Closure(Closure {
            func: FuncRef::new(UnitId(0), body),
            env,
        }),
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
        let unit = vm.install(Arc::clone(callable.program())).unwrap();
        let mut invocation = vm
            .start(
                callable.at(unit),
                vec![],
                TurnOptions::default().trace(recorder.mask(), &mut recorder),
            )
            .unwrap();
        let result = invocation.drive(1);
        if limit == 3 {
            assert!(matches!(
                result.unwrap_err().trap,
                Trap::AggregateWorkExceeded {
                    limit: 3,
                    used: 2,
                    requested: 2
                }
            ));
            assert!(invocation.drive(100).is_err());
        } else {
            assert_eq!(result.unwrap(), RunStatus::Suspended);
            invocation.drive(1).unwrap();
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
            func: FuncRef::new(UnitId(0), FuncId(0)),
            env,
        }),
    )
    .unwrap();
    let args = [Value::I64(2)];
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(Arc::clone(bad.program())).unwrap();
    let error = vm
        .start(bad.at(unit), args.to_vec(), TurnOptions::default())
        .unwrap_err();
    assert!(matches!(error.trap, Trap::ContractMismatch { .. }));
    assert_eq!(vm.aggregates().len_u32(), 0);
}

#[test]
fn capture_payloads_share_the_guest_budget_without_copying_owned_user_arguments() {
    let mut b = ProgramBuilder::new();
    let body = b
        .declare_function(signature(
            &[ValueType::Agg, ValueType::Str],
            &[ValueType::Str],
        ))
        .unwrap();
    let mut a = b.function_asm(body).unwrap();
    a.ret(Reg::EFFECT, &[Reg(2)]);
    b.define_function(body, a).unwrap();
    let program = Arc::new(b.build_verified().unwrap());
    let mut heap = AggHeap::new();
    let env = heap.tuple_new(vec![Value::Str("xy".into())]).unwrap();
    let callable = Callable::retain(
        Arc::clone(&program),
        &heap,
        ValueRef::Closure(Closure {
            func: FuncRef::new(UnitId(0), body),
            env,
        }),
    )
    .unwrap();
    drop(heap);
    let args = [Value::Str("abcde".into())];
    for limit in [1, 6, 7] {
        let mut vm = Vm::new(
            NoHost,
            Limits {
                max_payload_work: limit,
                ..Limits::default()
            },
        );
        let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
        let unit = vm.install(Arc::clone(callable.program())).unwrap();
        let started = vm.start(
            callable.at(unit),
            args.to_vec(),
            TurnOptions::default().trace(recorder.mask(), &mut recorder),
        );
        if limit == 1 {
            assert!(matches!(
                started.unwrap_err().trap,
                Trap::PayloadWorkExceeded {
                    used: 0,
                    requested: 2,
                    ..
                }
            ));
            assert_eq!(vm.aggregates().len_u32(), 0);
            assert!(recorder.finish().entry().is_none());
            continue;
        }
        let mut invocation = started.unwrap();
        assert_eq!(invocation.drive(0).unwrap(), RunStatus::Suspended);
        let result = invocation.drive(1);
        if limit == 6 {
            assert!(matches!(
                result.unwrap_err().trap,
                Trap::PayloadWorkExceeded {
                    used: 2,
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
        assert_eq!(record.entry().unwrap().work.payload, 2);
        match (limit, record.replay().unwrap()) {
            (6, ReplayOutcome::Trapped(error)) => assert!(matches!(
                error.trap,
                Trap::PayloadWorkExceeded {
                    used: 2,
                    requested: 5,
                    ..
                }
            )),
            (7, ReplayOutcome::Returned(values)) => assert_eq!(values.values(), args),
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
    a.tuple_get(tuple, Reg(1), 0)
        .tuple_get(answer, tuple, 0)
        .ret(Reg::EFFECT, &[answer]);
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
        ValueRef::Closure(Closure {
            func: FuncRef::new(UnitId(0), body),
            env,
        }),
    )
    .unwrap();
    drop(heap);
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(Arc::clone(callable.program())).unwrap();
    let mut run = vm
        .start(callable.at(unit), vec![], TurnOptions::default())
        .unwrap();
    let mut inspection = run.inspect();
    let root = inspection.frames().next().unwrap().activation;
    let Some(ValueRef::Agg(env)) = inspection.register(&root, Reg(1)).unwrap().value else {
        panic!("missing imported environment");
    };
    let first = run.aggregates().tuple_get(env, 0).unwrap();
    assert_eq!(first, run.aggregates().tuple_get(env, 1).unwrap());
    assert_ne!(first, run.aggregates().tuple_get(env, 2).unwrap());
    drop(inspection);
    run.drive(u64::MAX).unwrap();
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
    a.tuple_get(captured, Reg(1), 0)
        .i64_add(answer, captured, Reg(2))
        .ret(Reg::EFFECT, &[answer]);
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
        .closure_new(result, function, Reg(1))
        .ret(Reg::EFFECT, &[result]);
    b.define_function(outer, a).unwrap();
    let program = Arc::new(b.build_verified().unwrap());
    let mut heap = AggHeap::new();
    let env = heap.tuple_new(vec![Value::I64(40)]).unwrap();
    let outer = Callable::retain(
        Arc::clone(&program),
        &heap,
        ValueRef::Closure(Closure {
            func: FuncRef::new(UnitId(0), outer),
            env,
        }),
    )
    .unwrap();
    drop(heap);
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(Arc::clone(outer.program())).unwrap();
    let mut run = vm
        .start(outer.at(unit), vec![], TurnOptions::default())
        .unwrap();
    run.drive(u64::MAX).unwrap();
    let returned = returned_values(run);
    let inner = vm.retain_callable(&returned[0]).unwrap();
    drop(vm);
    drop(outer);
    assert_eq!(finish(&inner, &[Value::I64(2)]), [Value::I64(42)]);
}

#[test]
fn retaining_values_requires_a_callable_and_an_exact_code_owner() {
    let (program, factory) = maker();
    let bound = Callable::function(Arc::clone(&program), factory).unwrap();
    let args = [Value::I64(40)];
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(Arc::clone(bound.program())).unwrap();
    let mut run = vm
        .start(bound.at(unit), args.to_vec(), TurnOptions::default())
        .unwrap();
    run.drive(u64::MAX).unwrap();
    let returned = returned_values(run);
    let closure = vm.retain_callable(&returned[0]).unwrap();
    let args = [Value::I64(2)];
    let unit = vm.install(Arc::clone(closure.program())).unwrap();
    let mut run = vm
        .start(closure.at(unit), args.to_vec(), TurnOptions::default())
        .unwrap();
    run.drive(u64::MAX).unwrap();
    let returned = returned_values(run);
    assert_eq!(
        vm.retain_callable(&returned[0]).unwrap_err(),
        CallableError::NotCallable {
            actual: ValueType::I64
        }
    );
    let unit = vm.install(program.clone()).unwrap();
    let mut run = vm
        .start(FuncRef::new(unit, factory), &args, TurnOptions::default())
        .unwrap();
    run.drive(u64::MAX).unwrap();
    let returned = returned_values(run);
    let retained = vm.retain_callable(&returned[0]).unwrap();
    assert!(
        Arc::ptr_eq(retained.program(), &program),
        "a retained function keeps its unit's code owner"
    );
}

#[test]
fn snapshot_adoption_rejects_ambiguous_roots_and_non_environment_bodies() {
    let (program, factory) = maker();
    let heap = AggHeap::new();
    let snapshot = heap
        .snapshot(&[
            Value::Func(FuncRef::new(UnitId(0), factory)),
            Value::Func(FuncRef::new(UnitId(0), factory)),
        ])
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
            ValueRef::Closure(Closure {
                func: FuncRef::new(UnitId(0), factory),
                env
            })
        ),
        Err(CallableError::EnvironmentArgument {
            actual: Some(ValueType::I64),
            ..
        })
    ));
}

#[test]
fn an_owned_invocation_outlives_a_discarded_or_temporary_lookup_handle() {
    let mut vm = Vm::new(NoHost, Limits::default());
    let lookup = escaped();
    let weak = Arc::downgrade(lookup.program());
    let unit = vm.install(Arc::clone(lookup.program())).unwrap();
    let mut invocation = vm
        .start(lookup.at(unit), vec![Value::I64(2)], TurnOptions::default())
        .unwrap();
    drop(lookup);
    assert!(
        weak.upgrade().is_some(),
        "the paused turn owns its code independently"
    );
    invocation.drive(u64::MAX).unwrap();
    assert_eq!(invocation.result().unwrap().unwrap(), [Value::I64(42)]);
    drop(invocation);
    assert!(
        weak.upgrade().is_some(),
        "the VM explicitly retains every installed unit"
    );
    let temporary = escaped();
    let unit = vm.install(Arc::clone(temporary.program())).unwrap();
    let mut invocation = vm
        .start(
            temporary.at(unit),
            vec![Value::I64(3)],
            TurnOptions::default(),
        )
        .unwrap();
    invocation.drive(u64::MAX).unwrap();
    assert_eq!(invocation.result().unwrap().unwrap(), [Value::I64(43)]);
}

#[test]
fn owned_input_payloads_move_without_cloning_and_usage_includes_capture_and_guest_work() {
    #[derive(Default)]
    struct InputProbe {
        pointer: usize,
        work: DataWork,
    }
    impl TraceSink for InputProbe {
        fn values(
            &mut self,
            _: &execution_tape::program::Program,
            event: execution_tape::trace::ValueEvent<'_>,
        ) {
            if let execution_tape::trace::ValueEvent::Entry { args, work, .. } = event {
                let Some(Value::Str(value)) = args.last() else {
                    panic!("expected string input");
                };
                self.pointer = value.as_ptr() as usize;
                self.work = work;
            }
        }
    }
    let mut b = ProgramBuilder::new();
    let body = b
        .declare_function(signature(
            &[ValueType::Agg, ValueType::Str],
            &[ValueType::Str],
        ))
        .unwrap();
    let mut a = b.function_asm(body).unwrap();
    a.ret(Reg::EFFECT, &[Reg(2)]);
    b.define_function(body, a).unwrap();
    let program = Arc::new(b.build_verified().unwrap());
    let mut heap = AggHeap::new();
    let env = heap.tuple_new(vec![Value::Str("xy".into())]).unwrap();
    let callable = Callable::retain(
        program,
        &heap,
        ValueRef::Closure(Closure {
            func: FuncRef::new(UnitId(0), body),
            env,
        }),
    )
    .unwrap();
    let payload = String::from("abcde");
    let pointer = payload.as_ptr() as usize;
    let mut probe = InputProbe::default();
    let mut vm = Vm::new(
        NoHost,
        Limits {
            max_payload_work: 7,
            ..Limits::default()
        },
    );
    let unit = vm.install(Arc::clone(callable.program())).unwrap();
    let mut invocation = vm
        .start(
            callable.at(unit),
            vec![Value::Str(payload)],
            TurnOptions::default().trace(execution_tape::trace::TraceMask::VALUES, &mut probe),
        )
        .unwrap();
    assert_eq!(invocation.data_work(), callable.capture_work());
    invocation.drive(u64::MAX).unwrap();
    assert_eq!(invocation.data_work().payload, 7);
    drop(invocation);
    assert_eq!(
        probe.pointer, pointer,
        "prep must move the user's payload, not clone it"
    );
    assert_eq!(probe.work.payload, 2);
}

/// Finishes `turn`, whose computation must have returned, and takes what it returned.
#[track_caller]
fn returned_values<H: Host>(turn: execution_tape::vm::Turn<'_, H>) -> Vec<Value> {
    match turn.finish() {
        Ok(execution_tape::vm::TurnOutcome::Returned(values)) => values,
        other => panic!("the computation did not return: {other:?}"),
    }
}

#[test]
fn a_forgotten_turns_values_cannot_be_snapshotted_or_retained_but_finished_ones_can() {
    let (program, factory) = maker();
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(program).unwrap();
    let entry = FuncRef::new(unit, factory);
    let args = [Value::I64(40)];
    let mut turn = vm.start(entry, &args, TurnOptions::default()).unwrap();
    turn.drive(u64::MAX).unwrap();
    let unpublished = turn.result().unwrap().unwrap().to_vec();
    core::mem::forget(turn);
    // Both settle the forgotten turn first, which discards the closure's environment.
    assert!(vm.snapshot(&unpublished).is_err());
    assert!(vm.retain_callable(&unpublished[0]).is_err());

    let mut turn = vm.start(entry, &args, TurnOptions::default()).unwrap();
    turn.drive(u64::MAX).unwrap();
    let published = returned_values(turn);
    vm.snapshot(&published).unwrap();
    let closure = vm.retain_callable(&published[0]).unwrap();
    assert_eq!(finish(&closure, &[Value::I64(2)]), [Value::I64(42)]);
}
