// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A host can build rich results without mutating borrowed inputs or exporting foreign handles.

use execution_tape::{
    asm::{FunctionSig, ProgramBuilder},
    callable::Callable,
    host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef},
    program::{ElemTypeId, StructTypeDef, TypeId, ValueType},
    recording::{RecordLimits, Recorder, ReplayOutcome},
    trace::{TraceMask, TraceSink},
    value::{AggHandle, Value},
    vm::{DataWork, InvocationOptions, Limits, RunStatus, Trap, Vm},
};
use std::{cell::RefCell, rc::Rc, sync::Arc};

fn program() -> Callable {
    program_tail(false)
}
fn program_tail(tail: bool) -> Callable {
    program_shape(tail, false)
}
fn program_shape(tail: bool, two_calls: bool) -> Callable {
    let mut b = ProgramBuilder::new();
    b.struct_type(StructTypeDef {
        field_names: vec!["values".into()],
        field_types: vec![ValueType::Agg],
    });
    b.array_elem(ValueType::I64);
    let entry = b
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::Agg],
            ret_types: vec![ValueType::Agg],
        })
        .unwrap();
    let import = b.host_sig_for(
        "load",
        HostSig {
            args: vec![ValueType::Agg],
            rets: vec![ValueType::Agg],
        },
    );
    let mut a = b.function_asm(entry).unwrap();
    let result = a.alloc_register(ValueType::Agg);
    a.host_call_sig_id(0, import, 0, &[1], &[result]);
    if two_calls {
        let bridge = a.alloc_register(ValueType::Agg);
        a.tuple_new(bridge, &[result])
            .host_call_sig_id(0, import, 0, &[bridge], &[result]);
    }
    let output = if tail {
        let wrapped = a.alloc_register(ValueType::Agg);
        a.tuple_new(wrapped, &[result]);
        wrapped
    } else {
        result
    };
    a.ret(0, &[output]);
    b.define_function(entry, a).unwrap();
    Callable::function(Arc::new(b.build_verified().unwrap()), entry).unwrap()
}
#[derive(Default)]
struct Build {
    fail: bool,
}
fn build(
    args: &[ValueRef<'_>],
    rets: &mut [Value],
    mut cx: HostContext<'_, '_>,
) -> Result<u64, HostError> {
    let ValueRef::Agg(input) = args[0] else {
        panic!("input")
    };
    let expected = cx.tuple_get_ref(input, 0).unwrap().to_value();
    // This allocation spends work but must not be retained or alter later node identity.
    cx.tuple_new(vec![Value::Bytes(vec![0; 16])])?;
    let array = cx.array_new(ElemTypeId(0), vec![Value::I64(1), Value::I64(2)])?;
    assert_eq!(
        cx.array_get_ref(array, 1),
        Ok(ValueRef::I64(2)),
        "staged array lookup"
    );
    let st = cx.struct_new(TypeId(0), vec![Value::Agg(array)])?;
    assert_eq!(
        cx.struct_get_ref(st, 0),
        Ok(ValueRef::Agg(array)),
        "staged child identity"
    );
    let root = cx.tuple_new(vec![
        Value::Agg(input),
        Value::Agg(st),
        Value::Agg(st),
        Value::Str("loaded".into()),
    ])?;
    assert_eq!(cx.tuple_len(root), Ok(4), "staged tuple shape");
    assert_eq!(
        cx.tuple_get(input, 0).unwrap(),
        expected,
        "borrowed input remains unchanged"
    );
    rets[0] = Value::Agg(root);
    Ok(3)
}
impl Host for Build {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        args: &[ValueRef<'_>],
        rets: &mut [Value],
        cx: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        let outcome = build(args, rets, cx);
        if self.fail {
            Err(HostError::Failed)
        } else {
            outcome
        }
    }
}
fn input(vm: &mut Vm<Build>) -> AggHandle {
    vm.aggregates_mut()
        .tuple_new(vec![Value::Str("borrowed".into())])
        .unwrap()
}
fn assert_result(heap: &execution_tape::aggregates::AggHeap, roots: &[Value]) {
    let [Value::Agg(root)] = roots else {
        panic!("aggregate result")
    };
    let st = heap.tuple_get(*root, 1).unwrap();
    assert_eq!(
        st,
        heap.tuple_get(*root, 2).unwrap(),
        "shared struct identity"
    );
    let Value::Agg(st) = st else { panic!("struct") };
    let Value::Agg(array) = heap.struct_get(st, 0).unwrap() else {
        panic!("array")
    };
    assert_eq!(
        heap.array_get(array, 1),
        Ok(Value::I64(2)),
        "retained array content"
    );
    assert_eq!(
        heap.tuple_get(*root, 3),
        Ok(Value::Str("loaded".into())),
        "retained payload"
    );
    assert_eq!(
        heap.len_u32(),
        4,
        "one input plus three reachable results; dead staging retired"
    );
}
#[test]
fn synchronous_and_yielded_hosts_build_shared_structures_and_replay_identically() {
    for yielded in [false, true] {
        let target = program();
        let mut vm = Vm::new(Build::default(), Limits::default());
        let input = input(&mut vm);
        let capture = Rc::new(RefCell::new(Recorder::new(
            target.program().clone(),
            RecordLimits::default(),
        )));
        let mask = capture.mask();
        let options = InvocationOptions::default().trace_owned(mask, capture.clone());
        let options = if yielded {
            options.yield_host_calls()
        } else {
            options
        };
        let mut run = target
            .start_owned(vm, vec![Value::Agg(input)], options)
            .unwrap();
        if yielded {
            assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
            run.pending_host_call().unwrap().dispatch().unwrap();
        }
        assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::Complete);
        assert_result(run.aggregates(), run.result().unwrap().unwrap());
        assert_eq!(
            run.data_work(),
            DataWork {
                aggregate: 12,
                payload: 22
            }
        );
        let retained = run
            .aggregates()
            .snapshot(run.result().unwrap().unwrap())
            .unwrap();
        drop(run);
        assert_result(retained.aggregates(), retained.values());
        let record = Rc::try_unwrap(capture).ok().unwrap().into_inner().finish();
        assert!(record.status().is_ok(), "{:?}", record.status());
        let call = record.host_calls().next().unwrap();
        assert_eq!(
            call.work,
            DataWork {
                aggregate: 12,
                payload: 22
            }
        );
        assert_eq!(call.constructed.len(), 3);
        let ReplayOutcome::Returned(result) = record.replay().unwrap() else {
            panic!("replay return")
        };
        assert_result(result.aggregates(), result.values());
    }
}

