// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Read the tool's persisted artifacts as an independent consumer.

use execution_tape::program::Program;
use execution_tape::source::{SourceLimits, SourceMap};
use execution_tape::trace::TraceLocation;
use execution_tape::value::FuncId;
use execution_tape::verifier::{VerifyConfig, verify_program_owned};
use footfall::session::capture::CaptureReader;
use footfall::session::{OwnedLabelValue, OwnedObservation};
use std::fs::{self, File};

#[test]
fn saved_capture_resolves_instruction_locations_against_saved_program() {
    let directory = std::env::temp_dir().join(format!("execution-footfall-{}", std::process::id()));
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_footfall_capture"))
        .arg(&directory)
        .output()
        .expect("capture tool starts");
    assert!(result.status.success(), "{result:?}");
    let program = verify_program_owned(
        Program::decode(&fs::read(directory.join("program.extape")).unwrap()).unwrap(),
        &VerifyConfig::default(),
    )
    .unwrap();
    let sources = SourceMap::decode(
        &fs::read(directory.join("program.sources")).unwrap(),
        SourceLimits::default(),
    )
    .unwrap();
    let bound = sources.bind(program.program()).unwrap();
    let mut reader =
        CaptureReader::new(File::open(directory.join("trace.footfall")).unwrap()).unwrap();
    let capture = reader.read_snapshot().unwrap().expect("one capture window");
    assert!(reader.read_snapshot().unwrap().is_none());
    assert_eq!(capture.window().missed(), 0);
    let context = capture
        .contexts()
        .iter()
        .find(|context| {
            context.entries().iter().any(|entry| {
                entry.key() == "program" && entry.value() == &OwnedLabelValue::Unsigned(1)
            })
        })
        .expect("program key retained in the capture");
    let mut runs = 0;
    let mut instructions = 0;
    let mut sourced_instructions = 0;
    for event in capture.events() {
        match event.observation() {
            OwnedObservation::ScopeBegin { name, .. } if name == "execution_tape.run" => runs += 1,
            OwnedObservation::Message {
                name,
                context_id,
                args,
                ..
            } if name == "execution_tape.instr" => {
                assert_eq!(*context_id, context.id());
                let number = |key| {
                    let argument = args.iter().find(|argument| argument.key() == key).unwrap();
                    let OwnedLabelValue::Unsigned(value) = argument.value() else {
                        panic!("unsigned instruction field");
                    };
                    u32::try_from(*value).unwrap()
                };
                let function = program.function(FuncId(number("func"))).unwrap();
                let instruction = function
                    .instructions()
                    .find(|instruction| instruction.pc == number("pc"))
                    .expect("captured PC is an instruction boundary");
                assert_eq!(u32::from(instruction.opcode as u8), number("opcode"));
                let source = bound
                    .lookup(TraceLocation {
                        func: function.id(),
                        pc: instruction.pc,
                    })
                    .expect("captured location has an explicit source mapping");
                assert_eq!(source.document().name, "footfall_demo.rs");
                assert!(!source.text().is_empty());
                assert!(source.start().line > 0);
                let span = instruction
                    .span
                    .map(|span| OwnedLabelValue::Unsigned(span.get()));
                let recorded = args
                    .iter()
                    .find(|argument| argument.key() == "span_id")
                    .map(|argument| argument.value());
                assert_eq!(recorded, span.as_ref());
                sourced_instructions += usize::from(span.is_some());
                instructions += 1;
            }
            _ => {}
        }
    }
    assert_eq!(runs, 5);
    assert!(instructions > 0);
    assert!(sourced_instructions > 0 && sourced_instructions < instructions);
    let manifest = fs::read_to_string(directory.join("manifest.json")).unwrap();
    assert!(
        manifest
            .contains("\"key\": 1, \"file\": \"program.extape\", \"sources\": \"program.sources\"")
    );
    assert!(manifest.contains("\"replayable\": false"));
    assert!(
        !fs::read(directory.join("trace.pftrace"))
            .unwrap()
            .is_empty()
    );
    fs::remove_dir_all(directory).unwrap();
}
