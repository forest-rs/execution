// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! One verified definition, two host namespaces, one VM store.

use execution_tape::{
    asm::{FunctionSig, ProgramBuilder},
    host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef, sig_hash},
    instance::Instance,
    program::ValueType,
    value::{FuncId, Value},
    vm::{InvocationOptions, Limits, Vm},
};
use std::{collections::HashMap, error::Error};

fn signature() -> HostSig {
    HostSig {
        args: vec![],
        rets: vec![ValueType::U64],
    }
}

struct Counters {
    signature: SigHash,
    values: HashMap<Instance, u64>,
}
impl Host for Counters {
    fn call(
        &mut self,
        symbol: &str,
        hash: SigHash,
        _: &[ValueRef<'_>],
        results: &mut [Value],
        context: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        if symbol != "counter.next" {
            return Err(HostError::UnknownSymbol);
        }
        if hash != self.signature {
            return Err(HostError::SignatureMismatch);
        }
        let count = self.values.entry(context.instance().clone()).or_default();
        *count += 1;
        results[0] = Value::U64(*count);
        Ok(0)
    }
}

fn next(vm: &mut Vm<Counters>, instance: &Instance, entry: FuncId) -> Result<u64, Box<dyn Error>> {
    let mut call = instance.start(vm, entry, &[], InvocationOptions::default())?;
    call.resume(u64::MAX)?;
    match call.take_outputs().as_deref() {
        Some([Value::U64(count)]) => Ok(*count),
        _ => Err("counter entry must return one u64".into()),
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut builder = ProgramBuilder::new();
    builder.set_program_name("instance namespaces");
    let counter = builder.host_sig_for("counter.next", signature());
    let entry = builder.declare_function(FunctionSig {
        arg_types: vec![],
        ret_types: vec![ValueType::U64],
    })?;
    let mut body = builder.function_asm(entry)?;
    let [count] = builder.emit_call(&mut body, counter, &[])?;
    body.ret(0, &[count]);
    builder.define_function(entry, body)?;
    let program = builder.build_verified()?;

    let host = Counters {
        signature: sig_hash(&signature()),
        values: HashMap::new(),
    };
    let mut vm = Vm::new(host, Limits::default());
    let left = vm.instantiate(&program)?;
    let right = vm.instantiate(&program)?;
    println!("left:  {}", next(&mut vm, &left, entry)?);
    println!("right: {}", next(&mut vm, &right, entry)?);
    println!("left:  {}", next(&mut vm, &left, entry)?);
    Ok(())
}
