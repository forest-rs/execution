// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Reconstruct control decisions; recompute guest callback results and preserve nested evidence.

use execution_tape::{
    asm::{FunctionSig, ProgramBuilder},
    host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef},
    program::ValueType,
    recording::{
        CaptureFault, CaptureResource, ControlAction, InstructionCapture, InstructionStatus,
        RecordLimits, Recorder, Recording, RegisterWatch, ReplayError, ReplayOutcome,
    },
    trace::{TraceLocation, TraceSink},
    value::{Closure, FuncId, Value},
    verifier::VerifiedProgram,
    vm::{InvocationOptions, Limits, RunStatus, Trap, Vm},
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
        panic!("recording/replay must never dispatch a live native host");
    }
}
struct Fixture {
    program: Arc<VerifiedProgram>,
    root: FuncId,
    nested: FuncId,
    closure: FuncId,
    root_host: TraceLocation,
    host_result: u32,
}
fn fixture() -> Fixture {
    let mut b = ProgramBuilder::new();
    let abi = FunctionSig {
        arg_types: vec![ValueType::Agg],
        ret_types: vec![ValueType::Agg],
    };
    let root = b.declare_function(abi.clone()).unwrap();
    let nested = b.declare_function(abi).unwrap();
    let closure = b
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::Agg, ValueType::Agg],
            ret_types: vec![ValueType::Agg],
        })
        .unwrap();
    let outer = b.host_sig_for(
        "outer",
        HostSig {
            args: vec![ValueType::Agg],
            rets: vec![ValueType::Agg],
        },
    );
    let inner = b.host_sig_for(
        "inner",
        HostSig {
            args: vec![ValueType::Agg],
            rets: vec![ValueType::Agg],
        },
    );
    let mut a = b.function_asm(root).unwrap();
    let number = a.alloc_register(ValueType::I64);
    let unused = a.alloc_register(ValueType::Agg);
    let host_result = a.alloc_register(ValueType::Agg);
    let result = a.alloc_register(ValueType::Agg);
    a.const_i64(number, 11).tuple_new(unused, &[number]);
    let root_host = TraceLocation {
        func: root,
        pc: a.pc(),
    };
    a.host_call_sig_id(0, outer, 0, &[1], &[host_result])
        .tuple_new(result, &[host_result])
        .ret(0, &[result]);
    b.define_function(root, a).unwrap();
    let mut a = b.function_asm(nested).unwrap();
    let wrapped = a.alloc_register(ValueType::Agg);
    let result = a.alloc_register(ValueType::Agg);
    a.tuple_new(wrapped, &[1])
        .host_call_sig_id(0, inner, 0, &[wrapped], &[result])
        .ret(0, &[result]);
    b.define_function(nested, a).unwrap();
    let mut a = b.function_asm(closure).unwrap();
    let result = a.alloc_register(ValueType::Agg);
    a.tuple_new(result, &[1, 2]).ret(0, &[result]);
    b.define_function(closure, a).unwrap();
    Fixture {
        program: Arc::new(b.build_verified().unwrap()),
        root,
        nested,
        closure,
        root_host,
        host_result,
    }
}
fn finish(recorder: Rc<RefCell<Recorder>>) -> Recording {
    Rc::try_unwrap(recorder)
        .expect("invocation observer retired")
        .into_inner()
        .finish()
}
fn nested_recording(max_steps: Option<usize>, limits: RecordLimits) -> Recording {
    let f = fixture();
    let mut vm = Vm::new(NoHost, Limits::default());
    vm.aggregates_mut().tuple_new(vec![Value::I64(-1)]).unwrap(); // Unobserved initial node.
    let shared = vm
        .aggregates_mut()
        .tuple_new(vec![Value::Bytes(vec![7; 4096])])
        .unwrap();
    let distinct = vm
        .aggregates_mut()
        .tuple_new(vec![Value::Bytes(vec![7; 4096])])
        .unwrap();
    let input = vm
        .aggregates_mut()
        .tuple_new(vec![
            Value::Agg(shared),
            Value::Agg(shared),
            Value::Agg(distinct),
        ])
        .unwrap();
    let mut recorder = Recorder::new(f.program.clone(), limits);
    if let Some(max_steps) = max_steps {
        recorder = recorder.with_instructions(InstructionCapture {
            max_steps,
            watches: vec![RegisterWatch {
                location: f.root_host,
                registers: vec![1, f.host_result],
            }],
        });
    }
    let recorder = Rc::new(RefCell::new(recorder));
    let args = [Value::Agg(input)];
    let mut run = vm
        .start(
            &f.program,
            f.root,
            &args,
            InvocationOptions::default()
                .yield_host_calls()
                .trace_owned(recorder.mask(), recorder.clone()),
        )
        .unwrap();
    assert_eq!(
        run.resume(u64::MAX).unwrap(),
        RunStatus::AwaitingHost,
        "root waits for outer host"
    );
    let mut outer = run.pending_host_call().unwrap();
    let unused = outer.context().tuple_new(vec![Value::I64(77)]).unwrap();
    let staged = outer
        .context()
        .tuple_new(vec![
            Value::Agg(input),
            Value::Agg(input),
            Value::Bytes(vec![9; 32]),
        ])
        .unwrap();
    outer
        .call_guest(Value::Func(f.nested), vec![Value::Agg(staged)])
        .unwrap();
    assert_eq!(
        run.resume(u64::MAX).unwrap(),
        RunStatus::AwaitingHost,
        "nested callback waits for inner host"
    );
    let inner = run.pending_host_call().unwrap();
    assert_eq!(inner.symbol(), "inner", "nested request retains its import");
    let value = inner.arguments().next().unwrap().to_value();
    inner
        .call_guest(
            Value::Closure(Closure {
                func: f.closure,
                env: staged,
            }),
            vec![value],
        )
        .unwrap();
    assert_eq!(
        run.resume(u64::MAX).unwrap(),
        RunStatus::AwaitingHost,
        "closure returns to inner request"
    );
    let mut inner = run.pending_host_call().unwrap();
    let mut values = inner.take_callback_results().unwrap();
    inner.complete(&mut values, Ok(2)).unwrap();
    assert_eq!(
        run.resume(u64::MAX).unwrap(),
        RunStatus::AwaitingHost,
        "outer callback returns to original request"
    );
    let mut outer = run.pending_host_call().unwrap();
    assert_eq!(
        outer.symbol(),
        "outer",
        "parent request survives nested callbacks"
    );
    let mut values = outer.take_callback_results().unwrap();
    let result = outer
        .context()
        .tuple_new(vec![
            values.pop().unwrap(),
            Value::Agg(staged),
            Value::Agg(unused),
        ])
        .unwrap();
    outer.complete(&mut [Value::Agg(result)], Ok(3)).unwrap();
    assert_eq!(
        run.resume(u64::MAX).unwrap(),
        RunStatus::Complete,
        "root publishes after outer completion"
    );
    drop(run);
    drop(vm);
    finish(recorder)
}
#[test]
fn nested_callbacks_replay_with_closures_parent_staging_and_aliases_after_vm_drop() {
    let record = nested_recording(None, RecordLimits::default());
    record.status().unwrap();
    assert_eq!(record.stats().control_commands, 2);
    let commands: Vec<_> = record.control_commands().collect();
    assert_eq!(commands[0].sequence, 0);
    assert_eq!(commands[1].sequence, 1);
    let ControlAction::CallGuest {
        work,
        constructed,
        returned: Some(outer_return),
        ..
    } = &commands[0].action
    else {
        panic!("outer callback returned");
    };
    assert_eq!(work.aggregate, 6);
    assert_eq!(work.payload, 32);
    assert_eq!(
        constructed.len(),
        2,
        "even staging unused during callbacks must be restored"
    );
    let ControlAction::CallGuest {
        returned: Some(inner_return),
        ..
    } = &commands[1].action
    else {
        panic!("inner callback returned");
    };
    let calls: Vec<_> = record.host_calls().collect();
    assert_eq!(
        (
            inner_return.sequence,
            calls[0].sequence,
            outer_return.sequence,
            calls[1].sequence
        ),
        (2, 3, 4, 5)
    );
    assert_eq!(commands[1].stack.as_ref().unwrap().len(), 2);
    assert_eq!(inner_return.stack.as_ref().unwrap().len(), 3);
    let ReplayOutcome::Returned(snapshot) = record.replay().unwrap() else {
        panic!("recomputed guest results");
    };
    let heap = snapshot.aggregates();
    let Value::Agg(wrapper) = snapshot.values()[0] else {
        panic!("root wrapper");
    };
    let Value::Agg(root) = heap.tuple_get(wrapper, 0).unwrap() else {
        panic!("native final record");
    };
    let Value::Agg(callback) = heap.tuple_get(root, 0).unwrap() else {
        panic!("callback result");
    };
    let staged = heap.tuple_get(root, 1).unwrap();
    assert_eq!(
        heap.tuple_get(callback, 0).unwrap(),
        staged,
        "closure environment identity survives"
    );
    let Value::Agg(inner_argument) = heap.tuple_get(callback, 1).unwrap() else {
        panic!("nested guest argument");
    };
    assert_eq!(heap.tuple_get(inner_argument, 0).unwrap(), staged);
    let Value::Agg(staged) = staged else {
        panic!("staged input");
    };
    assert_eq!(
        heap.tuple_get(staged, 0).unwrap(),
        heap.tuple_get(staged, 1).unwrap()
    );
    let Value::Agg(input) = heap.tuple_get(staged, 0).unwrap() else {
        panic!("original graph");
    };
    assert_eq!(
        heap.tuple_get(input, 0).unwrap(),
        heap.tuple_get(input, 1).unwrap()
    );
    assert_ne!(
        heap.tuple_get(input, 0).unwrap(),
        heap.tuple_get(input, 2).unwrap(),
        "distinct equal nodes stay distinct"
    );
    let Value::Agg(unused) = heap.tuple_get(root, 2).unwrap() else {
        panic!("previously unused staging");
    };
    assert_eq!(heap.tuple_get(unused, 0).unwrap(), Value::I64(77));
}
#[test]
fn nested_attempts_and_late_parent_outputs_survive_instruction_prefix_limits() {
    for max_steps in [0, 2, 3, 4, 1000] {
        let record = nested_recording(Some(max_steps), RecordLimits::default());
        record.status().unwrap();
        if max_steps == 1000 {
            assert_eq!(
                record.instruction_status(),
                Some(InstructionStatus::Complete)
            );
            assert!(
                record
                    .instructions()
                    .any(|step| step.parent_attempt.is_some())
            );
        } else {
            assert_eq!(
                record.instruction_status(),
                Some(InstructionStatus::Truncated { limit: max_steps })
            );
        }
        if max_steps >= 3 {
            let host = record.instruction(2).unwrap();
            assert!(!host.inputs.is_empty());
            assert!(
                !host.outputs.is_empty(),
                "watched parent results arrive after callback truncation"
            );
        }
        assert!(matches!(
            record.replay().unwrap(),
            ReplayOutcome::Returned(_)
        ));
    }
}
#[test]
fn control_capture_budget_is_explicit_without_stopping_the_live_guest() {
    let record = nested_recording(
        Some(1000),
        RecordLimits {
            control_commands: 1,
            ..RecordLimits::default()
        },
    );
    assert_eq!(
        record.status(),
        Err(&CaptureFault::Budget {
            resource: CaptureResource::ControlCommands,
            limit: 1
        })
    );
    assert!(matches!(record.replay(), Err(ReplayError::Incomplete(_))));
}
#[test]
fn rejected_callback_admission_and_nested_host_failure_replay_as_actual_traps() {
    let f = fixture();
    for mode in 0..5 {
        let recorder = Rc::new(RefCell::new(
            Recorder::new(f.program.clone(), RecordLimits::default())
                .with_instructions(InstructionCapture::default()),
        ));
        let limits = match mode {
            3 => Limits {
                max_call_depth: 1,
                ..Limits::default()
            },
            4 => Limits {
                fuel: 3,
                ..Limits::default()
            },
            _ => Limits::default(),
        };
        let mut vm = Vm::new(NoHost, limits);
        let input = vm.aggregates_mut().tuple_new(vec![Value::I64(1)]).unwrap();
        let args = [Value::Agg(input)];
        let mut run = vm
            .start(
                &f.program,
                f.root,
                &args,
                InvocationOptions::default()
                    .yield_host_calls()
                    .trace_owned(recorder.mask(), recorder.clone()),
            )
            .unwrap();
        run.resume(u64::MAX).unwrap();
        let error = if mode == 0 {
            run.pending_host_call()
                .unwrap()
                .call_guest(Value::Func(FuncId(u32::MAX)), vec![])
                .unwrap_err()
        } else if mode == 1 {
            run.pending_host_call()
                .unwrap()
                .call_guest(Value::Func(f.nested), vec![Value::Bool(false)])
                .unwrap_err()
        } else if mode >= 3 {
            run.pending_host_call()
                .unwrap()
                .call_guest(Value::Func(f.nested), vec![Value::Agg(input)])
                .unwrap_err()
        } else {
            run.pending_host_call()
                .unwrap()
                .call_guest(Value::Func(f.nested), vec![Value::Agg(input)])
                .unwrap();
            run.resume(u64::MAX).unwrap();
            run.pending_host_call()
                .unwrap()
                .complete(&mut [], Err(HostError::Failed))
                .unwrap_err()
        };
        assert!(matches!(
            error.trap,
            Trap::InvalidPc
                | Trap::TypeMismatch { .. }
                | Trap::HostCallFailed { .. }
                | Trap::FuelExceeded
                | Trap::CallDepthExceeded
        ));
        drop(run);
        let record = finish(recorder);
        record.status().unwrap();
        assert_eq!(
            record.instruction_status(),
            Some(InstructionStatus::Complete)
        );
        let ControlAction::CallGuest { returned, .. } =
            record.control_commands().next().unwrap().action;
        assert!(
            returned.is_none(),
            "failed callbacks cannot acquire a successful return"
        );
        assert!(matches!(
            record.replay().unwrap(),
            ReplayOutcome::Trapped(_)
        ));
    }
}
#[test]
fn cancelled_callback_is_inspectable_but_cannot_replay_as_complete() {
    let f = fixture();
    for return_first in [false, true] {
        let recorder = Rc::new(RefCell::new(Recorder::new(
            f.program.clone(),
            RecordLimits::default(),
        )));
        let mut vm = Vm::new(NoHost, Limits::default());
        let input = vm.aggregates_mut().tuple_new(vec![]).unwrap();
        let args = [Value::Agg(input)];
        let mut run = vm
            .start(
                &f.program,
                f.root,
                &args,
                InvocationOptions::default()
                    .yield_host_calls()
                    .trace_owned(recorder.mask(), recorder.clone()),
            )
            .unwrap();
        run.resume(u64::MAX).unwrap();
        run.pending_host_call()
            .unwrap()
            .call_guest(Value::Func(f.nested), vec![Value::Agg(input)])
            .unwrap();
        run.resume(u64::MAX).unwrap();
        if return_first {
            run.pending_host_call()
                .unwrap()
                .complete(&mut [Value::Agg(input)], Ok(0))
                .unwrap();
            assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
        }
        drop(run);
        let record = finish(recorder);
        assert_eq!(record.status(), Err(&CaptureFault::MissingCompletion));
        assert_eq!(record.control_commands().len(), 1);
        let ControlAction::CallGuest { returned, .. } =
            record.control_commands().next().unwrap().action;
        assert_eq!(
            returned.is_some(),
            return_first,
            "guest return never fabricates parent completion"
        );
        assert!(matches!(
            record.replay(),
            Err(ReplayError::Incomplete(CaptureFault::MissingCompletion))
        ));
    }
}
