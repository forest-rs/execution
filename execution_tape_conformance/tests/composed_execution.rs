// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Cross-code control uses one VM, heap transaction and resource authority.

use execution_tape::recording::{
    CaptureFault, CaptureResource, InstructionCapture, InstructionStatus, Outcome, RecordLimits,
    RecordedValue, Recorder, Recording, TransferAction,
};
use execution_tape::{
    asm::{FunctionSig, ProgramBuilder},
    host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef},
    program::ValueType,
    trace::{ControlEvent, ScopeKind, TraceLocation, TraceMask, TraceOutcome, TraceSink},
    value::{FuncId, Value},
    verifier::VerifiedProgram,
    vm::{ControlError, InvocationOptions, Limits, RunStatus, Trap, Vm},
};
use std::{cell::RefCell, rc::Rc, sync::Arc};

fn recorded_composition(nonlocal: bool, limits: RecordLimits) -> Recording {
    let (parent, entry) = parent();
    let parent = Arc::new(parent);
    let (child, method) = if nonlocal {
        signalling_child()
    } else {
        child()
    };
    let mut vm = Vm::new(NoHost, Limits::default());
    let instance = vm.instantiate(&child).unwrap();
    let mut recorder =
        Recorder::new(parent.clone(), limits).with_instructions(InstructionCapture::default());
    {
        let mask = recorder.mask();
        let mut run = vm
            .start(
                &parent,
                entry,
                &[],
                InvocationOptions::default()
                    .yield_host_calls()
                    .trace(mask, &mut recorder),
            )
            .unwrap();
        run.resume(100).unwrap();
        let home = run.inspect().frames().next().unwrap().activation;
        let request = run.pending_host_call().unwrap();
        let argument = request.arguments().next().unwrap().to_value();
        request
            .call_guest(
                instance.function(method).expect("verified guest target"),
                vec![argument.clone()],
            )
            .unwrap();
        run.resume(100).unwrap();
        if nonlocal {
            run.pending_host_call()
                .unwrap()
                .return_from(&home, vec![argument])
                .unwrap();
        } else {
            let mut request = run.pending_host_call().unwrap();
            let mut values = request.take_callback_results().unwrap();
            request.complete(&mut values, Ok(0)).unwrap();
        }
        assert_eq!(
            run.resume(100).unwrap(),
            RunStatus::Complete,
            "capture admission never changes guest execution"
        );
    }
    drop(vm);
    drop(parent);
    recorder.finish()
}

#[test]
fn composed_capture_retains_exact_code_and_sharing_after_the_store_is_gone() {
    let record = recorded_composition(false, RecordLimits::default());
    assert_eq!(
        record.status(),
        Ok(()),
        "all child evidence and actual return relationships are owned"
    );
    let segments: Vec<_> = record.segments().collect();
    assert_eq!(
        segments.len(),
        2,
        "the actual child has its own evidence domain"
    );
    assert_eq!(
        segments[1].program.program().name(),
        Some("independent method"),
        "overlapping local function IDs keep their exact definitions"
    );
    assert_eq!(
        segments[1].parent.unwrap().segment,
        0,
        "the child's actual parent is retained"
    );
    let transfers: Vec<_> = record.transfers().collect();
    assert_eq!(
        transfers.len(),
        2,
        "entry and actual successful child return are distinct observations"
    );
    let TransferAction::Enter {
        args: [RecordedValue::Agg(parent_id)],
        ..
    } = transfers[0].action
    else {
        panic!("aggregate argument")
    };
    let child = segments[1].recording.expect("observed child");
    let [RecordedValue::Agg(child_id)] = child.invocation().unwrap().args else {
        panic!("child aggregate argument")
    };
    assert_eq!(
        record.aggregate_identity(*parent_id),
        child.aggregate_identity(*child_id),
        "cross-segment identity preserves the same original shared node"
    );
    assert_eq!(
        record.instruction_status(),
        Some(InstructionStatus::Complete),
        "retained root instructions remain complete across child execution"
    );
    assert!(
        record.replay().is_ok(),
        "checked replay must execute the child in the same fresh store: {:?}",
        record.replay()
    );
}

