// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Cooperative quanta preserve the ordinary VM's execution and ownership contracts.

use execution_tape::value::FuncRef;
use execution_tape::{
    asm::{FunctionSig, ProgramBuilder, Reg},
    host::{AccessSink, Host, HostContext, HostError, HostSig, ResourceKeyRef, SigHash, ValueRef},
    program::{Program, SpanId, ValueType},
    recording::{CaptureFault, RecordLimits, Recorder, ReplayOutcome},
    trace::{ScopeKind, TraceMask, TraceOutcome, TraceSink},
    value::{FuncId, Value},
    verifier::VerifiedProgram,
    vm::{Limits, RunStatus, Trap, TurnOptions, Vm},
};
use std::sync::Arc;

#[derive(Default)]
struct Echo {
    calls: usize,
    panic: bool,
}
impl Host for Echo {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        args: &[ValueRef<'_>],
        rets: &mut [Value],
        mut ctx: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        self.calls += 1;
        ctx.record_read(ResourceKeyRef::HostState {
            op: SigHash(1),
            key: self.calls as u64,
        });
        assert!(!self.panic, "injected host panic");
        rets[0] = args[0].to_value();
        Ok(2)
    }
}

fn fixture() -> (Arc<VerifiedProgram>, FuncId) {
    let mut b = ProgramBuilder::new();
    let child = b
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::Str],
            ret_types: vec![ValueType::Agg],
        })
        .unwrap();
    let entry = b
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::Str],
            ret_types: vec![ValueType::Agg],
        })
        .unwrap();
    let echo = b.host_sig_for(
        "echo",
        HostSig {
            args: vec![ValueType::Str],
            rets: vec![ValueType::Str],
        },
    );
    let mut a = b.function_asm(child).unwrap();
    let str_ = a.alloc_register(ValueType::Str);
    let tuple = a.alloc_register(ValueType::Agg);
    // The argument and returned payload deliberately share the same bytes but own their storage.
    a.host_call_sig_id(Reg::EFFECT, echo, Reg::EFFECT, &[Reg(1)], &[str_])
        .tuple_new(tuple, &[Reg(1), str_])
        .ret(Reg::EFFECT, &[tuple]);
    b.define_function(child, a).unwrap();
    let mut a = b.function_asm(entry).unwrap();
    let first = a.alloc_register(ValueType::Agg);
    let second = a.alloc_register(ValueType::Agg);
    let root = a.alloc_register(ValueType::Agg);
    a.call(Reg::EFFECT, child, Reg::EFFECT, &[Reg(1)], &[first])
        .call(Reg::EFFECT, child, Reg::EFFECT, &[Reg(1)], &[second])
        .tuple_new(root, &[first, second, first])
        .ret(Reg::EFFECT, &[root]);
    b.define_function(entry, a).unwrap();
    (Arc::new(b.build_verified().unwrap()), entry)
}

#[test]
fn every_instruction_boundary_preserves_values_sharing_recording_and_host_effects() {
    let (p, entry) = fixture();
    let args = [Value::Str("borrowed input".into())];
    for quantum in [1, 2, 7, u64::MAX] {
        let mut vm = Vm::new(Echo::default(), Limits::default());
        let mut recorder = Recorder::new(Arc::clone(&p), RecordLimits::default());
        let options = TurnOptions::default().trace(recorder.mask(), &mut recorder);
        let unit = vm.install(p.clone()).unwrap();
        let mut invocation = vm.start(FuncRef::new(unit, entry), &args, options).unwrap();
        assert_eq!(invocation.drive(0).unwrap(), RunStatus::Suspended);
        assert_eq!(invocation.host().calls, 0);
        assert!(invocation.result().is_none());
        let mut quanta = 0;
        while invocation.drive(quantum).unwrap() == RunStatus::Suspended {
            quanta += 1;
            assert!(quanta < 100, "resume must advance without restarting");
        }
        let fuel = invocation.remaining_fuel();
        assert_eq!(invocation.drive(1).unwrap(), RunStatus::Complete);
        assert_eq!(invocation.remaining_fuel(), fuel);
        assert_eq!(invocation.host().calls, 2);
        let outputs = returned_values(invocation);
        let Value::Agg(root) = outputs[0] else {
            panic!("aggregate result")
        };
        let Value::Agg(first) = vm.aggregates().tuple_get(root, 0).unwrap() else {
            panic!("nested result")
        };
        assert_eq!(
            vm.aggregates().tuple_get(root, 2).unwrap(),
            Value::Agg(first)
        );
        assert_eq!(vm.aggregates().tuple_get(first, 1).unwrap(), args[0]);
        let record = recorder.finish();
        record.status().unwrap();
        assert_eq!(record.host_calls().count(), 2);
        assert!(matches!(
            record.replay().unwrap(),
            ReplayOutcome::Returned(_)
        ));
    }
}

