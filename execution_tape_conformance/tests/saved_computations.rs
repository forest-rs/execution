// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A VM keeps saved computations between invocations and resumes each once, with a fresh
//! allowance.

use execution_tape::{
    aggregates::{AggError, AggHeap, AggShape},
    asm::{FunctionSig, ProgramBuilder, Reg},
    host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef},
    program::LinkId,
    program::{Pc, Program, SpanId, StructTypeDef, ValueType},
    recording::{CaptureFault, RecordLimits, Recorder, ReplayOutcome},
    trace::{Attachment, ScopeKind, TraceLocation, TraceMask, TraceOutcome, TraceSink},
    value::{AggHandle, AggType, FuncId, FuncRef, Value},
    verifier::VerifiedProgram,
    vm::{
        CollectError, ComputationId, InvocationOptions, Limits, RequestError, ResumeError,
        RunStatus, Trap, Vm,
    },
};
use std::sync::Arc;

/// Answers `ask` with 5 and `make` with `(222,)`, for runs that do not yield.
struct Answers;
impl Host for Answers {
    fn call(
        &mut self,
        symbol: &str,
        _: SigHash,
        _: &[ValueRef<'_>],
        rets: &mut [Value],
        mut context: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        rets[0] = match symbol {
            "make" => Value::Agg(context.tuple_new(vec![Value::I64(222)])?),
            _ => Value::I64(5),
        };
        Ok(0)
    }
}

fn sig(args: &[ValueType], rets: &[ValueType]) -> FunctionSig {
    FunctionSig {
        arg_types: args.to_vec(),
        ret_types: rets.to_vec(),
    }
}

/// `entry(text)` boxes `text`, asks the host for a number, and returns `((text,), number)`.
fn asks() -> (Arc<VerifiedProgram>, FuncId) {
    let mut b = ProgramBuilder::new();
    let ask = b.host_sig_for(
        "ask",
        HostSig {
            args: vec![],
            rets: vec![ValueType::I64],
        },
    );
    let entry = b
        .declare_function(sig(&[ValueType::Str], &[ValueType::Agg]))
        .unwrap();
    let mut a = b.function_asm(entry).unwrap();
    let (boxed, number, result) = (
        a.alloc_register(ValueType::Agg),
        a.alloc_register(ValueType::I64),
        a.alloc_register(ValueType::Agg),
    );
    a.tuple_new(boxed, &[Reg(1)])
        .host_call_sig_id(Reg::EFFECT, ask, Reg::EFFECT, &[], &[number])
        .tuple_new(result, &[boxed, number])
        .ret(Reg::EFFECT, &[result]);
    b.define_function(entry, a).unwrap();
    (Arc::new(b.build_verified().unwrap()), entry)
}

/// Starts `asks` and saves it waiting at its host request.
fn save_asking(vm: &mut Vm<Answers>, entry: FuncRef, text: &str) -> ComputationId {
    let args = [Value::Str(text.into())];
    let mut invocation = vm
        .start(
            entry,
            &args,
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::AwaitingHost,
        "it waits at `ask`"
    );
    invocation.save().unwrap()
}

/// Resumes a saved `asks`, answers its request with `answer` and returns its text and number.
fn finish_asking(vm: &mut Vm<Answers>, id: ComputationId, answer: i64) -> (String, i64) {
    let mut invocation = vm
        .resume(id, InvocationOptions::default().yield_host_calls())
        .unwrap();
    assert_eq!(
        invocation.resume(0).unwrap(),
        RunStatus::AwaitingHost,
        "the resumed computation still waits at `ask`"
    );
    invocation
        .pending_host_call()
        .unwrap()
        .complete(&mut [Value::I64(answer)], Ok(0))
        .unwrap();
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::Complete,
        "answered, it returns"
    );
    let [Value::Agg(result)] = invocation.take_outputs().unwrap()[..] else {
        panic!("one tuple");
    };
    drop(invocation);
    let heap = vm.aggregates();
    let Value::Agg(boxed) = heap.tuple_get(result, 0).unwrap() else {
        panic!("a boxed string");
    };
    let Value::Str(text) = heap.tuple_get(boxed, 0).unwrap() else {
        panic!("a string");
    };
    let Value::I64(number) = heap.tuple_get(result, 1).unwrap() else {
        panic!("a number");
    };
    (text, number)
}

#[test]
fn a_saved_request_outlives_other_runs_and_a_moving_collection() {
    let (program, entry) = asks();
    let mut vm = Vm::new(Answers, Limits::default());
    let entry = FuncRef::new(vm.install(program).unwrap(), entry);
    // Garbage below the saved computation's own aggregates, so collection moves them.
    vm.aggregates_mut().tuple_new(vec![]).unwrap();
    let id = save_asking(&mut vm, entry, "kept");
    assert_eq!(vm.heap_stats().saved_computations, 1);
    assert!(vm.heap_stats().saved_bytes > 0);

    // Other runs share the heap meanwhile.
    for _ in 0..3 {
        vm.run(entry, &[Value::Str("other".into())], TraceMask::NONE, None)
            .unwrap();
    }
    let report = vm.collect().unwrap();
    assert!(report.moved > 0, "{report:?}");
    assert_eq!(report.aggregates.live, 1, "only the saved box survives");

    assert_eq!(finish_asking(&mut vm, id, 9), ("kept".into(), 9));
    assert_eq!(vm.heap_stats().saved_computations, 0);
    assert_eq!(vm.heap_stats().saved_bytes, 0);
}

#[test]
fn saved_computations_resume_independently_and_each_id_once() {
    let (program, entry) = asks();
    let mut vm = Vm::new(Answers, Limits::default());
    let entry = FuncRef::new(vm.install(program).unwrap(), entry);
    let first = save_asking(&mut vm, entry, "first");
    let second = save_asking(&mut vm, entry, "second");
    assert_ne!(first, second);

    assert_eq!(finish_asking(&mut vm, second, 2), ("second".into(), 2));
    assert!(matches!(
        vm.resume(second, InvocationOptions::default()),
        Err(ResumeError::Unknown)
    ));
    // A later computation may reuse the slot; the old ID still names nothing.
    let third = save_asking(&mut vm, entry, "third");
    assert!(matches!(
        vm.resume(second, InvocationOptions::default()),
        Err(ResumeError::Unknown)
    ));
    assert_eq!(finish_asking(&mut vm, first, 1), ("first".into(), 1));
    assert_eq!(finish_asking(&mut vm, third, 3), ("third".into(), 3));
}

#[test]
fn dropping_a_resumed_invocation_cancels_it_and_keeps_its_older_sibling() {
    let (program, entry) = asks();
    let mut vm = Vm::new(Answers, Limits::default());
    let entry = FuncRef::new(vm.install(program).unwrap(), entry);
    let older = save_asking(&mut vm, entry, "older");
    let doomed = save_asking(&mut vm, entry, "doomed");
    let before = vm.aggregates().len_u32();
    {
        let mut invocation = vm
            .resume(doomed, InvocationOptions::default().yield_host_calls())
            .unwrap();
        invocation
            .pending_host_call()
            .unwrap()
            .complete(&mut [Value::I64(0)], Ok(0))
            .unwrap();
        // The result tuple is allocated before the invocation is dropped unfinished.
        assert_eq!(invocation.resume(1).unwrap(), RunStatus::Suspended);
        assert_eq!(invocation.aggregates().len_u32(), before + 1);
    }
    assert_eq!(
        vm.aggregates().len_u32(),
        before,
        "only its own lease rolled back"
    );
    assert!(matches!(
        vm.resume(doomed, InvocationOptions::default()),
        Err(ResumeError::Unknown)
    ));
    assert_eq!(finish_asking(&mut vm, older, 4), ("older".into(), 4));
}

#[test]
fn a_forgotten_resumed_invocation_never_frees_its_id() {
    let (program, entry) = asks();
    let mut vm = Vm::new(Answers, Limits::default());
    let entry = FuncRef::new(vm.install(program).unwrap(), entry);
    let lost = save_asking(&mut vm, entry, "lost");
    core::mem::forget(
        vm.resume(lost, InvocationOptions::default().yield_host_calls())
            .unwrap(),
    );
    let next = save_asking(&mut vm, entry, "next");
    assert!(matches!(
        vm.resume(lost, InvocationOptions::default()),
        Err(ResumeError::Unknown)
    ));
    assert_eq!(finish_asking(&mut vm, next, 6), ("next".into(), 6));
}

/// `entry()` adds one twenty times and returns the sum: 22 instructions.
fn counts() -> (Arc<VerifiedProgram>, FuncId) {
    let mut b = ProgramBuilder::new();
    let entry = b.declare_function(sig(&[], &[ValueType::I64])).unwrap();
    let mut a = b.function_asm(entry).unwrap();
    let (sum, one) = (
        a.alloc_register(ValueType::I64),
        a.alloc_register(ValueType::I64),
    );
    a.const_i64(one, 1).const_i64(sum, 0);
    for _ in 0..20 {
        a.i64_add(sum, sum, one);
    }
    a.ret(Reg::EFFECT, &[sum]);
    b.define_function(entry, a).unwrap();
    (Arc::new(b.build_verified().unwrap()), entry)
}

#[test]
fn each_resumed_invocation_starts_a_fresh_allowance() {
    let (program, entry) = counts();
    let limits = Limits {
        fuel: 12,
        ..Limits::default()
    };
    let mut vm = Vm::new(Answers, limits);
    let entry = FuncRef::new(vm.install(program).unwrap(), entry);
    let error = vm.run(entry, &[], TraceMask::NONE, None).unwrap_err();
    assert!(matches!(error.trap, Trap::FuelExceeded), "{error:?}");

    let mut invocation = vm.start(entry, &[], InvocationOptions::default()).unwrap();
    assert_eq!(invocation.resume(11).unwrap(), RunStatus::Suspended);
    let id = invocation.save().unwrap();
    let mut invocation = vm.resume(id, InvocationOptions::default()).unwrap();
    assert_eq!(invocation.remaining_fuel(), 12);
    assert_eq!(invocation.resume(u64::MAX).unwrap(), RunStatus::Complete);
    assert_eq!(invocation.result().unwrap().unwrap(), [Value::I64(20)]);
}

/// `entry()` calls `mid`, which asks the host and then calls `leaf`.
fn nested() -> (Arc<VerifiedProgram>, FuncId) {
    let mut b = ProgramBuilder::new();
    let ask = b.host_sig_for(
        "ask",
        HostSig {
            args: vec![],
            rets: vec![ValueType::I64],
        },
    );
    let entry = b.declare_function(sig(&[], &[ValueType::I64])).unwrap();
    let mid = b.declare_function(sig(&[], &[ValueType::I64])).unwrap();
    let leaf = b.declare_function(sig(&[], &[ValueType::I64])).unwrap();
    let mut a = b.function_asm(leaf).unwrap();
    let n = a.alloc_register(ValueType::I64);
    a.const_i64(n, 1).ret(Reg::EFFECT, &[n]);
    b.define_function(leaf, a).unwrap();
    let mut a = b.function_asm(mid).unwrap();
    let n = a.alloc_register(ValueType::I64);
    a.host_call_sig_id(Reg::EFFECT, ask, Reg::EFFECT, &[], &[n])
        .call(Reg::EFFECT, leaf, Reg::EFFECT, &[], &[n])
        .ret(Reg::EFFECT, &[n]);
    b.define_function(mid, a).unwrap();
    let mut a = b.function_asm(entry).unwrap();
    let n = a.alloc_register(ValueType::I64);
    a.call(Reg::EFFECT, mid, Reg::EFFECT, &[], &[n])
        .ret(Reg::EFFECT, &[n]);
    b.define_function(entry, a).unwrap();
    (Arc::new(b.build_verified().unwrap()), entry)
}

#[test]
fn a_resumed_computation_keeps_its_frames_and_activation_ids() {
    let (program, entry) = nested();
    let limits = Limits {
        max_call_depth: 2,
        ..Limits::default()
    };
    let mut vm = Vm::new(Answers, limits);
    let entry = FuncRef::new(vm.install(program).unwrap(), entry);
    let mut invocation = vm
        .start(entry, &[], InvocationOptions::default().yield_host_calls())
        .unwrap();
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::AwaitingHost
    );
    let before: Vec<_> = invocation
        .inspect()
        .frames()
        .map(|frame| frame.activation)
        .collect();
    let id = invocation.save().unwrap();

    let mut invocation = vm
        .resume(id, InvocationOptions::default().yield_host_calls())
        .unwrap();
    let after: Vec<_> = invocation
        .inspect()
        .frames()
        .map(|frame| frame.activation)
        .collect();
    assert_eq!(before, after);
    invocation
        .pending_host_call()
        .unwrap()
        .complete(&mut [Value::I64(0)], Ok(0))
        .unwrap();
    // The two held frames still count: `leaf` would be the third.
    let error = invocation.resume(u64::MAX).unwrap_err();
    assert!(matches!(error.trap, Trap::CallDepthExceeded), "{error:?}");
}

