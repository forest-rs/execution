// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Unit register width, guest/host transfers and simultaneous joins.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use execution_tape::{
    asm::{FunctionSig, ProgramBuilder},
    host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef},
    program::ValueType,
    trace::TraceMask,
    value::{FuncId, Value},
    verifier::VerifiedProgram,
    vm::{ExecutionContext, Limits, Vm},
};

struct UnitHost;
impl Host for UnitHost {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        args: &[ValueRef<'_>],
        rets: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        assert!(
            matches!(args, [ValueRef::Unit, ValueRef::Unit]),
            "host arguments must preserve Unit kinds"
        );
        rets.fill(Value::Unit);
        Ok(0)
    }
}

fn program(units: usize) -> (VerifiedProgram, FuncId) {
    let mut builder = ProgramBuilder::new();
    let entry = builder
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::Unit, ValueType::Unit, ValueType::Bool],
            ret_types: vec![ValueType::Unit; 2],
        })
        .unwrap();
    let helper = builder
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::Unit; 2],
            ret_types: vec![ValueType::Unit; 2],
        })
        .unwrap();
    let host = builder.host_sig_for(
        "units",
        HostSig {
            args: vec![ValueType::Unit; 2],
            rets: vec![ValueType::Unit; 2],
        },
    );
    let mut body = builder.function_asm(helper).unwrap();
    body.ret(0, &[2, 1]);
    builder.define_function(helper, body).unwrap();
    let mut body = builder.function_asm(entry).unwrap();
    for _ in 3..units {
        body.alloc_register(ValueType::Unit);
    }
    for _ in 0..16 {
        body.mov(1, 2).select(2, 3, 1, 2);
        let join = body.block(&[1, 2]);
        body.jmp(join.with_args(&[2, 1]));
        body.place(join).unwrap();
        body.call(0, helper, 0, &[1, 2], &[1, 2]);
        body.host_call_sig_id(0, host, 0, &[1, 2], &[1, 2]);
    }
    body.ret(0, &[1, 2]);
    builder.define_function(entry, body).unwrap();
    (builder.build_verified().unwrap(), entry)
}

fn units(c: &mut Criterion) {
    let mut group = c.benchmark_group("unit_registers");
    for width in [8, 65535] {
        let (program, entry) = program(width);
        let mut vm = Vm::new(UnitHost, Limits::default());
        let args = [Value::Unit, Value::Unit, Value::Bool(true)];
        let mut context = ExecutionContext::new();
        let mut output = Vec::new();
        vm.run_with_ctx(
            &mut context,
            &program,
            entry,
            &args,
            &mut output,
            TraceMask::NONE,
            None,
            None,
        )
        .unwrap();
        assert_eq!(output, [Value::Unit, Value::Unit]);
        println!(
            "width={width} frame_bytes={} context_bytes={}",
            program.function(entry).unwrap().stats().register_bytes,
            size_of::<ExecutionContext>()
        );
        group.bench_function(BenchmarkId::new("fresh_vm", width), |b| {
            b.iter(|| {
                let mut vm = Vm::new(UnitHost, Limits::default());
                black_box(
                    vm.run(
                        black_box(&program),
                        entry,
                        black_box(&args),
                        TraceMask::NONE,
                        None,
                    )
                    .unwrap(),
                );
            })
        });
        group.bench_function(BenchmarkId::new("reused_context", width), |b| {
            b.iter(|| {
                output.clear();
                vm.run_with_ctx(
                    &mut context,
                    black_box(&program),
                    entry,
                    black_box(&args),
                    &mut output,
                    TraceMask::NONE,
                    None,
                    None,
                )
                .unwrap();
                black_box(&output);
            })
        });
    }
    group.finish();
}

criterion_group!(benches, units);
criterion_main!(benches);
