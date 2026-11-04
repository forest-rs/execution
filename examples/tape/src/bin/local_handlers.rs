// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A returning effect handler that shares its resumer's lexical frame.

use execution_tape::{
    asm::{Asm, FunctionSig, ProgramBuilder},
    disasm::disassemble_verified,
    host::{Host, HostContext, HostError, SigHash, ValueRef},
    program::ValueType,
    source::{SourceDocument, SourceId, SourceLimits, SourceMap},
    trace::TraceMask,
    value::Value,
    vm::{Limits, Vm},
};
use std::error::Error;

const SOURCE: &str = "\
worker() {
    let first = perform next(10);
    return perform next(first);
}

main() {
    handle worker() with next(value, continue) {
        continue(value + 1) under the same handler;
    }
}
";

fn at(body: &mut Asm, expression: &str) {
    let start = SOURCE.find(expression).expect("authored expression");
    body.source(SourceId(0), start..start + expression.len());
}

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
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    if arguments.as_slice() == ["--help"] {
        println!(
            "usage: local_handlers [--disassemble]\n\nHandle two guest requests in one lexical clause, under a two-frame limit."
        );
        return Ok(());
    }
    if !arguments.is_empty() && arguments.as_slice() != ["--disassemble"] {
        return Err("usage: local_handlers [--disassemble]".into());
    }

    let mut builder = ProgramBuilder::new();
    builder.set_program_name("local handlers");
    let produces_integer = FunctionSig {
        arg_types: vec![],
        ret_types: vec![ValueType::I64],
    };
    let exchanges_integer = FunctionSig {
        arg_types: vec![ValueType::I64],
        ret_types: vec![ValueType::I64],
    };
    let initial = builder.call_sig(produces_integer.clone())?;
    let exchange = builder.call_sig(exchanges_integer.clone())?;
    let operation = builder.control_tag("next", exchanges_integer)?;
    let worker = builder.declare_function(produces_integer.clone())?;
    let main = builder.declare_function(produces_integer)?;
    builder.set_function_name(worker, "worker")?;
    builder.set_function_name(main, "main")?;

    let mut body = builder.function_asm(worker)?;
    let next = body.alloc_register(ValueType::Tag);
    let ten = body.alloc_register(ValueType::I64);
    at(&mut body, "let first = perform next(10);");
    body.const_tag(next, operation).const_i64(ten, 10);
    let first = builder.emit_suspend(&mut body, next, exchange, &[ten])?;
    at(&mut body, "return perform next(first);");
    let second = builder.emit_suspend(&mut body, next, exchange, &first)?;
    body.ret(0, &second);
    let mut mappings: Vec<_> = body.source_mappings(worker).collect();
    builder.define_function(worker, body)?;

    let mut body = builder.function_asm(main)?;
    let target = body.alloc_register(ValueType::Func);
    let next = body.alloc_register(ValueType::Tag);
    let one = body.alloc_register(ValueType::I64);
    at(&mut body, "handle worker() with next(value, continue)");
    body.const_func(target, worker)
        .const_tag(next, operation)
        .const_i64(one, 1);
    let computation = builder.emit_continuation(&mut body, target, initial)?;

    // A resume has explicit alternatives: final results, or a request and its continuation.
    let returned = builder.result_block(&mut body, initial)?;
    let clause = builder.handler_block(&mut body, next, exchange)?;
    builder.emit_resume(
        &mut body,
        computation,
        initial,
        &[],
        returned.label(),
        &[clause.arm()],
    )?;

    body.place(clause.label())?;
    at(&mut body, "continue(value + 1) under the same handler;");
    let answer = body.alloc_register(ValueType::I64);
    body.i64_add(answer, clause.request()[0], one);
    // Reentering this block does not add a handler function or another resumer frame.
    builder.emit_resume(
        &mut body,
        clause.continuation(),
        exchange,
        &[answer],
        returned.label(),
        &[clause.arm()],
    )?;

    body.place(returned.label())?;
    body.ret(0, returned.results());
    mappings.extend(body.source_mappings(main));
    builder.define_function(main, body)?;
    let program = builder.build_verified()?;
    let sources = SourceMap::new(
        program.program(),
        vec![SourceDocument {
            name: "local-handlers.guest".into(),
            text: SOURCE.into(),
        }],
        mappings,
        SourceLimits::default(),
    )?;
    // Validate the authored connections against exactly the definition being executed.
    sources.bind(program.program())?;

    if !arguments.is_empty() {
        println!("{SOURCE}\n{}", disassemble_verified(&program));
    }
    let mut vm = Vm::new(
        NoHost,
        Limits {
            max_call_depth: 2,
            ..Limits::default()
        },
    );
    let result = vm.run(&program, main, &[], TraceMask::NONE, None)?;
    assert_eq!(result, [Value::I64(12)]);
    println!("next(10) → 11; next(11) → 12. Final result: {result:?}");
    println!("Both requests used the same local clause, within a two-frame limit.");
    Ok(())
}