/// Checks that every exit closes the innermost thing open, whether it was entered or attached.
#[derive(Default)]
struct Balanced {
    /// Open scopes with where they were entered and that place's span.
    open: Vec<(ScopeKind, usize, TraceLocation, Option<SpanId>)>,
    attempt: Option<TraceLocation>,
    events: Vec<&'static str>,
    attached: Vec<(ScopeKind, usize, TraceLocation, Option<SpanId>)>,
}
impl TraceSink for Balanced {
    fn run_start(&mut self, _: &Program, _: FuncRef, _: usize) {
        self.events.push("start");
    }
    fn attach(&mut self, _: &Program, attachment: Attachment<'_>) {
        self.events.push("attach");
        assert!(
            self.open.is_empty() && self.attempt.is_none(),
            "a fresh observer"
        );
        self.open = attachment
            .scopes
            .iter()
            .map(|scope| (scope.kind, scope.depth, scope.location, scope.span_id))
            .collect();
        self.attached.clone_from(&self.open);
        self.attempt = attachment.attempt;
    }
    fn instr(&mut self, _: &Program, location: TraceLocation, _: Pc, _: Option<SpanId>, _: u16) {
        assert!(self.attempt.is_none(), "one attempt at a time");
        self.attempt = Some(location);
    }
    fn instr_end(
        &mut self,
        _: &Program,
        location: TraceLocation,
        _: Option<TraceLocation>,
        _: TraceOutcome<'_>,
    ) {
        assert_eq!(self.attempt.take(), Some(location), "ends the open attempt");
    }
    fn scope_enter(
        &mut self,
        _: &Program,
        kind: ScopeKind,
        depth: usize,
        location: TraceLocation,
        span: Option<SpanId>,
    ) {
        self.open.push((kind, depth, location, span));
    }
    fn scope_exit(
        &mut self,
        _: &Program,
        kind: ScopeKind,
        depth: usize,
        _: TraceLocation,
        _: Option<SpanId>,
        _: TraceOutcome<'_>,
    ) {
        assert_eq!(
            self.open.pop().map(|(kind, depth, ..)| (kind, depth)),
            Some((kind, depth)),
            "exits the innermost scope"
        );
    }
    fn release(&mut self, _: &Program) {
        self.events.push("release");
    }
    fn run_end(&mut self, _: &Program, outcome: TraceOutcome<'_>) {
        assert!(matches!(outcome, TraceOutcome::Ok), "{outcome:?}");
        assert!(self.open.is_empty(), "every scope closed: {:?}", self.open);
        assert!(self.attempt.is_none(), "every attempt ended");
        self.events.push("end");
    }
}

#[test]
fn observers_hand_over_a_waiting_computation_without_inventing_events() {
    let (program, entry) = nested();
    let mut vm = Vm::new(Answers, Limits::default());
    let entry = FuncRef::new(vm.install(program).unwrap(), entry);
    let mask = TraceMask::RUN | TraceMask::CALL | TraceMask::HOST | TraceMask::INSTR;

    let mut first = Balanced::default();
    let mut invocation = vm
        .start(
            entry,
            &[],
            InvocationOptions::default()
                .yield_host_calls()
                .trace(mask, &mut first),
        )
        .unwrap();
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::AwaitingHost
    );
    let id = invocation.save().unwrap();
    assert_eq!(first.events, ["start", "release"]);
    assert_eq!(
        first.open.len(),
        3,
        "two frames and the host call stay open"
    );

    let mut second = Balanced::default();
    let mut invocation = vm
        .resume(
            id,
            InvocationOptions::default()
                .yield_host_calls()
                .trace(mask, &mut second),
        )
        .unwrap();
    invocation
        .pending_host_call()
        .unwrap()
        .complete(&mut [Value::I64(0)], Ok(0))
        .unwrap();
    assert_eq!(invocation.resume(u64::MAX).unwrap(), RunStatus::Complete);
    drop(invocation);
    assert_eq!(second.events, ["attach", "end"]);
    assert_eq!(
        second.attached, first.open,
        // Including where each scope was entered and its span, as `scope_enter` reported them.
        "the same scopes, at the same depths"
    );
}

#[test]
fn a_computation_saved_between_quanta_hands_over_its_frames() {
    let (program, entry) = nested();
    let mut vm = Vm::new(Answers, Limits::default());
    let entry = FuncRef::new(vm.install(program).unwrap(), entry);
    let mask = TraceMask::RUN | TraceMask::CALL | TraceMask::INSTR;
    let mut invocation = vm.start(entry, &[], InvocationOptions::default()).unwrap();
    // Inside `mid`, before its host call.
    assert_eq!(invocation.resume(1).unwrap(), RunStatus::Suspended);
    let id = invocation.save().unwrap();

    let mut observer = Balanced::default();
    let mut invocation = vm
        .resume(id, InvocationOptions::default().trace(mask, &mut observer))
        .unwrap();
    assert_eq!(invocation.resume(u64::MAX).unwrap(), RunStatus::Complete);
    drop(invocation);
    assert_eq!(observer.events, ["attach", "end"]);
    assert_eq!(observer.attached.len(), 2, "entry and mid");
}

#[test]
fn a_recording_follows_its_computation_across_invocations_and_replays() {
    let (program, entry) = asks();
    let mut vm = Vm::new(Answers, Limits::default());
    let entry = FuncRef::new(vm.install(Arc::clone(&program)).unwrap(), entry);
    let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
    let mask = recorder.mask();
    let args = [Value::Str("text".into())];
    let mut invocation = vm
        .start(
            entry,
            &args,
            InvocationOptions::default()
                .yield_host_calls()
                .trace(mask, &mut recorder),
        )
        .unwrap();
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::AwaitingHost
    );
    let id = invocation.save().unwrap();

    let mut invocation = vm
        .resume(
            id,
            InvocationOptions::default()
                .yield_host_calls()
                .trace(mask, &mut recorder),
        )
        .unwrap();
    invocation
        .pending_host_call()
        .unwrap()
        .complete(&mut [Value::I64(7)], Ok(0))
        .unwrap();
    assert_eq!(invocation.resume(u64::MAX).unwrap(), RunStatus::Complete);
    drop(invocation);
    let recording = recorder.finish();
    assert_eq!(recording.status(), Ok(()));
    assert!(matches!(
        recording.replay().unwrap(),
        ReplayOutcome::Returned(_)
    ));
}

#[test]
fn replay_gives_each_invocation_the_allowance_the_original_had() {
    let (program, entry) = counts();
    // Too little fuel for the whole computation in one invocation.
    let limits = Limits {
        fuel: 12,
        ..Limits::default()
    };
    let mut vm = Vm::new(Answers, limits);
    let entry = FuncRef::new(vm.install(Arc::clone(&program)).unwrap(), entry);
    let mut recorder = Recorder::new(program, RecordLimits::default());
    let mask = recorder.mask();
    let mut invocation = vm
        .start(
            entry,
            &[],
            InvocationOptions::default().trace(mask, &mut recorder),
        )
        .unwrap();
    assert_eq!(invocation.resume(11).unwrap(), RunStatus::Suspended);
    let id = invocation.save().unwrap();
    let mut invocation = vm
        .resume(id, InvocationOptions::default().trace(mask, &mut recorder))
        .unwrap();
    assert_eq!(invocation.resume(u64::MAX).unwrap(), RunStatus::Complete);
    drop(invocation);
    let recording = recorder.finish();
    assert_eq!(recording.status(), Ok(()));
    let ReplayOutcome::Returned(values) = recording.replay().unwrap() else {
        panic!("replay ran out of fuel where the original did not");
    };
    assert_eq!(values.values(), [Value::I64(20)]);
}

#[test]
fn installs_and_relinks_while_saved_are_replayed_before_the_next_invocation() {
    let (program, entry) = asks_then_links(2);
    let mut vm = Vm::new(Answers, Limits::default());
    let unit = vm.install(Arc::clone(&program)).unwrap();
    let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
    let mask = recorder.mask();
    let args = [Value::I64(3)];
    let mut invocation = vm
        .start(
            FuncRef::new(unit, entry),
            &args,
            InvocationOptions::default()
                .yield_host_calls()
                .trace(mask, &mut recorder),
        )
        .unwrap();
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::AwaitingHost
    );
    let id = invocation.save().unwrap();
    // While it is saved: install a replacement and point the link at it.
    let (replacement, _) = asks_then_links(10);
    let replacement = vm.install(replacement).unwrap();
    // `scale` is each program's first function.
    vm.relink(unit, LinkId(0), FuncRef::new(replacement, FuncId(0)))
        .unwrap();

    let mut invocation = vm
        .resume(
            id,
            InvocationOptions::default()
                .yield_host_calls()
                .trace(mask, &mut recorder),
        )
        .unwrap();
    invocation
        .pending_host_call()
        .unwrap()
        .complete(&mut [Value::I64(0)], Ok(0))
        .unwrap();
    assert_eq!(invocation.resume(u64::MAX).unwrap(), RunStatus::Complete);
    assert_eq!(invocation.result().unwrap().unwrap(), [Value::I64(30)]);
    drop(invocation);
    let recording = recorder.finish();
    assert_eq!(recording.status(), Ok(()));
    let ReplayOutcome::Returned(values) = recording.replay().unwrap() else {
        panic!("replay trapped");
    };
    assert_eq!(values.values(), [Value::I64(30)]);
}

#[test]
fn a_recording_refuses_a_heap_changed_while_its_computation_was_saved() {
    let (program, entry) = asks();
    let mut vm = Vm::new(Answers, Limits::default());
    let entry = FuncRef::new(vm.install(Arc::clone(&program)).unwrap(), entry);
    let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
    let mask = recorder.mask();
    let args = [Value::Str("text".into())];
    let mut invocation = vm
        .start(
            entry,
            &args,
            InvocationOptions::default()
                .yield_host_calls()
                .trace(mask, &mut recorder),
        )
        .unwrap();
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::AwaitingHost
    );
    let id = invocation.save().unwrap();
    // Another run allocates meanwhile.
    vm.run(entry, &args, TraceMask::NONE, None).unwrap();
    let invocation = vm
        .resume(
            id,
            InvocationOptions::default()
                .yield_host_calls()
                .trace(mask, &mut recorder),
        )
        .unwrap();
    drop(invocation);
    assert_eq!(recorder.finish().status(), Err(&CaptureFault::Interleaved));
}

#[test]
fn a_recorder_cannot_join_a_computation_it_did_not_see_start() {
    let (program, entry) = asks();
    let mut vm = Vm::new(Answers, Limits::default());
    let entry = FuncRef::new(vm.install(Arc::clone(&program)).unwrap(), entry);
    let id = save_asking(&mut vm, entry, "text");
    let mut recorder = Recorder::new(program, RecordLimits::default());
    let mask = recorder.mask();
    let invocation = vm
        .resume(
            id,
            InvocationOptions::default()
                .yield_host_calls()
                .trace(mask, &mut recorder),
        )
        .unwrap();
    drop(invocation);
    assert_eq!(recorder.finish().status(), Err(&CaptureFault::MissingEntry));
}

