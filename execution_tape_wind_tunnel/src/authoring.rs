// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Two frontend workloads: incremental temporary allocation and pre-numbered lowered bodies.

use execution_tape::asm::{Asm, FunctionAbi, FunctionSig, ProgramBuilder};
use execution_tape::contract::Contract;
use execution_tape::program::{Program, SpanId, ValueType};

pub(super) fn streaming(steps: u32) -> Program {
    let mut builder = ProgramBuilder::new();
    let function = builder
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let mut body = builder.function_asm(function).unwrap();
    let one = body.alloc_register(ValueType::I64);
    body.const_i64(one, 1);
    let mut value = 1;
    for step in 0..steps {
        body.span(SpanId::try_from(u64::from(step) + 1).unwrap());
        let next = body.alloc_register(ValueType::I64);
        body.i64_add(next, value, one);
        if step % 4 == 0 {
            let condition = body.alloc_register(ValueType::Bool);
            let joined = body.alloc_register(ValueType::I64);
            body.i64_lt(condition, value, next);
            let join = body.block(&[joined]);
            body.br(condition, join.with_args(&[next]), join.with_args(&[value]));
            body.place(join).unwrap();
            value = joined;
        } else {
            value = next;
        }
    }
    body.ret(0, &[value]);
    builder.define_function(function, body).unwrap();
    builder.build()
}

pub(super) fn lowered(callers: u32) -> Program {
    let mut builder = ProgramBuilder::new();
    let integer = builder.contract(Contract::Kind(ValueType::I64)).unwrap();
    let environment = builder.contract(Contract::Tuple(&[Some(integer)])).unwrap();
    let callee = builder
        .declare_function(FunctionAbi {
            args: vec![environment.into(), integer.into()],
            rets: vec![integer.into()],
        })
        .unwrap();
    let signature = FunctionSig {
        arg_types: vec![ValueType::I64],
        ret_types: vec![ValueType::I64],
    };
    let call = builder.call_sig(&signature).unwrap();
    let functions: Vec<_> = (0..callers)
        .map(|index| {
            let function = builder.declare_function(&signature).unwrap();
            builder
                .set_function_name(function, &format!("function_{index}"))
                .unwrap();
            builder
                .set_function_input_name(function, 0, "input")
                .unwrap();
            builder
                .set_function_output_name(function, 0, "result")
                .unwrap();
            function
        })
        .collect();
    let mut body = Asm::new();
    body.set_register_types(vec![
        ValueType::Unit,
        ValueType::Agg,
        ValueType::I64,
        ValueType::I64,
        ValueType::I64,
    ]);
    body.tuple_get(3, 1, 0).i64_add(4, 2, 3).ret(0, &[4]);
    builder.define_function(callee, body).unwrap();
    for function in functions {
        let mut body = Asm::new();
        body.set_register_types(vec![
            ValueType::Unit,
            ValueType::I64,
            ValueType::I64,
            ValueType::Agg,
            ValueType::Func,
            ValueType::Closure,
            ValueType::I64,
        ]);
        body.span(SpanId::try_from(u64::from(function.0) + 1).unwrap());
        body.const_i64(2, 42)
            .tuple_new(3, &[2])
            .const_func(4, callee)
            .closure_new(5, 4, 3)
            .call_indirect(0, call, 5, 0, &[1], &[6])
            .ret(0, &[6]);
        builder.define_function(function, body).unwrap();
    }
    builder.build()
}
