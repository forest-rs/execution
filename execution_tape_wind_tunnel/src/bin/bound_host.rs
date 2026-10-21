// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Bind host operations before execution and reuse bindings with independent VM state.
//!
//! `cargo run -p execution_tape_wind_tunnel --bin bound_host`

use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
use execution_tape::host::{HostContext, HostError, HostSig, ValueRef};
use execution_tape::host_binding::HostFunctions;
use execution_tape::program::ValueType;
use execution_tape::trace::TraceMask;
use execution_tape::value::{FuncId, Value};
use execution_tape::vm::{Limits, Vm};

fn add(
    state: &mut i64,
    args: &[ValueRef<'_>],
    rets: &mut [Value],
    _: HostContext<'_, '_>,
) -> Result<u64, HostError> {
    let [ValueRef::I64(value)] = args else {
        return Err(HostError::Failed);
    };
    *state += value;
    rets[0] = Value::I64(*state);
    Ok(0)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let signature = HostSig {
        args: vec![ValueType::I64],
        rets: vec![ValueType::I64],
    };
    let mut functions = HostFunctions::new();
    functions.register("counter.add", signature.clone(), add)?;
    let mut builder = ProgramBuilder::new();
    let import = builder.host_sig_for("counter.add", signature);
    let mut body = Asm::new();
    body.host_call_sig_id(0, import, 0, &[1], &[2]);
    body.ret(0, &[2]);
    builder.push_function_checked(
        body,
        FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::I64],
        },
    )?;
    let program = builder.build_verified()?;

    // Every import is validated here, before either VM receives mutable host state.
    let bindings = functions.bind(&program)?;
    let mut first = Vm::new(bindings.with_state(0), Limits::default());
    let mut second = Vm::new(bindings.with_state(100), Limits::default());
    for vm in [&mut first, &mut second] {
        let result = vm.run(&program, FuncId(0), &[Value::I64(5)], TraceMask::NONE, None)?;
        println!("outputs={result:?} state={}", vm.host().state());
    }
    assert_eq!(*first.host().state(), 5);
    assert_eq!(*second.host().state(), 105);
    Ok(())
}
