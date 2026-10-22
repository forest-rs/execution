// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Capture one real invocation, drop the live host and VM, then replay and inspect it.

#[path = "../recording.rs"]
mod workload;

use execution_tape::trace::{TraceMask, TraceSink};
use execution_tape::value::FuncId;
use execution_tape_recording::{RecordLimits, Recorder, ReplayOutcome};
use std::sync::Arc;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let calls: i64 = args.next().map_or(Ok(1000), |s| s.parse())?;
    let depth: usize = args.next().map_or(Ok(256), |s| s.parse())?;
    let payload: usize = args.next().map_or(Ok(65536), |s| s.parse())?;
    if args.next().is_some() || !(1..=10_000).contains(&calls) || !(1..=100_000).contains(&depth) {
        return Err(
            "usage: record_replay [calls:1..10000] [depth:1..100000] [payload-bytes]".into(),
        );
    }
    let program = workload::program(calls, false);
    let (mut vm, args) = workload::fixture(depth, payload);
    let expected = vm.run(&program, FuncId(0), &args, TraceMask::NONE, None)?;
    let before = vm.host().calls;
    let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
    let actual = vm.run(
        &program,
        FuncId(0),
        &args,
        recorder.mask(),
        Some(&mut recorder),
    )?;
    assert_eq!(actual, expected, "capture preserves the live result");
    let record = recorder.finish();
    record.status().map_err(Clone::clone)?;
    assert_eq!(vm.host().calls - before, usize::try_from(calls)?);
    drop((vm, args));

    let ReplayOutcome::Returned(result) = record.replay()? else {
        return Err("replay unexpectedly trapped".into());
    };
    assert_eq!(result.aggregates().len_u32() as usize, depth);
    println!("Live host and VM dropped; fresh interpreter replay matched.");
    println!("Capture: {:?}", record.stats());
    println!(
        "Replay: {} roots, {} aggregate nodes",
        result.values().len(),
        result.aggregates().len_u32()
    );
    for (index, call) in record.host_calls().take(3).enumerate() {
        println!(
            "Call {index}: function {} pc {}, import {}, args {:?}",
            call.location.func.0, call.location.pc, call.import.0, call.args
        );
    }
    if record.host_calls().len() > 3 {
        println!("… {} further calls", record.host_calls().len() - 3);
    }
    println!("Final outcome: {:?}", record.outcome());
    Ok(())
}
