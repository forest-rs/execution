// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Stack switching: `cont.new`, `resume`, `suspend` and `on` clauses.

use execution_tape::{
    asm::{Asm, Catch, FunctionSig, ProgramBuilder},
    host::{Host, HostContext, HostError, SigHash, ValueRef},
    program::{CallSigId, TagId, ValueType},
    value::{FuncId, FuncRef, Value},
    verifier::VerifiedProgram,
    vm::{Limits, Trap, Vm},
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
        panic!("fixture makes no host calls")
    }
}

fn sig(args: &[ValueType], rets: &[ValueType]) -> FunctionSig {
    FunctionSig {
        arg_types: args.to_vec(),
        ret_types: rets.to_vec(),
    }
}

/// `gen(n)` suspends `yield(i)` for `i` in `0..n`, then returns.
fn emit_generator(b: &mut ProgramBuilder, yield_tag: TagId) -> FuncId {
    let generator = b.declare_function(sig(&[ValueType::I64], &[])).unwrap();
    let mut asm = b.function_asm(generator).unwrap();
    let n = b.argument(generator, 0).unwrap();
    let i = asm.alloc_register(ValueType::I64);
    let one = asm.alloc_register(ValueType::I64);
    let done = asm.alloc_register(ValueType::Bool);
    let (check, body, exit) = (asm.label(), asm.label(), asm.label());
    asm.const_i64(i, 0);
    asm.const_i64(one, 1);
    asm.jmp(check);
    asm.place(check).unwrap();
    asm.i64_ge(done, i, n);
    asm.br(done, exit, body);
    asm.place(body).unwrap();
    asm.suspend(0, yield_tag, 0, &[i], &[]);
    asm.i64_add(i, i, one);
    asm.jmp(check);
    asm.place(exit).unwrap();
    asm.ret(0, &[]);
    b.define_function(generator, asm).unwrap();
    generator
}

/// Emits `resume sig cont(args)` protected by `clauses`.
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

struct Generators {
    program: Arc<VerifiedProgram>,
    sum: FuncId,
}

/// `sum(n)` drives `gen(n)` to completion, adding every yielded value.
fn generators() -> Generators {
    let mut b = ProgramBuilder::new();
    let yield_tag = b.declare_tag("yield", &[ValueType::I64.into()]).unwrap();
    let generator = emit_generator(&mut b, yield_tag);
    let start = b.call_sig(sig(&[ValueType::I64], &[])).unwrap();
    let next = b.call_sig(sig(&[], &[])).unwrap();
    let sum = b
        .declare_function(sig(&[ValueType::I64], &[ValueType::I64]))
        .unwrap();

    let mut asm = b.function_asm(sum).unwrap();
    let n = b.argument(sum, 0).unwrap();
    let f = asm.alloc_register(ValueType::Func);
    let cont = asm.alloc_register(ValueType::Cont);
    let total = asm.alloc_register(ValueType::I64);
    let value = asm.alloc_register(ValueType::I64);
    let rest = asm.alloc_register(ValueType::Cont);
    let yielded = asm.block(&[value, rest]);
    asm.const_func(f, generator);
    asm.cont_new(cont, start, f);
    asm.const_i64(total, 0);
    protected_resume(
        &mut asm,
        &[Catch::On(yield_tag, yielded)],
        start,
        cont,
        &[n],
        &[],
    );
    asm.ret(0, &[total]);
    asm.place(yielded).unwrap();
    asm.i64_add(total, total, value);
    protected_resume(
        &mut asm,
        &[Catch::On(yield_tag, yielded)],
        next,
        rest,
        &[],
        &[],
    );
    asm.ret(0, &[total]);
    b.define_function(sum, asm).unwrap();

    Generators {
        program: Arc::new(b.build_verified().unwrap()),
        sum,
    }
}

fn run(
    program: &Arc<VerifiedProgram>,
    func: FuncId,
    args: &[Value],
    limits: Limits,
) -> Result<Vec<Value>, Trap> {
    let mut vm = Vm::new(NoHost, limits);
    let unit = vm.install(Arc::clone(program)).unwrap();
    vm.run(
        FuncRef::new(unit, func),
        args,
        execution_tape::trace::TraceMask::NONE,
        None,
    )
    .map_err(|info| info.trap)
}

#[test]
fn a_generator_suspends_each_value_to_its_resumer() {
    let fixture = generators();
    for (n, expected) in [(0, 0), (1, 0), (5, 10), (100, 4950)] {
        let result = run(
            &fixture.program,
            fixture.sum,
            &[Value::I64(n)],
            Limits::default(),
        );
        assert_eq!(result.unwrap(), vec![Value::I64(expected)], "sum of 0..{n}");
    }
}

#[test]
fn disassembly_names_continuation_operands() {
    let fixture = generators();
    let text = execution_tape::disasm::disassemble(fixture.program.program()).to_string();
    for line in [
        "suspend eff_out=r0, tag#0(\"yield\"), eff_in=r0, payload=[r2], rets=[] ; payload=[I64]",
        "on tag#0(\"yield\") -> @L1",
        "cont.new r3, call_sig#0, func=r2 ; sig=[I64] -> []",
        "resume eff_out=r0, call_sig#0, cont=r3, eff_in=r0, args=[r1], rets=[] ; sig=[I64] -> []",
        "block (r5:I64, r6:Cont)",
    ] {
        assert!(text.contains(line), "missing {line:?} in\n{text}");
    }
}

struct Effects {
    program: Arc<VerifiedProgram>,
    body: FuncId,
    run: FuncId,
    stale: FuncId,
    mismatched: FuncId,
    outer: FuncId,
}