/// `scale(x) = x * factor` and `entry(x)`, which asks the host, then calls `scale` through link
/// `"scale"`.
fn asks_then_links(factor: i64) -> (Arc<VerifiedProgram>, FuncId) {
    let mut b = ProgramBuilder::new();
    let ask = b.host_sig_for(
        "ask",
        HostSig {
            args: vec![],
            rets: vec![ValueType::I64],
        },
    );
    let scale = b
        .declare_function(sig(&[ValueType::I64], &[ValueType::I64]))
        .unwrap();
    let mut a = b.function_asm(scale).unwrap();
    let (k, y) = (
        a.alloc_register(ValueType::I64),
        a.alloc_register(ValueType::I64),
    );
    a.const_i64(k, factor)
        .i64_mul(y, Reg(1), k)
        .ret(Reg::EFFECT, &[y]);
    b.define_function(scale, a).unwrap();
    let link = b.declare_link("scale", scale).unwrap();
    let entry = b
        .declare_function(sig(&[ValueType::I64], &[ValueType::I64]))
        .unwrap();
    let mut a = b.function_asm(entry).unwrap();
    let (n, y) = (
        a.alloc_register(ValueType::I64),
        a.alloc_register(ValueType::I64),
    );
    a.host_call_sig_id(Reg::EFFECT, ask, Reg::EFFECT, &[], &[n])
        .call_link(Reg::EFFECT, link, Reg::EFFECT, &[Reg(1)], &[y])
        .ret(Reg::EFFECT, &[y]);
    b.define_function(entry, a).unwrap();
    (Arc::new(b.build_verified().unwrap()), entry)
}

/// `entry()` asks the host; `spin()` adds twice and returns, for the host to call back.
fn calls_back() -> (Arc<VerifiedProgram>, FuncId, FuncId) {
    let mut b = ProgramBuilder::new();
    let ask = b.host_sig_for(
        "ask",
        HostSig {
            args: vec![],
            rets: vec![ValueType::I64],
        },
    );
    let entry = b.declare_function(sig(&[], &[ValueType::I64])).unwrap();
    let spin = b.declare_function(sig(&[], &[ValueType::I64])).unwrap();
    let mut a = b.function_asm(spin).unwrap();
    let n = a.alloc_register(ValueType::I64);
    a.const_i64(n, 1).i64_add(n, n, n).i64_add(n, n, n);
    a.ret(Reg::EFFECT, &[n]);
    b.define_function(spin, a).unwrap();
    let mut a = b.function_asm(entry).unwrap();
    let n = a.alloc_register(ValueType::I64);
    a.host_call_sig_id(Reg::EFFECT, ask, Reg::EFFECT, &[], &[n])
        .ret(Reg::EFFECT, &[n]);
    b.define_function(entry, a).unwrap();
    (Arc::new(b.build_verified().unwrap()), entry, spin)
}

#[test]
fn a_computation_running_a_callback_resumes_only_unobserved() {
    let (program, entry, spin) = calls_back();
    let mut vm = Vm::new(Answers, Limits::default());
    let unit = vm.install(program).unwrap();
    let mut invocation = vm
        .start(
            FuncRef::new(unit, entry),
            &[],
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::AwaitingHost
    );
    invocation
        .pending_host_call()
        .unwrap()
        .call_guest(Value::Func(FuncRef::new(unit, spin)), vec![])
        .unwrap();
    assert_eq!(invocation.resume(1).unwrap(), RunStatus::Suspended);
    let id = invocation.save().unwrap();

    let mut observer = Balanced::default();
    assert!(matches!(
        vm.resume(
            id,
            InvocationOptions::default().trace(TraceMask::CALL, &mut observer)
        ),
        Err(ResumeError::Observed)
    ));
    let mut invocation = vm
        .resume(id, InvocationOptions::default().yield_host_calls())
        .unwrap();
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::CallbackReturned
    );
    let mut request = invocation.pending_host_call().unwrap();
    assert_eq!(request.take_callback_results().unwrap(), [Value::I64(4)]);
    request.complete(&mut [Value::I64(8)], Ok(0)).unwrap();
    assert_eq!(invocation.resume(u64::MAX).unwrap(), RunStatus::Complete);
    assert_eq!(invocation.result().unwrap().unwrap(), [Value::I64(8)]);
}

#[test]
fn a_finished_invocation_is_not_saved() {
    let (program, entry) = asks();
    let mut vm = Vm::new(Answers, Limits::default());
    let entry = FuncRef::new(vm.install(program).unwrap(), entry);
    let args = [Value::Str("text".into())];

    // Finished: its result is taken instead.
    let mut invocation = vm
        .start(entry, &args, InvocationOptions::default())
        .unwrap();
    assert_eq!(invocation.resume(u64::MAX).unwrap(), RunStatus::Complete);
    let invocation = invocation.save().unwrap_err();
    assert!(invocation.result().unwrap().is_ok());
    drop(invocation);
}

/// `entry()` asks the host to `make` a tuple and returns it; `spin()` returns 4, for the host
/// to call back.
fn makes() -> (Arc<VerifiedProgram>, FuncId, FuncId) {
    let mut b = ProgramBuilder::new();
    let make = b.host_sig_for(
        "make",
        HostSig {
            args: vec![],
            rets: vec![ValueType::Agg],
        },
    );
    let entry = b.declare_function(sig(&[], &[ValueType::Agg])).unwrap();
    let spin = b.declare_function(sig(&[], &[ValueType::I64])).unwrap();
    let mut a = b.function_asm(spin).unwrap();
    let n = a.alloc_register(ValueType::I64);
    a.const_i64(n, 1).i64_add(n, n, n).i64_add(n, n, n);
    a.ret(Reg::EFFECT, &[n]);
    b.define_function(spin, a).unwrap();
    let mut a = b.function_asm(entry).unwrap();
    let made = a.alloc_register(ValueType::Agg);
    a.host_call_sig_id(Reg::EFFECT, make, Reg::EFFECT, &[], &[made])
        .ret(Reg::EFFECT, &[made]);
    b.define_function(entry, a).unwrap();
    (Arc::new(b.build_verified().unwrap()), entry, spin)
}

#[test]
fn a_request_with_staged_host_aggregates_is_not_saved() {
    let (program, entry, _) = makes();
    let mut vm = Vm::new(Answers, Limits::default());
    let entry = FuncRef::new(vm.install(program).unwrap(), entry);
    let mut invocation = vm
        .start(entry, &[], InvocationOptions::default().yield_host_calls())
        .unwrap();
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::AwaitingHost
    );
    let staged = invocation
        .pending_host_call()
        .unwrap()
        .context()
        .tuple_new(vec![Value::I64(111)])
        .unwrap();
    // Only the host's handle reaches the staged tuple, and it would alias what later runs
    // allocate.
    let mut invocation = invocation.save().unwrap_err();
    let mut request = invocation.pending_host_call().unwrap();
    assert_eq!(request.context().tuple_len(staged), Ok(1));
    request.complete(&mut [Value::Agg(staged)], Ok(0)).unwrap();
    assert_eq!(invocation.resume(u64::MAX).unwrap(), RunStatus::Complete);
    let [Value::Agg(made)] = invocation.take_outputs().unwrap()[..] else {
        panic!("one tuple");
    };
    drop(invocation);
    assert_eq!(vm.aggregates().tuple_get(made, 0).unwrap(), Value::I64(111));
}

#[test]
fn promoted_host_aggregates_survive_saving_and_pin_collection_to_their_computation() {
    let (program, entry, spin) = makes();
    let mut vm = Vm::new(Answers, Limits::default());
    let unit = vm.install(program).unwrap();
    let entry = FuncRef::new(unit, entry);
    // Garbage below, so a collection would move the promoted tuple.
    vm.aggregates_mut().tuple_new(vec![]).unwrap();
    let mut invocation = vm
        .start(entry, &[], InvocationOptions::default().yield_host_calls())
        .unwrap();
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::AwaitingHost
    );
    let staged = {
        let mut request = invocation.pending_host_call().unwrap();
        let staged = request.context().tuple_new(vec![Value::I64(111)]).unwrap();
        // Calling back promotes the staged tuple into the heap.
        request
            .call_guest(Value::Func(FuncRef::new(unit, spin)), vec![])
            .unwrap();
        staged
    };
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::CallbackReturned
    );
    let id = invocation.save().unwrap();

    // Another run commits its own tuple meanwhile; nothing aliases.
    let [Value::Agg(other)] = vm.run(entry, &[], TraceMask::NONE, None).unwrap()[..] else {
        panic!("one tuple");
    };
    assert_ne!(other, staged);
    assert_eq!(
        vm.collect(),
        Err(CollectError::SavedHostHandles { computation: id })
    );

    let mut invocation = vm
        .resume(id, InvocationOptions::default().yield_host_calls())
        .unwrap();
    let mut request = invocation.pending_host_call().unwrap();
    assert_eq!(request.take_callback_results().unwrap(), [Value::I64(4)]);
    // The host keeps building on the same request after resuming.
    let wrapped = request
        .context()
        .tuple_new(vec![Value::Agg(staged)])
        .unwrap();
    request.complete(&mut [Value::Agg(wrapped)], Ok(0)).unwrap();
    assert_eq!(invocation.resume(u64::MAX).unwrap(), RunStatus::Complete);
    let [Value::Agg(made)] = invocation.take_outputs().unwrap()[..] else {
        panic!("one tuple");
    };
    drop(invocation);
    let heap = vm.aggregates();
    let Value::Agg(inner) = heap.tuple_get(made, 0).unwrap() else {
        panic!("the promoted tuple");
    };
    assert_eq!(heap.tuple_get(inner, 0).unwrap(), Value::I64(111));
    assert!(vm.collect().is_ok(), "the request completed");
}

#[test]
// Panic-abort targets, including default WASI, cannot catch the panic.
#[cfg(panic = "unwind")]
fn clearing_the_heap_of_saved_computations_panics_instead_of_aliasing() {
    let (program, entry) = asks();
    let mut vm = Vm::new(Answers, Limits::default());
    let entry = FuncRef::new(vm.install(program).unwrap(), entry);
    let id = save_asking(&mut vm, entry, "kept");
    for clear in [
        |vm: &mut Vm<Answers>| vm.aggregates_mut().clear(),
        |vm: &mut Vm<Answers>| drop(vm.aggregates_mut().take_nodes()),
    ] {
        let cleared = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| clear(&mut vm)));
        assert!(cleared.is_err());
    }
    // Replacing the heap outright is caught before the saved handles are used.
    let fresh = AggHeap::new();
    let previous = core::mem::replace(vm.aggregates_mut(), fresh);
    let resumed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drop(vm.resume(id, InvocationOptions::default()));
    }));
    assert!(resumed.is_err());
    *vm.aggregates_mut() = previous;
    assert_eq!(finish_asking(&mut vm, id, 3), ("kept".into(), 3));
    // With nothing saved, clearing is allowed again.
    vm.aggregates_mut().clear();
}

#[test]
fn another_vms_id_is_refused() {
    let (program, entry) = asks();
    let mut a = Vm::new(Answers, Limits::default());
    let mut b = Vm::new(Answers, Limits::default());
    let entry_a = FuncRef::new(a.install(Arc::clone(&program)).unwrap(), entry);
    let entry_b = FuncRef::new(b.install(program).unwrap(), entry);
    let from_a = save_asking(&mut a, entry_a, "a");
    let from_b = save_asking(&mut b, entry_b, "b");
    assert!(matches!(
        b.resume(from_a, InvocationOptions::default()),
        Err(ResumeError::ForeignVm)
    ));
    // The ID's VM checks come before the observer check.
    let mut sink = Silent;
    assert!(matches!(
        b.resume(
            from_a,
            InvocationOptions::default().trace(TraceMask::RUN, &mut sink)
        ),
        Err(ResumeError::ForeignVm)
    ));
    assert_eq!(finish_asking(&mut b, from_b, 2), ("b".into(), 2));
    assert_eq!(finish_asking(&mut a, from_a, 1), ("a".into(), 1));
}

/// `entry()` suspends with `need` to its embedding and returns the answer plus one.
fn suspends() -> (Arc<VerifiedProgram>, FuncId) {
    let mut b = ProgramBuilder::new();
    let need = b
        .declare_tag_with_results("need", &[], &[ValueType::I64.into()])
        .unwrap();
    let entry = b.declare_function(sig(&[], &[ValueType::I64])).unwrap();
    let mut a = b.function_asm(entry).unwrap();
    let (answer, one) = (
        a.alloc_register(ValueType::I64),
        a.alloc_register(ValueType::I64),
    );
    a.suspend(Reg::EFFECT, need, Reg::EFFECT, &[], &[answer])
        .const_i64(one, 1)
        .i64_add(answer, answer, one)
        .ret(Reg::EFFECT, &[answer]);
    b.define_function(entry, a).unwrap();
    (Arc::new(b.build_verified().unwrap()), entry)
}

