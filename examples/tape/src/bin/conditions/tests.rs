// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use super::*;
use crate::program::{self, Expr, Term};
use execution_tape::{
    recording::{CaptureFault, ControlAction, InstructionStatus, ReplayError, ReplayOutcome},
    vm::Trap,
};

fn runtime(name: &str, limits: Limits) -> Runtime {
    let fixture = program::scenario(name).unwrap();
    Runtime::new(fixture.program, fixture.entry, limits).unwrap()
}
fn drive(runtime: &mut Runtime, quantum: u64) {
    for _ in 0..100_000 {
        match runtime.poll(quantum).unwrap() {
            Status::Running => {}
            Status::Complete => return,
            Status::Waiting(_) => runtime.complete_native(vec![]).unwrap(),
            Status::Unhandled(condition) => {
                if let Some(restart) = runtime.available(condition).first() {
                    runtime.choose_restart(restart.token, 42).unwrap();
                } else {
                    runtime.recover(0, false).unwrap();
                }
            }
            Status::RestartReturned(values) => {
                let [Value::I64(value), Value::Bool(flag)] = values.as_slice() else {
                    panic!("typed debugger reply");
                };
                runtime.recover(*value, *flag).unwrap();
            }
        }
    }
    panic!("finite condition example did not finish");
}
fn assert_replay(runtime: Runtime, expected: &[Value]) -> Recording {
    assert_eq!(runtime.outputs().unwrap(), expected, "live typed results");
    assert!(
        runtime.active.is_empty(),
        "logical scopes retire before successful completion"
    );
    assert!(
        runtime.masks.is_empty(),
        "handler masks do not escape their dynamic call"
    );
    assert!(
        runtime.waiting.is_empty(),
        "no abandoned language continuations remain"
    );
    assert!(
        runtime
            .scopes
            .iter()
            .all(|scope| matches!(scope.binding, Binding::Retired { .. })),
        "retired extents retain token tombstones, never borrowed guest captures"
    );
    let record = runtime.into_session().recording;
    record.status().unwrap();
    assert_eq!(
        record.instruction_status(),
        Some(InstructionStatus::Complete),
        "actual overlapping instruction evidence is complete"
    );
    let ReplayOutcome::Returned(snapshot) = record.replay().unwrap() else {
        panic!("guest replay returned");
    };
    assert_eq!(
        snapshot.values(),
        expected,
        "replay reexecutes actual guest handlers, restarts and cleanup"
    );
    record
}
fn native(runtime: &Runtime) -> Vec<i64> {
    runtime
        .events
        .iter()
        .filter_map(|event| match event.action {
            Action::Native(value) => Some(value),
            _ => None,
        })
        .collect()
}
fn until_wait(runtime: &mut Runtime) {
    for _ in 0..10000 {
        match runtime.poll(8).unwrap() {
            Status::Running => {}
            Status::Waiting(_) => return,
            other => panic!("expected cleanup/handler wait, got {other:?}"),
        }
    }
    panic!("wait not reached");
}
fn until_unhandled(runtime: &mut Runtime) -> Condition {
    for _ in 0..10000 {
        match runtime.poll(8).unwrap() {
            Status::Running => {}
            Status::Unhandled(condition) => return condition,
            other => panic!("expected unhandled condition, got {other:?}"),
        }
    }
    panic!("unhandled condition not reached");
}

#[test]
fn language_protocol_composes_all_examples_with_single_step_and_batched_scheduling() {
    let expected = [
        ("recovery", 42, true),
        ("decline", 15, false),
        ("cl-decline", 15, false),
        ("nested", 107, true),
        ("nested-bind", 123, true),
        ("functions", 42, true),
        ("restart", 42, true),
        ("cleanup", 42, true),
        ("normal-cleanup", 7, true),
        ("cleanup-replaces", 99, true),
        ("cleanup-local-exit", 42, true),
        ("cleanup-signals", 42, true),
        ("unhandled", 42, true),
        ("normal-debugger", 43, true),
        ("expired", 77, true),
        ("rebind", 77, true),
        ("missing", 88, true),
        ("applicability", 0, false),
    ];
    for quantum in [1, 128] {
        for (name, value, flag) in expected {
            let mut runtime = runtime(name, Limits::default());
            drive(&mut runtime, quantum);
            assert_eq!(
                runtime.outputs().unwrap(),
                [Value::I64(value), Value::Bool(flag)],
                "case {name}, quantum {quantum}"
            );
            assert_replay(runtime, &[Value::I64(value), Value::Bool(flag)]);
        }
    }
}

