// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Real recording -> drop original VM/host -> fresh interpreter replay through public APIs.

use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
use execution_tape::host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef};
use execution_tape::program::{ElemTypeId, HostTypeId, TypeId, ValueType};
use execution_tape::recording::{
    CaptureFault, CaptureResource, HostOutcome, Outcome, RecordLimits, RecordedValue, Recorder,
    Recording, ReplayError, ReplayOutcome,
};
use execution_tape::trace::{TraceMask, TraceSink};
use execution_tape::value::{AggHandle, AggType, Closure, Decimal, FuncId, Obj, ObjHandle, Value};
use execution_tape::verifier::VerifiedProgram;
use execution_tape::vm::{Limits, Trap, Vm};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn program(
    args: &[ValueType],
    rets: &[ValueType],
    kinds: Vec<ValueType>,
    emit: impl FnOnce(&mut Asm, &mut ProgramBuilder),
) -> Arc<VerifiedProgram> {
    let mut builder = ProgramBuilder::new();
    let mut body = Asm::new();
    body.set_register_types(kinds);
    emit(&mut body, &mut builder);
    builder
        .push_function_checked(
            body,
            FunctionSig {
                arg_types: args.into(),
                ret_types: rets.into(),
            },
        )
        .unwrap();
    Arc::new(builder.build_verified().unwrap())
}

fn identity(kind: ValueType) -> Arc<VerifiedProgram> {
    program(&[kind], &[kind], vec![ValueType::Unit, kind], |body, _| {
        body.ret(0, &[1]);
    })
}

fn host_program(kind: ValueType) -> Arc<VerifiedProgram> {
    program(
        &[],
        &[kind],
        vec![ValueType::Unit, kind],
        |body, builder| {
            let import = builder.host_sig_for(
                "observe",
                HostSig {
                    args: vec![],
                    rets: vec![kind],
                },
            );
            body.host_call_sig_id(0, import, 0, &[], &[1]);
            body.ret(0, &[1]);
        },
    )
}

