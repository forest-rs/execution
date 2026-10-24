// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Opt-in inspection costs, separate from ordinary verifier/VM benchmarks.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use execution_tape::disasm::disassemble_function;
use execution_tape::program::Program;
use execution_tape::value::FuncId;
use execution_tape::verifier::{ProgramAnalysis, ValueFact, VerifyConfig};
use std::{hint::black_box, time::Duration};

#[path = "support/programs.rs"]
mod programs;

fn inspect(c: &mut Criterion) {
    let cases: [(&str, Program); 3] = [
        ("straight256", programs::straight_line(256)),
        ("diamonds16x64", programs::diamonds(16, 64)),
        ("tuples64", programs::tuples(64)),
    ];
    let mut group = c.benchmark_group("optional_inspection");
    for (name, program) in cases {
        // The borrowed proof is constructed once. Normal verification is measured by `verifier`.
        let proof = ProgramAnalysis::new(&program, &VerifyConfig::default()).unwrap();
        let pcs: Vec<_> = disassemble_function(&program, FuncId(0))
            .unwrap()
            .instrs()
            .map(|i| i.pc())
            .collect();
        let last = *pcs.last().unwrap();
        group.bench_with_input(
            BenchmarkId::new("function_analysis", name),
            &proof,
            |b, proof| {
                b.iter(|| black_box(proof.function(FuncId(0)).unwrap())); // Includes artifact drop.
            },
        );
        let mut analysis = proof.function(FuncId(0)).unwrap();
        assert!(matches!(
            analysis
                .instruction(last)
                .unwrap()
                .register(2)
                .unwrap()
                .before,
            ValueFact::Contract(_)
        ));
        for &pc in &pcs {
            assert!(analysis.instruction(pc).unwrap().reachable());
        }
        group.bench_function(BenchmarkId::new("same_pc_query", name), |b| {
            b.iter(|| {
                black_box(
                    analysis
                        .instruction(black_box(last))
                        .unwrap()
                        .register(black_box(2))
                        .unwrap(),
                )
            });
        });
        group.bench_function(BenchmarkId::new("forward_scan", name), |b| {
            b.iter(|| {
                for &pc in &pcs {
                    let view = analysis.instruction(pc).unwrap();
                    black_box(view.register(2));
                }
            });
        });
    }
    group.finish();
}
criterion_group! {
    name=benches;
    config=Criterion::default().warm_up_time(Duration::from_millis(300)).measurement_time(Duration::from_millis(1200)).sample_size(40);
    targets=inspect
}
criterion_main!(benches);
