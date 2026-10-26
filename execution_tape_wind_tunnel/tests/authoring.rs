// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

#[path = "../src/authoring.rs"]
mod authoring;

use execution_tape::{
    host::{Host, HostContext, HostError, SigHash, ValueRef},
    trace::TraceMask,
    value::{FuncId, Value},
    verifier::{VerifyConfig, verify_program_owned},
    vm::{Limits, Vm},
};

struct NoHost;
impl Host for NoHost {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        _: &[ValueRef<'_>],
        _: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        Err(HostError::UnknownSymbol)
    }
}

#[test]
fn streaming_registers_and_join_arguments_preserve_the_result() {
    for steps in [0, 32, 256, 1024] {
        let source = authoring::streaming(steps);
        let source = execution_tape::program::Program::decode(&source.encode()).unwrap();
        let program = verify_program_owned(source, &VerifyConfig::default()).unwrap();
        let mut vm = Vm::new(NoHost, Limits::default());
        for input in [-42, 0, 42] {
            assert_eq!(
                vm.run(
                    &program,
                    FuncId(0),
                    &[Value::I64(input)],
                    TraceMask::NONE,
                    None
                )
                .unwrap(),
                [Value::I64(input + i64::from(steps))]
            );
        }
    }
}

#[test]
fn lowered_callers_preserve_closure_contracts_and_source_metadata() {
    for count in [4, 32, 128] {
        let source = authoring::lowered(count);
        let source = execution_tape::program::Program::decode(&source.encode()).unwrap();
        let program = verify_program_owned(source, &VerifyConfig::default()).unwrap();
        let mut vm = Vm::new(NoHost, Limits::default());
        for function in 1..=count {
            assert_eq!(
                program.program().function_name(function),
                Some(format!("function_{}", function - 1).as_str())
            );
            assert_eq!(
                program.program().function_input_name(function, 0),
                Some("input")
            );
            assert_eq!(
                program.program().function_output_name(function, 0),
                Some("result")
            );
            let source = program.program();
            let spans = source
                .function_spans(&source.functions[function as usize])
                .unwrap();
            assert_eq!(spans.len(), 1);
            assert_eq!(spans[0].span_id.get(), u64::from(function) + 1);
            assert_eq!(
                vm.run(
                    &program,
                    FuncId(function),
                    &[Value::I64(-17)],
                    TraceMask::NONE,
                    None
                )
                .unwrap(),
                [Value::I64(25)]
            );
        }
    }
}
