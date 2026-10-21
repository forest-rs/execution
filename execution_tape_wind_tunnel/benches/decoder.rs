// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Container decoding and drop, with encoding and input validation outside the timed region.
//!
//! Run with `cargo bench -p execution_tape_wind_tunnel --bench decoder`.
//! Cases keep instruction count similar while varying the number of function bodies/span tables.

use std::hint::black_box;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
use execution_tape::program::{Program, SpanId, ValueType};

fn encoded_program(functions: u32, additions: u32) -> Vec<u8> {
    let mut builder = ProgramBuilder::new();
    for _ in 0..functions {
        let mut asm = Asm::new();
        asm.set_register_types(vec![ValueType::Unit, ValueType::I64]);
        asm.const_i64(1, 1);
        for instruction in 0..additions {
            asm.span(SpanId::try_from(u64::from(instruction) + 1).unwrap());
            asm.i64_add(1, 1, 1);
        }
        asm.ret(0, &[1]);
        builder
            .push_function_checked(
                asm,
                FunctionSig {
                    arg_types: vec![],
                    ret_types: vec![ValueType::I64],
                },
            )
            .unwrap();
    }
    let verified = builder.build_verified().unwrap();
    let program = verified.program();
    let encoded = program.encode();
    assert_eq!(Program::decode(&encoded).unwrap(), *program);
    encoded
}

fn bench_decoder(c: &mut Criterion) {
    let mut group = c.benchmark_group("program_decode");
    for (functions, additions) in [(1, 4096), (128, 32), (1024, 4)] {
        let encoded = encoded_program(functions, additions);
        group.throughput(Throughput::Bytes(encoded.len() as u64));
        group.bench_with_input(
            BenchmarkId::new("functions_x_additions", format!("{functions}x{additions}")),
            &encoded,
            |b, encoded| b.iter(|| Program::decode(black_box(encoded)).unwrap()),
        );
    }
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_millis(300))
        .measurement_time(Duration::from_millis(1200))
        .sample_size(40);
    targets = bench_decoder
}
criterion_main!(benches);
