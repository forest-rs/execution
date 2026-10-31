// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A first-class function returning a nested tuple through an exact indirect signature.
//! Pass an optional output filename to save the program for browsing.

use execution_tape::asm::{CallTarget, FunctionAbi, ProgramBuilder};
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
    let target = builder.declare_function(FunctionAbi {
        args: vec![],
        rets: vec![outer.into()],
    })?;
    let mut producer = builder.function_asm(target)?;
    let value = producer.alloc_register(ValueType::I64);
    let inner_tuple = producer.alloc_register(ValueType::Agg);
    let outer_tuple = producer.alloc_register(ValueType::Agg);
    producer
        .const_i64(value, 42)
        .tuple_new(inner_tuple, &[value])
        .tuple_new(outer_tuple, &[inner_tuple])
        .ret(0, &[outer_tuple]);
    builder.define_function(target, producer)?;
    builder.set_function_name(target, "produce_nested")?;
    let callable = builder.contract(Contract::Function {
        args: &[],
        rets: &[outer],
    })?;
    let indirect = builder.call_sig(FunctionAbi {
        args: vec![],
        rets: vec![outer.into()],
    })?;
    let entry = builder.declare_function(FunctionAbi {
        args: vec![callable.into()],
        rets: vec![ValueType::I64.into()],
    })?;
    let mut caller = builder.function_asm(entry)?;
    let function = builder.argument(entry, 0)?;
    let [outer_tuple] = builder.emit_call(
        &mut caller,
        CallTarget::Indirect {
            signature: indirect,
            callee: function,
        },
        &[],
    )?;
    let inner_tuple = caller.alloc_register(ValueType::Agg);
    let answer = caller.alloc_register(ValueType::I64);
    caller
        .tuple_get(inner_tuple, outer_tuple, 0)
        .tuple_get(answer, inner_tuple, 0)
        .ret(0, &[answer]);
    builder.define_function(entry, caller)?;
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
