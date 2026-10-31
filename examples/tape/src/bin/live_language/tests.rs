// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use super::*;
use execution_tape::recording::ReplayOutcome;
use execution_tape::vm::ControlError;

fn until_wait(task: &mut Task, quantum: u64) -> Status {
    for _ in 0..1_000 {
        let status = task.poll(quantum).unwrap();
        if status != Status::Running {
            return status;
        }
    }
    panic!("finite language fixture did not reach a boundary");
}

fn finish(task: &mut Task, quantum: u64) {
    assert_eq!(until_wait(task, quantum), Status::NeedValue);
    task.publish_replacement().unwrap();
    task.recover(42).unwrap();
    assert_eq!(until_wait(task, quantum), Status::CleanupWaiting);
    task.finish_cleanup_wait().unwrap();
    assert_eq!(until_wait(task, quantum), Status::Complete);
}

#[test]
fn lexical_exit_keeps_original_code_and_cleanup_after_publication() {
    let mut task = Task::new(true, 1_000).unwrap();
    finish(&mut task, 1);
    assert_eq!(task.answer, Some(42));
    assert_eq!(task.revision(), 1);
    let work = task.work().unwrap();
    assert_eq!(work.fuel, 1_000 - task.remaining_fuel);
    assert!(work.host_calls > 5);
    assert!(work.contract > 0);
    assert!(work.data.aggregate > 0);
    let actions: Vec<_> = task
        .events
        .iter()
        .map(|event| event.action.clone())
        .collect();
    assert_eq!(
        actions,
        vec![
            Action::RetainedBlock,
            Action::HandlerEntered,
            Action::Published(1),
            Action::Recovery(42),
            Action::ExitSelected,
            Action::CleanupStarted,
            Action::Cleaned(10),
            Action::CleanupFinished,
            Action::HomeReturned
        ]
    );
    assert!(task.stack().is_empty());
    assert!(
        task.home_target(HOME_TOKEN)
            .unwrap_err()
            .downcast_ref::<Failure>()
            .is_some_and(|error| matches!(error, Failure::CannotReturn))
    );
    assert!(
        task.retained_block.is_some(),
        "the block remains owned even after its home ends"
    );
    for evidence in &task.evidence {
        match evidence.termination {
            Termination::Returned => {
                evidence.recording.status().unwrap();
                assert!(matches!(
                    evidence.recording.replay().unwrap(),
                    ReplayOutcome::Returned(_)
                ));
            }
            Termination::Abandoned => {
                evidence.recording.status().unwrap();
                assert!(
                    matches!(evidence.recording.replay().unwrap(), ReplayOutcome::Trapped(error) if matches!(error.trap, Trap::Abandoned))
                );
            }
            Termination::Cancelled => {
                panic!("deliberate language exit must have complete stop evidence")
            }
            Termination::Trapped => panic!("successful language exit must not fabricate traps"),
        }
    }
    assert_eq!(
        task.evidence
            .iter()
            .filter(|record| record.termination == Termination::Abandoned)
            .count(),
        2
    );
}

#[test]
fn normal_return_runs_following_code_and_all_recordings_replay() {
    let mut task = Task::new(false, 1_000).unwrap();
    finish(&mut task, 64);
    assert_eq!(task.answer, Some(42));
    assert!(
        task.events
            .iter()
            .any(|event| event.action == Action::AfterMethod)
    );
    assert!(
        task.events
            .iter()
            .any(|event| event.action == Action::AfterSend)
    );
    assert!(
        !task
            .events
            .iter()
            .any(|event| event.action == Action::ExitSelected)
    );
    for evidence in &task.evidence {
        assert_eq!(evidence.termination, Termination::Returned);
        assert!(matches!(
            evidence.recording.replay().unwrap(),
            ReplayOutcome::Returned(_)
        ));
    }
}

#[test]
fn waiting_preserves_budget_events_and_exact_sources_across_local_id_collisions() {
    let mut task = Task::new(true, 1_000).unwrap();
    assert_eq!(until_wait(&mut task, 8), Status::NeedValue);
    let remaining = task.remaining_fuel;
    let events = task.events.len();
    for _ in 0..20 {
        assert_eq!(task.poll(64).unwrap(), Status::NeedValue);
    }
    assert_eq!(task.remaining_fuel, remaining);
    assert_eq!(task.events.len(), events);
    let stack = task.stack();
    assert_eq!(
        stack.len(),
        4,
        "returned factory is absent; its escaped block is active"
    );
    let sources: Vec<_> = stack
        .iter()
        .map(|(_, site)| crate::source(site).unwrap())
        .collect();
    assert!(sources[0].contains("home.st · Runtime send:"));
    assert!(sources[1].contains("through.st · Runtime protect:"));
    assert!(sources[2].contains("home.st · Runtime signal:"));
    assert!(sources[3].contains("handler.st · Runtime chooseRecovery"));
    assert_eq!(stack[0].1.location.func, stack[1].1.location.func);
    assert_eq!(stack[1].1.location.func, stack[3].1.location.func);
    assert!(!Arc::ptr_eq(
        &stack[0].1.code.program,
        &stack[1].1.code.program
    ));
    assert!(
        task.home_target(BLOCK_TOKEN).is_err(),
        "a different token cannot select the home"
    );
}

