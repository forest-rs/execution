// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Capture a typed join and nested host work with Footfall, including failure paths.

#[path = "../footfall_demo.rs"]
mod demo;

use demo::{Behavior, DemoHost, ENTRY, GUEST_TRAP};
use execution_tape::trace::{TraceMask, TraceSink};
use execution_tape::value::FuncId;
use execution_tape::value::Value;
use execution_tape::verifier::VerifiedProgram;
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
    // Resolve compact capture locations offline against this exact source/program pair.
    fs::write(
        directory.join("program.sources"),
        source_map(&program).encode(),
    )?;
    let mut writer = CaptureWriter::new(File::create(directory.join("trace.footfall"))?)?;
    writer.write_snapshot(&snapshot)?;
    writer.finish()?;
    footfall::export::write_perfetto_trace_file(&snapshot.into(), directory.join("trace.pftrace"))?;
    fs::write(
        directory.join("manifest.json"),
        format!(
            "{{\n  \"schema\": 1,\n  \"programs\": [{{\"key\": 1, \"file\": \"program.extape\", \"sources\": \"program.sources\"}}],\n  \"capture\": \"trace.footfall\",\n  \"perfetto\": \"trace.pftrace\",\n  \"runs\": 5,\n  \"events\": {events},\n  \"missed_events\": {missed},\n  \"replayable\": false\n}}\n"
        ),
    )?;
    println!(
        "capture={} runs=5 events={events} missed_events={missed}",
        directory.display()
    );
    Ok(())
}

/// Explicit mappings to this fixture's Rust construction statements, not inferred span IDs.
fn source_map(program: &VerifiedProgram) -> execution_tape::source::SourceMap {
    use execution_tape::source::{
        SourceDocument, SourceId, SourceLimits, SourceMap, SourceMapping,
    };
    use execution_tape::trace::TraceLocation;
    let text = include_str!("../footfall_demo.rs");
    let statements: &[(FuncId, &[&str])] = &[
        (
            FuncId(0),
            &[
                "callee.host_call_sig_id(0, import, 0, &[1], &[2]);",
                "callee.ret(0, &[2]);",
            ],
        ),
        (
            ENTRY,
            &[
                "body.br(1, yes, no);",
                "body.call(0, callee, 0, &[2], &[3]);",
                "body.jmp(join.with_args(&[3]));",
                "body.const_i64(4, 0);",
                "body.jmp(join.with_args(&[4]));",
                "body.place(join).expect(\"fresh join\");",
                "body.ret(0, &[5]);",
            ],
        ),
        (
            GUEST_TRAP,
            &[
                "trap.const_i64(1, 1);",
                "trap.const_i64(2, 0);",
                "trap.i64_div(3, 1, 2);",
                "trap.ret(0, &[3]);",
            ],
        ),
    ];
    let mut mappings = Vec::new();
    for &(func, statements) in statements {
        let source = execution_tape::disasm::disassemble_function(program.program(), func)
            .expect("fixture decodes");
        assert_eq!(
            source.instrs().count(),
            statements.len(),
            "each construction statement must map its emitted instruction"
        );
        for (instruction, statement) in source.instrs().zip(statements) {
            let start = text.find(statement).expect("fixture statement exists");
            mappings.push(SourceMapping {
                location: TraceLocation {
                    func,
                    pc: instruction.pc(),
                },
                document: SourceId(0),
                range: start..start + statement.len(),
            });
        }
    }
    SourceMap::new(
        program.program(),
        vec![SourceDocument {
            name: "footfall_demo.rs".into(),
            text: text.into(),
        }],
        mappings,
        SourceLimits::default(),
    )
    .expect("fixture source mappings are valid")
}
