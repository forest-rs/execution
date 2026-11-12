// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Protected regions and catch clauses.

use execution_tape::value::FuncRef;
use execution_tape::{
    asm::{Asm, Catch, FunctionSig, ProgramBuilder},
    contract::{Contract, ContractPosition},
    disasm::disassemble,
    host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef},
    program::{CatchClause, Program, RegionId, RegionRun, TagId, ValueType},
    recording::{InstructionCapture, InstructionStatus, RecordLimits, Recorder, ReplayOutcome},
    trace::{ScopeKind, TraceLocation, TraceMask, TraceOutcome, TraceSink, ValueEvent},
    value::{FuncId, Value},
    verifier::{
        RegionError, RegionRunError, VerifiedProgram, VerifyConfig, VerifyError, verify_program,
    },
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
        panic!("fixture host calls are yielded and completed by the test")
    }
}

fn sig(args: &[ValueType], rets: &[ValueType]) -> FunctionSig {
    FunctionSig {
        arg_types: args.to_vec(),
        ret_types: rets.to_vec(),
    }
}

fn run(program: &VerifiedProgram, entry: FuncId, args: &[Value]) -> Result<Vec<Value>, Trap> {
    {
        let mut vm = Vm::new(NoHost, Limits::default());
        let unit = vm.install(program.clone()).unwrap();
        vm.run(FuncRef::new(unit, entry), args, TraceMask::NONE, None)
    }
    .map_err(|error| error.trap)
}

/// Emits `handler: (payload) -> payload + offset` out of line and returns its label.
///
/// A function's entry cannot be a parameterized block, so callers jump over handlers first.
fn plus(body: &mut Asm, offset: i64) -> execution_tape::asm::Label {
    let caught = body.alloc_register(ValueType::I64);
    let constant = body.alloc_register(ValueType::I64);
    let label = body.block(&[caught]);
    body.place(label).unwrap();
    body.const_i64(constant, offset)
        .i64_add(caught, caught, constant)
        .ret(0, &[caught]);
    label
}

#[test]
fn a_throw_is_caught_by_the_innermost_matching_clause() {
    let mut b = ProgramBuilder::new();
    let failed = b.declare_tag("failed", &[ValueType::I64.into()]).unwrap();
    let other = b.declare_tag("other", &[ValueType::I64.into()]).unwrap();
    let function = b
        .declare_function(sig(&[ValueType::I64, ValueType::I64], &[ValueType::I64]))
        .unwrap();
    let mut body = b.function_asm(function).unwrap();
    let (value, which) = (
        b.argument(function, 0).unwrap(),
        b.argument(function, 1).unwrap(),
    );
    let start = body.label();
    body.jmp(start);
    let outer = plus(&mut body, 100);
    let inner = plus(&mut body, 10);
    let other_tag = body.label();
    let failed_tag = body.label();
    body.place(start).unwrap();
    let selector = body.alloc_register(ValueType::Bool);
    let zero = body.alloc_register(ValueType::I64);
    body.begin_region(&[Catch::Tag(failed, outer)]);
    body.begin_region(&[Catch::Tag(failed, inner)]);
    body.const_i64(zero, 0)
        .i64_eq(selector, which, zero)
        .br(selector, failed_tag, other_tag);
    body.place(failed_tag).unwrap();
    b.emit_throw(&mut body, failed, &[value]).unwrap();
    body.place(other_tag).unwrap();
    b.emit_throw(&mut body, other, &[value]).unwrap();
    assert_eq!(body.end_region(), Some(RegionId(1)));
    assert_eq!(body.end_region(), Some(RegionId(0)));
    assert_eq!(body.end_region(), None);
    b.define_function(function, body).unwrap();
    let program = b.build_verified().unwrap();

    assert_eq!(
        run(&program, function, &[Value::I64(1), Value::I64(0)]).unwrap(),
        [Value::I64(11)],
        "the inner region's clause wins"
    );
    let Err(Trap::Uncaught(exception)) = run(&program, function, &[Value::I64(1), Value::I64(1)])
    else {
        panic!("no region catches `other`");
    };
    assert_eq!(exception.local_tag(), other);
}

