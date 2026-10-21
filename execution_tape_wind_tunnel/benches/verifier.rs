// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Verification latency with program construction outside the timed region.
//!
//! Run with `cargo bench -p execution_tape_wind_tunnel --bench verifier`.
//! Straight-line functions vary instruction/register count; diamonds vary CFG size and live state.
//! Tuple-heavy functions exercise shape creation, moves, and projections.

use std::time::Duration;

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
use execution_tape::program::{Program, ValueType};
use execution_tape::verifier::{VerifyConfig, verify_program};

fn straight_line(instructions: u32) -> Program {
    let mut asm = Asm::new();
    asm.const_i64(1, 1);
    for dst in 2..instructions + 2 {
        asm.i64_add(dst, dst - 1, 1);
    }
    asm.ret(0, &[instructions + 1]);
    let mut kinds = vec![ValueType::I64; instructions as usize + 2];
    kinds[0] = ValueType::Unit;
    asm.set_register_types(kinds);
    program(asm)
}

fn diamonds(count: u32, registers: u32) -> Program {
    let mut asm = Asm::new();
    asm.const_bool(1, true);
    for reg in 2..registers {
        asm.const_i64(reg, 1);
    }
    // Include tuple metadata in the first block's outgoing state.
    asm.tuple_new(registers, &[2, 3]);
    for _ in 0..count {
        let left = asm.label();
        let right = asm.label();
        let join = asm.label();
        asm.br(1, left, right);
        asm.place(left).unwrap();
        asm.i64_add(2, 2, 3);
        asm.jmp(join);
        asm.place(right).unwrap();
        asm.i64_sub(2, 2, 3);
        asm.jmp(join);
        asm.place(join).unwrap();
    }
    asm.ret(0, &[2]);
    let mut kinds = vec![ValueType::I64; registers as usize + 1];
    kinds[0] = ValueType::Unit;
    kinds[1] = ValueType::Bool;
    kinds[registers as usize] = ValueType::Agg;
    asm.set_register_types(kinds);
    program(asm)
}

fn tuples(count: u32) -> Program {
    let mut asm = Asm::new();
    asm.const_i64(1, 1);
    asm.const_bool(2, true);
    for i in 0..count {
        let dst = 3 + i * 3;
        asm.tuple_new(dst, &[1, 2]);
        asm.mov(dst + 1, dst);
        asm.tuple_get(dst + 2, dst + 1, 0);
    }
    asm.ret(0, &[count * 3 + 2]);
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

fn bench_verifier(c: &mut Criterion) {
    let config = VerifyConfig::default();
    let mut group = c.benchmark_group("verify_program");
    for instructions in [32, 256, 1024] {
        let p = straight_line(instructions);
        group.bench_with_input(
            BenchmarkId::new("straight_line", instructions),
            &p,
            |b, p| b.iter(|| verify_program(black_box(p), black_box(&config)).unwrap()),
        );
    }
    for (count, registers) in [(16, 64), (64, 64), (64, 256)] {
        let p = diamonds(count, registers);
        group.bench_with_input(
            BenchmarkId::new("diamonds", format!("{count}x{registers}")),
            &p,
            |b, p| b.iter(|| verify_program(black_box(p), black_box(&config)).unwrap()),
        );
    }
    for count in [16, 64, 256] {
        let p = tuples(count);
        group.bench_with_input(BenchmarkId::new("tuples", count), &p, |b, p| {
            b.iter(|| verify_program(black_box(p), black_box(&config)).unwrap());
        });
    }
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_millis(300))
        .measurement_time(Duration::from_millis(1200))
        .sample_size(40);
    targets = bench_verifier
}
criterion_main!(benches);
