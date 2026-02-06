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

    a.const_bool(1, true);
    a.br(1, l_then, l_else);

    a.place(l_then).unwrap();
    a.const_i64(2, 10);
    a.ret(0, &[2]);

    a.place(l_else).unwrap();
    a.const_i64(2, 20);
    a.ret(0, &[2]);

    let mut pb = ProgramBuilder::new();
    pb.set_program_name("disasm_demo");
    let func = pb
        .push_function_checked(
            a,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
                reg_count: 3,
            },
        )
        .unwrap();
    pb.set_function_name(func, "main").unwrap();

    let program = pb.build_verified().unwrap();
    println!("{}", disassemble_verified(&program));
}