/// `body(x)` calls `leaf()` twice; each `leaf` suspends `ask() -> i64` and returns the answer
/// plus one. `run(answer, x)` handles `ask` by resuming with `answer`.
///
/// `outer(answer, x)` handles `ask` around `middle(x)`, which only handles `log` around
/// `logged(x)`: its `ask` suspension passes through `middle`'s `resume`.
fn effects() -> Effects {
    let mut b = ProgramBuilder::new();
    let ask = b
        .declare_tag_with_results("ask", &[], &[ValueType::I64.into()])
        .unwrap();
    let log = b.declare_tag("log", &[ValueType::I64.into()]).unwrap();
    let i64_to_i64 = b
        .call_sig(sig(&[ValueType::I64], &[ValueType::I64]))
        .unwrap();
    let unit_to_i64 = b.call_sig(sig(&[], &[ValueType::I64])).unwrap();

    let leaf = b.declare_function(sig(&[], &[ValueType::I64])).unwrap();
    let mut asm = b.function_asm(leaf).unwrap();
    let (answer, one) = (
        asm.alloc_register(ValueType::I64),
        asm.alloc_register(ValueType::I64),
    );
    asm.suspend(0, ask, 0, &[], &[answer]);
    asm.const_i64(one, 1);
    asm.i64_add(answer, answer, one);
    asm.ret(0, &[answer]);
    b.define_function(leaf, asm).unwrap();

    let body = b
        .declare_function(sig(&[ValueType::I64], &[ValueType::I64]))
        .unwrap();
    let mut asm = b.function_asm(body).unwrap();
    let x = b.argument(body, 0).unwrap();
    let [first] = b.emit_call(&mut asm, leaf, &[]).unwrap();
    let [second] = b.emit_call(&mut asm, leaf, &[]).unwrap();
    asm.i64_add(first, first, second);
    asm.i64_add(first, first, x);
    asm.ret(0, &[first]);
    b.define_function(body, asm).unwrap();

    // `driver(answer, x)` runs `target(x)` as a continuation, answering every `ask`.
    let driver = |b: &mut ProgramBuilder, target: FuncId, stale: bool, call_sig: CallSigId| {
        let driver = b
            .declare_function(sig(&[ValueType::I64, ValueType::I64], &[ValueType::I64]))
            .unwrap();
        let mut asm = b.function_asm(driver).unwrap();
        let (answer, x) = (
            b.argument(driver, 0).unwrap(),
            b.argument(driver, 1).unwrap(),
        );
        let f = asm.alloc_register(ValueType::Func);
        let cont = asm.alloc_register(ValueType::Cont);
        let out = asm.alloc_register(ValueType::I64);
        let rest = asm.alloc_register(ValueType::Cont);
        let asked = asm.block(&[rest]);
        asm.const_func(f, target);
        asm.cont_new(cont, call_sig, f);
        protected_resume(
            &mut asm,
            &[Catch::On(ask, asked)],
            i64_to_i64,
            cont,
            &[x],
            &[out],
        );
        asm.ret(0, &[out]);
        asm.place(asked).unwrap();
        // A stale driver resumes the consumed original instead of the suspended rest.
        let resumed = if stale { cont } else { rest };
        protected_resume(
            &mut asm,
            &[Catch::On(ask, asked)],
            i64_to_i64,
            resumed,
            &[answer],
            &[out],
        );
        asm.ret(0, &[out]);
        b.define_function(driver, asm).unwrap();
        driver
    };
    let run = driver(&mut b, body, false, i64_to_i64);
    let stale = driver(&mut b, body, true, i64_to_i64);

    // `logged(x)` asks, logs the answer, and returns it plus `x`.
    let logged = b
        .declare_function(sig(&[ValueType::I64], &[ValueType::I64]))
        .unwrap();
    let mut asm = b.function_asm(logged).unwrap();
    let x = b.argument(logged, 0).unwrap();
    let answer = asm.alloc_register(ValueType::I64);
    asm.suspend(0, ask, 0, &[], &[answer]);
    asm.suspend(0, log, 0, &[answer], &[]);
    asm.i64_add(answer, answer, x);
    asm.ret(0, &[answer]);
    b.define_function(logged, asm).unwrap();

    let middle = b
        .declare_function(sig(&[ValueType::I64], &[ValueType::I64]))
        .unwrap();
    let mut asm = b.function_asm(middle).unwrap();
    let x = b.argument(middle, 0).unwrap();
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
    b.define_function(middle, asm).unwrap();
    let outer = driver(&mut b, middle, false, i64_to_i64);
    // `leaf` takes no argument, so its continuation is not resumable as `i64_to_i64`.
    let mismatched = driver(&mut b, leaf, false, unit_to_i64);

    Effects {
        program: Arc::new(b.build_verified().unwrap()),
        body,
        run,
        stale,
        mismatched,
        outer,
    }
}

#[test]
fn a_suspension_captures_every_frame_above_its_handler_and_resumes_with_results() {
    let fixture = effects();
    let result = run(
        &fixture.program,
        fixture.run,
        &[Value::I64(10), Value::I64(1)],
        Limits::default(),
    );
    // Each of two nested `leaf` calls is answered 10 and adds one.
    assert_eq!(result.unwrap(), vec![Value::I64(23)]);
}

#[test]
fn a_suspension_passes_through_resumes_without_a_matching_clause() {
    let fixture = effects();
    let result = run(
        &fixture.program,
        fixture.outer,
        &[Value::I64(10), Value::I64(1)],
        Limits::default(),
    );
    assert_eq!(result.unwrap(), vec![Value::I64(11)]);
}

#[test]
fn continuations_are_one_shot() {
    let fixture = effects();
    let result = run(
        &fixture.program,
        fixture.stale,
        &[Value::I64(10), Value::I64(1)],
        Limits::default(),
    );
    assert!(
        matches!(result, Err(Trap::ContinuationConsumed)),
        "{result:?}"
    );
}

#[test]
fn a_resume_checks_the_continuation_type() {
    let fixture = effects();
    let result = run(
        &fixture.program,
        fixture.mismatched,
        &[Value::I64(10), Value::I64(1)],
        Limits::default(),
    );
    assert!(
        matches!(result, Err(Trap::ContinuationSignatureMismatch { .. })),
        "{result:?}"
    );
}

#[test]
fn a_suspension_without_a_handler_traps() {
    let fixture = effects();
    let result = run(
        &fixture.program,
        fixture.body,
        &[Value::I64(1)],
        Limits::default(),
    );
    assert!(
        matches!(result, Err(Trap::UnhandledSuspension { .. })),
        "{result:?}"
    );
}

#[test]
fn suspended_frames_count_against_the_frame_budget() {
    let fixture = generators();
    // `sum` and one suspended generator frame fit; the generator cannot also start a frame.
    let limits = Limits {
        max_call_depth: 2,
        ..Limits::default()
    };
    assert_eq!(
        run(
            &fixture.program,
            fixture.sum,
            &[Value::I64(3)],
            limits.clone()
        )
        .unwrap(),
        vec![Value::I64(3)]
    );
    let limits = Limits {
        max_call_depth: 1,
        ..Limits::default()
    };
    let result = run(&fixture.program, fixture.sum, &[Value::I64(3)], limits);
    assert!(matches!(result, Err(Trap::CallDepthExceeded)), "{result:?}");
}

