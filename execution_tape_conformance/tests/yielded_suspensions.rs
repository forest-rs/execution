// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Suspensions that no guest handler handles, yielded to the embedding.

use execution_tape::{
    asm::{Asm, Catch, ContractRef, FunctionSig, ProgramBuilder},
    contract::{Contract, ContractPosition},
    host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef},
    program::{CallSigId, Program, TagId, ValueType},
    recording::{RecordLimits, RecordedAnswer, RecordedValue, Recorder, ReplayOutcome},
    trace::{ScopeKind, TraceLocation, TraceMask, TraceOutcome, TraceSink},
    value::{FuncId, FuncRef, Value},
    verifier::VerifiedProgram,
    vm::PendingSuspension,
    vm::{Invocation, InvocationOptions, Limits, RunStatus, Trap, Vm},
};
use std::sync::Arc;

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
        panic!("fixture host calls are yielded")
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
    ask: TagId,
    stop: TagId,
    leaf: FuncId,
    body: FuncId,
    guarded: FuncId,
    picker: FuncId,
    outer: FuncId,
    callback: FuncId,
    waiter: FuncId,
}

/// Emits `resume sig cont(args) -> rets` protected by `clauses`.
fn protected_resume(
    asm: &mut Asm,
    clauses: &[Catch],
    call_sig: CallSigId,
    cont: u32,
    args: &[u32],
    rets: &[u32],
) {
    asm.begin_region(clauses);
    asm.resume(0, call_sig, cont, 0, args, rets);
    asm.end_region();
}

