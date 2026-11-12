// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Typed exception tags and `throw`.
//!
//! No protected region exists yet, so every raised exception leaves the invocation as
//! `Trap::Uncaught` with a payload snapshot that survives rollback.

use execution_tape::value::FuncRef;
use execution_tape::value::UnitId;
use execution_tape::{
    asm::{BuildError, FunctionSig, ProgramBuilder, TagBuildError},
    contract::{Contract, ContractId, ContractPosition},
    disasm::disassemble,
    host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef},
    program::{Program, TagId, ValueType},
    recording::{RecordLimits, Recorder, ReplayOutcome},
    trace::{ScopeKind, TraceLocation, TraceMask, TraceOutcome, TraceSink, ValueEvent},
    value::{FuncId, Value},
    verifier::{MetadataTable, VerifiedProgram, VerifyConfig, VerifyError, verify_program},
    vm::{InvocationOptions, Limits, RunStatus, Trap, TrapInfo, Vm},
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
        panic!("fixture host calls are yielded and completed by the test")
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
    failed: TagId,
    entry: FuncId,
    raise: FuncId,
    /// `pc` of the `throw` instruction in `raise`.
    throw_pc: u32,
}

/// `entry(code)` calls `raise(code, "boom")`, which throws `failed(code, "boom", (code,))`.
fn fixture() -> Fixture {
    let mut b = ProgramBuilder::new();
    let integer = b.contract(Contract::Kind(ValueType::I64)).unwrap();
    let pair = b.contract(Contract::Tuple(&[Some(integer)])).unwrap();
    let failed = b
        .declare_tag(
            "failed",
            &[
                ValueType::I64.into(),
                ValueType::Str.into(),
                execution_tape::asm::ContractRef::Id(pair),
            ],
        )
        .unwrap();
    let entry = b
        .declare_function(sig(&[ValueType::I64], &[ValueType::I64]))
        .unwrap();
    let raise = b
        .declare_function(sig(&[ValueType::I64, ValueType::Str], &[ValueType::I64]))
        .unwrap();
    let boom = b.const_str("boom");

    let mut body = b.function_asm(entry).unwrap();
    let code = b.argument(entry, 0).unwrap();
    let text = body.alloc_register(ValueType::Str);
    body.const_pool(text, boom);
    let [result] = b.emit_call(&mut body, raise, &[code, text]).unwrap();
    body.ret(0, &[result]);
    b.define_function(entry, body).unwrap();

    let mut body = b.function_asm(raise).unwrap();
    let code = b.argument(raise, 0).unwrap();
    let text = b.argument(raise, 1).unwrap();
    let tuple = body.alloc_register(ValueType::Agg);
    body.tuple_new(tuple, &[code]);
    let throw_pc = body.pc();
    b.emit_throw(&mut body, failed, &[code, text, tuple])
        .unwrap();
    b.define_function(raise, body).unwrap();

    Fixture {
        program: Arc::new(b.build_verified().unwrap()),
        failed,
        entry,
        raise,
        throw_pc,
    }
}

fn assert_uncaught(fixture: &Fixture, error: &TrapInfo, code: i64) {
    assert_eq!(
        (error.func, error.pc),
        (fixture.raise, fixture.throw_pc),
        "the trap is located at the throw"
    );
    let Trap::Uncaught(exception) = &error.trap else {
        panic!("expected an uncaught exception, got {error}");
    };
    assert_eq!(
        exception.local_tag(),
        Some(fixture.failed),
        "thrown tag identity"
    );
    assert_eq!(exception.name(), "failed", "thrown tag name");
    let payload = exception.payload();
    let [Value::I64(actual), Value::Str(text), Value::Agg(tuple)] = payload.values() else {
        panic!("unexpected payload {:?}", payload.values());
    };
    assert_eq!((*actual, text.as_str()), (code, "boom"), "scalar payload");
    assert!(
        matches!(
            payload.aggregates().tuple_get(*tuple, 0),
            Ok(Value::I64(value)) if value == code
        ),
        "the payload tuple resolves in the snapshot's own heap"
    );
    assert_eq!(
        error.to_string(),
        format!(
            "trap at u0:f{} pc={}: uncaught exception tag#0 (\"failed\") with 3 payload values",
            fixture.raise.0, fixture.throw_pc
        ),
        "trap display names the tag"
    );
}