#[test]
fn handler_normal_return_policy_and_explicit_decline_preserve_multiple_values() {
    for case in ["cl-decline", "decline"] {
        let mut runtime = runtime(case, Limits::default());
        drive(&mut runtime, 100);
        assert_eq!(native(&runtime), [11, 12]);
        assert_eq!(
            runtime
                .events
                .iter()
                .filter(|event| matches!(event.action, Action::HandlerDeclined(_)))
                .count(),
            1
        );
        assert_replay(runtime, &[Value::I64(15), Value::Bool(false)]);
    }
}

#[test]
fn nested_signals_mask_existing_inner_handlers_but_allow_new_bindings() {
    let mut nested = runtime("nested", Limits::default());
    drive(&mut nested, 100);
    assert_eq!(native(&nested), [11, 12]);
    let handlers: Vec<_> = nested
        .events
        .iter()
        .filter_map(|event| match event.action {
            Action::HandlerCalled(scope, class) => Some((scope, class)),
            _ => None,
        })
        .collect();
    assert_eq!(handlers, [(1, Class::DivisionByZero), (0, Class::Warning)]);
    assert_replay(nested, &[Value::I64(107), Value::Bool(true)]);
    let mut rebound = runtime("nested-bind", Limits::default());
    drive(&mut rebound, 100);
    assert_eq!(native(&rebound), [13]);
    assert_replay(rebound, &[Value::I64(123), Value::Bool(true)]);
}

#[test]
fn restart_functions_are_repeatable_and_do_not_imply_an_exit() {
    let mut runtime = runtime("functions", Limits::default());
    drive(&mut runtime, 100);
    assert_eq!(native(&runtime), [30, 30]);
    assert_eq!(
        runtime
            .events
            .iter()
            .filter(|event| matches!(event.action, Action::RestartCalled(3)))
            .count(),
        2
    );
    let record = assert_replay(runtime, &[Value::I64(42), Value::Bool(true)]);
    assert!(
        !record
            .control_commands()
            .any(|command| matches!(command.action, ControlAction::ReturnFrom { .. }))
    );
}

#[test]
fn normally_returning_restart_does_not_turn_a_common_lisp_handler_into_recovery() {
    let expression = Expr::Signal(Class::DivisionByZero, Term::Constant(5))
        .handle(
            Class::Condition,
            Policy::CommonLisp,
            Expr::Invoke(3, Term::Constant(1)),
        )
        .restart(
            3,
            RestartMode::Function,
            Expr::Value(Term::Argument.plus(Term::Capture), true),
        )
        .handle(Class::Condition, Policy::Dylan, Expr::value(55));
    let fixture = program::compile(expression, 40).unwrap();
    let mut runtime = Runtime::new(fixture.program, fixture.entry, Limits::default()).unwrap();
    drive(&mut runtime, 100);
    assert!(
        runtime
            .events
            .iter()
            .any(|event| matches!(event.action, Action::HandlerDeclined(_)))
    );
    assert_replay(runtime, &[Value::I64(55), Value::Bool(true)]);
}

#[test]
fn protected_cleanup_is_lifo_exactly_once_on_normal_and_nonlocal_completion() {
    for (case, value) in [("normal-cleanup", 7), ("cleanup", 42)] {
        let mut runtime = runtime(case, Limits::default());
        drive(&mut runtime, 100);
        assert_eq!(native(&runtime), [20, 10], "case {case}");
        assert_eq!(
            runtime
                .events
                .iter()
                .filter(|event| matches!(event.action, Action::CleanupStarted(_)))
                .count(),
            2
        );
        assert_eq!(
            runtime
                .events
                .iter()
                .filter(|event| matches!(event.action, Action::CleanupFinished(_)))
                .count(),
            2
        );
        assert_replay(runtime, &[Value::I64(value), Value::Bool(true)]);
    }
}