/// A condition-shaped program. `ask(x) -> i64` is a resumable condition no guest code
/// handles; `stop(code)` is an exception the guest catches in `guarded`.
///
/// - `leaf(x)` asks about `x` and returns the answer plus one.
/// - `body(x)` returns `leaf(x) + leaf(leaf(x))`: three conditions in one run.
/// - `guarded(x)` asks inside a region catching `stop(code)`, returning `code + 1000`.
/// - `picker()` suspends `pick() -> (i64,)`, whose result has a refined tuple contract.
/// - `outer(x)` runs `logged(x)` as a continuation handling only `log`: `logged` asks, which
///   passes through `outer`'s `resume` to the embedding, then logs the answer to `outer`.
/// - `waiter()` waits on the host, which calls back into `callback()`, which asks.
fn fixture() -> Fixture {
    let mut b = ProgramBuilder::new();
    let ask = b
        .declare_tag_with_results("ask", &[ValueType::I64.into()], &[ValueType::I64.into()])
        .unwrap();
    let stop = b.declare_tag("stop", &[ValueType::I64.into()]).unwrap();
    let log = b.declare_tag("log", &[ValueType::I64.into()]).unwrap();
    let int = b.contract(Contract::Kind(ValueType::I64)).unwrap();
    let single = b.contract(Contract::Tuple(&[Some(int)])).unwrap();
    let pick = b
        .declare_tag_with_results("pick", &[], &[ContractRef::Id(single)])
        .unwrap();
    let one_to_one = sig(&[ValueType::I64], &[ValueType::I64]);

    let leaf = b.declare_function(&one_to_one).unwrap();
    let mut asm = b.function_asm(leaf).unwrap();
    let x = b.argument(leaf, 0).unwrap();
    let (answer, one) = (
        asm.alloc_register(ValueType::I64),
        asm.alloc_register(ValueType::I64),
    );
    asm.suspend(0, ask, 0, &[x], &[answer]);
    asm.const_i64(one, 1);
    asm.i64_add(answer, answer, one);
    asm.ret(0, &[answer]);
    b.define_function(leaf, asm).unwrap();

    let body = b.declare_function(&one_to_one).unwrap();
    let mut asm = b.function_asm(body).unwrap();
    let x = b.argument(body, 0).unwrap();
    let [first] = b.emit_call(&mut asm, leaf, &[x]).unwrap();
    let [inner] = b.emit_call(&mut asm, leaf, &[first]).unwrap();
    let [second] = b.emit_call(&mut asm, leaf, &[inner]).unwrap();
    asm.i64_add(first, first, second);
    asm.ret(0, &[first]);
    b.define_function(body, asm).unwrap();

    let guarded = b.declare_function(&one_to_one).unwrap();
    let mut asm = b.function_asm(guarded).unwrap();
    let x = b.argument(guarded, 0).unwrap();
    let (answer, code, thousand) = (
        asm.alloc_register(ValueType::I64),
        asm.alloc_register(ValueType::I64),
        asm.alloc_register(ValueType::I64),
    );
    let stopped = asm.block(&[code]);
    asm.begin_region(&[Catch::Tag(stop, stopped)]);
    asm.suspend(0, ask, 0, &[x], &[answer]);
    asm.end_region();
    asm.ret(0, &[answer]);
    asm.place(stopped).unwrap();
    asm.const_i64(thousand, 1000);
    asm.i64_add(code, code, thousand);
    asm.ret(0, &[code]);
    b.define_function(guarded, asm).unwrap();

    let picker = b.declare_function(sig(&[], &[ValueType::I64])).unwrap();
    let mut asm = b.function_asm(picker).unwrap();
    let (tuple, first) = (
        asm.alloc_register(ValueType::Agg),
        asm.alloc_register(ValueType::I64),
    );
    asm.suspend(0, pick, 0, &[], &[tuple]);
    asm.tuple_get(first, tuple, 0);
    asm.ret(0, &[first]);
    b.define_function(picker, asm).unwrap();

    let logged = b.declare_function(&one_to_one).unwrap();
    let mut asm = b.function_asm(logged).unwrap();
    let x = b.argument(logged, 0).unwrap();
    let answer = asm.alloc_register(ValueType::I64);
    asm.suspend(0, ask, 0, &[x], &[answer]);
    asm.suspend(0, log, 0, &[answer], &[]);
    asm.ret(0, &[answer]);
    b.define_function(logged, asm).unwrap();

    let i64_to_i64 = b.call_sig(&one_to_one).unwrap();
    let unit_to_i64 = b.call_sig(sig(&[], &[ValueType::I64])).unwrap();
    let outer = b.declare_function(&one_to_one).unwrap();
    let mut asm = b.function_asm(outer).unwrap();
    let x = b.argument(outer, 0).unwrap();
    let f = asm.alloc_register(ValueType::Func);
    let cont = asm.alloc_register(ValueType::Cont);
    let out = asm.alloc_register(ValueType::I64);
    let (entry, rest) = (
        asm.alloc_register(ValueType::I64),
        asm.alloc_register(ValueType::Cont),
    );
    let logged_block = asm.block(&[entry, rest]);
    asm.const_func(f, logged);
    asm.cont_new(cont, i64_to_i64, f);
    protected_resume(
        &mut asm,
        &[Catch::On(log, logged_block)],
        i64_to_i64,
        cont,
        &[x],
        &[out],
    );
    asm.ret(0, &[out]);
    asm.place(logged_block).unwrap();
    protected_resume(
        &mut asm,
        &[Catch::On(log, logged_block)],
        unit_to_i64,
        rest,
        &[],
        &[out],
    );
    asm.ret(0, &[out]);
    b.define_function(outer, asm).unwrap();

    let wait = b.host_sig_for(
        "wait",
        HostSig {
            args: vec![],
            rets: vec![],
        },
    );
    let callback = b.declare_function(sig(&[], &[ValueType::I64])).unwrap();
    let mut asm = b.function_asm(callback).unwrap();
    let (x, answer) = (
        asm.alloc_register(ValueType::I64),
        asm.alloc_register(ValueType::I64),
    );
    asm.const_i64(x, 0);
    asm.suspend(0, ask, 0, &[x], &[answer]);
    asm.ret(0, &[answer]);
    b.define_function(callback, asm).unwrap();
    let waiter = b.declare_function(sig(&[], &[])).unwrap();
    let mut asm = b.function_asm(waiter).unwrap();
    asm.host_call(0, wait, 0, &[], &[]);
    asm.ret(0, &[]);
    b.define_function(waiter, asm).unwrap();

    Fixture {
        program: Arc::new(b.build_verified().unwrap()),
        ask,
        stop,
        leaf,
        body,
        guarded,
        picker,
        outer,
        callback,
        waiter,
    }
}

fn yielding<'a>() -> InvocationOptions<'a> {
    InvocationOptions::default().yield_suspensions()
}

fn start<'a>(
    vm: &'a mut Vm<NoHost>,
    unit: execution_tape::value::UnitId,
    func: FuncId,
    args: &'a [Value],
    options: InvocationOptions<'a>,
) -> Invocation<'a, NoHost> {
    vm.start(FuncRef::new(unit, func), args, options).unwrap()
}

