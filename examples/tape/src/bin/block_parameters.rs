// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Run equivalent loops and inspect their execution requirements.

#[path = "../support/block_parameters.rs"]
mod workload;
use execution_tape::trace::TraceMask;
use execution_tape::value::FuncId;
use execution_tape::verifier::{VerifyConfig, verify_program_owned};
use execution_tape::vm::{Limits, Vm};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    for typed in [false, true] {
        let program =
            verify_program_owned(workload::rotation(8, 64, typed), &VerifyConfig::default())?;
        let function = program.function(FuncId(0)).unwrap();
        let outputs = Vm::new(workload::NoHost, Limits::default()).run(
            &program,
            FuncId(0),
            &[],
            TraceMask::NONE,
            None,
        )?;
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