#[test]
fn pending_views_share_staging_and_retention_copies_only_the_reachable_graph() {
    let target = program();
    let mut vm = Vm::new(Build::default(), Limits::default());
    let input = input(&mut vm);
    let mut run = target
        .start_owned(
            vm,
            vec![Value::Agg(input)],
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    run.resume(u64::MAX).unwrap();
    let staged;
    let retained;
    {
        let mut request = run.pending_host_call().unwrap();
        let mut cx = request.context();
        cx.tuple_new(vec![Value::Bytes(vec![0; 1024])]).unwrap();
        staged = cx
            .tuple_new(vec![Value::Agg(input), Value::Str("retained".into())])
            .unwrap();
        retained = cx.retain(&[ValueRef::Agg(staged)]).unwrap();
        assert_eq!(cx.tuple_get_ref(staged, 1), Ok(ValueRef::Str("retained")));
        assert_eq!(
            cx.aggregates().len_u32(),
            1,
            "staging does not mutate borrowed storage"
        );
    }
    assert_eq!(
        run.data_work(),
        DataWork {
            aggregate: 5,
            payload: 1032
        }
    );
    assert_eq!(run.resume(0).unwrap(), RunStatus::AwaitingHost);
    let mut request = run.pending_host_call().unwrap();
    assert_eq!(request.context().tuple_len(staged), Ok(2));
    request.complete(&mut [Value::Agg(staged)], Ok(0)).unwrap();
    run.resume(u64::MAX).unwrap();
    assert_eq!(run.aggregates().len_u32(), 2);
    drop(run);
    assert_eq!(retained.aggregates().len_u32(), 2);
    let Value::Agg(root) = retained.values()[0] else {
        panic!("retained root")
    };
    assert_eq!(
        retained.aggregates().tuple_get(root, 1),
        Ok(Value::Str("retained".into()))
    );
}

#[test]
fn host_failure_discards_staging_but_replay_preserves_spent_work_and_original_error() {
    let target = program();
    let mut vm = Vm::new(Build { fail: true }, Limits::default());
    let input = input(&mut vm);
    let capture = Rc::new(RefCell::new(Recorder::new(
        target.program().clone(),
        RecordLimits::default(),
    )));
    let mask = capture.mask();
    let mut run = target
        .start(
            &mut vm,
            vec![Value::Agg(input)],
            InvocationOptions::default().trace_owned(mask, capture.clone()),
        )
        .unwrap();
    assert!(matches!(
        run.resume(u64::MAX).unwrap_err().trap,
        Trap::HostCallFailed { .. }
    ));
    assert_eq!(
        run.data_work(),
        DataWork {
            aggregate: 12,
            payload: 22
        }
    );
    assert_eq!(run.aggregates().len_u32(), 1);
    drop(run);
    let record = Rc::try_unwrap(capture).ok().unwrap().into_inner().finish();
    assert!(record.status().is_ok());
    let call = record.host_calls().next().unwrap();
    assert!(call.constructed.is_empty());
    assert_eq!(
        call.work,
        DataWork {
            aggregate: 12,
            payload: 22
        }
    );
    assert!(
        matches!(record.replay().unwrap(), ReplayOutcome::Trapped(trap) if matches!(trap.trap, Trap::HostCallFailed { .. }))
    );
}

#[test]
fn denied_construction_can_fall_back_without_allocating_or_spending_the_denied_debit() {
    for payload_denial in [false, true] {
        let target = program();
        let mut vm = Vm::new(
            Build::default(),
            Limits {
                max_aggregate_work: if payload_denial { 100 } else { 2 },
                max_payload_work: if payload_denial { 1 } else { 100 },
                ..Limits::default()
            },
        );
        let input = input(&mut vm);
        let capture = Rc::new(RefCell::new(Recorder::new(
            target.program().clone(),
            RecordLimits::default(),
        )));
        let mask = capture.mask();
        let mut run = target
            .start_owned(
                vm,
                vec![Value::Agg(input)],
                InvocationOptions::default()
                    .yield_host_calls()
                    .trace_owned(mask, capture.clone()),
            )
            .unwrap();
        run.resume(u64::MAX).unwrap();
        let mut request = run.pending_host_call().unwrap();
        {
            let mut cx = request.context();
            let error = cx
                .tuple_new(if payload_denial {
                    vec![Value::Str("too large".into())]
                } else {
                    vec![Value::Unit, Value::Unit]
                })
                .unwrap_err();
            let HostError::Custom(error) = error else {
                panic!("typed denial")
            };
            assert!(
                matches!(error.downcast_ref::<Trap>(), Some(Trap::PayloadWorkExceeded { .. }) if payload_denial)
                    || matches!(error.downcast_ref::<Trap>(), Some(Trap::AggregateWorkExceeded { .. }) if !payload_denial)
            );
            assert_eq!(cx.tuple_len(input), Ok(1));
        }
        request.complete(&mut [Value::Agg(input)], Ok(0)).unwrap();
        run.resume(u64::MAX).unwrap();
        assert_eq!(run.data_work(), DataWork::default());
        assert_eq!(run.aggregates().len_u32(), 1);
        drop(run);
        let record = Rc::try_unwrap(capture).ok().unwrap().into_inner().finish();
        assert!(matches!(
            record.replay().unwrap(),
            ReplayOutcome::Returned(_)
        ));
    }
}

#[test]
fn construction_rejects_invalid_nominal_shapes_and_dangling_children_before_mutation() {
    let target = program();
    let mut vm = Vm::new(Build::default(), Limits::default());
    let input = input(&mut vm);
    let mut run = target
        .start(
            &mut vm,
            vec![Value::Agg(input)],
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    run.resume(u64::MAX).unwrap();
    let mut request = run.pending_host_call().unwrap();
    {
        let mut cx = request.context();
        assert!(cx.array_new(ElemTypeId(9), vec![]).is_err());
        assert!(
            cx.array_new(ElemTypeId(0), vec![Value::Bool(false)])
                .is_err()
        );
        assert!(cx.struct_new(TypeId(0), vec![]).is_err());
        assert!(cx.struct_new(TypeId(0), vec![Value::I64(1)]).is_err());
        assert!(cx.struct_new(TypeId(9), vec![]).is_err());
        assert!(cx.tuple_new(vec![Value::Agg(AggHandle(99))]).is_err());
        assert!(cx.retain(&[ValueRef::Agg(AggHandle(99))]).is_err());
    }
    request.complete(&mut [Value::Agg(input)], Ok(0)).unwrap();
    run.resume(u64::MAX).unwrap();
    assert_eq!(run.data_work(), DataWork::default());
    assert_eq!(run.aggregates().len_u32(), 1);
}

#[test]
fn cancelling_staged_results_preserves_only_explicitly_retained_roots() {
    let target = program();
    let mut vm = Vm::new(Build::default(), Limits::default());
    let input = input(&mut vm);
    let retained;
    {
        let mut run = target
            .start(
                &mut vm,
                vec![Value::Agg(input)],
                InvocationOptions::default().yield_host_calls(),
            )
            .unwrap();
        run.resume(u64::MAX).unwrap();
        let mut request = run.pending_host_call().unwrap();
        let mut cx = request.context();
        let temporary = cx.tuple_new(vec![Value::Bytes(vec![0; 1024])]).unwrap();
        retained = cx.retain(&[ValueRef::Agg(temporary)]).unwrap();
    }
    assert_eq!(vm.aggregates().len_u32(), 1);
    assert_eq!(retained.aggregates().len_u32(), 1);
    let Value::Agg(root) = retained.values()[0] else {
        panic!("retained root")
    };
    assert_eq!(
        retained.aggregates().tuple_get(root, 0),
        Ok(Value::Bytes(vec![0; 1024]))
    );
    assert_eq!(
        vm.run(
            target.program(),
            target.function_id(),
            &[Value::Agg(input)],
            TraceMask::NONE,
            None
        )
        .unwrap()
        .len(),
        1
    );
}

#[test]
fn staged_work_uses_the_guest_budget_and_replay_does_not_replenish_it() {
    let target = program_tail(true);
    let mut vm = Vm::new(
        Build::default(),
        Limits {
            max_aggregate_work: 12,
            ..Limits::default()
        },
    );
    let input = input(&mut vm);
    let capture = Rc::new(RefCell::new(Recorder::new(
        target.program().clone(),
        RecordLimits::default(),
    )));
    let mask = capture.mask();
    let mut run = target
        .start(
            &mut vm,
            vec![Value::Agg(input)],
            InvocationOptions::default().trace_owned(mask, capture.clone()),
        )
        .unwrap();
    assert!(matches!(
        run.resume(u64::MAX).unwrap_err().trap,
        Trap::AggregateWorkExceeded {
            limit: 12,
            used: 12,
            requested: 2
        }
    ));
    assert_eq!(
        run.aggregates().len_u32(),
        1,
        "failed guest rolls back adopted host results"
    );
    drop(run);
    let record = Rc::try_unwrap(capture).ok().unwrap().into_inner().finish();
    assert!(
        matches!(record.replay().unwrap(), ReplayOutcome::Trapped(trap) if matches!(trap.trap, Trap::AggregateWorkExceeded { limit: 12, used: 12, requested: 2 }))
    );
}

#[test]
fn bad_return_validation_keeps_raw_evidence_and_discards_unreturned_staging() {
    for bad_arity in [true, false] {
        let target = program();
        let mut vm = Vm::new(Build::default(), Limits::default());
        let input = input(&mut vm);
        let capture = Rc::new(RefCell::new(Recorder::new(
            target.program().clone(),
            RecordLimits::default(),
        )));
        let mask = capture.mask();
        let mut run = target
            .start(
                &mut vm,
                vec![Value::Agg(input)],
                InvocationOptions::default()
                    .yield_host_calls()
                    .trace_owned(mask, capture.clone()),
            )
            .unwrap();
        run.resume(u64::MAX).unwrap();
        let mut request = run.pending_host_call().unwrap();
        request.context().tuple_new(vec![Value::I64(1)]).unwrap();
        let error = if bad_arity {
            request.complete(&mut [], Ok(0))
        } else {
            request.complete(&mut [Value::I64(0)], Ok(0))
        }
        .unwrap_err();
        assert!(
            matches!(error.trap, Trap::HostReturnArityMismatch { .. }) && bad_arity
                || matches!(error.trap, Trap::TypeMismatch { .. }) && !bad_arity
        );
        assert_eq!(
            run.data_work(),
            DataWork {
                aggregate: 2,
                payload: 0
            }
        );
        assert_eq!(run.aggregates().len_u32(), 1);
        drop(run);
        let record = Rc::try_unwrap(capture).ok().unwrap().into_inner().finish();
        assert!(record.status().is_ok());
        assert!(record.host_calls().next().unwrap().constructed.is_empty());
        assert!(matches!(
            record.replay().unwrap(),
            ReplayOutcome::Trapped(_)
        ));
    }
}

#[test]
fn abandoning_an_async_producer_retires_its_staging_and_never_redispatches() {
    use std::{
        future::{Future, pending},
        task::{Context, Waker},
    };
    let target = program();
    let mut vm = Vm::new(Build::default(), Limits::default());
    let input = input(&mut vm);
    let mut run = target
        .start(
            &mut vm,
            vec![Value::Agg(input)],
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    run.resume(u64::MAX).unwrap();
    let mut producer = Box::pin(
        run.pending_host_call()
            .unwrap()
            .complete_with(async |request| {
                request
                    .context()
                    .tuple_new(vec![Value::Bytes(vec![0; 1024])])?;
                pending::<()>().await;
                unreachable!()
            }),
    );
    assert!(
        producer
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    drop(producer);
    assert_eq!(
        run.data_work(),
        DataWork {
            aggregate: 2,
            payload: 1024
        }
    );
    assert!(matches!(
        run.resume(u64::MAX).unwrap_err().trap,
        Trap::HostCallAbandoned
    ));
    assert_eq!(
        run.data_work(),
        DataWork {
            aggregate: 2,
            payload: 1024
        }
    );
    assert_eq!(run.aggregates().len_u32(), 1);
}

#[test]
fn guest_created_values_between_host_results_keep_their_identity_during_replay() {
    let target = program_shape(false, true);
    let mut vm = Vm::new(Build::default(), Limits::default());
    let input = input(&mut vm);
    let mut recorder = Recorder::new(target.program().clone(), RecordLimits::default());
    let mask = recorder.mask();
    let result = vm
        .run(
            target.program(),
            target.function_id(),
            &[Value::Agg(input)],
            mask,
            Some(&mut recorder),
        )
        .unwrap();
    assert!(matches!(result.as_slice(), [Value::Agg(_)]));
    assert_eq!(vm.aggregates().len_u32(), 8);
    let record = recorder.finish();
    assert_eq!(
        record
            .host_calls()
            .map(|call| call.constructed.len())
            .collect::<Vec<_>>(),
        [3, 3]
    );
    let ReplayOutcome::Returned(replayed) = record.replay().unwrap() else {
        panic!("replay return")
    };
    assert_eq!(replayed.aggregates().len_u32(), 8);
    let Value::Agg(root) = replayed.values()[0] else {
        panic!("root")
    };
    let Value::Agg(bridge) = replayed.aggregates().tuple_get(root, 0).unwrap() else {
        panic!("bridge")
    };
    let Value::Agg(first) = replayed.aggregates().tuple_get(bridge, 0).unwrap() else {
        panic!("first result")
    };
    assert_eq!(
        replayed.aggregates().tuple_get(first, 3),
        Ok(Value::Str("loaded".into()))
    );
}

#[test]
fn retiring_an_owned_invocation_moves_rich_payloads_into_its_snapshot() {
    let target = program();
    let mut vm = Vm::new(Build::default(), Limits::default());
    let input = input(&mut vm);
    let mut run = target
        .start_owned(
            vm,
            vec![Value::Agg(input)],
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    run.resume(u64::MAX).unwrap();
    let bytes = vec![42; 8192];
    let pointer = bytes.as_ptr();
    let mut request = run.pending_host_call().unwrap();
    let root = request
        .context()
        .tuple_new(vec![Value::Bytes(bytes)])
        .unwrap();
    request.complete(&mut [Value::Agg(root)], Ok(0)).unwrap();
    run.resume(u64::MAX).unwrap();
    let snapshot = run.into_snapshot().unwrap().unwrap();
    let Value::Agg(root) = snapshot.values()[0] else {
        panic!("snapshot root")
    };
    let Value::Bytes(bytes) = snapshot.aggregates().tuple_get_ref(root, 0).unwrap() else {
        panic!("payload")
    };
    assert_eq!(
        bytes.as_ptr(),
        pointer,
        "retiring the owned VM does not clone its payload"
    );
    assert_eq!(
        snapshot.aggregates().len_u32(),
        1,
        "unused original input retired"
    );
}

#[test]
fn consuming_a_borrowed_invocation_preserves_its_vms_existing_raw_handles() {
    let target = program();
    let mut vm = Vm::new(Build::default(), Limits::default());
    let input = input(&mut vm);
    let mut run = target
        .start(
            &mut vm,
            vec![Value::Agg(input)],
            InvocationOptions::default(),
        )
        .unwrap();
    run.resume(u64::MAX).unwrap();
    let roots = run.result().unwrap().unwrap().to_vec();
    let snapshot = run.into_snapshot().unwrap().unwrap();
    assert_result(vm.aggregates(), &roots);
    assert_result(snapshot.aggregates(), snapshot.values());
}

#[test]
fn nominal_result_code_owners_survive_dropped_lookup_handles_and_vm_retirement() {
    let target = program();
    let expected = Arc::downgrade(target.program());
    let mut vm = Vm::new(Build::default(), Limits::default());
    let input = input(&mut vm);
    let mut run = target
        .start_owned(vm, vec![Value::Agg(input)], InvocationOptions::default())
        .unwrap();
    drop(target);
    run.resume(u64::MAX).unwrap();
    let owner = run
        .program_owner()
        .cloned()
        .expect("callable entry retains its code");
    assert!(Arc::ptr_eq(&owner, &expected.upgrade().unwrap()));
    let snapshot = run.into_snapshot().unwrap().unwrap();
    let values = execution_graph::TapeValue::from_snapshot(snapshot, Some(owner)).unwrap();
    assert!(matches!(
        values[0].domain(),
        execution_graph::TapeValueDomain::Program(_)
    ));
    assert!(
        expected.upgrade().is_some(),
        "retained nominal results keep their exact code owner"
    );
    drop(values);
    assert!(
        expected.upgrade().is_none(),
        "last retained result releases code without keeping a VM"
    );
}