#[test]
fn yielded_suspensions_resume_with_values_over_many_turns() {
    let fixture = fixture();
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(Arc::clone(&fixture.program)).unwrap();
    let tag = vm.units().tag(unit, fixture.ask).unwrap();
    let args = [Value::I64(5)];
    let mut run = start(&mut vm, unit, fixture.body, &args, yielding());
    let mut asked = Vec::new();
    loop {
        match run.resume(u64::MAX).unwrap() {
            RunStatus::Complete => break,
            RunStatus::AwaitingHandler => {
                let pending = run.pending_suspension().unwrap();
                assert_eq!(pending.tag(), tag);
                assert_eq!(pending.local_tag(), fixture.ask);
                assert_eq!(pending.tag_name(), "ask");
                assert_eq!(pending.payload_types(), [ValueType::I64]);
                assert_eq!(pending.result_types(), [ValueType::I64]);
                assert_eq!(pending.location().func, fixture.leaf);
                assert_eq!(pending.location().unit, unit);
                let [ValueRef::I64(x)] = pending.payload().collect::<Vec<_>>()[..] else {
                    panic!("one i64 payload value");
                };
                asked.push(x);
                // The embedding answers each condition with ten times its value.
                pending.resume(vec![Value::I64(x * 10)]).unwrap();
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    // leaf(5) = 51, leaf(51) = 511, leaf(511) = 5111; body = 51 + 5111.
    assert_eq!(asked, [5, 51, 511]);
    assert_eq!(run.take_outputs().unwrap(), [Value::I64(5162)]);
}

#[test]
fn without_opting_in_unhandled_suspensions_still_trap() {
    let fixture = fixture();
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(Arc::clone(&fixture.program)).unwrap();
    let error = vm
        .run(
            FuncRef::new(unit, fixture.body),
            &[Value::I64(5)],
            TraceMask::NONE,
            None,
        )
        .unwrap_err();
    assert!(
        matches!(error.trap, Trap::UnhandledSuspension { .. }),
        "{error:?}"
    );
    let args = [Value::I64(5)];
    let mut run = start(
        &mut vm,
        unit,
        fixture.body,
        &args,
        InvocationOptions::default().yield_host_calls(),
    );
    let error = run.resume(u64::MAX).unwrap_err();
    assert!(
        matches!(error.trap, Trap::UnhandledSuspension { .. }),
        "{error:?}"
    );
    assert!(run.pending_suspension().is_none());
}

#[test]
fn a_waiting_suspension_executes_nothing_and_shows_its_frames() {
    let fixture = fixture();
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(Arc::clone(&fixture.program)).unwrap();
    let args = [Value::I64(5)];
    let mut run = start(&mut vm, unit, fixture.body, &args, yielding());
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHandler);
    let fuel = run.remaining_fuel();
    // Repeated queries, dropped views and further quanta neither execute nor answer.
    assert!(run.pending_suspension().is_some());
    assert_eq!(run.resume(0).unwrap(), RunStatus::AwaitingHandler);
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHandler);
    assert_eq!(run.remaining_fuel(), fuel);

    let location = run.pending_suspension().unwrap().location();
    let frames: Vec<_> = run.inspect().frames().collect();
    assert_eq!(frames.len(), 2);
    // The leaf waits at its `suspend`; its caller waits at the call that entered it.
    assert_eq!(frames[0].location, location);
    assert!(frames[0].waiting_for_return && frames[1].waiting_for_return);
    assert_eq!(frames[0].caller.unwrap().func, fixture.body);
    let leaf = frames[0].activation.clone();
    {
        let pending = run.pending_suspension().unwrap();
        let mut inspection = pending.inspect();
        // The argument is initialized; the `suspend`'s result is not.
        let registers: Vec<_> = inspection.registers(&leaf).unwrap().collect();
        assert!(
            registers
                .iter()
                .any(|r| matches!(r.value, Some(ValueRef::I64(5))))
        );
        assert!(
            registers
                .iter()
                .filter(|r| r.storage_kind == ValueType::I64)
                .any(|r| r.value.is_none())
        );
    }
    run.pending_suspension()
        .unwrap()
        .resume(vec![Value::I64(7)])
        .unwrap();
    // The resumed leaf keeps its activation identity.
    assert_eq!(run.resume(1).unwrap(), RunStatus::Suspended);
    assert_eq!(run.inspect().frames().next().unwrap().activation, leaf);
}

#[test]
fn resume_throw_raises_at_the_suspend() {
    let fixture = fixture();
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(Arc::clone(&fixture.program)).unwrap();
    let stop = vm.units().tag(unit, fixture.stop).unwrap();

    // A region around the `suspend` catches the raised exception.
    let args = [Value::I64(5)];
    let mut run = start(&mut vm, unit, fixture.guarded, &args, yielding());
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHandler);
    run.pending_suspension()
        .unwrap()
        .resume_throw(stop, vec![Value::I64(3)])
        .unwrap();
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::Complete);
    assert_eq!(run.take_outputs().unwrap(), [Value::I64(1003)]);
    drop(run);

    // Nothing catches it in `body`: it leaves the invocation, originating at the `suspend`.
    let mut run = start(&mut vm, unit, fixture.body, &args, yielding());
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHandler);
    let location = run.pending_suspension().unwrap().location();
    let error = run
        .pending_suspension()
        .unwrap()
        .resume_throw(stop, vec![Value::I64(3)])
        .unwrap_err();
    let Trap::Uncaught(exception) = &error.trap else {
        panic!("{error:?}");
    };
    assert_eq!(exception.origin(), location);
    assert_eq!((error.func, error.pc), (location.func, location.pc));
    assert!(matches!(run.resume(1), Err(again) if matches!(again.trap, Trap::Uncaught(_))));
    drop(run);

    // A wrong payload traps at the `suspend` like an invalid host throw.
    let mut run = start(&mut vm, unit, fixture.guarded, &args, yielding());
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHandler);
    let error = run
        .pending_suspension()
        .unwrap()
        .resume_throw(stop, vec![])
        .unwrap_err();
    assert!(matches!(error.trap, Trap::ArityMismatch), "{error:?}");
}

