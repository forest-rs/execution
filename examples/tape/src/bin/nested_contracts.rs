// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A nested guest result through the ordinary assembly, codec, verifier and interpreter path.

use std::error::Error;
use std::io::Write;

use execution_tape::asm::{Asm, FunctionAbi, FunctionSig, ProgramBuilder};
use execution_tape::contract::Contract;
use execution_tape::host::{Host, HostContext, HostError, SigHash, ValueRef};
use execution_tape::program::{Program, ValueType};
use execution_tape::trace::TraceMask;
use execution_tape::value::Value;
use execution_tape::verifier::{VerifyConfig, verify_program_owned};
use execution_tape::vm::{Limits, Vm};

#[derive(Debug)]
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

fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let path = arguments.next();
    if arguments.next().is_some() {
        return Err("usage: nested_contracts [OUTPUT.tape]".into());
    }
    if path.as_deref() == Some(std::ffi::OsStr::new("--help")) {
        println!(
            "usage: nested_contracts [OUTPUT.tape]\n\nExecute a nested-result example; optionally create a new compiled tape for Observatory."
        );
        return Ok(());
    }
    let mut builder = ProgramBuilder::new();
    builder.set_program_name("nested_results");
    let integer = builder.contract(Contract::Kind(ValueType::I64))?;
    let boolean = builder.contract(Contract::Kind(ValueType::Bool))?;
    let inner = builder.contract(Contract::Tuple(&[Some(integer)]))?;
    let outer = builder.contract(Contract::Tuple(&[Some(inner), Some(boolean)]))?;
    let entry = builder.declare_function(FunctionSig {
        arg_types: vec![],
        ret_types: vec![ValueType::I64],
    })?;
    let mut producer = Asm::new();
    producer.set_register_types(vec![
        ValueType::Unit,
        ValueType::I64,
        ValueType::Bool,
        ValueType::Agg,
        ValueType::Agg,
    ]);
    producer
        .const_i64(1, 42)
        .const_bool(2, true)
        .tuple_new(3, &[1])
        .tuple_new(4, &[3, 2])
        .ret(0, &[4]);
    let make = builder.push_function_checked(
        producer,
        FunctionAbi {
            args: vec![],
            rets: vec![outer.into()],
        },
    )?;
    builder.set_function_name(make, "make_nested")?;
    let mut consumer = Asm::new();
    consumer.set_register_types(vec![ValueType::Unit, ValueType::Agg, ValueType::I64]);
    consumer
        .call(0, make, 0, &[], &[1])
        .tuple_get(1, 1, 0)
        .tuple_get(2, 1, 0)
        .ret(0, &[2]);
    builder.define_function(entry, consumer)?;
    builder.set_function_name(entry, "main")?;
    let source = builder.build_checked()?;
    let bytes = source.encode();
    let program = verify_program_owned(Program::decode(&bytes)?, &VerifyConfig::default())?;
    let values =
        Vm::new(NoHost, Limits::default()).run(&program, entry, &[], TraceMask::NONE, None)?;
    assert_eq!(values, vec![Value::I64(42)]);
    println!("make_nested() returns ((42), true); main projects the nested integer: {values:?}");
    println!(
        "{} shared contracts, {} encoded bytes",
        program.program().contracts.len(),
        bytes.len()
    );
    for (id, contract) in program.program().contracts.iter() {
        println!("contract #{}: {contract:?}", id.0);
    }
    if let Some(path) = path {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        file.write_all(&bytes)?;
        println!("Created {}", std::path::Path::new(&path).display());
    }
    Ok(())
}