#[derive(Default)]
struct Events {
    entered: usize,
    cancelled_scopes: usize,
    cancelled_runs: usize,
}
impl TraceSink for Events {
    fn scope_enter(
        &mut self,
        _: &Program,
        _: ScopeKind,
        _: usize,
        _location: execution_tape::trace::TraceLocation,
        _: Option<SpanId>,
    ) {
        self.entered += 1;
    }
    fn scope_exit(
        &mut self,
        _: &Program,
        _: ScopeKind,
        _: usize,
        _location: execution_tape::trace::TraceLocation,
        _: Option<SpanId>,
        outcome: TraceOutcome<'_>,
    ) {
        self.cancelled_scopes += usize::from(matches!(outcome, TraceOutcome::Cancelled));
    }
    fn run_end(&mut self, _: &Program, outcome: TraceOutcome<'_>) {
        self.cancelled_runs += usize::from(matches!(outcome, TraceOutcome::Cancelled));
    }
}

#[test]
fn cancellation_rolls_back_only_new_aggregates_without_undoing_host_effects() {
    let (p, entry) = fixture();
    let args = [Value::Str("value".into())];
    let mut vm = Vm::new(Echo::default(), Limits::default());
    let retained = vm.aggregates_mut().tuple_new(vec![Value::I64(42)]).unwrap();
    let mut events = Events::default();
    {
        let options = TurnOptions::default().trace(
            TraceMask::RUN | TraceMask::CALL | TraceMask::HOST,
            &mut events,
        );
        let unit = vm.install(p.clone()).unwrap();
        let mut invocation = vm.start(FuncRef::new(unit, entry), &args, options).unwrap();
        // Root call, child host call, child tuple creation; both frames remain active.
        assert_eq!(invocation.drive(3).unwrap(), RunStatus::Suspended);
        assert_eq!(invocation.host().calls, 1);
        assert!(invocation.aggregates().len_u32() > 1);
    }
    assert_eq!(vm.aggregates().len_u32(), 1);
    assert_eq!(
        vm.aggregates().tuple_get(retained, 0).unwrap(),
        Value::I64(42)
    );
    assert_eq!(vm.host().calls, 1);
    assert_eq!(events.cancelled_scopes, 2);
    assert_eq!(events.cancelled_runs, 1);
    assert_eq!(events.entered, 3); // one completed host scope, two cancelled guest frames
    let unit = vm.install(p.clone()).unwrap();
    assert!(
        vm.run(FuncRef::new(unit, entry), &args, TraceMask::NONE, None)
            .is_ok()
    );
}

#[test]
fn dropping_a_partial_recording_is_explicitly_incomplete_and_not_replayable() {
    let (p, entry) = fixture();
    let args = [Value::Str("value".into())];
    let mut vm = Vm::new(Echo::default(), Limits::default());
    let mut recorder = Recorder::new(Arc::clone(&p), RecordLimits::default());
    {
        let options = TurnOptions::default().trace(recorder.mask(), &mut recorder);
        let unit = vm.install(p.clone()).unwrap();
        let mut invocation = vm.start(FuncRef::new(unit, entry), &args, options).unwrap();
        invocation.drive(3).unwrap();
    }
    let record = recorder.finish();
    assert!(matches!(
        record.status(),
        Err(CaptureFault::MissingCompletion)
    ));
    assert!(record.replay().is_err());
}