#[test]
fn a_continuation_of_one_unit_suspends_to_a_handler_in_another() {
    // Unit A asks through an imported tag; unit B handles it around A's function.
    let mut a = ProgramBuilder::new();
    let ask = a
        .import_tag_with_results("ask", &[], &[ValueType::I64.into()])
        .unwrap();
    let asker = a
        .declare_function(sig(&[ValueType::I64], &[ValueType::I64]))
        .unwrap();
    let mut asm = a.function_asm(asker).unwrap();
    let x = a.argument(asker, 0).unwrap();
    let answer = asm.alloc_register(ValueType::I64);
    asm.suspend(0, ask, 0, &[], &[answer]);
    asm.i64_add(answer, answer, x);
    asm.ret(0, &[answer]);
    a.define_function(asker, asm).unwrap();
    let a = Arc::new(a.build_verified().unwrap());

    let mut b = ProgramBuilder::new();
    let ask = b
        .import_tag_with_results("ask", &[], &[ValueType::I64.into()])
        .unwrap();
    let i64_to_i64 = b
        .call_sig(sig(&[ValueType::I64], &[ValueType::I64]))
        .unwrap();
    let drive = b
        .declare_function(sig(
            &[ValueType::Func, ValueType::I64, ValueType::I64],
            &[ValueType::I64],
        ))
        .unwrap();
    let mut asm = b.function_asm(drive).unwrap();
    let (f, answer, x) = (
        b.argument(drive, 0).unwrap(),
        b.argument(drive, 1).unwrap(),
        b.argument(drive, 2).unwrap(),
    );
    let cont = asm.alloc_register(ValueType::Cont);
    let out = asm.alloc_register(ValueType::I64);
    let rest = asm.alloc_register(ValueType::Cont);
    let asked = asm.block(&[rest]);
    asm.cont_new(cont, i64_to_i64, f);
    protected_resume(
        &mut asm,
        &[Catch::On(ask, asked)],
        i64_to_i64,
        cont,
        &[x],
        &[out],
    );
    asm.ret(0, &[out]);
    asm.place(asked).unwrap();
    protected_resume(
        &mut asm,
        &[Catch::On(ask, asked)],
        i64_to_i64,
        rest,
        &[answer],
        &[out],
    );
    asm.ret(0, &[out]);
    b.define_function(drive, asm).unwrap();
    let b = Arc::new(b.build_verified().unwrap());

    let mut vm = Vm::new(NoHost, Limits::default());
    // Importing requires the VM tag to declare the same results.
    vm.define_tag("ask", &[]).unwrap();
    assert!(matches!(
        vm.install(Arc::clone(&a)),
        Err(execution_tape::vm::InstallError::TagPayloadMismatch { .. })
    ));
    let mut vm = Vm::new(NoHost, Limits::default());
    let tag = vm
        .define_tag_with_results("ask", &[], &[ValueType::I64])
        .unwrap();
    assert_eq!(vm.units().tag_results(tag), Some(&[ValueType::I64][..]));
    let unit_a = vm.install(a).unwrap();
    let unit_b = vm.install(b).unwrap();
    let result = vm
        .run(
            FuncRef::new(unit_b, drive),
            &[
                Value::Func(FuncRef::new(unit_a, asker)),
                Value::I64(10),
                Value::I64(1),
            ],
            execution_tape::trace::TraceMask::NONE,
            None,
        )
        .unwrap();
    assert_eq!(result, vec![Value::I64(11)]);
}

/// Counts call-frame scopes by outcome and checks that they nest.
#[derive(Default)]
struct Scopes {
    open: Vec<usize>,
    entered: usize,
    returned: usize,
    suspended: usize,
    attempts: usize,
}

impl execution_tape::trace::TraceSink for Scopes {
    fn mask(&self) -> execution_tape::trace::TraceMask {
        execution_tape::trace::TraceMask::CALL | execution_tape::trace::TraceMask::INSTR
    }
    fn instr(
        &mut self,
        _: &execution_tape::program::Program,
        _: execution_tape::trace::TraceLocation,
        _: u32,
        _: Option<u64>,
        _: u16,
    ) {
        self.attempts += 1;
    }
    fn instr_end(
        &mut self,
        _: &execution_tape::program::Program,
        _: execution_tape::trace::TraceLocation,
        _: Option<execution_tape::trace::TraceLocation>,
        _: execution_tape::trace::TraceOutcome<'_>,
    ) {
        self.attempts -= 1;
    }
    fn scope_enter(
        &mut self,
        _: &execution_tape::program::Program,
        _: execution_tape::trace::ScopeKind,
        depth: usize,
        _: execution_tape::trace::TraceLocation,
        _: Option<u64>,
    ) {
        assert_eq!(depth, self.open.len() + 1, "scopes enter at the next depth");
        self.open.push(depth);
        self.entered += 1;
    }
    fn scope_exit(
        &mut self,
        _: &execution_tape::program::Program,
        _: execution_tape::trace::ScopeKind,
        depth: usize,
        _: execution_tape::trace::TraceLocation,
        _: Option<u64>,
        outcome: execution_tape::trace::TraceOutcome<'_>,
    ) {
        assert_eq!(self.open.pop(), Some(depth), "scopes exit innermost first");
        match outcome {
            execution_tape::trace::TraceOutcome::Ok => self.returned += 1,
            execution_tape::trace::TraceOutcome::Suspended => self.suspended += 1,
            other => panic!("unexpected scope outcome {other:?}"),
        }
    }
}

#[test]
fn suspended_activations_close_their_scopes_and_reenter_when_resumed() {
    let fixture = effects();
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(Arc::clone(&fixture.program)).unwrap();
    let mut scopes = Scopes::default();
    let mask = execution_tape::trace::TraceSink::mask(&scopes);
    let result = vm
        .run(
            FuncRef::new(unit, fixture.run),
            &[Value::I64(10), Value::I64(1)],
            mask,
            Some(&mut scopes),
        )
        .unwrap();
    assert_eq!(result, vec![Value::I64(23)]);
    assert!(scopes.open.is_empty());
    assert_eq!(scopes.attempts, 0, "every instruction attempt completes");
    // Two suspensions each close `leaf` and `body`, which re-enter when resumed.
    assert_eq!(scopes.suspended, 4);
    assert_eq!(scopes.entered, scopes.returned + scopes.suspended);
}

#[test]
fn recordings_replay_suspensions_and_resumptions() {
    use execution_tape::recording::{RecordLimits, Recorder, ReplayOutcome};
    use execution_tape::trace::TraceSink as _;
    let fixture = effects();
    for entry in [fixture.run, fixture.outer] {
        let mut recorder = Recorder::new(Arc::clone(&fixture.program), RecordLimits::default());
        let result = {
            let mut vm = Vm::new(NoHost, Limits::default());
            let unit = vm.install(Arc::clone(&fixture.program)).unwrap();
            vm.run(
                FuncRef::new(unit, entry),
                &[Value::I64(10), Value::I64(1)],
                recorder.mask(),
                Some(&mut recorder),
            )
        }
        .unwrap();
        let recording = recorder.finish();
        recording.status().unwrap();
        match recording.replay() {
            Ok(ReplayOutcome::Returned(values)) => assert_eq!(values.values(), result),
            other => panic!("replay diverged: {other:?}"),
        }
    }
}

struct Exceptional {
    program: Arc<VerifiedProgram>,
    thrower: FuncId,
    throw_into: FuncId,
    escape: FuncId,
    unstarted: FuncId,
    uncaught: FuncId,
    bound: FuncId,
}

