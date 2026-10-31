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
    }
}

fn main() -> program::Result<()> {
    let mut non_local = true;
    let mut explain = false;
    let mut snapshot = false;
    let mut explore = false;
    let mut fuel = 1_000;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--normal-return" => non_local = false,
            "--explain" => explain = true,
            "--snapshot" => snapshot = true,
            "--explore" => explore = true,
            "--fuel" => fuel = arguments.next().ok_or("--fuel needs an integer")?.parse()?,
            "--help" | "-h" => {
                println!(
                    "live_language [--normal-return] [--explain] [--snapshot | --explore] [--fuel N]\n\nA retained block signals through an independently compiled protected method.\nWhile its handler waits, publish a replacement; recover with 42 and run the\noriginal cleanup before returning to the exact lexical home.\n\n--normal-return keeps both callers' following code reachable.\n--explain shows exact source and the separate VM frames at each native wait.\n--snapshot renders both paused task views without terminal modes.\n--explore opens the read-only task view; q returns to the driver.\nThe finite language task owns dispatch, handler selection and cleanup policy.\nIts shared instruction fuel does not establish a shared memory/contract quota.\nCrossed child VMs are explicitly abandoned; their prefixes are checked by replay."
                );
                return Ok(());
            }
            _ => return Err(format!("unknown argument {argument:?}; use --help").into()),
        }
    }
    if snapshot && explore {
        return Err("choose --snapshot or --explore".into());
    }
    let mut task = Task::new(non_local, fuel)?;
    loop {
        match task.poll(32)? {
            Status::Running => {}
            Status::NeedValue => {
                println!("NeedValue handler is waiting for an explicit recovery choice.");
                if explain {
                    print_stack(&task)?;
                }
                if snapshot {
                    print!("{}", task.explorer()?.render_text(160, 48)?);
                }
                if explore {
                    task.explorer()?.run()?;
                }
                task.publish_replacement()?;
                println!(
                    "Published method revision {}; the pending method is still revision 0.",
                    task.revision()
                );
                task.recover(42)?;
            }
            Status::CleanupWaiting => {
                println!("Original ensure cleanup is waiting for native work.");
                if explain {
                    print_stack(&task)?;
                }
                if snapshot {
                    print!("{}", task.explorer()?.render_text(160, 48)?);
                }
                if explore {
                    task.explorer()?.run()?;
                }
                task.finish_cleanup_wait()?;
            }
            Status::Complete => break,
            Status::Cancelled => return Err("the language task was cancelled".into()),
        }
    }
    println!(
        "Answer: {}; task instruction fuel remaining: {}",
        task.answer.expect("successful answer"),
        task.remaining_fuel
    );
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
                println!(
                    "    cancelled during language exit; capture is incomplete, replay unavailable"
                );
            }
            Termination::Trapped => println!("    trapped"),
        }
    }
    println!("Separate VM recordings do not replay method lookup or language cleanup policy.");
    Ok(())
}