#[test]
fn an_unmatched_inner_region_defers_to_its_parent_and_catch_all() {
    let mut b = ProgramBuilder::new();
    let failed = b.declare_tag("failed", &[ValueType::I64.into()]).unwrap();
    let other = b.declare_tag("other", &[]).unwrap();
    let function = b
        .declare_function(sig(&[ValueType::I64], &[ValueType::I64]))
        .unwrap();
    let mut body = b.function_asm(function).unwrap();
    let value = b.argument(function, 0).unwrap();
    let start = body.label();
    body.jmp(start);
    let outer = plus(&mut body, 100);
    let all = body.label();
    body.place(all).unwrap();
    let minus = body.alloc_register(ValueType::I64);
    body.const_i64(minus, -1).ret(0, &[minus]);
    body.place(start).unwrap();
    body.begin_region(&[Catch::Tag(failed, outer)]);
    body.begin_region(&[Catch::Tag(other, all)]);
    b.emit_throw(&mut body, failed, &[value]).unwrap();
    body.end_region();
    body.end_region();
    b.define_function(function, body).unwrap();
    let program = b.build_verified().unwrap();
    assert_eq!(
        run(&program, function, &[Value::I64(1)]).unwrap(),
        [Value::I64(101)]
    );
}

/// `entry(n)` calls `recurse(n)`, whose every activation protects a call to `recurse(n - 1)` and
/// adds its own `n` to whatever it catches. `recurse(0)` throws `failed(0)`.
fn recursion() -> (VerifiedProgram, FuncId) {
    let mut b = ProgramBuilder::new();
    let failed = b.declare_tag("failed", &[ValueType::I64.into()]).unwrap();
    let recurse = b
        .declare_function(sig(&[ValueType::I64], &[ValueType::I64]))
        .unwrap();
    let mut body = b.function_asm(recurse).unwrap();
    let n = b.argument(recurse, 0).unwrap();
    let caught = body.alloc_register(ValueType::I64);
    let handler = body.block(&[caught]);
    let bottom = body.label();
    let deeper = body.label();
    let zero = body.alloc_register(ValueType::I64);
    let one = body.alloc_register(ValueType::I64);
    let done = body.alloc_register(ValueType::Bool);
    let next = body.alloc_register(ValueType::I64);
    body.const_i64(zero, 0)
        .const_i64(one, 1)
        .i64_eq(done, n, zero)
        .br(done, bottom, deeper);
    body.place(bottom).unwrap();
    b.emit_throw(&mut body, failed, &[n]).unwrap();
    body.place(deeper).unwrap();
    body.i64_sub(next, n, one);
    body.begin_region(&[Catch::Tag(failed, handler)]);
    let [result] = b.emit_call(&mut body, recurse, &[next]).unwrap();
    body.end_region();
    body.ret(0, &[result]);
    body.place(handler).unwrap();
    body.i64_add(caught, caught, n).ret(0, &[caught]);
    b.define_function(recurse, body).unwrap();
    (b.build_verified().unwrap(), recurse)
}

#[test]
fn recursion_is_caught_by_the_nearest_activation() {
    let (program, recurse) = recursion();
    // recurse(0) throws 0; recurse(1) catches it and returns 0 + 1 to recurse(2), which returns.
    assert_eq!(
        run(&program, recurse, &[Value::I64(5)]).unwrap(),
        [Value::I64(1)]
    );
}