#[test]
fn cleanup_wait_preserves_signaling_stack_but_restores_logical_extent() {
    let fixture = program::scenario("cleanup").unwrap();
    let mut runtime =
        Runtime::new(fixture.program.clone(), fixture.entry, Limits::default()).unwrap();
    until_wait(&mut runtime);
    assert_eq!(native(&runtime), [20]);
    assert_eq!(
        runtime.active.len(),
        3,
        "crossed inner protection has already retired logically"
    );
    let inspection = runtime.run.inspect();
    let frames: Vec<_> = inspection.frames().collect();
    assert!(
        frames.len() > runtime.active.len(),
        "physical signaling frames remain retained through cooperative cleanup"
    );
    let leaf = &frames[0];
    assert!(
        fixture
            .program
            .program()
            .function_name(leaf.location.func.0)
            .unwrap()
            .starts_with("cleanup")
    );
    let bound = fixture
        .sources
        .bind(runtime.run.program().program())
        .unwrap();
    assert_eq!(
        bound.lookup(leaf.location).unwrap().text(),
        "await host work"
    );
    assert!(frames.iter().any(|frame| {
        bound
            .lookup(frame.location)
            .is_some_and(|source| source.text().starts_with("signal DivisionByZero"))
    }));
    drop(inspection);
    let fuel = runtime.run.remaining_fuel();
    let work = runtime.run.data_work();
    for _ in 0..5 {
        assert!(matches!(runtime.poll(0).unwrap(), Status::Waiting(_)));
    }
    assert_eq!(runtime.run.remaining_fuel(), fuel);
    assert_eq!(runtime.run.data_work(), work);
    runtime.complete_native(vec![]).unwrap();
    drive(&mut runtime, 100);
    assert_replay(runtime, &[Value::I64(42), Value::Bool(true)]);
}

#[test]
fn cleanup_can_replace_an_exit_without_rerunning_itself() {
    let mut runtime = runtime("cleanup-replaces", Limits::default());
    drive(&mut runtime, 100);
    assert_eq!(native(&runtime), [20]);
    assert_eq!(
        runtime
            .events
            .iter()
            .filter(|event| matches!(event.action, Action::CleanupStarted(_)))
            .count(),
        1
    );
    assert_eq!(
        runtime
            .events
            .iter()
            .filter(|event| matches!(event.action, Action::CleanupFinished(_)))
            .count(),
        0
    );
    assert_eq!(
        runtime
            .events
            .iter()
            .filter(|event| matches!(event.action, Action::CleanupAbandoned(_)))
            .count(),
        1
    );
    assert_replay(runtime, &[Value::I64(99), Value::Bool(true)]);
}

#[test]
fn local_exit_inside_cleanup_preserves_the_original_pending_transfer() {
    let mut runtime = runtime("cleanup-local-exit", Limits::default());
    drive(&mut runtime, 100);
    assert_eq!(native(&runtime), [20, 21]);
    assert!(
        !runtime
            .events
            .iter()
            .any(|event| matches!(event.action, Action::CleanupAbandoned(_)))
    );
    let record = assert_replay(runtime, &[Value::I64(42), Value::Bool(true)]);
    assert_eq!(
        record
            .control_commands()
            .filter(|command| matches!(command.action, ControlAction::ReturnFrom { .. }))
            .count(),
        2
    );
}

#[test]
fn cleanup_restores_outer_handler_masks_before_signaling_again() {
    let mut runtime = runtime("cleanup-signals", Limits::default());
    drive(&mut runtime, 100);
    assert_eq!(native(&runtime), [11, 20, 30]);
    let handlers: Vec<_> = runtime
        .events
        .iter()
        .filter_map(|event| match event.action {
            Action::HandlerCalled(scope, class) => Some((scope, class)),
            _ => None,
        })
        .collect();
    assert_eq!(
        handlers,
        [(1, Class::DivisionByZero), (1, Class::Warning)],
        "same logical binding can handle cleanup's nested signal after its abandoned invocation is masked out"
    );
    assert_replay(runtime, &[Value::I64(42), Value::Bool(true)]);
}

#[test]
fn restart_tokens_expire_and_name_rebinding_cannot_revive_old_tokens() {
    for case in ["expired", "rebind"] {
        let mut runtime = runtime(case, Limits::default());
        drive(&mut runtime, 100);
        let token = runtime.saved_restart.unwrap();
        assert!(matches!(runtime.restart(token), Err(Error::ExpiredRestart)));
        assert!(runtime.events.iter().any(|event| matches!(
            event.action,
            Action::HandlerCalled(_, Class::InvalidRestart)
        )));
        assert_replay(runtime, &[Value::I64(77), Value::Bool(true)]);
    }
}

