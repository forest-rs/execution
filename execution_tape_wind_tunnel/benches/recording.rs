// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Complete live execution, capture/freeze/drop, and fresh replay costs.

#[path = "../src/recording.rs"]
mod workload;

use criterion::{Criterion, criterion_group, criterion_main};
use execution_tape::recording::{RecordLimits, Recorder, ReplayOutcome};
use execution_tape::trace::{TraceMask, TraceSink};
use execution_tape::value::FuncId;
use std::hint::black_box;
use std::sync::Arc;

fn recording(c: &mut Criterion) {
    for calls in [1, 1000] {
        let program = workload::program(calls);
        let (mut vm, args) = workload::fixture(256, 65_536);
        let expected = vm
            .run(&program, FuncId(0), &args, TraceMask::NONE, None)
            .unwrap();
        let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
        let actual = vm
            .run(
                &program,
                FuncId(0),
                &args,
                recorder.mask(),
                Some(&mut recorder),
            )
            .unwrap();
        assert_eq!(actual, expected);
        let record = recorder.finish();
        record.status().unwrap();
        let ReplayOutcome::Returned(result) = record.replay().unwrap() else {
            panic!("replay trapped")
        };
        assert_eq!(result.aggregates().len_u32(), 256);
        assert_eq!(record.stats().aggregates, 256);
        assert_eq!(record.stats().payload_bytes, 65_536);

        let mut group = c.benchmark_group(format!("recording/{calls}_calls"));
        group.bench_function("live", |b| {
            b.iter(|| {
                let result = vm
                    .run(&program, FuncId(0), black_box(&args), TraceMask::NONE, None)
                    .unwrap();
                black_box(result);
            })
        });
        group.bench_function("capture_freeze_drop", |b| {
            b.iter(|| {
                let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
                let result = vm
                    .run(
                        &program,
                        FuncId(0),
                        black_box(&args),
                        recorder.mask(),
                        Some(&mut recorder),
                    )
                    .unwrap();
                let record = recorder.finish();
                record.status().unwrap();
                black_box((result, record));
            })
        });
        group.bench_function("fresh_replay_drop", |b| {
            b.iter(|| {
                black_box(record.replay().unwrap());
            })
        });
        group.finish();
    }
}
criterion_group!(benches, recording);
criterion_main!(benches);
