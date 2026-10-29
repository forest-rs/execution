// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use super::*;
use execution_tape::{source::SourceError, vm::Trap};
use std::error::Error;

#[test]
fn guest_sends_select_old_then_new_method_and_charge_callback_work() {
    let report = run(&Options::default()).unwrap();
    assert_eq!(report.outcome.as_ref().unwrap(), &[1, 11]);
    assert_eq!(report.published_revision, 1);
    assert_eq!(report.host.0.borrow().value, 11);
    assert_eq!(report.host.0.borrow().native_calls, 2);
    assert_eq!(report.remaining_fuel, 89);
    assert_eq!(
        report.links.iter().map(|r| r.revision).collect::<Vec<_>>(),
        [Some(0), Some(1)]
    );
    assert!(report.links.iter().all(|r| r.method_fuel == Some(3)));
    assert_eq!(report.ledger.0.borrow().len(), 7);
    assert_eq!(verify(&report).unwrap(), 3);
    let bound = report
        .caller
        .sources
        .bind(report.caller.program.program())
        .unwrap();
    assert_eq!(
        bound.lookup(report.links[0].location).unwrap().start().line,
        2
    );
    assert_eq!(
        bound.lookup(report.links[1].location).unwrap().start().line,
        3
    );
    assert_ne!(report.links[0].location, report.links[1].location);
    assert!(matches!(
        report.old.sources.bind(report.new.program.program()),
        Err(SourceError::ProgramMismatch)
    ));
}

#[test]
fn method_and_caller_snapshots_explain_the_actual_second_guest_send() {
    let report = run(&Options {
        view: View::Snapshot,
        request: 1,
        ..Options::default()
    })
    .unwrap();
    let method = &report
        .snapshots
        .iter()
        .find(|(name, _)| name == "method-1-start.txt")
        .unwrap()
        .1;
    assert!(method.contains("Counter.increment · +10"));
    assert!(method.contains("external caller"));
    assert!(method.contains("main → host Object.send"));
    assert!(method.contains("CounterCell.add(receiver, 10)"));
    let caller = &report
        .snapshots
        .iter()
        .find(|(name, _)| name == "caller-1-before-reply.txt")
        .unwrap()
        .1;
    assert!(caller.contains("CALLER INSTRUCTIONS"));
    assert!(caller.contains("messages.expr"));
    assert!(caller.contains("r3 · i64 = 1"));
    assert!(caller.contains("r4 · i64 = unavailable"));
    assert_eq!(verify(&report).unwrap(), 3);
}

#[test]
fn missing_method_reports_owned_domain_error_at_the_real_guest_send() {
    let report = run(&Options {
        selector: "missing".into(),
        ..Options::default()
    })
    .unwrap();
    let error = report.outcome.as_ref().unwrap_err();
    let Trap::HostCallFailed { error, .. } = &error.trap else {
        panic!("host failure")
    };
    let failure = error
        .source()
        .unwrap()
        .downcast_ref::<MethodFailure>()
        .unwrap();
    assert!(
        matches!(failure.cause.downcast_ref::<DispatchError>(), Some(DispatchError::NoSuchMethod { selector, .. }) if selector == "missing")
    );
    assert_eq!(report.host.0.borrow().native_calls, 0);
    assert!(report.methods.is_empty());
    assert_eq!(report.links.len(), 1);
    let source = report
        .caller
        .sources
        .bind(report.caller.program.program())
        .unwrap()
        .lookup(report.links[0].location)
        .unwrap();
    assert_eq!(source.text(), "counter.missing()");
    assert_eq!(verify(&report).unwrap(), 1);
}

#[test]
fn callback_fuel_is_bounded_before_return_and_performed_cell_effects_survive_failure() {
    let report = run(&Options {
        fuel: 4,
        ..Options::default()
    })
    .unwrap();
    assert!(report.outcome.is_err());
    assert_eq!(report.links.len(), 1);
    assert_eq!(report.host.0.borrow().native_calls, 1);
    assert_eq!(report.host.0.borrow().value, 1);
    assert_eq!(report.published_revision, 1);
    assert!(
        matches!(report.methods[0].recording.replay().unwrap(), ReplayOutcome::Trapped(error) if matches!(error.trap, Trap::FuelExceeded))
    );
    assert_eq!(verify(&report).unwrap(), 2);
}
