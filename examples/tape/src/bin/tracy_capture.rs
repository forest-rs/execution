// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Simple Tracy-backed example for `execution_tape_tracy`.
//!
//! Run with:
//! `cargo run -p execution_tape_examples --bin tracy_capture`

use execution_tape::asm::{FunctionSig, ProgramBuilder};
use execution_tape::host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef, sig_hash};
use execution_tape::program::ValueType;
use execution_tape::trace::TraceSink;
use execution_tape::value::{FuncId, Value};
use execution_tape::verifier::VerifiedProgram;
use execution_tape::vm::{Limits, Vm};

use execution_tape_tracy::{ProfilingTraceSink, ProgramSymbolResolver};

struct EchoHost {
    expected_sig: SigHash,
}

impl Host for EchoHost {
    fn call(
        &mut self,
        symbol: &str,
        sig_hash: SigHash,
        args: &[ValueRef<'_>],
        rets: &mut [Value],
        _ctx: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        if symbol != "trace.echo" {
            return Err(HostError::UnknownSymbol);
        }
        if sig_hash != self.expected_sig {
            return Err(HostError::SignatureMismatch);
        }
        let [ValueRef::I64(arg)] = args else {
            return Err(HostError::Failed);
        };
        rets[0] = Value::I64(*arg);
        Ok(0)
    }
}

fn build_program() -> (VerifiedProgram, FuncId, SigHash) {
    let mut pb = ProgramBuilder::new();
    pb.set_program_name("tracy_example");
    let host_sig = HostSig {
        args: vec![ValueType::I64],
        rets: vec![ValueType::I64],
    };
    let expected_sig = sig_hash(&host_sig);
    let host_sig_id = pb.host_sig_for("trace.echo", host_sig);

    let leaf = pb
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    pb.set_function_name(leaf, "leaf").expect("set leaf name");
    pb.set_function_input_name(leaf, 0, "x")
        .expect("set leaf input name");
    pb.set_function_output_name(leaf, 0, "x")
        .expect("set leaf output name");
    let stage_one = pb
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    pb.set_function_name(stage_one, "stage_one")
        .expect("set stage_one name");
    pb.set_function_input_name(stage_one, 0, "x")
        .expect("set stage_one input name");
    pb.set_function_output_name(stage_one, 0, "y")
        .expect("set stage_one output name");
    let stage_two = pb
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    pb.set_function_name(stage_two, "stage_two")
        .expect("set stage_two name");
    pb.set_function_input_name(stage_two, 0, "x")
        .expect("set stage_two input name");
    pb.set_function_output_name(stage_two, 0, "y")
        .expect("set stage_two output name");
    let main = pb
        .declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    pb.set_function_name(main, "main").expect("set main name");
    pb.set_function_output_name(main, 0, "result")
        .expect("set main output name");

    let mut a_leaf = pb.function_asm(leaf).unwrap();
    a_leaf.ret(0, &[1]);
    pb.define_function(leaf, a_leaf).unwrap();

    let mut a_stage_one = pb.function_asm(stage_one).unwrap();
    a_stage_one.alloc_register(ValueType::I64);
    a_stage_one.alloc_register(ValueType::I64);
    a_stage_one.call(0, leaf, 0, &[1], &[2]);
    a_stage_one.host_call_sig_id(0, host_sig_id, 0, &[2], &[3]);
    a_stage_one.ret(0, &[3]);
    pb.define_function(stage_one, a_stage_one).unwrap();

    let mut a_stage_two = pb.function_asm(stage_two).unwrap();
    a_stage_two.alloc_register(ValueType::I64);
    a_stage_two.call(0, stage_one, 0, &[1], &[2]);
    a_stage_two.ret(0, &[2]);
    pb.define_function(stage_two, a_stage_two).unwrap();

    let mut a_main = pb.function_asm(main).unwrap();
    for _ in 0..3 {
        a_main.alloc_register(ValueType::I64);
    }
    a_main.const_i64(1, 42);
    a_main.call(0, stage_two, 0, &[1], &[2]);
    a_main.host_call_sig_id(0, host_sig_id, 0, &[2], &[3]);
    a_main.ret(0, &[3]);
    pb.define_function(main, a_main).unwrap();

    let program = pb.build_verified().unwrap();
    (program, main, expected_sig)
}

fn main() {
    // Tracy requires the client to be started before instrumentation.
    let _tracy = tracy_client::Client::start();
    println!("Waiting for Tracy connection...");
    let start = std::time::Instant::now();
    while !tracy_client::Client::is_connected()
        && start.elapsed() < std::time::Duration::from_secs(5)
    {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    if tracy_client::Client::is_connected() {
        println!("Tracy connected.");
    } else {
        println!("No Tracy connection detected; continuing.");
    }

    let (program, entry, expected_sig) = build_program();
    let sources = source_map(&program);

    let host = EchoHost { expected_sig };
    let mut vm = Vm::new(host, Limits::default());

    let mut sink = ProfilingTraceSink::with_resolver(ProgramSymbolResolver::default())
        .sources(program.program(), &sources)
        .expect("matching source map");
    let mask = sink.mask();
    let out = vm
        .run(&program, entry, &[], mask, Some(&mut sink))
        .expect("vm run failed");

    println!("result: {:?}", out);
}

// The demo maps explicit Rust construction statements, not inferred language spans.
fn source_map(program: &VerifiedProgram) -> execution_tape::source::SourceMap {
    use execution_tape::source::{
        SourceDocument, SourceId, SourceLimits, SourceMap, SourceMapping,
    };
    use execution_tape::trace::TraceLocation;
    let text = include_str!("tracy_capture.rs");
    let statements: &[&[&str]] = &[
        &["a_leaf.ret(0, &[1]);"],
        &[
            "a_stage_one.call(0, leaf, 0, &[1], &[2]);",
            "a_stage_one.host_call_sig_id(0, host_sig_id, 0, &[2], &[3]);",
            "a_stage_one.ret(0, &[3]);",
        ],
        &[
            "a_stage_two.call(0, stage_one, 0, &[1], &[2]);",
            "a_stage_two.ret(0, &[2]);",
        ],
        &[
            "a_main.const_i64(1, 42);",
            "a_main.call(0, stage_two, 0, &[1], &[2]);",
            "a_main.host_call_sig_id(0, host_sig_id, 0, &[2], &[3]);",
            "a_main.ret(0, &[3]);",
        ],
    ];
    let mut mappings = Vec::new();
    for (function, statements) in statements.iter().enumerate() {
        let func = FuncId(u32::try_from(function).unwrap());
        let instructions = program
            .function(func)
            .expect("demo function")
            .instructions();
        assert_eq!(
            instructions.len(),
            statements.len(),
            "each demo instruction must have its construction source"
        );
        for (instruction, statement) in instructions.zip(*statements) {
            let start = text.find(statement).expect("demo statement exists");
            mappings.push(SourceMapping {
                location: TraceLocation {
                    func,
                    pc: instruction.pc,
                },
                document: SourceId(0),
                range: start..start + statement.len(),
            });
        }
    }
    SourceMap::new(
        program.program(),
        vec![SourceDocument {
            name: file!().into(),
            text: text.into(),
        }],
        mappings,
        SourceLimits::default(),
    )
    .expect("demo source maps")
}

#[cfg(test)]
mod tests {
    use super::*;
    use execution_tape::trace::{TraceLocation, TraceMask};

    #[test]
    fn demo_executes_and_maps_every_instruction_without_a_tracy_client() {
        let (program, entry, expected_sig) = build_program();
        let sources = source_map(&program);
        let bound = sources.bind(program.program()).unwrap();
        for index in 0..program.program().functions.len() {
            let func = FuncId(u32::try_from(index).unwrap());
            for instruction in program.function(func).unwrap().instructions() {
                assert!(
                    !bound
                        .lookup(TraceLocation {
                            func,
                            pc: instruction.pc
                        })
                        .unwrap()
                        .text()
                        .is_empty()
                );
            }
        }
        let mut vm = Vm::new(EchoHost { expected_sig }, Limits::default());
        assert_eq!(
            vm.run(&program, entry, &[], TraceMask::NONE, None).unwrap(),
            [Value::I64(42)]
        );
    }
}