/// `guarded(n)` yields `n` inside a region catching `stop(code)`, returning `code` when the
/// exception arrives at its suspension and `n` when resumed normally. `thrower(x)` throws
/// `stop(x)`. Each driver adds 1000 to a `stop` it catches itself.
fn exceptional() -> Exceptional {
    let mut b = ProgramBuilder::new();
    let stop = b.declare_tag("stop", &[ValueType::I64.into()]).unwrap();
    let yield_tag = b.declare_tag("yield", &[ValueType::I64.into()]).unwrap();
    let i64_to_i64 = b
        .call_sig(sig(&[ValueType::I64], &[ValueType::I64]))
        .unwrap();
    let unit_to_i64 = b.call_sig(sig(&[], &[ValueType::I64])).unwrap();
    let one_to_one = sig(&[ValueType::I64], &[ValueType::I64]);

    let guarded = b.declare_function(&one_to_one).unwrap();
    let mut asm = b.function_asm(guarded).unwrap();
    let n = b.argument(guarded, 0).unwrap();
    let code = asm.alloc_register(ValueType::I64);
    let stopped = asm.block(&[code]);
    asm.begin_region(&[Catch::Tag(stop, stopped)]);
    asm.suspend(0, yield_tag, 0, &[n], &[]);
    asm.end_region();
    asm.ret(0, &[n]);
    asm.place(stopped).unwrap();
    asm.ret(0, &[code]);
    b.define_function(guarded, asm).unwrap();

    let thrower = b.declare_function(&one_to_one).unwrap();
    let mut asm = b.function_asm(thrower).unwrap();
    let x = b.argument(thrower, 0).unwrap();
    b.emit_throw(&mut asm, stop, &[x]).unwrap();
    b.define_function(thrower, asm).unwrap();

    // Drivers share one shape: create a continuation of `target`, then `start` it.
    enum Start {
        Resume,
        Throw,
        Bind,
    }
    let driver = |b: &mut ProgramBuilder, target: FuncId, start: Start, catch: bool| {
        let driver = b.declare_function(&one_to_one).unwrap();
        let mut asm = b.function_asm(driver).unwrap();
        let x = b.argument(driver, 0).unwrap();
        let f = asm.alloc_register(ValueType::Func);
        let cont = asm.alloc_register(ValueType::Cont);
        let out = asm.alloc_register(ValueType::I64);
        let (value, rest) = (
            asm.alloc_register(ValueType::I64),
            asm.alloc_register(ValueType::Cont),
        );
        let code = asm.alloc_register(ValueType::I64);
        let thousand = asm.alloc_register(ValueType::I64);
        let yielded = asm.block(&[value, rest]);
        let stopped = asm.block(&[code]);
        let mut clauses = vec![Catch::On(yield_tag, yielded)];
        if catch {
            clauses.push(Catch::Tag(stop, stopped));
        }
        asm.const_func(f, target);
        asm.cont_new(cont, i64_to_i64, f);
        asm.begin_region(&clauses);
        match start {
            Start::Resume => {
                asm.resume(0, i64_to_i64, cont, 0, &[x], &[out]);
            }
            Start::Throw => {
                asm.resume_throw(0, i64_to_i64, cont, 0, stop, &[x], &[out]);
            }
            Start::Bind => {
                asm.cont_bind(cont, i64_to_i64, unit_to_i64, cont, &[x]);
                asm.resume(0, unit_to_i64, cont, 0, &[], &[out]);
            }
        }
        asm.end_region();
        asm.ret(0, &[out]);
        asm.place(yielded).unwrap();
        asm.begin_region(&clauses);
        match start {
            // A bound continuation's suspension resumes normally; others raise `stop` into it.
            Start::Bind => asm.resume(0, unit_to_i64, rest, 0, &[], &[out]),
            _ => asm.resume_throw(0, unit_to_i64, rest, 0, stop, &[value], &[out]),
        };
        asm.end_region();
        asm.ret(0, &[out]);
        asm.place(stopped).unwrap();
        asm.const_i64(thousand, 1000);
        asm.i64_add(code, code, thousand);
        asm.ret(0, &[code]);
        b.define_function(driver, asm).unwrap();
        driver
    };
    let throw_into = driver(&mut b, guarded, Start::Resume, true);
    let escape = driver(&mut b, thrower, Start::Resume, true);
    let unstarted = driver(&mut b, guarded, Start::Throw, true);
    let uncaught = driver(&mut b, thrower, Start::Resume, false);
    let bound = driver(&mut b, guarded, Start::Bind, true);
    Exceptional {
        program: Arc::new(b.build_verified().unwrap()),
        thrower,
        throw_into,
        escape,
        unstarted,
        uncaught,
        bound,
    }
}

fn exceptional_run(func: impl Fn(&Exceptional) -> FuncId) -> Result<Vec<Value>, Trap> {
    let fixture = exceptional();
    run(
        &fixture.program,
        func(&fixture),
        &[Value::I64(7)],
        Limits::default(),
    )
}

#[test]
fn resume_throw_raises_at_the_suspension_point() {
    // `guarded` catches the `stop(7)` raised at its `suspend` and returns 7.
    assert_eq!(
        exceptional_run(|f| f.throw_into).unwrap(),
        vec![Value::I64(7)]
    );
}

#[test]
fn an_exception_escaping_a_continuation_reaches_its_resume() {
    assert_eq!(
        exceptional_run(|f| f.escape).unwrap(),
        vec![Value::I64(1007)]
    );
}

#[test]
fn resume_throw_raises_at_the_resumer_for_an_unstarted_continuation() {
    assert_eq!(
        exceptional_run(|f| f.unstarted).unwrap(),
        vec![Value::I64(1007)]
    );
}

#[test]
fn an_uncaught_exception_from_a_continuation_names_its_origin() {
    let fixture = exceptional();
    let result = run(
        &fixture.program,
        fixture.uncaught,
        &[Value::I64(7)],
        Limits::default(),
    );
    let Err(Trap::Uncaught(exception)) = result else {
        panic!("expected an uncaught exception, got {result:?}");
    };
    assert_eq!(exception.origin().func, fixture.thrower);
}

#[test]
fn cont_bind_supplies_leading_inputs() {
    assert_eq!(exceptional_run(|f| f.bound).unwrap(), vec![Value::I64(7)]);
}

#[test]
fn invocations_pause_and_resume_at_every_boundary_inside_continuations() {
    use execution_tape::recording::{RecordLimits, Recorder, ReplayOutcome};
    use execution_tape::trace::TraceSink as _;
    use execution_tape::vm::{InvocationOptions, RunStatus};
    let fixture = effects();
    for quantum in [1, 2, 5, u64::MAX] {
        let mut vm = Vm::new(NoHost, Limits::default());
        let unit = vm.install(Arc::clone(&fixture.program)).unwrap();
        let mut recorder = Recorder::new(Arc::clone(&fixture.program), RecordLimits::default());
        let options = InvocationOptions::default().trace(recorder.mask(), &mut recorder);
        let mut invocation = vm
            .start(
                FuncRef::new(unit, fixture.outer),
                &[Value::I64(10), Value::I64(1)],
                options,
            )
            .unwrap();
        let mut deepest = 0;
        while invocation.resume(quantum).unwrap() == RunStatus::Suspended {
            // Paused frames are the active stack, including resumed continuations.
            deepest = deepest.max(invocation.inspect().frames().len());
        }
        assert_eq!(invocation.take_outputs().unwrap(), vec![Value::I64(11)]);
        drop(invocation);
        if quantum == 1 {
            // `outer`, `middle` and `logged` are all on the stack while `logged` runs.
            assert_eq!(deepest, 3);
        }
        let recording = recorder.finish();
        recording.status().unwrap();
        assert!(matches!(
            recording.replay(),
            Ok(ReplayOutcome::Returned(values)) if values.values() == [Value::I64(11)]
        ));
    }
}

