// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Reentry keeps the signaling stack, value domain and hard budgets in one invocation.

use execution_tape::{
    asm::{FunctionAbi, FunctionSig, ProgramBuilder},
    contract::Contract,
    host::{AccessSink, Host, HostContext, HostError, HostSig, ResourceKeyRef, SigHash, ValueRef},
    program::{HostTypeId, Program, ValueType},
    recording::{CaptureFault, RecordLimits, Recorder, ReplayError},
    trace::{ScopeKind, TraceLocation, TraceMask, TraceOutcome, TraceSink, ValueEvent},
    value::{AggHandle, Closure, FuncId, Obj, ObjHandle, Value},
    verifier::VerifiedProgram,
    vm::{InvocationOptions, Limits, ObjectRootOrigin, RunStatus, Trap, Vm},
};
use std::{cell::RefCell, rc::Rc, sync::Arc};

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
        panic!("embedding must explicitly complete yielded host calls");
    }
}
fn sig(args: &[ValueType], rets: &[ValueType]) -> FunctionSig {
    FunctionSig {
        arg_types: args.to_vec(),
        ret_types: rets.to_vec(),
    }
}
struct Fixture {
    program: Arc<VerifiedProgram>,
    entry: FuncId,
    signal: FuncId,
    handler: FuncId,
    plus_one: FuncId,
    multi: FuncId,
    object: FuncId,
    text: FuncId,
    tuple: FuncId,
    closure: FuncId,
}
fn fixture() -> Fixture {
    let mut b = ProgramBuilder::new();
    let outer = b.host_sig_for(
        "signal",
        HostSig {
            args: vec![ValueType::I64],
            rets: vec![ValueType::I64],
        },
    );
    let inner = b.host_sig_for(
        "lookup",
        HostSig {
            args: vec![ValueType::I64],
            rets: vec![ValueType::I64],
        },
    );
    let integer = sig(&[ValueType::I64], &[ValueType::I64]);
    let entry = b.declare_function(integer.clone()).unwrap();
    let signal = b.declare_function(integer.clone()).unwrap();
    let handler = b.declare_function(integer.clone()).unwrap();
    let plus_one = b.declare_function(integer).unwrap();
    let multi = b
        .declare_function(sig(&[ValueType::I64], &[ValueType::I64, ValueType::I64]))
        .unwrap();
    let object = b
        .declare_function(sig(
            &[ValueType::Obj(HostTypeId(77))],
            &[ValueType::Obj(HostTypeId(77))],
        ))
        .unwrap();
    let text = b
        .declare_function(sig(&[ValueType::Str], &[ValueType::Str]))
        .unwrap();
    let tuple = b
        .declare_function(sig(&[ValueType::Agg], &[ValueType::Agg]))
        .unwrap();
    let i64_contract = b.contract(Contract::Kind(ValueType::I64)).unwrap();
    let env = b.contract(Contract::Tuple(&[Some(i64_contract)])).unwrap();
    let closure = b
        .declare_function(FunctionAbi {
            args: vec![env.into(), i64_contract.into()],
            rets: vec![i64_contract.into()],
        })
        .unwrap();
    let mut a = b.function_asm(entry).unwrap();
    let r = a.alloc_register(ValueType::I64);
    a.call(0, signal, 0, &[1], &[r]).ret(0, &[r]);
    b.define_function(entry, a).unwrap();
    for (function, import) in [(signal, outer), (handler, inner)] {
        let mut a = b.function_asm(function).unwrap();
        let r = a.alloc_register(ValueType::I64);
        a.host_call_sig_id(0, import, 0, &[1], &[r]).ret(0, &[r]);
        b.define_function(function, a).unwrap();
    }
    let mut a = b.function_asm(plus_one).unwrap();
    let one = a.alloc_register(ValueType::I64);
    let r = a.alloc_register(ValueType::I64);
    a.const_i64(one, 1).i64_add(r, 1, one).ret(0, &[r]);
    b.define_function(plus_one, a).unwrap();
    let mut a = b.function_asm(multi).unwrap();
    let one = a.alloc_register(ValueType::I64);
    let r = a.alloc_register(ValueType::I64);
    a.const_i64(one, 1).i64_add(r, 1, one).ret(0, &[1, r]);
    b.define_function(multi, a).unwrap();
    let mut a = b.function_asm(text).unwrap();
    a.ret(0, &[1]);
    b.define_function(text, a).unwrap();
    let mut a = b.function_asm(object).unwrap();
    a.ret(0, &[1]);
    b.define_function(object, a).unwrap();
    let mut a = b.function_asm(tuple).unwrap();
    let wrapped = a.alloc_register(ValueType::Agg);
    a.tuple_new(wrapped, &[1]).ret(0, &[wrapped]);
    b.define_function(tuple, a).unwrap();
    let mut a = b.function_asm(closure).unwrap();
    let captured = a.alloc_register(ValueType::I64);
    let r = a.alloc_register(ValueType::I64);
    a.tuple_get(captured, 1, 0)
        .i64_add(r, captured, 2)
        .ret(0, &[r]);
    b.define_function(closure, a).unwrap();
    for (function, name) in [
        (entry, "main"),
        (signal, "signal"),
        (handler, "handler"),
        (plus_one, "plus_one"),
        (text, "text"),
        (tuple, "tuple"),
        (closure, "closure"),
    ] {
        b.set_function_name(function, name).unwrap();
    }
    Fixture {
        program: Arc::new(b.build_verified().unwrap()),
        entry,
        signal,
        handler,
        plus_one,
        multi,
        object,
        text,
        tuple,
        closure,
    }
}
fn options<'a>() -> InvocationOptions<'a> {
    InvocationOptions::default().yield_host_calls()
}

