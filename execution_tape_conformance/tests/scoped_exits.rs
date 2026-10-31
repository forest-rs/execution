// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Typed home returns refer to an exact activation and share the invocation's hard budgets.

use execution_tape::{
    asm::{FunctionSig, ProgramBuilder},
    host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef},
    program::ValueType,
    recording::{CaptureFault, RecordLimits, Recorder, ReplayError},
    trace::{ScopeKind, TraceLocation, TraceMask, TraceOutcome, TraceSink, ValueEvent},
    value::{FuncId, Value},
    verifier::VerifiedProgram,
    vm::{ControlError, InvocationOptions, Limits, RunStatus, Trap, Vm},
};
use std::{cell::RefCell, rc::Rc, sync::Arc};

struct NeverHost;
impl Host for NeverHost {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        _: &[ValueRef<'_>],
        _: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        panic!("control transfer must not dispatch an abandoned native request")
    }
}
struct Fixture {
    program: Arc<VerifiedProgram>,
    root: FuncId,
    middle: FuncId,
    leaf: FuncId,
}
fn fixture() -> Fixture {
    let mut b = ProgramBuilder::new();
    let sig = FunctionSig {
        arg_types: vec![],
        ret_types: vec![ValueType::I64, ValueType::Bool],
    };
    let root = b.declare_function(sig.clone()).unwrap();
    let middle = b.declare_function(sig.clone()).unwrap();
    let leaf = b.declare_function(sig).unwrap();
    let signal = b.host_sig_for(
        "signal",
        HostSig {
            args: vec![],
            rets: vec![],
        },
    );
    let after = b.host_sig_for(
        "after",
        HostSig {
            args: vec![],
            rets: vec![],
        },
    );
    for (function, callee, increment) in [
        (root, Some(middle), 1000),
        (middle, Some(leaf), 100),
        (leaf, None, 0),
    ] {
        let mut a = b.function_asm(function).unwrap();
        let number = a.alloc_register(ValueType::I64);
        let flag = a.alloc_register(ValueType::Bool);
        let amount = a.alloc_register(ValueType::I64);
        if let Some(callee) = callee {
            a.call(0, callee, 0, &[], &[number, flag]);
            a.const_i64(amount, increment)
                .i64_add(number, number, amount);
            if function == root {
                a.host_call_sig_id(0, after, 0, &[], &[]);
            }
        } else {
            a.host_call_sig_id(0, signal, 0, &[], &[])
                .const_i64(number, 1)
                .const_bool(flag, false);
        }
        a.ret(0, &[number, flag]);
        b.define_function(function, a).unwrap();
    }
    Fixture {
        program: Arc::new(b.build_verified().unwrap()),
        root,
        middle,
        leaf,
    }
}
fn options<'a>() -> InvocationOptions<'a> {
    InvocationOptions::default().yield_host_calls()
}

#[test]
fn home_returns_use_target_multiple_values_and_resume_only_its_real_caller() {
    let f = fixture();
    for (frame_index, expected) in [(0, 1107), (1, 1007), (2, 7)] {
        let mut vm = Vm::new(NeverHost, Limits::default());
        let mut run = vm.start(&f.program, f.root, &[], options()).unwrap();
        assert_eq!(run.resume(100).unwrap(), RunStatus::AwaitingHost);
        let frames: Vec<_> = run.inspect().frames().collect();
        assert_eq!(
            frames.iter().map(|f| f.location.func).collect::<Vec<_>>(),
            [f.leaf, f.middle, f.root]
        );
        run.pending_host_call()
            .unwrap()
            .return_from(
                &frames[frame_index].activation,
                vec![Value::I64(7), Value::Bool(true)],
            )
            .unwrap();
        if frame_index != 2 {
            assert_eq!(run.resume(100).unwrap(), RunStatus::AwaitingHost);
            let after = run.pending_host_call().unwrap();
            assert_eq!(after.symbol(), "after");
            after.complete(&mut [], Ok(0)).unwrap();
        }
        assert_eq!(run.resume(100).unwrap(), RunStatus::Complete);
        assert_eq!(
            run.result().unwrap().unwrap(),
            [Value::I64(expected), Value::Bool(true)]
        );
        assert_eq!(run.inspect().frames().len(), 0);
    }
}