struct TestHost {
    value: Value,
    fuel: u64,
    error: Option<HostError>,
    calls: Arc<AtomicUsize>,
}
impl TestHost {
    fn returning(value: Value) -> Self {
        Self {
            value,
            fuel: 0,
            error: None,
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }
}
impl Host for TestHost {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        args: &[ValueRef<'_>],
        rets: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        rets[0] = args
            .first()
            .map_or_else(|| self.value.clone(), |arg| arg.to_value());
        self.error.clone().map_or(Ok(self.fuel), Err)
    }
}

fn capture(vm: &mut Vm<TestHost>, program: &Arc<VerifiedProgram>, args: &[Value]) -> Recording {
    let mut recorder = Recorder::new(Arc::clone(program), RecordLimits::default());
    let _ = vm.run(
        program,
        FuncId(0),
        args,
        recorder.mask(),
        Some(&mut recorder),
    );
    let recording = recorder.finish();
    recording.status().unwrap();
    recording
}

fn returned(record: &Recording) -> execution_tape::aggregates::ValueSnapshot {
    let ReplayOutcome::Returned(snapshot) = record.replay().unwrap() else {
        panic!("expected replay return");
    };
    snapshot
}

#[test]
fn final_outcome_locations_resolve_returns_traps_and_absent_completion() {
    let p = program(
        &[],
        &[ValueType::I64],
        vec![ValueType::Unit, ValueType::I64],
        |body, _| {
            body.const_i64(1, 7).ret(0, &[1]);
        },
    );
    let mut vm = Vm::new(TestHost::returning(Value::Unit), Limits::default());
    let record = capture(&mut vm, &p, &[]);
    let expected = p
        .function(FuncId(0))
        .unwrap()
        .instructions()
        .last()
        .unwrap()
        .pc;
    assert_eq!(
        record.outcome_location(),
        Some(execution_tape::trace::TraceLocation {
            func: FuncId(0),
            pc: expected
        })
    );
    assert!(matches!(record.outcome(), Some(Outcome::Returned(_))));
    let unfinished = Recorder::new(Arc::clone(&p), RecordLimits::default()).finish();
    assert!(unfinished.outcome_location().is_none());

    let p = program(
        &[],
        &[],
        vec![ValueType::Unit, ValueType::I64],
        |body, _| {
            body.const_i64(1, 7).trap(42);
        },
    );
    let record = capture(&mut vm, &p, &[]);
    let Some(Outcome::Trapped(trap)) = record.outcome() else {
        panic!("expected guest trap")
    };
    assert_eq!(
        record.outcome_location(),
        Some(execution_tape::trace::TraceLocation {
            func: trap.func,
            pc: trap.pc
        })
    );
    assert_ne!(
        trap.pc, 0,
        "trap provenance is the executed trap instruction"
    );
}

#[test]
fn deep_graphs_are_walked_iteratively_and_repeated_host_boundaries_keep_one_copy() {
    const DEPTH: usize = 10_000;
    const CALLS: usize = 128;
    let p = program(
        &[ValueType::Agg],
        &[ValueType::Agg],
        vec![ValueType::Unit, ValueType::Agg, ValueType::Agg],
        |body, builder| {
            let import = builder.host_sig_for(
                "echo",
                HostSig {
                    args: vec![ValueType::Agg],
                    rets: vec![ValueType::Agg],
                },
            );
            for _ in 0..CALLS {
                body.host_call_sig_id(0, import, 0, &[1], &[2]);
            }
            body.ret(0, &[2]);
        },
    );
    let mut vm = Vm::new(TestHost::returning(Value::Unit), Limits::default());
    let mut root = vm
        .aggregates_mut()
        .tuple_new(vec![Value::Bytes(vec![42; 1024])])
        .unwrap();
    for _ in 1..DEPTH {
        root = vm
            .aggregates_mut()
            .tuple_new(vec![Value::Agg(root)])
            .unwrap();
    }
    let record = capture(&mut vm, &p, &[Value::Agg(root)]);
    assert_eq!(
        record.stats().aggregates,
        DEPTH,
        "each immutable node is retained once per invocation"
    );
    assert_eq!(record.stats().payload_bytes, 1024);
    assert_eq!(record.stats().values, DEPTH + 2 * CALLS + 2);
    drop(vm);
    let result = returned(&record);
    assert_eq!(result.aggregates().len_u32() as usize, DEPTH);
    drop(result);
    drop(record);
}

#[test]
#[cfg_attr(panic = "abort", ignore = "requires an unwinding panic runtime")]
fn unwinding_host_leaves_an_explicitly_incomplete_recording() {
    struct Panics;
    impl Host for Panics {
        fn call(
            &mut self,
            _: &str,
            _: SigHash,
            _: &[ValueRef<'_>],
            _: &mut [Value],
            _: HostContext<'_, '_>,
        ) -> Result<u64, HostError> {
            panic!("host unwound");
        }
    }
    let p = host_program(ValueType::I64);
    let mut vm = Vm::new(Panics, Limits::default());
    let mut recorder = Recorder::new(Arc::clone(&p), RecordLimits::default());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        vm.run(&p, FuncId(0), &[], recorder.mask(), Some(&mut recorder))
    }));
    assert!(result.is_err());
    let record = recorder.finish();
    assert_eq!(record.status(), Err(&CaptureFault::MissingCompletion));
    assert!(record.invocation().is_some());
    assert!(record.outcome().is_none());
    assert!(matches!(
        record.replay(),
        Err(ReplayError::Incomplete(CaptureFault::MissingCompletion))
    ));
}