#[test]
fn discard_ends_the_invocation_with_the_unhandled_trap() {
    let fixture = fixture();
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(Arc::clone(&fixture.program)).unwrap();
    let tag = vm.units().tag(unit, fixture.ask).unwrap();
    let args = [Value::I64(5)];
    let mut run = start(&mut vm, unit, fixture.body, &args, yielding());
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHandler);
    let location = run.pending_suspension().unwrap().location();
    let error = run.pending_suspension().unwrap().discard();
    assert!(
        matches!(error.trap, Trap::UnhandledSuspension { tag: t } if t == tag),
        "{error:?}"
    );
    assert_eq!(
        (error.unit, error.func, error.pc),
        (location.unit, location.func, location.pc)
    );
    // The invocation is finished: its frames are gone and nothing is pending.
    assert!(run.pending_suspension().is_none());
    assert_eq!(run.inspect().frames().len(), 0);
    assert!(
        matches!(run.result(), Some(Err(info)) if matches!(info.trap, Trap::UnhandledSuspension { .. }))
    );
    assert!(run.resume(u64::MAX).is_err());
}

#[test]
fn answers_are_checked_like_host_returns() {
    let fixture = fixture();
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(Arc::clone(&fixture.program)).unwrap();
    let args = [Value::I64(5)];
    let answer = |vm: &mut Vm<NoHost>, func: FuncId, values: Vec<Value>| {
        let mut run = start(vm, unit, func, &args, yielding());
        assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHandler);
        let location = run.pending_suspension().unwrap().location();
        let result = run
            .pending_suspension()
            .unwrap()
            .resume(values)
            .and_then(|()| match run.resume(u64::MAX)? {
                RunStatus::Complete => Ok(run.take_outputs().unwrap()),
                other => panic!("unexpected {other:?}"),
            });
        (result, location)
    };
    for (values, expected) in [
        (vec![], "ArityMismatch"),
        (vec![Value::I64(1), Value::I64(2)], "ArityMismatch"),
        (vec![Value::Bool(true)], "TypeMismatch"),
    ] {
        let (result, location) = answer(&mut vm, fixture.leaf, values);
        let error = result.unwrap_err();
        assert!(
            format!("{:?}", error.trap).starts_with(expected),
            "{error:?}"
        );
        // Traps are located at the `suspend`.
        assert_eq!((error.func, error.pc), (location.func, location.pc));
    }
}

/// A value of the right kind that violates the tag's refined result contract traps with the
/// resumption's contract position. The answers are handles already in the VM's heap.
#[test]
fn answers_satisfy_the_tags_refined_result_contracts() {
    let fixture = fixture();
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(Arc::clone(&fixture.program)).unwrap();
    let flagged = vm
        .aggregates_mut()
        .tuple_new(vec![Value::Bool(true)])
        .unwrap();
    let numbered = vm.aggregates_mut().tuple_new(vec![Value::I64(9)]).unwrap();
    for (handle, expected) in [(flagged, None), (numbered, Some(9))] {
        let args: [Value; 0] = [];
        let mut run = start(&mut vm, unit, fixture.picker, &args, yielding());
        assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHandler);
        let result = run
            .pending_suspension()
            .unwrap()
            .resume(vec![Value::Agg(handle)]);
        match expected {
            Some(value) => {
                result.unwrap();
                assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::Complete);
                assert_eq!(run.take_outputs().unwrap(), [Value::I64(value)]);
            }
            None => {
                let error = result.unwrap_err();
                assert!(
                    matches!(
                        error.trap,
                        Trap::ContractMismatch {
                            position: ContractPosition::Resumption { index: 0, .. },
                            ..
                        }
                    ),
                    "{error:?}"
                );
            }
        }
    }
}