#[test]
fn caught_exceptions_release_every_abandoned_frame() {
    let mut b = ProgramBuilder::new();
    let failed = b.declare_tag("failed", &[]).unwrap();
    let leaf = b.declare_function(sig(&[], &[])).unwrap();
    let middle = b.declare_function(sig(&[], &[])).unwrap();
    let entry = b.declare_function(sig(&[], &[ValueType::I64])).unwrap();
    let mut body = b.function_asm(leaf).unwrap();
    b.emit_throw(&mut body, failed, &[]).unwrap();
    b.define_function(leaf, body).unwrap();
    let mut body = b.function_asm(middle).unwrap();
    let [] = b.emit_call(&mut body, leaf, &[]).unwrap();
    body.ret(0, &[]);
    b.define_function(middle, body).unwrap();
    // Loop 50 times; each iteration enters two frames that only a catch can release.
    let mut body = b.function_asm(entry).unwrap();
    let count = body.alloc_register(ValueType::I64);
    let limit = body.alloc_register(ValueType::I64);
    let one = body.alloc_register(ValueType::I64);
    let more = body.alloc_register(ValueType::Bool);
    body.const_i64(count, 0)
        .const_i64(limit, 50)
        .const_i64(one, 1);
    let head = body.label();
    let caught = body.label();
    let exit = body.label();
    body.jmp(head);
    body.place(head).unwrap();
    body.begin_region(&[Catch::All(caught)]);
    let [] = b.emit_call(&mut body, middle, &[]).unwrap();
    body.end_region();
    body.ret(0, &[count]);
    body.place(caught).unwrap();
    body.i64_add(count, count, one)
        .i64_lt(more, count, limit)
        .br(more, head, exit);
    body.place(exit).unwrap();
    body.ret(0, &[count]);
    b.define_function(entry, body).unwrap();
    let program = b.build_verified().unwrap();
    let limits = Limits {
        max_call_depth: 3,
        ..Limits::default()
    };
    let result = {
        let mut vm = Vm::new(NoHost, limits);
        let unit = vm.install(program.clone()).unwrap();
        vm.run(FuncRef::new(unit, entry), &[], TraceMask::NONE, None)
    }
    .unwrap();
    assert_eq!(result, [Value::I64(50)]);
}

#[test]
fn exceptions_cross_closures_and_indirect_calls() {
    let mut b = ProgramBuilder::new();
    let failed = b.declare_tag("failed", &[ValueType::I64.into()]).unwrap();
    let signature = b.call_sig(sig(&[ValueType::I64], &[])).unwrap();
    let body_sig = FunctionSig::closure_body(&[ValueType::I64], &[]);
    let raise = b.declare_function(body_sig).unwrap();
    let mut body = b.function_asm(raise).unwrap();
    let value = b.argument(raise, 1).unwrap();
    b.emit_throw(&mut body, failed, &[value]).unwrap();
    b.define_function(raise, body).unwrap();

    let entry = b
        .declare_function(sig(&[ValueType::I64], &[ValueType::I64]))
        .unwrap();
    let mut body = b.function_asm(entry).unwrap();
    let value = b.argument(entry, 0).unwrap();
    let start = body.label();
    body.jmp(start);
    let handler = plus(&mut body, 1000);
    body.place(start).unwrap();
    let function = body.alloc_register(ValueType::Func);
    let environment = body.alloc_register(ValueType::Agg);
    let closure = body.alloc_register(ValueType::Closure);
    body.const_func(function, raise)
        .tuple_new(environment, &[])
        .closure_new(closure, function, environment);
    body.begin_region(&[Catch::Tag(failed, handler)]);
    let [] = b
        .emit_call(
            &mut body,
            execution_tape::asm::CallTarget::Indirect {
                signature,
                callee: closure,
            },
            &[value],
        )
        .unwrap();
    body.end_region();
    body.ret(0, &[value]);
    b.define_function(entry, body).unwrap();
    let program = b.build_verified().unwrap();
    assert_eq!(
        run(&program, entry, &[Value::I64(7)]).unwrap(),
        [Value::I64(1007)]
    );
}

/// `outer` protects a yielded host call; the embedding reenters `raise`, which throws.
fn callback_fixture() -> (VerifiedProgram, FuncId, FuncId) {
    let mut b = ProgramBuilder::new();
    let failed = b.declare_tag("failed", &[ValueType::I64.into()]).unwrap();
    let wait = b.host_sig_for(
        "wait",
        HostSig {
            args: vec![],
            rets: vec![ValueType::I64],
        },
    );
    let outer = b.declare_function(sig(&[], &[ValueType::I64])).unwrap();
    let raise = b.declare_function(sig(&[ValueType::I64], &[])).unwrap();
    let mut body = b.function_asm(outer).unwrap();
    let start = body.label();
    body.jmp(start);
    let handler = plus(&mut body, 1);
    body.place(start).unwrap();
    body.begin_region(&[Catch::Tag(failed, handler)]);
    let [waited] = b.emit_call(&mut body, wait, &[]).unwrap();
    body.end_region();
    body.ret(0, &[waited]);
    b.define_function(outer, body).unwrap();
    let mut body = b.function_asm(raise).unwrap();
    let code = b.argument(raise, 0).unwrap();
    b.emit_throw(&mut body, failed, &[code]).unwrap();
    b.define_function(raise, body).unwrap();
    (b.build_verified().unwrap(), outer, raise)
}