#[test]
fn actual_guest_arithmetic_and_host_values_replay_after_live_host_is_dropped() {
    let p = program(
        &[ValueType::Bool],
        &[ValueType::I64],
        vec![
            ValueType::Unit,
            ValueType::Bool,
            ValueType::I64,
            ValueType::I64,
            ValueType::I64,
            ValueType::I64,
        ],
        |body, builder| {
            let host = builder.host_sig_for(
                "observe",
                HostSig {
                    args: vec![],
                    rets: vec![ValueType::I64],
                },
            );
            let yes = body.label();
            let no = body.label();
            let join = body.block(&[5]);
            body.br(1, yes, no);
            body.place(yes).unwrap();
            body.host_call_sig_id(0, host, 0, &[], &[2]);
            body.const_i64(3, 2);
            body.i64_mul(4, 2, 3);
            body.jmp(join.with_args(&[4]));
            body.place(no).unwrap();
            body.const_i64(4, 7);
            body.jmp(join.with_args(&[4]));
            body.place(join).unwrap();
            body.ret(0, &[5]);
        },
    );
    for (condition, expected, expected_calls) in [(true, 42, 1), (false, 7, 0)] {
        let host = TestHost::returning(Value::I64(21));
        let effects = Arc::clone(&host.calls);
        let mut vm = Vm::new(host, Limits::default());
        let record = capture(&mut vm, &p, &[Value::Bool(condition)]);
        drop(vm);
        assert_eq!(returned(&record).values(), [Value::I64(expected)]);
        assert_eq!(record.host_calls().len(), expected_calls);
        assert_eq!(
            effects.load(Ordering::Relaxed),
            expected_calls,
            "replay invokes no live host"
        );
    }
}

#[test]
fn initial_graphs_absent_from_entry_inputs_restore_without_unrelated_heap_nodes() {
    let p = host_program(ValueType::Agg);
    let mut vm = Vm::new(TestHost::returning(Value::Unit), Limits::default());
    for i in 0..100 {
        vm.aggregates_mut().tuple_new(vec![Value::I64(i)]).unwrap();
    }
    let child = vm
        .aggregates_mut()
        .tuple_new(vec![Value::Str("retained".into())])
        .unwrap();
    let root = vm
        .aggregates_mut()
        .tuple_new(vec![Value::Agg(child), Value::Agg(child)])
        .unwrap();
    vm.host_mut().value = Value::Agg(root);
    let record = capture(&mut vm, &p, &[]);
    drop(vm);
    assert_eq!(record.stats().aggregates, 2);
    let HostOutcome::Returned { values, .. } = record.host_calls().next().unwrap().outcome else {
        panic!("host return");
    };
    let RecordedValue::Agg(id) = values[0] else {
        panic!("recorded root");
    };
    assert!(record.aggregate(id).unwrap().initial);
    let output = returned(&record);
    assert_eq!(output.aggregates().len_u32(), 2);
    let Value::Agg(root) = output.values()[0] else {
        panic!("root");
    };
    assert_eq!(
        output.aggregates().tuple_get(root, 0).unwrap(),
        output.aggregates().tuple_get(root, 1).unwrap()
    );
}

#[test]
fn guest_created_host_return_uses_allocation_ordinal_without_preloading_guest_nodes() {
    let p = program(
        &[],
        &[ValueType::Agg],
        vec![
            ValueType::Unit,
            ValueType::I64,
            ValueType::Agg,
            ValueType::Agg,
            ValueType::Agg,
        ],
        |body, builder| {
            let host = builder.host_sig_for(
                "observe",
                HostSig {
                    args: vec![],
                    rets: vec![ValueType::Agg],
                },
            );
            body.const_i64(1, 42);
            body.tuple_new(2, &[1]);
            body.tuple_new(3, &[2, 2]);
            body.host_call_sig_id(0, host, 0, &[], &[4]);
            body.ret(0, &[4]);
        },
    );
    let mut vm = Vm::new(
        TestHost::returning(Value::Agg(AggHandle(101))),
        Limits::default(),
    );
    for i in 0..100 {
        vm.aggregates_mut().tuple_new(vec![Value::I64(i)]).unwrap();
    }
    let record = capture(&mut vm, &p, &[]);
    drop(vm);
    assert_eq!(record.stats().aggregates, 2);
    let output = returned(&record);
    let Value::Agg(root) = output.values()[0] else {
        panic!("root");
    };
    let Value::Agg(child) = output.aggregates().tuple_get(root, 0).unwrap() else {
        panic!("child");
    };
    assert_eq!(
        output.aggregates().tuple_get(child, 0).unwrap(),
        Value::I64(42)
    );
    assert_eq!(
        output.aggregates().tuple_get(root, 1).unwrap(),
        Value::Agg(child)
    );
}

