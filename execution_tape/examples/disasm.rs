// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Disassembler example.
//!
//! Run with:
//! `cargo run -p execution_tape --example disasm`

use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
use execution_tape::disasm::disassemble_verified;
use execution_tape::program::ValueType;

fn main() {
    let mut a = Asm::new();
    let l_then = a.label_named("then");
    let l_else = a.label_named("else");

    // if cond { a + b } else { a - b }
    a.br(1, l_then, l_else);

    a.place(l_then).unwrap();
    a.i64_add(4, 2, 3);
    a.ret(0, &[4]);

    a.place(l_else).unwrap();
    a.i64_sub(4, 2, 3);
    a.ret(0, &[4]);

    let mut pb = ProgramBuilder::new();
    pb.set_program_name("disasm_demo");
    let func = pb
        .push_function_checked(
            a,
            FunctionSig {
                arg_types: vec![ValueType::Bool, ValueType::I64, ValueType::I64],
                ret_types: vec![ValueType::I64],
            },
        )
        .unwrap();
    pb.set_function_name(func, "main").unwrap();
    pb.set_function_input_name(func, 0, "cond").unwrap();
    pb.set_function_input_name(func, 1, "a").unwrap();
    pb.set_function_input_name(func, 2, "b").unwrap();
    pb.set_function_output_name(func, 0, "out").unwrap();

    let program = pb.build_verified().unwrap();
    println!("{}", disassemble_verified(&program));
}