#[test]
fn guest_handler_can_wait_and_reenter_again_without_losing_the_signaling_stack() {
    let f = fixture();
    let args = [Value::I64(40)];
    let mut vm = Vm::new(NoHost, Limits::default());
    let mut run = vm.start(&f.program, f.entry, &args, options()).unwrap();
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    let before: Vec<_> = run.inspect().frames().collect();
    assert_eq!(
        before
            .iter()
            .map(|frame| frame.location.func)
            .collect::<Vec<_>>(),
        [f.signal, f.entry]
    );
    let original = run.pending_host_call().unwrap().location();
    run.pending_host_call()
        .unwrap()
        .call_guest(Value::Func(f.handler), vec![Value::I64(40)])
        .unwrap();
    assert!(
        run.pending_host_call().is_none(),
        "callback has not executed its host call yet"
    );
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    assert_eq!(run.pending_host_call().unwrap().symbol(), "lookup");
    let nested: Vec<_> = run.inspect().frames().collect();
    assert_eq!(nested.len(), 3);
    assert_eq!(nested[0].caller, Some(original));
    assert_eq!(nested[1].activation, before[0].activation);
    assert_eq!(nested[2].activation, before[1].activation);
    assert!(nested.iter().all(|frame| frame.waiting_for_return));
    assert!(
        run.inspect()
            .register(&before[0].activation, 2)
            .unwrap()
            .value
            .is_none(),
        "original host result is still unavailable"
    );
    run.pending_host_call()
        .unwrap()
        .call_guest(Value::Func(f.plus_one), vec![Value::I64(40)])
        .unwrap();
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    let mut inner = run.pending_host_call().unwrap();
    assert_eq!(inner.symbol(), "lookup");
    let mut result = inner.take_callback_results().unwrap();
    assert_eq!(result, [Value::I64(41)]);
    assert!(inner.take_callback_results().is_none());
    inner.complete(&mut result, Ok(0)).unwrap();
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    assert!(
        run.result().is_none(),
        "callback completion is not invocation completion"
    );
    let mut outer = run.pending_host_call().unwrap();
    assert_eq!(outer.location(), original);
    assert_eq!(outer.symbol(), "signal");
    let mut result = outer.take_callback_results().unwrap();
    assert_eq!(result, [Value::I64(41)]);
    outer.complete(&mut result, Ok(0)).unwrap();
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::Complete);
    assert_eq!(run.result().unwrap().unwrap(), [Value::I64(41)]);
}

