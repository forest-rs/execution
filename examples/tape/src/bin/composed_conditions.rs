// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Conditions, returning restarts, protected non-local exit and live code in one VM store.

#[path = "composed_conditions/program.rs"]
mod program;
#[path = "composed_conditions/runtime.rs"]
mod runtime;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Resume,
    Recover,
    Escape,
}

fn main() -> program::Result<()> {
    let mut mode = Mode::Resume;
    let mut snapshot = false;
    let mut explain = false;
    let mut disassemble = false;
    let mut fuel = 10_000;
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "resume" => mode = Mode::Resume,
            "recover" => mode = Mode::Recover,
            "escape" => mode = Mode::Escape,
            "--snapshot" => snapshot = true,
            "--explain" => explain = true,
            "--disassemble" => disassemble = true,
            "--fuel" => fuel = args.next().ok_or("--fuel needs an integer")?.parse()?,
            "--help" => {
                println!(
                    "composed_conditions [resume | recover | escape] [--explain] [--snapshot] [--disassemble] [--fuel N]\n\nresume: invoke a scoped returning restart, then finish old and replacement methods\nrecover: let the dynamic handler supply a resumption value directly\nescape: run guest cleanup, then return across handler and method to the lexical home\n\nAll cases use one VM store and one execution budget. Source and stack views use actual retained frames."
                );
                return Ok(());
            }
            _ => return Err(format!("unexpected argument {argument:?}; use --help").into()),
        }
    }
    let programs = program::Programs::build()?;
    if disassemble {
        for compiled in programs.all() {
            println!(
                "{}",
                execution_tape::disasm::disassemble_verified(&compiled.code)
            );
        }
        return Ok(());
    }
    let mut store = runtime::store(fuel);
    runtime::Runtime::new(&programs, &mut store, mode, snapshot, explain)?.run()
}