fn drive_callback(
    program: &Arc<VerifiedProgram>,
    outer: FuncId,
    raise: FuncId,
    recorder: Option<Rc<RefCell<Recorder>>>,
) {
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(Arc::clone(program)).unwrap();
    let mut options = InvocationOptions::default().yield_host_calls();
    if let Some(recorder) = recorder {
        options = options.trace_owned(recorder.mask(), recorder);
    }
    let mut invocation = vm.start(FuncRef::new(unit, outer), &[], options).unwrap();
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::AwaitingHost,
        "outer waits in its protected host call"
    );
    invocation
        .pending_host_call()
        .unwrap()
        .call_guest(Value::Func(raise), vec![Value::I64(41)])
        .unwrap();
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::Complete,
        "outer's handler returns after the callback throws"
    );
    assert!(
        invocation.pending_host_call().is_none(),
        "the abandoned host request is gone"
    );
    assert!(
        matches!(
            invocation.result(),
            Some(Ok(values)) if values == [Value::I64(42)]
        ),
        "the handler adds one to the payload"
    );
}

#[test]
fn an_exception_from_a_guest_callback_abandons_its_host_request_and_is_caught() {
    let (program, outer, raise) = callback_fixture();
    drive_callback(&Arc::new(program), outer, raise, None);
}

#[test]
fn recorded_callback_catches_replay_with_complete_instruction_history() {
    let (program, outer, raise) = callback_fixture();
    let program = Arc::new(program);
    let recorder = Rc::new(RefCell::new(
        Recorder::new(Arc::clone(&program), RecordLimits::default()).with_instructions(
            InstructionCapture {
                max_steps: 64,
                watches: vec![],
            },
        ),
    ));
    drive_callback(&program, outer, raise, Some(recorder.clone()));
    let recording = Rc::try_unwrap(recorder).ok().unwrap().into_inner().finish();
    recording.status().unwrap();
    assert_eq!(
        recording.instruction_status(),
        Some(InstructionStatus::Complete)
    );
    let Ok(ReplayOutcome::Returned(values)) = recording.replay() else {
        panic!("replay should reproduce the caught callback exception");
    };
    assert_eq!(values.values(), &[Value::I64(42)]);
}

fn protected(body_trap: bool) -> VerifiedProgram {
    // A catch-all region around a call whose callee traps or rejects a payload contract.
    let mut b = ProgramBuilder::new();
    let integer = b.contract(Contract::Kind(ValueType::I64)).unwrap();
    let pair = b.contract(Contract::Tuple(&[Some(integer)])).unwrap();
    let shaped = b.declare_tag("shaped", &[pair.into()]).unwrap();
    let callee = b.declare_function(sig(&[ValueType::Agg], &[])).unwrap();
    let entry = b
        .declare_function(sig(&[ValueType::Agg], &[ValueType::I64]))
        .unwrap();
    let mut body = b.function_asm(callee).unwrap();
    let value = b.argument(callee, 0).unwrap();
    if body_trap {
        let zero = body.alloc_register(ValueType::I64);
        body.const_i64(zero, 0)
            .i64_div(zero, zero, zero)
            .ret(0, &[]);
    } else {
        b.emit_throw(&mut body, shaped, &[value]).unwrap();
    }
    b.define_function(callee, body).unwrap();
    let mut body = b.function_asm(entry).unwrap();
    let value = b.argument(entry, 0).unwrap();
    let all = body.label();
    let result = body.alloc_register(ValueType::I64);
    body.begin_region(&[Catch::All(all)]);
    let [] = b.emit_call(&mut body, callee, &[value]).unwrap();
    body.end_region();
    body.const_i64(result, 0).ret(0, &[result]);
    body.place(all).unwrap();
    body.const_i64(result, 1).ret(0, &[result]);
    b.define_function(entry, body).unwrap();
    b.build_verified().unwrap()
}

