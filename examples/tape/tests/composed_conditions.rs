// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use std::process::Command;

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_composed_conditions"))
        .args(args)
        .output()
        .expect("run the compiled example")
}

#[test]
fn returning_restart_keeps_old_code_live_and_uses_replacement_on_the_next_lookup() {
    let output = run(&["resume", "--explain"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.contains("Restart returned [I64(2)]"),
        "the restart body actually executed"
    );
    assert!(
        text.contains("Method returned [I64(42)]") && text.contains("Method returned [I64(99)]"),
        "old and replacement definitions retain their separate behavior"
    );
    assert!(
        text.contains("Result computed by guest code: [I64(141)]"),
        "main combines actual guest results"
    );
    assert!(
        text.contains("zero_divisor_handler · f0:")
            && text.contains("divide/v1 · f0:")
            && text.contains("main · f0:"),
        "the real stack distinguishes independently compiled functions with equal local IDs"
    );
    assert!(
        text.contains("signal ZeroDivisor(lhs, rhs)"),
        "the explanation includes the actual authored signalling source"
    );
}

#[test]
fn direct_recovery_returns_without_invoking_the_restart() {
    let output = run(&["recover"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.contains("Result computed by guest code: [I64(141)]"),
        "the handler's direct resumption reaches the same computation"
    );
    assert!(
        !text.contains("Invoking the nearest live"),
        "recovery does not execute a restart body"
    );
}

#[test]
fn nonlocal_exit_waits_for_guest_cleanup_and_skips_the_second_send() {
    let output = run(&["escape", "--explain"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.contains("ensure_cleanup · f0:") && text.contains("native_cleanup()"),
        "cleanup is an actual source-linked guest frame"
    );
    let cleanup = text
        .find("Native cleanup effect runs")
        .expect("cleanup ran");
    let exit = text
        .find("returning 42 from the lexical main activation")
        .expect("typed transfer occurred");
    assert!(
        cleanup < exit,
        "language cleanup finishes before the cross-code non-local return"
    );
    assert!(
        text.contains("Result computed by guest code: [I64(42)]")
            && text.contains("Cleanup effects: 1"),
        "root transfer preserves the performed host effect"
    );
    assert!(
        !text.contains("Lookup chooses divide/v2"),
        "a non-local return never executes the abandoned second send"
    );
}

#[test]
fn hard_fuel_exhaustion_does_not_run_language_cleanup() {
    let output = run(&["escape", "--fuel", "5"]);
    assert!(
        !output.status.success(),
        "hard exhaustion must terminate the whole execution"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("FuelExceeded"),
        "the actual VM denial is retained"
    );
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("Native cleanup effect runs"),
        "hard cancellation does not execute protected guest cleanup implicitly"
    );
}
