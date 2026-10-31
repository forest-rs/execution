// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Escaped blocks, independently compiled methods and cooperative language control.

#[path = "live_language/program.rs"]
mod program;
#[path = "live_language/runtime.rs"]
mod runtime;

use execution_tape::recording::ReplayOutcome;
use execution_tape::vm::Trap;
use runtime::{Action, Site, Status, Task, Termination};

#[derive(Clone, Copy)]
enum View {
    Headless,
    Snapshot,
    Interactive,
}

fn driver_choice(task: &Task, view: View, choices: &[&str]) -> program::Result<bool> {
    match view {
        View::Headless => Ok(true),
        View::Snapshot => {
            print!("{}", task.explorer()?.render_choices(choices, 160, 48)?);
            Ok(true)
        }
        View::Interactive => Ok(task.explorer()?.choose(choices)? == Some(0)),
    }
}

fn source(site: &Site) -> program::Result<String> {
    let sources = site.code.sources.bind(site.code.program.program())?;
    let range = sources
        .lookup(site.location)
        .ok_or("missing authored source at a language boundary")?;
    Ok(format!("{} · {}", range.document().name, range.text()))
}

fn print_stack(task: &Task) -> program::Result<()> {
    println!("Language task → independent VM frames (outermost first):");
    for (label, site) in task.stack() {
        println!("  {label}: {}", source(&site)?);
    }
    Ok(())
}

fn describe(action: &Action) -> String {
    match action {
        Action::RetainedBlock => {
            "retain the escaped block with its original code and captures".into()
        }
        Action::HandlerEntered => "NeedValue selects the language task's handler".into(),
        Action::Published(revision) => {
            format!("publish method revision {revision}; active code and cleanup keep revision 0")
        }
        Action::Recovery(answer) => format!("the driver supplies recovery value {answer}"),
        Action::ExitSelected => {
            "select the block's exact live lexical home; cleanup must run first".into()
        }
        Action::CleanupStarted => {
            "consume the protection binding and enter its original cleanup".into()
        }
        Action::Cleaned(marker) => format!("the original cleanup records {marker}"),
        Action::CleanupFinished => "cleanup returned successfully".into(),
        Action::HomeReturned => {
            "retire crossed invocations; issue the checked return inside the home VM".into()
        }
        Action::AfterMethod => "the protected method continues after its body".into(),
        Action::AfterSend => "the sender continues after the message".into(),
        Action::Stopped => {
            "the driver chooses an explicit task stop; no further guest cleanup runs".into()
        }
    }
}

const HELP: &str = r#"live_language [--normal-return] [--explain] [--snapshot | --explore]
              [--recover N] [--fuel N] [--cancel-at handler|cleanup]

An escaped block signals through an independently compiled protected method.
Publish a compatible replacement while its handler waits, recover, then run
original cleanup before returning to the exact lexical home.

--normal-return lets both callers continue after cleanup.
--explain prints source-linked language decisions and actual paused frames.
--snapshot renders both task views and their offered choices without a terminal.
--explore lets you inspect, then choose recovery/completion or cancellation.
          Number keys return a choice to the driver; q cancels the task.
--recover N selects the offered recovery value (42 by default).
--cancel-at supplies a headless cancellation choice at the named native wait.

The language task owns dispatch, handler selection and cleanup policy.
Its shared fuel does not establish a shared memory/contract quota.
Complete runs check both independent guest replay and composed fixture replay.
Explicit driver stops are checked; dropped or failed tasks remain inspection-only.
"#;

