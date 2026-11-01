// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Cross-code control uses one VM, heap transaction and resource authority.

use execution_tape::{
    asm::{FunctionSig, ProgramBuilder},
    host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef},
    program::ValueType,
    trace::{ControlEvent, ScopeKind, TraceLocation, TraceMask, TraceOutcome, TraceSink},
    value::{FuncId, Value},
    verifier::VerifiedProgram,
    vm::{ControlError, InvocationOptions, Limits, RunStatus, Trap, Vm},
};
use std::{cell::RefCell, rc::Rc};

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
    fn run_start(&mut self, code: &execution_tape::program::Program, _: FuncId, _: usize) {
        let name = code.name().unwrap_or("unnamed").to_owned();
        self.entered.push(name.clone());
        self.runs.push(name);
    }
    fn run_end(&mut self, code: &execution_tape::program::Program, _: TraceOutcome<'_>) {
        assert_eq!(
            self.runs.pop().as_deref(),
            code.name(),
            "child runs close before their suspended parents"
        );
    }
    fn scope_enter(
        &mut self,
        code: &execution_tape::program::Program,
        kind: ScopeKind,
        _: usize,
        _: FuncId,
        _: u32,
        _: Option<u64>,
    ) {
        self.scopes
            .push((code.name().unwrap_or("unnamed").into(), kind));
    }
    fn scope_exit(
        &mut self,
        code: &execution_tape::program::Program,
        kind: ScopeKind,
        _: usize,
        _: FuncId,
        _: u32,
        _: Option<u64>,
        _: TraceOutcome<'_>,
    ) {
        assert_eq!(
            self.scopes.pop(),
            Some((code.name().unwrap_or("unnamed").into(), kind)),
            "profiling scopes retain their exact code owner across a switch"
        );
    }
    fn control(&mut self, _: &execution_tape::program::Program, event: ControlEvent<'_>) {
        let (kind, source, target) = match event {
            ControlEvent::EnterInstance { parent, child, .. } => ("enter", parent, child),
            ControlEvent::ReturnInstance { child, parent, .. } => ("return", child, parent),
            ControlEvent::ReturnAcross { source, target, .. } => ("exit", source, target),
        };
        self.transfers.push((kind, source.clone(), target.clone()));
    }
    fn instr_end(
        &mut self,
        _: &execution_tape::program::Program,
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
        .call_instance(
            &instance,
            method,
            vec![original.clone()],
            InvocationOptions::default().yield_host_calls(),
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
            .call_instance(
                &instance,
                method,
                vec![argument.clone()],
                InvocationOptions::default().yield_host_calls(),
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
fn nested_raw_function_ids_are_rejected_instead_of_reinterpreted_in_the_child() {
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
    run.resume(100).unwrap();
    let mut request = run.pending_host_call().unwrap();
    let tuple = request
        .context()
        .tuple_new(vec![Value::Func(entry)])
        .unwrap();
    let error = request
        .call_instance(
            &instance,
            method,
            vec![Value::Agg(tuple)],
            InvocationOptions::default(),
        )
        .unwrap_err();
    assert!(
        matches!(error, ControlError::Trapped(error) if matches!(error.trap, Trap::CrossInstanceValue { kind: ValueType::Func })),
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
        .call_instance(
            &first,
            method,
            vec![argument.clone()],
            InvocationOptions::default(),
        )
        .unwrap();
    let expired = run.inspect().frames().next().unwrap().activation;
    run.resume(100).unwrap();
    run.pending_host_call()
        .unwrap()
        .call_instance(
            &second,
            next,
            vec![argument.clone()],
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    run.resume(100).unwrap();
    let current = run.inspect().frames().next().unwrap().activation;
    assert_eq!(
        current.ordinal(),
        expired.ordinal(),
        "display ordinals alone are deliberately insufficient"
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
        .call_instance(
            &instance,
            method,
            vec![original.clone()],
            InvocationOptions::default().yield_host_calls(),
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
            .call_instance(
                &instance,
                method,
                vec![argument],
                InvocationOptions::default(),
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
            .call_instance(
                &instance,
                method,
                vec![argument],
                InvocationOptions::default(),
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
