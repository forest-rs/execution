// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use super::super::program;
use super::*;
use execution_tape::{
    recording::{CaptureFault, ControlAction, InstructionStatus, ReplayError, ReplayOutcome},
    source::SourceError,
    vm::{ControlError, Trap, TrapInfo},
};

fn until_wait(runtime: &mut Runtime) {
    for _ in 0..10_000 {
        match runtime.poll(8).unwrap() {
            Status::Running => {}
            Status::Waiting => return,
            other => panic!("expected native cleanup wait, got {other:?}"),
        }
    }
    panic!("cleanup wait not reached");
}
fn until_cannot_return(runtime: &mut Runtime) -> CannotReturn {
    for _ in 0..10_000 {
        match runtime.poll(8).unwrap() {
            Status::Running => {}
            Status::Waiting => runtime.finish_native_wait().unwrap(),
            Status::CannotReturn(error) => return error,
            other => panic!("expected escaped return, got {other:?}"),
        }
    }
    panic!("escaped return not reached");
}
fn drive(runtime: &mut Runtime, quantum: u64, recovery: i64) {
    for _ in 0..10_000 {
        match runtime.poll(quantum).unwrap() {
            Status::Running => {}
            Status::Waiting => runtime.finish_native_wait().unwrap(),
            Status::CannotReturn(_) => runtime.supply_return_fallback(recovery).unwrap(),
            Status::Complete => return,
        }
    }
    panic!("finite block story did not complete");
}
fn assert_session(session: &Session, fixture: &program::Fixture, expected: [i64; 4]) {
    session.recording.status().unwrap();
    assert_eq!(
        session.recording.instruction_status(),
        Some(InstructionStatus::Complete)
    );
    let ReplayOutcome::Returned(snapshot) = session.recording.replay().unwrap() else {
        panic!("guest replay returned");
    };
    assert_eq!(
        decode_outputs(snapshot.values(), snapshot.aggregates()).unwrap(),
        expected
    );
    let sources = fixture
        .sources
        .bind(session.recording.program().program())
        .unwrap();
    let check = |location| {
        assert!(
            sources.lookup(location).is_some(),
            "actual source at {location:?}"
        );
    };
    for event in &session.events {
        check(event.location);
        if let Policy::CannotReturn(error) = &event.policy {
            check(error.block);
            check(error.home);
        }
    }
    for step in session.recording.instructions() {
        check(step.location);
    }
    for call in session.recording.host_calls() {
        check(call.location);
    }
    for command in session.recording.control_commands() {
        check(command.location);
        match command.action {
            ControlAction::ReturnFrom {
                target_location, ..
            } => check(target_location),
            ControlAction::CallGuest {
                returned: Some(returned),
                ..
            } => check(returned.location),
            _ => {}
        }
    }
}

#[test]
fn shared_escaping_blocks_and_lexical_returns_compose_with_ensure_and_checked_replay() {
    for non_local in [false, true] {
        for quantum in [1, 128] {
            let fixture = program::compile(non_local).unwrap();
            let mut runtime = Runtime::new(&fixture, Limits::default()).unwrap();
            drive(&mut runtime, quantum, -1);
            assert_eq!(runtime.outputs().unwrap(), [41, 42, 42, -1]);
            assert_eq!(runtime.run.host().value, Some(42));
            assert!(runtime.protections.is_empty());
            assert!(runtime.waiting.is_empty());
            assert!(runtime.transfer.is_none());
            let session = runtime.into_session();
            assert_eq!(
                session.cleanups,
                [2, 1],
                "nested ensure is LIFO and exactly once"
            );
            assert_eq!(
                session.after_ensure, !non_local,
                "only ^ leaves the lexical home"
            );
            assert_eq!(
                session
                    .recording
                    .control_commands()
                    .filter(|command| matches!(command.action, ControlAction::ReturnFrom { .. }))
                    .count(),
                usize::from(non_local)
            );
            assert_session(&session, &fixture, [41, 42, 42, -1]);
        }
    }
}