#[test]
fn hard_limits_accumulate_across_resumes_and_terminal_traps_do_not_repeat_effects() {
    let (p, entry) = fixture();
    let args = [Value::Str("payload".into())];
    let cases = [
        Limits {
            fuel: 2,
            ..Limits::default()
        },
        Limits {
            max_call_depth: 1,
            ..Limits::default()
        },
        Limits {
            max_host_calls: 1,
            ..Limits::default()
        },
        Limits {
            max_payload_work: 21,
            ..Limits::default()
        },
        Limits {
            max_aggregate_work: 3,
            ..Limits::default()
        },
    ];
    for limits in cases {
        let mut uninterrupted = Vm::new(Echo::default(), limits.clone());
        let unit = uninterrupted.install(p.clone()).unwrap();
        let expected = uninterrupted
            .run(FuncRef::new(unit, entry), &args, TraceMask::NONE, None)
            .unwrap_err();
        for quantum in [1, 2, 7] {
            let mut vm = Vm::new(Echo::default(), limits.clone());
            let unit = vm.install(p.clone()).unwrap();
            let mut invocation = vm
                .start(FuncRef::new(unit, entry), &args, TurnOptions::default())
                .unwrap();
            let actual = loop {
                match invocation.drive(quantum) {
                    Err(error) => break error,
                    Ok(RunStatus::Suspended) => {}
                    Ok(
                        RunStatus::AwaitingHost
                        | RunStatus::CallbackReturned
                        | RunStatus::AwaitingHandler,
                    ) => {
                        panic!("default invocations dispatch their host synchronously")
                    }
                    Ok(RunStatus::Complete) => panic!("expected the same hard limit"),
                    Ok(status) => panic!("unexpected run status {status:?}"),
                }
            };
            assert_eq!((actual.func, actual.pc), (expected.func, expected.pc));
            assert_eq!(format!("{:?}", actual.trap), format!("{:?}", expected.trap));
            let calls = invocation.host().calls;
            let repeated = invocation.drive(u64::MAX).unwrap_err();
            assert_eq!(repeated.pc, actual.pc);
            assert_eq!(invocation.host().calls, calls);
            assert_eq!(calls, uninterrupted.host().calls);
            assert!(matches!(invocation.result(), Some(Err(_))));
            drop(invocation);
            assert_eq!(vm.aggregates().len_u32(), 0);
        }
    }
}

#[test]
fn rejected_entry_emits_its_trap_without_a_cancellation_or_host_call() {
    let (p, entry) = fixture();
    let mut vm = Vm::new(Echo::default(), Limits::default());
    let mut recorder = Recorder::new(Arc::clone(&p), RecordLimits::default());
    let unit = vm.install(p.clone()).unwrap();
    let error = vm
        .start(
            FuncRef::new(unit, entry),
            &[],
            TurnOptions::default().trace(recorder.mask(), &mut recorder),
        )
        .unwrap_err();
    assert!(matches!(error.trap, Trap::ArityMismatch));
    assert_eq!(vm.host().calls, 0);
    let record = recorder.finish();
    record.status().unwrap();
    assert!(matches!(
        record.replay().unwrap(),
        ReplayOutcome::Trapped(_)
    ));
}