#[test]
fn traps_and_payload_contract_failures_are_never_caught() {
    let program = protected(true);
    let mut vm = Vm::new(NoHost, Limits::default());
    let tuple = vm.aggregates_mut().tuple_new(vec![]).unwrap();
    let unit = vm.install(program.clone()).unwrap();
    let error = vm
        .run(
            FuncRef::new(unit, FuncId(1)),
            &[Value::Agg(tuple)],
            TraceMask::NONE,
            None,
        )
        .unwrap_err();
    assert!(matches!(error.trap, Trap::DivByZero), "{error}");

    let program = protected(false);
    let mut vm = Vm::new(NoHost, Limits::default());
    let wrong = vm
        .aggregates_mut()
        .tuple_new(vec![Value::Bool(true)])
        .unwrap();
    let unit = vm.install(program.clone()).unwrap();
    let error = vm
        .run(
            FuncRef::new(unit, FuncId(1)),
            &[Value::Agg(wrong)],
            TraceMask::NONE,
            None,
        )
        .unwrap_err();
    assert!(matches!(
        error.trap,
        Trap::ContractMismatch {
            position: ContractPosition::Payload { .. },
            ..
        }
    ));
    let right = vm.aggregates_mut().tuple_new(vec![Value::I64(3)]).unwrap();
    let unit = vm.install(program.clone()).unwrap();
    assert_eq!(
        vm.run(
            FuncRef::new(unit, FuncId(1)),
            &[Value::Agg(right)],
            TraceMask::NONE,
            None
        )
        .unwrap(),
        [Value::I64(1)]
    );
    // Fuel exhaustion inside the region is a trap too.
    let mut vm = Vm::new(
        NoHost,
        Limits {
            fuel: 2,
            ..Limits::default()
        },
    );
    let right = vm.aggregates_mut().tuple_new(vec![Value::I64(3)]).unwrap();
    let unit = vm.install(program.clone()).unwrap();
    let error = vm
        .run(
            FuncRef::new(unit, FuncId(1)),
            &[Value::Agg(right)],
            TraceMask::NONE,
            None,
        )
        .unwrap_err();
    assert!(matches!(error.trap, Trap::FuelExceeded), "{error}");
}

#[test]
fn caught_payload_contracts_type_handler_projections() {
    // The handler projects its Agg parameter; only the tag's tuple contract makes that legal.
    let mut b = ProgramBuilder::new();
    let integer = b.contract(Contract::Kind(ValueType::I64)).unwrap();
    let pair = b.contract(Contract::Tuple(&[Some(integer)])).unwrap();
    let shaped = b.declare_tag("shaped", &[pair.into()]).unwrap();
    let entry = b
        .declare_function(sig(&[ValueType::I64], &[ValueType::I64]))
        .unwrap();
    let mut body = b.function_asm(entry).unwrap();
    let value = b.argument(entry, 0).unwrap();
    let tuple = body.alloc_register(ValueType::Agg);
    let caught = body.alloc_register(ValueType::Agg);
    let field = body.alloc_register(ValueType::I64);
    let handler = body.block(&[caught]);
    body.tuple_new(tuple, &[value]);
    body.begin_region(&[Catch::Tag(shaped, handler)]);
    b.emit_throw(&mut body, shaped, &[tuple]).unwrap();
    body.end_region();
    body.place(handler).unwrap();
    body.tuple_get(field, caught, 0).ret(0, &[field]);
    b.define_function(entry, body).unwrap();
    let program = b.build_verified().unwrap();
    assert_eq!(
        run(&program, entry, &[Value::I64(9)]).unwrap(),
        [Value::I64(9)]
    );
}

/// `caller` protects a call that writes `r_result`, after initializing `r_before` in the same
/// block. The handler reads one of them.
fn handler_reads(read_result: bool) -> Result<Program, VerifyError> {
    let mut b = ProgramBuilder::new();
    let failed = b.declare_tag("failed", &[]).unwrap();
    let callee = b.declare_function(sig(&[], &[ValueType::I64])).unwrap();
    let caller = b.declare_function(sig(&[], &[ValueType::I64])).unwrap();
    let mut body = b.function_asm(callee).unwrap();
    b.emit_throw(&mut body, failed, &[]).unwrap();
    b.define_function(callee, body).unwrap();
    let mut body = b.function_asm(caller).unwrap();
    let before = body.alloc_register(ValueType::I64);
    let handler = body.label();
    body.const_i64(before, 5);
    body.begin_region(&[Catch::All(handler)]);
    let [result] = b.emit_call(&mut body, callee, &[]).unwrap();
    body.end_region();
    body.ret(0, &[result]);
    body.place(handler).unwrap();
    body.ret(0, &[if read_result { result } else { before }]);
    b.define_function(caller, body).unwrap();
    let program = b.build();
    verify_program(&program, &VerifyConfig::default()).map(|()| program)
}

