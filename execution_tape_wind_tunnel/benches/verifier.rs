// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Verification latency with program construction outside the timed region.
//!
//! Run with `cargo bench -p execution_tape_wind_tunnel --bench verifier`.
//! Straight-line functions vary instruction/register count; diamonds vary CFG size and live state.
//! Tuple-heavy functions exercise shape creation, moves, and projections.

use std::time::Duration;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use execution_tape::verifier::{VerifyConfig, verify_program};
use std::hint::black_box;

#[path = "support/programs.rs"]
mod programs;
use programs::{diamonds, straight_line, tuples};

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