#[test]
fn suspended_cleanup_retires_its_binding_but_keeps_physical_signaling_frames() {
    let fixture = program::compile(true).unwrap();
    let mut runtime = Runtime::new(&fixture, Limits::default()).unwrap();
    until_wait(&mut runtime);
    assert_eq!(
        runtime.protections.len(),
        1,
        "inner ensure is consumed before cleanup starts"
    );
    assert_eq!(runtime.transfer.as_ref().unwrap().value, 42);
    assert!(
        runtime.run.host().cleanups.is_empty(),
        "wait precedes inner cleanup effect"
    );
    let frames = runtime.paused_frames();
    let sources = fixture.sources.bind(fixture.program.program()).unwrap();
    assert_eq!(
        sources.lookup(frames[0].location).unwrap().text(),
        "Demo wait"
    );
    assert!(frames.iter().any(|frame| {
        fixture
            .program
            .program()
            .function_name(frame.location.func.0)
            .is_some_and(|name| name == "answerUsing: / return block")
    }));
    let fuel = runtime.run.remaining_fuel();
    let work = runtime.run.data_work();
    for _ in 0..5 {
        assert!(matches!(runtime.poll(0).unwrap(), Status::Waiting));
    }
    assert_eq!(runtime.run.remaining_fuel(), fuel);
    assert_eq!(runtime.run.data_work(), work);
    runtime.finish_native_wait().unwrap();
    drive(&mut runtime, 128, -1);
    let session = runtime.into_session();
    assert_eq!(session.cleanups, [2, 1]);
    assert_session(&session, &fixture, [41, 42, 42, -1]);
}

#[test]
fn expired_home_is_a_recoverable_language_condition_with_both_source_locations() {
    let fixture = program::compile(true).unwrap();
    let mut runtime = Runtime::new(&fixture, Limits::default()).unwrap();
    let error = until_cannot_return(&mut runtime);
    assert_eq!(error.home_name, "makeEscaped:");
    assert_eq!(error.attempted_value, 42);
    assert_ne!(error.home, error.block);
    let sources = fixture.sources.bind(fixture.program.program()).unwrap();
    assert_eq!(
        sources.lookup(error.block).unwrap().text(),
        "[ ^ counter value ]"
    );
    assert_eq!(
        sources.lookup(error.home).unwrap().text(),
        "makeEscaped: counter"
    );
    let target = runtime
        .homes
        .iter()
        .find(|home| home.site.activation.ordinal() == error.home_activation)
        .unwrap()
        .site
        .activation
        .clone();
    let fuel = runtime.run.remaining_fuel();
    let work = runtime.run.data_work();
    assert!(
        matches!(
            runtime
                .run
                .pending_host_call()
                .unwrap()
                .return_from(&target, vec![]),
            Err(ControlError::ExpiredActivation)
        ),
        "core independently rejects the expired home before admitting values"
    );
    assert!(
        runtime.finish_native_wait().is_err(),
        "a native completion cannot bypass the condition"
    );
    for _ in 0..5 {
        assert!(matches!(runtime.poll(0).unwrap(), Status::CannotReturn(_)));
    }
    assert_eq!(runtime.run.remaining_fuel(), fuel);
    assert_eq!(runtime.run.data_work(), work);
    assert!(runtime.run.result().is_none());
    runtime.supply_return_fallback(-7).unwrap();
    drive(&mut runtime, 128, -7);
    let session = runtime.into_session();
    assert_eq!(
        session.cleanups,
        [2, 1],
        "handling the expired return repeats no old cleanup"
    );
    assert_session(&session, &fixture, [41, 42, 42, -7]);
}

#[test]
fn callbacks_and_cleanup_share_the_original_hard_fuel_budget() {
    let fixture = program::compile(true).unwrap();
    let mut baseline = Runtime::new(&fixture, Limits::default()).unwrap();
    until_wait(&mut baseline);
    let spent = Limits::default().fuel - baseline.run.remaining_fuel();
    let mut limited = Runtime::new(
        &fixture,
        Limits {
            fuel: spent,
            ..Limits::default()
        },
    )
    .unwrap();
    until_wait(&mut limited);
    assert_eq!(limited.run.remaining_fuel(), 0);
    limited.finish_native_wait().unwrap();
    let failure = limited
        .poll(128)
        .unwrap_err()
        .downcast::<TrapInfo>()
        .unwrap();
    assert!(matches!(failure.trap, Trap::FuelExceeded));
    assert!(limited.protections.is_empty());
    assert!(limited.waiting.is_empty());
    let session = limited.into_session();
    assert!(
        session.cleanups.is_empty(),
        "no further guest cleanup runs after hard failure"
    );
    session.recording.status().unwrap();
    assert!(
        matches!(session.recording.replay().unwrap(), ReplayOutcome::Trapped(trap)
        if matches!(trap.trap, Trap::FuelExceeded))
    );
}

