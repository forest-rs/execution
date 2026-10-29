// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Borrowed boundary observations through ordinary public execution and retention APIs.

use execution_tape::aggregates::{AggError, ValueSnapshot};
use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
use execution_tape::host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef};
use execution_tape::program::{HostSigId, Program, ValueType};
use execution_tape::trace::{
    HostCallOutcome, TraceLocation, TraceMask, TraceOutcome, TraceSink, ValueEvent,
};
use execution_tape::value::{AggHandle, FuncId, Value};
use execution_tape::verifier::VerifiedProgram;
use execution_tape::vm::{ExecutionContext, Limits, Trap, TrapInfo, Vm};

type RetainedValues = Result<ValueSnapshot, AggError>;

#[derive(Default)]
struct Observe {
    order: Vec<&'static str>,
    entry: Option<(FuncId, Vec<Value>, u32, Limits)>,
    host_args: Vec<ValueSnapshot>,
    host_returns: Vec<(TraceLocation, HostSigId, Vec<Value>, u64, RetainedValues)>,
    host_errors: Vec<HostError>,
    candidate: Option<(TraceLocation, Vec<Value>, RetainedValues)>,
    outcome: Option<Result<(), TrapInfo>>,
}

impl TraceSink for Observe {
    fn mask(&self) -> TraceMask {
        TraceMask::VALUES | TraceMask::RUN
    }

    fn run_start(&mut self, _: &Program, _: FuncId, _: usize) {
        self.order.push("run_start");
    }

    fn values(&mut self, _: &Program, event: ValueEvent<'_>) {
        match event {
            ValueEvent::Entry {
                entry,
                args,
                aggregates,
                limits,
                work,
            } => {
                assert_eq!(
                    work,
                    execution_tape::vm::DataWork::default(),
                    "borrowed entry arguments have no preparation debit"
                );
                self.order.push("entry");
                self.entry = Some((entry, args.to_vec(), aggregates.len_u32(), limits.clone()));
            }
            ValueEvent::HostCall {
                location,
                host_sig,
                args,
                outcome,
                aggregates,
            } => {
                self.order.push("host");
                let args: Vec<_> = args.iter().copied().map(ValueRef::to_value).collect();
                self.host_args.push(aggregates.snapshot(&args).unwrap());
                match outcome {
                    HostCallOutcome::Returned { values, extra_fuel } => {
                        self.host_returns.push((
                            location,
                            host_sig,
                            values.to_vec(),
                            extra_fuel,
                            aggregates.snapshot(values),
                        ));
                    }
                    HostCallOutcome::Failed(error) => self.host_errors.push(error.clone()),
                }
            }
            ValueEvent::ReturnCandidate {
                location,
                values,
                aggregates,
            } => {
                self.order.push("candidate");
                self.candidate = Some((location, values.to_vec(), aggregates.snapshot(values)));
            }
        }
    }

    fn run_end(&mut self, _: &Program, outcome: TraceOutcome<'_>) {
        self.order.push("run_end");
        self.outcome = Some(match outcome {
            TraceOutcome::Ok => Ok(()),
            TraceOutcome::Trap(error) => Err(error.clone()),
            TraceOutcome::Cancelled => panic!("run-to-completion cannot cancel"),
        });
    }
}

struct NeverHost;
impl Host for NeverHost {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        _: &[ValueRef<'_>],
        _: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        panic!("the VM must not invoke this host");
    }
}

struct EchoHost;
impl Host for EchoHost {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        args: &[ValueRef<'_>],
        rets: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        rets[0] = args[0].to_value();
        Ok(0)
    }
}

fn identity() -> VerifiedProgram {
    let mut builder = ProgramBuilder::new();
    let mut body = Asm::new();
    body.set_register_types(vec![ValueType::Unit, ValueType::I64]);
    body.ret(0, &[1]);
    builder
        .push_function_checked(
            body,
            FunctionSig {
                arg_types: vec![ValueType::I64],
                ret_types: vec![ValueType::I64],
            },
        )
        .unwrap();
    builder.build_verified().unwrap()
}