#[test]
fn promoted_host_handles_survive_guest_allocations_and_later_host_construction() {
    let f = fixture();
    let mut vm = Vm::new(NoHost, Limits::default());
    let args = [Value::I64(0)];
    let mut run = vm.start(&f.program, f.entry, &args, options()).unwrap();
    run.resume(u64::MAX).unwrap();
    let mut host = run.pending_host_call().unwrap();
    let staged = host.context().tuple_new(vec![Value::I64(42)]).unwrap();
    let unused = host.context().tuple_new(vec![Value::Bool(false)]).unwrap();
    assert_eq!(staged, AggHandle(0));
    assert_eq!(unused, AggHandle(1));
    drop(host);
    let work = run.data_work();
    run.pending_host_call()
        .unwrap()
        .call_guest(Value::Func(f.tuple), vec![Value::Agg(staged)])
        .unwrap();
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    let mut host = run.pending_host_call().unwrap();
    let returned = host.take_callback_results().unwrap();
    let [Value::Agg(wrapped)] = returned.as_slice() else {
        panic!("tuple callback result");
    };
    assert_eq!(
        host.aggregates().tuple_get(*wrapped, 0).unwrap(),
        Value::Agg(staged)
    );
    assert_eq!(
        host.aggregates().tuple_get(staged, 0).unwrap(),
        Value::I64(42)
    );
    assert_eq!(
        host.aggregates().tuple_get(unused, 0).unwrap(),
        Value::Bool(false)
    );
    let next = host
        .context()
        .tuple_new(vec![Value::Agg(*wrapped), Value::Agg(staged)])
        .unwrap();
    assert!(
        next.0 > wrapped.0,
        "new host staging follows the callback's allocations"
    );
    assert_eq!(
        host.context().tuple_get_ref(next, 1),
        Ok(ValueRef::Agg(staged))
    );
    host.complete(&mut [Value::I64(7)], Ok(0)).unwrap();
    assert!(
        run.data_work().aggregate > work.aggregate,
        "promoted and subsequent work stay charged"
    );
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::Complete);
    assert_eq!(
        run.aggregates().len_u32(),
        0,
        "root commit must reclaim unused promoted/callback/staged nodes"
    );
}

#[test]
fn typed_closure_environment_can_come_from_parent_host_staging() {
    let f = fixture();
    let args = [Value::I64(0)];
    let mut vm = Vm::new(NoHost, Limits::default());
    let mut run = vm.start(&f.program, f.entry, &args, options()).unwrap();
    run.resume(u64::MAX).unwrap();
    let environment = run
        .pending_host_call()
        .unwrap()
        .context()
        .tuple_new(vec![Value::I64(40)])
        .unwrap();
    run.pending_host_call()
        .unwrap()
        .call_guest(
            Value::Closure(Closure {
                func: f.closure,
                env: environment,
            }),
            vec![Value::I64(2)],
        )
        .unwrap();
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    assert_eq!(
        run.pending_host_call().unwrap().take_callback_results(),
        Some(vec![Value::I64(42)])
    );
    // Another callback does not reset the host operation or its original argument.
    run.pending_host_call()
        .unwrap()
        .call_guest(Value::Func(f.plus_one), vec![Value::I64(8)])
        .unwrap();
    run.resume(u64::MAX).unwrap();
    let mut host = run.pending_host_call().unwrap();
    assert_eq!(host.arguments().next(), Some(ValueRef::I64(0)));
    let mut result = host.take_callback_results().unwrap();
    host.complete(&mut result, Ok(0)).unwrap();
    run.resume(u64::MAX).unwrap();
    assert_eq!(run.result().unwrap().unwrap(), [Value::I64(9)]);
}

