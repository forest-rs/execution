// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Function values preserve executable identity across independently compiled code.

use execution_tape::{
    asm::{FunctionAbi, FunctionSig, ProgramBuilder},
    contract::Contract,
    host::{Host, HostContext, HostError, SigHash, ValueRef},
    program::ValueType,
    recording::{InstructionCapture, RecordLimits, Recorder, ReplayOutcome, TransferAction},
    trace::{EntryOrigin, TraceSink},
    value::{Closure, FuncId, Value},
    verifier::VerifiedProgram,
    vm::{InvocationOptions, Limits, RunStatus, Vm},
};
use std::sync::Arc;

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
        panic!("indirect guest calls must not be delegated to a host");
    }
}

fn callee(closure: bool) -> (VerifiedProgram, FuncId) {
    let mut builder = ProgramBuilder::new();
    builder.set_program_name("callee with its own f0");
    let integer = builder.contract(Contract::Kind(ValueType::I64)).unwrap();
    let args = if closure {
        let env = builder.contract(Contract::Tuple(&[Some(integer)])).unwrap();
        vec![env.into(), integer.into()]
    } else {
        vec![integer.into()]
    };
    let entry = builder
        .declare_function(FunctionAbi {
            args,
            rets: vec![integer.into()],
        })
        .unwrap();
    let mut body = builder.function_asm(entry).unwrap();
    let forty = body.alloc_register(ValueType::I64);
    if closure {
        body.tuple_get(forty, 1, 0);
    } else {
        body.const_i64(forty, 40);
    }
    let answer = body.alloc_register(ValueType::I64);
    body.i64_add(answer, forty, if closure { 2 } else { 1 })
        .ret(0, &[answer]);
    builder.define_function(entry, body).unwrap();
    (builder.build_verified().unwrap(), entry)
}

