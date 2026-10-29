// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Admitted host instructions can wait without replaying effects or weakening validation.

use execution_tape::{
    asm::{FunctionSig, ProgramBuilder},
    host::{AccessSink, Host, HostContext, HostError, HostSig, ResourceKeyRef, SigHash, ValueRef},
    program::{Program, ValueType},
    recording::{CaptureFault, RecordLimits, Recorder, ReplayOutcome},
    source::{SourceDocument, SourceId, SourceLimits, SourceMap, SourceMapping},
    trace::{
        RegisterEvent, ScopeKind, TraceLocation, TraceMask, TraceOutcome, TraceSink, ValueEvent,
    },
    value::{FuncId, Value},
    verifier::VerifiedProgram,
    vm::{InvocationOptions, Limits, RunStatus, Trap, Vm},
};
use std::{cell::RefCell, rc::Rc, sync::Arc};

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
        mut context: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        self.calls += 1;
        assert!(!self.panic, "injected host panic");
        context.record_read(ResourceKeyRef::Input("message"));
        rets[0] = args[0].to_value();
        Ok(2)
    }
}
struct Fixture {
    program: Arc<VerifiedProgram>,
    entry: FuncId,
    child: FuncId,
    pc: u32,
    result: u32,
    sources: SourceMap,
}
fn fixture(calls: usize, nested: bool) -> Fixture {
    let mut b = ProgramBuilder::new();
    let import = b.host_sig_for(
        "echo",
        HostSig {
            args: vec![ValueType::Str],
            rets: vec![ValueType::Str],
        },
    );
    let sig = FunctionSig {
        arg_types: vec![ValueType::Str],
        ret_types: vec![ValueType::Str],
    };
    let child = b.declare_function(sig.clone()).unwrap();
    let mut a = b.function_asm(child).unwrap();
    let tmp = a.alloc_register(ValueType::Agg);
    let result = a.alloc_register(ValueType::Str);
    a.tuple_new(tmp, &[1]);
    let pc = a.pc();
    for _ in 0..calls {
        a.host_call_sig_id(0, import, 0, &[1], &[result]);
    }
    a.ret(0, &[result]);
    b.define_function(child, a).unwrap();
    let entry = if nested {
        let root = b.declare_function(sig).unwrap();
        let mut a = b.function_asm(root).unwrap();
        let result = a.alloc_register(ValueType::Str);
        a.call(0, child, 0, &[1], &[result]).ret(0, &[result]);
        b.define_function(root, a).unwrap();
        root
    } else {
        child
    };
    let program = Arc::new(b.build_verified().unwrap());
    let sources = SourceMap::new(
        program.program(),
        vec![SourceDocument {
            name: "message.expr".into(),
            text: "echo(message)".into(),
        }],
        vec![SourceMapping {
            location: TraceLocation { func: child, pc },
            document: SourceId(0),
            range: 0..13,
        }],
        SourceLimits::default(),
    )
    .unwrap();
    Fixture {
        program,
        entry,
        child,
        pc,
        result,
        sources,
    }
}
fn args() -> [Value; 1] {
    [Value::Str("borrowed".into())]
}
#[derive(Default)]
struct Access(usize);
impl AccessSink for Access {
    fn read(&mut self, key: ResourceKeyRef<'_>) {
        assert!(
            matches!(key, ResourceKeyRef::Input("message")),
            "host access must retain its original resource key"
        );
        self.0 += 1;
    }
    fn write(&mut self, _: ResourceKeyRef<'_>) {}
}

#[test]
fn pending_request_borrows_inputs_keeps_source_and_hides_unreturned_destinations() {
    let f = fixture(1, true);
    let args = args();
    let Value::Str(text) = &args[0] else {
        unreachable!()
    };
    let mut vm = Vm::new(Echo::default(), Limits::default());
    let mut access = Access::default();
    let mut call = vm
        .start(
            &f.program,
            f.entry,
            &args,
            InvocationOptions::default()
                .yield_host_calls()
                .access(&mut access),
        )
        .unwrap();
    assert!(call.pending_host_call().is_none());
    assert_eq!(call.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    let fuel = call.remaining_fuel();
    let work = call.data_work();
    assert_eq!(call.host().calls, 0);
    for quantum in [0, 1, u64::MAX] {
        let mut request = call.pending_host_call().unwrap();
        assert_eq!(
            request.location(),
            TraceLocation {
                func: f.child,
                pc: f.pc
            }
        );
        assert_eq!(request.symbol(), "echo");
        assert_eq!(request.argument_types(), &[ValueType::Str]);
        assert_eq!(request.return_types(), &[ValueType::Str]);
        assert_eq!(request.import().0, 0);
        assert_eq!(
            request.signature_hash(),
            execution_tape::host::sig_hash(&HostSig {
                args: vec![ValueType::Str],
                rets: vec![ValueType::Str]
            })
        );
        let ValueRef::Str(value) = request.arguments().next().unwrap() else {
            panic!("string argument")
        };
        assert_eq!(
            value.as_ptr(),
            text.as_ptr(),
            "suspension does not clone payloads"
        );
        assert_eq!(
            request
                .context()
                .retain(&[ValueRef::from_value(&args[0])])
                .unwrap()
                .values(),
            &args
        );
        assert_eq!(
            f.sources
                .bind(request.program().program())
                .unwrap()
                .lookup(request.location())
                .unwrap()
                .text(),
            "echo(message)"
        );
        drop(request);
        assert_eq!(call.resume(quantum).unwrap(), RunStatus::AwaitingHost);
        assert_eq!(call.remaining_fuel(), fuel);
        assert_eq!(call.data_work(), work);
    }
    let mut inspection = call.inspect();
    let frames: Vec<_> = inspection.frames().collect();
    assert_eq!(frames.len(), 2);
    assert!(frames.iter().all(|frame| frame.waiting_for_return));
    assert_eq!(frames[0].location.pc, f.pc);
    assert!(
        inspection
            .register(&frames[0].activation, f.result)
            .unwrap()
            .value
            .is_none()
    );
    drop(inspection);
    call.pending_host_call().unwrap().dispatch().unwrap();
    assert!(call.pending_host_call().is_none());
    assert_eq!(call.host().calls, 1);
    assert_eq!(
        call.remaining_fuel(),
        fuel - 2,
        "only host extra fuel is charged at completion"
    );
    assert_eq!(call.resume(u64::MAX).unwrap(), RunStatus::Complete);
    assert_eq!(call.result().unwrap().unwrap(), &args);
    drop(call);
    assert_eq!(access.0, 1);
    assert_eq!(
        vm.aggregates().len_u32(),
        0,
        "unused guest allocations retire"
    );
}

#[test]
fn external_completion_runs_no_native_host_and_finishes_only_its_instruction() {
    let f = fixture(2, false);
    let args = args();
    let mut vm = Vm::new(Echo::default(), Limits::default());
    let mut recorder = Recorder::new(Arc::clone(&f.program), RecordLimits::default());
    let mut call = vm
        .start(
            &f.program,
            f.entry,
            &args,
            InvocationOptions::default()
                .yield_host_calls()
                .trace(recorder.mask(), &mut recorder),
        )
        .unwrap();
    let mut requests = 0;
    loop {
        match call.resume(1).unwrap() {
            RunStatus::Suspended => {}
            RunStatus::AwaitingHost => {
                let before = call.remaining_fuel();
                let mut request = call.pending_host_call().unwrap();
                request
                    .context()
                    .record_read(ResourceKeyRef::Input("message"));
                request
                    .complete(&mut [Value::Str(format!("response {requests}"))], Ok(3))
                    .unwrap();
                requests += 1;
                assert_eq!(call.remaining_fuel(), before - 3);
                assert!(
                    call.result().is_none(),
                    "host completion does not execute guest return"
                );
            }
            RunStatus::Complete => break,
        }
    }
    assert_eq!(requests, 2);
    assert_eq!(call.host().calls, 0);
    assert_eq!(
        call.result().unwrap().unwrap(),
        &[Value::Str("response 1".into())]
    );
    let work = call.data_work();
    assert_eq!(work.aggregate, 2);
    assert_eq!(work.payload, 8 + 10 + 10 + 10); // tuple copy, two host results, root copy
    drop(call);
    let record = recorder.finish();
    record.status().unwrap();
    assert_eq!(record.host_calls().count(), 2);
    let ReplayOutcome::Returned(values) = record.replay().unwrap() else {
        panic!("returned replay")
    };
    assert_eq!(values.values(), &[Value::Str("response 1".into())]);
}

#[test]
fn return_arity_kind_payload_and_owned_host_errors_use_normal_admission_and_rollback() {
    let f = fixture(1, true);
    let args = args();
    for case in 0..4 {
        let mut vm = Vm::new(
            Echo::default(),
            Limits {
                max_payload_work: 10,
                ..Limits::default()
            },
        );
        let root = vm.aggregates_mut().tuple_new(vec![Value::I64(7)]).unwrap();
        let mut recorder = Recorder::new(Arc::clone(&f.program), RecordLimits::default());
        let mut call = vm
            .start(
                &f.program,
                f.entry,
                &args,
                InvocationOptions::default()
                    .yield_host_calls()
                    .trace(recorder.mask(), &mut recorder),
            )
            .unwrap();
        assert_eq!(call.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
        let fuel = call.remaining_fuel();
        let outcome = match case {
            0 => Ok((vec![], 2)),
            1 => Ok((vec![Value::Bool(true)], 2)),
            2 => Ok((vec![Value::Str("payload".into())], 2)),
            _ => Err(HostError::custom(std::io::Error::other("operation failed"))),
        };
        let (mut values, outcome) = match outcome {
            Ok((values, fuel)) => (values, Ok(fuel)),
            Err(error) => (Vec::new(), Err(error)),
        };
        let error = call
            .pending_host_call()
            .unwrap()
            .complete(&mut values, outcome)
            .unwrap_err();
        assert_eq!((error.func, error.pc), (f.child, f.pc));
        match (&error.trap, case) {
            (
                Trap::HostReturnArityMismatch {
                    expected: 1,
                    actual: 0,
                },
                0,
            ) => {}
            (
                Trap::TypeMismatch {
                    expected: ValueType::Str,
                    actual: ValueType::Bool,
                },
                1,
            ) => {}
            (
                Trap::PayloadWorkExceeded {
                    limit: 10,
                    used: 8,
                    requested: 7,
                },
                2,
            ) => {}
            (Trap::HostCallFailed { error, .. }, 3) => {
                assert!(
                    std::error::Error::source(error)
                        .unwrap()
                        .is::<std::io::Error>()
                );
            }
            _ => panic!("unexpected outcome {case}: {error}"),
        }
        assert_eq!(call.remaining_fuel(), fuel - if case == 3 { 0 } else { 2 });
        assert!(call.pending_host_call().is_none());
        assert!(call.result().unwrap().is_err());
        assert_eq!(call.resume(1).unwrap_err().to_string(), error.to_string());
        drop(call);
        assert_eq!(vm.aggregates().len_u32(), 1);
        assert_eq!(vm.aggregates().tuple_get(root, 0).unwrap(), Value::I64(7));
        assert_eq!(vm.host().calls, 0);
        let record = recorder.finish();
        record.status().unwrap();
        assert!(matches!(
            record.replay().unwrap(),
            ReplayOutcome::Trapped(_)
        ));
    }
}

#[test]
fn hard_limits_deny_requests_before_external_effects_and_waiting_cannot_replenish_them() {
    let f = fixture(2, false);
    let args = args();
    for max_host_calls in [0, 1] {
        let mut vm = Vm::new(
            Echo::default(),
            Limits {
                max_host_calls,
                ..Limits::default()
            },
        );
        let mut call = vm
            .start(
                &f.program,
                f.entry,
                &args,
                InvocationOptions::default().yield_host_calls(),
            )
            .unwrap();
        if max_host_calls == 1 {
            assert_eq!(call.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
            call.pending_host_call().unwrap().dispatch().unwrap();
        }
        assert!(matches!(
            call.resume(u64::MAX).unwrap_err().trap,
            Trap::HostCallLimitExceeded
        ));
        assert!(call.pending_host_call().is_none());
        assert_eq!(call.host().calls as u64, max_host_calls);
    }
    for fuel in [0, 1] {
        let mut vm = Vm::new(
            Echo::default(),
            Limits {
                fuel,
                ..Limits::default()
            },
        );
        let mut call = vm
            .start(
                &f.program,
                f.entry,
                &args,
                InvocationOptions::default().yield_host_calls(),
            )
            .unwrap();
        assert!(matches!(
            call.resume(u64::MAX).unwrap_err().trap,
            Trap::FuelExceeded
        ));
        assert!(call.pending_host_call().is_none());
        assert_eq!(call.host().calls, 0);
    }
}

#[test]
fn async_producer_borrows_payload_across_a_real_pending_poll_then_completes_once() {
    use std::{
        future::{Future, poll_fn},
        task::{Context, Poll, Waker},
    };
    let f = fixture(1, true);
    let args = args();
    let Value::Str(input) = &args[0] else {
        unreachable!()
    };
    let mut vm = Vm::new(Echo::default(), Limits::default());
    let mut call = vm
        .start(
            &f.program,
            f.entry,
            &args,
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    assert_eq!(call.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    let fuel = call.remaining_fuel();
    let mut producer = Box::pin(
        call.pending_host_call()
            .unwrap()
            .complete_with(async |request| {
                let ValueRef::Str(value) = request.arguments().next().unwrap() else {
                    panic!("string input")
                };
                let mut first = true;
                poll_fn(|cx| {
                    if first {
                        first = false;
                        cx.waker().wake_by_ref();
                        Poll::Pending
                    } else {
                        Poll::Ready(())
                    }
                })
                .await;
                assert_eq!(value.as_ptr(), input.as_ptr());
                Ok((vec![Value::Str(value.into())], 2))
            }),
    );
    let mut cx = Context::from_waker(Waker::noop());
    assert!(producer.as_mut().poll(&mut cx).is_pending());
    assert!(matches!(
        producer.as_mut().poll(&mut cx),
        Poll::Ready(Ok(()))
    ));
    drop(producer);
    assert_eq!(call.remaining_fuel(), fuel - 2);
    assert!(call.pending_host_call().is_none());
    assert_eq!(call.host().calls, 0);
    assert_eq!(call.resume(u64::MAX).unwrap(), RunStatus::Complete);
    assert_eq!(call.result().unwrap().unwrap(), &args);
}

#[test]
fn cancelling_a_polled_async_delivery_prevents_redispatch_and_keeps_effects() {
    use std::{
        cell::Cell,
        future::{Future, pending},
        task::{Context, Waker},
    };
    let f = fixture(1, true);
    let args = args();
    let effects = Cell::new(0);
    let mut vm = Vm::new(Echo::default(), Limits::default());
    let mut recorder = Recorder::new(Arc::clone(&f.program), RecordLimits::default());
    let mut call = vm
        .start(
            &f.program,
            f.entry,
            &args,
            InvocationOptions::default()
                .yield_host_calls()
                .trace(recorder.mask(), &mut recorder),
        )
        .unwrap();
    assert_eq!(call.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    let budget = (call.remaining_fuel(), call.data_work());
    let mut producer = Box::pin(call.pending_host_call().unwrap().complete_with(async |_| {
        effects.set(effects.get() + 1);
        pending::<()>().await;
        Ok((vec![Value::Str("unused".into())], 0))
    }));
    assert!(
        producer
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    drop(producer);
    assert_eq!(
        effects.get(),
        1,
        "performed external effects are not rolled back"
    );
    assert!(
        call.pending_host_call().is_none(),
        "an abandoned producer cannot be dispatched again"
    );
    assert!(matches!(
        call.resume(1).unwrap_err().trap,
        Trap::HostCallAbandoned
    ));
    assert_eq!(
        (call.remaining_fuel(), call.data_work()),
        budget,
        "abandonment preserves spent budget diagnostics"
    );
    assert_eq!(call.host().calls, 0);
    drop(call);
    assert!(matches!(
        recorder.finish().status(),
        Err(CaptureFault::MissingCompletion)
    ));
}

#[test]
fn an_unpolled_async_delivery_leaves_the_request_unclaimed() {
    let f = fixture(1, false);
    let args = args();
    let mut vm = Vm::new(Echo::default(), Limits::default());
    let mut call = vm
        .start(
            &f.program,
            f.entry,
            &args,
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    assert_eq!(call.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    drop(call.pending_host_call().unwrap().complete_with(async |_| {
        panic!("unpolled producer must not run");
    }));
    assert_eq!(call.resume(0).unwrap(), RunStatus::AwaitingHost);
    call.pending_host_call().unwrap().dispatch().unwrap();
    assert_eq!(call.host().calls, 1);
    assert_eq!(call.resume(u64::MAX).unwrap(), RunStatus::Complete);
}

#[test]
fn successful_completion_moves_payloads_from_reusable_slots_without_an_extra_copy() {
    let f = fixture(1, false);
    let args = args();
    let mut vm = Vm::new(Echo::default(), Limits::default());
    let mut call = vm
        .start(
            &f.program,
            f.entry,
            &args,
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    assert_eq!(call.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    let text = String::from("owned reply");
    let pointer = text.as_ptr();
    let mut slots = [Value::Str(text)];
    call.pending_host_call()
        .unwrap()
        .complete(&mut slots, Ok(0))
        .unwrap();
    assert_eq!(slots, [Value::Unit]);
    let mut inspection = call.inspect();
    let frame = inspection.frames().next().unwrap();
    assert!(!frame.waiting_for_return);
    let register = inspection.register(&frame.activation, f.result).unwrap();
    let Some(ValueRef::Str(value)) = register.value else {
        panic!("admitted return")
    };
    assert_eq!(
        value.as_ptr(),
        pointer,
        "the owned payload moves into VM storage"
    );
}

#[test]
fn owned_code_and_inputs_survive_a_dropped_lookup_while_a_host_request_is_pending() {
    use execution_tape::callable::Callable;
    let f = fixture(1, false);
    let weak = Arc::downgrade(&f.program);
    let mut vm = Vm::new(Echo::default(), Limits::default());
    let mut call = Callable::function(f.program, f.entry)
        .unwrap()
        .start(
            &mut vm,
            args().to_vec(),
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    assert_eq!(call.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    assert!(weak.upgrade().is_some());
    assert_eq!(call.pending_host_call().unwrap().arguments().len(), 1);
    call.pending_host_call().unwrap().dispatch().unwrap();
    assert_eq!(call.resume(u64::MAX).unwrap(), RunStatus::Complete);
    assert_eq!(call.result().unwrap().unwrap(), &args());
    drop(call);
    assert!(weak.upgrade().is_none());
}

#[test]
fn empty_and_wide_host_boundaries_use_the_same_request_and_completion_contract() {
    struct Fill;
    impl Host for Fill {
        fn call(
            &mut self,
            _: &str,
            _: SigHash,
            args: &[ValueRef<'_>],
            values: &mut [Value],
            _: HostContext<'_, '_>,
        ) -> Result<u64, HostError> {
            for (slot, value) in values.iter_mut().zip(args) {
                *slot = value.to_value();
            }
            Ok(0)
        }
    }
    for count in [0_u32, 9, 32] {
        let mut b = ProgramBuilder::new();
        let types = vec![ValueType::I64; count as usize];
        let import = b.host_sig_for(
            "fill",
            HostSig {
                args: types.clone(),
                rets: types.clone(),
            },
        );
        let entry = b
            .declare_function(FunctionSig {
                arg_types: types.clone(),
                ret_types: types,
            })
            .unwrap();
        let mut a = b.function_asm(entry).unwrap();
        let results: Vec<_> = (0..count)
            .map(|_| a.alloc_register(ValueType::I64))
            .collect();
        let inputs: Vec<_> = (1..=count).collect();
        a.host_call_sig_id(0, import, 0, &inputs, &results)
            .ret(0, &results);
        b.define_function(entry, a).unwrap();
        let p = b.build_verified().unwrap();
        let args: Vec<_> = (0..count).map(|n| Value::I64(i64::from(n))).collect();
        for dispatch in [false, true] {
            let mut vm = Vm::new(Fill, Limits::default());
            let mut call = vm
                .start(
                    &p,
                    entry,
                    &args,
                    InvocationOptions::default().yield_host_calls(),
                )
                .unwrap();
            assert_eq!(call.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
            let request = call.pending_host_call().unwrap();
            assert_eq!(request.arguments().len(), count as usize);
            if dispatch {
                request.dispatch().unwrap();
            } else {
                request.complete(&mut args.clone(), Ok(0)).unwrap();
            }
            assert_eq!(call.resume(u64::MAX).unwrap(), RunStatus::Complete);
            assert_eq!(call.result().unwrap().unwrap(), &args);
        }
    }
}

#[test]
fn cancellation_of_a_pending_host_call_is_incomplete_and_releases_only_new_aggregates() {
    let f = fixture(1, true);
    let args = args();
    let mut vm = Vm::new(Echo::default(), Limits::default());
    vm.aggregates_mut().tuple_new(vec![Value::I64(7)]).unwrap();
    let mut recorder = Recorder::new(Arc::clone(&f.program), RecordLimits::default());
    let mut call = vm
        .start(
            &f.program,
            f.entry,
            &args,
            InvocationOptions::default()
                .yield_host_calls()
                .trace(recorder.mask(), &mut recorder),
        )
        .unwrap();
    assert_eq!(call.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    drop(call.pending_host_call().unwrap());
    drop(call);
    assert_eq!(vm.aggregates().len_u32(), 1);
    assert_eq!(vm.host().calls, 0);
    assert!(matches!(
        recorder.finish().status(),
        Err(CaptureFault::MissingCompletion)
    ));
}

#[derive(Clone, Default)]
struct Log(Rc<RefCell<Vec<String>>>);
impl TraceSink for Log {
    fn run_start(&mut self, _: &Program, entry: FuncId, count: usize) {
        self.0.borrow_mut().push(format!("start {entry:?} {count}"));
    }
    fn instr(&mut self, _: &Program, func: FuncId, pc: u32, next: u32, _: Option<u64>, _: u8) {
        self.0
            .borrow_mut()
            .push(format!("instruction {func:?} {pc} {next}"));
    }
    fn instr_end(
        &mut self,
        _: &Program,
        location: TraceLocation,
        next: Option<TraceLocation>,
        outcome: TraceOutcome<'_>,
    ) {
        self.0
            .borrow_mut()
            .push(format!("end {location:?} {next:?} {outcome:?}"));
    }
    fn scope_enter(
        &mut self,
        _: &Program,
        kind: ScopeKind,
        depth: usize,
        func: FuncId,
        pc: u32,
        _: Option<u64>,
    ) {
        self.0
            .borrow_mut()
            .push(format!("enter {kind:?} {depth} {func:?} {pc}"));
    }
    fn scope_exit(
        &mut self,
        _: &Program,
        kind: ScopeKind,
        depth: usize,
        func: FuncId,
        pc: u32,
        _: Option<u64>,
        outcome: TraceOutcome<'_>,
    ) {
        self.0
            .borrow_mut()
            .push(format!("exit {kind:?} {depth} {func:?} {pc} {outcome:?}"));
    }
    fn values(&mut self, _: &Program, event: ValueEvent<'_>) {
        self.0.borrow_mut().push(format!("value {event:?}"));
    }
    fn registers(&mut self, _: &Program, event: RegisterEvent<'_>) {
        self.0.borrow_mut().push(format!(
            "registers {:?} {:?} {:?}",
            event.location,
            event.phase,
            event.values.iter().collect::<Vec<_>>()
        ));
    }
    fn run_end(&mut self, _: &Program, outcome: TraceOutcome<'_>) {
        self.0.borrow_mut().push(format!("run end {outcome:?}"));
    }
}

#[test]
fn yielded_dispatch_has_exactly_the_same_trace_register_phases_and_fuel_as_sync_execution() {
    let f = fixture(2, true);
    let args = args();
    let mask = TraceMask::RUN
        | TraceMask::INSTR
        | TraceMask::CALL
        | TraceMask::HOST
        | TraceMask::VALUES
        | TraceMask::REGISTERS;
    let mut expected = None;
    for yielding in [false, true] {
        let mut log = Log::default();
        let mirror = log.clone();
        let mut vm = Vm::new(Echo::default(), Limits::default());
        let options = InvocationOptions::default().trace(mask, &mut log);
        let mut call = vm
            .start(
                &f.program,
                f.entry,
                &args,
                if yielding {
                    options.yield_host_calls()
                } else {
                    options
                },
            )
            .unwrap();
        loop {
            match call.resume(1).unwrap() {
                RunStatus::Complete => break,
                RunStatus::Suspended => {}
                RunStatus::AwaitingHost => {
                    let before = mirror.0.borrow().len();
                    assert!(
                        mirror
                            .0
                            .borrow()
                            .last()
                            .unwrap()
                            .starts_with("enter HostCall")
                    );
                    assert_eq!(call.resume(0).unwrap(), RunStatus::AwaitingHost);
                    assert_eq!(mirror.0.borrow().len(), before);
                    call.pending_host_call().unwrap().dispatch().unwrap();
                }
            }
        }
        let fuel = call.remaining_fuel();
        drop(call);
        let observations = mirror.0.borrow().clone();
        if let Some((ref expected, expected_fuel)) = expected {
            assert_eq!(&observations, expected);
            assert_eq!(fuel, expected_fuel);
        } else {
            expected = Some((observations, fuel));
        }
    }
}

#[cfg(panic = "unwind")]
#[test]
fn unwound_host_delivery_cannot_retry_effects_and_records_cancellation_instead_of_a_guest_trap() {
    let f = fixture(1, true);
    let args = args();
    let mut vm = Vm::new(
        Echo {
            calls: 0,
            panic: true,
        },
        Limits::default(),
    );
    let mut recorder = Recorder::new(Arc::clone(&f.program), RecordLimits::default());
    let mut call = vm
        .start(
            &f.program,
            f.entry,
            &args,
            InvocationOptions::default()
                .yield_host_calls()
                .trace(recorder.mask(), &mut recorder),
        )
        .unwrap();
    assert_eq!(call.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        call.pending_host_call().unwrap().dispatch()
    }));
    assert!(result.is_err());
    assert!(call.pending_host_call().is_none());
    assert_eq!(call.host().calls, 1);
    assert!(matches!(
        call.resume(0).unwrap_err().trap,
        Trap::HostCallAbandoned
    ));
    assert!(matches!(
        call.resume(u64::MAX).unwrap_err().trap,
        Trap::HostCallAbandoned
    ));
    assert_eq!(call.host().calls, 1);
    drop(call);
    assert_eq!(vm.aggregates().len_u32(), 0);
    assert!(matches!(
        recorder.finish().status(),
        Err(CaptureFault::MissingCompletion)
    ));
}

#[cfg(panic = "unwind")]
#[test]
fn abandonment_is_terminal_even_when_a_cancellation_observer_panics() {
    use std::{
        cell::Cell,
        future::{Future, pending},
        panic::{AssertUnwindSafe, catch_unwind},
        task::{Context, Waker},
    };
    struct PanicOnCancellation(Rc<Cell<usize>>);
    impl TraceSink for PanicOnCancellation {
        fn instr_end(
            &mut self,
            _: &Program,
            _: TraceLocation,
            _: Option<TraceLocation>,
            outcome: TraceOutcome<'_>,
        ) {
            if matches!(outcome, TraceOutcome::Cancelled) {
                self.0.set(self.0.get() + 1);
                panic!("injected cancellation observer panic");
            }
        }
    }
    let f = fixture(1, false);
    let args = args();
    let cancellations = Rc::new(Cell::new(0));
    let mut observer = PanicOnCancellation(Rc::clone(&cancellations));
    let mut vm = Vm::new(Echo::default(), Limits::default());
    let mut call = vm
        .start(
            &f.program,
            f.entry,
            &args,
            InvocationOptions::default()
                .yield_host_calls()
                .trace(TraceMask::INSTR, &mut observer),
        )
        .unwrap();
    assert_eq!(call.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    let mut producer = Box::pin(call.pending_host_call().unwrap().complete_with(async |_| {
        pending::<()>().await;
        Ok((Vec::new(), 0))
    }));
    assert!(
        producer
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    drop(producer);
    let budget = (call.remaining_fuel(), call.data_work());
    assert!(catch_unwind(AssertUnwindSafe(|| call.resume(1))).is_err());
    assert!(matches!(
        call.result()
            .expect("terminal before notifying observers")
            .unwrap_err()
            .trap,
        Trap::HostCallAbandoned
    ));
    assert!(matches!(
        call.resume(u64::MAX).unwrap_err().trap,
        Trap::HostCallAbandoned
    ));
    assert_eq!((call.remaining_fuel(), call.data_work()), budget);
    assert_eq!(call.host().calls, 0);
    assert_eq!(cancellations.get(), 1);
    drop(call);
    assert_eq!(vm.aggregates().len_u32(), 0);
    assert_eq!(
        cancellations.get(),
        1,
        "drop cannot re-notify the panicking observer"
    );
}
