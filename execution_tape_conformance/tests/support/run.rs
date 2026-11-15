// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Value-oriented assertions resolve the public durable result. This is test glue, not another
//! interpreter or a recommended embedding ownership model.

use execution_tape::{
    callable::Callable,
    host::Host,
    trace::{TraceMask, TraceSink},
    value::{FuncId, Value},
    verifier::VerifiedProgram,
    vm::{ControlError, Trap, TrapInfo, TurnOptions, TurnOutcome, Vm},
};
use std::sync::Arc;

pub(super) trait RunValues {
    fn run_values(
        &mut self,
        program: &VerifiedProgram,
        entry: FuncId,
        arguments: &[Value],
        mask: TraceMask,
        sink: Option<&mut dyn TraceSink>,
    ) -> Result<Vec<Value>, TrapInfo>;
}
impl<H: Host> RunValues for Vm<H> {
    fn run_values(
        &mut self,
        program: &VerifiedProgram,
        entry: FuncId,
        arguments: &[Value],
        mask: TraceMask,
        sink: Option<&mut dyn TraceSink>,
    ) -> Result<Vec<Value>, TrapInfo> {
        let fail = |trap| TrapInfo {
            func: entry,
            pc: 0,
            span_id: None,
            trap,
        };
        let callable = Callable::function(Arc::new(program.clone()), entry)
            .map_err(|_| fail(Trap::InvalidPc))?;
        let mut options = TurnOptions::default();
        if let Some(sink) = sink {
            options = options.trace(mask, sink);
        }
        match self.run(callable, arguments, options) {
            Ok(TurnOutcome::Returned(values)) => Ok(self
                .resolve(&values)
                .expect("published test result")
                .values()
                .to_vec()),
            Ok(other) => panic!("value assertion expected a return, got {other:?}"),
            Err(ControlError::Trapped(error)) => Err(error),
            Err(ControlError::Input(trap)) => Err(fail(trap)),
            Err(ControlError::Retention(error)) => Err(fail(Trap::ValueRoot(Box::new(error)))),
            Err(error) => panic!("invalid value assertion setup: {error}"),
        }
    }
}