#[test]
fn composed_capture_distinguishes_unwind_from_an_unobserved_or_successful_child_return() {
    let record = recorded_composition(true, RecordLimits::default());
    assert_eq!(
        record.status(),
        Ok(()),
        "a fully observed non-local transfer is complete evidence"
    );
    let segments: Vec<_> = record.segments().collect();
    assert!(
        matches!(
            segments[1].recording.unwrap().outcome(),
            Some(Outcome::Transferred {
                segment: 0,
                activation: 0
            })
        ),
        "the child was abandoned by the actual root transfer"
    );
    assert!(
        matches!(record.transfers().last().unwrap().action, TransferAction::ReturnFrom { target, .. } if target.segment == 0),
        "the initiating command retains its qualified target"
    );
    assert_eq!(
        record.instruction_status(),
        Some(InstructionStatus::Complete),
        "cross-code exit completion is recorded without inventing a local control command"
    );
    assert!(
        record.replay().is_ok(),
        "checked replay must reissue the actual cross-code non-local transfer: {:?}",
        record.replay()
    );
}

#[test]
fn capture_limits_do_not_reset_for_a_new_segment() {
    let record = recorded_composition(
        false,
        RecordLimits {
            aggregates: 1,
            ..RecordLimits::default()
        },
    );
    assert!(
        matches!(
            record.status(),
            Err(CaptureFault::Budget {
                resource: CaptureResource::Aggregates,
                limit: 1
            })
        ),
        "parent and child arena retention share one capture allowance"
    );
    assert!(
        record.stats().aggregates <= 1,
        "the capture rejects further allocation before exceeding the global limit"
    );
    assert_eq!(
        record.fault_program().unwrap().program().name(),
        Some("independent method"),
        "a shared capture failure retains the child's source domain"
    );
}

#[test]
fn qualified_guest_calls_inherit_the_execution_observer() {
    let (parent, entry) = parent();
    let parent = Arc::new(parent);
    let (child, method) = child();
    let mut vm = Vm::new(NoHost, Limits::default());
    let instance = vm.instantiate(&child).unwrap();
    let mut recorder = Recorder::new(parent.clone(), RecordLimits::default());
    {
        let mask = recorder.mask();
        let mut run = vm
            .start(
                &parent,
                entry,
                &[],
                InvocationOptions::default()
                    .yield_host_calls()
                    .trace(mask, &mut recorder),
            )
            .unwrap();
        run.resume(100).unwrap();
        let request = run.pending_host_call().unwrap();
        let argument = request.arguments().next().unwrap().to_value();
        request
            .call_guest(
                instance.function(method).expect("verified guest target"),
                vec![argument],
            )
            .unwrap();
        run.resume(100).unwrap();
        let mut request = run.pending_host_call().unwrap();
        let mut values = request.take_callback_results().unwrap();
        request.complete(&mut values, Ok(0)).unwrap();
        run.resume(100).unwrap();
    }
    let record = recorder.finish();
    assert_eq!(record.status(), Ok(()));
    let child = record.segments().nth(1).unwrap();
    assert!(!child.denied_before_entry);
    assert!(
        child.recording.is_some(),
        "the child keeps the same execution observer"
    );
    assert!(record.replay().is_ok());
}

#[test]
fn a_denied_entry_is_complete_trap_evidence_without_a_fabricated_child_run() {
    let (parent, entry) = parent();
    let parent = Arc::new(parent);
    let (child, method) = child();
    let mut vm = Vm::new(
        NoHost,
        Limits {
            fuel: 3,
            ..Limits::default()
        },
    );
    let instance = vm.instantiate(&child).unwrap();
    let mut recorder = Recorder::new(parent.clone(), RecordLimits::default());
    {
        let mask = recorder.mask();
        let mut run = vm
            .start(
                &parent,
                entry,
                &[],
                InvocationOptions::default()
                    .yield_host_calls()
                    .trace(mask, &mut recorder),
            )
            .unwrap();
        run.resume(100).unwrap();
        let request = run.pending_host_call().unwrap();
        let argument = request.arguments().next().unwrap().to_value();
        assert!(
            matches!(request.call_guest(instance.function(method).expect("verified guest target"), vec![argument]), Err(ControlError::Trapped(error)) if matches!(error.trap, Trap::FuelExceeded))
        );
    }
    let record = recorder.finish();
    assert_eq!(record.status(), Ok(()));
    let child = record.segments().nth(1).unwrap();
    assert!(child.denied_before_entry && child.recording.is_none());
    assert!(
        matches!(record.replay().unwrap(), execution_tape::recording::ReplayOutcome::Trapped(error) if matches!(error.trap, Trap::FuelExceeded))
    );
}

