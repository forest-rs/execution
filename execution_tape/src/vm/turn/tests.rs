// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use super::*;
use crate::asm::{FunctionSig, ProgramBuilder};
use crate::host::HostSig;

#[derive(Default)]
struct Echo {
    calls: usize,
}
impl Host for Echo {
    fn call(
        &mut self,
        _: &str,
        _: crate::host::SigHash,
        args: &[ValueRef<'_>],
        values: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        self.calls += 1;
        for (slot, value) in values.iter_mut().zip(args) {
            *slot = value.to_value();
        }
        Ok(0)
    }
}
fn program(calls: usize) -> VerifiedProgram {
    let mut builder = ProgramBuilder::new();
    let echo = builder.host_sig_for(
        "echo",
        HostSig {
            args: vec![ValueType::Str],
            rets: vec![ValueType::Str],
        },
    );
    let entry = builder
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::Str],
            ret_types: vec![ValueType::Agg],
        })
        .unwrap();
    let mut body = builder.function_asm(entry).unwrap();
    let tuple = body.alloc_register(ValueType::Agg);
    let text = body.alloc_register(ValueType::Str);
    body.tuple_new(tuple, &[]); // An unreachable allocation before the actual result.
    for _ in 0..calls {
        body.host_call_sig_id(0, echo, 0, &[1], &[text]);
    }
    body.tuple_new(tuple, &[if calls == 0 { 1 } else { text }]);
    body.ret(0, &[tuple]);
    builder.define_function(entry, body).unwrap();
    builder.build_verified().unwrap()
}
fn until_boundary(turn: &mut Turn<'_, Echo>) -> RunStatus {
    loop {
        let status = turn.drive(u64::MAX).unwrap();
        if status != RunStatus::Suspended {
            return status;
        }
    }
}
fn returned(outcome: TurnOutcome) -> RootedValues {
    match outcome {
        TurnOutcome::Returned(values) => values,
        other => panic!("expected return, got {other:?}"),
    }
}

#[test]
fn returning_does_not_publish_and_drop_aborts_the_new_heap_suffix() {
    let code = program(0);
    let mut vm = Vm::new(Echo::default(), Limits::default());
    let function = vm.function(&code, FuncId(0)).unwrap();
    {
        let mut turn = vm.turn(TurnOptions::default()).unwrap();
        turn.start(function.clone(), [Value::Str("discarded".into())])
            .unwrap();
        assert_eq!(until_boundary(&mut turn), RunStatus::Complete);
        assert_eq!(turn.vm.aggregates().len_u32(), 2);
        assert_eq!(
            turn.vm.roots.live(),
            0,
            "drive has not registered an output"
        );
    }
    assert_eq!(vm.aggregates().len_u32(), 0);
    let roots = {
        let mut turn = vm.turn(TurnOptions::default()).unwrap();
        turn.start(function, [Value::Str("published".into())])
            .unwrap();
        assert_eq!(until_boundary(&mut turn), RunStatus::Complete);
        returned(turn.finish().unwrap())
    };
    assert_eq!(
        vm.aggregates().len_u32(),
        1,
        "publication removes unreachable new storage"
    );
    let view = vm.resolve(&roots).unwrap();
    let Value::Agg(tuple) = view.values()[0] else {
        panic!("tuple result")
    };
    assert_eq!(
        view.aggregates().tuple_get(tuple, 0).unwrap(),
        Value::Str("published".into())
    );
    vm.collect().unwrap();
    assert_eq!(vm.resolve(&roots).unwrap().values().len(), 1);
}