#[test]
fn a_computation_saved_at_a_yielded_suspension_is_answered_later() {
    let (program, entry) = suspends();
    let mut vm = Vm::new(Answers, Limits::default());
    let entry = FuncRef::new(vm.install(program).unwrap(), entry);
    let mut invocation = vm
        .start(entry, &[], InvocationOptions::default().yield_suspensions())
        .unwrap();
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::AwaitingHandler
    );
    let id = invocation.save().unwrap();
    let (asking, ask) = asks();
    let ask = FuncRef::new(vm.install(asking).unwrap(), ask);
    vm.run(ask, &[Value::Str("other".into())], TraceMask::NONE, None)
        .unwrap();

    let mut invocation = vm
        .resume(id, InvocationOptions::default().yield_suspensions())
        .unwrap();
    assert_eq!(
        invocation.resume(0).unwrap(),
        RunStatus::AwaitingHandler,
        "still waiting"
    );
    invocation
        .pending_suspension()
        .unwrap()
        .resume(vec![Value::I64(41)])
        .unwrap();
    assert_eq!(invocation.resume(u64::MAX).unwrap(), RunStatus::Complete);
    assert_eq!(invocation.result().unwrap().unwrap(), [Value::I64(42)]);
}

/// `entry(text)` asks the host twice, boxing as it goes: `((((text,), x),), y)`.
fn asks_twice() -> (Arc<VerifiedProgram>, FuncId) {
    let mut b = ProgramBuilder::new();
    let ask = b.host_sig_for(
        "ask",
        HostSig {
            args: vec![],
            rets: vec![ValueType::I64],
        },
    );
    let entry = b
        .declare_function(sig(&[ValueType::Str], &[ValueType::Agg]))
        .unwrap();
    let mut a = b.function_asm(entry).unwrap();
    let (first, x, second, y, third) = (
        a.alloc_register(ValueType::Agg),
        a.alloc_register(ValueType::I64),
        a.alloc_register(ValueType::Agg),
        a.alloc_register(ValueType::I64),
        a.alloc_register(ValueType::Agg),
    );
    a.tuple_new(first, &[Reg(1)])
        .host_call_sig_id(Reg::EFFECT, ask, Reg::EFFECT, &[], &[x])
        .tuple_new(second, &[first, x])
        .host_call_sig_id(Reg::EFFECT, ask, Reg::EFFECT, &[], &[y])
        .tuple_new(third, &[second, y])
        .ret(Reg::EFFECT, &[third]);
    b.define_function(entry, a).unwrap();
    (Arc::new(b.build_verified().unwrap()), entry)
}

/// Resumes `id`, answers one request with `answer`, and saves it again or returns its result.
fn answer_once(
    vm: &mut Vm<Answers>,
    id: ComputationId,
    answer: i64,
) -> Result<Value, ComputationId> {
    let mut invocation = vm
        .resume(id, InvocationOptions::default().yield_host_calls())
        .unwrap();
    invocation
        .pending_host_call()
        .unwrap()
        .complete(&mut [Value::I64(answer)], Ok(0))
        .unwrap();
    match invocation.resume(u64::MAX).unwrap() {
        RunStatus::AwaitingHost => Err(invocation.save().unwrap()),
        RunStatus::Complete => Ok(invocation.take_outputs().unwrap().remove(0)),
        status => panic!("unexpected {status:?}"),
    }
}

#[test]
fn interleaved_computations_allocate_across_leases_and_collections() {
    let (program, entry) = asks_twice();
    let mut vm = Vm::new(Answers, Limits::default());
    let entry = FuncRef::new(vm.install(program).unwrap(), entry);
    let start = |vm: &mut Vm<Answers>, text: &str| {
        let args = [Value::Str(text.into())];
        let mut invocation = vm
            .start(
                entry,
                &args,
                InvocationOptions::default().yield_host_calls(),
            )
            .unwrap();
        assert_eq!(
            invocation.resume(u64::MAX).unwrap(),
            RunStatus::AwaitingHost
        );
        invocation.save().unwrap()
    };
    let a = start(&mut vm, "a");
    let b = start(&mut vm, "b");
    let a = answer_once(&mut vm, a, 1).unwrap_err();
    vm.aggregates_mut().tuple_new(vec![]).unwrap();
    assert!(vm.collect().unwrap().moved > 0);
    let b = answer_once(&mut vm, b, 2).unwrap_err();
    vm.aggregates_mut().tuple_new(vec![]).unwrap();
    assert!(vm.collect().unwrap().moved > 0);
    let Value::Agg(a) = answer_once(&mut vm, a, 3).unwrap() else {
        panic!("a tuple");
    };
    let a = vm.root(vec![Value::Agg(a)]).unwrap();
    vm.collect().unwrap();
    let Value::Agg(b) = answer_once(&mut vm, b, 4).unwrap() else {
        panic!("a tuple");
    };
    let read = |vm: &Vm<Answers>, outer: AggHandle| {
        let heap = vm.aggregates();
        let Value::Agg(second) = heap.tuple_get(outer, 0).unwrap() else {
            panic!("second");
        };
        let Value::Agg(first) = heap.tuple_get(second, 0).unwrap() else {
            panic!("first");
        };
        (
            heap.tuple_get(first, 0).unwrap(),
            heap.tuple_get(second, 1).unwrap(),
            heap.tuple_get(outer, 1).unwrap(),
        )
    };
    assert_eq!(
        read(&vm, b),
        (Value::Str("b".into()), Value::I64(2), Value::I64(4))
    );
    let [Value::Agg(a)] = vm.resolve(&a).unwrap()[..] else {
        panic!("rooted tuple");
    };
    assert_eq!(
        read(&vm, a),
        (Value::Str("a".into()), Value::I64(1), Value::I64(3))
    );
}

#[derive(Default)]
struct Silent;
impl TraceSink for Silent {}

/// Checks that every exit and completion closes the innermost thing open, entered or attached,
/// that `release` and `run_end` are last, and that what an exit names was open.
#[derive(Default)]
struct Strict {
    scopes: Vec<(ScopeKind, usize)>,
    attempts: Vec<TraceLocation>,
    done: Option<&'static str>,
    panic_on_release: bool,
}
impl Strict {
    fn event(&self) {
        assert!(self.done.is_none(), "an event after {:?}", self.done);
    }
}
impl TraceSink for Strict {
    fn attach(&mut self, _: &Program, attachment: Attachment<'_>) {
        self.event();
        assert!(
            self.scopes.is_empty() && self.attempts.is_empty(),
            "a fresh observer"
        );
        self.scopes = attachment
            .scopes
            .iter()
            .map(|scope| (scope.kind, scope.depth))
            .collect();
        self.attempts = attachment.attempt.into_iter().collect();
    }
    fn instr(&mut self, _: &Program, location: TraceLocation, _: Pc, _: Option<SpanId>, _: u16) {
        self.event();
        self.attempts.push(location);
    }
    fn instr_end(
        &mut self,
        _: &Program,
        location: TraceLocation,
        _: Option<TraceLocation>,
        _: TraceOutcome<'_>,
    ) {
        self.event();
        assert_eq!(
            self.attempts.pop(),
            Some(location),
            "ends the innermost attempt"
        );
    }
    fn scope_enter(
        &mut self,
        _: &Program,
        kind: ScopeKind,
        depth: usize,
        _: TraceLocation,
        _: Option<SpanId>,
    ) {
        self.event();
        self.scopes.push((kind, depth));
    }
    fn scope_exit(
        &mut self,
        _: &Program,
        kind: ScopeKind,
        depth: usize,
        _: TraceLocation,
        _: Option<SpanId>,
        _: TraceOutcome<'_>,
    ) {
        self.event();
        assert_eq!(
            self.scopes.pop(),
            Some((kind, depth)),
            "exits the innermost scope"
        );
    }
    fn release(&mut self, _: &Program) {
        self.event();
        self.done = Some("release");
        assert!(!self.panic_on_release, "injected release panic");
    }
    fn run_end(&mut self, _: &Program, _: TraceOutcome<'_>) {
        self.event();
        assert!(
            self.scopes.is_empty(),
            "every scope closed: {:?}",
            self.scopes
        );
        assert!(self.attempts.is_empty(), "every attempt ended");
        self.done = Some("end");
    }
}

/// `body()` catches `failed((7,))` as a reference and parks to `entry`, which handles `park`,
/// asks the host to `pause`, resumes the parked continuation, and returns what `body` rethrew
/// and caught: 7.
fn throws_and_parks() -> (Arc<VerifiedProgram>, FuncId) {
    use execution_tape::asm::Catch;
    use execution_tape::contract::Contract;
    let mut b = ProgramBuilder::new();
    let pause = b.host_sig_for(
        "pause",
        HostSig {
            args: vec![],
            rets: vec![],
        },
    );
    let integer = b.contract(Contract::Kind(ValueType::I64)).unwrap();
    let boxed = b.contract(Contract::Tuple(&[Some(integer)])).unwrap();
    let failed = b.declare_tag("failed", &[boxed.into()]).unwrap();
    let park = b.declare_tag_with_results("park", &[], &[]).unwrap();
    let thunk = b.call_sig(sig(&[], &[ValueType::I64])).unwrap();
    let entry = b.declare_function(sig(&[], &[ValueType::I64])).unwrap();
    let body = b.declare_function(sig(&[], &[ValueType::I64])).unwrap();

    let mut a = b.function_asm(body).unwrap();
    let (seven, tuple, caught, unboxed, n) = (
        a.alloc_register(ValueType::I64),
        a.alloc_register(ValueType::Agg),
        a.alloc_register(ValueType::Exn),
        a.alloc_register(ValueType::Agg),
        a.alloc_register(ValueType::I64),
    );
    a.const_i64(seven, 7).tuple_new(tuple, &[seven]);
    let handler = a.block(&[caught]);
    a.begin_region(&[Catch::AllRef(handler)]);
    b.emit_throw(&mut a, failed, &[tuple]).unwrap();
    a.end_region();
    a.place(handler).unwrap();
    a.suspend(Reg::EFFECT, park, Reg::EFFECT, &[], &[]);
    let unbox = a.block(&[unboxed]);
    a.begin_region(&[Catch::Tag(failed, unbox)]);
    a.throw_ref(Reg::EFFECT, caught);
    a.end_region();
    a.place(unbox).unwrap();
    a.tuple_get(n, unboxed, 0).ret(Reg::EFFECT, &[n]);
    b.define_function(body, a).unwrap();

    let mut a = b.function_asm(entry).unwrap();
    let (function, cont, parked, result) = (
        a.alloc_register(ValueType::Func),
        a.alloc_register(ValueType::Cont),
        a.alloc_register(ValueType::Cont),
        a.alloc_register(ValueType::I64),
    );
    a.const_func(function, body).cont_new(cont, thunk, function);
    let handle = a.block(&[parked]);
    a.begin_region(&[Catch::On(park, handle)]);
    a.resume(Reg::EFFECT, thunk, cont, Reg::EFFECT, &[], &[result]);
    a.end_region();
    a.ret(Reg::EFFECT, &[result]);
    a.place(handle).unwrap();
    a.host_call_sig_id(Reg::EFFECT, pause, Reg::EFFECT, &[], &[])
        .resume(Reg::EFFECT, thunk, parked, Reg::EFFECT, &[], &[result])
        .ret(Reg::EFFECT, &[result]);
    b.define_function(entry, a).unwrap();
    (Arc::new(b.build_verified().unwrap()), entry)
}

