// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Complete live execution, capture/freeze/drop, and fresh replay costs.

#[path = "../../examples/tape/src/support/recording.rs"]
mod workload;

use criterion::{Criterion, criterion_group, criterion_main};
use execution_tape::recording::{RecordLimits, Recorder, ReplayOutcome};
use execution_tape::trace::TraceSink;
use execution_tape::value::FuncId;
use std::hint::black_box;
use std::sync::Arc;

fn recording(c: &mut Criterion) {
    for calls in [1, 1000] {
        let program = workload::program(calls);
        let (mut vm, args) = workload::fixture(256, 65_536);
        let expected = vm
            .run(
                execution_tape::callable::Callable::function(
                    std::sync::Arc::new(execution_tape::verifier::VerifiedProgram::clone(&program)),
                    FuncId(0),
                )
                .expect("verified entry"),
                &args,
                execution_tape::vm::TurnOptions::default(),
            )
            .unwrap();
        let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
        let actual = vm
            .run(
                execution_tape::callable::Callable::function(
                    std::sync::Arc::new(execution_tape::verifier::VerifiedProgram::clone(&program)),
                    FuncId(0),
                )
                .expect("verified entry"),
                &args,
                execution_tape::vm::TurnOptions::default().trace(recorder.mask(), &mut recorder),
            )
            .unwrap();
        assert_eq!(
            vm.resolve(&actual.into_returned().unwrap())
                .unwrap()
                .values(),
            vm.resolve(&expected.into_returned().unwrap())
                .unwrap()
                .values()
        );
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
                    .run(
                        execution_tape::callable::Callable::function(
                            std::sync::Arc::new(execution_tape::verifier::VerifiedProgram::clone(
                                &program,
                            )),
                            FuncId(0),
                        )
                        .expect("verified entry"),
                        black_box(&args),
                        execution_tape::vm::TurnOptions::default(),
                    )
                    .unwrap();
                black_box(result);
            })
        });
        group.bench_function("capture_freeze_drop", |b| {
            b.iter(|| {
                let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
                let result = vm
                    .run(
                        execution_tape::callable::Callable::function(
                            std::sync::Arc::new(execution_tape::verifier::VerifiedProgram::clone(
                                &program,
                            )),
                            FuncId(0),
                        )
                        .expect("verified entry"),
                        black_box(&args),
                        execution_tape::vm::TurnOptions::default()
                            .trace(recorder.mask(), &mut recorder),
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
