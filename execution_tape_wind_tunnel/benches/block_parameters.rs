// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Compare cold preparation, frame/prepared bytes, and reused execution of equivalent transfers.
//! Preparation excludes source construction, cloning, and container decoding. Buffer byte counts
//! describe this build's element layouts rather than total retained memory or RSS.

#[path = "../src/block_parameters.rs"]
mod workload;
use criterion::{BatchSize, BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use execution_tape::trace::TraceMask;
use execution_tape::value::{FuncId, Value};
use execution_tape::verifier::{VerifyConfig, verify_program_owned};
use execution_tape::vm::{ExecutionContext, Limits, Vm};

fn benchmarks(c: &mut Criterion) {
    let config = VerifyConfig::default();
    for width in [4, 32, 256] {
        for typed in [false, true] {
            let mode = if typed { "arguments" } else { "moves" };
            let source = workload::rotation(width, 64, typed);
            let verified = verify_program_owned(source.clone(), &config).unwrap();
            let stats = verified.function(FuncId(0)).unwrap().stats();
            eprintln!(
                "{mode}/{width}: instructions={} operands={} register_bytes={} prepared_used_bytes={} prepared_capacity_bytes={}",
                stats.instruction_count,
                stats.operand_count,
                stats.register_bytes,
                stats.prepared_used_bytes,
                stats.prepared_capacity_bytes
            );
            let mut vm = Vm::new(workload::NoHost, Limits::default());
            let mut ctx = ExecutionContext::new();
            let mut outputs = Vec::new();
            vm.run_with_ctx(
                &mut ctx,
                &verified,
                FuncId(0),
                &[],
                &mut outputs,
                TraceMask::NONE,
                None,
                None,
            )
            .unwrap();
            let expected: Vec<_> = (0..width)
                .map(|index| Value::I64(i64::from((index + 63) % width + 1)))
                .collect();
            assert_eq!(
                outputs, expected,
                "workload variants must have equal results"
            );
            c.bench_with_input(
                BenchmarkId::new(format!("block_prepare/{mode}"), width),
                &source,
                |b, p| {
                    b.iter_batched(
                        || p.clone(),
                        |owned| verify_program_owned(black_box(owned), black_box(&config)).unwrap(),
                        BatchSize::SmallInput,
                    );
                },
            );
            c.bench_function(&format!("block_execute/{mode}/{width}"), |b| {
                b.iter(|| {
                    outputs.clear();
                    vm.run_with_ctx(
                        &mut ctx,
                        black_box(&verified),
                        FuncId(0),
                        &[],
                        &mut outputs,
                        TraceMask::NONE,
                        None,
                        None,
                    )
                    .unwrap();
                    black_box(&outputs);
                });
            });
        }
    }
}
criterion_group! {name=benches;config=Criterion::default().warm_up_time(std::time::Duration::from_millis(300)).measurement_time(std::time::Duration::from_millis(1200)).sample_size(40);targets=benchmarks}
criterion_main!(benches);
