// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Compile and inspect a small condition/restart language protocol.

#[path = "conditions/language.rs"]
mod language;
#[path = "conditions/program.rs"]
mod program;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut case = None;
    let mut snapshot = false;
    let mut explore = false;
    let mut disassemble = false;
    for argument in std::env::args().skip(1) {
        match argument.as_str() {
            "--snapshot" => snapshot = true,
            "--explore" => explore = true,
            "--disassemble" => disassemble = true,
            "--help" => {
                println!(
                    "conditions [CASE] [--snapshot | --explore | --disassemble]\ncases: {}",
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
    println!("; entry=f{}", fixture.entry.0);
    if disassemble || !(snapshot || explore) {
        print!(
            "{}",
            execution_tape::disasm::disassemble_verified(&fixture.program)
        );
    } else {
        let viewer = execution_tape_observatory::ProgramBrowser::new(fixture.program.program())
            .sources(&fixture.sources)?;
        if explore {
            viewer.run()?;
        } else {
            println!("{}", viewer.render_text(140, 44)?);
        }
    }
    Ok(())
}