#[test]
fn cancellation_during_cleanup_runs_no_guest_code_and_leaves_incomplete_evidence() {
    let fixture = program::compile(true).unwrap();
    let mut runtime = Runtime::new(&fixture, Limits::default()).unwrap();
    until_wait(&mut runtime);
    let session = runtime.into_session();
    assert!(session.cleanups.is_empty());
    assert!(
        session
            .events
            .iter()
            .any(|event| matches!(event.policy, Policy::CleanupStarted { .. }))
    );
    assert!(
        !session
            .events
            .iter()
            .any(|event| matches!(event.policy, Policy::CleanupFinished { .. }))
    );
    assert!(matches!(
        session.recording.status(),
        Err(CaptureFault::MissingCompletion)
    ));
    assert!(matches!(
        session.recording.replay(),
        Err(ReplayError::Incomplete(_))
    ));
}

#[test]
fn changing_block_return_semantics_invalidates_the_old_source_sidecar() {
    let lexical = program::compile(true).unwrap();
    let normal = program::compile(false).unwrap();
    assert!(matches!(
        lexical.sources.bind(normal.program.program()),
        Err(SourceError::ProgramMismatch)
    ));
}

#[test]
fn requiring_a_block_checks_a_dynamic_payload_before_typed_invocation() {
    use execution_tape::{
        asm::{FunctionSig, ProgramBuilder},
        host::HostSig,
        program::ValueType,
        trace::TraceMask,
    };
    let mut program = ProgramBuilder::new();
    let entry = program
        .declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::Closure],
        })
        .unwrap();
    let require_block = program.host_sig_for(
        "Object.requireBlock",
        HostSig {
            args: vec![ValueType::Agg],
            rets: vec![ValueType::Closure],
        },
    );
    let mut body = program.function_asm(entry).unwrap();
    let integer = body.alloc_register(ValueType::I64);
    let boxed = body.alloc_register(ValueType::Agg);
    body.const_i64(integer, 42).tuple_new(boxed, &[integer]);
    let [block] = program
        .emit_call(&mut body, require_block, &[boxed])
        .unwrap();
    body.ret(0, &[block]);
    program.define_function(entry, body).unwrap();
    let program = program.build_verified().unwrap();
    let failure = Vm::new(Cells::default(), Limits::default())
        .run(&program, entry, &[], TraceMask::NONE, None)
        .unwrap_err();
    assert!(
        matches!(failure.trap, Trap::HostCallFailed { symbol, error: HostError::SignatureMismatch, .. }
        if symbol.as_ref() == "Object.requireBlock")
    );
}

#[test]
fn nonlocal_return_boxing_preserves_the_owned_aggregate_budget_failure() {
    let fixture = program::compile(true).unwrap();
    let mut baseline = Runtime::new(&fixture, Limits::default()).unwrap();
    until_wait(&mut baseline);
    let spent = baseline.run.data_work().aggregate;
    let mut limited = Runtime::new(
        &fixture,
        Limits {
            max_aggregate_work: spent,
            ..Limits::default()
        },
    )
    .unwrap();
    until_wait(&mut limited);
    limited.finish_native_wait().unwrap();
    let failure = loop {
        match limited.poll(128) {
            Ok(Status::Running) => {}
            Err(error) => break error.downcast::<TrapInfo>().unwrap(),
            other => panic!("expected boxed return budget failure, got {other:?}"),
        }
    };
    let check = |failure: &TrapInfo| {
        let Trap::HostCallFailed {
            error: HostError::Custom(source),
            ..
        } = &failure.trap
        else {
            panic!("owned host failure");
        };
        assert!(matches!(
            source.downcast_ref::<Trap>(),
            Some(Trap::AggregateWorkExceeded { .. })
        ));
    };
    check(&failure);
    let session = limited.into_session();
    assert_eq!(
        session.cleanups,
        [2, 1],
        "the denied final return does not repeat cleanup"
    );
    assert!(
        session
            .recording
            .control_commands()
            .all(|command| !matches!(command.action, ControlAction::ReturnFrom { .. }))
    );
    session.recording.status().unwrap();
    let ReplayOutcome::Trapped(replayed) = session.recording.replay().unwrap() else {
        panic!("trapped guest replay");
    };
    check(&replayed);
}