#[test]
fn host_admissions_survive_turns_and_stale_responses_cannot_complete_another_call() {
    let code = program(2);
    let mut vm = Vm::new(Echo::default(), Limits::default());
    let function = vm.function(&code, FuncId(0)).unwrap();
    let first = {
        let args = [Value::Str("borrowed input".into())];
        let Value::Str(text) = &args[0] else {
            unreachable!()
        };
        let mut turn = vm.turn(TurnOptions::default().yield_host_calls()).unwrap();
        turn.start(function.clone(), &args).unwrap();
        assert!(matches!(
            until_boundary(&mut turn),
            RunStatus::AwaitingHost(_)
        ));
        let key = turn.host_request_id().unwrap();
        let request = turn.pending_host_call(&key).unwrap();
        let ValueRef::Str(actual) = request.arguments().next().unwrap() else {
            panic!("string")
        };
        assert_eq!(
            actual.as_ptr(),
            text.as_ptr(),
            "inputs stay borrowed within the turn"
        );
        drop(request);
        match turn.finish().unwrap() {
            TurnOutcome::AwaitingHost(id) => id,
            _ => panic!("pending host"),
        }
    }; // The source input and lease are gone. No borrowed data may remain in the VM.
    vm.collect().unwrap();
    let second = {
        let mut turn = vm.turn(TurnOptions::default().yield_host_calls()).unwrap();
        assert!(matches!(turn.start(function, []), Err(ControlError::Busy)));
        assert!(matches!(turn.drive(0).unwrap(), RunStatus::AwaitingHost(_)));
        turn.pending_host_call(&first).unwrap().dispatch().unwrap();
        assert!(matches!(
            until_boundary(&mut turn),
            RunStatus::AwaitingHost(_)
        ));
        let second = turn.host_request_id().unwrap();
        assert_ne!(first, second);
        let work = turn.work();
        assert!(matches!(
            turn.pending_host_call(&first),
            Err(ControlError::HostRequestMismatch)
        ));
        assert_eq!(turn.work(), work);
        assert_eq!(turn.host().calls, 1);
        match turn.finish().unwrap() {
            TurnOutcome::AwaitingHost(id) => id,
            _ => panic!("second pending host"),
        }
    };
    let roots = {
        let mut turn = vm.turn(TurnOptions::default()).unwrap();
        turn.pending_host_call(&second).unwrap().dispatch().unwrap();
        assert_eq!(until_boundary(&mut turn), RunStatus::Complete);
        assert_eq!(turn.host().calls, 2);
        returned(turn.finish().unwrap())
    };
    let view = vm.resolve(&roots).unwrap();
    let Value::Agg(tuple) = view.values()[0] else {
        panic!("tuple")
    };
    assert_eq!(
        view.aggregates().tuple_get(tuple, 0).unwrap(),
        Value::Str("borrowed input".into())
    );
}

#[test]
fn failed_publication_discards_control_and_new_allocations() {
    let code = program(0);
    let mut vm = Vm::new(
        Echo::default(),
        Limits {
            max_value_roots: 0,
            ..Limits::default()
        },
    );
    let function = vm.function(&code, FuncId(0)).unwrap();
    let mut turn = vm.turn(TurnOptions::default()).unwrap();
    turn.start(function, [Value::Str("not published".into())])
        .unwrap();
    assert_eq!(until_boundary(&mut turn), RunStatus::Complete);
    assert!(matches!(
        turn.finish(),
        Err(ControlError::Retention(RootError::Limit { .. }))
    ));
    assert!(!vm.domain.as_ref().unwrap().has_control());
    assert_eq!(vm.aggregates().len_u32(), 0);
}

#[test]
fn foreign_host_response_and_function_leave_current_work_untouched() {
    let code = program(1);
    let mut first = Vm::new(Echo::default(), Limits::default());
    let function = first.function(&code, FuncId(0)).unwrap();
    let mut turn = first
        .turn(TurnOptions::default().yield_host_calls())
        .unwrap();
    turn.start(function.clone(), [Value::Str("first".into())])
        .unwrap();
    assert!(matches!(
        until_boundary(&mut turn),
        RunStatus::AwaitingHost(_)
    ));
    let foreign = turn.host_request_id().unwrap();
    turn.finish().unwrap();
    let mut second = Vm::new(Echo::default(), Limits::default());
    let local = second.function(&code, FuncId(0)).unwrap();
    let mut turn = second
        .turn(TurnOptions::default().yield_host_calls())
        .unwrap();
    assert!(matches!(
        turn.start(function, []),
        Err(ControlError::ForeignStore)
    ));
    turn.start(local, [Value::Str("second".into())]).unwrap();
    assert!(matches!(
        until_boundary(&mut turn),
        RunStatus::AwaitingHost(_)
    ));
    let key = turn.host_request_id().unwrap();
    let work = turn.work();
    assert!(matches!(
        turn.pending_host_call(&foreign),
        Err(ControlError::HostRequestMismatch)
    ));
    assert_eq!(turn.work(), work);
    turn.pending_host_call(&key).unwrap().dispatch().unwrap();
    assert_eq!(until_boundary(&mut turn), RunStatus::Complete);
    turn.finish().unwrap();
}