#[test]
fn cancellation_at_cleanup_runs_no_further_guest_code() {
    let mut task = Task::new(true, 1_000).unwrap();
    assert_eq!(until_wait(&mut task, 8), Status::NeedValue);
    task.recover(42).unwrap();
    assert_eq!(until_wait(&mut task, 8), Status::CleanupWaiting);
    assert!(
        task.retained_cleanup.is_none(),
        "binding consumed before cleanup entry"
    );
    task.cancel();
    assert_eq!(task.poll(64).unwrap(), Status::Cancelled);
    assert!(task.stack().is_empty());
    assert!(task.retained_block.is_none());
    assert!(task.transfer.is_none());
    assert!(!task.events.iter().any(|event| matches!(
        event.action,
        Action::Cleaned(_) | Action::CleanupFinished | Action::HomeReturned
    )));
    assert!(
        task.evidence
            .iter()
            .any(|evidence| evidence.label == "ensure block / revision 0"
                && evidence.termination == Termination::Cancelled)
    );
}

#[test]
fn task_fuel_is_shared_and_quantum_independent() {
    let mut first = Task::new(true, 1_000).unwrap();
    let mut second = Task::new(true, 1_000).unwrap();
    finish(&mut first, 1);
    finish(&mut second, 128);
    assert_eq!(first.remaining_fuel, second.remaining_fuel);
    let spent = 1_000 - first.remaining_fuel;
    assert!(spent > 20);
    let mut limited = Task::new(true, spent - 1).unwrap();
    assert_eq!(until_wait(&mut limited, 128), Status::NeedValue);
    limited.recover(42).unwrap();
    assert_eq!(until_wait(&mut limited, 128), Status::CleanupWaiting);
    limited.finish_cleanup_wait().unwrap();
    let error = loop {
        match limited.poll(128) {
            Ok(Status::Running) => {}
            Err(error) => break error,
            other => panic!("expected task fuel failure, got {other:?}"),
        }
    };
    assert!(matches!(
        error.downcast_ref::<Failure>(),
        Some(Failure::FuelExhausted)
    ));
    assert_eq!(limited.remaining_fuel, 0);
    assert_eq!(limited.poll(1).unwrap(), Status::Cancelled);
    assert_eq!(limited.answer, None);
}

#[test]
fn a_home_activation_cannot_be_reinterpreted_in_another_invocation() {
    let mut task = Task::new(true, 1_000).unwrap();
    assert_eq!(until_wait(&mut task, 16), Status::NeedValue);
    let target = task.home_target(HOME_TOKEN).unwrap();
    let block = task.block.as_mut().unwrap();
    let before = block.call.remaining_fuel();
    let denied = block
        .call
        .pending_host_call()
        .unwrap()
        .return_from(&target, vec![Value::I64(99)]);
    assert!(matches!(denied, Err(ControlError::ForeignInvocation)));
    assert_eq!(block.call.remaining_fuel(), before);
    assert_eq!(block.request().unwrap().0, "Runtime.signal");
    task.recover(42).unwrap();
    assert_eq!(until_wait(&mut task, 16), Status::CleanupWaiting);
    let remaining = task.remaining_fuel;
    let events = task.events.len();
    for _ in 0..20 {
        assert_eq!(task.poll(16).unwrap(), Status::CleanupWaiting);
    }
    assert_eq!(task.remaining_fuel, remaining);
    assert_eq!(task.events.len(), events);
    task.finish_cleanup_wait().unwrap();
    assert_eq!(until_wait(&mut task, 16), Status::Complete);
    assert_eq!(task.answer, Some(42));
}