#[test]
fn a_guest_callback_cannot_suspend_its_pending_host_request() {
    use execution_tape::host::HostSig;
    use execution_tape::vm::{InvocationOptions, RunStatus};
    let mut b = ProgramBuilder::new();
    let ask = b
        .declare_tag_with_results("ask", &[], &[ValueType::I64.into()])
        .unwrap();
    let wait = b.host_sig_for(
        "wait",
        HostSig {
            args: vec![],
            rets: vec![],
        },
    );
    let unit_to_unit = b.call_sig(sig(&[], &[])).unwrap();
    let asker = b.declare_function(sig(&[], &[ValueType::I64])).unwrap();
    let mut asm = b.function_asm(asker).unwrap();
    let answer = asm.alloc_register(ValueType::I64);
    asm.suspend(0, ask, 0, &[], &[answer]);
    asm.ret(0, &[answer]);
    b.define_function(asker, asm).unwrap();
    let waiter = b.declare_function(sig(&[], &[])).unwrap();
    let mut asm = b.function_asm(waiter).unwrap();
    asm.host_call(0, wait, 0, &[], &[]);
    asm.ret(0, &[]);
    b.define_function(waiter, asm).unwrap();
    // `drive()` handles `ask` around a continuation that waits on the host.
    let drive = b.declare_function(sig(&[], &[])).unwrap();
    let mut asm = b.function_asm(drive).unwrap();
    let f = asm.alloc_register(ValueType::Func);
    let cont = asm.alloc_register(ValueType::Cont);
    let rest = asm.alloc_register(ValueType::Cont);
    let asked = asm.block(&[rest]);
    asm.const_func(f, waiter);
    asm.cont_new(cont, unit_to_unit, f);
    protected_resume(
        &mut asm,
        &[Catch::On(ask, asked)],
        unit_to_unit,
        cont,
        &[],
        &[],
    );
    asm.ret(0, &[]);
    asm.place(asked).unwrap();
    asm.ret(0, &[]);
    b.define_function(drive, asm).unwrap();
    let program = Arc::new(b.build_verified().unwrap());

    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(program).unwrap();
    let mut run = vm
        .start(
            FuncRef::new(unit, drive),
            &[],
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    run.pending_host_call()
        .unwrap()
        .call_guest(Value::Func(FuncRef::new(unit, asker)), vec![])
        .unwrap();
    let error = run.resume(u64::MAX).unwrap_err();
    assert!(
        matches!(error.trap, Trap::SuspensionAcrossHost { .. }),
        "{error:?}"
    );
}

fn rejected(edit: impl FnOnce(&mut ProgramBuilder)) -> execution_tape::verifier::VerifyError {
    let mut b = ProgramBuilder::new();
    edit(&mut b);
    execution_tape::verifier::verify_program(
        &b.build(),
        &execution_tape::verifier::VerifyConfig::default(),
    )
    .unwrap_err()
}

#[test]
fn verifier_keeps_continuations_inside_guest_registers_and_checks_their_instructions() {
    use execution_tape::host::HostSig;
    use execution_tape::verifier::{ExceptionPlace, RegionError, VerifyError};
    let error = rejected(|b| {
        b.host_sig_for(
            "leak",
            HostSig {
                args: vec![],
                rets: vec![ValueType::Cont],
            },
        );
    });
    assert_eq!(
        error,
        VerifyError::ContinuationReference {
            place: ExceptionPlace::HostSig { host_sig: 0 }
        }
    );
    let error = rejected(|b| {
        b.declare_tag_with_results("resumes", &[], &[ValueType::Cont.into()])
            .unwrap();
    });
    assert_eq!(
        error,
        VerifyError::ContinuationReference {
            place: ExceptionPlace::TagPayload { tag: 0 }
        }
    );

    // An `on` target must bind the payload and then the continuation.
    let error = rejected(|b| {
        let yield_tag = b.declare_tag("yield", &[ValueType::I64.into()]).unwrap();
        let next = b.call_sig(sig(&[], &[])).unwrap();
        let f = b.declare_function(sig(&[], &[])).unwrap();
        let mut asm = b.function_asm(f).unwrap();
        let cont = asm.alloc_register(ValueType::Cont);
        let value = asm.alloc_register(ValueType::I64);
        let handler = asm.block(&[value]);
        let func = asm.alloc_register(ValueType::Func);
        asm.const_func(func, f);
        asm.cont_new(cont, next, func);
        protected_resume(
            &mut asm,
            &[Catch::On(yield_tag, handler)],
            next,
            cont,
            &[],
            &[],
        );
        asm.ret(0, &[]);
        asm.place(handler).unwrap();
        asm.ret(0, &[]);
        b.define_function(f, asm).unwrap();
    });
    assert!(
        matches!(
            error,
            VerifyError::InvalidRegion {
                error: RegionError::ClauseParams {
                    expected: 2,
                    actual: 1,
                    ..
                },
                ..
            }
        ),
        "{error:?}"
    );

    // `cont.bind` must drop exactly the bound leading arguments.
    let error = rejected(|b| {
        let from = b
            .call_sig(sig(&[ValueType::I64], &[ValueType::I64]))
            .unwrap();
        let to = b.call_sig(sig(&[], &[])).unwrap();
        let f = b.declare_function(sig(&[], &[])).unwrap();
        let mut asm = b.function_asm(f).unwrap();
        let cont = asm.alloc_register(ValueType::Cont);
        let value = asm.alloc_register(ValueType::I64);
        let func = asm.alloc_register(ValueType::Func);
        asm.const_func(func, f);
        asm.const_i64(value, 1);
        asm.cont_new(cont, from, func);
        asm.cont_bind(cont, from, to, cont, &[value]);
        asm.ret(0, &[]);
        b.define_function(f, asm).unwrap();
    });
    assert!(
        matches!(error, VerifyError::ContBindSignature { .. }),
        "{error:?}"
    );
}

#[test]
fn programs_with_continuations_round_trip_through_the_wire_format() {
    for program in [
        generators().program,
        effects().program,
        exceptional().program,
    ] {
        let program = program.program();
        let decoded = execution_tape::program::Program::decode(&program.encode()).unwrap();
        assert_eq!(&decoded, program);
    }
}

#[test]
fn suspended_registers_survive_resumption_at_another_depth() {
    // `holder(x)` keeps an `I32` and an unstarted continuation live across a suspension;
    // `drive` resumes it from inside `helper`, one frame deeper than where it suspended.
    let mut b = ProgramBuilder::new();
    let tick = b.declare_tag("tick", &[]).unwrap();
    let unit_to_i64 = b.call_sig(sig(&[], &[ValueType::I64])).unwrap();
    let held = sig(&[], &[ValueType::I32, ValueType::I64]);
    let held_sig = b.call_sig(held.clone()).unwrap();
    let start_sig = b
        .call_sig(sig(&[ValueType::I32], &[ValueType::I32, ValueType::I64]))
        .unwrap();
    let five = b.declare_function(sig(&[], &[ValueType::I64])).unwrap();
    let mut asm = b.function_asm(five).unwrap();
    let value = asm.alloc_register(ValueType::I64);
    asm.const_i64(value, 5);
    asm.ret(0, &[value]);
    b.define_function(five, asm).unwrap();

    let holder = b
        .declare_function(sig(&[ValueType::I32], &[ValueType::I32, ValueType::I64]))
        .unwrap();
    let mut asm = b.function_asm(holder).unwrap();
    let x = b.argument(holder, 0).unwrap();
    let f = asm.alloc_register(ValueType::Func);
    let inner = asm.alloc_register(ValueType::Cont);
    let out = asm.alloc_register(ValueType::I64);
    let done = asm.alloc_register(ValueType::Cont);
    let ignored = asm.block(&[done]);
    asm.const_func(f, five);
    asm.cont_new(inner, unit_to_i64, f);
    asm.suspend(0, tick, 0, &[], &[]);
    protected_resume(
        &mut asm,
        &[Catch::On(tick, ignored)],
        unit_to_i64,
        inner,
        &[],
        &[out],
    );
    asm.ret(0, &[x, out]);
    asm.place(ignored).unwrap();
    asm.trap(1);
    b.define_function(holder, asm).unwrap();

    let helper = b
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::Cont],
            ret_types: held.ret_types.clone(),
        })
        .unwrap();
    let mut asm = b.function_asm(helper).unwrap();
    let cont = b.argument(helper, 0).unwrap();
    let (a, c) = (
        asm.alloc_register(ValueType::I32),
        asm.alloc_register(ValueType::I64),
    );
    asm.resume(0, held_sig, cont, 0, &[], &[a, c]);
    asm.ret(0, &[a, c]);
    b.define_function(helper, asm).unwrap();

    let drive = b
        .declare_function(sig(&[ValueType::I32], &[ValueType::I32, ValueType::I64]))
        .unwrap();
    let mut asm = b.function_asm(drive).unwrap();
    let x = b.argument(drive, 0).unwrap();
    let f = asm.alloc_register(ValueType::Func);
    let cont = asm.alloc_register(ValueType::Cont);
    let (a, c) = (
        asm.alloc_register(ValueType::I32),
        asm.alloc_register(ValueType::I64),
    );
    let rest = asm.alloc_register(ValueType::Cont);
    let ticked = asm.block(&[rest]);
    asm.const_func(f, holder);
    asm.cont_new(cont, start_sig, f);
    protected_resume(
        &mut asm,
        &[Catch::On(tick, ticked)],
        start_sig,
        cont,
        &[x],
        &[a, c],
    );
    asm.ret(0, &[a, c]);
    asm.place(ticked).unwrap();
    let [a, c] = b.emit_call(&mut asm, helper, &[rest]).unwrap();
    asm.ret(0, &[a, c]);
    b.define_function(drive, asm).unwrap();
    let program = Arc::new(b.build_verified().unwrap());
    assert_eq!(
        run(&program, drive, &[Value::I32(-3)], Limits::default()).unwrap(),
        vec![Value::I32(-3), Value::I64(5)]
    );
}

