// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A first-class function returning a nested tuple through an exact indirect signature.
//! Pass an optional output filename to save the program for browsing.

use execution_tape::asm::{Asm, FunctionAbi, ProgramBuilder};
use execution_tape::contract::Contract;
use execution_tape::host::{Host, HostContext, HostError, SigHash, ValueRef};
use execution_tape::program::ValueType;
use execution_tape::trace::TraceMask;
use execution_tape::value::Value;
use execution_tape::vm::{Limits, Vm};

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
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut builder = ProgramBuilder::new();
    builder.set_program_name("typed_callable");
    let integer = builder.contract(Contract::Kind(ValueType::I64))?;
    let inner = builder.contract(Contract::Tuple(&[Some(integer)]))?;
    let outer = builder.contract(Contract::Tuple(&[Some(inner)]))?;
    let mut producer = Asm::new();
    producer.set_register_types(vec![
        ValueType::Unit,
        ValueType::I64,
        ValueType::Agg,
        ValueType::Agg,
    ]);
    producer
        .const_i64(1, 42)
        .tuple_new(2, &[1])
        .tuple_new(3, &[2])
        .ret(0, &[3]);
    let target = builder.push_function_checked(
        producer,
        FunctionAbi {
            args: vec![],
            rets: vec![outer.into()],
        },
    )?;
    builder.set_function_name(target, "produce_nested")?;
    let callable = builder.contract(Contract::Function {
        args: &[],
        rets: &[outer],
    })?;
    let indirect = builder.call_sig(FunctionAbi {
        args: vec![],
        rets: vec![outer.into()],
    })?;
    let mut caller = Asm::new();
    caller.set_register_types(vec![
        ValueType::Unit,
        ValueType::Func,
        ValueType::Agg,
        ValueType::I64,
    ]);
    caller
        .call_indirect(0, indirect, 1, 0, &[], &[2])
        .tuple_get(2, 2, 0)
        .tuple_get(3, 2, 0)
        .ret(0, &[3]);
    let entry = builder.push_function_checked(
        caller,
        FunctionAbi {
            args: vec![callable.into()],
            rets: vec![ValueType::I64.into()],
        },
    )?;
    builder.set_function_name(entry, "invoke_nested")?;
    let program = builder.build_verified()?;
    if let Some(path) = std::env::args_os().nth(1) {
        std::fs::write(path, program.program().encode())?;
    }
    let result = Vm::new(NoHost, Limits::default()).run(
        &program,
        entry,
        &[Value::Func(target)],
        TraceMask::NONE,
        None,
    )?;
    assert_eq!(result, vec![Value::I64(42)]);
    println!(
        "{}",
        match result[0] {
            Value::I64(value) => value,
            _ => unreachable!(),
        }
    );
    Ok(())
}