fn host_program(args: &[ValueType], ret: ValueType) -> VerifiedProgram {
    let mut builder = ProgramBuilder::new();
    let import = builder.host_sig_for(
        "observe",
        HostSig {
            args: args.into(),
            rets: vec![ret],
        },
    );
    let mut body = Asm::new();
    let mut kinds = vec![ValueType::Unit];
    kinds.extend_from_slice(args);
    kinds.push(ret);
    body.set_register_types(kinds);
    let output = u32::try_from(args.len()).unwrap() + 1;
    let inputs: Vec<_> = (1..output).collect();
    body.host_call_sig_id(0, import, 0, &inputs, &[output]);
    body.ret(0, &[output]);
    builder
        .push_function_checked(
            body,
            FunctionSig {
                arg_types: args.into(),
                ret_types: vec![ret],
            },
        )
        .unwrap();
    builder.build_verified().unwrap()
}

#[test]
fn value_delivery_is_independent_of_metadata_masks() {
    let program = identity();
    let mut vm = Vm::new(NeverHost, Limits::default());
    for mask in [
        TraceMask::NONE,
        TraceMask::RUN,
        TraceMask::INSTR,
        TraceMask::CALL,
        TraceMask::HOST,
        TraceMask::VALUES,
        TraceMask::VALUES | TraceMask::RUN,
    ] {
        let mut sink = Observe::default();
        assert_eq!(
            vm.run(
                &program,
                FuncId(0),
                &[Value::I64(42)],
                mask,
                Some(&mut sink)
            )
            .unwrap(),
            [Value::I64(42)]
        );
        let values = mask.contains(TraceMask::VALUES);
        assert_eq!(sink.entry.is_some(), values);
        assert_eq!(sink.candidate.is_some(), values);
        assert_eq!(sink.outcome.is_some(), mask.contains(TraceMask::RUN));
        if values {
            assert_eq!(sink.entry.as_ref().unwrap().1, [Value::I64(42)]);
            assert_eq!(sink.candidate.as_ref().unwrap().1, [Value::I64(42)]);
        }
    }
}

#[test]
fn entry_values_and_limits_precede_rejected_invocation_validation() {
    let program = identity();
    let cases = [
        (FuncId(99), vec![Value::I64(7)], Limits::default()),
        (FuncId(0), vec![], Limits::default()),
        (FuncId(0), vec![Value::Bool(true)], Limits::default()),
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
    ];
    for (entry, args, limits) in cases {
        let mut vm = Vm::new(NeverHost, limits.clone());
        let mut sink = Observe::default();
        vm.run(&program, entry, &args, sink.mask(), Some(&mut sink))
            .unwrap_err();
        assert_eq!(sink.order, ["run_start", "entry", "run_end"]);
        let observed = sink.entry.unwrap();
        assert_eq!(observed.0, entry);
        assert_eq!(observed.1, args);
        assert_eq!(observed.2, 0);
        assert_eq!(observed.3.fuel, limits.fuel);
        assert_eq!(observed.3.max_call_depth, limits.max_call_depth);
        assert!(sink.outcome.unwrap().is_err());
    }
}

struct ReturnHost {
    values: Vec<Value>,
    fuel: u64,
    error: Option<HostError>,
}

impl Host for ReturnHost {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        _: &[ValueRef<'_>],
        rets: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        rets.clone_from_slice(&self.values);
        self.error.clone().map_or(Ok(self.fuel), Err)
    }
}

