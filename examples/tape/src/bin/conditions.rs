// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A language-layer condition/restart protocol using typed VM callbacks and scoped exits.

#[path = "conditions/language.rs"]
mod language;
#[path = "conditions/program.rs"]
mod program;
#[path = "conditions/runtime.rs"]
mod runtime;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut case = None;
    let mut snapshot = false;
    let mut explore = false;
    let mut disassemble = false;
    let mut static_view = false;
    for argument in std::env::args().skip(1) {
        match argument.as_str() {
            "--snapshot" => snapshot = true,
            "--explore" => explore = true,
            "--disassemble" => disassemble = true,
            "--static" => static_view = true,
            "--help" => {
                println!(
                    "conditions [CASE] [--snapshot | --explore | --disassemble] [--static]\ncases: {}",
                    program::CASES.join(", ")
                );
                return Ok(());
            }
            name if !name.starts_with('-') && case.is_none() => case = Some(argument),
            _ => return Err(format!("unexpected argument {argument:?}; use --help").into()),
        }
    }
    let fixture = match case {
        Some(name) => program::scenario(&name)?,
        None => program::demo()?,
    };
    if disassemble {
        print!(
            "{}",
            execution_tape::disasm::disassemble_verified(&fixture.program)
        );
        return Ok(());
    }
    if static_view {
        let viewer = execution_tape_observatory::ProgramBrowser::new(fixture.program.program())
            .sources(&fixture.sources)?;
        if explore {
            viewer.run()?;
        } else {
            println!("{}", viewer.render_text(140, 44)?);
        }
        return Ok(());
    }
    let mut runtime = runtime::Runtime::new(fixture.program, fixture.entry, Default::default())?;
    loop {
        match runtime.poll(128)? {
            runtime::Status::Running => {}
            runtime::Status::Complete => break,
            runtime::Status::Waiting(symbol) => {
                println!("waiting for {symbol}\n{}", runtime.explanation());
                runtime.complete_native(vec![])?;
            }
            runtime::Status::Unhandled(condition) => {
                let restarts = runtime.available(condition);
                println!("unhandled {condition:?}\n{}", runtime.explanation());
                if let Some(restart) = restarts.first() {
                    println!("embedding chooses {}", program::restart_name(restart.name));
                    runtime.choose_restart(restart.token, 42)?;
                } else {
                    println!("embedding default handler supplies (0, false)");
                    runtime.recover(0, false)?;
                }
            }
            runtime::Status::RestartReturned(values) => {
                println!("restart returned to debugger: {values:?}; signal remains pending");
                let [
                    execution_tape::value::Value::I64(value),
                    execution_tape::value::Value::Bool(flag),
                ] = values.as_slice()
                else {
                    return Err("wrong restart reply".into());
                };
                runtime.recover(*value, *flag)?;
            }
        }
    }
    println!("result: {:?}", runtime.outputs()?);
    let session = runtime.into_session();
    println!("language policy end: {:?}", session.policy_end);
    let record = &session.recording;
    let code = record.program();
    let sources = fixture.sources.bind(code.program())?;
    for event in &session.policy {
        let name = code
            .program()
            .function_name(event.location.func.0)
            .unwrap_or("unnamed");
        let source = sources.lookup(event.location);
        println!(
            "{:?} at {name}:{:06} · activation {:?} · {}",
            event.action,
            event.location.pc,
            event.activation,
            source
                .as_ref()
                .map_or("source unavailable", |source| source.text())
        );
    }
    if session.policy_truncated {
        println!("language policy log truncated; inspect VM capture completeness separately");
    }
    record.status().map_err(Clone::clone)?;
    println!(
        "checked guest replay under recorded controls: {:?}",
        record.replay()?
    );
    if snapshot || explore {
        let viewer =
            execution_tape_observatory::RecordingExplorer::new(record).sources(&fixture.sources)?;
        if explore {
            viewer.run()?;
        } else {
            println!("{}", viewer.render_text(140, 44)?);
        }
    }
    Ok(())
}