#[test]
fn moved_callback_text_is_distinct_from_root_argument_sources_and_charged() {
    let f = fixture();
    let args = [Value::I64(1)];
    let mut vm = Vm::new(NoHost, Limits::default());
    let mut run = vm.start(&f.program, f.entry, &args, options()).unwrap();
    run.resume(u64::MAX).unwrap();
    let value = String::from("callback-only text");
    let length = value.len() as u64;
    run.pending_host_call()
        .unwrap()
        .call_guest(Value::Func(f.text), vec![Value::Str(value)])
        .unwrap();
    run.resume(u64::MAX).unwrap();
    assert_eq!(
        run.pending_host_call().unwrap().take_callback_results(),
        Some(vec![Value::Str("callback-only text".into())])
    );
    assert_eq!(
        run.data_work().payload,
        length * 2,
        "host ingress plus result materialization"
    );
}

#[test]
fn callbacks_share_fuel_call_depth_host_call_and_payload_limits() {
    let f = fixture();
    let args = [Value::I64(1)];
    for (limits, expected) in [
        (
            Limits {
                fuel: 2,
                ..Limits::default()
            },
            "fuel",
        ),
        (
            Limits {
                max_call_depth: 2,
                ..Limits::default()
            },
            "depth",
        ),
        (
            Limits {
                max_host_calls: 1,
                ..Limits::default()
            },
            "host",
        ),
        (
            Limits {
                max_payload_work: 0,
                ..Limits::default()
            },
            "payload",
        ),
    ] {
        let mut vm = Vm::new(NoHost, limits);
        let mut run = vm.start(&f.program, f.entry, &args, options()).unwrap();
        run.resume(u64::MAX).unwrap();
        let request = run.pending_host_call().unwrap();
        let result = if expected == "payload" {
            request.call_guest(Value::Func(f.text), vec![Value::Str("x".into())])
        } else {
            request.call_guest(Value::Func(f.handler), vec![Value::I64(1)])
        };
        let error = match result {
            Err(error) => error,
            Ok(()) => run.resume(u64::MAX).unwrap_err(),
        };
        assert!(match expected {
            "fuel" => matches!(error.trap, Trap::FuelExceeded),
            "depth" => matches!(error.trap, Trap::CallDepthExceeded),
            "host" => matches!(error.trap, Trap::HostCallLimitExceeded),
            "payload" => matches!(error.trap, Trap::PayloadWorkExceeded { .. }),
            _ => unreachable!(),
        });
        assert!(run.result().unwrap().is_err());
        assert!(
            run.pending_host_call().is_none(),
            "failed callback admission cannot redispatch its host"
        );
        assert!(
            run.resume(0).is_err(),
            "terminal invocation cannot retry callback work"
        );
    }
}

#[test]
fn callback_rejects_bad_targets_arguments_and_closure_contracts_before_guest_work() {
    let f = fixture();
    let args = [Value::I64(1)];
    for mode in 0..5 {
        let mut vm = Vm::new(NoHost, Limits::default());
        let env = vm
            .aggregates_mut()
            .tuple_new(vec![Value::Bool(false)])
            .unwrap();
        let mut run = vm.start(&f.program, f.entry, &args, options()).unwrap();
        run.resume(u64::MAX).unwrap();
        let (target, args) = match mode {
            0 => (Value::Func(FuncId(u32::MAX)), vec![]),
            1 => (Value::I64(4), vec![]),
            2 => (Value::Func(f.plus_one), vec![]),
            3 => (Value::Func(f.plus_one), vec![Value::Bool(true)]),
            _ => (
                Value::Closure(Closure {
                    func: f.closure,
                    env,
                }),
                vec![Value::I64(2)],
            ),
        };
        let error = run
            .pending_host_call()
            .unwrap()
            .call_guest(target, args)
            .unwrap_err();
        assert!(match mode {
            0 => matches!(error.trap, Trap::InvalidPc),
            1 | 3 => matches!(error.trap, Trap::TypeMismatch { .. }),
            2 => matches!(error.trap, Trap::ArityMismatch),
            _ => matches!(error.trap, Trap::ContractMismatch { .. }),
        });
        assert!(
            run.pending_host_call().is_none(),
            "terminal callback denial cannot revive the host"
        );
        assert_eq!(
            run.inspect().frames().len(),
            0,
            "terminal inspection must not expose active frames"
        );
    }
}

