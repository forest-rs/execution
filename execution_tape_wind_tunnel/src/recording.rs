// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Shared workload for the recording tool and its benchmark.

use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
use execution_tape::host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef};
use execution_tape::program::ValueType;
use execution_tape::value::Value;
use execution_tape::verifier::VerifiedProgram;
use execution_tape::vm::{Limits, Vm};
use std::sync::Arc;

/// Live host whose only effect counts calls and whose return borrows an aggregate argument.
#[derive(Debug, Default)]
pub(super) struct Echo {
    pub(super) calls: usize,
}
impl Host for Echo {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        args: &[ValueRef<'_>],
        rets: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        self.calls += 1;
        rets[0] = args[0].to_value();
        Ok(0)
    }
}

/// A guest loop repeatedly passes the same graph through a host call.
pub(super) fn program(calls: i64, evolving: bool) -> Arc<VerifiedProgram> {
    assert!(calls > 0, "calls must be positive");
    let mut builder = ProgramBuilder::new();
    let import = builder.host_sig_for(
        "echo",
        HostSig {
            args: vec![ValueType::Agg],
            rets: vec![ValueType::Agg],
        },
    );
    let mut body = Asm::new();
    body.set_register_types(vec![
        ValueType::Unit,
        ValueType::Agg,
        ValueType::Agg,
        ValueType::I64,
        ValueType::I64,
        ValueType::I64,
        ValueType::Bool,
    ]);
    let head = body.label();
    let exit = body.label();
    body.const_i64(3, 0).const_i64(4, calls).const_i64(5, 1);
    body.jmp(head);
    body.place(head).unwrap();
    if evolving {
        body.tuple_new(2, &[3, 1]);
    }
    body.host_call_sig_id(0, import, 0, &[if evolving { 2 } else { 1 }], &[2]);
    body.i64_add(3, 3, 5).i64_lt(6, 3, 4).br(6, head, exit);
    body.place(exit).unwrap();
    body.ret(0, &[2]);
    builder
        .push_function_checked(
            body,
            FunctionSig {
                arg_types: vec![ValueType::Agg],
                ret_types: vec![ValueType::Agg],
            },
        )
        .unwrap();
    Arc::new(builder.build_verified().unwrap())
}

/// Seeds an immutable graph with two references to each predecessor and one byte payload.
pub(super) fn fixture(depth: usize, payload_bytes: usize) -> (Vm<Echo>, Vec<Value>) {
    assert!(depth > 0, "depth must be positive");
    let mut vm = Vm::new(Echo::default(), Limits::default());
    let mut root = vm
        .aggregates_mut()
        .tuple_new(vec![Value::Bytes(vec![42; payload_bytes])])
        .unwrap();
    for _ in 1..depth {
        root = vm
            .aggregates_mut()
            .tuple_new(vec![Value::Agg(root), Value::Agg(root)])
            .unwrap();
    }
    (vm, vec![Value::Agg(root)])
}
