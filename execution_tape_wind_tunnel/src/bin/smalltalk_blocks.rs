// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A Smalltalk-style block story: shared cells, lexical returns, ensure and an escaped return.

#[path = "smalltalk_blocks/program.rs"]
mod program;
#[path = "smalltalk_blocks/runtime.rs"]
mod runtime;

use execution_tape::recording::ReplayOutcome;
use runtime::{Policy, Runtime, Status};

fn main() -> program::Result<()> {
    let mut non_local = true;
    let mut explain = false;
    let mut snapshot = false;
    let mut explore = false;
    let mut disassemble = false;
    for argument in std::env::args().skip(1) {
        match argument.as_str() {
            "--normal-return" => non_local = false,
            "--explain" => explain = true,
            "--snapshot" => snapshot = true,
            "--explore" => explore = true,
            "--disassemble" => disassemble = true,
            "--help" | "-h" => {
                println!(
                    "smalltalk_blocks [--normal-return] [--explain] [--snapshot | --explore | --disassemble]\n\nTwo distinct blocks share a counter, then a block returns from its lexical home\nthrough nested ensure cleanup. A later block outlives its home and raises\nBlockCannotReturn. This driver explicitly supplies -1 as a fallback for the failed return.\n\n--normal-return removes the first block's ^, so code after ensure executes.\n--explain includes language decisions, retained guest frames and authored source.\n--snapshot renders the completed VM recording without terminal modes.\nThis hand-lowered fixture is not a Smalltalk parser or complete language runtime."
                );
                return Ok(());
            }
            _ => return Err(format!("unexpected argument {argument:?}; use --help").into()),
        }
    }
    let fixture = program::compile(non_local)?;
    if disassemble {
        print!(
            "{}",
            execution_tape::disasm::disassemble_verified(&fixture.program)
        );
        return Ok(());
    }
    let sources = fixture.sources.bind(fixture.program.program())?;
    let mut runtime = Runtime::new(&fixture, Default::default())?;
    loop {
        match runtime.poll(64)? {
            Status::Running => {}
            Status::Complete => break,
            Status::Waiting => {
                println!("ensure cleanup is waiting for native work");
                if explain {
                    for frame in runtime.paused_frames() {
                        let name = fixture
                            .program
                            .program()
                            .function_name(frame.location.func.0)
                            .unwrap_or("unnamed");
                        println!(
                            "  activation #{}: {name} at byte {}",
                            frame.activation.ordinal(),
                            frame.location.pc
                        );
                    }
                }
                runtime.finish_native_wait()?;
            }
            Status::CannotReturn(error) => {
                println!("{error}");
                if let Some(source) = sources.lookup(error.block) {
                    println!("  attempted return: {}", source.text());
                }
                if let Some(source) = sources.lookup(error.home) {
                    println!("  lexical home: {}", source.text());
                }
                println!("driver supplies -1 as an explicit fallback; the block returns normally");
                runtime.supply_return_fallback(-1)?;
            }
        }
    }
    let [first, second, answer, late] = runtime.outputs()?;
    let session = runtime.into_session();
    println!("shared counter: first block → {first}, second block → {second}");
    println!(
        "answerUsing: → {answer}; cleanup order: {:?}",
        session.cleanups
    );
    println!(
        "code after ensure: {}",
        if session.after_ensure {
            "executed"
        } else {
            "skipped by lexical return"
        }
    );
    println!("escaped block after explicit recovery: {late}");
    if explain {
        println!("Language decisions at pending service sites (separate from VM replay evidence):");
        for event in &session.events {
            let decision = match &event.policy {
                Policy::HomeCaptured { activation } => {
                    format!("capture lexical home activation #{activation}")
                }
                Policy::EnsureEntered { body } => {
                    format!("enter ensure for body activation #{body}")
                }
                Policy::CleanupStarted { body } => {
                    format!("start cleanup for consumed body activation #{body}")
                }
                Policy::CleanupFinished { body } => {
                    format!("cleanup for body activation #{body} returned")
                }
                Policy::ReturnSelected { home, value } => {
                    format!("select return of {value} from home activation #{home}")
                }
                Policy::CannotReturn(error) => error.to_string(),
                Policy::ReturnFallback { value } => {
                    format!("embedding supplies fallback {value} for the failed return")
                }
            };
            let source = sources.lookup(event.location);
            println!(
                "  {decision}\n    {}",
                source
                    .as_ref()
                    .map_or("source unavailable", |source| source.text())
            );
        }
    }
    session.recording.status().map_err(Clone::clone)?;
    let ReplayOutcome::Returned(replayed) = session.recording.replay()? else {
        return Err("guest replay did not return".into());
    };
    let replayed = runtime::decode_outputs(replayed.values(), replayed.aggregates())?;
    if replayed != [first, second, answer, late] {
        return Err("guest replay result disagrees with live execution".into());
    }
    println!("checked guest replay under recorded controls: {replayed:?}");
    if snapshot || explore {
        let viewer = execution_tape_observatory::RecordingExplorer::new(&session.recording)
            .sources(&fixture.sources)?;
        if explore {
            viewer.run()?;
        } else {
            println!("{}", viewer.render_text(140, 44)?);
        }
    }
    Ok(())
}