#[test]
fn host_raw_values_errors_and_fuel_precede_vm_validation() {
    let program = host_program(&[ValueType::I64], ValueType::I64);
    for (value, fuel, succeeds) in [
        (Value::I64(8), 3, true),
        (Value::Bool(true), 17, false),
        (Value::I64(8), u64::MAX, false),
    ] {
        let mut vm = Vm::new(
            ReturnHost {
                values: vec![value.clone()],
                fuel,
                error: None,
            },
            Limits::default(),
        );
        let mut sink = Observe::default();
        let result = vm.run(
            &program,
            FuncId(0),
            &[Value::I64(7)],
            sink.mask(),
            Some(&mut sink),
        );
        assert_eq!(result.is_ok(), succeeds);
        assert_eq!(sink.host_args[0].values(), [Value::I64(7)]);
        let (location, import, raw, charged, _) = &sink.host_returns[0];
        assert_eq!(
            *location,
            TraceLocation {
                func: FuncId(0),
                pc: 0
            }
        );
        assert_eq!(*import, HostSigId(0));
        assert_eq!(raw, &[value]);
        assert_eq!(*charged, fuel);
        assert_eq!(sink.candidate.is_some(), succeeds);
        if fuel == u64::MAX {
            assert!(matches!(
                result,
                Err(TrapInfo {
                    trap: Trap::FuelExceeded,
                    ..
                })
            ));
        } else if !succeeds {
            assert!(matches!(
                result,
                Err(TrapInfo {
                    trap: Trap::TypeMismatch { .. },
                    ..
                })
            ));
        }
    }
    let mut vm = Vm::new(
        ReturnHost {
            values: vec![Value::I64(999)],
            fuel: 100,
            error: Some(HostError::Failed),
        },
        Limits::default(),
    );
    let mut sink = Observe::default();
    vm.run(
        &program,
        FuncId(0),
        &[Value::I64(7)],
        sink.mask(),
        Some(&mut sink),
    )
    .unwrap_err();
    assert_eq!(sink.order, ["run_start", "entry", "host", "run_end"]);
    assert!(matches!(sink.host_errors.as_slice(), [HostError::Failed]));
    assert!(
        sink.host_returns.is_empty(),
        "partial writes on host failure are not returns"
    );

    let mut vm = Vm::new(
        NeverHost,
        Limits {
            max_host_calls: 0,
            ..Limits::default()
        },
    );
    let mut sink = Observe::default();
    let error = vm
        .run(
            &program,
            FuncId(0),
            &[Value::I64(7)],
            sink.mask(),
            Some(&mut sink),
        )
        .unwrap_err();
    assert!(matches!(error.trap, Trap::HostCallLimitExceeded));
    assert_eq!(sink.order, ["run_start", "entry", "run_end"]);
}

#[test]
fn borrowed_host_failure_can_be_retained_without_flattening_its_typed_payload() {
    #[derive(Debug)]
    struct DomainFailure(u64);
    impl std::fmt::Display for DomainFailure {
        fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            panic!("observing a typed error must not format it");
        }
    }
    impl std::error::Error for DomainFailure {}
    let program = host_program(&[], ValueType::I64);
    let mut vm = Vm::new(
        ReturnHost {
            values: vec![Value::I64(999)],
            fuel: 3,
            error: Some(HostError::custom(DomainFailure(42))),
        },
        Limits::default(),
    );
    let mut sink = Observe::default();
    let failure = vm
        .run(&program, FuncId(0), &[], sink.mask(), Some(&mut sink))
        .unwrap_err();
    drop(vm);
    let Trap::HostCallFailed {
        error: HostError::Custom(returned),
        ..
    } = failure.trap
    else {
        panic!("typed VM failure");
    };
    let HostError::Custom(observed) = sink.host_errors.pop().unwrap() else {
        panic!("typed observed failure");
    };
    assert!(std::sync::Arc::ptr_eq(&returned, &observed));
    assert_eq!(observed.downcast_ref::<DomainFailure>().unwrap().0, 42);
    assert!(sink.host_returns.is_empty());
}