#[test]
fn a_forgotten_lease_never_resets_the_control_namespace() {
    let mut vm = Vm::new(Echo::default(), Limits::default());
    let turn = vm.turn(TurnOptions::default()).unwrap();
    core::mem::forget(turn);
    assert!(matches!(
        vm.turn(TurnOptions::default()),
        Err(ControlError::StoreLeased)
    ));
    assert!(matches!(vm.collect(), Err(RootError::StoreLeased)));
    assert!(matches!(vm.take_aggregates(), Err(RootError::HeapInUse)));
    assert!(
        vm.domain.is_none(),
        "leaking control cannot manufacture a fresh arena"
    );
}

#[test]
fn owning_borrowed_progress_spends_payload_work_and_owned_progress_moves() {
    let code = program(0);
    for borrowed in [false, true] {
        let args = [Value::Str("a borrowed payload".into())];
        let mut vm = Vm::new(
            Echo::default(),
            Limits {
                max_payload_work: 0,
                ..Limits::default()
            },
        );
        let function = vm.function(&code, FuncId(0)).unwrap();
        let mut turn = vm.turn(TurnOptions::default()).unwrap();
        if borrowed {
            turn.start(function, &args).unwrap();
        } else {
            turn.start(function, args.to_vec()).unwrap();
        }
        assert_eq!(turn.drive(1).unwrap(), RunStatus::Suspended);
        let result = turn.finish();
        if borrowed {
            assert!(matches!(
                result,
                Err(ControlError::Input(Trap::PayloadWorkExceeded { .. }))
            ));
            assert!(!vm.domain.as_ref().unwrap().has_control());
            assert_eq!(vm.aggregates().len_u32(), 0);
        } else {
            assert!(matches!(result, Ok(TurnOutcome::Paused)));
            assert!(vm.domain.as_ref().unwrap().has_control());
        }
    }
}

#[test]
fn a_fresh_observer_joins_live_context_without_historical_entry_events() {
    use crate::trace::{ScopeSnapshot, StackSnapshot, TraceContext};
    #[derive(Default)]
    struct Observer {
        starts: usize,
        ends: usize,
        contexts: usize,
        releases: usize,
        scopes: Vec<ScopeSnapshot>,
    }
    impl TraceSink for Observer {
        fn run_start(&mut self, _: TraceContext<'_>, _: FuncId, _: usize) {
            self.starts += 1;
        }
        fn run_end(&mut self, _: TraceContext<'_>, _: TraceOutcome<'_>) {
            self.ends += 1;
        }
        fn stack_context(&mut self, _: TraceContext<'_>, snapshot: StackSnapshot<'_>) {
            assert_eq!(snapshot.frames.len(), 1);
            assert!(!snapshot.parked);
            assert!(snapshot.pending.is_some());
            self.contexts += 1;
            self.scopes.extend_from_slice(snapshot.scopes);
        }
        fn stack_release(&mut self, _: TraceContext<'_>) {
            self.releases += 1;
        }
    }
    let code = program(1);
    let mut vm = Vm::new(Echo::default(), Limits::default());
    let function = vm.function(&code, FuncId(0)).unwrap();
    let mut turn = vm.turn(TurnOptions::default().yield_host_calls()).unwrap();
    turn.start(
        function,
        [Value::Str("first observer sees no entry".into())],
    )
    .unwrap();
    assert!(matches!(
        until_boundary(&mut turn),
        RunStatus::AwaitingHost(_)
    ));
    let id = turn.host_request_id().unwrap();
    turn.finish().unwrap();
    let mut first = Observer::default();
    let turn = vm
        .turn(TurnOptions::default().trace(
            TraceMask::RUN | TraceMask::CALL | TraceMask::HOST,
            &mut first,
        ))
        .unwrap();
    turn.finish().unwrap();
    assert_eq!(
        (first.starts, first.ends, first.contexts, first.releases),
        (0, 0, 1, 1)
    );
    assert_eq!(first.scopes.len(), 2);
    let mut second = Observer::default();
    let mut turn = vm
        .turn(TurnOptions::default().trace(
            TraceMask::RUN | TraceMask::CALL | TraceMask::HOST,
            &mut second,
        ))
        .unwrap();
    turn.pending_host_call(&id).unwrap().dispatch().unwrap();
    assert_eq!(until_boundary(&mut turn), RunStatus::Complete);
    let roots = returned(turn.finish().unwrap());
    assert_eq!((second.starts, second.ends, second.contexts), (0, 1, 1));
    assert_eq!(vm.resolve(&roots).unwrap().values().len(), 1);
}

