// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Capture a typed join and nested host work with Footfall, including failure paths.

#[path = "../footfall_demo.rs"]
mod demo;

use demo::{Behavior, DemoHost, ENTRY, GUEST_TRAP};
use execution_tape::trace::{TraceMask, TraceSink};
use execution_tape::value::Value;
use execution_tape::vm::{Limits, Vm};
use execution_tape_footfall::FootfallSink;
use footfall::runtime::Runtime;
use footfall::session::capture::CaptureWriter;
use footfall::{LabelKey, RegisteredContext};
use std::fs::{self, File};
use std::path::PathBuf;

static PROGRAM: LabelKey = LabelKey::new("program");

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let directory = std::env::args_os()
        .nth(1)
        .map_or_else(|| PathBuf::from(".local/footfall-demo"), PathBuf::from);
    let runtime = Runtime::builder()
        .process_name("execution_tape")
        .session_name("typed join and host boundary")
        .event_capacity(4096)
        .install_global()?;
    // This capture's manifest maps program key 1 to the exact saved container.
    let context = RegisteredContext::new(&footfall::context!(PROGRAM = 1_u64));
    let program = demo::program();
    let mut vm = Vm::new(
        DemoHost {
            context: context.handle(),
            behavior: Behavior::Success,
        },
        Limits::default(),
    );
    let mut sink = FootfallSink::new(context.handle());
    let mask = sink.mask() | TraceMask::INSTR;
    for (condition, expected) in [(true, 42), (false, 0)] {
        let result = vm.run(
            &program,
            ENTRY,
            &[Value::Bool(condition), Value::I64(41)],
            mask,
            Some(&mut sink),
        )?;
        assert_eq!(result, [Value::I64(expected)]);
    }
    for behavior in [Behavior::Failure, Behavior::InvalidReturn] {
        vm.host_mut().behavior = behavior;
        vm.run(
            &program,
            ENTRY,
            &[Value::Bool(true), Value::I64(41)],
            mask,
            Some(&mut sink),
        )
        .expect_err("intentional host failure");
    }
    vm.run(&program, GUEST_TRAP, &[], mask, Some(&mut sink))
        .expect_err("intentional guest failure");
    drop(sink);

    let snapshot = runtime.snapshot();
    let events = snapshot.events().len();
    let missed = snapshot.window().missed();
    fs::create_dir_all(&directory)?;
    fs::write(directory.join("program.extape"), program.program().encode())?;
    let mut writer = CaptureWriter::new(File::create(directory.join("trace.footfall"))?)?;
    writer.write_snapshot(&snapshot)?;
    writer.finish()?;
    footfall::export::write_perfetto_trace_file(&snapshot.into(), directory.join("trace.pftrace"))?;
    fs::write(
        directory.join("manifest.json"),
        format!(
            "{{\n  \"schema\": 1,\n  \"programs\": [{{\"key\": 1, \"file\": \"program.extape\"}}],\n  \"capture\": \"trace.footfall\",\n  \"perfetto\": \"trace.pftrace\",\n  \"runs\": 5,\n  \"events\": {events},\n  \"missed_events\": {missed},\n  \"replayable\": false\n}}\n"
        ),
    )?;
    println!(
        "capture={} runs=5 events={events} missed_events={missed}",
        directory.display()
    );
    Ok(())
}