#[test]
// Panic-abort targets, including default WASI, cannot catch a host panic or run its cleanup.
#[cfg(panic = "unwind")]
fn host_panic_unwinding_cancels_scopes_and_restores_the_vm() {
    let (p, entry) = fixture();
    let args = [Value::Str("value".into())];
    let mut vm = Vm::new(
        Echo {
            panic: true,
            ..Echo::default()
        },
        Limits::default(),
    );
    let mut events = Events::default();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let options = TurnOptions::default().trace(
            TraceMask::RUN | TraceMask::CALL | TraceMask::HOST,
            &mut events,
        );
        let unit = vm.install(p.clone()).unwrap();
        let mut invocation = vm.start(FuncRef::new(unit, entry), &args, options).unwrap();
        invocation.drive(u64::MAX).unwrap();
    }));
    assert!(result.is_err());
    assert_eq!(events.entered, events.cancelled_scopes);
    assert_eq!(events.cancelled_runs, 1);
    assert_eq!(vm.aggregates().len_u32(), 0);
    vm.host_mut().panic = false;
    let unit = vm.install(p.clone()).unwrap();
    assert!(
        vm.run(FuncRef::new(unit, entry), &args, TraceMask::NONE, None)
            .is_ok()
    );
}

#[test]
#[cfg(panic = "unwind")]
fn return_observer_panic_releases_the_execution_lease_without_a_second_panic() {
    struct PanicAtReturn {
        instruction: bool,
        fired: bool,
    }
    impl TraceSink for PanicAtReturn {
        fn instr_end(
            &mut self,
            _: &Program,
            _: execution_tape::trace::TraceLocation,
            next: Option<execution_tape::trace::TraceLocation>,
            outcome: TraceOutcome<'_>,
        ) {
            if self.instruction
                && !self.fired
                && next.is_none()
                && matches!(outcome, TraceOutcome::Ok)
            {
                self.fired = true;
                panic!("injected return instruction observer panic");
            }
        }
        fn scope_exit(
            &mut self,
            _: &Program,
            _: ScopeKind,
            _: usize,
            _location: execution_tape::trace::TraceLocation,
            _: Option<SpanId>,
            outcome: TraceOutcome<'_>,
        ) {
            if !self.instruction && !self.fired && matches!(outcome, TraceOutcome::Ok) {
                self.fired = true;
                panic!("injected return scope observer panic");
            }
        }
    }
    let mut builder = ProgramBuilder::new();
    let entry = builder
        .declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::Agg],
        })
        .unwrap();
    let mut body = builder.function_asm(entry).unwrap();
    let tuple = body.alloc_register(ValueType::Agg);
    body.tuple_new(tuple, &[]).ret(Reg::EFFECT, &[tuple]);
    builder.define_function(entry, body).unwrap();
    let program = builder.build_verified().unwrap();
    for instruction in [false, true] {
        let mut vm = Vm::new(Echo::default(), Limits::default());
        let retained = vm.aggregates_mut().tuple_new(vec![Value::I64(99)]).unwrap();
        let mut observer = PanicAtReturn {
            instruction,
            fired: false,
        };
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let unit = vm.install(program.clone()).unwrap();
                let mut invocation = vm
                    .start(
                        FuncRef::new(unit, entry),
                        &[],
                        TurnOptions::default()
                            .trace(TraceMask::CALL | TraceMask::INSTR, &mut observer),
                    )
                    .unwrap();
                invocation.drive(u64::MAX).unwrap();
            }))
            .is_err()
        );
        assert!(observer.fired);
        assert_eq!(vm.aggregates().len_u32(), 1);
        assert_eq!(
            vm.aggregates().tuple_get(retained, 0).unwrap(),
            Value::I64(99)
        );
        let unit = vm.install(program.clone()).unwrap();
        assert!(
            vm.run(FuncRef::new(unit, entry), &[], TraceMask::NONE, None)
                .is_ok()
        );
    }
}

