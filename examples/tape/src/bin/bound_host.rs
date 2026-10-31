// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Bind host operations before execution and reuse bindings with independent VM state.
//!
//! `cargo run -p execution_tape_examples --bin bound_host`

use execution_tape::asm::{FunctionSig, ProgramBuilder};
use execution_tape::host::{HostContext, HostError, HostSig, ValueRef};
use execution_tape::host_binding::HostFunctions;
use execution_tape::program::ValueType;
use execution_tape::trace::TraceMask;
use execution_tape::value::Value;
use execution_tape::vm::Trap;
use execution_tape::vm::{Limits, Vm};

#[derive(Debug)]
struct CounterOverflow {
    current: i64,
    addend: i64,
}

impl std::fmt::Display for CounterOverflow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "counter overflow: {} + {}", self.current, self.addend)
    }
}

impl std::error::Error for CounterOverflow {}

fn add(
    state: &mut i64,
    args: &[ValueRef<'_>],
    rets: &mut [Value],
    _: HostContext<'_, '_>,
) -> Result<u64, HostError> {
    let [ValueRef::I64(value)] = args else {
        return Err(HostError::Failed);
    };
    *state = state.checked_add(*value).ok_or_else(|| {
        HostError::custom(CounterOverflow {
            current: *state,
            addend: *value,
        })
    })?;
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
    let entry = builder.declare_function(FunctionSig {
        arg_types: vec![ValueType::I64],
        ret_types: vec![ValueType::I64],
    })?;
    let mut body = builder.function_asm(entry)?;
    let addend = builder.argument(entry, 0)?;
    let [total] = builder.emit_call(&mut body, import, &[addend])?;
    body.ret(0, &[total]);
    builder.define_function(entry, body)?;
    let program = builder.build_verified()?;

    // Every import is validated here, before either VM receives mutable host state.
    let bindings = functions.bind(&program)?;
    let mut first = Vm::new(bindings.with_state(0), Limits::default());
    let mut second = Vm::new(bindings.with_state(100), Limits::default());
    for vm in [&mut first, &mut second] {
        let result = vm.run(&program, entry, &[Value::I64(5)], TraceMask::NONE, None)?;
        println!("outputs={result:?} state={}", vm.host().state());
    }
    assert_eq!(*first.host().state(), 5);
    assert_eq!(*second.host().state(), 105);
    *first.host_mut().state_mut() = i64::MAX;
    let failure = first
        .run(&program, entry, &[Value::I64(1)], TraceMask::NONE, None)
        .unwrap_err();
    let Trap::HostCallFailed {
        error: HostError::Custom(source),
        ..
    } = &failure.trap
    else {
        return Err(Box::new(failure));
    };
    let detail = source
        .downcast_ref::<CounterOverflow>()
        .expect("typed operation failure");
    assert_eq!(detail.current, i64::MAX);
    assert_eq!(detail.addend, 1);
    assert_eq!(*first.host().state(), i64::MAX);
    println!("failure={detail}");
    Ok(())
}