#[test]
fn shared_nodes_distinct_equal_nodes_and_interned_payloads_keep_their_meaning() {
    let p = identity(ValueType::Agg);
    let mut vm = Vm::new(TestHost::returning(Value::Unit), Limits::default());
    let a = vm
        .aggregates_mut()
        .tuple_new(vec![Value::Str("same".into())])
        .unwrap();
    let b = vm
        .aggregates_mut()
        .tuple_new(vec![Value::Str("same".into())])
        .unwrap();
    let root = vm
        .aggregates_mut()
        .tuple_new(vec![Value::Agg(a), Value::Agg(a), Value::Agg(b)])
        .unwrap();
    let record = capture(&mut vm, &p, &[Value::Agg(root)]);
    drop(vm);
    let RecordedValue::Agg(id) = record.invocation().unwrap().args[0] else {
        panic!("root");
    };
    let fields = record.aggregate(id).unwrap().values;
    let [
        RecordedValue::Agg(a),
        RecordedValue::Agg(shared),
        RecordedValue::Agg(b),
    ] = fields
    else {
        panic!("fields");
    };
    assert_eq!(a, shared);
    assert_ne!(a, b);
    let RecordedValue::Str(a_text) = &record.aggregate(*a).unwrap().values[0] else {
        panic!("text");
    };
    let RecordedValue::Str(b_text) = &record.aggregate(*b).unwrap().values[0] else {
        panic!("text");
    };
    assert!(Arc::ptr_eq(a_text, b_text));
    assert_eq!(
        Arc::strong_count(a_text),
        2,
        "construction indexes were discarded"
    );
    assert_eq!(record.stats().payload_bytes, 4);
    assert_eq!(record.stats().aggregates, 3);
    let replay = returned(&record);
    let Value::Agg(root) = replay.values()[0] else {
        panic!("root");
    };
    assert_eq!(
        replay.aggregates().tuple_get(root, 0).unwrap(),
        replay.aggregates().tuple_get(root, 1).unwrap()
    );
    assert_ne!(
        replay.aggregates().tuple_get(root, 0).unwrap(),
        replay.aggregates().tuple_get(root, 2).unwrap()
    );
}