#[cfg(panic = "unwind")]
#[test]
fn finishing_directly_after_an_interrupted_host_admission_aborts() {
    use crate::trace::TraceContext;
    extern crate std;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    struct Interrupted;
    impl TraceSink for Interrupted {
        fn scope_enter(
            &mut self,
            _: TraceContext<'_>,
            kind: ScopeKind,
            _: usize,
            _: FuncId,
            _: u32,
            _: Option<u64>,
        ) {
            assert!(
                !matches!(kind, ScopeKind::HostCall { .. }),
                "injected admission panic"
            );
        }
    }
    let code = program(1);
    let mut vm = Vm::new(Echo::default(), Limits::default());
    let function = vm.function(&code, FuncId(0)).unwrap();
    let mut observer = Interrupted;
    let mut turn = vm
        .turn(
            TurnOptions::default()
                .yield_host_calls()
                .trace(TraceMask::HOST, &mut observer),
        )
        .unwrap();
    turn.start(function, [Value::Str("abort admission".into())])
        .unwrap();
    assert!(catch_unwind(AssertUnwindSafe(|| until_boundary(&mut turn))).is_err());
    assert!(matches!(
        turn.finish(),
        Err(ControlError::Trapped(TrapInfo {
            trap: Trap::HostCallAbandoned,
            ..
        }))
    ));
    assert!(!vm.domain.as_ref().unwrap().has_control());
    assert_eq!(vm.aggregates().len_u32(), 0);
    assert_eq!(vm.host().calls, 0);
}

fn effect_programs(
    repeated: bool,
) -> (
    VerifiedProgram,
    VerifiedProgram,
    crate::program::ControlTagId,
) {
    let mut caller = ProgramBuilder::new();
    let exchange = FunctionSig {
        arg_types: vec![ValueType::Agg],
        ret_types: vec![ValueType::I64],
    };
    let operation = caller
        .control_tag("supply-number", exchange.clone())
        .unwrap();
    let signature = FunctionSig {
        arg_types: vec![ValueType::Tag, ValueType::Agg],
        ret_types: vec![ValueType::I64],
    };
    let invoke = caller.call_sig(signature.clone()).unwrap();
    let entry = caller
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::Func, ValueType::Tag, ValueType::Agg],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let mut body = caller.function_asm(entry).unwrap();
    let answer = body.alloc_register(ValueType::I64);
    body.call_indirect(0, invoke, 1, 0, &[2, 3], &[answer]);
    body.ret(0, &[answer]);
    caller.define_function(entry, body).unwrap();

    let mut worker = ProgramBuilder::new();
    let exchange = worker.call_sig(exchange).unwrap();
    let entry = worker.declare_function(signature).unwrap();
    let mut body = worker.function_asm(entry).unwrap();
    let mut answer = worker.emit_suspend(&mut body, 1, exchange, &[2]).unwrap()[0];
    if repeated {
        answer = worker.emit_suspend(&mut body, 1, exchange, &[2]).unwrap()[0];
    }
    body.ret(0, &[answer]);
    worker.define_function(entry, body).unwrap();
    (
        caller.build_verified().unwrap(),
        worker.build_verified().unwrap(),
        operation,
    )
}
fn start_effect(
    vm: &mut Vm<Echo>,
    caller: &Instance,
    worker: &Instance,
    tag: crate::program::ControlTagId,
) -> Effect {
    let tuple = vm
        .aggregate_builder()
        .tuple_new(vec![Value::Str("request context".into())])
        .unwrap();
    let mut turn = vm.turn(TurnOptions::default()).unwrap();
    turn.start(
        caller.function(FuncId(0)).unwrap(),
        [
            Value::Func(worker.function(FuncId(0)).unwrap()),
            Value::Tag(caller.control_tag(tag).unwrap()),
            Value::Agg(tuple),
        ],
    )
    .unwrap();
    assert_eq!(until_boundary(&mut turn), RunStatus::AwaitingEffect);
    let pending = turn.pending_effect().unwrap();
    assert_eq!(pending.instance(), worker);
    assert_eq!(pending.tag().name(), "supply-number");
    match turn.finish().unwrap() {
        TurnOutcome::Effect(effect) => effect,
        other => panic!("expected effect: {other:?}"),
    }
}

