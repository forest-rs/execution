// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! No backend is installed in this separate integration test process.

use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
use execution_tape::host::{Host, HostContext, HostError, SigHash, ValueRef};
use execution_tape::program::ValueType;
use execution_tape::trace::{TraceMask, TraceSink};
use execution_tape::value::{FuncId, Value};
use execution_tape::vm::{Limits, Vm};
use execution_tape_footfall::FootfallSink;
use footfall::ContextHandle;

#[test]
fn runs_and_reuses_sink_without_backend() {
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
            unreachable!("no host instructions");
        }
    }
    let mut builder = ProgramBuilder::new();
    let mut body = Asm::new();
    body.set_register_types(vec![ValueType::Unit, ValueType::I64]);
    body.const_i64(1, 42);
    body.ret(0, &[1]);
    builder
        .push_function_checked(
            body,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
            },
        )
        .unwrap();
    let program = builder.build_verified().unwrap();
    let mut sink = FootfallSink::new(ContextHandle::ROOT);
    let mask = sink.mask() | TraceMask::INSTR;
    let mut vm = Vm::new(NoHost, Limits::default());
    for _ in 0..2 {
        assert_eq!(
            vm.run(&program, FuncId(0), &[], mask, Some(&mut sink))
                .unwrap(),
            [Value::I64(42)]
        );
    }
}