#[test]
fn float_bits_decimal_scale_objects_and_nested_closure_environments_survive_replay() {
    let p = identity(ValueType::Closure);
    let mut vm = Vm::new(TestHost::returning(Value::Unit), Limits::default());
    let array = vm
        .aggregates_mut()
        .array_new(
            ElemTypeId(55),
            vec![
                Value::F64(-0.0),
                Value::F64(f64::from_bits(0x7ff8_0000_0000_0042)),
                Value::Decimal(Decimal::new(i128::MIN, 2)),
                Value::Bytes(vec![0, 255]),
            ],
        )
        .unwrap();
    let inner = Closure {
        func: FuncId(0),
        env: array,
    };
    let env = vm
        .aggregates_mut()
        .struct_new(
            TypeId(77),
            vec![
                Value::Closure(inner),
                Value::Obj(Obj {
                    host_type: HostTypeId(123),
                    handle: ObjHandle(987),
                }),
                Value::Func(FuncId(0)),
            ],
        )
        .unwrap();
    let record = capture(
        &mut vm,
        &p,
        &[Value::Closure(Closure {
            func: FuncId(0),
            env,
        })],
    );
    drop(vm);
    let output = returned(&record);
    let Value::Closure(outer) = output.values()[0] else {
        panic!("closure");
    };
    assert_eq!(
        output.aggregates().agg_type(outer.env).unwrap(),
        AggType::Struct {
            type_id: TypeId(77)
        }
    );
    let Value::Closure(inner) = output.aggregates().struct_get(outer.env, 0).unwrap() else {
        panic!("nested closure");
    };
    let Value::F64(zero) = output.aggregates().array_get(inner.env, 0).unwrap() else {
        panic!("zero");
    };
    let Value::F64(nan) = output.aggregates().array_get(inner.env, 1).unwrap() else {
        panic!("NaN");
    };
    assert_eq!(zero.to_bits(), (-0.0_f64).to_bits());
    assert_eq!(nan.to_bits(), 0x7ff8_0000_0000_0042);
    assert_eq!(
        output.aggregates().array_get(inner.env, 2).unwrap(),
        Value::Decimal(Decimal::new(i128::MIN, 2))
    );
    assert_eq!(
        output.aggregates().array_get(inner.env, 3).unwrap(),
        Value::Bytes(vec![0, 255])
    );
    assert_eq!(
        output.aggregates().struct_get(outer.env, 1).unwrap(),
        Value::Obj(Obj {
            host_type: HostTypeId(123),
            handle: ObjHandle(987)
        })
    );
}

#[test]
fn typed_host_errors_raw_wrong_types_and_fuel_are_replayed_as_actual_traps() {
    #[derive(Debug)]
    struct Failure(u64);
    impl std::fmt::Display for Failure {
        fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            panic!("never format error payload");
        }
    }
    impl std::error::Error for Failure {}
    let p = host_program(ValueType::I64);
    for (value, fuel, error) in [
        (Value::I64(7), 0, Some(HostError::custom(Failure(42)))),
        (Value::Bool(true), 0, None),
        (Value::I64(7), u64::MAX, None),
    ] {
        let mut host = TestHost::returning(value);
        host.fuel = fuel;
        host.error = error;
        let mut vm = Vm::new(host, Limits::default());
        let record = capture(&mut vm, &p, &[]);
        drop(vm);
        assert!(matches!(record.outcome(), Some(Outcome::Trapped(_))));
        let ReplayOutcome::Trapped(replayed) = record.replay().unwrap() else {
            panic!("trap");
        };
        match replayed.trap {
            Trap::HostCallFailed {
                error: HostError::Custom(error),
                ..
            } => assert_eq!(error.downcast_ref::<Failure>().unwrap().0, 42),
            Trap::TypeMismatch { actual, .. } => assert_eq!(actual, ValueType::Bool),
            Trap::FuelExceeded => assert_eq!(fuel, u64::MAX),
            other => panic!("unexpected trap: {other:?}"),
        }
    }
}

#[test]
fn rejected_entries_and_execution_limits_remain_replayable() {
    let p = identity(ValueType::I64);
    for (entry, args, limits) in [
        (FuncId(99), vec![], Limits::default()),
        (FuncId(0), vec![], Limits::default()),
        (FuncId(0), vec![Value::Bool(false)], Limits::default()),
        (
            FuncId(0),
            vec![Value::I64(7)],
            Limits {
                max_call_depth: 0,
                ..Limits::default()
            },
        ),
        (
            FuncId(0),
            vec![Value::I64(7)],
            Limits {
                fuel: 0,
                ..Limits::default()
            },
        ),
    ] {
        let mut vm = Vm::new(TestHost::returning(Value::Unit), limits);
        let mut recorder = Recorder::new(Arc::clone(&p), RecordLimits::default());
        vm.run(&p, entry, &args, recorder.mask(), Some(&mut recorder))
            .unwrap_err();
        let record = recorder.finish();
        record.status().unwrap();
        assert!(matches!(
            record.replay().unwrap(),
            ReplayOutcome::Trapped(_)
        ));
    }
}