#[test]
fn handlers_see_writes_before_the_raising_instruction_but_not_its_results() {
    let program = handler_reads(false).unwrap();
    let verified =
        execution_tape::verifier::verify_program_owned(program, &VerifyConfig::default()).unwrap();
    assert_eq!(run(&verified, FuncId(1), &[]).unwrap(), [Value::I64(5)]);
    assert!(matches!(
        handler_reads(true),
        Err(VerifyError::UninitializedRead { func: 1, .. })
    ));
}

#[derive(Default)]
struct Observe {
    events: Vec<String>,
}
impl TraceSink for Observe {
    fn mask(&self) -> TraceMask {
        TraceMask::RUN | TraceMask::CALL | TraceMask::VALUES | TraceMask::INSTR
    }
    fn values(&mut self, _: &Program, event: ValueEvent<'_>) {
        if let ValueEvent::Caught {
            location,
            tag,
            handler,
            depth,
            region,
            clause,
            ..
        } = event
        {
            self.events.push(format!(
                "caught tag#{} from f{}:{} at f{}:{} depth {depth} region#{} clause {clause}",
                tag.0, location.func.0, location.pc, handler.func.0, handler.pc, region.0
            ));
        }
    }
    fn instr_end(
        &mut self,
        _: &Program,
        location: TraceLocation,
        next: Option<TraceLocation>,
        outcome: TraceOutcome<'_>,
    ) {
        if let TraceOutcome::Caught { depth } = outcome {
            self.events.push(format!(
                "instr f{}:{} caught at depth {depth} next {:?}",
                location.func.0,
                location.pc,
                next.map(|next| (next.func.0, next.pc))
            ));
        }
    }
    fn scope_exit(
        &mut self,
        _: &Program,
        kind: ScopeKind,
        _: usize,
        _: FuncId,
        _: u32,
        _: Option<u64>,
        outcome: TraceOutcome<'_>,
    ) {
        if let (ScopeKind::CallFrame { func, .. }, TraceOutcome::Caught { depth }) = (kind, outcome)
        {
            self.events
                .push(format!("exit f{} caught at depth {depth}", func.0));
        }
    }
}

#[test]
fn trace_and_recording_explain_and_replay_a_caught_exception() {
    let (program, recurse) = recursion();
    let program = Arc::new(program);
    let mut sink = Observe::default();
    {
        let mut vm = Vm::new(NoHost, Limits::default());
        let unit = vm.install(program.clone()).unwrap();
        vm.run(
            FuncRef::new(unit, recurse),
            &[Value::I64(2)],
            sink.mask(),
            Some(&mut sink),
        )
    }
    .unwrap();
    // recurse(2) -> recurse(1) -> recurse(0) throws; recurse(1) at depth 2 catches.
    assert_eq!(sink.events.len(), 3, "{:#?}", sink.events);
    assert!(
        sink.events[0].starts_with("caught tag#0 from f0:"),
        "{:#?}",
        sink.events
    );
    assert!(
        sink.events[0].contains("depth 2 region#0 clause 0"),
        "{:#?}",
        sink.events
    );
    assert!(
        sink.events[1].contains("caught at depth 2 next Some((0,"),
        "{:#?}",
        sink.events
    );
    assert_eq!(sink.events[2], "exit f0 caught at depth 2");

    let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default())
        .with_instructions(InstructionCapture {
            max_steps: 256,
            watches: vec![],
        });
    let result = {
        let mut vm = Vm::new(NoHost, Limits::default());
        let unit = vm.install(program.clone()).unwrap();
        vm.run(
            FuncRef::new(unit, recurse),
            &[Value::I64(3)],
            recorder.mask(),
            Some(&mut recorder),
        )
    }
    .unwrap();
    assert_eq!(result, [Value::I64(1)]);
    let recording = recorder.finish();
    recording.status().unwrap();
    assert_eq!(
        recording.instruction_status(),
        Some(InstructionStatus::Complete)
    );
    let Ok(ReplayOutcome::Returned(values)) = recording.replay() else {
        panic!("replay should reproduce the caught exception");
    };
    assert_eq!(values.values(), &[Value::I64(1)]);
}