#[test]
fn a_suspension_passes_guest_resumes_without_a_clause_before_it_yields() {
    let fixture = fixture();
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(Arc::clone(&fixture.program)).unwrap();
    let args = [Value::I64(5)];
    let mut run = start(&mut vm, unit, fixture.outer, &args, yielding());
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHandler);
    // `outer` waits at its `resume`, `logged` at its `suspend`.
    assert_eq!(run.inspect().frames().len(), 2);
    run.pending_suspension()
        .unwrap()
        .resume(vec![Value::I64(42)])
        .unwrap();
    // `log` is handled by `outer`, which resumes `logged` to return the answer.
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::Complete);
    assert_eq!(run.take_outputs().unwrap(), [Value::I64(42)]);
}

#[test]
fn a_suspension_inside_a_guest_callback_still_traps() {
    let fixture = fixture();
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(Arc::clone(&fixture.program)).unwrap();
    let args: [Value; 0] = [];
    let mut run = start(
        &mut vm,
        unit,
        fixture.waiter,
        &args,
        yielding().yield_host_calls(),
    );
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    run.pending_host_call()
        .unwrap()
        .call_guest(Value::Func(FuncRef::new(unit, fixture.callback)), vec![])
        .unwrap();
    let error = run.resume(u64::MAX).unwrap_err();
    assert!(
        matches!(error.trap, Trap::SuspensionAcrossHost { .. }),
        "{error:?}"
    );
}

/// Records scope transitions as `(depth, outcome)` and checks that they nest.
#[derive(Default)]
struct Scopes {
    open: Vec<(usize, Option<TraceLocation>)>,
    log: Vec<String>,
    end: Option<String>,
}

impl TraceSink for Scopes {
    fn mask(&self) -> TraceMask {
        TraceMask::CALL | TraceMask::RUN | TraceMask::INSTR
    }
    fn scope_enter(
        &mut self,
        _: &Program,
        kind: ScopeKind,
        depth: usize,
        _: TraceLocation,
        _: Option<u64>,
    ) {
        let ScopeKind::CallFrame { caller, .. } = kind else {
            return;
        };
        assert_eq!(depth, self.open.len() + 1, "scopes enter at the next depth");
        self.open.push((depth, caller));
        self.log.push(format!("enter {depth}"));
    }
    fn scope_exit(
        &mut self,
        _: &Program,
        kind: ScopeKind,
        depth: usize,
        _: TraceLocation,
        _: Option<u64>,
        outcome: TraceOutcome<'_>,
    ) {
        let ScopeKind::CallFrame { caller, .. } = kind else {
            return;
        };
        assert_eq!(self.open.pop(), Some((depth, caller)), "innermost first");
        self.log.push(format!("exit {depth} {outcome:?}"));
    }
    fn run_end(&mut self, _: &Program, outcome: TraceOutcome<'_>) {
        self.end = Some(format!("{outcome:?}"));
    }
}

#[test]
fn yielded_activations_close_their_scopes_and_reenter_when_answered() {
    let fixture = fixture();
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(Arc::clone(&fixture.program)).unwrap();
    let args = [Value::I64(5)];
    let mut scopes = Scopes::default();
    {
        let mask = scopes.mask();
        let mut run = start(
            &mut vm,
            unit,
            fixture.guarded,
            &args,
            yielding().trace(mask, &mut scopes),
        );
        assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHandler);
        run.pending_suspension()
            .unwrap()
            .resume(vec![Value::I64(8)])
            .unwrap();
        assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::Complete);
    }
    assert_eq!(
        scopes.log,
        ["enter 1", "exit 1 Suspended", "enter 1", "exit 1 Ok"]
    );
    assert_eq!(scopes.end.as_deref(), Some("Ok"));

    // Dropping an invocation while its suspension waits cancels it; no scope is left open.
    let mut scopes = Scopes::default();
    {
        let mask = scopes.mask();
        let mut run = start(
            &mut vm,
            unit,
            fixture.body,
            &args,
            yielding().trace(mask, &mut scopes),
        );
        assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHandler);
    }
    assert!(scopes.open.is_empty());
    assert_eq!(
        scopes.log,
        ["enter 1", "enter 2", "exit 2 Suspended", "exit 1 Suspended"]
    );
    assert_eq!(scopes.end.as_deref(), Some("Cancelled"));
    // The VM is free again; the cancelled invocation's frames were released.
    let error = vm
        .run(
            FuncRef::new(unit, fixture.leaf),
            &args,
            TraceMask::NONE,
            None,
        )
        .unwrap_err();
    assert!(
        matches!(error.trap, Trap::UnhandledSuspension { .. }),
        "{error:?}"
    );
}