#[derive(Default)]
struct Trace {
    enter: usize,
    exit: usize,
    instr: usize,
    end: usize,
    callback_enter: usize,
    callback_return: usize,
    panic_callback_scope: bool,
}
impl TraceSink for Trace {
    fn instr(&mut self, _: &Program, _: FuncId, _: u32, _: u32, _: Option<u64>, _: u8) {
        self.instr += 1;
    }
    fn instr_end(
        &mut self,
        _: &Program,
        _: TraceLocation,
        _: Option<TraceLocation>,
        _: TraceOutcome<'_>,
    ) {
        self.end += 1;
    }
    fn scope_enter(
        &mut self,
        _: &Program,
        kind: ScopeKind,
        depth: usize,
        _: FuncId,
        _: u32,
        _: Option<u64>,
    ) {
        self.enter += 1;
        assert!(
            !(self.panic_callback_scope
                && depth == 3
                && matches!(kind, ScopeKind::CallFrame { .. })),
            "injected callback entry observer panic"
        );
    }
    fn scope_exit(
        &mut self,
        _: &Program,
        _: ScopeKind,
        _: usize,
        _: FuncId,
        _: u32,
        _: Option<u64>,
        _: TraceOutcome<'_>,
    ) {
        self.exit += 1;
    }
    fn values(&mut self, _: &Program, event: ValueEvent<'_>) {
        match event {
            ValueEvent::CallbackEnter { .. } => self.callback_enter += 1,
            ValueEvent::CallbackReturn { .. } => self.callback_return += 1,
            _ => {}
        }
    }
}
fn mask() -> TraceMask {
    TraceMask::RUN | TraceMask::CALL | TraceMask::HOST | TraceMask::INSTR | TraceMask::VALUES
}
#[test]
fn cancellation_and_traps_close_nested_guest_host_and_instruction_scopes() {
    let f = fixture();
    for trap in [false, true] {
        let capture = Rc::new(RefCell::new(Trace::default()));
        let mut vm = Vm::new(NoHost, Limits::default());
        let args = [Value::I64(1)];
        let mut run = vm
            .start(
                &f.program,
                f.entry,
                &args,
                options().trace_owned(mask(), capture.clone()),
            )
            .unwrap();
        run.resume(u64::MAX).unwrap();
        run.pending_host_call()
            .unwrap()
            .context()
            .tuple_new(vec![Value::I64(1)])
            .unwrap();
        run.pending_host_call()
            .unwrap()
            .call_guest(Value::Func(f.handler), vec![Value::I64(1)])
            .unwrap();
        run.resume(u64::MAX).unwrap();
        if trap {
            run.pending_host_call()
                .unwrap()
                .complete(&mut [], Err(HostError::Failed))
                .unwrap_err();
        }
        drop(run);
        assert_eq!(
            vm.aggregates().len_u32(),
            0,
            "cancelling nested guest reentry rolls back its shared heap"
        );
        let capture = capture.borrow();
        assert_eq!(capture.enter, capture.exit);
        assert_eq!(capture.instr, capture.end);
        assert_eq!((capture.callback_enter, capture.callback_return), (1, 0));
    }
}
#[test]
fn callback_observations_do_not_claim_a_replayable_capture_without_a_driver_transcript() {
    let f = fixture();
    let recorder = Rc::new(RefCell::new(Recorder::new(
        f.program.clone(),
        RecordLimits::default(),
    )));
    let mut vm = Vm::new(NoHost, Limits::default());
    let args = [Value::I64(1)];
    let mut run = vm
        .start(
            &f.program,
            f.entry,
            &args,
            options().trace_owned(recorder.mask(), recorder.clone()),
        )
        .unwrap();
    run.resume(u64::MAX).unwrap();
    run.pending_host_call()
        .unwrap()
        .call_guest(Value::Func(f.plus_one), vec![Value::I64(1)])
        .unwrap();
    run.resume(u64::MAX).unwrap();
    let mut host = run.pending_host_call().unwrap();
    let mut result = host.take_callback_results().unwrap();
    host.complete(&mut result, Ok(0)).unwrap();
    run.resume(u64::MAX).unwrap();
    drop(run);
    let record = Rc::try_unwrap(recorder)
        .expect("observer retired")
        .into_inner()
        .finish();
    assert_eq!(record.status(), Err(&CaptureFault::UnsupportedCallback));
    assert!(matches!(
        record.replay(),
        Err(ReplayError::Incomplete(CaptureFault::UnsupportedCallback))
    ));
}
#[test]
#[cfg(panic = "unwind")]
fn observer_panic_after_entering_callback_prevents_retry_and_keeps_inspection_valid() {
    let f = fixture();
    for target in [f.handler, f.signal] {
        let capture = Rc::new(RefCell::new(Trace {
            panic_callback_scope: true,
            ..Trace::default()
        }));
        let mut vm = Vm::new(NoHost, Limits::default());
        let args = [Value::I64(1)];
        let mut run = vm
            .start(
                &f.program,
                f.entry,
                &args,
                options().trace_owned(mask(), capture.clone()),
            )
            .unwrap();
        run.resume(u64::MAX).unwrap();
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run.pending_host_call()
                    .unwrap()
                    .call_guest(Value::Func(target), vec![Value::I64(1)])
                    .unwrap();
            }))
            .is_err()
        );
        assert!(
            run.pending_host_call().is_none(),
            "poisoned callback cannot be redispatched"
        );
        assert_eq!(run.inspect().frames().next().unwrap().location.func, target);
        assert!(
            !run.inspect().frames().next().unwrap().waiting_for_return,
            "poison source belongs to the suspended parent, even for recursive reentry"
        );
        let error = run.resume(u64::MAX).unwrap_err();
        assert!(matches!(error.trap, Trap::HostCallAbandoned));
        drop(run);
        assert_eq!(capture.borrow().enter, capture.borrow().exit);
        assert_eq!(capture.borrow().instr, capture.borrow().end);
    }
}