#[test]
fn uncaught_exception_detaches_its_payload_before_rollback() {
    let f = fixture();
    let mut vm = Vm::new(NoHost, Limits::default());
    let before = vm.aggregates().len_u32();
    let unit = vm.install(f.program.clone()).unwrap();
    let error = vm
        .run(
            FuncRef::new(unit, f.entry),
            &[Value::I64(7)],
            TraceMask::NONE,
            None,
        )
        .unwrap_err();
    assert_uncaught(&f, &error, 7);
    assert_eq!(
        vm.aggregates().len_u32(),
        before,
        "the failed invocation's aggregates are rolled back; the payload survives in the trap"
    );
    // The VM remains usable after an uncaught exception.
    let again = vm
        .run(
            FuncRef::new(unit, f.entry),
            &[Value::I64(8)],
            TraceMask::NONE,
            None,
        )
        .unwrap_err();
    assert_uncaught(&f, &again, 8);
}

#[test]
fn resumable_invocations_report_the_same_uncaught_trap() {
    let f = fixture();
    let mut vm = Vm::new(NoHost, Limits::default());
    let args = [Value::I64(3)];
    let unit = vm.install(f.program.clone()).unwrap();
    let mut run = vm
        .start(
            FuncRef::new(unit, f.entry),
            &args,
            InvocationOptions::default(),
        )
        .unwrap();
    let error = loop {
        match run.resume(1) {
            Ok(RunStatus::Suspended) => {}
            Ok(status) => panic!("unexpected status {status:?}"),
            Err(error) => break error,
        }
    };
    assert_uncaught(&f, &error, 3);
    assert!(matches!(run.result(), Some(Err(trap)) if matches!(trap.trap, Trap::Uncaught(_))));
}

