// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Construction-only and construction-plus-verification, both including destruction.
//! Neither window executes the guest; the verifier benchmark isolates preparation itself.

use std::{hint::black_box, time::Duration};

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use execution_tape::verifier::{VerifyConfig, verify_program_owned};

#[path = "../src/authoring.rs"]
mod authoring;

fn authoring(c: &mut Criterion) {
    let mut group = c.benchmark_group("authoring");
    for (name, build, sizes) in [
        (
            "streaming",
            authoring::streaming as fn(u32) -> _,
            [32, 256, 1024],
        ),
        ("lowered", authoring::lowered as fn(u32) -> _, [4, 32, 128]),
    ] {
        for size in sizes {
            // Establish validity before measuring; construction includes destruction.
            verify_program_owned(build(size), &VerifyConfig::default()).unwrap();
            group.bench_function(BenchmarkId::new(name, size), |b| {
                b.iter(|| black_box(build(black_box(size))));
            });
            group.bench_function(BenchmarkId::new(format!("{name}_verified"), size), |b| {
                b.iter(|| {
                    verify_program_owned(build(black_box(size)), &VerifyConfig::default()).unwrap()
                });
            });
        }
    }
    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_millis(300))
        .measurement_time(Duration::from_millis(1200))
        .sample_size(40);
    targets = authoring
}
criterion_main!(benches);