#[test]
fn callback_result_signature_is_independent_of_the_pending_host_signature() {
    let f = fixture();
    let capture = Rc::new(RefCell::new(Trace::default()));
    let mut vm = Vm::new(NoHost, Limits::default());
    let args = [Value::I64(40)];
    let mut run = vm
        .start(
            &f.program,
            f.entry,
            &args,
            options().trace_owned(mask(), capture.clone()),
        )
        .unwrap();
    run.resume(u64::MAX).unwrap();
    run.pending_host_call()
        .unwrap()
        .call_guest(Value::Func(f.multi), vec![Value::I64(40)])
        .unwrap();
    run.resume(u64::MAX).unwrap();
    let mut request = run.pending_host_call().unwrap();
    let mut results = request.take_callback_results().unwrap();
    assert_eq!(results, [Value::I64(40), Value::I64(41)]);
    request.complete(&mut results[1..], Ok(0)).unwrap();
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::Complete);
    assert_eq!(run.result().unwrap().unwrap(), [Value::I64(41)]);
    drop(run);
    let trace = capture.borrow();
    assert_eq!((trace.callback_enter, trace.callback_return), (1, 1));
    assert_eq!(trace.enter, trace.exit);
    assert_eq!(
        trace.instr, trace.end,
        "parent host attempt must complete exactly once"
    );
}