#[test]
fn contract_proof_work_and_successful_proofs_survive_suspension() {
    use execution_tape::{
        asm::{FunctionAbi, Reg},
        contract::Contract,
    };
    let mut b = ProgramBuilder::new();
    let integer = b.contract(Contract::Kind(ValueType::I64)).unwrap();
    let tuple = b.contract(Contract::Tuple(&[Some(integer)])).unwrap();
    let child = b
        .declare_function(FunctionAbi {
            args: vec![tuple.into()],
            rets: vec![integer.into()],
        })
        .unwrap();
    let mut a = b.function_asm(child).unwrap();
    let r = a.alloc_register(ValueType::I64);
    a.tuple_get(r, Reg(1), 0).ret(Reg::EFFECT, &[r]);
    b.define_function(child, a).unwrap();
    let entry = b
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::Agg],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let mut a = b.function_asm(entry).unwrap();
    let r = a.alloc_register(ValueType::I64);
    a.call(Reg::EFFECT, child, Reg::EFFECT, &[Reg(1)], &[r]);
    let second = a.pc();
    a.call(Reg::EFFECT, child, Reg::EFFECT, &[Reg(1)], &[r])
        .ret(Reg::EFFECT, &[r]);
    b.define_function(entry, a).unwrap();
    let p = b.build_verified().unwrap();
    let mut vm = Vm::new(
        Echo::default(),
        Limits {
            max_contract_work: 3,
            ..Limits::default()
        },
    );
    let root = vm.aggregates_mut().tuple_new(vec![Value::I64(42)]).unwrap();
    let args = [Value::Agg(root)];
    let unit = vm.install(p.clone()).unwrap();
    let mut invocation = vm
        .start(FuncRef::new(unit, entry), &args, TurnOptions::default())
        .unwrap();
    let error = loop {
        match invocation.drive(1) {
            Ok(RunStatus::Suspended) => {}
            Ok(
                RunStatus::AwaitingHost | RunStatus::CallbackReturned | RunStatus::AwaitingHandler,
            ) => {
                panic!("default invocations dispatch their host synchronously")
            }
            Ok(RunStatus::Complete) => {
                panic!("the second cached visit must still consume contract work")
            }
            Err(error) => break error,
            Ok(status) => panic!("unexpected run status {status:?}"),
        }
    };
    assert!(matches!(error.trap, Trap::ContractWorkExceeded { .. }));
    assert_eq!((error.func, error.pc), (entry, second));
}

#[test]
fn resource_observer_is_retained_across_quanta() {
    #[derive(Default)]
    struct Reads(Vec<u64>);
    impl AccessSink for Reads {
        fn read(&mut self, key: ResourceKeyRef<'_>) {
            if let ResourceKeyRef::HostState { key, .. } = key {
                self.0.push(key);
            }
        }
        fn write(&mut self, _: ResourceKeyRef<'_>) {
            panic!("no writes in this fixture");
        }
    }
    let (p, entry) = fixture();
    let args = [Value::Str("value".into())];
    let mut vm = Vm::new(Echo::default(), Limits::default());
    let mut reads = Reads::default();
    {
        let unit = vm.install(p.clone()).unwrap();
        let mut invocation = vm
            .start(
                FuncRef::new(unit, entry),
                &args,
                TurnOptions::default().access(&mut reads),
            )
            .unwrap();
        while invocation.drive(1).unwrap() == RunStatus::Suspended {}
    }
    assert_eq!(reads.0, [1, 2]);
}

#[test]
fn a_forgotten_invocation_is_cancelled_before_its_vm_is_used_again() {
    let (p, entry) = fixture();
    let args = [Value::Str("value".into())];
    let mut vm = Vm::new(Echo::default(), Limits::default());
    let retained = vm.aggregates_mut().tuple_new(vec![Value::I64(1)]).unwrap();
    let unit = vm.install(p).unwrap();
    let entry = FuncRef::new(unit, entry);
    let mut invocation = vm.start(entry, &args, TurnOptions::default()).unwrap();
    // Through the first child's tuple.
    assert_eq!(invocation.drive(4).unwrap(), RunStatus::Suspended);
    core::mem::forget(invocation);
    assert_eq!(vm.aggregates().len_u32(), 2);

    // The next run first discards what the forgotten turn allocated.
    let outputs = vm.run(entry, &args, TraceMask::NONE, None).unwrap();
    assert_eq!(vm.aggregates().len_u32(), 4);
    assert!(matches!(outputs.as_slice(), [Value::Agg(_)]));
    assert_eq!(
        vm.aggregates().tuple_get(retained, 0).unwrap(),
        Value::I64(1)
    );
}