#[derive(Default)]
struct Audit {
    runs: Vec<String>,
    scopes: Vec<(String, ScopeKind)>,
    transfers: Vec<(
        &'static str,
        execution_tape::vm::ActivationId,
        execution_tape::vm::ActivationId,
    )>,
    entered: Vec<String>,
}
impl TraceSink for Audit {
    fn run_start(&mut self, context: execution_tape::trace::TraceContext<'_>, _: FuncId, _: usize) {
        let code = context.program();
        let name = code.name().unwrap_or("unnamed").to_owned();
        self.entered.push(name.clone());
        self.runs.push(name);
    }
    fn run_end(&mut self, context: execution_tape::trace::TraceContext<'_>, _: TraceOutcome<'_>) {
        let code = context.program();
        assert_eq!(
            self.runs.pop().as_deref(),
            code.name(),
            "child runs close before their suspended parents"
        );
    }
    fn scope_enter(
        &mut self,
        context: execution_tape::trace::TraceContext<'_>,
        kind: ScopeKind,
        _: usize,
        _: FuncId,
        _: u32,
        _: Option<u64>,
    ) {
        let code = context.program();
        self.scopes
            .push((code.name().unwrap_or("unnamed").into(), kind));
    }
    fn scope_exit(
        &mut self,
        context: execution_tape::trace::TraceContext<'_>,
        kind: ScopeKind,
        _: usize,
        _: FuncId,
        _: u32,
        _: Option<u64>,
        _: TraceOutcome<'_>,
    ) {
        let code = context.program();
        assert_eq!(
            self.scopes.pop(),
            Some((code.name().unwrap_or("unnamed").into(), kind)),
            "profiling scopes retain their exact code owner across a switch"
        );
    }
    fn control(
        &mut self,
        _context: execution_tape::trace::TraceContext<'_>,
        event: ControlEvent<'_>,
    ) {
        let (kind, source, target) = match event {
            ControlEvent::EnterInstance { parent, child, .. } => ("enter", parent, child),
            ControlEvent::RejectInstance { parent, child, .. } => ("reject", parent, child),
            ControlEvent::ReturnInstance { child, parent, .. } => ("return", child, parent),
            ControlEvent::ReturnAcross { source, target, .. } => ("exit", source, target),
            ControlEvent::TailTransfer { source, target, .. } => ("tail", source, target),
        };
        self.transfers.push((kind, source.clone(), target.clone()));
    }
    fn instr_end(
        &mut self,
        _context: execution_tape::trace::TraceContext<'_>,
        _: TraceLocation,
        _: Option<TraceLocation>,
        _: TraceOutcome<'_>,
    ) {
    }
}

const OBSERVE: TraceMask = TraceMask::RUN;

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
        panic!("the embedding handles every boundary explicitly")
    }
}

fn parent() -> (VerifiedProgram, FuncId) {
    let mut builder = ProgramBuilder::new();
    builder.set_program_name("caller");
    let call = builder.host_sig_for(
        "method",
        HostSig {
            args: vec![ValueType::Agg],
            rets: vec![ValueType::Agg],
        },
    );
    let entry = builder
        .declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::Agg],
        })
        .unwrap();
    let mut body = builder.function_asm(entry).unwrap();
    let forty_two = body.alloc_register(ValueType::I64);
    let shared = body.alloc_register(ValueType::Agg);
    body.const_i64(forty_two, 42);
    body.tuple_new(shared, &[forty_two]);
    let [answer] = builder.emit_call(&mut body, call, &[shared]).unwrap();
    let result = body.alloc_register(ValueType::Agg);
    body.tuple_new(result, &[shared, answer]);
    body.ret(0, &[result]);
    builder.define_function(entry, body).unwrap();
    (builder.build_verified().unwrap(), entry)
}