#[test]
fn an_exception_escaping_a_guest_callback_ends_the_whole_invocation() {
    // `outer` waits in a yielded host request; the embedding reenters `raise`, which throws.
    // Without a protected region the exception abandons the pending request and its callers.
    let mut b = ProgramBuilder::new();
    let failed = b.declare_tag("failed", &[ValueType::I64.into()]).unwrap();
    let wait = b.host_sig_for(
        "wait",
        HostSig {
            args: vec![],
            rets: vec![],
        },
    );
    let outer = b.declare_function(sig(&[], &[])).unwrap();
    let raise = b.declare_function(sig(&[ValueType::I64], &[])).unwrap();
    let mut body = b.function_asm(outer).unwrap();
    let [] = b.emit_call(&mut body, wait, &[]).unwrap();
    body.ret(0, &[]);
    b.define_function(outer, body).unwrap();
    let mut body = b.function_asm(raise).unwrap();
    let code = b.argument(raise, 0).unwrap();
    let throw_pc = body.pc();
    b.emit_throw(&mut body, failed, &[code]).unwrap();
    b.define_function(raise, body).unwrap();
    let program = b.build_verified().unwrap();

    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(program.clone()).unwrap();
    let mut run = vm
        .start(
            FuncRef::new(unit, outer),
            &[],
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    run.pending_host_call()
        .unwrap()
        .call_guest(
            Value::Func(FuncRef::new(UnitId(0), raise)),
            vec![Value::I64(5)],
        )
        .unwrap();
    let error = run.resume(u64::MAX).unwrap_err();
    assert_eq!((error.func, error.pc), (raise, throw_pc));
    let Trap::Uncaught(exception) = &error.trap else {
        panic!("expected an uncaught exception, got {error}");
    };
    assert_eq!(exception.payload().values(), &[Value::I64(5)]);
    assert!(run.pending_host_call().is_none());
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
        if let ValueEvent::Thrown {
            location,
            tag,
            values,
            aggregates,
            ..
        } = event
        {
            let Value::Agg(tuple) = values[2] else {
                panic!("payload tuple");
            };
            assert!(aggregates.tuple_get(tuple, 0).is_ok(), "live at the throw");
            self.events.push(format!(
                "thrown tag#{} at f{}:{} {:?}",
                tag.0,
                location.func.0,
                location.pc,
                &values[..2]
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
        if let TraceOutcome::Trap(error) = outcome {
            assert!(next.is_none(), "a trapped throw has no successor");
            self.events.push(format!(
                "instr f{}:{} trapped at f{}:{}",
                location.func.0, location.pc, error.func.0, error.pc
            ));
        }
    }
    fn scope_exit(
        &mut self,
        _: &Program,
        kind: ScopeKind,
        _: usize,
        location: TraceLocation,
        _: Option<u64>,
        outcome: TraceOutcome<'_>,
    ) {
        let (func, pc) = (location.func, location.pc);
        let ScopeKind::CallFrame { func: frame, .. } = kind else {
            return;
        };
        let outcome = match outcome {
            TraceOutcome::Trap(error) if matches!(error.trap, Trap::Uncaught(_)) => "uncaught",
            other => panic!("unexpected scope outcome {other:?}"),
        };
        self.events.push(format!(
            "exit f{} {outcome} at f{}:{pc}",
            frame.func.0, func.0
        ));
    }
    fn run_end(&mut self, _: &Program, outcome: TraceOutcome<'_>) {
        assert!(
            matches!(outcome, TraceOutcome::Trap(error) if matches!(error.trap, Trap::Uncaught(_))),
            "the run ends with the uncaught exception"
        );
        self.events.push("run_end".into());
    }
}

#[test]
fn trace_observes_the_payload_and_closes_every_scope_at_the_throw() {
    let f = fixture();
    let mut sink = Observe::default();
    let error = {
        let mut vm = Vm::new(NoHost, Limits::default());
        let unit = vm.install(f.program.clone()).unwrap();
        vm.run(
            FuncRef::new(unit, f.entry),
            &[Value::I64(9)],
            sink.mask(),
            Some(&mut sink),
        )
    }
    .unwrap_err();
    assert_uncaught(&f, &error, 9);
    let (raise, entry, pc) = (f.raise.0, f.entry.0, f.throw_pc);
    assert_eq!(
        sink.events,
        [
            format!("thrown tag#0 at f{raise}:{pc} [I64(9), Str(\"boom\")]"),
            format!("instr f{raise}:{pc} trapped at f{raise}:{pc}"),
            format!("exit f{raise} uncaught at f{raise}:{pc}"),
            format!("exit f{entry} uncaught at f{raise}:{pc}"),
            "run_end".into(),
        ]
    );
}

#[test]
fn recorded_uncaught_exceptions_replay_with_their_payload() {
    let f = fixture();
    let mut recorder = Recorder::new(Arc::clone(&f.program), RecordLimits::default());
    let error = {
        let mut vm = Vm::new(NoHost, Limits::default());
        let unit = vm.install(f.program.clone()).unwrap();
        vm.run(
            FuncRef::new(unit, f.entry),
            &[Value::I64(4)],
            recorder.mask(),
            Some(&mut recorder),
        )
    }
    .unwrap_err();
    assert_uncaught(&f, &error, 4);
    let recording = recorder.finish();
    recording.status().unwrap();
    assert_eq!(
        recording.outcome_location(),
        Some(TraceLocation {
            unit: UnitId(0),
            func: f.raise,
            pc: f.throw_pc
        })
    );
    let Ok(ReplayOutcome::Trapped(replayed)) = recording.replay() else {
        panic!("replay should reproduce the uncaught exception");
    };
    assert_uncaught(&f, &replayed, 4);
}

fn shaped_program(statically_wrong: bool) -> Result<(VerifiedProgram, TagId, u32), BuildError> {
    let mut b = ProgramBuilder::new();
    let integer = b.contract(Contract::Kind(ValueType::I64)).unwrap();
    let pair: ContractId = b.contract(Contract::Tuple(&[Some(integer)])).unwrap();
    let shaped = b.declare_tag("shaped", &[pair.into()])?;
    let entry = b.declare_function(sig(&[ValueType::Agg], &[]))?;
    let mut body = b.function_asm(entry)?;
    let mut value = b.argument(entry, 0)?;
    if statically_wrong {
        let flag = body.alloc_register(ValueType::Bool);
        value = body.alloc_register(ValueType::Agg);
        body.const_bool(flag, true).tuple_new(value, &[flag]);
    }
    let pc = body.pc();
    b.emit_throw(&mut body, shaped, &[value])?;
    b.define_function(entry, body)?;
    Ok((b.build_verified()?, shaped, pc))
}

#[test]
fn payload_contracts_are_checked_statically_and_where_the_exception_is_raised() {
    let Err(BuildError::Verify(VerifyError::ContractMismatch { pc, position, .. })) =
        shaped_program(true)
    else {
        panic!("a statically contradicted payload must fail verification");
    };
    assert!(matches!(position, ContractPosition::Payload { tag, index: 0 } if tag.0 == 0));
    assert!(pc > 0);

    let (program, shaped, pc) = shaped_program(false).unwrap();
    let mut vm = Vm::new(NoHost, Limits::default());
    let wrong = vm
        .aggregates_mut()
        .tuple_new(vec![Value::Bool(true)])
        .unwrap();
    let unit = vm.install(program.clone()).unwrap();
    let error = vm
        .run(
            FuncRef::new(unit, FuncId(0)),
            &[Value::Agg(wrong)],
            TraceMask::NONE,
            None,
        )
        .unwrap_err();
    assert_eq!(error.pc, pc);
    assert!(matches!(
        error.trap,
        Trap::ContractMismatch { position: ContractPosition::Payload { tag, index: 0 }, .. }
            if tag == shaped
    ));
    assert!(error.to_string().contains("throw tag#0: payload[0]"));

    let right = vm.aggregates_mut().tuple_new(vec![Value::I64(1)]).unwrap();
    let error = vm
        .run(
            FuncRef::new(unit, FuncId(0)),
            &[Value::Agg(right)],
            TraceMask::NONE,
            None,
        )
        .unwrap_err();
    assert!(matches!(error.trap, Trap::Uncaught(_)));
}

#[test]
fn tags_round_trip_and_disassemble_with_their_names() {
    let f = fixture();
    let program = f.program.program();
    assert_eq!(&Program::decode(&program.encode()).unwrap(), program);
    assert_eq!(program.tag_name(f.failed), Some("failed"));
    let text = disassemble(program).to_string();
    assert!(
        text.starts_with("tag#0 \"failed\" ; payload=[I64, Str, Agg]\n"),
        "{text}"
    );
    assert!(
        text.contains(
            "throw eff=r0, tag#0(\"failed\"), payload=[r1, r2, r3] ; payload=[I64, Str, Agg]"
        ),
        "{text}"
    );
}

#[test]
fn builder_rejects_invalid_tags_and_throws_without_mutation() {
    let mut b = ProgramBuilder::new();
    let failed = b.declare_tag("failed", &[ValueType::I64.into()]).unwrap();
    assert_eq!(
        b.declare_tag("failed", &[ValueType::Bool.into()]),
        Err(TagBuildError::DuplicateName {
            name: "failed".into(),
            existing: failed,
        })
    );
    // Equal payloads still declare distinct tags; an empty payload needs no annotation.
    assert_ne!(
        b.declare_tag("other", &[ValueType::I64.into()]).unwrap(),
        failed
    );
    assert_eq!(b.declare_tag("abort", &[]).unwrap().0, 2);

    let function = b.declare_function(sig(&[ValueType::Bool], &[])).unwrap();
    let mut body = b.function_asm(function).unwrap();
    let flag = b.argument(function, 0).unwrap();
    let pc = body.pc();
    assert_eq!(
        b.emit_throw(&mut body, TagId(9), &[flag]),
        Err(TagBuildError::UnknownTag(TagId(9)))
    );
    assert_eq!(
        b.emit_throw(&mut body, failed, &[]),
        Err(TagBuildError::Payload {
            tag: failed,
            expected: 1,
            actual: 0,
        })
    );
    assert_eq!(
        b.emit_throw(&mut body, failed, &[flag]),
        Err(TagBuildError::PayloadKind {
            tag: failed,
            register: flag,
            index: 0,
            expected: ValueType::I64,
            actual: ValueType::Bool,
        })
    );
    assert_eq!(
        b.emit_throw(&mut body, failed, &[7]),
        Err(TagBuildError::Register(7))
    );
    assert_eq!(body.pc(), pc, "rejected throws emit nothing");
}

#[test]
fn verifier_names_the_tag_in_throw_diagnostics() {
    let base = fixture().program.program().clone();
    let cfg = VerifyConfig::default();

    // Raw bytecode bypasses builder checks: an unknown tag, a short payload, a wrong kind.
    let mut b = ProgramBuilder::new();
    let failed = b.declare_tag("failed", &[ValueType::I64.into()]).unwrap();
    let function = b.declare_function(sig(&[ValueType::Bool], &[])).unwrap();
    let flag = b.argument(function, 0).unwrap();
    let mut cases = Vec::new();
    for (tag, payload) in [(TagId(5), vec![]), (failed, vec![]), (failed, vec![flag])] {
        let mut body = b.function_asm(function).unwrap();
        body.throw(0, tag, &payload);
        let mut builder = b.clone();
        builder.define_function(function, body).unwrap();
        cases.push(verify_program(&builder.build(), &cfg).unwrap_err());
    }
    assert_eq!(
        cases[0],
        VerifyError::TagOutOfBounds {
            func: 0,
            pc: 0,
            tag: 5
        }
    );
    assert_eq!(
        cases[1],
        VerifyError::ThrowArityMismatch {
            func: 0,
            pc: 0,
            tag: 0,
            name: "failed".into(),
            expected: 1,
            actual: 0,
        }
    );
    assert_eq!(
        cases[2],
        VerifyError::ThrowPayloadMismatch {
            func: 0,
            pc: 0,
            tag: 0,
            name: "failed".into(),
            index: 0,
            reg: flag,
            expected: ValueType::I64,
            actual: ValueType::Bool,
        }
    );
    assert!(cases[2].to_string().contains("tag#0 (\"failed\")"));
    assert_eq!(cases[2].function(), Some(FuncId(0)));
    assert_eq!(cases[2].pc(), Some(0));

    // Tag table entries are checked even when nothing throws them.
    let mut duplicate = base.clone();
    let mut copy = duplicate.tags[0].clone();
    duplicate.tags.push(copy.clone());
    assert_eq!(
        verify_program(&duplicate, &cfg),
        Err(VerifyError::DuplicateTagName {
            tag: 1,
            first: 0,
            name: "failed".into(),
        })
    );
    copy.payload.offset = u32::MAX;
    let mut malformed = base;
    malformed.tags.push(copy);
    assert!(matches!(
        verify_program(&malformed, &cfg),
        Err(VerifyError::InvalidMetadata {
            table: MetadataTable::Tags,
            entry: 1,
            ..
        })
    ));
}