#[test]
fn foreign_debugger_restart_tokens_are_recoverable_denials_before_guest_work() {
    let mut first = runtime("unhandled", Limits::default());
    let mut second = runtime("unhandled", Limits::default());
    let condition = until_unhandled(&mut first);
    let other_condition = until_unhandled(&mut second);
    let foreign = first.available(condition)[0].token;
    assert_ne!(foreign, second.available(other_condition)[0].token);
    let fuel = second.run.remaining_fuel();
    let commands = second.waiting.len();
    assert!(matches!(
        second.choose_restart(foreign, 7),
        Err(Error::ForeignRestart)
    ));
    assert!(matches!(second.poll(0).unwrap(), Status::Unhandled(_)));
    assert_eq!(second.run.remaining_fuel(), fuel);
    assert_eq!(second.waiting.len(), commands);
    drive(&mut second, 100);
    assert_replay(second, &[Value::I64(42), Value::Bool(true)]);
}

#[test]
fn debugger_can_call_a_restart_function_repeatedly_before_resolving_the_signal() {
    let mut runtime = runtime("normal-debugger", Limits::default());
    let condition = until_unhandled(&mut runtime);
    let token = runtime.available(condition)[0].token;
    for argument in [1, 2] {
        runtime.choose_restart(token, argument).unwrap();
        loop {
            match runtime.poll(100).unwrap() {
                Status::Running => {}
                Status::RestartReturned(values) => {
                    assert_eq!(values, [Value::I64(argument + 1), Value::Bool(true)]);
                    break;
                }
                other => panic!("expected a normal debugger reply, got {other:?}"),
            }
        }
        assert!(matches!(runtime.poll(0).unwrap(), Status::Unhandled(_)));
        assert!(
            runtime.run.result().is_none(),
            "normal restart return is not signal recovery"
        );
    }
    runtime.recover(9, false).unwrap();
    drive(&mut runtime, 100);
    assert_replay(runtime, &[Value::I64(9), Value::Bool(false)]);
}

#[test]
fn cancellation_during_cleanup_does_not_finish_cleanup_or_run_outer_cleanup_on_drop() {
    let mut runtime = runtime("cleanup", Limits::default());
    until_wait(&mut runtime);
    assert_eq!(native(&runtime), [20]);
    assert!(
        !runtime
            .events
            .iter()
            .any(|event| matches!(event.action, Action::CleanupFinished(_)))
    );
    let record = runtime.into_session().recording;
    assert!(matches!(
        record.status(),
        Err(CaptureFault::MissingCompletion)
    ));
    assert!(matches!(record.replay(), Err(ReplayError::Incomplete(_))));
    assert!(
        !record
            .control_commands()
            .any(|command| matches!(command.action, ControlAction::ReturnFrom { .. })),
        "final physical transfer has not been issued while cleanup waits"
    );
}

#[test]
fn cleanup_execution_and_prior_work_share_the_original_hard_fuel_budget() {
    let mut baseline = runtime("cleanup", Limits::default());
    until_wait(&mut baseline);
    let spent = Limits::default().fuel - baseline.run.remaining_fuel();
    let mut limited = runtime(
        "cleanup",
        Limits {
            fuel: spent,
            ..Limits::default()
        },
    );
    until_wait(&mut limited);
    assert_eq!(limited.run.remaining_fuel(), 0);
    limited.complete_native(vec![]).unwrap();
    assert!(
        matches!(limited.poll(100), Err(Error::Vm(trap)) if matches!(trap.trap, Trap::FuelExceeded))
    );
    assert_eq!(
        native(&limited),
        [20],
        "outer cleanup never runs after hard exhaustion"
    );
    let record = limited.into_session().recording;
    record.status().unwrap();
    assert!(
        matches!(record.replay().unwrap(), ReplayOutcome::Trapped(trap) if matches!(trap.trap, Trap::FuelExceeded))
    );
}