fn caller(closure: bool) -> (VerifiedProgram, FuncId) {
    let mut builder = ProgramBuilder::new();
    builder.set_program_name("caller with a different f0");
    let kind = if closure {
        ValueType::Closure
    } else {
        ValueType::Func
    };
    let signature = builder
        .call_sig(FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let entry = builder
        .declare_function(FunctionSig {
            arg_types: vec![kind],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let mut body = builder.function_asm(entry).unwrap();
    // Force a normal aggregate construction/projection path before invocation. The value's
    // origin must travel with it rather than being reconstructed from the caller's code.
    let tuple = body.alloc_register(ValueType::Agg);
    let target = body.alloc_register(kind);
    let two = body.alloc_register(ValueType::I64);
    let answer = body.alloc_register(ValueType::I64);
    body.tuple_new(tuple, &[1])
        .tuple_get(target, tuple, 0)
        .const_i64(two, 2);
    body.call_indirect(0, signature, target, 0, &[two], &[answer])
        .ret(0, &[answer]);
    builder.define_function(entry, body).unwrap();
    (builder.build_verified().unwrap(), entry)
}

#[test]
fn ordinary_indirect_calls_follow_qualified_functions_and_closures_through_aggregates() {
    for closure in [false, true] {
        let (caller, entry) = caller(closure);
        let caller = Arc::new(caller);
        let (callee, function) = callee(closure);
        let mut vm = Vm::new(NoHost, Limits::default());
        let instance = vm.instantiate(&callee).unwrap();
        let function = instance.function(function).unwrap();
        let env = vm.aggregates_mut().tuple_new(vec![Value::I64(40)]).unwrap();
        let argument = if closure {
            Value::Closure(Closure {
                func: function,
                env,
            })
        } else {
            Value::Func(function)
        };
        let mut recorder = Recorder::new(caller.clone(), RecordLimits::default())
            .with_instructions(InstructionCapture::default());
        {
            let mask = recorder.mask();
            let args = [argument.clone()];
            let mut run = vm
                .start(
                    &caller,
                    entry,
                    &args,
                    InvocationOptions::default().trace(mask, &mut recorder),
                )
                .unwrap();
            let mut complete = false;
            for _ in 0..20 {
                match run.resume(100).unwrap() {
                    RunStatus::Suspended => {}
                    RunStatus::AwaitingHost => panic!("a guest call needs no host choreography"),
                    RunStatus::Complete => {
                        complete = true;
                        break;
                    }
                }
            }
            assert!(
                complete,
                "the foreign f0 is invoked rather than recursing into the caller's f0"
            );
            assert_eq!(run.take_outputs().unwrap(), [Value::I64(42)]);
        }
        assert_eq!(
            vm.run(
                &caller,
                entry,
                &[argument],
                execution_tape::trace::TraceMask::NONE,
                None
            )
            .unwrap(),
            [Value::I64(42)],
            "synchronous execution uses the same qualified guest call path"
        );
        let record = recorder.finish();
        assert_eq!(record.status(), Ok(()));
        assert!(record.transfers().any(|transfer| matches!(
            transfer.action,
            TransferAction::Enter {
                origin: EntryOrigin::GuestCall,
                ..
            }
        )));
        let ReplayOutcome::Returned(values) = record.replay().unwrap() else {
            panic!("checked guest return");
        };
        assert_eq!(values.values(), &[Value::I64(42)]);
    }
}

#[test]
fn a_closure_keeps_nominal_captures_private_across_code_boundaries() {
    use execution_tape::{host::HostSig, program::StructTypeDef};
    for nested in [false, true] {
        let mut builder = ProgramBuilder::new();
        builder.set_program_name("nominal capture owner");
        let integer = builder.contract(Contract::Kind(ValueType::I64)).unwrap();
        let schema = builder.struct_type(StructTypeDef {
            field_names: vec!["number".into()],
            field_types: vec![ValueType::I64],
        });
        let nominal = builder
            .contract(Contract::Struct {
                type_id: schema,
                fields: &[integer],
            })
            .unwrap();
        let environment = if nested {
            builder.contract(Contract::Tuple(&[Some(nominal)])).unwrap()
        } else {
            nominal
        };
        let entry = builder
            .declare_function(FunctionAbi {
                args: vec![environment.into(), integer.into()],
                rets: vec![integer.into()],
            })
            .unwrap();
        let mut body = builder.function_asm(entry).unwrap();
        let environment = builder.argument(entry, 0).unwrap();
        let argument = builder.argument(entry, 1).unwrap();
        let capture = if nested {
            let capture = body.alloc_register(ValueType::Agg);
            body.tuple_get(capture, environment, 0);
            capture
        } else {
            environment
        };
        let forty = body.alloc_register(ValueType::I64);
        let answer = body.alloc_register(ValueType::I64);
        body.struct_get(forty, capture, 0)
            .i64_add(answer, forty, argument)
            .ret(0, &[answer]);
        builder.define_function(entry, body).unwrap();
        let code = builder.build_verified().unwrap();

        // Both ordinary guest calls and embedding-requested callbacks enter the owner.
        // Callback evidence must retain the closure, rather than recording its environment
        // as if it were an explicit argument originating in the caller's definition.
        for native in [false, true] {
            let (caller, root) = if native {
                let mut builder = ProgramBuilder::new();
                let pause = builder.host_sig_for(
                    "schedule",
                    HostSig {
                        args: vec![],
                        rets: vec![ValueType::I64],
                    },
                );
                let root = builder
                    .declare_function(FunctionSig {
                        arg_types: vec![],
                        ret_types: vec![ValueType::I64],
                    })
                    .unwrap();
                let mut body = builder.function_asm(root).unwrap();
                let [answer] = builder.emit_call(&mut body, pause, &[]).unwrap();
                body.ret(0, &[answer]);
                builder.define_function(root, body).unwrap();
                (builder.build_verified().unwrap(), root)
            } else {
                caller(true)
            };
            let caller = Arc::new(caller);
            let mut vm = Vm::new(NoHost, Limits::default());
            let function = vm.function(&code, entry).unwrap();
            let capture = vm
                .aggregates_mut()
                .struct_new(schema, vec![Value::I64(40)])
                .unwrap();
            let environment = if nested {
                vm.aggregates_mut()
                    .tuple_new(vec![Value::Agg(capture)])
                    .unwrap()
            } else {
                capture
            };
            let closure = Value::Closure(Closure {
                func: function,
                env: environment,
            });
            let mut recorder = Recorder::new(caller.clone(), RecordLimits::default())
                .with_instructions(InstructionCapture::default());
            {
                let args = if native {
                    vec![]
                } else {
                    vec![closure.clone()]
                };
                let mut run = vm
                    .start(
                        &caller,
                        root,
                        &args,
                        InvocationOptions::default()
                            .yield_host_calls()
                            .trace(recorder.mask(), &mut recorder),
                    )
                    .unwrap();
                if native {
                    assert_eq!(run.resume(100).unwrap(), RunStatus::AwaitingHost);
                    run.pending_host_call()
                        .unwrap()
                        .call_guest(closure, vec![Value::I64(2)])
                        .unwrap();
                    while run.resume(100).unwrap() != RunStatus::AwaitingHost {}
                    let mut request = run.pending_host_call().unwrap();
                    let mut values = request.take_callback_results().unwrap();
                    assert_eq!(values, [Value::I64(42)]);
                    request.complete(&mut values, Ok(0)).unwrap();
                }
                while run.resume(100).unwrap() != RunStatus::Complete {}
                assert_eq!(run.take_outputs().unwrap(), [Value::I64(42)]);
            }
            let recording = recorder.finish();
            assert_eq!(recording.status(), Ok(()));
            assert!(
                matches!(recording.replay().unwrap(), ReplayOutcome::Returned(values)
                if values.values() == [Value::I64(42)])
            );
        }
    }
}

fn replacement_method(add: i64, pause: bool) -> (VerifiedProgram, FuncId) {
    use execution_tape::host::HostSig;
    let mut builder = ProgramBuilder::new();
    builder.set_program_name(if pause {
        "old method"
    } else {
        "compiled while paused"
    });
    let entry = builder
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let wait = builder.host_sig_for(
        "pause",
        HostSig {
            args: vec![],
            rets: vec![],
        },
    );
    let mut body = builder.function_asm(entry).unwrap();
    if pause {
        let [] = builder.emit_call(&mut body, wait, &[]).unwrap();
    }
    let amount = body.alloc_register(ValueType::I64);
    let answer = body.alloc_register(ValueType::I64);
    body.const_i64(amount, add)
        .i64_add(answer, 1, amount)
        .ret(0, &[answer]);
    builder.define_function(entry, body).unwrap();
    (builder.build_verified().unwrap(), entry)
}

#[test]
fn replacement_can_be_compiled_and_instantiated_while_an_old_method_is_suspended() {
    use execution_tape::host::HostSig;
    let mut builder = ProgramBuilder::new();
    builder.set_program_name("live lookup caller");
    let entry = builder
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::Func],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let signature = builder
        .call_sig(FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let lookup = builder.host_sig_for(
        "lookup",
        HostSig {
            args: vec![],
            rets: vec![ValueType::Func],
        },
    );
    let mut body = builder.function_asm(entry).unwrap();
    let two = body.alloc_register(ValueType::I64);
    let first = body.alloc_register(ValueType::I64);
    let second = body.alloc_register(ValueType::I64);
    let total = body.alloc_register(ValueType::I64);
    body.const_i64(two, 2)
        .call_indirect(0, signature, 1, 0, &[two], &[first]);
    let [target] = builder.emit_call(&mut body, lookup, &[]).unwrap();
    body.call_indirect(0, signature, target, 0, &[two], &[second])
        .i64_add(total, first, second)
        .ret(0, &[total]);
    builder.define_function(entry, body).unwrap();
    let caller = Arc::new(builder.build_verified().unwrap());
    let (old_code, old_entry) = replacement_method(40, true);
    let mut vm = Vm::new(NoHost, Limits::default());
    let old_instance = vm.instantiate(&old_code).unwrap();
    let argument = Value::Func(old_instance.function(old_entry).unwrap());
    drop(old_instance);
    drop(old_code);
    let mut recorder = Recorder::new(caller.clone(), RecordLimits::default());
    {
        let mask = recorder.mask();
        let args = [argument];
        let mut run = vm
            .start(
                &caller,
                entry,
                &args,
                InvocationOptions::default()
                    .yield_host_calls()
                    .trace(mask, &mut recorder),
            )
            .unwrap();
        let mut replacement = None;
        let mut complete = false;
        for _ in 0..20 {
            match run.resume(100).unwrap() {
                RunStatus::Suspended => {}
                RunStatus::Complete => {
                    complete = true;
                    break;
                }
                RunStatus::AwaitingHost => {
                    if replacement.is_none() {
                        assert_eq!(run.program().program().name(), Some("old method"));
                        let (code, entry) = replacement_method(100, false);
                        let instance = run.instantiate(&code).unwrap();
                        replacement = Some(instance.function(entry).unwrap());
                        assert_eq!(
                            run.program().program().name(),
                            Some("old method"),
                            "instantiation never patches an existing frame"
                        );
                    }
                    let request = run.pending_host_call().unwrap();
                    match request.symbol() {
                        "pause" => request.complete(&mut [], Ok(0)).unwrap(),
                        "lookup" => request
                            .complete(
                                &mut [Value::Func(replacement.as_ref().unwrap().clone())],
                                Ok(0),
                            )
                            .unwrap(),
                        symbol => panic!("unexpected native boundary {symbol}"),
                    }
                }
            }
        }
        assert!(complete);
        assert_eq!(
            run.take_outputs().unwrap(),
            [Value::I64(144)],
            "old code returns 42 and the subsequent lookup calls new code returning 102"
        );
    }
    let record = recorder.finish();
    assert_eq!(record.status(), Ok(()));
    let ReplayOutcome::Returned(values) = record.replay().unwrap() else {
        panic!("checked live replacement result");
    };
    assert_eq!(values.values(), &[Value::I64(144)]);
}

#[test]
fn an_escaped_closure_can_capture_foreign_code_and_run_after_its_original_store_is_dropped() {
    use execution_tape::callable::Callable;
    let mut builder = ProgramBuilder::new();
    builder.set_program_name("escaped composition");
    let closure_kind = builder
        .contract(Contract::Kind(ValueType::Closure))
        .unwrap();
    let environment_contract = builder
        .contract(Contract::Tuple(&[Some(closure_kind)]))
        .unwrap();
    let signature = builder
        .call_sig(FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let body = builder
        .declare_function(FunctionAbi {
            args: vec![environment_contract.into()],
            rets: vec![ValueType::I64.into()],
        })
        .unwrap();
    let mut code = builder.function_asm(body).unwrap();
    let nested = code.alloc_register(ValueType::Closure);
    let two = code.alloc_register(ValueType::I64);
    let answer = code.alloc_register(ValueType::I64);
    code.tuple_get(nested, 1, 0)
        .const_i64(two, 2)
        .call_indirect(0, signature, nested, 0, &[two], &[answer])
        .ret(0, &[answer]);
    builder.define_function(body, code).unwrap();
    let factory = builder
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::Closure],
            ret_types: vec![ValueType::Closure],
        })
        .unwrap();
    let mut code = builder.function_asm(factory).unwrap();
    let function = code.alloc_register(ValueType::Func);
    let environment = code.alloc_register(ValueType::Agg);
    let closure = code.alloc_register(ValueType::Closure);
    code.const_func(function, body)
        .tuple_new(environment, &[1])
        .closure_new(closure, function, environment)
        .ret(0, &[closure]);
    builder.define_function(factory, code).unwrap();
    let program = Arc::new(builder.build_verified().unwrap());
    let retained: Callable = {
        let (foreign, target) = callee(true);
        let mut source = Vm::new(NoHost, Limits::default());
        let function = source.function(&foreign, target).unwrap();
        let env = source
            .aggregates_mut()
            .tuple_new(vec![Value::I64(40)])
            .unwrap();
        let args = [Value::Closure(Closure {
            func: function,
            env,
        })];
        let mut run = source
            .start(&program, factory, &args, InvocationOptions::default())
            .unwrap();
        assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::Complete);
        run.retain_callable(0).unwrap()
    };
    let mut destination = Vm::new(NoHost, Limits::default());
    let mut recorder = Recorder::new(program, RecordLimits::default())
        .with_instructions(InstructionCapture::default());
    {
        let mask = recorder.mask();
        let mut run = retained
            .start(
                &mut destination,
                vec![],
                InvocationOptions::default().trace(mask, &mut recorder),
            )
            .unwrap();
        loop {
            match run.resume(100).unwrap() {
                RunStatus::Suspended => {}
                RunStatus::AwaitingHost => panic!("captured guest calls execute in the VM"),
                RunStatus::Complete => break,
            }
        }
        assert_eq!(run.take_outputs().unwrap(), [Value::I64(42)]);
    }
    assert!(
        matches!(recorder.finish().replay().unwrap(), ReplayOutcome::Returned(values) if values.values() == [Value::I64(42)])
    );
}