#[test]
// Panic-abort targets, including default WASI, cannot catch a host panic or run its cleanup.
#[cfg(panic = "unwind")]
fn a_host_panic_mid_run_discards_the_runs_aggregates_before_the_vm_is_used_again() {
    /// Echoes, except that its second call panics.
    struct PanicsOnce(usize);
    impl Host for PanicsOnce {
        fn call(
            &mut self,
            _: &str,
            _: SigHash,
            args: &[ValueRef<'_>],
            rets: &mut [Value],
            _: HostContext<'_, '_>,
        ) -> Result<u64, HostError> {
            self.0 += 1;
            assert_ne!(self.0, 2, "injected host panic");
            rets[0] = args[0].to_value();
            Ok(0)
        }
    }
    let (p, entry) = fixture();
    let args = [Value::Str("value".into())];
    let mut vm = Vm::new(PanicsOnce(0), Limits::default());
    let retained = vm.aggregates_mut().tuple_new(vec![Value::I64(1)]).unwrap();
    let unit = vm.install(p).unwrap();
    let entry = FuncRef::new(unit, entry);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        vm.run(entry, &args, TraceMask::NONE, None)
    }));
    assert!(result.is_err());
    // The first child's tuple outlives the panic until the VM is next used.
    assert_eq!(vm.aggregates().len_u32(), 2);

    let outputs = vm.run(entry, &args, TraceMask::NONE, None).unwrap();
    assert_eq!(vm.aggregates().len_u32(), 4);
    assert!(matches!(outputs.as_slice(), [Value::Agg(_)]));
    assert_eq!(
        vm.aggregates().tuple_get(retained, 0).unwrap(),
        Value::I64(1)
    );
}

#[test]
fn a_returned_turn_dropped_or_forgotten_discards_what_it_allocated_and_finishing_keeps_it() {
    let (p, entry) = fixture();
    let mut vm = Vm::new(Echo::default(), Limits::default());
    let unit = vm.install(p).unwrap();
    let entry = FuncRef::new(unit, entry);
    let first = [Value::Str("first".into())];
    let before = vm.aggregates().len_u32();
    for forget in [false, true] {
        let mut turn = vm.start(entry, &first, TurnOptions::default()).unwrap();
        assert_eq!(turn.drive(u64::MAX).unwrap(), RunStatus::Complete);
        assert!(turn.result().is_some_and(|result| result.is_ok()));
        if forget {
            core::mem::forget(turn);
        } else {
            drop(turn);
        }
        // The VM's next entry point settles a forgotten turn as dropping it does.
        assert_eq!(vm.aggregates_mut().len_u32(), before, "forgotten: {forget}");
    }

    let mut turn = vm.start(entry, &first, TurnOptions::default()).unwrap();
    assert_eq!(turn.drive(u64::MAX).unwrap(), RunStatus::Complete);
    let outputs = returned_values(turn);
    let root = vm.root(outputs).unwrap();
    vm.run(entry, &[Value::Str("second".into())], TraceMask::NONE, None)
        .unwrap();
    vm.collect().unwrap();
    let [Value::Agg(outer)] = vm.resolve(&root).unwrap() else {
        panic!("one tuple");
    };
    let Value::Agg(inner) = vm.aggregates().tuple_get(*outer, 0).unwrap() else {
        panic!("a nested tuple");
    };
    assert_eq!(
        vm.aggregates().tuple_get(inner, 0).unwrap(),
        Value::Str("first".into())
    );
}

#[test]
fn registering_a_group_first_discards_what_a_forgotten_invocation_allocated() {
    let (p, entry) = fixture();
    let args = [Value::Str("value".into())];
    let mut vm = Vm::new(Echo::default(), Limits::default());
    let unit = vm.install(p).unwrap();
    let mut invocation = vm
        .start(FuncRef::new(unit, entry), &args, TurnOptions::default())
        .unwrap();
    // Through the first child's tuple.
    assert_eq!(invocation.drive(4).unwrap(), RunStatus::Suspended);
    core::mem::forget(invocation);
    let end = vm.aggregates().handle_end();
    let tuple = execution_tape::value::AggHandle(end - 1);
    assert!(matches!(
        vm.root(vec![Value::Agg(tuple)]),
        Err(execution_tape::vm::RootError::DanglingHandle { .. })
    ));
}

