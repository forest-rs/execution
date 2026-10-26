// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Report prepared frames, constant arenas, and used recording value storage on this build.

use execution_tape::{
    asm::{Asm, FunctionSig, ProgramBuilder},
    host::{Host, HostContext, HostError, SigHash, ValueRef},
    program::{Const, VERSION_MAJOR, VERSION_MINOR, ValueType},
    recording::{RecordLimits, RecordedValue, Recorder},
    trace::TraceSink,
    value::{Decimal, Value},
    vm::{Limits, Vm},
};

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

fn main() {
    println!(
        "wire={VERSION_MAJOR}.{VERSION_MINOR} decimal_bytes={}",
        size_of::<Decimal>()
    );
    for kind in [ValueType::I64, ValueType::F64, ValueType::Decimal] {
        let mut builder = ProgramBuilder::new();
        let mut body = Asm::new();
        body.set_register_types(vec![ValueType::Unit, kind, kind]);
        match kind {
            ValueType::I64 => {
                body.const_i64(2, 1);
            }
            ValueType::F64 => {
                body.const_f64(2, 0.01);
            }
            ValueType::Decimal => {
                body.const_decimal(2, 1, 2);
            }
            _ => unreachable!(),
        }
        for _ in 0..1024 {
            match kind {
                ValueType::I64 => {
                    body.i64_add(1, 1, 2);
                }
                ValueType::F64 => {
                    body.f64_add(1, 1, 2);
                }
                ValueType::Decimal => {
                    body.dec_add(1, 1, 2);
                }
                _ => unreachable!(),
            }
        }
        body.ret(0, &[1]);
        let id = builder
            .push_function_checked(
                body,
                FunctionSig {
                    arg_types: vec![kind],
                    ret_types: vec![kind],
                },
            )
            .unwrap();
        let program = builder.build_verified().unwrap();
        let stats = program.function(id).unwrap().stats();
        println!(
            "{kind:?}: source={} instructions={} prepared_used={} prepared_capacity={} frame_elements={}",
            stats.bytecode_bytes,
            stats.instruction_count,
            stats.prepared_used_bytes,
            stats.prepared_capacity_bytes,
            stats.register_bytes
        );
    }
    for decimal in [false, true] {
        let mut builder = ProgramBuilder::new();
        for index in 0..1024 {
            builder.constant(if decimal {
                Const::Decimal(Decimal::new(i128::from(index), 2))
            } else {
                Const::I64(index)
            });
        }
        let program = builder.build();
        println!(
            "constants/{}: entries_used={} entries_capacity={} blob_used={} blob_capacity={}",
            if decimal { "Decimal" } else { "I64" },
            program.const_pool.len() * size_of::<execution_tape::program::ConstEntry>(),
            program.const_pool.capacity() * size_of::<execution_tape::program::ConstEntry>(),
            program.const_bytes_data.len(),
            program.const_bytes_data.capacity()
        );
    }
    for kind in [ValueType::I64, ValueType::Decimal] {
        let mut body = Asm::new();
        body.set_register_types(vec![ValueType::Unit, kind]);
        if kind == ValueType::I64 {
            body.const_i64(1, 12345);
        } else {
            body.const_decimal(1, 12345, 2);
        }
        body.ret(0, &vec![1; 1024]);
        let mut builder = ProgramBuilder::new();
        let id = builder
            .push_function_checked(
                body,
                FunctionSig {
                    arg_types: vec![],
                    ret_types: vec![kind; 1024],
                },
            )
            .unwrap();
        let program = std::sync::Arc::new(builder.build_verified().unwrap());
        let mut recorder = Recorder::new(std::sync::Arc::clone(&program), RecordLimits::default());
        Vm::new(NoHost, Limits::default())
            .run(&program, id, &[], recorder.mask(), Some(&mut recorder))
            .unwrap();
        let recording = recorder.finish();
        recording.status().unwrap();
        let stats = recording.stats();
        println!(
            "recording/{kind:?}: values={} used_value_bytes={} (excludes capacities/metadata/shared program)",
            stats.values,
            stats.values * size_of::<RecordedValue>()
        );
        let execution_tape::recording::ReplayOutcome::Returned(snapshot) =
            recording.replay().unwrap()
        else {
            panic!("expected return");
        };
        assert_eq!(snapshot.values().len(), 1024);
    }
}
