// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Cooperative quanta preserve the ordinary VM's execution and ownership contracts.

use execution_tape::{
    asm::{FunctionSig, ProgramBuilder},
    host::{AccessSink, Host, HostContext, HostError, HostSig, ResourceKeyRef, SigHash, ValueRef},
    program::{Program, ValueType},
    recording::{CaptureFault, RecordLimits, Recorder, ReplayOutcome},
    trace::{ScopeKind, TraceMask, TraceOutcome, TraceSink},
    value::{FuncId, Value},
    verifier::VerifiedProgram,
    vm::{InvocationOptions, Limits, RunStatus, Trap, Vm},
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
    a.host_call_sig_id(0, echo, 0, &[1], &[str_])
        .tuple_new(tuple, &[1, str_])
        .ret(0, &[tuple]);
    b.define_function(child, a).unwrap();
    let mut a = b.function_asm(entry).unwrap();
    let first = a.alloc_register(ValueType::Agg);
    let second = a.alloc_register(ValueType::Agg);
    let root = a.alloc_register(ValueType::Agg);
    a.call(0, child, 0, &[1], &[first])
        .call(0, child, 0, &[1], &[second])
        .tuple_new(root, &[first, second, first])
        .ret(0, &[root]);
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
        let options = InvocationOptions::default().trace(recorder.mask(), &mut recorder);
        let mut invocation = vm.start(&p, entry, &args, options).unwrap();
        assert_eq!(invocation.resume(0).unwrap(), RunStatus::Suspended);
        assert_eq!(invocation.host().calls, 0);
        assert!(invocation.result().is_none());
        let mut quanta = 0;
        while invocation.resume(quantum).unwrap() == RunStatus::Suspended {
            quanta += 1;
            assert!(quanta < 100, "resume must advance without restarting");
        }
        let fuel = invocation.remaining_fuel();
        assert_eq!(invocation.resume(1).unwrap(), RunStatus::Complete);
        assert_eq!(invocation.remaining_fuel(), fuel);
        assert_eq!(invocation.host().calls, 2);
        let outputs = invocation.take_outputs().unwrap();
        drop(invocation);
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
        _: FuncId,
        _: u32,
        _: Option<u64>,
    ) {
        self.entered += 1;
    }
    fn scope_exit(
        &mut self,
        _: &Program,
        _: ScopeKind,
        _: usize,
        _: FuncId,
        _: u32,
        _: Option<u64>,
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
        let options = InvocationOptions::default().trace(
            TraceMask::RUN | TraceMask::CALL | TraceMask::HOST,
            &mut events,
        );
        let mut invocation = vm.start(&p, entry, &args, options).unwrap();
        // Root call, child host call, child tuple creation; both frames remain active.
        assert_eq!(invocation.resume(3).unwrap(), RunStatus::Suspended);
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
    assert!(vm.run(&p, entry, &args, TraceMask::NONE, None).is_ok());
}

#[test]
fn dropping_a_partial_recording_is_explicitly_incomplete_and_not_replayable() {
    let (p, entry) = fixture();
    let args = [Value::Str("value".into())];
    let mut vm = Vm::new(Echo::default(), Limits::default());
    let mut recorder = Recorder::new(Arc::clone(&p), RecordLimits::default());
    {
        let options = InvocationOptions::default().trace(recorder.mask(), &mut recorder);
        let mut invocation = vm.start(&p, entry, &args, options).unwrap();
        invocation.resume(3).unwrap();
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
        let expected = uninterrupted
            .run(&p, entry, &args, TraceMask::NONE, None)
            .unwrap_err();
        for quantum in [1, 2, 7] {
            let mut vm = Vm::new(Echo::default(), limits.clone());
            let mut invocation = vm
                .start(&p, entry, &args, InvocationOptions::default())
                .unwrap();
            let actual = loop {
                match invocation.resume(quantum) {
                    Err(error) => break error,
                    Ok(RunStatus::Suspended) => {}
                    Ok(RunStatus::AwaitingHost) => {
                        panic!("default invocations dispatch their host synchronously")
                    }
                    Ok(RunStatus::Complete) => panic!("expected the same hard limit"),
                }
            };
            assert_eq!((actual.func, actual.pc), (expected.func, expected.pc));
            assert_eq!(format!("{:?}", actual.trap), format!("{:?}", expected.trap));
            let calls = invocation.host().calls;
            let repeated = invocation.resume(u64::MAX).unwrap_err();
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
    let error = vm
        .start(
            &p,
            entry,
            &[],
            InvocationOptions::default().trace(recorder.mask(), &mut recorder),
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
        let options = InvocationOptions::default().trace(
            TraceMask::RUN | TraceMask::CALL | TraceMask::HOST,
            &mut events,
        );
        let mut invocation = vm.start(&p, entry, &args, options).unwrap();
        invocation.resume(u64::MAX).unwrap();
    }));
    assert!(result.is_err());
    assert_eq!(events.entered, events.cancelled_scopes);
    assert_eq!(events.cancelled_runs, 1);
    assert_eq!(vm.aggregates().len_u32(), 0);
    vm.host_mut().panic = false;
    assert!(vm.run(&p, entry, &args, TraceMask::NONE, None).is_ok());
}

#[test]
fn contract_proof_work_and_successful_proofs_survive_suspension() {
    use execution_tape::{asm::FunctionAbi, contract::Contract};
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
    a.tuple_get(r, 1, 0).ret(0, &[r]);
    b.define_function(child, a).unwrap();
    let entry = b
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::Agg],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let mut a = b.function_asm(entry).unwrap();
    let r = a.alloc_register(ValueType::I64);
    a.call(0, child, 0, &[1], &[r]);
    let second = a.pc();
    a.call(0, child, 0, &[1], &[r]).ret(0, &[r]);
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
    let mut invocation = vm
        .start(&p, entry, &args, InvocationOptions::default())
        .unwrap();
    let error = loop {
        match invocation.resume(1) {
            Ok(RunStatus::Suspended) => {}
            Ok(RunStatus::AwaitingHost) => {
                panic!("default invocations dispatch their host synchronously")
            }
            Ok(RunStatus::Complete) => {
                panic!("the second cached visit must still consume contract work")
            }
            Err(error) => break error,
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
        let mut invocation = vm
            .start(
                &p,
                entry,
                &args,
                InvocationOptions::default().access(&mut reads),
            )
            .unwrap();
        while invocation.resume(1).unwrap() == RunStatus::Suspended {}
    }
    assert_eq!(reads.0, [1, 2]);
}
