// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder, Reg};
use execution_tape::program::{Program, ValueType};

pub(super) fn straight_line(instructions: u32) -> Program {
    let mut asm = Asm::new();
    asm.const_i64(Reg(1), 1);
    for dst in 2..instructions + 2 {
        asm.i64_add(Reg(dst), Reg(dst - 1), Reg(1));
    }
    asm.ret(Reg(0), &[Reg(instructions + 1)]);
    let mut kinds = vec![ValueType::I64; instructions as usize + 2];
    kinds[0] = ValueType::Unit;
    asm.set_register_types(kinds);
    program(asm)
}

pub(super) fn diamonds(count: u32, registers: u32) -> Program {
    let mut asm = Asm::new();
    asm.const_bool(Reg(1), true);
    for reg in 2..registers {
        asm.const_i64(Reg(reg), 1);
    }
    // Include tuple metadata in the first block's outgoing state.
    asm.tuple_new(Reg(registers), &[Reg(2), Reg(3)]);
    for _ in 0..count {
        let left = asm.label();
        let right = asm.label();
        let join = asm.label();
        asm.br(Reg(1), left, right);
        asm.place(left).unwrap();
        asm.i64_add(Reg(2), Reg(2), Reg(3));
        asm.jmp(join);
        asm.place(right).unwrap();
        asm.i64_sub(Reg(2), Reg(2), Reg(3));
        asm.jmp(join);
        asm.place(join).unwrap();
    }
    asm.ret(Reg(0), &[Reg(2)]);
    let mut kinds = vec![ValueType::I64; registers as usize + 1];
    kinds[0] = ValueType::Unit;
    kinds[1] = ValueType::Bool;
    kinds[registers as usize] = ValueType::Agg;
    asm.set_register_types(kinds);
    program(asm)
}

pub(super) fn tuples(count: u32) -> Program {
    let mut asm = Asm::new();
    asm.const_i64(Reg(1), 1);
    asm.const_bool(Reg(2), true);
    for i in 0..count {
        let dst = 3 + i * 3;
        asm.tuple_new(Reg(dst), &[Reg(1), Reg(2)]);
        asm.mov(Reg(dst + 1), Reg(dst));
        asm.tuple_get(Reg(dst + 2), Reg(dst + 1), 0);
    }
    asm.ret(Reg(0), &[Reg(count * 3 + 2)]);
    let mut kinds = Vec::with_capacity(3 + count as usize * 3);
    kinds.extend_from_slice(&[ValueType::Unit, ValueType::I64, ValueType::Bool]);
    for _ in 0..count {
        kinds.extend_from_slice(&[ValueType::Agg, ValueType::Agg, ValueType::I64]);
    }
    asm.set_register_types(kinds);
    program(asm)
}

fn program(asm: Asm) -> Program {
    let mut builder = ProgramBuilder::new();
    builder
        .push_function_checked(
            asm,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
            },
        )
        .unwrap();
    builder.build()
}