#[test]
fn entry_and_host_arguments_are_borrowed_without_materializing_strings() {
    struct Borrowed {
        expected: usize,
        seen: usize,
    }
    impl TraceSink for Borrowed {
        fn values(&mut self, _: &Program, event: ValueEvent<'_>) {
            let pointer = match event {
                ValueEvent::Entry { args, .. } => {
                    let Value::Str(s) = &args[0] else {
                        panic!("string entry");
                    };
                    s.as_ptr() as usize
                }
                ValueEvent::HostCall { args, .. } => {
                    let ValueRef::Str(s) = args[0] else {
                        panic!("string host input");
                    };
                    s.as_ptr() as usize
                }
                ValueEvent::ReturnCandidate { .. } => return,
            };
            assert_eq!(pointer, self.expected);
            self.seen += 1;
        }
    }
    let args = [Value::Str("borrowed input".repeat(128))];
    let Value::Str(s) = &args[0] else {
        unreachable!()
    };
    let mut sink = Borrowed {
        expected: s.as_ptr() as usize,
        seen: 0,
    };
    let program = host_program(&[ValueType::Str], ValueType::Str);
    let mut vm = Vm::new(EchoHost, Limits::default());
    assert_eq!(
        vm.run(
            &program,
            FuncId(0),
            &args,
            TraceMask::VALUES,
            Some(&mut sink)
        )
        .unwrap(),
        args
    );
    assert_eq!(sink.seen, 2);
}

#[test]
fn return_candidates_preserve_pre_compaction_identity_and_exclude_output_prefix() {
    let mut builder = ProgramBuilder::new();
    let import = builder.host_sig_for(
        "observe",
        HostSig {
            args: vec![ValueType::Agg],
            rets: vec![ValueType::Agg],
        },
    );
    let mut body = Asm::new();
    body.set_register_types(vec![
        ValueType::Unit,
        ValueType::I64,
        ValueType::Agg,
        ValueType::Agg,
        ValueType::Agg,
    ]);
    body.const_i64(1, 7);
    for _ in 0..10 {
        body.tuple_new(2, &[1]);
    }
    body.tuple_new(3, &[2, 2]);
    body.host_call_sig_id(0, import, 0, &[3], &[4]);
    body.ret(0, &[4]);
    builder
        .push_function_checked(
            body,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::Agg],
            },
        )
        .unwrap();
    let program = builder.build_verified().unwrap();
    let mut vm = Vm::new(EchoHost, Limits::default());
    let mut sink = Observe::default();
    let mut outputs = vec![Value::I64(99)];
    vm.run_with_ctx(
        &mut ExecutionContext::new(),
        &program,
        FuncId(0),
        &[],
        &mut outputs,
        sink.mask(),
        Some(&mut sink),
        None,
    )
    .unwrap();
    assert_eq!(outputs[0], Value::I64(99));
    let (location, candidate, snapshot) = sink.candidate.unwrap();
    assert_eq!(location.func, FuncId(0));
    assert_eq!(candidate, [Value::Agg(AggHandle(10))]);
    assert_eq!(
        sink.host_returns[0].2, candidate,
        "identity is stable across boundary events"
    );
    assert_ne!(candidate[0], outputs[1], "commit remaps the retained graph");
    assert_eq!(vm.aggregates().len_u32(), 2);
    assert!(sink.outcome.unwrap().is_ok());
    drop(vm);
    let snapshot = snapshot.unwrap();
    let Value::Agg(root) = snapshot.values()[0] else {
        panic!("root");
    };
    let first = snapshot.aggregates().tuple_get(root, 0).unwrap();
    assert_eq!(first, snapshot.aggregates().tuple_get(root, 1).unwrap());
    let Value::Agg(child) = first else {
        panic!("child");
    };
    assert_eq!(
        snapshot.aggregates().tuple_get(child, 0).unwrap(),
        Value::I64(7)
    );
}

#[test]
fn dangling_host_and_root_values_remain_observable_before_the_commit_trap() {
    let program = host_program(&[], ValueType::Agg);
    let mut vm = Vm::new(
        ReturnHost {
            values: vec![Value::Agg(AggHandle(u32::MAX))],
            fuel: 0,
            error: None,
        },
        Limits::default(),
    );
    let mut sink = Observe::default();
    let mut outputs = vec![Value::I64(99)];
    let error = vm
        .run_with_ctx(
            &mut ExecutionContext::new(),
            &program,
            FuncId(0),
            &[],
            &mut outputs,
            sink.mask(),
            Some(&mut sink),
            None,
        )
        .unwrap_err();
    assert!(
        matches!(error.trap, Trap::AggError(AggError::UnresolvedStagedHandle)),
        "{error:?}"
    );
    assert_eq!(outputs, [Value::I64(99)]);
    assert_eq!(
        sink.order,
        ["run_start", "entry", "host", "candidate", "run_end"]
    );
    assert!(matches!(sink.host_returns[0].4, Err(AggError::BadHandle)));
    let (_, raw, retained) = sink.candidate.unwrap();
    assert_eq!(raw, [Value::Agg(AggHandle(u32::MAX))]);
    assert!(matches!(retained, Err(AggError::BadHandle)));
    assert!(sink.outcome.unwrap().is_err());
}

