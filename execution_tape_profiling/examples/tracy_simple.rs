// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Simple Tracy-backed example for `execution_tape_profiling`.
//!
//! Run with:
//! `cargo run -p execution_tape_profiling --example tracy_simple`

use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
use execution_tape::host::{AccessSink, Host, HostError, HostSig, SigHash, ValueRef, sig_hash};
use execution_tape::program::ValueType;
use execution_tape::trace::TraceSink;
use execution_tape::value::{FuncId, Value};
use execution_tape::verifier::VerifiedProgram;
use execution_tape::vm::{Limits, Vm};

use execution_tape_profiling::{ProfilingTraceSink, ProgramSymbolResolver};

struct EchoHost {
    expected_sig: SigHash,
}

impl Host for EchoHost {
    fn call(
        &mut self,
        symbol: &str,
        sig_hash: SigHash,
        args: &[ValueRef<'_>],
        _access: Option<&mut dyn AccessSink>,
    ) -> Result<(Vec<Value>, u64), HostError> {
        if symbol != "trace.echo" {
            return Err(HostError::UnknownSymbol);
        }
        if sig_hash != self.expected_sig {
            return Err(HostError::SignatureMismatch);
        }
        let [ValueRef::I64(arg)] = args else {
            return Err(HostError::Failed);
        };
        Ok((vec![Value::I64(*arg)], 0))
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

    let leaf = pb.declare_function(FunctionSig {
        arg_types: vec![ValueType::I64],
        ret_types: vec![ValueType::I64],
        reg_count: 2,
    });
    pb.set_function_name(leaf, "leaf").expect("set leaf name");
    pb.set_function_input_name(leaf, 0, "x")
        .expect("set leaf input name");
    pb.set_function_output_name(leaf, 0, "x")
        .expect("set leaf output name");
    let stage_one = pb.declare_function(FunctionSig {
        arg_types: vec![ValueType::I64],
        ret_types: vec![ValueType::I64],
        reg_count: 4,
    });
    pb.set_function_name(stage_one, "stage_one")
        .expect("set stage_one name");
    pb.set_function_input_name(stage_one, 0, "x")
        .expect("set stage_one input name");
    pb.set_function_output_name(stage_one, 0, "y")
        .expect("set stage_one output name");
    let stage_two = pb.declare_function(FunctionSig {
        arg_types: vec![ValueType::I64],
        ret_types: vec![ValueType::I64],
        reg_count: 3,
    });
    pb.set_function_name(stage_two, "stage_two")
        .expect("set stage_two name");
    pb.set_function_input_name(stage_two, 0, "x")
        .expect("set stage_two input name");
    pb.set_function_output_name(stage_two, 0, "y")
        .expect("set stage_two output name");
    let main = pb.declare_function(FunctionSig {
        arg_types: vec![],
        ret_types: vec![ValueType::I64],
        reg_count: 4,
    });
    pb.set_function_name(main, "main").expect("set main name");
    pb.set_function_output_name(main, 0, "result")
        .expect("set main output name");

    let mut a_leaf = Asm::new();
    a_leaf.ret(0, &[1]);
    pb.define_function(leaf, a_leaf).unwrap();

    let mut a_stage_one = Asm::new();
    a_stage_one.call(0, leaf, 0, &[1], &[2]);
    a_stage_one.host_call_sig_id(0, host_sig_id, 0, &[2], &[3]);
    a_stage_one.ret(0, &[3]);
    pb.define_function(stage_one, a_stage_one).unwrap();

    let mut a_stage_two = Asm::new();
    a_stage_two.call(0, stage_one, 0, &[1], &[2]);
    a_stage_two.ret(0, &[2]);
    pb.define_function(stage_two, a_stage_two).unwrap();

    let mut a_main = Asm::new();
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

    let host = EchoHost { expected_sig };
    let mut vm = Vm::new(host, Limits::default());

    let mut sink = ProfilingTraceSink::with_resolver(ProgramSymbolResolver::default());
    let mask = sink.mask();
    let out = vm
        .run(&program, entry, &[], mask, Some(&mut sink))
        .expect("vm run failed");

    println!("result: {:?}", out);
}