/// Runs `entry` to its end, answering requests and suspensions, and calling `callback` back
/// once from the first host request if given. With `rotate`, the computation is saved after
/// every instruction and resumed under the next of several trace masks, each with a fresh
/// [`Strict`] observer.
fn drive_rotating(
    program: &Arc<VerifiedProgram>,
    entry: FuncId,
    args: &[Value],
    callback: Option<FuncId>,
    rotate: bool,
) -> Result<Vec<Value>, Trap> {
    let masks = [
        TraceMask::NONE,
        TraceMask::RUN,
        TraceMask::RUN | TraceMask::CALL,
        TraceMask::CALL | TraceMask::HOST | TraceMask::INSTR,
        TraceMask::RUN | TraceMask::CALL | TraceMask::HOST | TraceMask::INSTR | TraceMask::VALUES,
    ];
    let mut vm = Vm::new(Answers, Limits::default());
    let unit = vm.install(Arc::clone(program)).unwrap();
    fn options<'a>() -> InvocationOptions<'a> {
        InvocationOptions::default()
            .yield_host_calls()
            .yield_suspensions()
    }
    let mut saved: Option<ComputationId> = None;
    let mut called_back = false;
    let mut calling_back = false;
    for lease in 0.. {
        let mut sink = Strict::default();
        let mask = if rotate {
            masks[lease % masks.len()]
        } else {
            masks[masks.len() - 1]
        };
        let mut invocation = match saved {
            None => vm
                .start(
                    FuncRef::new(unit, entry),
                    args,
                    options().trace(mask, &mut sink),
                )
                .unwrap(),
            // While a callback runs, it can only be resumed unobserved.
            Some(id) if calling_back => vm.resume(id, options()).unwrap(),
            Some(id) => vm.resume(id, options().trace(mask, &mut sink)).unwrap(),
        };
        let quantum = if rotate { 1 } else { u64::MAX };
        let status = match invocation.resume(quantum) {
            Ok(status) => status,
            Err(error) => return Err(error.trap),
        };
        match status {
            RunStatus::Suspended => {}
            RunStatus::Complete => return Ok(invocation.take_outputs().unwrap()),
            RunStatus::AwaitingHandler => invocation
                .pending_suspension()
                .unwrap()
                .resume(vec![Value::I64(41)])
                .unwrap(),
            RunStatus::CallbackReturned => {
                calling_back = false;
                let mut request = invocation.pending_host_call().unwrap();
                request.take_callback_results().unwrap();
                request.complete(&mut [Value::I64(5)], Ok(0)).unwrap();
            }
            RunStatus::AwaitingHost => {
                let request = invocation.pending_host_call().unwrap();
                if let Some(callback) = callback.filter(|_| !called_back) {
                    called_back = true;
                    calling_back = true;
                    request
                        .call_guest(Value::Func(FuncRef::new(unit, callback)), vec![])
                        .unwrap();
                } else {
                    let mut answer = match request.symbol() {
                        "pause" => vec![],
                        _ => vec![Value::I64(5)],
                    };
                    request.complete(&mut answer, Ok(0)).unwrap();
                }
            }
            status => panic!("unexpected {status:?}"),
        }
        saved = Some(invocation.save().unwrap());
    }
    unreachable!("the computation ends")
}

#[test]
fn saving_after_every_instruction_under_rotating_observers_keeps_every_trace_balanced() {
    let (nested, nested_entry) = nested();
    let (calls, calls_entry, spin) = calls_back();
    let (suspending, suspending_entry) = suspends();
    let (twice, twice_entry) = asks_twice();
    let (throwing, throwing_entry) = throws_and_parks();
    let text = [Value::Str("text".into())];
    /// A program, its entry, the entry's arguments and a function to call back, if any.
    type Case<'a> = (
        &'a Arc<VerifiedProgram>,
        FuncId,
        &'a [Value],
        Option<FuncId>,
    );
    let cases: [Case<'_>; 5] = [
        (&nested, nested_entry, &[], None),
        (&calls, calls_entry, &[], Some(spin)),
        (&suspending, suspending_entry, &[], None),
        (&twice, twice_entry, &text, None),
        (&throwing, throwing_entry, &[], None),
    ];
    for (program, entry, args, callback) in cases {
        let rotated = drive_rotating(program, entry, args, callback, true).unwrap();
        let straight = drive_rotating(program, entry, args, callback, false).unwrap();
        assert_eq!(rotated.len(), straight.len());
        if !matches!(straight[..], [Value::Agg(_)]) {
            assert_eq!(rotated, straight, "the same result as without saving");
        }
    }
}

#[test]
fn a_trap_and_a_cancellation_after_an_observed_resume_close_what_was_attached() {
    let (program, entry) = counts();
    let mask = TraceMask::RUN | TraceMask::CALL | TraceMask::INSTR;
    let limits = Limits {
        fuel: 5,
        ..Limits::default()
    };
    let mut vm = Vm::new(Answers, limits);
    let entry = FuncRef::new(vm.install(program).unwrap(), entry);
    for trap in [true, false] {
        let mut invocation = vm.start(entry, &[], InvocationOptions::default()).unwrap();
        assert_eq!(invocation.resume(4).unwrap(), RunStatus::Suspended);
        let id = invocation.save().unwrap();
        let mut sink = Strict::default();
        let mut invocation = vm
            .resume(id, InvocationOptions::default().trace(mask, &mut sink))
            .unwrap();
        if trap {
            let error = invocation.resume(u64::MAX).unwrap_err();
            assert!(matches!(error.trap, Trap::FuelExceeded), "{error:?}");
        } else {
            assert_eq!(invocation.resume(2).unwrap(), RunStatus::Suspended);
        }
        drop(invocation);
        assert_eq!(sink.done, Some("end"), "the run ended, balanced");
    }
}

#[test]
// Panic-abort targets, including default WASI, cannot catch the panic.
#[cfg(panic = "unwind")]
fn release_is_the_last_event_even_when_it_panics() {
    let (program, entry) = asks();
    let mut vm = Vm::new(Answers, Limits::default());
    let entry = FuncRef::new(vm.install(program).unwrap(), entry);
    let args = [Value::Str("text".into())];
    let mut sink = Strict {
        panic_on_release: true,
        ..Strict::default()
    };
    let saved = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut invocation = vm
            .start(
                entry,
                &args,
                InvocationOptions::default().yield_host_calls().trace(
                    TraceMask::RUN | TraceMask::CALL | TraceMask::HOST,
                    &mut sink,
                ),
            )
            .unwrap();
        assert_eq!(
            invocation.resume(u64::MAX).unwrap(),
            RunStatus::AwaitingHost
        );
        invocation.save().map(drop).map_err(drop)
    }));
    // The panic cancelled the computation instead, without reporting to the released sink.
    assert!(saved.is_err());
    assert_eq!(sink.done, Some("release"));
    assert_eq!(vm.heap_stats().saved_computations, 0);
}

/// `entry()` waits, by asking the host or suspending with `need`, then adds one twenty times to
/// the answer: about 23 instructions.
fn waits_then_counts(suspend: bool) -> (Arc<VerifiedProgram>, FuncId) {
    let mut b = ProgramBuilder::new();
    let ask = b.host_sig_for(
        "ask",
        HostSig {
            args: vec![],
            rets: vec![ValueType::I64],
        },
    );
    let need = b
        .declare_tag_with_results("need", &[], &[ValueType::I64.into()])
        .unwrap();
    let entry = b.declare_function(sig(&[], &[ValueType::I64])).unwrap();
    let mut a = b.function_asm(entry).unwrap();
    let (sum, one) = (
        a.alloc_register(ValueType::I64),
        a.alloc_register(ValueType::I64),
    );
    if suspend {
        a.suspend(Reg::EFFECT, need, Reg::EFFECT, &[], &[sum]);
    } else {
        a.host_call_sig_id(Reg::EFFECT, ask, Reg::EFFECT, &[], &[sum]);
    }
    a.const_i64(one, 1);
    for _ in 0..20 {
        a.i64_add(sum, sum, one);
    }
    a.ret(Reg::EFFECT, &[sum]);
    b.define_function(entry, a).unwrap();
    (Arc::new(b.build_verified().unwrap()), entry)
}

/// Replays `recorder`'s recording and returns its values, which must be one `I64`.
fn replayed(recorder: Recorder) -> i64 {
    let recording = recorder.finish();
    assert_eq!(recording.status(), Ok(()), "a complete recording");
    match recording.replay() {
        Ok(ReplayOutcome::Returned(values)) => match values.values() {
            [Value::I64(n)] => *n,
            other => panic!("replay returned {other:?}"),
        },
        other => panic!("replay ended {other:?}"),
    }
}

#[test]
fn replay_saves_where_the_original_saved_at_a_wait_and_resumes_under_its_limits() {
    for suspend in [false, true] {
        let (program, entry) = waits_then_counts(suspend);
        // Enough fuel to reach the wait, not to finish.
        let mut vm = Vm::new(
            Answers,
            Limits {
                fuel: 2,
                ..Limits::default()
            },
        );
        let entry = FuncRef::new(vm.install(Arc::clone(&program)).unwrap(), entry);
        let mut recorder = Recorder::new(program, RecordLimits::default());
        let mask = recorder.mask();
        let mut invocation = vm
            .start(
                entry,
                &[],
                InvocationOptions::default()
                    .yield_host_calls()
                    .yield_suspensions()
                    .trace(mask, &mut recorder),
            )
            .unwrap();
        let waiting = invocation.resume(u64::MAX).unwrap();
        assert_ne!(waiting, RunStatus::Suspended);
        let id = invocation.save().ok().unwrap();
        vm.set_limits(Limits::default());
        let mut invocation = vm
            .resume(
                id,
                InvocationOptions::default()
                    .yield_host_calls()
                    .yield_suspensions()
                    .trace(mask, &mut recorder),
            )
            .unwrap();
        if suspend {
            invocation
                .pending_suspension()
                .unwrap()
                .resume(vec![Value::I64(10)])
                .unwrap();
        } else {
            invocation
                .pending_host_call()
                .unwrap()
                .complete(&mut [Value::I64(10)], Ok(0))
                .unwrap();
        }
        assert_eq!(invocation.resume(u64::MAX).unwrap(), RunStatus::Complete);
        assert_eq!(invocation.result().unwrap().unwrap(), [Value::I64(30)]);
        drop(invocation);
        // Without saving at the wait, replay would run out of the first invocation's fuel.
        assert_eq!(replayed(recorder), 30);
    }
}

#[test]
fn replay_repeats_every_save_of_a_computation_saved_many_times() {
    let (program, entry) = counts();
    let mut vm = Vm::new(
        Answers,
        Limits {
            fuel: 5,
            ..Limits::default()
        },
    );
    let entry = FuncRef::new(vm.install(Arc::clone(&program)).unwrap(), entry);
    let mut recorder = Recorder::new(program, RecordLimits::default());
    let mask = recorder.mask();
    let mut invocation = vm
        .start(
            entry,
            &[],
            InvocationOptions::default().trace(mask, &mut recorder),
        )
        .unwrap();
    let mut saves = 0;
    while invocation.resume(4).unwrap() == RunStatus::Suspended {
        let id = invocation.save().ok().unwrap();
        saves += 1;
        invocation = vm
            .resume(id, InvocationOptions::default().trace(mask, &mut recorder))
            .unwrap();
    }
    drop(invocation);
    assert!(saves >= 5, "{saves}");
    assert_eq!(replayed(recorder), 20);
}

/// `fwd(x)` calls `scale` (`x * factor`) through link `"scale"`; `fwd` is function 1.
fn forwards(factor: i64) -> Arc<VerifiedProgram> {
    let mut b = ProgramBuilder::new();
    let scale = b
        .declare_function(sig(&[ValueType::I64], &[ValueType::I64]))
        .unwrap();
    let mut a = b.function_asm(scale).unwrap();
    let (k, y) = (
        a.alloc_register(ValueType::I64),
        a.alloc_register(ValueType::I64),
    );
    a.const_i64(k, factor)
        .i64_mul(y, Reg(1), k)
        .ret(Reg::EFFECT, &[y]);
    b.define_function(scale, a).unwrap();
    let link = b.declare_link("scale", scale).unwrap();
    let fwd = b
        .declare_function(sig(&[ValueType::I64], &[ValueType::I64]))
        .unwrap();
    let mut a = b.function_asm(fwd).unwrap();
    let y = a.alloc_register(ValueType::I64);
    a.call_link(Reg::EFFECT, link, Reg::EFFECT, &[Reg(1)], &[y])
        .ret(Reg::EFFECT, &[y]);
    b.define_function(fwd, a).unwrap();
    Arc::new(b.build_verified().unwrap())
}

#[test]
fn units_installed_and_relinked_while_saved_replay_with_their_links() {
    let (program, entry) = asks_then_links(2);
    let mut vm = Vm::new(Answers, Limits::default());
    let unit = vm.install(Arc::clone(&program)).unwrap();
    let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
    let mask = recorder.mask();
    let args = [Value::I64(3)];
    let mut invocation = vm
        .start(
            FuncRef::new(unit, entry),
            &args,
            InvocationOptions::default()
                .yield_host_calls()
                .trace(mask, &mut recorder),
        )
        .unwrap();
    // Saved before its first instruction.
    assert_eq!(invocation.resume(0).unwrap(), RunStatus::Suspended);
    let id = invocation.save().ok().unwrap();
    let forward = vm.install(forwards(10)).unwrap();
    let hundred = vm.install(forwards(100)).unwrap();
    vm.relink(forward, LinkId(0), FuncRef::new(hundred, FuncId(0)))
        .unwrap();
    vm.relink(unit, LinkId(0), FuncRef::new(forward, FuncId(1)))
        .unwrap();
    let mut invocation = vm
        .resume(
            id,
            InvocationOptions::default()
                .yield_host_calls()
                .trace(mask, &mut recorder),
        )
        .unwrap();
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::AwaitingHost
    );
    invocation
        .pending_host_call()
        .unwrap()
        .complete(&mut [Value::I64(0)], Ok(0))
        .unwrap();
    assert_eq!(invocation.resume(u64::MAX).unwrap(), RunStatus::Complete);
    assert_eq!(invocation.result().unwrap().unwrap(), [Value::I64(300)]);
    drop(invocation);
    assert_eq!(replayed(recorder), 300);
}