#[test]
fn foreign_and_retired_targets_are_recoverable_without_spending_work_or_answering_host() {
    let f = fixture();
    let mut first = Vm::new(NeverHost, Limits::default());
    let mut other = Vm::new(NeverHost, Limits::default());
    let mut run = first.start(&f.program, f.root, &[], options()).unwrap();
    let mut foreign = other.start(&f.program, f.root, &[], options()).unwrap();
    run.resume(100).unwrap();
    foreign.resume(100).unwrap();
    let foreign_target = foreign.inspect().frames().last().unwrap().activation;
    let retired = run.inspect().frames().nth(1).unwrap().activation;
    let fuel = run.remaining_fuel();
    let work = run.data_work();
    assert!(matches!(
        run.pending_host_call()
            .unwrap()
            .return_from(&foreign_target, vec![]),
        Err(ControlError::ForeignInvocation)
    ));
    assert_eq!(run.remaining_fuel(), fuel);
    assert_eq!(run.data_work(), work);
    assert_eq!(run.pending_host_call().unwrap().symbol(), "signal");
    run.pending_host_call()
        .unwrap()
        .return_from(&retired, vec![Value::I64(7), Value::Bool(true)])
        .unwrap();
    run.resume(100).unwrap();
    let fuel = run.remaining_fuel();
    assert!(matches!(
        run.pending_host_call()
            .unwrap()
            .return_from(&retired, vec![]),
        Err(ControlError::ExpiredActivation)
    ));
    assert_eq!(run.remaining_fuel(), fuel);
    assert_eq!(run.pending_host_call().unwrap().symbol(), "after");
    run.pending_host_call()
        .unwrap()
        .complete(&mut [], Ok(0))
        .unwrap();
    assert_eq!(run.resume(100).unwrap(), RunStatus::Complete);
}

#[test]
fn exits_cross_nested_callbacks_or_deliver_to_an_exact_callback_root() {
    let f = fixture();
    for root_target in [false, true] {
        let mut vm = Vm::new(NeverHost, Limits::default());
        let mut run = vm.start(&f.program, f.root, &[], options()).unwrap();
        run.resume(100).unwrap();
        let home = run.inspect().frames().last().unwrap().activation;
        let original = run.pending_host_call().unwrap().location();
        run.pending_host_call()
            .unwrap()
            .call_guest(Value::Func(f.middle), vec![])
            .unwrap();
        run.resume(100).unwrap();
        let callback_home = run.inspect().frames().nth(1).unwrap().activation;
        run.pending_host_call()
            .unwrap()
            .call_guest(Value::Func(f.leaf), vec![])
            .unwrap();
        run.resume(100).unwrap();
        let target = if root_target { &home } else { &callback_home };
        run.pending_host_call()
            .unwrap()
            .return_from(target, vec![Value::I64(9), Value::Bool(true)])
            .unwrap();
        if root_target {
            assert_eq!(run.resume(100).unwrap(), RunStatus::Complete);
            assert_eq!(
                run.result().unwrap().unwrap(),
                [Value::I64(9), Value::Bool(true)]
            );
        } else {
            let mut request = run.pending_host_call().unwrap();
            assert_eq!(request.location(), original);
            assert_eq!(
                request.take_callback_results().unwrap(),
                [Value::I64(9), Value::Bool(true)]
            );
            assert_eq!(request.inspect().frames().len(), 3);
            request.complete(&mut [], Ok(0)).unwrap();
            run.resume(100).unwrap();
            run.pending_host_call()
                .unwrap()
                .complete(&mut [], Ok(0))
                .unwrap();
            assert_eq!(run.resume(100).unwrap(), RunStatus::Complete);
            assert_eq!(
                run.result().unwrap().unwrap(),
                [Value::I64(1101), Value::Bool(false)]
            );
        }
    }
}

#[test]
fn admitted_bad_results_and_exhausted_fuel_terminate_with_no_budget_reset() {
    let f = fixture();
    for case in 0..3 {
        let mut vm = Vm::new(
            NeverHost,
            Limits {
                fuel: if case == 2 { 3 } else { 100 },
                ..Limits::default()
            },
        );
        let mut run = vm.start(&f.program, f.root, &[], options()).unwrap();
        run.resume(100).unwrap();
        let home = run.inspect().frames().last().unwrap().activation;
        let values = match case {
            0 => vec![],
            1 => vec![Value::Bool(true), Value::Bool(true)],
            _ => vec![Value::I64(9), Value::Bool(true)],
        };
        let Err(ControlError::Trapped(error)) =
            run.pending_host_call().unwrap().return_from(&home, values)
        else {
            panic!("admitted invalid return traps");
        };
        match case {
            0 => assert!(matches!(error.trap, Trap::ArityMismatch)),
            1 => assert!(matches!(error.trap, Trap::TypeMismatch { .. })),
            _ => assert!(matches!(error.trap, Trap::FuelExceeded)),
        }
        assert!(run.result().unwrap().is_err());
        assert!(run.pending_host_call().is_none());
        assert!(run.resume(100).is_err());
    }
}