#[test]
fn every_budget_is_explicit_and_does_not_stop_live_execution() {
    let p = identity(ValueType::Str);
    for (limits, resource) in [
        (
            RecordLimits {
                values: 0,
                ..RecordLimits::default()
            },
            CaptureResource::Values,
        ),
        (
            RecordLimits {
                payload_bytes: 3,
                ..RecordLimits::default()
            },
            CaptureResource::PayloadBytes,
        ),
    ] {
        let mut vm = Vm::new(TestHost::returning(Value::Unit), Limits::default());
        let mut recorder = Recorder::new(Arc::clone(&p), limits);
        let args = [Value::Str("abcd".into())];
        assert_eq!(
            vm.run(&p, FuncId(0), &args, recorder.mask(), Some(&mut recorder))
                .unwrap(),
            args
        );
        let record = recorder.finish();
        assert!(
            matches!(record.status(), Err(CaptureFault::Budget { resource: actual, .. }) if *actual == resource)
        );
        assert_eq!(record.stats().payload_bytes, 0);
        assert!(matches!(record.replay(), Err(ReplayError::Incomplete(_))));
    }
    let p = identity(ValueType::Agg);
    let mut vm = Vm::new(TestHost::returning(Value::Unit), Limits::default());
    let root = vm.aggregates_mut().tuple_new(vec![]).unwrap();
    let mut recorder = Recorder::new(
        Arc::clone(&p),
        RecordLimits {
            aggregates: 0,
            ..RecordLimits::default()
        },
    );
    vm.run(
        &p,
        FuncId(0),
        &[Value::Agg(root)],
        recorder.mask(),
        Some(&mut recorder),
    )
    .unwrap();
    assert!(matches!(
        recorder.finish().status(),
        Err(CaptureFault::Budget {
            resource: CaptureResource::Aggregates,
            ..
        })
    ));

    let p = host_program(ValueType::I64);
    let host = TestHost::returning(Value::I64(42));
    let effects = Arc::clone(&host.calls);
    let mut vm = Vm::new(host, Limits::default());
    let mut recorder = Recorder::new(
        Arc::clone(&p),
        RecordLimits {
            host_calls: 0,
            ..RecordLimits::default()
        },
    );
    assert_eq!(
        vm.run(&p, FuncId(0), &[], recorder.mask(), Some(&mut recorder))
            .unwrap(),
        [Value::I64(42)]
    );
    let record = recorder.finish();
    assert!(matches!(
        record.status(),
        Err(CaptureFault::Budget {
            resource: CaptureResource::HostCalls,
            ..
        })
    ));
    assert!(record.fault_location().is_some());
    assert_eq!(effects.load(Ordering::Relaxed), 1);
}

#[test]
fn missing_masks_program_mismatch_reuse_and_invalid_graphs_never_claim_completeness() {
    let p = identity(ValueType::I64);
    let mut vm = Vm::new(TestHost::returning(Value::Unit), Limits::default());
    for (mask, expected) in [
        (TraceMask::VALUES, CaptureFault::MissingCompletion),
        (TraceMask::RUN, CaptureFault::MissingEntry),
        (TraceMask::NONE, CaptureFault::MissingEntry),
    ] {
        let mut recorder = Recorder::new(Arc::clone(&p), RecordLimits::default());
        vm.run(&p, FuncId(0), &[Value::I64(7)], mask, Some(&mut recorder))
            .unwrap();
        let record = recorder.finish();
        assert_eq!(record.status(), Err(&expected));
    }
    let other = identity(ValueType::I64);
    let mut recorder = Recorder::new(Arc::clone(&p), RecordLimits::default());
    vm.run(
        &other,
        FuncId(0),
        &[Value::I64(7)],
        recorder.mask(),
        Some(&mut recorder),
    )
    .unwrap();
    assert_eq!(
        recorder.finish().status(),
        Err(&CaptureFault::ProgramMismatch)
    );
    let mut recorder = Recorder::new(Arc::clone(&p), RecordLimits::default());
    for _ in 0..2 {
        vm.run(
            &p,
            FuncId(0),
            &[Value::I64(7)],
            recorder.mask(),
            Some(&mut recorder),
        )
        .unwrap();
    }
    assert_eq!(recorder.finish().status(), Err(&CaptureFault::MultipleRuns));
    let p = host_program(ValueType::Agg);
    let mut vm = Vm::new(
        TestHost::returning(Value::Agg(AggHandle(u32::MAX))),
        Limits::default(),
    );
    let mut recorder = Recorder::new(Arc::clone(&p), RecordLimits::default());
    vm.run(&p, FuncId(0), &[], recorder.mask(), Some(&mut recorder))
        .unwrap_err();
    let record = recorder.finish();
    assert_eq!(
        record.status(),
        Err(&CaptureFault::InvalidAggregate { handle: u32::MAX })
    );
    assert!(record.fault_location().is_some());
    assert!(matches!(record.outcome(), Some(Outcome::Trapped(_))));
    assert!(matches!(record.replay(), Err(ReplayError::Incomplete(_))));
}