fn main() -> program::Result<()> {
    let mut non_local = true;
    let mut explain = false;
    let mut snapshot = false;
    let mut explore = false;
    let mut fuel = 1_000;
    let mut recovery = 42_i64;
    let mut cancel_at = None;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--normal-return" => non_local = false,
            "--explain" => explain = true,
            "--snapshot" => snapshot = true,
            "--explore" => explore = true,
            "--fuel" => fuel = arguments.next().ok_or("--fuel needs an integer")?.parse()?,
            "--recover" => {
                recovery = arguments
                    .next()
                    .ok_or("--recover needs an integer")?
                    .parse()?
            }
            "--cancel-at" => {
                cancel_at = Some(match arguments.next().as_deref() {
                    Some("handler") => Status::NeedValue,
                    Some("cleanup") => Status::CleanupWaiting,
                    _ => return Err("--cancel-at needs handler or cleanup".into()),
                });
            }
            "--help" | "-h" => {
                print!("{HELP}");
                return Ok(());
            }
            _ => return Err(format!("unknown argument {argument:?}; use --help").into()),
        }
    }
    if snapshot && explore {
        return Err("choose --snapshot or --explore".into());
    }
    if explore && cancel_at.is_some() {
        return Err("choose interactive decisions or --cancel-at".into());
    }
    let view = if snapshot {
        View::Snapshot
    } else if explore {
        View::Interactive
    } else {
        View::Headless
    };
    let mut task = Task::new(non_local, fuel)?;
    let mut failure = None;
    loop {
        let status = match task.poll(32) {
            Ok(status) => status,
            Err(error) => {
                failure = Some(error);
                break;
            }
        };
        match status {
            Status::Running => {}
            Status::NeedValue => {
                println!("NeedValue handler is waiting for an explicit recovery choice.");
                if explain {
                    print_stack(&task)?;
                }
                task.publish_replacement()?;
                println!(
                    "Published method revision {}; the pending method is still revision 0.",
                    task.revision()
                );
                let recover = format!("Recover NeedValue with {recovery}");
                if !driver_choice(&task, view, &[&recover, "Stop now; skip guest cleanup"])?
                    || cancel_at == Some(Status::NeedValue)
                {
                    task.stop()?;
                    break;
                }
                task.recover(recovery)?;
            }
            Status::CleanupWaiting => {
                println!("Original ensure cleanup is waiting for native work.");
                if explain {
                    print_stack(&task)?;
                }
                if !driver_choice(
                    &task,
                    view,
                    &[
                        "Complete native cleanup work",
                        "Stop now; skip guest cleanup",
                    ],
                )? || cancel_at == Some(Status::CleanupWaiting)
                {
                    task.stop()?;
                    break;
                }
                task.finish_cleanup_wait()?;
            }
            Status::Complete | Status::Stopped => break,
            Status::Cancelled => break,
        }
    }
    if let Some(answer) = task.answer {
        println!(
            "Answer: {answer}; task instruction fuel remaining: {}",
            task.remaining_fuel
        );
    } else {
        println!(
            "Task {} without an answer; already performed effects remain.",
            if failure.is_some() {
                "failed"
            } else {
                "cancelled"
            }
        );
    }
    let work = task.work()?;
    println!(
        "VM work: {} fuel, {} host calls, {} contract units, {} aggregate units, {} payload bytes",
        work.fuel, work.host_calls, work.contract, work.data.aggregate, work.data.payload
    );
    if explain {
        println!("Language decisions (embedding policy, separate from checked guest replay):");
        for event in &task.events {
            println!(
                "  {}\n    at {}",
                describe(&event.action),
                source(&event.site)?
            );
        }
    }
    println!("VM evidence:");
    for evidence in &task.evidence {
        println!(
            "  {} [{}]",
            evidence.label,
            evidence
                .code
                .program
                .program()
                .name()
                .unwrap_or("unnamed program")
        );
        match evidence.termination {
            Termination::Abandoned => {
                if !matches!(evidence.recording.replay()?, ReplayOutcome::Trapped(error) if matches!(error.trap, Trap::Abandoned))
                {
                    return Err("explicitly stopped invocation did not replay its stop".into());
                }
                println!("    explicitly abandoned; checked guest prefix and stop passed");
            }
            Termination::Returned => {
                if !matches!(evidence.recording.replay()?, ReplayOutcome::Returned(_)) {
                    return Err("successful invocation did not replay successfully".into());
                }
                println!("    returned; checked guest replay passed");
            }
            Termination::Cancelled => {
                println!("    cancelled; capture is incomplete, replay unavailable");
            }
            Termination::Trapped => println!("    trapped"),
        }
    }
    println!("Separate VM recordings do not replay method lookup or language cleanup policy.");
    if task.answer.is_some() || task.stopped() {
        let checked = task.check_replay()?;
        println!(
            "Checked composed task replay ({}): {} invocations, {} language decisions, {} fuel; native policy and guest observations matched.",
            if checked.stopped {
                "explicit stop"
            } else {
                "returned"
            },
            checked.invocations,
            checked.decisions,
            checked.work.fuel
        );
    } else {
        println!("Composed replay unavailable: this task did not complete.");
    }
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(())
}