#[test]
fn an_indirect_signature_denial_names_the_actual_code_and_replays_without_entering_it() {
    use execution_tape::vm::Trap;
    let (caller, entry) = caller(false);
    let caller = Arc::new(caller);
    let mut builder = ProgramBuilder::new();
    builder.set_program_name("incompatible target");
    let target = builder
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::Bool],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let mut code = builder.function_asm(target).unwrap();
    let answer = code.alloc_register(ValueType::I64);
    code.const_i64(answer, 99).ret(0, &[answer]);
    builder.define_function(target, code).unwrap();
    let foreign = builder.build_verified().unwrap();
    let mut vm = Vm::new(NoHost, Limits::default());
    let target = vm.function(&foreign, target).unwrap();
    let mut recorder = Recorder::new(caller.clone(), RecordLimits::default());
    let mask = recorder.mask();
    let error = vm
        .run(
            &caller,
            entry,
            &[Value::Func(target.clone())],
            mask,
            Some(&mut recorder),
        )
        .unwrap_err();
    assert!(
        matches!(error.trap, Trap::CallableSignatureMismatch { function, .. } if function == target)
    );
    let record = recorder.finish();
    assert_eq!(
        record.transfers().count(),
        0,
        "invalid signatures never enter foreign code"
    );
    assert!(
        matches!(record.replay().unwrap(), ReplayOutcome::Trapped(error) if matches!(error.trap, Trap::CallableSignatureMismatch { ref function, .. } if function.instance().program().shares_definition_with(&foreign)))
    );
}