#[test]
fn a_native_effect_outlives_its_turn_and_resumes_an_independent_code_chain_after_collection() {
    let (caller_code, worker_code, operation) = effect_programs(false);
    let mut vm = Vm::new(Echo::default(), Limits::default());
    let caller = vm.instantiate(&caller_code).unwrap();
    let worker = vm.instantiate(&worker_code).unwrap();
    vm.aggregate_builder().tuple_new(vec![]).unwrap(); // A dead prefix forces moving collection.
    let effect = start_effect(&mut vm, &caller, &worker, operation);
    let alias = effect.continuation.clone();
    assert_eq!(effect.continuation.parameters().kinds(), &[ValueType::I64]);
    assert_eq!(effect.continuation.results().kinds(), &[ValueType::I64]);
    assert_eq!(vm.domain.as_ref().unwrap().frames, 2);
    let stats = vm.collect().unwrap();
    assert!(stats.nodes_after < stats.nodes_before);
    let request = vm.resolve(&effect.request).unwrap();
    let Value::Agg(tuple) = request.values()[0] else {
        panic!("request tuple")
    };
    assert_eq!(
        request.aggregates().tuple_get(tuple, 0).unwrap(),
        Value::Str("request context".into())
    );
    let roots = {
        let mut turn = vm.turn(TurnOptions::default()).unwrap();
        turn.resume_continuation(&effect.continuation, [Value::I64(42)])
            .unwrap();
        assert_eq!(until_boundary(&mut turn), RunStatus::Complete);
        returned(turn.finish().unwrap())
    };
    assert_eq!(vm.resolve(&roots).unwrap().values(), &[Value::I64(42)]);
    assert_eq!(vm.domain.as_ref().unwrap().frames, 0);
    let mut turn = vm.turn(TurnOptions::default()).unwrap();
    let work = turn.work();
    assert!(matches!(
        turn.resume_continuation(&alias, []),
        Err(ControlError::Resumption(_))
    ));
    assert_eq!(turn.work(), work);
    turn.finish().unwrap();
}

#[test]
fn a_native_effect_can_be_answered_without_finishing_or_resetting_the_turn() {
    let (caller_code, worker_code, operation) = effect_programs(false);
    let mut vm = Vm::new(Echo::default(), Limits::default());
    let caller = vm.instantiate(&caller_code).unwrap();
    let worker = vm.instantiate(&worker_code).unwrap();
    let tuple = vm.aggregate_builder().tuple_new(vec![]).unwrap();
    let mut turn = vm.turn(TurnOptions::default()).unwrap();
    turn.start(
        caller.function(FuncId(0)).unwrap(),
        [
            Value::Func(worker.function(FuncId(0)).unwrap()),
            Value::Tag(caller.control_tag(operation).unwrap()),
            Value::Agg(tuple),
        ],
    )
    .unwrap();
    assert_eq!(until_boundary(&mut turn), RunStatus::AwaitingEffect);
    let capability = turn.pending_effect().unwrap().continuation().clone();
    let work = turn.work();
    turn.resume_continuation(&capability, [Value::I64(73)])
        .unwrap();
    assert_eq!(until_boundary(&mut turn), RunStatus::Complete);
    assert!(
        turn.work().fuel > work.fuel,
        "responding does not replenish the turn's allowance"
    );
    let roots = returned(turn.finish().unwrap());
    assert_eq!(vm.resolve(&roots).unwrap().values(), &[Value::I64(73)]);
}

#[test]
fn aborting_a_resumed_turn_retires_its_descendants_and_preserves_an_older_sibling() {
    let (caller_code, worker_code, operation) = effect_programs(true);
    let mut vm = Vm::new(Echo::default(), Limits::default());
    let caller = vm.instantiate(&caller_code).unwrap();
    let worker = vm.instantiate(&worker_code).unwrap();
    let first = start_effect(&mut vm, &caller, &worker, operation);
    let sibling = start_effect(&mut vm, &caller, &worker, operation);
    let descendant;
    {
        let mut turn = vm.turn(TurnOptions::default()).unwrap();
        turn.resume_continuation(&first.continuation, [Value::I64(1)])
            .unwrap();
        assert_eq!(until_boundary(&mut turn), RunStatus::AwaitingEffect);
        descendant = turn.pending_effect().unwrap().continuation().clone();
        // No finish: this turn's new saved suffix is cancelled; the predecessor stays consumed.
    }
    assert_eq!(vm.domain.as_ref().unwrap().saved.len(), 1);
    assert_eq!(vm.domain.as_ref().unwrap().frames, 2);
    vm.collect().unwrap();
    let mut turn = vm.turn(TurnOptions::default()).unwrap();
    for stale in [&first.continuation, &descendant] {
        assert!(matches!(
            turn.resume_continuation(stale, [Value::I64(2)]),
            Err(ControlError::Resumption(_))
        ));
    }
    turn.resume_continuation(&sibling.continuation, [Value::I64(2)])
        .unwrap();
    assert_eq!(until_boundary(&mut turn), RunStatus::AwaitingEffect);
    let next = turn.pending_effect().unwrap().continuation().clone();
    turn.resume_continuation(&next, [Value::I64(3)]).unwrap();
    assert_eq!(until_boundary(&mut turn), RunStatus::Complete);
    let roots = returned(turn.finish().unwrap());
    assert_eq!(vm.resolve(&roots).unwrap().values(), &[Value::I64(3)]);
    assert_eq!(vm.domain.as_ref().unwrap().frames, 0);
}