#[test]
fn a_retained_host_graph_absent_from_entry_args_can_be_restored_before_replay() {
    let program = host_program(&[], ValueType::Agg);
    let mut live = Vm::new(
        ReturnHost {
            values: vec![Value::Unit],
            fuel: 0,
            error: None,
        },
        Limits::default(),
    );
    live.aggregates_mut()
        .tuple_new(vec![Value::I64(123)])
        .unwrap();
    let root = live
        .aggregates_mut()
        .tuple_new(vec![
            Value::Str("durable host value".into()),
            Value::F64(f64::from_bits(0x7ff8_0000_0000_0042)),
        ])
        .unwrap();
    live.host_mut().values[0] = Value::Agg(root);
    let mut capture = Observe::default();
    live.run(&program, FuncId(0), &[], capture.mask(), Some(&mut capture))
        .unwrap();
    assert_eq!(capture.entry.unwrap().2, 2);
    let snapshot = capture.host_returns.pop().unwrap().4.unwrap();
    drop(live);

    // The recorded host needs no live object state and cannot mutate HostContext. Restore its
    // discovered initial-state graph into a fresh VM through the existing public import API.
    let mut replay = Vm::new(
        ReturnHost {
            values: vec![Value::Unit],
            fuel: 0,
            error: None,
        },
        Limits::default(),
    );
    replay.host_mut().values = replay.aggregates_mut().import(&snapshot).unwrap();
    let outputs = replay
        .run(&program, FuncId(0), &[], TraceMask::NONE, None)
        .unwrap();
    let Value::Agg(restored) = outputs[0] else {
        panic!("replayed graph");
    };
    assert_ne!(
        root, restored,
        "heap-local IDs are remapped, not treated as durable identity"
    );
    assert_eq!(
        replay.aggregates().tuple_get(restored, 0).unwrap(),
        Value::Str("durable host value".into())
    );
    let Value::F64(value) = replay.aggregates().tuple_get(restored, 1).unwrap() else {
        panic!("NaN payload");
    };
    assert_eq!(value.to_bits(), 0x7ff8_0000_0000_0042);
}

#[test]
fn nested_function_returns_do_not_emit_root_candidates() {
    let mut builder = ProgramBuilder::new();
    let mut callee = Asm::new();
    callee.set_register_types(vec![ValueType::Unit, ValueType::I64]);
    callee.ret(0, &[1]);
    let callee = builder
        .push_function_checked(
            callee,
            FunctionSig {
                arg_types: vec![ValueType::I64],
                ret_types: vec![ValueType::I64],
            },
        )
        .unwrap();
    let mut body = Asm::new();
    body.set_register_types(vec![ValueType::Unit, ValueType::I64, ValueType::I64]);
    body.call(0, callee, 0, &[1], &[2]);
    body.ret(0, &[2]);
    let entry = builder
        .push_function_checked(
            body,
            FunctionSig {
                arg_types: vec![ValueType::I64],
                ret_types: vec![ValueType::I64],
            },
        )
        .unwrap();
    let program = builder.build_verified().unwrap();
    let mut vm = Vm::new(NeverHost, Limits::default());
    let mut sink = Observe::default();
    vm.run(
        &program,
        entry,
        &[Value::I64(42)],
        sink.mask(),
        Some(&mut sink),
    )
    .unwrap();
    assert_eq!(sink.order, ["run_start", "entry", "candidate", "run_end"]);
    assert_eq!(sink.candidate.unwrap().0.func, entry);
}