#[test]
fn regions_round_trip_and_disassemble_with_their_clauses() {
    let (program, _) = recursion();
    let program = program.program();
    assert_eq!(&Program::decode(&program.encode()).unwrap(), program);
    let text = disassemble(program).to_string();
    assert!(
        text.contains("region#0 covers [") && text.contains(": catch tag#0(\"failed\") -> @L"),
        "{text}"
    );
}

fn malformed(edit: impl FnOnce(&mut Program)) -> VerifyError {
    let (program, _) = recursion();
    let mut program = program.into_program();
    edit(&mut program);
    verify_program(&program, &VerifyConfig::default()).unwrap_err()
}

#[test]
fn verifier_names_the_region_and_clause_it_rejects() {
    assert_eq!(
        malformed(|p| p.catch_clauses[0] = CatchClause::Tag {
            tag: TagId(7),
            target: p.catch_clauses[0].target(),
        }),
        VerifyError::InvalidRegion {
            func: 0,
            region: 0,
            error: RegionError::ClauseTag { clause: 0, tag: 7 },
        }
    );
    assert!(matches!(
        malformed(|p| p.catch_clauses[0] = CatchClause::All {
            target: p.catch_clauses[0].target(),
        }),
        VerifyError::InvalidRegion {
            func: 0,
            region: 0,
            error: RegionError::ClauseParams {
                clause: 0,
                tag: None,
                expected: 0,
                actual: 1,
                ..
            },
        }
    ));
    let error = malformed(|p| {
        p.catch_clauses[0] = CatchClause::Tag {
            tag: TagId(0),
            target: 1,
        }
    });
    assert!(
        matches!(
            error,
            VerifyError::InvalidRegion {
                error: RegionError::ClauseTarget {
                    clause: 0,
                    target: 1
                },
                ..
            }
        ),
        "{error}"
    );
    assert!(error.to_string().contains("region#0"), "{error}");
    assert_eq!(
        malformed(|p| p.regions[0].parent = Some(RegionId(0))),
        VerifyError::InvalidRegion {
            func: 0,
            region: 0,
            error: RegionError::Parent { parent: 0 },
        }
    );
    assert_eq!(
        malformed(|p| p.region_runs[0].region = RegionId(3)),
        VerifyError::InvalidRegionRun {
            func: 0,
            run: 0,
            error: RegionRunError::UnknownRegion { region: 3 },
        }
    );
    assert_eq!(
        malformed(|p| {
            let run = p.region_runs[0];
            p.region_runs[0] = RegionRun {
                end_pc: run.start_pc,
                ..run
            };
        }),
        VerifyError::InvalidRegionRun {
            func: 0,
            run: 0,
            error: RegionRunError::Range,
        }
    );
}

#[test]
fn clause_parameters_must_match_the_payload_kinds() {
    let mut b = ProgramBuilder::new();
    let failed = b.declare_tag("failed", &[ValueType::I64.into()]).unwrap();
    let function = b.declare_function(sig(&[], &[])).unwrap();
    let mut body = b.function_asm(function).unwrap();
    let flag = body.alloc_register(ValueType::Bool);
    let handler = body.block(&[flag]);
    let value = body.alloc_register(ValueType::I64);
    body.const_i64(value, 1);
    body.begin_region(&[Catch::Tag(failed, handler)]);
    b.emit_throw(&mut body, failed, &[value]).unwrap();
    body.end_region();
    body.place(handler).unwrap();
    body.ret(0, &[]);
    b.define_function(function, body).unwrap();
    let error = verify_program(&b.build(), &VerifyConfig::default()).unwrap_err();
    assert_eq!(
        error,
        VerifyError::InvalidRegion {
            func: 0,
            region: 0,
            error: RegionError::ClauseParamKind {
                clause: 0,
                tag: 0,
                name: "failed".into(),
                index: 0,
                expected: ValueType::I64,
                actual: ValueType::Bool,
            },
        }
    );
    assert!(error.to_string().contains("tag#0 \"failed\""), "{error}");
}