#[test]
fn tags_defined_while_saved_replay_before_the_next_invocation() {
    for install_first in [true, false] {
        let (program, entry) = counts();
        let mut vm = Vm::new(Answers, Limits::default());
        let entry = FuncRef::new(vm.install(Arc::clone(&program)).unwrap(), entry);
        let mut recorder = Recorder::new(program, RecordLimits::default());
        let mask = recorder.mask();
        let mut invocation = vm
            .start(
                entry,
                &[],
                InvocationOptions::default().trace(mask, &mut recorder),
            )
            .unwrap();
        assert_eq!(invocation.resume(3).unwrap(), RunStatus::Suspended);
        let id = invocation.save().ok().unwrap();
        if install_first {
            vm.install(forwards(1)).unwrap();
        }
        vm.define_tag("later", &[]).unwrap();
        let mut invocation = vm
            .resume(id, InvocationOptions::default().trace(mask, &mut recorder))
            .unwrap();
        assert_eq!(invocation.resume(u64::MAX).unwrap(), RunStatus::Complete);
        drop(invocation);
        assert_eq!(replayed(recorder), 20);
    }
}

#[test]
fn an_invocation_run_without_the_recorder_makes_the_capture_incomplete() {
    let (program, entry) = counts();
    let mut vm = Vm::new(Answers, Limits::default());
    let entry = FuncRef::new(vm.install(Arc::clone(&program)).unwrap(), entry);
    let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
    let mask = recorder.mask();
    let mut invocation = vm
        .start(
            entry,
            &[],
            InvocationOptions::default().trace(mask, &mut recorder),
        )
        .unwrap();
    assert_eq!(invocation.resume(6).unwrap(), RunStatus::Suspended);
    let id = invocation.save().ok().unwrap();
    // Another recorder, or none, runs the next invocation.
    let mut stranger = Recorder::new(program, RecordLimits::default());
    let mut invocation = vm
        .resume(id, InvocationOptions::default().trace(mask, &mut stranger))
        .unwrap();
    assert_eq!(invocation.resume(6).unwrap(), RunStatus::Suspended);
    let id = invocation.save().ok().unwrap();
    assert_eq!(stranger.finish().status(), Err(&CaptureFault::MissingEntry));
    let mut invocation = vm
        .resume(id, InvocationOptions::default().trace(mask, &mut recorder))
        .unwrap();
    assert_eq!(invocation.resume(u64::MAX).unwrap(), RunStatus::Complete);
    drop(invocation);
    assert_eq!(recorder.finish().status(), Err(&CaptureFault::Unrecorded));
}

/// `entry(x)` returns `x + x`, one addition at a time.
fn doubles() -> (Arc<VerifiedProgram>, FuncId) {
    let mut b = ProgramBuilder::new();
    let entry = b
        .declare_function(sig(&[ValueType::I64], &[ValueType::I64]))
        .unwrap();
    let mut a = b.function_asm(entry).unwrap();
    let y = a.alloc_register(ValueType::I64);
    a.const_i64(y, 0)
        .i64_add(y, y, Reg(1))
        .i64_add(y, y, Reg(1))
        .ret(Reg::EFFECT, &[y]);
    b.define_function(entry, a).unwrap();
    (Arc::new(b.build_verified().unwrap()), entry)
}

#[test]
fn a_recorder_handed_another_computations_resume_is_incomplete() {
    let (program, entry) = doubles();
    let mut vm = Vm::new(Answers, Limits::default());
    let entry = FuncRef::new(vm.install(Arc::clone(&program)).unwrap(), entry);
    let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
    let mask = recorder.mask();
    let mut first = vm
        .start(
            entry,
            &[Value::I64(3)],
            InvocationOptions::default().trace(mask, &mut recorder),
        )
        .unwrap();
    assert_eq!(first.resume(1).unwrap(), RunStatus::Suspended);
    let first = first.save().ok().unwrap();
    let mut second = vm
        .start(entry, &[Value::I64(4)], InvocationOptions::default())
        .unwrap();
    assert_eq!(second.resume(1).unwrap(), RunStatus::Suspended);
    let second = second.save().ok().unwrap();

    // The recorded computation finishes unobserved, without allocating; the other one, whose
    // invocation count matches, resumes with the recorder.
    let mut invocation = vm.resume(first, InvocationOptions::default()).unwrap();
    assert_eq!(invocation.resume(u64::MAX).unwrap(), RunStatus::Complete);
    assert_eq!(invocation.result().unwrap().unwrap(), [Value::I64(6)]);
    drop(invocation);
    let mut invocation = vm
        .resume(
            second,
            InvocationOptions::default().trace(mask, &mut recorder),
        )
        .unwrap();
    assert_eq!(invocation.resume(u64::MAX).unwrap(), RunStatus::Complete);
    assert_eq!(invocation.result().unwrap().unwrap(), [Value::I64(8)]);
    drop(invocation);
    assert_eq!(recorder.finish().status(), Err(&CaptureFault::MissingEntry));
}

#[test]
fn a_save_inside_a_callback_resumed_unobserved_leaves_the_recording_incomplete() {
    let (program, entry, spin) = calls_back();
    let mut vm = Vm::new(Answers, Limits::default());
    let unit = vm.install(Arc::clone(&program)).unwrap();
    let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
    let mask = recorder.mask();
    let mut invocation = vm
        .start(
            FuncRef::new(unit, entry),
            &[],
            InvocationOptions::default()
                .yield_host_calls()
                .trace(mask, &mut recorder),
        )
        .unwrap();
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::AwaitingHost
    );
    invocation
        .pending_host_call()
        .unwrap()
        .call_guest(Value::Func(FuncRef::new(unit, spin)), vec![])
        .unwrap();
    assert_eq!(invocation.resume(1).unwrap(), RunStatus::Suspended);
    let id = invocation.save().ok().unwrap();
    // A running callback is resumed only unobserved.
    let mut invocation = vm
        .resume(id, InvocationOptions::default().yield_host_calls())
        .unwrap();
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::CallbackReturned
    );
    let id = invocation.save().ok().unwrap();
    let mut invocation = vm
        .resume(
            id,
            InvocationOptions::default()
                .yield_host_calls()
                .trace(mask, &mut recorder),
        )
        .unwrap();
    let mut request = invocation.pending_host_call().unwrap();
    assert_eq!(request.take_callback_results().unwrap(), [Value::I64(4)]);
    request.complete(&mut [Value::I64(8)], Ok(0)).unwrap();
    assert_eq!(invocation.resume(u64::MAX).unwrap(), RunStatus::Complete);
    drop(invocation);
    assert_eq!(recorder.finish().status(), Err(&CaptureFault::Unrecorded));
}

#[test]
fn a_computation_saved_where_its_callback_returned_replays_under_tight_fuel() {
    let (program, entry, spin) = calls_back();
    // The least fuel that reaches the callback's return.
    let fuel = (1..32)
        .find(|&fuel| {
            let mut vm = Vm::new(
                Answers,
                Limits {
                    fuel,
                    ..Limits::default()
                },
            );
            let unit = vm.install(Arc::clone(&program)).unwrap();
            let mut invocation = vm
                .start(
                    FuncRef::new(unit, entry),
                    &[],
                    InvocationOptions::default().yield_host_calls(),
                )
                .unwrap();
            if invocation.resume(u64::MAX).ok() != Some(RunStatus::AwaitingHost) {
                return false;
            }
            invocation
                .pending_host_call()
                .unwrap()
                .call_guest(Value::Func(FuncRef::new(unit, spin)), vec![])
                .is_ok()
                && invocation.resume(u64::MAX).ok() == Some(RunStatus::CallbackReturned)
        })
        .expect("some fuel reaches the return");
    let mut vm = Vm::new(
        Answers,
        Limits {
            fuel,
            ..Limits::default()
        },
    );
    let unit = vm.install(Arc::clone(&program)).unwrap();
    let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
    let mask = recorder.mask();
    let mut invocation = vm
        .start(
            FuncRef::new(unit, entry),
            &[],
            InvocationOptions::default()
                .yield_host_calls()
                .trace(mask, &mut recorder),
        )
        .unwrap();
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::AwaitingHost
    );
    invocation
        .pending_host_call()
        .unwrap()
        .call_guest(Value::Func(FuncRef::new(unit, spin)), vec![])
        .unwrap();
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::CallbackReturned
    );
    let id = invocation.save().ok().unwrap();
    vm.set_limits(Limits::default());
    let mut invocation = vm
        .resume(
            id,
            InvocationOptions::default()
                .yield_host_calls()
                .trace(mask, &mut recorder),
        )
        .unwrap();
    let mut request = invocation.pending_host_call().unwrap();
    assert_eq!(request.take_callback_results().unwrap(), [Value::I64(4)]);
    request.complete(&mut [Value::I64(8)], Ok(0)).unwrap();
    assert_eq!(invocation.resume(u64::MAX).unwrap(), RunStatus::Complete);
    assert_eq!(invocation.result().unwrap().unwrap(), [Value::I64(8)]);
    drop(invocation);
    assert_eq!(replayed(recorder), 8);
}

/// `entry(n)` asks the host, then calls `make(n)` through a link; `make` boxes `n` in a tuple.
fn asks_then_makes() -> (Arc<VerifiedProgram>, FuncId) {
    let mut b = ProgramBuilder::new();
    let ask = b.host_sig_for(
        "ask",
        HostSig {
            args: vec![],
            rets: vec![ValueType::I64],
        },
    );
    let make = b
        .declare_function(sig(&[ValueType::I64], &[ValueType::Agg]))
        .unwrap();
    let mut a = b.function_asm(make).unwrap();
    let boxed = a.alloc_register(ValueType::Agg);
    a.tuple_new(boxed, &[Reg(1)]).ret(Reg::EFFECT, &[boxed]);
    b.define_function(make, a).unwrap();
    let link = b.declare_link("make", make).unwrap();
    let entry = b.declare_function(sig(&[], &[ValueType::Agg])).unwrap();
    let mut a = b.function_asm(entry).unwrap();
    let (n, made) = (
        a.alloc_register(ValueType::I64),
        a.alloc_register(ValueType::Agg),
    );
    a.host_call_sig_id(Reg::EFFECT, ask, Reg::EFFECT, &[], &[n])
        .call_link(Reg::EFFECT, link, Reg::EFFECT, &[n], &[made])
        .ret(Reg::EFFECT, &[made]);
    b.define_function(entry, a).unwrap();
    (Arc::new(b.build_verified().unwrap()), entry)
}

/// `make(n)` builds the struct `{ x: n, y: n }`.
fn makes_structs() -> Arc<VerifiedProgram> {
    let mut b = ProgramBuilder::new();
    let point = b.struct_type(StructTypeDef {
        field_names: vec!["x".into(), "y".into()],
        field_types: vec![ValueType::I64, ValueType::I64],
    });
    let make = b
        .declare_function(sig(&[ValueType::I64], &[ValueType::Agg]))
        .unwrap();
    let mut a = b.function_asm(make).unwrap();
    let made = a.alloc_register(ValueType::Agg);
    a.struct_new(made, point, &[Reg(1), Reg(1)])
        .ret(Reg::EFFECT, &[made]);
    b.define_function(make, a).unwrap();
    Arc::new(b.build_verified().unwrap())
}

/// The struct shape and fields of `value` in `heap`.
fn struct_of(heap: &AggHeap, value: &Value) -> (AggShape, Vec<Value>) {
    let Value::Agg(handle) = value else {
        panic!("an aggregate: {value:?}");
    };
    let AggType::Struct { shape, .. } = heap.agg_type(*handle).unwrap() else {
        panic!("a struct");
    };
    let fields = (0..heap.struct_field_count(*handle).unwrap())
        .map(|field| heap.struct_get(*handle, field).unwrap())
        .collect();
    (heap.shape(shape).unwrap().clone(), fields)
}

