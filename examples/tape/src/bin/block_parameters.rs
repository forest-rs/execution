// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Run equivalent loops and inspect their execution requirements.

#[path = "../support/block_parameters.rs"]
mod workload;
use execution_tape::value::FuncId;
use execution_tape::verifier::{VerifyConfig, verify_program_owned};
use execution_tape::vm::{Limits, Vm};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    for typed in [false, true] {
        let program =
            verify_program_owned(workload::rotation(8, 64, typed), &VerifyConfig::default())?;
        let function = program.function(FuncId(0)).unwrap();
        let mut vm = Vm::new(workload::NoHost, Limits::default());
        let entry = vm.function(&program, FuncId(0))?;
        let roots = vm
            .run(entry, [], execution_tape::vm::TurnOptions::default())?
            .into_returned()
            .expect("loop has no external request");
        let outputs = vm.resolve(&roots)?.values();
        println!(
            "{}: {:?}\nrequirements: {:?}",
            if typed {
                "block arguments"
            } else {
                "explicit moves"
            },
            outputs,
            function.stats()
        );
    }
    Ok(())
}