fn child() -> (VerifiedProgram, FuncId) {
    let mut builder = ProgramBuilder::new();
    builder.set_program_name("independent method");
    let entry = builder
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::Agg],
            ret_types: vec![ValueType::Agg],
        })
        .unwrap();
    let mut body = builder.function_asm(entry).unwrap();
    let garbage = body.alloc_register(ValueType::Agg);
    body.tuple_new(garbage, &[]);
    body.ret(0, &[1]);
    builder.define_function(entry, body).unwrap();
    (builder.build_verified().unwrap(), entry)
}

fn signalling_child() -> (VerifiedProgram, FuncId) {
    let mut builder = ProgramBuilder::new();
    builder.set_program_name("condition signalling method");
    let signal = builder.host_sig_for(
        "signal",
        HostSig {
            args: vec![ValueType::Agg],
            rets: vec![ValueType::Agg],
        },
    );
    let entry = builder
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::Agg],
            ret_types: vec![ValueType::Agg],
        })
        .unwrap();
    let mut body = builder.function_asm(entry).unwrap();
    let [answer] = builder.emit_call(&mut body, signal, &[1]).unwrap();
    body.ret(0, &[answer]);
    builder.define_function(entry, body).unwrap();
    (builder.build_verified().unwrap(), entry)
}