#[test]
fn native_effects_construct_aggregate_responses_without_resetting_the_turn() {
    let mut builder = ProgramBuilder::new();
    let protocol = FunctionSig {
        arg_types: vec![],
        ret_types: vec![ValueType::Agg],
    };
    let operation = builder
        .control_tag("supply-pair", protocol.clone())
        .unwrap();
    let signature = builder.call_sig(protocol.clone()).unwrap();
    let root = builder.declare_function(protocol).unwrap();
    let mut body = builder.function_asm(root).unwrap();
    let tag = body.alloc_register(ValueType::Tag);
    body.const_tag(tag, operation);
    let answer = builder
        .emit_suspend(&mut body, tag, signature, &[])
        .unwrap()[0];
    body.ret(0, &[answer]);
    builder.define_function(root, body).unwrap();
    let code = builder.build_verified().unwrap();
    let mut vm = Vm::new(Echo::default(), Limits::default());
    let function = vm.function(&code, root).unwrap();
    let mut turn = vm.turn(TurnOptions::default()).unwrap();
    turn.start(function, []).unwrap();
    assert_eq!(until_boundary(&mut turn), RunStatus::AwaitingEffect);
    let before = turn.work();
    let (capability, response) = {
        let mut effect = turn.pending_effect().unwrap();
        let capability = effect.continuation().clone();
        let mut values = effect.values();
        let child = values
            .tuple_new(vec![Value::Str("new response".into())])
            .unwrap();
        let pair = values
            .tuple_new(vec![Value::Agg(child), Value::Agg(child)])
            .unwrap();
        assert_eq!(values.tuple_len(pair).unwrap(), 2);
        (capability, values.arguments([Value::Agg(pair)]))
    };
    assert_eq!(turn.work().data.aggregate - before.data.aggregate, 5);
    assert_eq!(
        turn.work().data.payload - before.data.payload,
        "new response".len() as u64
    );
    turn.resume_continuation(&capability, response).unwrap();
    assert_eq!(until_boundary(&mut turn), RunStatus::Complete);
    let roots = returned(turn.finish().unwrap());
    vm.collect().unwrap();
    let view = vm.resolve(&roots).unwrap();
    let Value::Agg(pair) = view.values()[0] else {
        panic!("pair")
    };
    let first = view.aggregates().tuple_get(pair, 0).unwrap();
    assert_eq!(
        first,
        view.aggregates().tuple_get(pair, 1).unwrap(),
        "response sharing survives publication"
    );
    let Value::Agg(child) = first else {
        panic!("child")
    };
    assert_eq!(
        view.aggregates().tuple_get(child, 0).unwrap(),
        Value::Str("new response".into())
    );
}

#[test]
fn registered_argument_selection_is_lazy_owner_preserving_and_charges_only_selected_payloads() {
    let mut builder = ProgramBuilder::new();
    let root = builder
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let mut body = builder.function_asm(root).unwrap();
    body.ret(0, &[1]);
    builder.define_function(root, body).unwrap();
    let code = builder.build_verified().unwrap();
    let mut vm = Vm::new(
        Echo::default(),
        Limits {
            max_payload_work: 0,
            ..Limits::default()
        },
    );
    let instance = vm.instantiate(&code).unwrap();
    let values = vm
        .retain_values(
            &instance,
            vec![
                Value::Str("this payload must not be copied".into()),
                Value::I64(91),
            ],
        )
        .unwrap();
    let selected = values.arguments(1..);
    let mut turn = vm.turn(TurnOptions::default()).unwrap();
    turn.start(instance.function(root).unwrap(), selected)
        .unwrap();
    assert_eq!(until_boundary(&mut turn), RunStatus::Complete);
    assert_eq!(turn.work().data.payload, 0);
    let outputs = returned(turn.finish().unwrap());
    assert_eq!(vm.resolve(&outputs).unwrap().values(), &[Value::I64(91)]);
    let mut turn = vm.turn(TurnOptions::default()).unwrap();
    assert!(matches!(
        turn.start(instance.function(root).unwrap(), values.arguments(5..)),
        Err(ControlError::Retention(RootError::InvalidRange {
            len: 2,
            ..
        }))
    ));
    assert!(matches!(turn.finish().unwrap(), TurnOutcome::Idle));
}