#[test]
fn payload_admission_and_root_copy_rejections_preserve_raw_host_outcomes_for_replay() {
    let p = host_program(ValueType::Bytes);
    for (limit, used) in [(3, 0), (7, 4)] {
        let mut vm = Vm::new(
            TestHost::returning(Value::Bytes(vec![1; 4])),
            Limits {
                max_payload_work: limit,
                ..Limits::default()
            },
        );
        let record = capture(&mut vm, &p, &[]);
        drop(vm);
        assert!(
            matches!(record.host_calls().next().unwrap().outcome, HostOutcome::Returned { values, .. } if values.len() == 1)
        );
        let ReplayOutcome::Trapped(error) = record.replay().unwrap() else {
            panic!("trap")
        };
        assert!(
            matches!(error.trap, Trap::PayloadWorkExceeded { limit: actual, used: actual_used, requested: 4 } if actual == limit && actual_used == used)
        );
        assert!(error.to_string().contains("Limits::max_payload_work"));
    }
    let mut vm = Vm::new(
        TestHost::returning(Value::Bytes(vec![1; 4])),
        Limits {
            max_payload_work: 8,
            ..Limits::default()
        },
    );
    let record = capture(&mut vm, &p, &[]);
    drop(vm);
    assert!(matches!(record.replay(), Ok(ReplayOutcome::Returned(_))));
}

#[test]
fn wide_decimal_host_results_survive_drop_and_checked_replay() {
    let p = host_program(ValueType::Decimal);
    for mantissa in [i128::MIN, i128::MAX] {
        let value = Value::Decimal(Decimal::new(mantissa, 255));
        let mut vm = Vm::new(TestHost::returning(value.clone()), Limits::default());
        let record = capture(&mut vm, &p, &[]);
        drop(vm);
        assert_eq!(returned(&record).values(), [value]);
    }
}

#[test]
fn wide_decimal_register_arguments_are_borrowed_by_hosts_and_returned_exactly() {
    let p = program(
        &[ValueType::Decimal],
        &[ValueType::Decimal],
        vec![ValueType::Unit, ValueType::Decimal],
        |body, builder| {
            let import = builder.host_sig_for(
                "observe",
                HostSig {
                    args: vec![ValueType::Decimal],
                    rets: vec![ValueType::Decimal],
                },
            );
            body.host_call_sig_id(0, import, 0, &[1], &[1]).ret(0, &[1]);
        },
    );
    for mantissa in [i128::MIN, i128::MAX] {
        let value = Value::Decimal(Decimal::new(mantissa, 255));
        let host = TestHost::returning(Value::Unit);
        let calls = Arc::clone(&host.calls);
        let mut vm = Vm::new(host, Limits::default());
        let record = capture(&mut vm, &p, core::slice::from_ref(&value));
        drop(vm);
        assert_eq!(returned(&record).values(), [value]);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }
}