#[test]
fn inspection_and_nonlocal_return_follow_the_actual_cross_instance_parent() {
    let (parent, entry) = parent();
    let (child, method) = signalling_child();
    let mut vm = Vm::new(NoHost, Limits::default());
    let instance = vm.instantiate(&child).unwrap();
    let mut run = vm
        .start(
            &parent,
            entry,
            &[],
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    run.resume(100).unwrap();
    let home = run.inspect().frames().next().unwrap().activation;
    let request = run.pending_host_call().unwrap();
    let original = request.arguments().next().unwrap().to_value();
    request
        .call_guest(
            instance.function(method).expect("verified guest target"),
            vec![original.clone()],
        )
        .unwrap();
    run.resume(100).unwrap();
    let mut view = run.inspect();
    let frames: Vec<_> = view.frames().collect();
    assert_eq!(frames.len(), 2, "inspection includes the suspended caller");
    assert!(
        frames[0].program.shares_definition_with(&child),
        "leaf locations use the child definition"
    );
    assert!(
        frames[1].program.shares_definition_with(&parent),
        "caller locations keep their own definition"
    );
    assert_eq!(
        frames[0].parent,
        Some(home.clone()),
        "the VM owns the cross-instance ancestry link"
    );
    assert_eq!(
        view.register(&home, 2).unwrap().value.unwrap().to_value(),
        original,
        "parent registers remain queryable under their original sources"
    );
    drop(view);
    run.pending_host_call()
        .unwrap()
        .return_from(&home, vec![original.clone()])
        .unwrap();
    assert_eq!(
        run.resume(100).unwrap(),
        RunStatus::Complete,
        "non-local return abandons the child and returns from the exact home"
    );
    assert_eq!(
        run.take_outputs().unwrap(),
        [original],
        "the root result is committed without a foreign handle import"
    );
    assert_eq!(
        run.inspect().frames().len(),
        0,
        "all crossed activations are retired"
    );
}

#[test]
fn inherited_observers_see_real_cross_code_enter_and_exit_without_fabricated_host_completion() {
    let (parent, entry) = parent();
    let (child, method) = signalling_child();
    let mut vm = Vm::new(NoHost, Limits::default());
    let instance = vm.instantiate(&child).unwrap();
    let audit = Rc::new(RefCell::new(Audit::default()));
    let mask = OBSERVE | TraceMask::CALL | TraceMask::HOST | TraceMask::INSTR | TraceMask::VALUES;
    {
        let mut run = vm
            .start(
                &parent,
                entry,
                &[],
                InvocationOptions::default()
                    .yield_host_calls()
                    .trace_owned(mask, audit.clone()),
            )
            .unwrap();
        run.resume(100).unwrap();
        let home = run.inspect().frames().next().unwrap().activation;
        let request = run.pending_host_call().unwrap();
        let argument = request.arguments().next().unwrap().to_value();
        request
            .call_guest(
                instance.function(method).expect("verified guest target"),
                vec![argument.clone()],
            )
            .unwrap();
        run.resume(100).unwrap();
        run.pending_host_call()
            .unwrap()
            .return_from(&home, vec![argument])
            .unwrap();
        assert_eq!(
            run.resume(100).unwrap(),
            RunStatus::Complete,
            "non-local transfer completed the root"
        );
    }
    let audit = audit.borrow();
    assert_eq!(
        audit.entered,
        ["caller", "condition signalling method"],
        "root observers inherit into independently compiled children"
    );
    assert!(
        audit.runs.is_empty() && audit.scopes.is_empty(),
        "every actual scope closes in nesting order"
    );
    assert_eq!(
        audit.transfers.len(),
        2,
        "one entry and one actual non-local return were observed"
    );
    assert_eq!(
        audit.transfers[0].1, audit.transfers[1].2,
        "the non-local target is the exact original caller"
    );
    assert_eq!(
        audit.transfers[0].2, audit.transfers[1].1,
        "the initiator is the exact independently compiled child"
    );
}

#[test]
fn nested_foreign_store_functions_are_rejected_before_entering_a_child() {
    let (parent, entry) = parent();
    let (child, method) = child();
    let mut vm = Vm::new(NoHost, Limits::default());
    let instance = vm.instantiate(&child).unwrap();
    let entry_value = Vm::new(NoHost, Limits::default())
        .function(&parent, entry)
        .unwrap();
    let mut run = vm
        .start(
            &parent,
            entry,
            &[],
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    run.resume(100).unwrap();
    let mut request = run.pending_host_call().unwrap();
    let tuple = request
        .context()
        .tuple_new(vec![Value::Func(entry_value)])
        .unwrap();
    let error = request
        .call_guest(
            instance.function(method).expect("verified guest target"),
            vec![Value::Agg(tuple)],
        )
        .unwrap_err();
    assert!(
        matches!(error, ControlError::Trapped(error) if matches!(error.trap, Trap::ForeignStoreValue { kind: ValueType::Func })),
        "provenance is checked recursively through aggregate members"
    );
    assert_eq!(
        run.aggregates().len_u32(),
        0,
        "a rejected transfer rolls back promoted staging and parent temporaries"
    );
}

#[test]
fn a_returned_child_home_cannot_retarget_the_next_child_with_the_same_local_ordinal() {
    let (parent, entry) = parent();
    let (child, method) = child();
    let (signal, next) = signalling_child();
    let mut vm = Vm::new(NoHost, Limits::default());
    let first = vm.instantiate(&child).unwrap();
    let second = vm.instantiate(&signal).unwrap();
    let mut run = vm
        .start(
            &parent,
            entry,
            &[],
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    run.resume(100).unwrap();
    let request = run.pending_host_call().unwrap();
    let argument = request.arguments().next().unwrap().to_value();
    request
        .call_guest(
            first.function(method).expect("verified guest target"),
            vec![argument.clone()],
        )
        .unwrap();
    let expired = run.inspect().frames().next().unwrap().activation;
    run.resume(100).unwrap();
    run.pending_host_call()
        .unwrap()
        .call_guest(
            second.function(next).expect("verified guest target"),
            vec![argument.clone()],
        )
        .unwrap();
    run.resume(100).unwrap();
    let current = run.inspect().frames().next().unwrap().activation;
    assert_eq!(
        current.ordinal(),
        expired.ordinal(),
        "display ordinals alone are deliberately insufficient"
    );
    assert_ne!(
        current.segment_ordinal(),
        expired.segment_ordinal(),
        "segment display identities are never reused within one execution"
    );
    let work = run.work();
    let error = run
        .pending_host_call()
        .unwrap()
        .return_from(&expired, vec![argument.clone()])
        .unwrap_err();
    assert!(
        matches!(error, ControlError::ExpiredActivation),
        "a retired target stays retired within the same execution"
    );
    assert_eq!(
        run.work(),
        work,
        "pure expired-target denial spends no work"
    );
    run.pending_host_call()
        .unwrap()
        .complete(&mut [argument], Ok(0))
        .unwrap();
    assert_eq!(
        run.resume(100).unwrap(),
        RunStatus::AwaitingHost,
        "denial leaves the live child and its caller usable"
    );
}

#[test]
fn independently_compiled_child_returns_the_same_aggregate_without_import_or_compaction() {
    let (parent, entry) = parent();
    let (child, method) = child();
    let mut vm = Vm::new(NoHost, Limits::default());
    let instance = vm.instantiate(&child).unwrap();
    let mut run = vm
        .start(
            &parent,
            entry,
            &[],
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    assert_eq!(
        run.resume(100).unwrap(),
        RunStatus::AwaitingHost,
        "caller reaches method lookup"
    );
    let request = run.pending_host_call().unwrap();
    let original = request.arguments().next().unwrap().to_value();
    request
        .call_guest(
            instance.function(method).expect("verified guest target"),
            vec![original.clone()],
        )
        .unwrap();
    assert_eq!(
        run.resume(100).unwrap(),
        RunStatus::AwaitingHost,
        "child returns to the original request"
    );
    assert!(
        run.result().is_none(),
        "a child return does not publish the root"
    );
    assert_eq!(
        run.aggregates().len_u32(),
        2,
        "child garbage is not compacted across live callers"
    );
    let mut request = run.pending_host_call().unwrap();
    let mut values = request.take_callback_results().unwrap();
    assert_eq!(
        values,
        [original],
        "transport retains the original heap identity"
    );
    request.complete(&mut values, Ok(0)).unwrap();
    assert_eq!(
        run.resume(100).unwrap(),
        RunStatus::Complete,
        "caller resumes and commits once"
    );
    let output = run.take_outputs().unwrap();
    let Value::Agg(pair) = output[0] else {
        panic!("pair result")
    };
    assert_eq!(
        run.aggregates().tuple_get(pair, 0).unwrap(),
        run.aggregates().tuple_get(pair, 1).unwrap(),
        "both fields retain sharing after the root remap"
    );
    assert_eq!(
        run.aggregates().len_u32(),
        2,
        "only the root commit discards child garbage"
    );
}

#[test]
fn suspended_parents_count_against_child_depth_and_failure_rolls_back_every_segment() {
    let (parent, entry) = parent();
    let (child, method) = child();
    let mut vm = Vm::new(
        NoHost,
        Limits {
            max_call_depth: 1,
            ..Limits::default()
        },
    );
    let instance = vm.instantiate(&child).unwrap();
    {
        let mut run = vm
            .start(
                &parent,
                entry,
                &[],
                InvocationOptions::default().yield_host_calls(),
            )
            .unwrap();
        run.resume(100).unwrap();
        let request = run.pending_host_call().unwrap();
        let argument = request.arguments().next().unwrap().to_value();
        let error = request
            .call_guest(
                instance.function(method).expect("verified guest target"),
                vec![argument],
            )
            .unwrap_err();
        assert!(
            matches!(error, ControlError::Trapped(error) if matches!(error.trap, Trap::CallDepthExceeded)),
            "child cannot reset the frame allowance"
        );
        assert_eq!(
            run.aggregates().len_u32(),
            0,
            "failure rolls back parent allocations too"
        );
        assert!(
            run.result().unwrap().is_err(),
            "the whole execution is terminal"
        );
    }
    assert!(
        vm.aggregates().len_u32() == 0,
        "dropping terminal control does not revive a parent"
    );
}

#[test]
fn dropping_an_active_child_cancels_the_whole_execution() {
    let (parent, entry) = parent();
    let (child, method) = child();
    let mut vm = Vm::new(NoHost, Limits::default());
    let instance = vm.instantiate(&child).unwrap();
    {
        let mut run = vm
            .start(
                &parent,
                entry,
                &[],
                InvocationOptions::default().yield_host_calls(),
            )
            .unwrap();
        run.resume(100).unwrap();
        let request = run.pending_host_call().unwrap();
        let argument = request.arguments().next().unwrap().to_value();
        request
            .call_guest(
                instance.function(method).expect("verified guest target"),
                vec![argument],
            )
            .unwrap();
        assert_eq!(
            run.resume(1).unwrap(),
            RunStatus::Suspended,
            "child is still active"
        );
    }
    assert!(
        vm.aggregates().len_u32() == 0,
        "the single transaction owns every new allocation"
    );
    let fresh = vm.aggregates_mut().tuple_new(vec![Value::I64(9)]).unwrap();
    assert_eq!(
        vm.run(&child, method, &[Value::Agg(fresh)], TraceMask::NONE, None)
            .unwrap(),
        [Value::Agg(fresh)],
        "the store remains usable after cancelling all control"
    );
}

#[test]
fn rejected_nonlocal_results_retain_both_the_initiating_code_and_contract_owner() {
    use execution_tape::{asm::FunctionAbi, contract::Contract};
    let mut builder = ProgramBuilder::new();
    builder.set_program_name("lexical home");
    let member = builder.contract(Contract::Kind(ValueType::I64)).unwrap();
    let tuple = builder.contract(Contract::Tuple(&[Some(member)])).unwrap();
    let entry = builder
        .declare_function(FunctionAbi {
            args: vec![],
            rets: vec![tuple.into()],
        })
        .unwrap();
    let pause = builder.host_sig_for(
        "method",
        HostSig {
            args: vec![],
            rets: vec![ValueType::Agg],
        },
    );
    let mut body = builder.function_asm(entry).unwrap();
    let [answer] = builder.emit_call(&mut body, pause, &[]).unwrap();
    body.ret(0, &[answer]);
    builder.define_function(entry, body).unwrap();
    let parent = Arc::new(builder.build_verified().unwrap());

    let mut builder = ProgramBuilder::new();
    builder.set_program_name("independent handler");
    // Deliberately overlap the home's contract IDs with incompatible child contracts.
    let member = builder.contract(Contract::Kind(ValueType::Bool)).unwrap();
    builder.contract(Contract::Tuple(&[Some(member)])).unwrap();
    let method = builder
        .declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![],
        })
        .unwrap();
    let pause = builder.host_sig_for(
        "escape",
        HostSig {
            args: vec![ValueType::Agg],
            rets: vec![],
        },
    );
    let mut body = builder.function_asm(method).unwrap();
    let boolean = body.alloc_register(ValueType::Bool);
    let aggregate = body.alloc_register(ValueType::Agg);
    body.const_bool(boolean, true)
        .tuple_new(aggregate, &[boolean]);
    let [] = builder.emit_call(&mut body, pause, &[aggregate]).unwrap();
    body.ret(0, &[]);
    builder.define_function(method, body).unwrap();
    let child = builder.build_verified().unwrap();
    let mut vm = Vm::new(NoHost, Limits::default());
    let instance = vm.instantiate(&child).unwrap();
    let mut recorder = Recorder::new(parent.clone(), RecordLimits::default());
    {
        let mask = recorder.mask();
        let mut run = vm
            .start(
                &parent,
                entry,
                &[],
                InvocationOptions::default()
                    .yield_host_calls()
                    .trace(mask, &mut recorder),
            )
            .unwrap();
        let home = run.inspect().frames().next().unwrap().activation;
        run.resume(100).unwrap();
        run.pending_host_call()
            .unwrap()
            .call_guest(
                instance.function(method).expect("verified guest target"),
                vec![],
            )
            .unwrap();
        run.resume(100).unwrap();
        let request = run.pending_host_call().unwrap();
        let site = request.location();
        let argument = request.arguments().next().unwrap().to_value();
        let ControlError::Trapped(trap) = request.return_from(&home, vec![argument]).unwrap_err()
        else {
            panic!("fatal boundary failure");
        };
        assert_eq!(
            (trap.func, trap.pc),
            (site.func, site.pc),
            "the error points at the initiating handler instruction"
        );
        let Trap::ContractBoundary(boundary) = trap.trap else {
            panic!("the result contract's owner must be retained");
        };
        assert!(boundary.program.shares_definition_with(&parent));
        assert!(
            matches!(
                boundary.error,
                execution_tape::vm::ContractBoundaryFailure::Mismatch(_)
            ) && boundary.function == entry
        );
        assert!(run.program().shares_definition_with(&child));
    }
    let record = recorder.finish();
    assert_eq!(record.status(), Ok(()));
    assert!(
        record
            .outcome_program()
            .unwrap()
            .shares_definition_with(&child)
    );
    assert!(
        matches!(
            record.replay().unwrap(),
            execution_tape::recording::ReplayOutcome::Trapped(_)
        ),
        "checked replay preserves the qualified contract failure"
    );
}

#[test]
fn cross_code_exit_to_a_local_callback_restores_its_original_host_request() {
    let mut builder = ProgramBuilder::new();
    let signature = FunctionSig {
        arg_types: vec![ValueType::Agg],
        ret_types: vec![ValueType::Agg],
    };
    let entry = builder.declare_function(signature.clone()).unwrap();
    let callback = builder.declare_function(signature).unwrap();
    for (function, symbol) in [(entry, "outer"), (callback, "inner")] {
        let pause = builder.host_sig_for(
            symbol,
            HostSig {
                args: vec![ValueType::Agg],
                rets: vec![ValueType::Agg],
            },
        );
        let mut body = builder.function_asm(function).unwrap();
        let [answer] = builder.emit_call(&mut body, pause, &[1]).unwrap();
        body.ret(0, &[answer]);
        builder.define_function(function, body).unwrap();
    }
    let parent = Arc::new(builder.build_verified().unwrap());
    let (child, method) = signalling_child();
    let mut vm = Vm::new(NoHost, Limits::default());
    let instance = vm.instantiate(&child).unwrap();
    let tuple = Value::Agg(vm.aggregates_mut().tuple_new(vec![Value::I64(42)]).unwrap());
    let mut recorder = Recorder::new(parent.clone(), RecordLimits::default())
        .with_instructions(InstructionCapture::default());
    {
        let mask = recorder.mask();
        let args = [tuple.clone()];
        let mut run = vm
            .start(
                &parent,
                entry,
                &args,
                InvocationOptions::default()
                    .yield_host_calls()
                    .trace(mask, &mut recorder),
            )
            .unwrap();
        run.resume(100).unwrap();
        {
            let request = run.pending_host_call().unwrap();
            let (callee, arguments) = (
                Value::Func(request.instance().function(callback).unwrap()),
                vec![tuple.clone()],
            );
            request.call_guest(callee, arguments)
        }
        .unwrap();
        run.resume(100).unwrap();
        let home = run.inspect().frames().next().unwrap().activation;
        assert_ne!(
            home.segment_ordinal(),
            0,
            "target the actual callback segment, not the original root"
        );
        run.pending_host_call()
            .unwrap()
            .call_guest(
                instance.function(method).expect("verified guest target"),
                vec![tuple.clone()],
            )
            .unwrap();
        run.resume(100).unwrap();
        assert_eq!(run.inspect().frames().count(), 3);
        run.pending_host_call()
            .unwrap()
            .return_from(&home, vec![tuple.clone()])
            .unwrap();
        let mut request = run.pending_host_call().unwrap();
        assert_eq!(
            request.symbol(),
            "outer",
            "the original operation survives the callback's exit"
        );
        let mut values = request.take_callback_results().unwrap();
        assert_eq!(values, [tuple]);
        request.complete(&mut values, Ok(0)).unwrap();
        assert_eq!(run.resume(100).unwrap(), RunStatus::Complete);
    }
    let record = recorder.finish();
    assert_eq!(record.status(), Ok(()));
    assert_eq!(
        record.instruction_status(),
        Some(InstructionStatus::Complete)
    );
    assert!(
        record.replay().is_ok(),
        "the journal composes same-code callbacks with cross-code exits"
    );
}