#[test]
fn native_effect_inspection_finds_saved_frames_and_registers_without_a_host_call() {
    let (caller_code, worker_code, operation) = effect_programs(false);
    let mut vm = Vm::new(Echo::default(), Limits::default());
    let caller = vm.instantiate(&caller_code).unwrap();
    let worker = vm.instantiate(&worker_code).unwrap();
    let effect = start_effect(&mut vm, &caller, &worker, operation);
    let mut view = vm.inspect().unwrap();
    assert_eq!(view.frames().len(), 0);
    let saved = view.saved_continuations().next().unwrap();
    assert_eq!(saved.id(), effect.continuation.id());
    let frames: Vec<_> = saved.frames().collect();
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].instance.as_ref(), Some(&worker));
    assert_eq!(frames[1].instance.as_ref(), Some(&caller));
    assert_eq!(frames[0].parent.as_ref(), Some(&frames[1].activation));
    assert!(matches!(
        view.register(&frames[0].activation, 1).unwrap().value,
        Some(ValueRef::Tag(_))
    ));
    assert!(matches!(
        view.register(&frames[0].activation, 2).unwrap().value,
        Some(ValueRef::Agg(_))
    ));
    let turn = vm.turn(TurnOptions::default()).unwrap();
    assert_eq!(
        turn.inspect()
            .saved_continuations()
            .next()
            .unwrap()
            .frames()
            .len(),
        2
    );
    turn.finish().unwrap();
}

fn closure_code(
    suspends: bool,
) -> (
    alloc::sync::Arc<VerifiedProgram>,
    FuncId,
    Option<crate::program::ControlTagId>,
) {
    let mut builder = ProgramBuilder::new();
    let protocol = FunctionSig {
        arg_types: vec![],
        ret_types: vec![],
    };
    let operation = suspends.then(|| builder.control_tag("pause", protocol.clone()).unwrap());
    let exchange = builder.call_sig(protocol).unwrap();
    let integer = builder.contract(Contract::Kind(ValueType::I64)).unwrap();
    let environment = builder.contract(Contract::Tuple(&[Some(integer)])).unwrap();
    let root = builder
        .declare_function(crate::asm::FunctionAbi {
            args: vec![environment.into()],
            rets: vec![ValueType::I64.into()],
        })
        .unwrap();
    let mut body = builder.function_asm(root).unwrap();
    if let Some(operation) = operation {
        let tag = body.alloc_register(ValueType::Tag);
        body.const_tag(tag, operation);
        builder.emit_suspend(&mut body, tag, exchange, &[]).unwrap();
    }
    let answer = body.alloc_register(ValueType::I64);
    body.tuple_get(answer, 1, 0).ret(0, &[answer]);
    builder.define_function(root, body).unwrap();
    (
        alloc::sync::Arc::new(builder.build_verified().unwrap()),
        root,
        operation,
    )
}
fn retained_closure(
    code: &VerifiedProgram,
    entry: FuncId,
    number: i64,
) -> crate::callable::Callable {
    let mut source = Vm::new(Echo::default(), Limits::default());
    let func = source.function(code, entry).unwrap();
    let env = source
        .aggregate_builder()
        .tuple_new(vec![Value::I64(number)])
        .unwrap();
    crate::callable::Callable::retain(
        source.aggregates(),
        ValueRef::Closure(&Closure { func, env }),
    )
    .unwrap()
}