#[derive(Default)]
struct Evidence {
    exits: usize,
    unwind_instructions: usize,
    unwind_frames: usize,
    active_scopes: usize,
    transfer: Option<TraceLocation>,
}
impl TraceSink for Evidence {
    fn mask(&self) -> TraceMask {
        TraceMask::RUN | TraceMask::CALL | TraceMask::HOST | TraceMask::INSTR | TraceMask::VALUES
    }
    fn values(&mut self, _: &execution_tape::program::Program, event: ValueEvent<'_>) {
        if let ValueEvent::ReturnFrom { location, .. } = event {
            self.exits += 1;
            self.transfer = Some(location);
        }
    }
    fn scope_enter(
        &mut self,
        _: &execution_tape::program::Program,
        _: ScopeKind,
        _: usize,
        _: FuncId,
        _: u32,
        _: Option<u64>,
    ) {
        self.active_scopes += 1;
    }
    fn scope_exit(
        &mut self,
        _: &execution_tape::program::Program,
        kind: ScopeKind,
        _: usize,
        _: FuncId,
        _: u32,
        _: Option<u64>,
        outcome: TraceOutcome<'_>,
    ) {
        self.active_scopes -= 1;
        self.unwind_frames += usize::from(
            matches!(kind, ScopeKind::CallFrame { .. })
                && matches!(outcome, TraceOutcome::Unwound { .. }),
        );
    }
    fn instr_end(
        &mut self,
        _: &execution_tape::program::Program,
        _: TraceLocation,
        _: Option<TraceLocation>,
        outcome: TraceOutcome<'_>,
    ) {
        self.unwind_instructions += usize::from(matches!(outcome, TraceOutcome::Unwound { .. }));
    }
}
#[test]
fn abandoned_attempts_and_scopes_are_unwound_not_successful_cancelled_or_trapped() {
    let f = fixture();
    let evidence = Rc::new(RefCell::new(Evidence::default()));
    let mut vm = Vm::new(NeverHost, Limits::default());
    let mut run = vm
        .start(
            &f.program,
            f.root,
            &[],
            options().trace_owned(evidence.mask(), evidence.clone()),
        )
        .unwrap();
    run.resume(100).unwrap();
    let source = run.pending_host_call().unwrap().location();
    let home = run.inspect().frames().last().unwrap().activation;
    run.pending_host_call()
        .unwrap()
        .call_guest(Value::Func(f.middle), vec![])
        .unwrap();
    run.resume(100).unwrap();
    run.pending_host_call()
        .unwrap()
        .call_guest(Value::Func(f.leaf), vec![])
        .unwrap();
    run.resume(100).unwrap();
    let actual_source = run.pending_host_call().unwrap().location();
    run.pending_host_call()
        .unwrap()
        .return_from(&home, vec![Value::I64(3), Value::Bool(true)])
        .unwrap();
    assert_eq!(run.resume(100).unwrap(), RunStatus::Complete);
    assert_eq!(run.remaining_fuel(), 1_000_000 - 9);
    drop(run);
    let evidence = evidence.borrow();
    assert_eq!(evidence.exits, 1);
    assert_eq!(evidence.transfer, Some(actual_source));
    assert_eq!(source.func, f.leaf);
    assert_eq!(evidence.unwind_instructions, 3);
    assert_eq!(evidence.unwind_frames, 6);
    assert_eq!(evidence.active_scopes, 0);
}

#[test]
fn unfinished_transfer_recording_refuses_to_claim_replay_support() {
    let f = fixture();
    let recorder = Rc::new(RefCell::new(Recorder::new(
        f.program.clone(),
        RecordLimits::default(),
    )));
    let mut vm = Vm::new(NeverHost, Limits::default());
    let mut run = vm
        .start(
            &f.program,
            f.root,
            &[],
            options().trace_owned(recorder.mask(), recorder.clone()),
        )
        .unwrap();
    run.resume(100).unwrap();
    let home = run.inspect().frames().last().unwrap().activation;
    run.pending_host_call()
        .unwrap()
        .return_from(&home, vec![Value::I64(3), Value::Bool(true)])
        .unwrap();
    assert_eq!(run.resume(100).unwrap(), RunStatus::Complete);
    drop(run);
    let record = Rc::try_unwrap(recorder).unwrap().into_inner().finish();
    assert!(matches!(
        record.status(),
        Err(CaptureFault::UnsupportedTransfer)
    ));
    assert!(matches!(
        record.replay(),
        Err(ReplayError::Incomplete(CaptureFault::UnsupportedTransfer))
    ));
}