#[test]
fn a_closure_continuation_receives_its_environment_first() {
    let mut b = ProgramBuilder::new();
    let i64_to_i64 = b
        .call_sig(sig(&[ValueType::I64], &[ValueType::I64]))
        .unwrap();
    let int = b
        .contract(execution_tape::contract::Contract::Kind(ValueType::I64))
        .unwrap();
    let pair = b
        .contract(execution_tape::contract::Contract::Tuple(&[Some(int)]))
        .unwrap();
    let body = b
        .declare_function(execution_tape::asm::FunctionAbi {
            args: vec![
                execution_tape::asm::ContractRef::Id(pair),
                ValueType::I64.into(),
            ],
            rets: vec![ValueType::I64.into()],
        })
        .unwrap();
    let mut asm = b.function_asm(body).unwrap();
    let (env, x) = (b.argument(body, 0).unwrap(), b.argument(body, 1).unwrap());
    let captured = asm.alloc_register(ValueType::I64);
    asm.tuple_get(captured, env, 0);
    asm.i64_add(captured, captured, x);
    asm.ret(0, &[captured]);
    b.define_function(body, asm).unwrap();
    let drive = b
        .declare_function(sig(&[ValueType::I64, ValueType::I64], &[ValueType::I64]))
        .unwrap();
    let mut asm = b.function_asm(drive).unwrap();
    let (captured, x) = (b.argument(drive, 0).unwrap(), b.argument(drive, 1).unwrap());
    let f = asm.alloc_register(ValueType::Func);
    let env = asm.alloc_register(ValueType::Agg);
    let closure = asm.alloc_register(ValueType::Closure);
    let cont = asm.alloc_register(ValueType::Cont);
    let out = asm.alloc_register(ValueType::I64);
    asm.const_func(f, body);
    asm.tuple_new(env, &[captured]);
    asm.closure_new(closure, f, env);
    asm.cont_new(cont, i64_to_i64, closure);
    asm.resume(0, i64_to_i64, cont, 0, &[x], &[out]);
    asm.ret(0, &[out]);
    b.define_function(drive, asm).unwrap();
    let program = Arc::new(b.build_verified().unwrap());
    assert_eq!(
        run(
            &program,
            drive,
            &[Value::I64(40), Value::I64(2)],
            Limits::default()
        )
        .unwrap(),
        vec![Value::I64(42)]
    );
}

#[test]
fn continuations_cannot_cross_the_invocation_boundary() {
    let mut b = ProgramBuilder::new();
    let takes = b.declare_function(sig(&[ValueType::Cont], &[])).unwrap();
    let mut asm = b.function_asm(takes).unwrap();
    asm.ret(0, &[]);
    b.define_function(takes, asm).unwrap();
    let program = Arc::new(b.build_verified().unwrap());
    let result = run(&program, takes, &[Value::Unit], Limits::default());
    assert!(
        matches!(result, Err(Trap::ContinuationBoundary)),
        "{result:?}"
    );
    let mut vm = Vm::new(NoHost, Limits::default());
    assert_eq!(
        vm.define_tag_with_results("resumes", &[], &[ValueType::Cont]),
        Err(execution_tape::vm::TagError::ContinuationPayload)
    );
}