#[test]
fn automatic_entries_share_capture_limits_without_turning_them_into_driver_commands() {
    use execution_tape::recording::{CaptureFault, CaptureResource};
    for allowance in [0, 1] {
        let (caller, entry) = caller(false);
        let caller = Arc::new(caller);
        let (callee, target) = callee(false);
        let mut vm = Vm::new(NoHost, Limits::default());
        let target = vm.function(&callee, target).unwrap();
        let mut recorder = Recorder::new(
            caller.clone(),
            RecordLimits {
                control_commands: allowance,
                ..RecordLimits::default()
            },
        );
        let mask = recorder.mask();
        assert_eq!(
            vm.run(
                &caller,
                entry,
                &[Value::Func(target)],
                mask,
                Some(&mut recorder)
            )
            .unwrap(),
            [Value::I64(42)]
        );
        let record = recorder.finish();
        assert_eq!(record.control_commands().count(), 0);
        assert_eq!(record.stats().control_commands, allowance);
        if allowance == 0 {
            assert!(matches!(
                record.status(),
                Err(CaptureFault::Budget {
                    resource: CaptureResource::ControlCommands,
                    limit: 0
                })
            ));
            assert!(
                record.replay().is_err(),
                "missing transfers cannot be guessed"
            );
        } else {
            assert!(
                matches!(record.replay().unwrap(), ReplayOutcome::Returned(values) if values.values() == [Value::I64(42)])
            );
        }
    }
}

#[test]
fn importing_defaults_from_distinct_stores_preserves_distinct_namespaces() {
    let (program, entry) = callee(false);
    let first = Vm::new(NoHost, Limits::default())
        .function(&program, entry)
        .unwrap();
    let second = Vm::new(NoHost, Limits::default())
        .function(&program, entry)
        .unwrap();
    assert_ne!(first, second);
    let mut destination = Vm::new(NoHost, Limits::default());
    let a = destination
        .bind_instance(first.instance())
        .unwrap()
        .function(first.id())
        .unwrap();
    let b = destination
        .bind_instance(second.instance())
        .unwrap()
        .function(second.id())
        .unwrap();
    assert_ne!(a, b, "code equality does not merge namespace authority");
    assert_eq!(
        destination
            .bind_instance(first.instance())
            .unwrap()
            .function(first.id())
            .unwrap(),
        a
    );
    assert_eq!(
        destination
            .bind_instance(second.instance())
            .unwrap()
            .function(second.id())
            .unwrap(),
        b
    );
    assert!(destination.owns_instance(a.instance()));
    assert!(destination.owns_instance(b.instance()));
}