#[test]
fn callback_results_must_satisfy_their_own_rich_contract_before_returning_to_host() {
    let mut b = ProgramBuilder::new();
    let i64_contract = b.contract(Contract::Kind(ValueType::I64)).unwrap();
    let result = b.contract(Contract::Tuple(&[Some(i64_contract)])).unwrap();
    let callback = b
        .declare_function(FunctionAbi {
            args: vec![],
            rets: vec![result.into()],
        })
        .unwrap();
    let import = b.host_sig_for(
        "opaque",
        HostSig {
            args: vec![],
            rets: vec![ValueType::Agg],
        },
    );
    let mut a = b.function_asm(callback).unwrap();
    let r = a.alloc_register(ValueType::Agg);
    a.host_call_sig_id(0, import, 0, &[], &[r]).ret(0, &[r]);
    b.define_function(callback, a).unwrap();
    let entry = b.declare_function(sig(&[], &[ValueType::Agg])).unwrap();
    let mut a = b.function_asm(entry).unwrap();
    let r = a.alloc_register(ValueType::Agg);
    a.host_call_sig_id(0, import, 0, &[], &[r]).ret(0, &[r]);
    b.define_function(entry, a).unwrap();
    let program = b.build_verified().unwrap();
    let mut vm = Vm::new(NoHost, Limits::default());
    let mut run = vm.start(&program, entry, &[], options()).unwrap();
    run.resume(u64::MAX).unwrap();
    run.pending_host_call()
        .unwrap()
        .call_guest(Value::Func(callback), vec![])
        .unwrap();
    run.resume(u64::MAX).unwrap();
    let mut request = run.pending_host_call().unwrap();
    let wrong = request
        .context()
        .tuple_new(vec![Value::Bool(true)])
        .unwrap();
    request.complete(&mut [Value::Agg(wrong)], Ok(0)).unwrap();
    let error = run.resume(u64::MAX).unwrap_err();
    assert!(matches!(error.trap, Trap::ContractMismatch { function, .. } if function == callback));
    assert!(
        run.pending_host_call().is_none(),
        "failed callback return cannot revive the outer host"
    );
    assert_eq!(
        run.aggregates().len_u32(),
        0,
        "callback trap rolls back shared invocation aggregates"
    );
}

#[test]
fn promoted_staging_and_callback_allocations_share_one_aggregate_budget() {
    let f = fixture();
    let args = [Value::I64(1)];
    let mut vm = Vm::new(
        NoHost,
        Limits {
            max_aggregate_work: 4,
            ..Limits::default()
        },
    );
    let mut run = vm.start(&f.program, f.entry, &args, options()).unwrap();
    run.resume(u64::MAX).unwrap();
    let staged = run
        .pending_host_call()
        .unwrap()
        .context()
        .tuple_new(vec![Value::I64(1)])
        .unwrap();
    run.pending_host_call()
        .unwrap()
        .call_guest(Value::Func(f.tuple), vec![Value::Agg(staged)])
        .unwrap();
    run.resume(u64::MAX).unwrap();
    let mut request = run.pending_host_call().unwrap();
    let error = request
        .context()
        .tuple_new(vec![Value::I64(2)])
        .unwrap_err();
    assert!(matches!(
        std::error::Error::source(&error)
            .unwrap()
            .downcast_ref::<Trap>(),
        Some(Trap::AggregateWorkExceeded { used: 4, .. })
    ));
    request.complete(&mut [Value::I64(42)], Ok(0)).unwrap();
    run.resume(u64::MAX).unwrap();
    assert_eq!(run.data_work().aggregate, 4);
    assert_eq!(run.result().unwrap().unwrap(), [Value::I64(42)]);
}