#[test]
fn abandoned_continuations_hold_their_frames_until_the_invocation_ends() {
    // Current behaviour: a continuation that is never resumed again keeps its frames charged
    // against `max_call_depth` until the invocation ends. `take_first(n)` starts `n`
    // generators, takes one value from each and drops it.
    let mut b = ProgramBuilder::new();
    let yield_tag = b.declare_tag("yield", &[ValueType::I64.into()]).unwrap();
    let generator = emit_generator(&mut b, yield_tag);
    let start = b.call_sig(sig(&[ValueType::I64], &[])).unwrap();
    let take_first = b
        .declare_function(sig(&[ValueType::I64], &[ValueType::I64]))
        .unwrap();
    let mut asm = b.function_asm(take_first).unwrap();
    let n = b.argument(take_first, 0).unwrap();
    let f = asm.alloc_register(ValueType::Func);
    let cont = asm.alloc_register(ValueType::Cont);
    let (value, rest) = (
        asm.alloc_register(ValueType::I64),
        asm.alloc_register(ValueType::Cont),
    );
    let (one, done) = (
        asm.alloc_register(ValueType::I64),
        asm.alloc_register(ValueType::Bool),
    );
    let (check, body, exit) = (asm.label(), asm.label(), asm.label());
    let yielded = asm.block(&[value, rest]);
    asm.const_i64(one, 1);
    asm.const_func(f, generator);
    asm.jmp(check);
    asm.place(check).unwrap();
    asm.const_i64(value, 0);
    asm.i64_ge(done, value, n);
    asm.br(done, exit, body);
    asm.place(body).unwrap();
    asm.cont_new(cont, start, f);
    protected_resume(
        &mut asm,
        &[Catch::On(yield_tag, yielded)],
        start,
        cont,
        &[one],
        &[],
    );
    asm.trap(1);
    asm.place(yielded).unwrap();
    asm.i64_sub(n, n, one);
    asm.jmp(check);
    asm.place(exit).unwrap();
    asm.ret(0, &[n]);
    b.define_function(take_first, asm).unwrap();
    let program = Arc::new(b.build_verified().unwrap());
    let limits = Limits {
        max_call_depth: 8,
        ..Limits::default()
    };
    // `take_first` and seven abandoned generator frames fit; an eighth does not.
    assert_eq!(
        run(&program, take_first, &[Value::I64(7)], limits.clone()).unwrap(),
        vec![Value::I64(0)]
    );
    let result = run(&program, take_first, &[Value::I64(8)], limits);
    assert!(matches!(result, Err(Trap::CallDepthExceeded)), "{result:?}");
}

#[test]
fn recordings_replay_continuation_traps() {
    use execution_tape::recording::{RecordLimits, Recorder, ReplayOutcome};
    use execution_tape::trace::TraceSink as _;
    let mut b = ProgramBuilder::new();
    let ask = b.declare_tag("ask", &[]).unwrap();
    let f = b.declare_function(sig(&[], &[])).unwrap();
    let mut asm = b.function_asm(f).unwrap();
    asm.suspend(0, ask, 0, &[], &[]);
    asm.ret(0, &[]);
    b.define_function(f, asm).unwrap();
    let program = Arc::new(b.build_verified().unwrap());

    let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
    let result = {
        let mut vm = Vm::new(NoHost, Limits::default());
        let unit = vm.install(Arc::clone(&program)).unwrap();
        vm.run(
            FuncRef::new(unit, f),
            &[],
            recorder.mask(),
            Some(&mut recorder),
        )
    };
    assert!(matches!(
        result.map_err(|error| error.trap),
        Err(Trap::UnhandledSuspension { .. })
    ));
    let recording = recorder.finish();
    recording.status().unwrap();
    let replayed = recording.replay();
    assert!(
        matches!(
            &replayed,
            Ok(ReplayOutcome::Trapped(info)) if matches!(info.trap, Trap::UnhandledSuspension { .. })
        ),
        "replay diverged: {replayed:?}"
    );
}

#[test]
fn verifier_checks_a_known_callable_against_its_continuation_signature() {
    use execution_tape::verifier::VerifyError;
    let error = rejected(|b| {
        let unit_to_i64 = b.call_sig(sig(&[], &[ValueType::I64])).unwrap();
        let f = b
            .declare_function(sig(&[ValueType::I64], &[ValueType::I64]))
            .unwrap();
        let mut asm = b.function_asm(f).unwrap();
        let x = b.argument(f, 0).unwrap();
        let func = asm.alloc_register(ValueType::Func);
        let cont = asm.alloc_register(ValueType::Cont);
        asm.const_func(func, f);
        asm.cont_new(cont, unit_to_i64, func);
        asm.ret(0, &[x]);
        b.define_function(f, asm).unwrap();
    });
    assert!(
        matches!(error, VerifyError::CallIndirectContractMismatch { .. }),
        "{error:?}"
    );
}

#[test]
fn exceptional_switching_records_replays_and_pauses_anywhere() {
    use execution_tape::recording::{RecordLimits, Recorder, ReplayOutcome};
    use execution_tape::trace::TraceSink as _;
    use execution_tape::vm::{InvocationOptions, RunStatus};
    let fixture = exceptional();
    for entry in [
        fixture.throw_into,
        fixture.escape,
        fixture.unstarted,
        fixture.uncaught,
        fixture.bound,
    ] {
        let expected = run(&fixture.program, entry, &[Value::I64(7)], Limits::default());
        let check = |recorder: Recorder| {
            let recording = recorder.finish();
            recording.status().unwrap();
            match (recording.replay(), &expected) {
                (Ok(ReplayOutcome::Returned(values)), Ok(expected)) => {
                    assert_eq!(values.values(), expected.as_slice());
                }
                (Ok(ReplayOutcome::Trapped(info)), Err(Trap::Uncaught(_))) => {
                    assert!(matches!(info.trap, Trap::Uncaught(_)));
                }
                (other, _) => panic!("replay diverged: {other:?}"),
            }
        };
        let mut recorder = Recorder::new(Arc::clone(&fixture.program), RecordLimits::default());
        {
            let mut vm = Vm::new(NoHost, Limits::default());
            let unit = vm.install(Arc::clone(&fixture.program)).unwrap();
            let _ = vm.run(
                FuncRef::new(unit, entry),
                &[Value::I64(7)],
                recorder.mask(),
                Some(&mut recorder),
            );
        }
        check(recorder);
        for quantum in [1, 2, 3] {
            let mut vm = Vm::new(NoHost, Limits::default());
            let unit = vm.install(Arc::clone(&fixture.program)).unwrap();
            let mut recorder = Recorder::new(Arc::clone(&fixture.program), RecordLimits::default());
            let options = InvocationOptions::default().trace(recorder.mask(), &mut recorder);
            let mut invocation = vm
                .start(FuncRef::new(unit, entry), &[Value::I64(7)], options)
                .unwrap();
            loop {
                match invocation.resume(quantum) {
                    Ok(RunStatus::Suspended) => {}
                    Ok(RunStatus::Complete) => {
                        let outputs = invocation.take_outputs().unwrap();
                        assert_eq!(Ok(&outputs), expected.as_ref().map_err(|_| ()));
                        break;
                    }
                    Ok(other) => panic!("unexpected status {other:?}"),
                    Err(error) => {
                        assert!(matches!(error.trap, Trap::Uncaught(_)), "{error:?}");
                        break;
                    }
                }
            }
            drop(invocation);
            check(recorder);
        }
    }
}