#[test]
fn retained_block_still_executes_after_its_home_has_expired() {
    let mut task = Task::new(true, 1_000).unwrap();
    finish(&mut task, 16);
    let home_code = Arc::clone(
        &task
            .evidence
            .iter()
            .find(|record| record.label == "lexical home")
            .unwrap()
            .code,
    );
    let mut block = Frame::start(
        "late block",
        home_code,
        task.retained_block.as_ref().unwrap(),
        vec![],
        1_000,
    )
    .unwrap();
    let mut fuel = 1_000;
    assert_eq!(
        block.advance(64, &mut fuel).unwrap(),
        RunStatus::AwaitingHost
    );
    assert_eq!(block.request().unwrap().0, "Runtime.signal");
    block.reply(vec![Value::I64(99)]).unwrap();
    assert_eq!(
        block.advance(64, &mut fuel).unwrap(),
        RunStatus::AwaitingHost
    );
    let (symbol, values) = block.request().unwrap();
    assert_eq!(symbol, "Runtime.return");
    let [Value::U64(home), Value::I64(99)] = values.as_slice() else {
        panic!("retained captures and fresh answer")
    };
    assert!(matches!(
        task.home_target(*home)
            .unwrap_err()
            .downcast_ref::<Failure>(),
        Some(Failure::CannotReturn)
    ));
    let evidence = block.retire();
    assert_eq!(evidence.termination, Termination::Cancelled);
    assert!(evidence.recording.replay().is_err());
}

#[test]
fn future_lookup_executes_replacement_cleanup_with_its_own_source() {
    let mut task = Task::new(true, 1_000).unwrap();
    finish(&mut task, 16);
    let current = task.methods.current();
    assert!(Arc::ptr_eq(
        current.callable().program(),
        &task.replacement.program
    ));
    let mut method = Frame::start(
        "future method",
        Arc::clone(&task.replacement),
        current.callable(),
        vec![Value::U64(BLOCK_TOKEN)],
        1_000,
    )
    .unwrap();
    let mut fuel = 1_000;
    assert_eq!(
        method.advance(64, &mut fuel).unwrap(),
        RunStatus::AwaitingHost
    );
    let (_, values) = method.request().unwrap();
    let [Value::U64(BLOCK_TOKEN), cleanup @ Value::Closure(_)] = values.as_slice() else {
        panic!("replacement protection")
    };
    let callable = method.retain(cleanup).unwrap();
    let mut cleanup = Frame::start(
        "future cleanup",
        Arc::clone(&task.replacement),
        &callable,
        vec![],
        fuel,
    )
    .unwrap();
    assert_eq!(
        cleanup.advance(64, &mut fuel).unwrap(),
        RunStatus::AwaitingHost
    );
    cleanup.reply(vec![]).unwrap();
    assert_eq!(
        cleanup.advance(64, &mut fuel).unwrap(),
        RunStatus::AwaitingHost
    );
    let (symbol, values) = cleanup.request().unwrap();
    assert_eq!(symbol, "Runtime.cleaned");
    assert_eq!(values, vec![Value::I64(100)]);
    let site = cleanup.site().unwrap();
    assert_eq!(
        crate::source(&site).unwrap(),
        "through.st · Runtime cleaned: 100"
    );
    assert!(
        task.method_code
            .sources
            .bind(cleanup.call.program().program())
            .is_err(),
        "same labels and local IDs cannot attach revision 0 source to revision 1"
    );
    cleanup.reply(vec![]).unwrap();
    assert_eq!(cleanup.advance(64, &mut fuel).unwrap(), RunStatus::Complete);
    let evidence = cleanup.retire();
    assert!(matches!(
        evidence.recording.replay().unwrap(),
        ReplayOutcome::Returned(_)
    ));
}

#[test]
fn every_instruction_budget_cut_retires_the_whole_task() {
    let mut reference = Task::new(true, 1_000).unwrap();
    finish(&mut reference, 1);
    let required = 1_000 - reference.remaining_fuel;
    for quantum in [1, 64] {
        for fuel in 0..=required {
            let mut task = Task::new(true, fuel).unwrap();
            let mut terminal = false;
            for _ in 0..100 {
                match task.poll(quantum) {
                    Ok(Status::Running) => {}
                    Ok(Status::NeedValue) => {
                        task.publish_replacement().unwrap();
                        task.recover(42).unwrap();
                    }
                    Ok(Status::CleanupWaiting) => task.finish_cleanup_wait().unwrap(),
                    Ok(Status::Complete) => {
                        assert_eq!(fuel, required, "fuel={fuel}, quantum={quantum}");
                        assert_eq!(task.answer, Some(42));
                        terminal = true;
                        break;
                    }
                    Err(error) => {
                        assert!(fuel < required);
                        assert!(matches!(
                            error.downcast_ref::<Failure>(),
                            Some(Failure::FuelExhausted)
                        ));
                        assert_eq!(task.poll(1).unwrap(), Status::Cancelled);
                        assert!(task.stack().is_empty());
                        assert!(task.retained_cleanup.is_none());
                        assert!(task.retained_block.is_none());
                        assert!(task.transfer.is_none());
                        assert_eq!(task.answer, None);
                        terminal = true;
                        break;
                    }
                    other => {
                        panic!("unexpected state for fuel={fuel}, quantum={quantum}: {other:?}")
                    }
                }
            }
            assert!(terminal, "finite story, fuel={fuel}, quantum={quantum}");
            assert_eq!(task.remaining_fuel, 0);
        }
    }
}
