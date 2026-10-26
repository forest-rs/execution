// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use super::*;
use crate::asm::{FunctionSig, ProgramBuilder};
use crate::host::{HostError, SigHash, ValueRef};
use crate::program::StructTypeDef;
use alloc::string::ToString;
use alloc::sync::Arc;
use alloc::vec;

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

fn construction(kind: &str, width: usize, count: usize) -> VerifiedProgram {
    let mut builder = ProgramBuilder::new();
    let st = builder.struct_type(StructTypeDef {
        field_names: (0..width).map(|i| alloc::format!("f{i}")).collect(),
        field_types: vec![ValueType::I64; width],
    });
    let elem = builder.array_elem(ValueType::I64);
    let entry = builder
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::Agg],
        })
        .unwrap();
    let mut body = builder.function_asm(entry).unwrap();
    let aggregate = body.alloc_register(ValueType::Agg);
    let fields = vec![1; width];
    for _ in 0..count {
        match kind {
            "tuple" => body.tuple_new(aggregate, &fields),
            "struct" => body.struct_new(aggregate, st, &fields),
            "array" => body.array_new(aggregate, elem, &fields),
            _ => unreachable!(),
        };
    }
    body.ret(0, &[aggregate]);
    builder.define_function(entry, body).unwrap();
    builder.build_verified().unwrap()
}

#[test]
fn constructors_charge_nodes_and_fields_before_allocation_and_reset_each_run() {
    for kind in ["tuple", "struct", "array"] {
        for width in [0, 1, 1024] {
            let program = construction(kind, width, 2);
            let cost = 1 + width as u64;
            let mut vm = Vm::new(
                NoHost,
                Limits {
                    max_aggregate_work: 2 * cost - 1,
                    max_payload_work: 0,
                    ..Limits::default()
                },
            );
            let retained = vm.aggregates_mut().tuple_new(vec![Value::I64(99)]).unwrap();
            let mut ctx = ExecutionContext::new();
            let mut outputs = vec![Value::Bool(true)];
            let error = vm
                .run_with_ctx(
                    &mut ctx,
                    &program,
                    FuncId(0),
                    &[Value::I64(7)],
                    &mut outputs,
                    TraceMask::NONE,
                    None,
                    None,
                )
                .unwrap_err();
            assert!(
                matches!(error.trap, Trap::AggregateWorkExceeded { limit, used, requested }
                if limit == 2 * cost - 1 && used == cost && requested == cost)
            );
            assert_eq!(ctx.aggregate_work, cost);
            assert_eq!(outputs, [Value::Bool(true)]);
            assert_eq!(vm.aggregates().len_u32(), 1);
            assert_eq!(
                vm.aggregates().tuple_get(retained, 0).unwrap(),
                Value::I64(99)
            );
            assert!(error.to_string().contains("Limits::max_aggregate_work"));

            vm.limits.max_aggregate_work = 2 * cost;
            for _ in 0..2 {
                outputs.clear();
                vm.run_with_ctx(
                    &mut ctx,
                    &program,
                    FuncId(0),
                    &[Value::I64(7)],
                    &mut outputs,
                    TraceMask::NONE,
                    None,
                    None,
                )
                .unwrap();
                assert_eq!(ctx.aggregate_work, 2 * cost);
                assert_eq!(ctx.payload_work, 0);
            }
            ctx.clear();
            assert_eq!((ctx.aggregate_work, ctx.aggregate_limit), (0, 0));
        }
    }
}

#[test]
fn aggregate_budget_is_shared_by_guest_calls_and_checked_replay() {
    use crate::recording::{RecordLimits, Recorder, ReplayOutcome};
    use crate::trace::TraceSink;

    let mut builder = ProgramBuilder::new();
    let sig = FunctionSig {
        arg_types: vec![],
        ret_types: vec![ValueType::Agg],
    };
    let entry = builder.declare_function(sig.clone()).unwrap();
    let helper = builder.declare_function(sig).unwrap();
    let mut body = builder.function_asm(entry).unwrap();
    let dst = body.alloc_register(ValueType::Agg);
    body.call(0, helper, 0, &[], &[dst]);
    body.call(0, helper, 0, &[], &[dst]);
    body.ret(0, &[dst]);
    builder.define_function(entry, body).unwrap();
    let mut body = builder.function_asm(helper).unwrap();
    let dst = body.alloc_register(ValueType::Agg);
    body.tuple_new(dst, &[]);
    body.ret(0, &[dst]);
    builder.define_function(helper, body).unwrap();
    let program = Arc::new(builder.build_verified().unwrap());
    for limit in [0, 1, 2] {
        let mut vm = Vm::new(
            NoHost,
            Limits {
                max_aggregate_work: limit,
                ..Limits::default()
            },
        );
        let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
        let result = vm.run(&program, entry, &[], recorder.mask(), Some(&mut recorder));
        let recording = recorder.finish();
        drop(vm);
        if limit == 2 {
            result.unwrap();
            assert!(matches!(recording.replay(), Ok(ReplayOutcome::Returned(_))));
        } else {
            let error = result.unwrap_err();
            assert_eq!(error.func, helper);
            assert!(
                matches!(error.trap, Trap::AggregateWorkExceeded { limit: actual, used, requested: 1 }
                if actual == limit && used == limit)
            );
            assert!(matches!(recording.replay(), Ok(ReplayOutcome::Trapped(_))));
        }
    }
}

#[test]
fn aggregate_work_arithmetic_rejects_overflow_without_charging() {
    let mut ctx = ExecutionContext {
        aggregate_limit: u64::MAX,
        ..ExecutionContext::default()
    };
    // Exercise the accounting boundary without allocating an enormous guest value.
    if usize::BITS == 64 {
        assert!(matches!(
            ctx.charge_aggregate(usize::MAX),
            Err(Trap::AggregateSizeOverflow)
        ));
        assert_eq!(ctx.aggregate_work, 0);
    }
    ctx.aggregate_work = u64::MAX - 1;
    ctx.charge_aggregate(0).unwrap();
    assert!(matches!(
        ctx.charge_aggregate(0),
        Err(Trap::AggregateWorkExceeded {
            limit: u64::MAX,
            used: u64::MAX,
            requested: 1
        })
    ));
    assert_eq!(ctx.aggregate_work, u64::MAX);
}

#[test]
fn borrowed_nested_aggregates_charge_only_new_immediate_fields() {
    let mut builder = ProgramBuilder::new();
    let entry = builder
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::Agg],
            ret_types: vec![ValueType::Agg],
        })
        .unwrap();
    let mut body = builder.function_asm(entry).unwrap();
    let dst = body.alloc_register(ValueType::Agg);
    body.tuple_new(dst, &[1, 1]);
    body.ret(0, &[dst]);
    builder.define_function(entry, body).unwrap();
    let program = builder.build_verified().unwrap();
    let mut vm = Vm::new(
        NoHost,
        Limits {
            max_aggregate_work: 3,
            max_payload_work: 0,
            ..Limits::default()
        },
    );
    let input = vm
        .aggregates_mut()
        .tuple_new(vec![Value::I64(7); 4096])
        .unwrap();
    let output = vm
        .run(&program, entry, &[Value::Agg(input)], TraceMask::NONE, None)
        .unwrap();
    let Value::Agg(root) = output[0] else {
        panic!("aggregate result")
    };
    assert_eq!(
        vm.aggregates().tuple_get(root, 0).unwrap(),
        Value::Agg(input)
    );
    assert_eq!(
        vm.aggregates().tuple_get(root, 1).unwrap(),
        Value::Agg(input)
    );
}