#[test]
fn pending_callback_object_results_remain_visible_to_the_embedding_collector() {
    let f = fixture();
    let mut vm = Vm::new(NoHost, Limits::default());
    let args = [Value::I64(1)];
    let object = Obj {
        host_type: HostTypeId(77),
        handle: ObjHandle(42),
    };
    let mut run = vm.start(&f.program, f.entry, &args, options()).unwrap();
    run.resume(u64::MAX).unwrap();
    run.pending_host_call()
        .unwrap()
        .call_guest(Value::Func(f.object), vec![Value::Obj(object)])
        .unwrap();
    let activation = run.inspect().frames().next().unwrap().activation;
    run.resume(u64::MAX).unwrap();
    assert!(
        run.inspect().register(&activation, 1).is_none(),
        "returned callback frame must expire"
    );
    assert_eq!(
        run.pending_host_call().unwrap().callback_results(),
        Some(&[Value::Obj(object)][..])
    );
    let mut roots = Vec::new();
    run.inspect().visit_objects(|root| roots.push(root));
    assert_eq!(
        roots.len(),
        1,
        "only the stored result retains this direct object token"
    );
    assert_eq!(roots[0].object, object);
    assert_eq!(
        roots[0].origin,
        ObjectRootOrigin::CallbackResult { index: 0 }
    );
    let _external_root = run
        .pending_host_call()
        .unwrap()
        .take_callback_results()
        .unwrap();
    let mut remaining = 0;
    run.inspect().visit_objects(|_| remaining += 1);
    assert_eq!(remaining, 0, "taken slots are now embedding-owned roots");
}

#[test]
fn original_host_state_arguments_and_access_sink_are_shared_across_callback_dispatch() {
    #[derive(Default)]
    struct Reads(Vec<String>);
    impl AccessSink for Reads {
        fn read(&mut self, key: ResourceKeyRef<'_>) {
            let ResourceKeyRef::Input(name) = key else {
                panic!("named host read");
            };
            self.0.push(name.into());
        }
        fn write(&mut self, _: ResourceKeyRef<'_>) {
            panic!("read-only host");
        }
    }
    #[derive(Default)]
    struct Echo(Vec<String>);
    impl Host for Echo {
        fn call(
            &mut self,
            symbol: &str,
            _: SigHash,
            args: &[ValueRef<'_>],
            rets: &mut [Value],
            mut cx: HostContext<'_, '_>,
        ) -> Result<u64, HostError> {
            self.0.push(symbol.into());
            cx.record_read(ResourceKeyRef::Input(symbol));
            rets[0] = args[0].to_value();
            Ok(0)
        }
    }
    let f = fixture();
    let reads = Rc::new(RefCell::new(Reads::default()));
    let args = [Value::I64(40)];
    let mut vm = Vm::new(Echo::default(), Limits::default());
    let mut run = vm
        .start(
            &f.program,
            f.entry,
            &args,
            options().access_owned(reads.clone()),
        )
        .unwrap();
    run.resume(u64::MAX).unwrap();
    run.pending_host_call()
        .unwrap()
        .call_guest(Value::Func(f.handler), vec![Value::I64(2)])
        .unwrap();
    run.resume(u64::MAX).unwrap();
    run.pending_host_call().unwrap().dispatch().unwrap();
    run.resume(u64::MAX).unwrap();
    assert_eq!(
        run.pending_host_call().unwrap().callback_results(),
        Some(&[Value::I64(2)][..])
    );
    run.pending_host_call().unwrap().dispatch().unwrap();
    run.resume(u64::MAX).unwrap();
    assert_eq!(
        run.result().unwrap().unwrap(),
        [Value::I64(40)],
        "outer dispatch uses its original arguments"
    );
    assert_eq!(run.host().0, ["lookup", "signal"]);
    assert_eq!(reads.borrow().0, ["lookup", "signal"]);
}