/// Runs `func` yielding suspensions, answering each with `answers` in order, optionally one
/// instruction per quantum, and returns the recording with the live outcome.
fn record(
    fixture: &Fixture,
    func: FuncId,
    quantum: u64,
    answers: &[&Answering],
) -> (
    execution_tape::recording::Recording,
    Result<Vec<Value>, Trap>,
) {
    let mut recorder = Recorder::new(Arc::clone(&fixture.program), RecordLimits::default())
        .with_instructions(execution_tape::recording::InstructionCapture::default());
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(Arc::clone(&fixture.program)).unwrap();
    let args = [Value::I64(5)];
    let outcome = {
        let mask = recorder.mask();
        let mut run = start(
            &mut vm,
            unit,
            func,
            &args,
            yielding().trace(mask, &mut recorder),
        );
        let mut answers = answers.iter();
        loop {
            match run.resume(quantum) {
                Err(error) => break Err(error.trap),
                Ok(RunStatus::Complete) => break Ok(run.take_outputs().unwrap()),
                Ok(RunStatus::Suspended) => {}
                Ok(RunStatus::AwaitingHandler) => {
                    let mut pending = run.pending_suspension().unwrap();
                    let delivered = match answers.next().expect("an answer")(&mut pending) {
                        Answer::Resume(values) => pending.resume(values),
                        Answer::Throw(tag, payload) => pending.resume_throw(tag, payload),
                        Answer::Discard => Err(pending.discard()),
                    };
                    if let Err(error) = delivered {
                        break Err(error.trap);
                    }
                }
                Ok(other) => panic!("unexpected {other:?}"),
            }
        }
    };
    (recorder.finish(), outcome)
}

type Answering = dyn Fn(&mut PendingSuspension<'_, '_, NoHost>) -> Answer;

enum Answer {
    Resume(Vec<Value>),
    Throw(execution_tape::value::TagRef, Vec<Value>),
    Discard,
}

#[test]
fn yielded_suspensions_record_and_replay() {
    let fixture = fixture();
    let tenfold = |pending: &mut PendingSuspension<'_, '_, NoHost>| {
        let ValueRef::I64(x) = pending.payload().next().unwrap() else {
            panic!("i64 payload");
        };
        Answer::Resume(vec![Value::I64(x * 10)])
    };
    // An install while the suspension waits is repeated before its answer.
    let installing = |pending: &mut PendingSuspension<'_, '_, NoHost>| {
        let program = Arc::clone(pending.units().program(pending.location().unit).unwrap());
        pending.install(program).unwrap();
        Answer::Resume(vec![Value::I64(1)])
    };
    let stop = |pending: &mut PendingSuspension<'_, '_, NoHost>| {
        let unit = pending.location().unit;
        Answer::Throw(
            pending.units().tag(unit, TagId(1)).unwrap(),
            vec![Value::I64(3)],
        )
    };
    let discard = |_: &mut PendingSuspension<'_, '_, NoHost>| Answer::Discard;
    let wrong =
        |_: &mut PendingSuspension<'_, '_, NoHost>| Answer::Resume(vec![Value::Bool(false)]);

    for quantum in [1, u64::MAX] {
        let (recording, live) = record(
            &fixture,
            fixture.body,
            quantum,
            &[&tenfold, &installing, &tenfold],
        );
        // leaf(5) asks 5 and gets 50; leaf(51) gets 1; leaf(2) gets 20: 51 + 21.
        assert_eq!(live.unwrap(), [Value::I64(72)]);
        recording.status().unwrap();
        assert_eq!(
            recording.instruction_status(),
            Some(execution_tape::recording::InstructionStatus::Complete)
        );
        // Each `suspend` attempt completes with no guest successor: the embedding chose.
        let suspends: Vec<_> = recording
            .instructions()
            .filter(|step| step.location.func == fixture.leaf && step.next.is_none())
            .collect();
        assert_eq!(suspends.len(), 3);
        let suspensions: Vec<_> = recording.suspensions().collect();
        assert_eq!(suspensions.len(), 3);
        assert!(matches!(suspensions[0].payload, [RecordedValue::I64(5)]));
        assert!(matches!(
            suspensions[0].answer,
            Some(RecordedAnswer::Resumed([RecordedValue::I64(50)]))
        ));
        assert_eq!(suspensions[0].stack.clone().unwrap().count(), 2);
        assert_eq!(recording.units().len(), 2);
        assert!(suspensions[0].sequence < suspensions[1].sequence);
        match recording.replay() {
            Ok(ReplayOutcome::Returned(values)) => {
                assert_eq!(values.values(), [Value::I64(72)]);
            }
            other => panic!("replay diverged: {other:?}"),
        }
    }

    for (func, answer, expected) in [
        (fixture.guarded, &stop as &Answering, Ok(1003)),
        (fixture.body, &stop, Err("Uncaught")),
        (fixture.body, &discard, Err("UnhandledSuspension")),
        (fixture.body, &wrong, Err("TypeMismatch")),
        // `ask` passes `outer`'s `resume` and yields from inside the continuation.
        (fixture.outer, &tenfold, Ok(50)),
    ] {
        let (recording, live) = record(&fixture, func, u64::MAX, &[answer]);
        recording.status().unwrap();
        assert_eq!(
            recording.instruction_status(),
            Some(execution_tape::recording::InstructionStatus::Complete)
        );
        match (expected, live, recording.replay()) {
            (Ok(value), Ok(live), Ok(ReplayOutcome::Returned(values))) => {
                assert_eq!(live, [Value::I64(value)]);
                assert_eq!(values.values(), [Value::I64(value)]);
            }
            (Err(name), Err(live), Ok(ReplayOutcome::Trapped(info))) => {
                assert!(format!("{live:?}").starts_with(name), "{live:?}");
                assert!(format!("{:?}", info.trap).starts_with(name), "{info:?}");
            }
            (expected, live, replayed) => {
                panic!("expected {expected:?}, live {live:?}, replay {replayed:?}")
            }
        }
    }
}