#[test]
fn shapes_interned_while_saved_replay_the_same_structs() {
    let (program, entry) = asks_then_makes();
    let mut vm = Vm::new(Answers, Limits::default());
    let unit = vm.install(Arc::clone(&program)).unwrap();
    let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
    let mask = recorder.mask();
    let mut invocation = vm
        .start(
            FuncRef::new(unit, entry),
            &[],
            InvocationOptions::default()
                .yield_host_calls()
                .trace(mask, &mut recorder),
        )
        .unwrap();
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::AwaitingHost
    );
    let id = invocation.save().ok().unwrap();
    // The host interns a shape of its own, then a unit whose struct shape is new is linked in.
    vm.aggregates_mut()
        .intern_shape(AggShape::Struct(StructTypeDef {
            field_names: vec!["host".into()],
            field_types: vec![ValueType::Bool],
        }))
        .unwrap();
    let structs = vm.install(makes_structs()).unwrap();
    vm.relink(unit, LinkId(0), FuncRef::new(structs, FuncId(0)))
        .unwrap();
    let mut invocation = vm
        .resume(
            id,
            InvocationOptions::default()
                .yield_host_calls()
                .trace(mask, &mut recorder),
        )
        .unwrap();
    invocation
        .pending_host_call()
        .unwrap()
        .complete(&mut [Value::I64(7)], Ok(0))
        .unwrap();
    assert_eq!(invocation.resume(u64::MAX).unwrap(), RunStatus::Complete);
    let live = invocation.take_outputs().unwrap();
    drop(invocation);
    let live = struct_of(vm.aggregates(), &live[0]);
    assert_eq!(live.1, [Value::I64(7), Value::I64(7)]);

    let recording = recorder.finish();
    assert_eq!(recording.status(), Ok(()), "a complete recording");
    let Ok(ReplayOutcome::Returned(replayed)) = recording.replay() else {
        panic!("replay returns");
    };
    assert_eq!(
        struct_of(replayed.aggregates(), &replayed.values()[0]),
        live
    );
}

#[test]
fn a_spawned_computation_waits_before_its_first_instruction_until_resumed() {
    let (program, entry) = counts();
    let mut vm = Vm::new(Answers, Limits::default());
    let entry = FuncRef::new(vm.install(program).unwrap(), entry);
    let id = vm.spawn(entry, vec![]).unwrap();
    assert_eq!(vm.heap_stats().saved_computations, 1);
    assert_eq!(vm.heap_stats().saved_frames, 1, "its entry frame");
    let mut invocation = vm.resume(id, InvocationOptions::default()).unwrap();
    assert_eq!(invocation.work().fuel, 0, "nothing ran before");
    assert_eq!(invocation.resume(u64::MAX).unwrap(), RunStatus::Complete);
    assert_eq!(invocation.result().unwrap().unwrap(), [Value::I64(20)]);
    drop(invocation);
    let error = vm.spawn(entry, vec![Value::I64(1)]).unwrap_err();
    assert!(matches!(error.trap, Trap::ArityMismatch), "{error:?}");
    assert_eq!(vm.heap_stats().saved_computations, 0);
}

#[test]
fn discarding_a_saved_computation_releases_its_frames_and_aggregates() {
    let (program, entry) = asks();
    let mut vm = Vm::new(Answers, Limits::default());
    let entry = FuncRef::new(vm.install(program).unwrap(), entry);
    let kept = save_asking(&mut vm, entry, "kept");
    let discarded = save_asking(&mut vm, entry, "discarded");
    assert_eq!(vm.aggregates().len_u32(), 2);
    vm.discard(discarded).unwrap();
    assert!(matches!(vm.discard(discarded), Err(ResumeError::Unknown)));
    assert!(matches!(
        vm.resume(discarded, InvocationOptions::default()),
        Err(ResumeError::Unknown)
    ));
    assert_eq!(vm.heap_stats().saved_computations, 1);
    assert_eq!(vm.heap_stats().saved_frames, 1);
    assert_eq!(
        vm.collect().unwrap().aggregates.live,
        1,
        "only the kept box"
    );
    assert_eq!(finish_asking(&mut vm, kept, 1), ("kept".into(), 1));
}

#[test]
fn the_vm_keeps_at_most_its_limit_of_saved_computations() {
    let (program, entry) = asks();
    let limits = Limits {
        max_saved_computations: 2,
        ..Limits::default()
    };
    let mut vm = Vm::new(Answers, limits);
    let entry = FuncRef::new(vm.install(program).unwrap(), entry);
    let first = save_asking(&mut vm, entry, "first");
    save_asking(&mut vm, entry, "second");
    let args = [Value::Str("third".into())];
    let mut invocation = vm
        .start(
            entry,
            &args,
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::AwaitingHost
    );
    let invocation = invocation.save().unwrap_err();
    drop(invocation);
    let error = vm.spawn(entry, args.to_vec()).unwrap_err();
    assert!(
        matches!(error.trap, Trap::SavedComputationLimitExceeded { limit: 2 }),
        "{error:?}"
    );
    vm.discard(first).unwrap();
    save_asking(&mut vm, entry, "third");
}

#[test]
fn saved_frames_count_against_the_vms_frame_limit() {
    let (program, entry) = nested();
    let limits = Limits {
        max_frames: 3,
        ..Limits::default()
    };
    let mut vm = Vm::new(Answers, limits);
    let entry = FuncRef::new(vm.install(program).unwrap(), entry);
    let mut invocation = vm
        .start(entry, &[], InvocationOptions::default().yield_host_calls())
        .unwrap();
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::AwaitingHost
    );
    let saved = invocation.save().unwrap();
    assert_eq!(vm.heap_stats().saved_frames, 2);
    // Its entry frame fits beside the two saved ones; the call into `mid` does not.
    let error = vm.run(entry, &[], TraceMask::NONE, None).unwrap_err();
    assert!(
        matches!(
            error.trap,
            Trap::FrameLimitExceeded {
                limit: 3,
                resident: 3
            }
        ),
        "{error:?}"
    );
    vm.discard(saved).unwrap();
    assert_eq!(
        vm.run(entry, &[], TraceMask::NONE, None).unwrap(),
        [Value::I64(1)],
        "discarding released the frames"
    );
}

#[test]
fn a_frame_refused_for_another_computations_frames_replays_the_refusal() {
    let (program, entry) = nested();
    let limits = Limits {
        max_frames: 3,
        ..Limits::default()
    };
    let mut vm = Vm::new(Answers, limits);
    let entry = FuncRef::new(vm.install(Arc::clone(&program)).unwrap(), entry);
    let mut waiting = vm
        .start(entry, &[], InvocationOptions::default().yield_host_calls())
        .unwrap();
    assert_eq!(waiting.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    waiting.save().ok().unwrap();

    let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
    let mask = recorder.mask();
    let mut invocation = vm
        .start(
            entry,
            &[],
            InvocationOptions::default()
                .yield_host_calls()
                .trace(mask, &mut recorder),
        )
        .unwrap();
    let error = invocation.resume(u64::MAX).unwrap_err();
    assert!(
        matches!(
            error.trap,
            Trap::FrameLimitExceeded {
                limit: 3,
                resident: 3
            }
        ),
        "{error:?}"
    );
    drop(invocation);
    let recording = recorder.finish();
    assert_eq!(recording.status(), Ok(()));
    let outcome = recording.replay();
    let Ok(ReplayOutcome::Trapped(replayed)) = outcome else {
        panic!("replay traps as the original did: {outcome:?}");
    };
    assert!(
        matches!(
            replayed.trap,
            Trap::FrameLimitExceeded {
                limit: 3,
                resident: 3
            }
        ),
        "{replayed:?}"
    );
}

#[test]
fn a_spawned_computation_is_recorded_from_its_first_resume_and_replays() {
    let (program, entry) = counts();
    let mut vm = Vm::new(Answers, Limits::default());
    let entry = FuncRef::new(vm.install(Arc::clone(&program)).unwrap(), entry);
    let mut id = vm.spawn(entry, vec![]).unwrap();
    let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
    let mask = recorder.mask();
    let result = loop {
        let mut invocation = vm
            .resume(id, InvocationOptions::default().trace(mask, &mut recorder))
            .unwrap();
        match invocation.resume(7).unwrap() {
            RunStatus::Suspended => id = invocation.save().ok().unwrap(),
            RunStatus::Complete => break invocation.result().unwrap().unwrap().to_vec(),
            status => panic!("unexpected {status:?}"),
        }
    };
    assert_eq!(result, [Value::I64(20)]);
    assert_eq!(replayed(recorder), 20);
}

#[test]
fn an_argument_naming_no_aggregate_is_refused_at_entry() {
    let (program, entry) = asks();
    let mut vm = Vm::new(Answers, Limits::default());
    let unit = vm.install(program).unwrap();
    // `asks` takes a string; a closure-taking entry would do as well: any aggregate handle.
    let mut b = ProgramBuilder::new();
    let keeps = b
        .declare_function(sig(&[ValueType::Agg], &[ValueType::Agg]))
        .unwrap();
    let mut a = b.function_asm(keeps).unwrap();
    a.ret(Reg::EFFECT, &[Reg(1)]);
    b.define_function(keeps, a).unwrap();
    let keeps = FuncRef::new(
        vm.install(Arc::new(b.build_verified().unwrap())).unwrap(),
        keeps,
    );
    let dangling = [Value::Agg(AggHandle(999))];
    let error = vm.spawn(keeps, dangling.to_vec()).unwrap_err();
    assert!(
        matches!(error.trap, Trap::AggError(AggError::BadHandle)),
        "{error:?}"
    );
    let error = vm
        .start(keeps, &dangling, InvocationOptions::default())
        .err()
        .unwrap();
    assert!(
        matches!(error.trap, Trap::AggError(AggError::BadHandle)),
        "{error:?}"
    );
    assert_eq!(vm.heap_stats().saved_computations, 0);
    vm.collect().unwrap();
    save_asking(&mut vm, FuncRef::new(unit, entry), "still saves");
}

#[test]
fn a_request_id_finds_its_request_after_saving_and_refuses_once_answered() {
    let (program, entry) = asks_twice();
    let mut vm = Vm::new(Answers, Limits::default());
    let entry = FuncRef::new(vm.install(program).unwrap(), entry);
    let args = [Value::Str("text".into())];
    fn options<'a>() -> InvocationOptions<'a> {
        InvocationOptions::default().yield_host_calls()
    }
    let mut invocation = vm.start(entry, &args, options()).unwrap();
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::AwaitingHost
    );
    let first = invocation.pending_host_call().unwrap().id();
    let id = invocation.save().unwrap();

    let mut invocation = vm.resume(id, options()).unwrap();
    assert_eq!(
        invocation.pending_host_call().unwrap().id(),
        first,
        "stable"
    );
    invocation
        .host_request(first)
        .unwrap()
        .complete(&mut [Value::I64(1)], Ok(0))
        .unwrap();
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::AwaitingHost
    );
    // The same instruction admitted again is a new request.
    assert!(matches!(
        invocation.host_request(first),
        Err(RequestError::NotWaiting)
    ));
    let second = invocation.pending_host_call().unwrap().id();
    assert_ne!(second, first);
    invocation
        .host_request(second)
        .unwrap()
        .complete(&mut [Value::I64(2)], Ok(0))
        .unwrap();
    assert_eq!(invocation.resume(u64::MAX).unwrap(), RunStatus::Complete);
    assert!(matches!(
        invocation.host_request(second),
        Err(RequestError::NotWaiting)
    ));
}