#[test]
fn a_temporary_code_selection_can_be_dropped_immediately_after_start() {
    let (code, entry, _) = closure_code(false);
    let mut slot = crate::callable::CodeSlot::new(retained_closure(&code, entry, 41));
    let mut vm = Vm::new(Echo::default(), Limits::default());
    let mut turn = vm.turn(TurnOptions::default()).unwrap();
    turn.start(slot.current().callable(), []).unwrap();
    assert_eq!(
        turn.work().data.aggregate,
        2,
        "capture import is charged exactly once"
    );
    slot.replace(retained_closure(&code, entry, 99)).unwrap();
    drop(slot);
    drop(code);
    assert_eq!(until_boundary(&mut turn), RunStatus::Complete);
    let roots = returned(turn.finish().unwrap());
    assert_eq!(vm.resolve(&roots).unwrap().values(), &[Value::I64(41)]);
}

#[test]
fn repeated_retained_entries_spend_one_shared_capture_allowance() {
    let (code, entry, _) = closure_code(true);
    let callable = retained_closure(&code, entry, 41);
    let mut vm = Vm::new(
        Echo::default(),
        Limits {
            max_aggregate_work: 2,
            ..Limits::default()
        },
    );
    let mut turn = vm.turn(TurnOptions::default()).unwrap();
    turn.start(&callable, []).unwrap();
    assert_eq!(until_boundary(&mut turn), RunStatus::AwaitingEffect);
    let capability = turn.pending_effect().unwrap().continuation().clone();
    turn.discard_continuation(&capability).unwrap();
    assert!(matches!(
        turn.start(&callable, []),
        Err(ControlError::Input(Trap::AggregateWorkExceeded {
            used: 2,
            requested: 2,
            ..
        }))
    ));
    assert_eq!(turn.work().data.aggregate, 2);
    assert!(matches!(turn.finish().unwrap(), TurnOutcome::Idle));
    assert_eq!(vm.aggregates().len_u32(), 0);
}

#[test]
fn selecting_a_registered_callee_resolves_its_environment_after_collection() {
    let (code, entry, _) = closure_code(false);
    let mut vm = Vm::new(Echo::default(), Limits::default());
    let instance = vm.instantiate(&code).unwrap();
    vm.aggregate_builder().tuple_new(vec![]).unwrap();
    let env = vm
        .aggregate_builder()
        .tuple_new(vec![Value::I64(42)])
        .unwrap();
    let values = vm
        .retain_values(
            &instance,
            vec![Value::Closure(Closure {
                func: instance.function(entry).unwrap(),
                env,
            })],
        )
        .unwrap();
    let selected = values.callee(0);
    drop(values);
    let stats = vm.collect().unwrap();
    assert_eq!((stats.nodes_before, stats.nodes_after), (2, 1));
    let mut turn = vm.turn(TurnOptions::default()).unwrap();
    turn.start(selected, []).unwrap();
    assert_eq!(until_boundary(&mut turn), RunStatus::Complete);
    assert_eq!(
        turn.work().data.aggregate,
        0,
        "registered captures stay in their existing store"
    );
    let roots = returned(turn.finish().unwrap());
    assert_eq!(vm.resolve(&roots).unwrap().values(), &[Value::I64(42)]);
    assert_eq!(
        vm.collect().unwrap().nodes_after,
        0,
        "the temporary callee selection released its root"
    );
}

#[test]
fn invalid_registered_callees_are_denied_before_installing_control_or_spending_work() {
    let (code, entry, _) = closure_code(false);
    let mut vm = Vm::new(Echo::default(), Limits::default());
    let instance = vm.instantiate(&code).unwrap();
    let scalar = vm.retain_values(&instance, vec![Value::I64(3)]).unwrap();
    let mut foreign = Vm::new(Echo::default(), Limits::default());
    let foreign_instance = foreign.instantiate(&code).unwrap();
    let foreign_values = foreign
        .retain_values(
            &foreign_instance,
            vec![Value::Func(foreign_instance.function(entry).unwrap())],
        )
        .unwrap();
    let mut turn = vm.turn(TurnOptions::default()).unwrap();
    let before = turn.work();
    assert!(matches!(
        turn.start(scalar.callee(1), []),
        Err(ControlError::Retention(RootError::InvalidIndex {
            index: 1,
            len: 1
        }))
    ));
    assert!(matches!(
        turn.start(scalar.callee(0), []),
        Err(ControlError::NotCallable {
            actual: ValueType::I64
        })
    ));
    assert!(matches!(
        turn.start(foreign_values.callee(0), []),
        Err(ControlError::Retention(RootError::ForeignStore))
    ));
    assert_eq!(turn.work(), before);
    assert_eq!(turn.inspect().frames().count(), 0);
    assert!(matches!(turn.finish().unwrap(), TurnOutcome::Idle));
}