#[test]
fn a_recording_cancelled_while_a_suspension_waits_is_incomplete() {
    let fixture = fixture();
    let mut recorder = Recorder::new(Arc::clone(&fixture.program), RecordLimits::default());
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(Arc::clone(&fixture.program)).unwrap();
    let args = [Value::I64(5)];
    {
        let mask = recorder.mask();
        let mut run = start(
            &mut vm,
            unit,
            fixture.body,
            &args,
            yielding().trace(mask, &mut recorder),
        );
        assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHandler);
    }
    let recording = recorder.finish();
    assert!(recording.status().is_err());
    let suspensions: Vec<_> = recording.suspensions().collect();
    assert_eq!(suspensions.len(), 1);
    assert!(suspensions[0].answer.is_none());
    assert!(recording.replay().is_err());
}

#[cfg(panic = "unwind")]
#[test]
fn an_answer_interrupted_by_an_observer_panic_is_terminal() {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    /// Panics when the first suspended activation re-enters.
    #[derive(Default)]
    struct PanicOnReentry {
        entered: usize,
    }
    impl TraceSink for PanicOnReentry {
        fn scope_enter(
            &mut self,
            _: &Program,
            _: ScopeKind,
            _: usize,
            _: TraceLocation,
            _: Option<u64>,
        ) {
            self.entered += 1;
            if self.entered == 3 {
                panic!("injected re-entry observer panic");
            }
        }
    }
    let fixture = fixture();
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(Arc::clone(&fixture.program)).unwrap();
    let args = [Value::I64(5)];
    let mut observer = PanicOnReentry::default();
    let mut run = start(
        &mut vm,
        unit,
        fixture.body,
        &args,
        yielding().trace(TraceMask::CALL, &mut observer),
    );
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHandler);
    let answered = catch_unwind(AssertUnwindSafe(|| {
        run.pending_suspension()
            .unwrap()
            .resume(vec![Value::I64(1)])
    }));
    assert!(answered.is_err());
    // The `suspend` never received its result, so the invocation cannot continue after it.
    assert!(run.pending_suspension().is_none());
    let error = run.resume(u64::MAX).unwrap_err();
    assert!(matches!(error.trap, Trap::HostCallAbandoned), "{error:?}");
    assert_eq!(error.func, fixture.leaf);
    assert!(matches!(run.resume(0), Err(again) if matches!(again.trap, Trap::HostCallAbandoned)));
}