#[test]
fn a_request_id_is_refused_by_other_computations_and_for_the_other_kind() {
    let (program, entry) = asks();
    let (suspending, suspends) = suspends();
    let mut vm = Vm::new(Answers, Limits::default());
    let entry = FuncRef::new(vm.install(program).unwrap(), entry);
    let suspends = FuncRef::new(vm.install(suspending).unwrap(), suspends);
    let one = save_asking(&mut vm, entry, "one");
    let two = save_asking(&mut vm, entry, "two");
    fn options<'a>() -> InvocationOptions<'a> {
        InvocationOptions::default()
            .yield_host_calls()
            .yield_suspensions()
    }

    let mut invocation = vm.resume(one, options()).unwrap();
    let request = invocation.pending_host_call().unwrap().id();
    let one = invocation.save().unwrap();
    let mut invocation = vm.resume(two, options()).unwrap();
    assert!(matches!(
        invocation.host_request(request),
        Err(RequestError::OtherComputation)
    ));
    drop(invocation);

    // Another VM, whose computation is numbered and admitted the same way.
    let (program, asks) = asks();
    let mut other = Vm::new(Answers, Limits::default());
    let asks = FuncRef::new(other.install(program).unwrap(), asks);
    let elsewhere = save_asking(&mut other, asks, "elsewhere");
    let mut invocation = other.resume(elsewhere, options()).unwrap();
    assert!(matches!(
        invocation.host_request(request),
        Err(RequestError::OtherComputation)
    ));
    drop(invocation);

    let mut invocation = vm.start(suspends, &[], options()).unwrap();
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::AwaitingHandler
    );
    let suspension = invocation.pending_suspension().unwrap().id();
    assert!(matches!(
        invocation.host_request(suspension),
        Err(RequestError::OtherKind)
    ));
    let waiting = invocation.save().unwrap();
    let mut invocation = vm.resume(waiting, options()).unwrap();
    invocation
        .suspension(suspension)
        .unwrap()
        .resume(vec![Value::I64(41)])
        .unwrap();
    assert!(matches!(
        invocation.suspension(suspension),
        Err(RequestError::NotWaiting)
    ));
    assert_eq!(invocation.resume(u64::MAX).unwrap(), RunStatus::Complete);
    assert_eq!(invocation.result().unwrap().unwrap(), [Value::I64(42)]);
    drop(invocation);
    assert_eq!(finish_asking(&mut vm, one, 7), ("one".into(), 7));
}

#[test]
fn a_request_a_callback_returned_to_keeps_its_id() {
    let (program, entry, spin) = calls_back();
    let mut vm = Vm::new(Answers, Limits::default());
    let unit = vm.install(program).unwrap();
    fn options<'a>() -> InvocationOptions<'a> {
        InvocationOptions::default().yield_host_calls()
    }
    let mut invocation = vm.start(FuncRef::new(unit, entry), &[], options()).unwrap();
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::AwaitingHost
    );
    let request = invocation.pending_host_call().unwrap().id();
    invocation
        .host_request(request)
        .unwrap()
        .call_guest(Value::Func(FuncRef::new(unit, spin)), vec![])
        .unwrap();
    assert!(
        matches!(
            invocation.host_request(request),
            Err(RequestError::NotWaiting)
        ),
        "it waits for its callback"
    );
    assert_eq!(invocation.resume(1).unwrap(), RunStatus::Suspended);
    let id = invocation.save().unwrap();
    let mut invocation = vm.resume(id, options()).unwrap();
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::CallbackReturned
    );
    let mut pending = invocation.host_request(request).unwrap();
    assert_eq!(pending.take_callback_results().unwrap(), [Value::I64(4)]);
    pending.complete(&mut [Value::I64(8)], Ok(0)).unwrap();
    assert_eq!(invocation.resume(u64::MAX).unwrap(), RunStatus::Complete);
}

#[test]
fn ids_from_a_dropped_vm_name_nothing_in_a_later_one() {
    let (program, entry) = asks();
    fn options<'a>() -> InvocationOptions<'a> {
        InvocationOptions::default().yield_host_calls()
    }
    let (old_computation, old_request) = {
        let mut vm = Vm::new(Answers, Limits::default());
        let entry = FuncRef::new(vm.install(Arc::clone(&program)).unwrap(), entry);
        let id = save_asking(&mut vm, entry, "old");
        let mut invocation = vm.resume(id, options()).unwrap();
        let request = invocation.pending_host_call().unwrap().id();
        (invocation.save().ok().unwrap(), request)
    };
    // The same program, numbered and admitted the same way, in a VM made after the first one
    // was dropped.
    let mut vm = Vm::new(Answers, Limits::default());
    let entry = FuncRef::new(vm.install(program).unwrap(), entry);
    let id = save_asking(&mut vm, entry, "new");
    assert!(matches!(
        vm.discard(old_computation),
        Err(ResumeError::ForeignVm)
    ));
    assert!(matches!(
        vm.inspect(old_computation),
        Err(ResumeError::ForeignVm)
    ));
    assert!(matches!(
        vm.resume(old_computation, options()),
        Err(ResumeError::ForeignVm)
    ));
    let mut invocation = vm.resume(id, options()).unwrap();
    assert!(matches!(
        invocation.host_request(old_request),
        Err(RequestError::OtherComputation)
    ));
    let request = invocation.pending_host_call().unwrap().id();
    assert_ne!(request, old_request);
    drop(invocation);
}

/// `entry()` asks the host; `inner()` asks too, for the host to call back while it waits.
fn asks_from_callbacks() -> (Arc<VerifiedProgram>, FuncId, FuncId) {
    let mut b = ProgramBuilder::new();
    let ask = b.host_sig_for(
        "ask",
        HostSig {
            args: vec![],
            rets: vec![ValueType::I64],
        },
    );
    let entry = b.declare_function(sig(&[], &[ValueType::I64])).unwrap();
    let inner = b.declare_function(sig(&[], &[ValueType::I64])).unwrap();
    for function in [entry, inner] {
        let mut a = b.function_asm(function).unwrap();
        let n = a.alloc_register(ValueType::I64);
        a.host_call_sig_id(Reg::EFFECT, ask, Reg::EFFECT, &[], &[n])
            .ret(Reg::EFFECT, &[n]);
        b.define_function(function, a).unwrap();
    }
    (Arc::new(b.build_verified().unwrap()), entry, inner)
}

#[test]
fn request_ids_of_a_request_and_its_callbacks_request_survive_a_save() {
    let (program, entry, inner) = asks_from_callbacks();
    let mut vm = Vm::new(Answers, Limits::default());
    let unit = vm.install(program).unwrap();
    fn options<'a>() -> InvocationOptions<'a> {
        InvocationOptions::default().yield_host_calls()
    }
    let mut invocation = vm.start(FuncRef::new(unit, entry), &[], options()).unwrap();
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::AwaitingHost
    );
    let outer = invocation.pending_host_call().unwrap().id();
    invocation
        .host_request(outer)
        .unwrap()
        .call_guest(Value::Func(FuncRef::new(unit, inner)), vec![])
        .unwrap();
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::AwaitingHost
    );
    let nested = invocation.pending_host_call().unwrap().id();
    assert_ne!(nested, outer);
    let id = invocation.save().ok().unwrap();

    let mut invocation = vm.resume(id, options()).unwrap();
    assert!(matches!(
        invocation.host_request(outer),
        Err(RequestError::NotWaiting)
    ));
    invocation
        .host_request(nested)
        .unwrap()
        .complete(&mut [Value::I64(4)], Ok(0))
        .unwrap();
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::CallbackReturned
    );
    let id = invocation.save().ok().unwrap();
    let mut invocation = vm.resume(id, options()).unwrap();
    assert!(matches!(
        invocation.host_request(nested),
        Err(RequestError::NotWaiting)
    ));
    let mut request = invocation.host_request(outer).unwrap();
    assert_eq!(request.take_callback_results().unwrap(), [Value::I64(4)]);
    request.complete(&mut [Value::I64(9)], Ok(0)).unwrap();
    assert_eq!(invocation.resume(u64::MAX).unwrap(), RunStatus::Complete);
    assert_eq!(invocation.result().unwrap().unwrap(), [Value::I64(9)]);
}

#[test]
fn a_saved_computation_is_inspected_as_it_will_resume() {
    let (program, entry) = nested();
    let mut vm = Vm::new(Answers, Limits::default());
    let entry = FuncRef::new(vm.install(program).unwrap(), entry);
    let mut invocation = vm
        .start(entry, &[], InvocationOptions::default().yield_host_calls())
        .unwrap();
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::AwaitingHost
    );
    let live: Vec<_> = invocation.inspect().frames().collect();
    let id = invocation.save().unwrap();

    let saved: Vec<_> = vm.inspect(id).unwrap().frames().collect();
    assert_eq!(saved, live, "same frames, locations and activation IDs");
    assert!(saved.iter().all(|frame| frame.waiting_for_return));
    let mut invocation = vm
        .resume(id, InvocationOptions::default().yield_host_calls())
        .unwrap();
    let resumed: Vec<_> = invocation.inspect().frames().collect();
    assert_eq!(resumed, saved);
    invocation
        .pending_host_call()
        .unwrap()
        .complete(&mut [Value::I64(0)], Ok(0))
        .unwrap();
    assert_eq!(invocation.resume(u64::MAX).unwrap(), RunStatus::Complete);
    drop(invocation);
    assert!(matches!(vm.inspect(id), Err(ResumeError::Unknown)));
}

#[test]
fn a_waiting_request_inspects_another_saved_computation() {
    let (program, entry) = asks();
    let mut vm = Vm::new(Answers, Limits::default());
    let entry = FuncRef::new(vm.install(program).unwrap(), entry);
    let other = save_asking(&mut vm, entry, "other");
    let gone = save_asking(&mut vm, entry, "gone");
    vm.discard(gone).unwrap();
    let mut elsewhere = Vm::new(Answers, Limits::default());
    let (program, asks) = asks();
    let asks = FuncRef::new(elsewhere.install(program).unwrap(), asks);
    let foreign = save_asking(&mut elsewhere, asks, "foreign");

    let args = [Value::Str("mine".into())];
    let mut invocation = vm
        .start(
            entry,
            &args,
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::AwaitingHost
    );
    let request = invocation.pending_host_call().unwrap();
    let mut inspection = request.inspect_saved(other).unwrap();
    let frame = inspection.frames().next().unwrap();
    assert_ne!(
        frame.activation,
        request.inspect().frames().next().unwrap().activation,
        "another computation's activation"
    );
    let text = inspection.register(&frame.activation, Reg(1)).unwrap();
    assert!(
        matches!(text.value, Some(ValueRef::Str("other"))),
        "{text:?}"
    );
    assert!(matches!(
        request.inspect_saved(gone),
        Err(ResumeError::Unknown)
    ));
    assert!(matches!(
        request.inspect_saved(foreign),
        Err(ResumeError::ForeignVm)
    ));
    request.complete(&mut [Value::I64(3)], Ok(0)).unwrap();
    assert_eq!(invocation.resume(u64::MAX).unwrap(), RunStatus::Complete);
    assert!(invocation.inspect_saved(other).is_ok());
    drop(invocation);
    assert_eq!(finish_asking(&mut vm, other, 4), ("other".into(), 4));
}

#[test]
fn a_spawned_computation_inspects_the_same_before_and_after_its_first_resume() {
    let (program, entry) = nested();
    let mut vm = Vm::new(Answers, Limits::default());
    let entry = FuncRef::new(vm.install(program).unwrap(), entry);
    let id = vm.spawn(entry, vec![]).unwrap();
    let before: Vec<_> = vm.inspect(id).unwrap().frames().collect();
    assert_eq!(before.len(), 1, "its entry frame");
    let invocation = vm
        .resume(id, InvocationOptions::default().yield_host_calls())
        .unwrap();
    let after: Vec<_> = invocation.inspect().frames().collect();
    assert_eq!(after, before, "same frame, location and activation ID");
}

#[test]
fn a_saved_computation_is_inspected_where_a_collection_moved_its_aggregates() {
    let (program, entry) = asks();
    let mut vm = Vm::new(Answers, Limits::default());
    let entry = FuncRef::new(vm.install(program).unwrap(), entry);
    // Garbage below the computation's box, so the collection moves the box.
    vm.aggregates_mut().tuple_new(vec![Value::I64(0)]).unwrap();
    let id = save_asking(&mut vm, entry, "moved");
    let boxed = |vm: &Vm<Answers>| {
        let mut inspection = vm.inspect(id).unwrap();
        let frame = inspection.frames().next().unwrap();
        let Some(ValueRef::Agg(handle)) = inspection
            .register(&frame.activation, Reg(2))
            .unwrap()
            .value
        else {
            panic!("the boxed text");
        };
        handle
    };
    let before = boxed(&vm);
    let report = vm.collect().unwrap();
    assert_eq!(report.aggregates.live, 1, "only the box survives");
    let after = boxed(&vm);
    assert_ne!(after, before, "the box moved");
    assert_eq!(
        vm.aggregates().tuple_get(after, 0).unwrap(),
        Value::Str("moved".into())
    );
    assert_eq!(finish_asking(&mut vm, id, 2), ("moved".into(), 2));
}
