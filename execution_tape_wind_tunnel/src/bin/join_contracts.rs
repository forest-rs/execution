// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Compare different tuple members at one typed join, then browse its shared shape facts.

use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
use execution_tape::host::{Host, HostContext, HostError, SigHash, ValueRef};
use execution_tape::program::{Program, ValueType};
use execution_tape::trace::TraceMask;
use execution_tape::value::Value;
use execution_tape::verifier::{VerifyConfig, verify_program_owned};
use execution_tape::vm::{Limits, Vm};
use std::{error::Error, io::Write};

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
        return Err("usage: join_contracts [OUTPUT.tape]".into());
    }
    if path.as_deref() == Some(std::ffi::OsStr::new("--help")) {
        println!(
            "usage: join_contracts [OUTPUT.tape]\n\nExecute both arms of a typed join; optionally create a new tape for Observatory."
        );
        return Ok(());
    }
    let mut builder = ProgramBuilder::new();
    builder.set_program_name("join_contracts");
    let mut body = Asm::new();
    body.set_register_types(vec![
        ValueType::Unit,
        ValueType::Bool,
        ValueType::I64,
        ValueType::Bool,
        ValueType::Agg,
        ValueType::Agg,
        ValueType::Agg,
        ValueType::U64,
    ]);
    let join = body.block(&[6]);
    body.const_i64(2, 42)
        .const_bool(3, true)
        .tuple_new(4, &[2])
        .tuple_new(5, &[3]);
    body.br(1, join.with_args(&[4]), join.with_args(&[5]));
    let header = body.pc();
    body.place(join)?;
    body.tuple_len(7, 6).ret(0, &[7]);
    let entry = builder.push_function_checked(
        body,
        FunctionSig {
            arg_types: vec![ValueType::Bool],
            ret_types: vec![ValueType::U64],
        },
    )?;
    builder.set_function_name(entry, "choose_tuple")?;
    let bytes = builder.build_checked()?.encode();
    let prepared = verify_program_owned(Program::decode(&bytes)?, &VerifyConfig::default())?;
    let mut vm = Vm::new(NoHost, Limits::default());
    for condition in [true, false] {
        let values = vm.run(
            &prepared,
            entry,
            &[Value::Bool(condition)],
            TraceMask::NONE,
            None,
        )?;
        assert_eq!(values, vec![Value::U64(1)]);
        println!("condition={condition}: tuple length {values:?}");
    }
    println!(
        "Join at byte {header}: r6 receives tuple(i64) or tuple(bool).\nThe common fact is tuple(?); its length is still known.\nIn Observatory, focus instructions, select BrArgs and Enter to follow the join."
    );
    if let Some(path) = path {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?
            .write_all(&bytes)?;
        println!("Created {}", std::path::Path::new(&path).display());
    }
    Ok(())
}