#[test]
fn answers_holding_aggregates_record_and_replay() {
    let fixture = fixture();
    let mut recorder = Recorder::new(Arc::clone(&fixture.program), RecordLimits::default());
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(Arc::clone(&fixture.program)).unwrap();
    // A retained aggregate from before the invocation answers its `pick` suspension.
    let numbered = vm.aggregates_mut().tuple_new(vec![Value::I64(9)]).unwrap();
    let args: [Value; 0] = [];
    {
        let mask = recorder.mask();
        let mut run = start(
            &mut vm,
            unit,
            fixture.picker,
            &args,
            yielding().trace(mask, &mut recorder),
        );
        assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHandler);
        run.pending_suspension()
            .unwrap()
            .resume(vec![Value::Agg(numbered)])
            .unwrap();
        assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::Complete);
        assert_eq!(run.take_outputs().unwrap(), [Value::I64(9)]);
    }
    let recording = recorder.finish();
    recording.status().unwrap();
    let suspension = recording.suspensions().next().unwrap();
    let Some(RecordedAnswer::Resumed([RecordedValue::Agg(id)])) = suspension.answer else {
        panic!("aggregate answer: {suspension:?}");
    };
    assert!(matches!(
        recording.aggregate(*id).unwrap().values,
        [RecordedValue::I64(9)]
    ));
    match recording.replay() {
        Ok(ReplayOutcome::Returned(values)) => assert_eq!(values.values(), [Value::I64(9)]),
        other => panic!("replay diverged: {other:?}"),
    }
}

/// `tell(t)` suspends `told(t)` with no handler, where the payload must be a one-`i64` tuple.
fn teller() -> (Arc<VerifiedProgram>, FuncId) {
    let mut b = ProgramBuilder::new();
    let int = b.contract(Contract::Kind(ValueType::I64)).unwrap();
    let single = b.contract(Contract::Tuple(&[Some(int)])).unwrap();
    let told = b
        .declare_tag_with_results("told", &[ContractRef::Id(single)], &[])
        .unwrap();
    let tell = b.declare_function(sig(&[ValueType::Agg], &[])).unwrap();
    let mut asm = b.function_asm(tell).unwrap();
    let t = b.argument(tell, 0).unwrap();
    asm.suspend(0, told, 0, &[t], &[]);
    asm.ret(0, &[]);
    b.define_function(tell, asm).unwrap();
    (Arc::new(b.build_verified().unwrap()), tell)
}

#[test]
fn an_unhandled_suspension_checks_its_payload_whether_or_not_it_yields() {
    let (program, tell) = teller();
    for yields in [false, true] {
        let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
        let mut vm = Vm::new(NoHost, Limits::default());
        let unit = vm.install(Arc::clone(&program)).unwrap();
        let flagged = vm
            .aggregates_mut()
            .tuple_new(vec![Value::Bool(true)])
            .unwrap();
        let args = [Value::Agg(flagged)];
        let mask = recorder.mask();
        let options = InvocationOptions::default().trace(mask, &mut recorder);
        let options = if yields {
            options.yield_suspensions()
        } else {
            options
        };
        let mut run = start(&mut vm, unit, tell, &args, options);
        let error = run.resume(u64::MAX).unwrap_err();
        assert!(
            matches!(
                error.trap,
                Trap::ContractMismatch {
                    position: ContractPosition::Payload { index: 0, .. },
                    ..
                }
            ),
            "{error:?}"
        );
        drop(run);
        // Replay reproduces the trap without knowing whether the original opted in.
        let recording = recorder.finish();
        recording.status().unwrap();
        assert!(
            matches!(&recording.replay(), Ok(ReplayOutcome::Trapped(info))
                if matches!(info.trap, Trap::ContractMismatch { .. })),
            "{:?}",
            recording.replay()
        );
    }
}

#[cfg(panic = "unwind")]
#[test]
fn a_yield_interrupted_by_an_observer_panic_is_terminal() {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    /// Panics when the yielded activation closes its scope.
    struct PanicOnSuspension;
    impl TraceSink for PanicOnSuspension {
        fn scope_exit(
            &mut self,
            _: &Program,
            _: ScopeKind,
            _: usize,
            _: TraceLocation,
            _: Option<u64>,
            outcome: TraceOutcome<'_>,
        ) {
            if matches!(outcome, TraceOutcome::Suspended) {
                panic!("injected suspension observer panic");
            }
        }
    }
    let fixture = fixture();
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(Arc::clone(&fixture.program)).unwrap();
    let args = [Value::I64(5)];
    let mut observer = PanicOnSuspension;
    let mut run = start(
        &mut vm,
        unit,
        fixture.leaf,
        &args,
        yielding().trace(TraceMask::CALL, &mut observer),
    );
    assert!(catch_unwind(AssertUnwindSafe(|| run.resume(u64::MAX))).is_err());
    // The `suspend` was left part way: the invocation must not continue past it.
    assert!(run.pending_suspension().is_none());
    let error = run.resume(u64::MAX).unwrap_err();
    assert!(matches!(error.trap, Trap::HostCallAbandoned), "{error:?}");
}