#[test]
fn recursion_keeps_distinct_exit_targets_even_at_the_same_function_and_site() {
    let mut b = ProgramBuilder::new();
    let recursive = b
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let signal = b.host_sig_for(
        "bottom",
        HostSig {
            args: vec![],
            rets: vec![],
        },
    );
    let mut a = b.function_asm(recursive).unwrap();
    let zero = a.alloc_register(ValueType::I64);
    let one = a.alloc_register(ValueType::I64);
    let condition = a.alloc_register(ValueType::Bool);
    let child = a.alloc_register(ValueType::I64);
    let result = a.alloc_register(ValueType::I64);
    let descend = a.label();
    let bottom = a.label();
    a.const_i64(zero, 0)
        .const_i64(one, 1)
        .i64_gt(condition, 1, zero)
        .br(condition, descend, bottom);
    a.place(descend).unwrap();
    a.i64_sub(child, 1, one)
        .call(0, recursive, 0, &[child], &[result])
        .i64_add(result, result, one)
        .ret(0, &[result]);
    a.place(bottom).unwrap();
    a.host_call_sig_id(0, signal, 0, &[], &[]).ret(0, &[zero]);
    b.define_function(recursive, a).unwrap();
    let program = b.build_verified().unwrap();
    let mut vm = Vm::new(NeverHost, Limits::default());
    let args = [Value::I64(5)];
    let mut run = vm.start(&program, recursive, &args, options()).unwrap();
    run.resume(100).unwrap();
    let frames: Vec<_> = run.inspect().frames().collect();
    assert_eq!(frames.len(), 6);
    assert!(frames.iter().all(|frame| frame.location.func == recursive));
    assert_ne!(frames[0].activation, frames[2].activation);
    run.pending_host_call()
        .unwrap()
        .return_from(&frames[2].activation, vec![Value::I64(7)])
        .unwrap();
    assert_eq!(run.resume(100).unwrap(), RunStatus::Complete);
    assert_eq!(run.result().unwrap().unwrap(), [Value::I64(10)]);
}

#[test]
fn promoted_return_graphs_obey_rich_contracts_work_limits_and_root_retirement() {
    use execution_tape::{asm::FunctionAbi, contract::Contract};
    for case in 0..3 {
        let mut b = ProgramBuilder::new();
        let integer = b.contract(Contract::Kind(ValueType::I64)).unwrap();
        let tuple = b.contract(Contract::Tuple(&[Some(integer)])).unwrap();
        let root = b
            .declare_function(FunctionAbi {
                args: vec![],
                rets: vec![tuple.into()],
            })
            .unwrap();
        let signal = b.host_sig_for(
            "opaque",
            HostSig {
                args: vec![],
                rets: vec![ValueType::Agg],
            },
        );
        let mut a = b.function_asm(root).unwrap();
        let value = a.alloc_register(ValueType::Agg);
        a.host_call_sig_id(0, signal, 0, &[], &[value])
            .ret(0, &[value]);
        b.define_function(root, a).unwrap();
        let program = b.build_verified().unwrap();
        let mut vm = Vm::new(
            NeverHost,
            Limits {
                max_contract_work: if case == 2 { 0 } else { 100 },
                ..Limits::default()
            },
        );
        let retained = vm.aggregates_mut().tuple_new(vec![Value::I64(-1)]).unwrap();
        let mut run = vm.start(&program, root, &[], options()).unwrap();
        run.resume(100).unwrap();
        let home = run.inspect().frames().next().unwrap().activation;
        let mut request = run.pending_host_call().unwrap();
        request.context().tuple_new(vec![Value::I64(-2)]).unwrap();
        let result = request
            .context()
            .tuple_new(vec![if case == 1 {
                Value::Bool(true)
            } else {
                Value::I64(42)
            }])
            .unwrap();
        let result = request.return_from(&home, vec![Value::Agg(result)]);
        assert_eq!(run.data_work().aggregate, 4);
        if case == 0 {
            result.unwrap();
            assert_eq!(run.resume(100).unwrap(), RunStatus::Complete);
            let [Value::Agg(handle)] = run.result().unwrap().unwrap() else {
                panic!("aggregate result");
            };
            assert_eq!(
                run.aggregates().tuple_get(*handle, 0).unwrap(),
                Value::I64(42)
            );
            assert_eq!(run.aggregates().len_u32(), 2);
        } else {
            let Err(ControlError::Trapped(trap)) = result else {
                panic!("rich boundary must reject");
            };
            if case == 1 {
                assert!(matches!(trap.trap, Trap::ContractMismatch { .. }));
            } else {
                assert!(matches!(trap.trap, Trap::ContractWorkExceeded { .. }));
            }
            assert_eq!(run.aggregates().len_u32(), 1);
        }
        assert_eq!(
            run.aggregates().tuple_get(retained, 0).unwrap(),
            Value::I64(-1)
        );
    }
}

