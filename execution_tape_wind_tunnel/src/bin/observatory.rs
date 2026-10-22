// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Explore real captured host boundaries, evolving guest graphs and source instructions.

#[path = "../recording.rs"]
mod workload;

use execution_tape::trace::TraceSink;
use execution_tape::value::FuncId;
use execution_tape_recording::{RecordLimits, Recorder};
use std::sync::Arc;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if !args.is_empty() && args != ["--snapshot"] {
        return Err("usage: observatory [--snapshot]".into());
    }
    let program = workload::program(12, true);
    let (mut vm, inputs) = workload::fixture(4, 48);
    let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
    vm.run(
        &program,
        FuncId(0),
        &inputs,
        recorder.mask(),
        Some(&mut recorder),
    )?;
    let record = recorder.finish();
    record.status().map_err(Clone::clone)?;
    drop((vm, inputs));
    if args.is_empty() {
        execution_tape_observatory::explore(&record)?;
    } else {
        print!(
            "{}",
            execution_tape_observatory::render_text(&record, 120, 36)?
        );
    }
    Ok(())
}
