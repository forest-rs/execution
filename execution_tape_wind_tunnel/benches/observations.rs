// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Compare ordinary execution, bounded instruction history and explicit register watches.

#[path = "support/programs.rs"]
mod programs;
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use execution_tape::recording::{
    InstructionCapture, InstructionStatus, RecordLimits, Recorder, RegisterWatch,
};
use execution_tape::{
    disasm::disassemble_function,
    host::{Host, HostContext, HostError, SigHash, ValueRef},
    trace::{TraceLocation, TraceMask, TraceSink},
    value::{FuncId, Value},
    verifier::{VerifyConfig, verify_program_owned},
    vm::{Limits, Vm},
};
use std::{hint::black_box, sync::Arc, time::Duration};
struct NoHost;
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
fn observations(c: &mut Criterion) {
    let cases = [
        ("straight256", programs::straight_line(256)),
        ("diamonds16x64", programs::diamonds(16, 64)),
        ("tuples64", programs::tuples(64)),
    ];
    let mut group = c.benchmark_group("optional_observations");
    for (name, program) in cases {
        let program = Arc::new(verify_program_owned(program, &VerifyConfig::default()).unwrap());
        let mut vm = Vm::new(NoHost, Limits::default());
        let expected = vm
            .run(&program, FuncId(0), &[], TraceMask::NONE, None)
            .unwrap();
        let source = disassemble_function(program.program(), FuncId(0)).unwrap();
        let watches = source
            .instrs()
            .map(|i| RegisterWatch {
                location: TraceLocation {
                    func: source.func(),
                    pc: i.pc(),
                },
                registers: i.reads().chain(i.writes()).collect(),
            })
            .collect::<Vec<_>>();
        for mode in ["boundaries", "history", "watched"] {
            let config = (mode != "boundaries").then(|| InstructionCapture {
                max_steps: 5000,
                watches: if mode == "watched" {
                    watches.clone()
                } else {
                    vec![]
                },
            });
            let mut capture = || {
                let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
                if let Some(config) = &config {
                    recorder = recorder.with_instructions(config.clone());
                }
                let result = vm
                    .run(
                        &program,
                        FuncId(0),
                        &[],
                        recorder.mask(),
                        Some(&mut recorder),
                    )
                    .unwrap();
                (result, recorder.finish())
            };
            let (result, record) = capture();
            assert_eq!(result, expected);
            record.status().unwrap();
            assert_eq!(
                record.instruction_status(),
                if mode == "boundaries" {
                    None
                } else {
                    Some(InstructionStatus::Complete)
                }
            );
            record.replay().unwrap();
            group.bench_function(BenchmarkId::new(mode, name), |b| {
                b.iter(|| black_box(capture())); // Includes config cloning, capture/freeze and drop.
            });
        }
        group.bench_function(BenchmarkId::new("live", name), |b| {
            b.iter(|| {
                black_box(
                    vm.run(&program, FuncId(0), &[], TraceMask::NONE, None)
                        .unwrap(),
                )
            });
        });
    }
    group.finish();
}
criterion_group! {
    name=benches;
    config=Criterion::default().warm_up_time(Duration::from_millis(300)).measurement_time(Duration::from_millis(1200)).sample_size(40);
    targets=observations
}
criterion_main!(benches);