#[test]
fn payload_ingress_is_charged_once_before_transfer_and_stays_cumulative() {
    for budget in [2, 3] {
        let mut b = ProgramBuilder::new();
        let root = b
            .declare_function(FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::Str],
            })
            .unwrap();
        let signal = b.host_sig_for(
            "signal",
            HostSig {
                args: vec![],
                rets: vec![ValueType::Str],
            },
        );
        let mut a = b.function_asm(root).unwrap();
        let text = a.alloc_register(ValueType::Str);
        a.host_call_sig_id(0, signal, 0, &[], &[text])
            .ret(0, &[text]);
        b.define_function(root, a).unwrap();
        let program = b.build_verified().unwrap();
        let mut vm = Vm::new(
            NeverHost,
            Limits {
                max_payload_work: budget,
                ..Limits::default()
            },
        );
        let mut run = vm.start(&program, root, &[], options()).unwrap();
        run.resume(100).unwrap();
        let home = run.inspect().frames().next().unwrap().activation;
        let text = "abc".to_string();
        let pointer = text.as_ptr();
        let result = run
            .pending_host_call()
            .unwrap()
            .return_from(&home, vec![Value::Str(text)]);
        if budget == 2 {
            assert!(
                matches!(result, Err(ControlError::Trapped(error)) if matches!(error.trap, Trap::PayloadWorkExceeded { .. }))
            );
        } else {
            result.unwrap();
            assert_eq!(run.data_work().payload, 3);
            let [Value::Str(text)] = run.result().unwrap().unwrap() else {
                panic!("string result");
            };
            assert_eq!(text, "abc");
            assert_eq!(text.as_ptr(), pointer);
        }
    }
}

#[test]
fn panicking_transfer_observers_cannot_expose_caller_results_or_retry_the_operation() {
    struct Panics {
        on_scope: bool,
    }
    impl TraceSink for Panics {
        fn mask(&self) -> TraceMask {
            TraceMask::CALL | TraceMask::HOST | TraceMask::VALUES | TraceMask::INSTR
        }
        fn values(&mut self, _: &execution_tape::program::Program, event: ValueEvent<'_>) {
            if !self.on_scope && matches!(event, ValueEvent::ReturnFrom { .. }) {
                panic!("admission observer");
            }
        }
        fn scope_exit(
            &mut self,
            _: &execution_tape::program::Program,
            kind: ScopeKind,
            _: usize,
            _: FuncId,
            _: u32,
            _: Option<u64>,
            outcome: TraceOutcome<'_>,
        ) {
            if self.on_scope
                && matches!(kind, ScopeKind::CallFrame { .. })
                && matches!(outcome, TraceOutcome::Unwound { .. })
            {
                panic!("exit observer");
            }
        }
    }
    let f = fixture();
    for on_scope in [false, true] {
        let mut vm = Vm::new(NeverHost, Limits::default());
        let sink = Panics { on_scope };
        let mut run = vm
            .start(
                &f.program,
                f.root,
                &[],
                options().trace_owned(sink.mask(), sink),
            )
            .unwrap();
        run.resume(100).unwrap();
        let home = run.inspect().frames().last().unwrap().activation;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run.pending_host_call()
                .unwrap()
                .return_from(&home, vec![Value::I64(5), Value::Bool(true)])
                .unwrap();
        }));
        assert!(result.is_err());
        let mut inspection = run.inspect();
        let frames: Vec<_> = inspection.frames().collect();
        assert_eq!(frames.len(), 3);
        assert!(frames.iter().all(|frame| frame.waiting_for_return));
        for frame in frames.iter().skip(1) {
            assert!(
                inspection
                    .register(&frame.activation, 1)
                    .unwrap()
                    .value
                    .is_none()
            );
        }
        drop(inspection);
        assert!(run.pending_host_call().is_none());
        assert!(matches!(
            run.resume(100).unwrap_err().trap,
            Trap::HostCallAbandoned
        ));
        assert!(run.resume(100).is_err());
    }
}
