// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Equivalent loops for comparing block arguments with explicit register moves.

use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
use execution_tape::host::{Host, HostContext, HostError, SigHash, ValueRef};
use execution_tape::program::{Program, ValueType};
use execution_tape::value::Value;

/// A host that rejects all calls; these workloads perform none.
#[derive(Debug)]
pub struct NoHost;
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

/// Builds a loop that rotates `width` values `rounds - 1` times and returns them in register order.
///
/// `typed` uses simultaneous block arguments. The alternative expands the same transfers into
/// source moves with a temporary. Both variants retain the same arithmetic and branch condition.
pub fn rotation(width: u32, rounds: i64, typed: bool) -> Program {
    assert!((2..=4096).contains(&width), "width must be in 2..=4096");
    assert!(rounds > 0, "rounds must be positive");
    let mut asm = Asm::new();
    let values: Vec<_> = (1..=width).collect();
    let head = if typed {
        asm.block(&values)
    } else {
        asm.label()
    };
    let copying = if typed { None } else { Some(asm.label()) };
    let exit = asm.label();
    for &reg in &values {
        asm.const_i64(reg, i64::from(reg));
    }
    let counter = width + 1;
    let limit = width + 2;
    let one = width + 3;
    let condition = width + 4;
    let temporary = width + 5;
    asm.const_i64(counter, 0)
        .const_i64(limit, rounds)
        .const_i64(one, 1);
    asm.jmp(if typed {
        head.with_args(&values)
    } else {
        head.into()
    });
    asm.place(head).unwrap();
    asm.i64_add(counter, counter, one)
        .i64_lt(condition, counter, limit);
    if typed {
        let rotated: Vec<_> = values
            .iter()
            .copied()
            .cycle()
            .skip(1)
            .take(values.len())
            .collect();
        asm.br(condition, head.with_args(&rotated), exit);
    } else {
        let copying = copying.unwrap();
        asm.br(condition, copying, exit);
        asm.place(copying).unwrap();
        asm.mov(temporary, 1);
        for reg in 1..width {
            asm.mov(reg, reg + 1);
        }
        asm.mov(width, temporary).jmp(head);
    }
    asm.place(exit).unwrap();
    asm.ret(0, &values);
    let register_count = if typed { condition + 1 } else { temporary + 1 };
    let mut kinds = vec![ValueType::I64; register_count as usize];
    kinds[0] = ValueType::Unit;
    kinds[condition as usize] = ValueType::Bool;
    asm.set_register_types(kinds);
    let mut builder = ProgramBuilder::new();
    builder
        .push_function_checked(
            asm,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64; width as usize],
            },
        )
        .unwrap();
    builder.build()
}