/// Finishes `turn`, whose computation must have returned, and takes what it returned.
#[track_caller]
fn returned_values<H: Host>(turn: execution_tape::vm::Turn<'_, H>) -> Vec<Value> {
    match turn.finish() {
        Ok(execution_tape::vm::TurnOutcome::Returned(values)) => values,
        other => panic!("the computation did not return: {other:?}"),
    }
}

/// The string at `(root.0).0` of a returned value.
fn nested_text(vm: &Vm<Echo>, value: &Value) -> Value {
    let Value::Agg(outer) = value else {
        panic!("a tuple");
    };
    let Value::Agg(inner) = vm.aggregates().tuple_get(*outer, 0).unwrap() else {
        panic!("a nested tuple");
    };
    vm.aggregates().tuple_get(inner, 0).unwrap()
}

#[test]
fn handles_copied_from_a_dropped_turn_name_what_later_runs_allocate() {
    let (p, entry) = fixture();
    let mut vm = Vm::new(Echo::default(), Limits::default());
    let entry = FuncRef::new(vm.install(p).unwrap(), entry);
    let dropped = [Value::Str("dropped".into())];
    let mut turn = vm.start(entry, &dropped, TurnOptions::default()).unwrap();
    assert_eq!(turn.drive(u64::MAX).unwrap(), RunStatus::Complete);
    // A peek keeps nothing alive: only `finish` publishes.
    let copied = turn.result().unwrap().unwrap().to_vec();
    drop(turn);
    vm.run(entry, &[Value::Str("later".into())], TraceMask::NONE, None)
        .unwrap();
    // The copied handle is not refused: it names the later run's aggregate.
    let root = vm.root(copied).unwrap();
    let values = vm.resolve(&root).unwrap().to_vec();
    assert_eq!(nested_text(&vm, &values[0]), Value::Str("later".into()));
}

#[test]
fn a_returned_turn_collected_before_it_finishes_keeps_older_roots_however_it_ends() {
    let (p, entry) = fixture();
    let args = [Value::Str("returned".into())];
    for ending in ["finish", "drop", "forget"] {
        let mut vm = Vm::new(Echo::default(), Limits::default());
        let entry = FuncRef::new(vm.install(Arc::clone(&p)).unwrap(), entry);
        // An older rooted aggregate, and garbage below the turn so collections move things.
        vm.aggregates_mut().tuple_new(vec![]).unwrap();
        let older = vm.aggregates_mut().tuple_new(vec![Value::I64(7)]).unwrap();
        let older = vm.root(vec![Value::Agg(older)]).unwrap();
        let mut turn = vm.start(entry, &args, TurnOptions::default()).unwrap();
        assert_eq!(turn.drive(u64::MAX).unwrap(), RunStatus::Complete);
        turn.collect().unwrap();
        let kept = match ending {
            "finish" => Some(returned_values(turn)),
            "drop" => {
                drop(turn);
                None
            }
            _ => {
                core::mem::forget(turn);
                None
            }
        };
        let kept = kept.map(|values| vm.root(values).unwrap());
        vm.collect().unwrap();
        let [Value::Agg(older)] = vm.resolve(&older).unwrap() else {
            panic!("the older root");
        };
        assert_eq!(
            vm.aggregates().tuple_get(*older, 0).unwrap(),
            Value::I64(7),
            "{ending}"
        );
        match kept {
            Some(kept) => {
                let values = vm.resolve(&kept).unwrap().to_vec();
                assert_eq!(nested_text(&vm, &values[0]), Value::Str("returned".into()));
            }
            None => assert_eq!(
                vm.aggregates().len_u32(),
                1,
                "{ending}: only the older root survives"
            ),
        }
    }
}