#[test]
fn native_completion_cannot_bypass_an_unhandled_language_condition() {
    let mut runtime = runtime("unhandled", Limits::default());
    let condition = until_unhandled(&mut runtime);
    let fuel = runtime.run.remaining_fuel();
    let work = runtime.run.data_work();
    let token = runtime.available(condition)[0].token;
    assert!(matches!(
        runtime.complete_native(vec![Value::I64(7), Value::Bool(true)]),
        Err(Error::Abi(_))
    ));
    assert!(matches!(runtime.poll(0).unwrap(), Status::Unhandled(current) if current == condition));
    assert_eq!(runtime.run.remaining_fuel(), fuel);
    assert_eq!(runtime.run.data_work(), work);
    assert_eq!(runtime.available(condition)[0].token, token);
    drive(&mut runtime, 100);
    assert_replay(runtime, &[Value::I64(42), Value::Bool(true)]);
}

#[test]
fn malformed_native_reply_traps_and_retires_borrowed_language_state() {
    let mut runtime = runtime("cleanup", Limits::default());
    until_wait(&mut runtime);
    assert!(matches!(runtime.complete_native(vec![Value::I64(1)]),
        Err(Error::Vm(trap))
        if matches!(trap.trap, Trap::HostReturnArityMismatch { .. })));
    assert!(runtime.active.is_empty());
    assert!(runtime.waiting.is_empty());
    assert!(runtime.masks.is_empty());
    assert!(
        runtime
            .scopes
            .iter()
            .all(|scope| matches!(scope.binding, Binding::Retired { .. }))
    );
    assert!(runtime.native_wait.is_none());
    assert!(runtime.complete_native(vec![]).is_err());
    assert!(runtime.poll(100).is_err());
    assert_eq!(
        native(&runtime),
        [20],
        "hard failure does not execute outer cleanup"
    );
    let session = runtime.into_session();
    assert_eq!(session.policy_end, PolicyEnd::Trapped);
    session.recording.status().unwrap();
    assert!(
        matches!(session.recording.replay().unwrap(), ReplayOutcome::Trapped(trap)
        if matches!(trap.trap, Trap::HostReturnArityMismatch { .. }))
    );
}

#[test]
fn owned_policy_evidence_distinguishes_replaced_intents_from_actual_vm_transfers() {
    let mut runtime = runtime("cleanup-replaces", Limits::default());
    drive(&mut runtime, 100);
    let session = runtime.into_session();
    assert_eq!(session.policy_end, PolicyEnd::Returned);
    assert!(!session.policy_truncated);
    let intents: Vec<_> = session
        .policy
        .iter()
        .filter_map(|event| match event.action {
            Action::ExitSelected { reply, .. } => Some(reply.value),
            _ => None,
        })
        .collect();
    assert_eq!(intents, [42, 99]);
    assert_eq!(
        session
            .recording
            .control_commands()
            .filter(|command| matches!(command.action, ControlAction::ReturnFrom { .. }))
            .count(),
        1
    );
    assert!(
        session
            .policy
            .iter()
            .any(|event| matches!(event.action, Action::CleanupAbandoned(_)))
    );
    assert!(
        matches!(session.recording.replay().unwrap(), ReplayOutcome::Returned(values)
        if values.values() == [Value::I64(99), Value::Bool(true)])
    );
}

#[test]
fn authored_source_covers_policy_and_actual_vm_boundaries_without_invented_locations() {
    for name in program::CASES {
        let fixture = program::scenario(name).unwrap();
        let mut runtime =
            Runtime::new(fixture.program.clone(), fixture.entry, Limits::default()).unwrap();
        drive(&mut runtime, 100);
        let session = runtime.into_session();
        let sources = fixture
            .sources
            .bind(session.recording.program().program())
            .unwrap();
        let check = |location| {
            let range = sources
                .lookup(location)
                .expect("frontend mapped each actual instruction");
            assert!(!range.text().is_empty(), "case {name}");
        };
        for event in session.policy {
            check(event.location);
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
}

#[test]
fn restart_lookup_checks_applicability_before_invocation() {
    let mut runtime = runtime("applicability", Limits::default());
    let condition = until_unhandled(&mut runtime);
    assert_eq!(condition.class, Class::InvalidRestart);
    assert!(runtime.available(condition).is_empty());
    assert!(runtime.events.iter().any(|event| matches!(
        event.action,
        Action::InvalidRestart {
            reason: RestartDenial::Absent,
            ..
        }
    )));
    runtime.recover(0, false).unwrap();
    drive(&mut runtime, 100);
    assert_replay(runtime, &[Value::I64(0), Value::Bool(false)]);
}