#[test]
fn resume_throw_consumes_its_continuation() {
    let mut b = ProgramBuilder::new();
    let stop = b.declare_tag("stop", &[]).unwrap();
    let unit_to_unit = b.call_sig(sig(&[], &[])).unwrap();
    let f = b.declare_function(sig(&[], &[])).unwrap();
    let mut asm = b.function_asm(f).unwrap();
    let func = asm.alloc_register(ValueType::Func);
    let cont = asm.alloc_register(ValueType::Cont);
    let stopped = asm.block(&[]);
    asm.const_func(func, f);
    asm.cont_new(cont, unit_to_unit, func);
    asm.begin_region(&[Catch::Tag(stop, stopped)]);
    asm.resume_throw(0, unit_to_unit, cont, 0, stop, &[], &[]);
    asm.end_region();
    asm.ret(0, &[]);
    asm.place(stopped).unwrap();
    // The unstarted continuation was discarded; using it again traps.
    asm.resume(0, unit_to_unit, cont, 0, &[], &[]);
    asm.ret(0, &[]);
    b.define_function(f, asm).unwrap();
    let program = Arc::new(b.build_verified().unwrap());
    let result = run(&program, f, &[], Limits::default());
    assert!(
        matches!(result, Err(Trap::ContinuationConsumed)),
        "{result:?}"
    );
}

/// `leaf()` suspends `ask`, whose result contract is the tuple `(i64,)`, and projects it. The
/// driver binds a value to the suspended continuation instead of resuming it with one.
fn bind_to_suspension(bound: impl FnOnce(&mut Asm, u32, u32)) -> Result<Vec<Value>, String> {
    use execution_tape::asm::{ContractRef, FunctionAbi};
    use execution_tape::contract::Contract;
    let mut b = ProgramBuilder::new();
    let int = b.contract(Contract::Kind(ValueType::I64)).unwrap();
    let single = b.contract(Contract::Tuple(&[Some(int)])).unwrap();
    let ask = b
        .declare_tag_with_results("ask", &[], &[ContractRef::Id(single)])
        .unwrap();
    let unit_to_i64 = b.call_sig(sig(&[], &[ValueType::I64])).unwrap();
    let tuple_to_i64 = b
        .call_sig(FunctionAbi {
            args: vec![ContractRef::Id(single)],
            rets: vec![ValueType::I64.into()],
        })
        .unwrap();
    let leaf = b.declare_function(sig(&[], &[ValueType::I64])).unwrap();
    let mut asm = b.function_asm(leaf).unwrap();
    let (tuple, first) = (
        asm.alloc_register(ValueType::Agg),
        asm.alloc_register(ValueType::I64),
    );
    asm.suspend(0, ask, 0, &[], &[tuple]);
    asm.tuple_get(first, tuple, 0);
    asm.ret(0, &[first]);
    b.define_function(leaf, asm).unwrap();

    let driver = b
        .declare_function(sig(&[ValueType::Bool], &[ValueType::I64]))
        .unwrap();
    let mut asm = b.function_asm(driver).unwrap();
    let pick = b.argument(driver, 0).unwrap();
    let f = asm.alloc_register(ValueType::Func);
    let cont = asm.alloc_register(ValueType::Cont);
    let out = asm.alloc_register(ValueType::I64);
    let rest = asm.alloc_register(ValueType::Cont);
    let value = asm.alloc_register(ValueType::Agg);
    let asked = asm.block(&[rest]);
    asm.const_func(f, leaf);
    asm.cont_new(cont, unit_to_i64, f);
    protected_resume(
        &mut asm,
        &[Catch::On(ask, asked)],
        unit_to_i64,
        cont,
        &[],
        &[out],
    );
    asm.ret(0, &[out]);
    asm.place(asked).unwrap();
    bound(&mut asm, value, pick);
    asm.cont_bind(rest, tuple_to_i64, unit_to_i64, rest, &[value]);
    asm.resume(0, unit_to_i64, rest, 0, &[], &[out]);
    asm.ret(0, &[out]);
    b.define_function(driver, asm).unwrap();
    let program = Arc::new(b.build_verified().map_err(|error| format!("{error:?}"))?);
    run(&program, driver, &[Value::Bool(true)], Limits::default())
        .map_err(|trap| format!("{trap:?}"))
}

#[test]
fn values_bound_to_a_suspension_satisfy_the_tags_result_contracts() {
    // A statically known contradiction is rejected by the verifier.
    let error = bind_to_suspension(|asm, value, _| {
        let flag = asm.alloc_register(ValueType::Bool);
        asm.const_bool(flag, true);
        asm.tuple_new(value, &[flag]);
    })
    .unwrap_err();
    assert!(error.contains("ContractMismatch"), "{error}");

    // An unproven value is checked when it is bound.
    let unproven = |asm: &mut Asm, value: u32, pick: u32| {
        let flag = asm.alloc_register(ValueType::Bool);
        let number = asm.alloc_register(ValueType::I64);
        let (flagged, numbered) = (
            asm.alloc_register(ValueType::Agg),
            asm.alloc_register(ValueType::Agg),
        );
        asm.const_bool(flag, true);
        asm.const_i64(number, 9);
        asm.tuple_new(flagged, &[flag]);
        asm.tuple_new(numbered, &[number]);
        asm.select(value, pick, flagged, numbered);
    };
    let error = bind_to_suspension(unproven).unwrap_err();
    assert!(error.starts_with("ContractMismatch"), "{error}");
}

#[test]
fn cont_bind_consumes_its_continuation() {
    let mut b = ProgramBuilder::new();
    let i64_to_i64 = b
        .call_sig(sig(&[ValueType::I64], &[ValueType::I64]))
        .unwrap();
    let unit_to_i64 = b.call_sig(sig(&[], &[ValueType::I64])).unwrap();
    let f = b
        .declare_function(sig(&[ValueType::I64], &[ValueType::I64]))
        .unwrap();
    let mut asm = b.function_asm(f).unwrap();
    let x = b.argument(f, 0).unwrap();
    let func = asm.alloc_register(ValueType::Func);
    let (cont, bound) = (
        asm.alloc_register(ValueType::Cont),
        asm.alloc_register(ValueType::Cont),
    );
    asm.const_func(func, f);
    asm.cont_new(cont, i64_to_i64, func);
    asm.cont_bind(bound, i64_to_i64, unit_to_i64, cont, &[x]);
    asm.resume(0, i64_to_i64, cont, 0, &[x], &[x]);
    asm.ret(0, &[x]);
    b.define_function(f, asm).unwrap();
    let program = Arc::new(b.build_verified().unwrap());
    let result = run(&program, f, &[Value::I64(1)], Limits::default());
    assert!(
        matches!(result, Err(Trap::ContinuationConsumed)),
        "{result:?}"
    );
}
