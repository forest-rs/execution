// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Public control authoring: a captured computation resumes under a new handler context.

use execution_tape::{
    asm::{FunctionSig, ProgramBuilder},
    host::{Host, HostContext, HostError, SigHash, ValueRef},
    program::ValueType,
    value::{FuncId, Value},
    verifier::VerifiedProgram,
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
        panic!("this computation is entirely guest controlled")
    }
}

fn worker(nested: bool) -> (VerifiedProgram, FuncId) {
    let mut program = ProgramBuilder::new();
    let exchange = program
        .call_sig(FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let function = program
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::Tag],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let leaf = program
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::Tag, ValueType::I64],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let mut leaf_body = program.function_asm(leaf).unwrap();
    let tag = program.argument(leaf, 0).unwrap();
    let request = program.argument(leaf, 1).unwrap();
    let response = program
        .emit_suspend(&mut leaf_body, tag, exchange, &[request])
        .unwrap();
    leaf_body.ret(0, &response);
    program.define_function(leaf, leaf_body).unwrap();

    let mut body = program.function_asm(function).unwrap();
    let tag = program.argument(function, 0).unwrap();
    let request = body.alloc_register(ValueType::I64);
    let sum = body.alloc_register(ValueType::I64);
    body.const_i64(request, 1);
    let first = if nested {
        let [first] = program.emit_call(&mut body, leaf, &[tag, request]).unwrap();
        first
    } else {
        program
            .emit_suspend(&mut body, tag, exchange, &[request])
            .unwrap()[0]
    };
    body.const_i64(request, 2);
    let second = if nested {
        let [second] = program.emit_call(&mut body, leaf, &[tag, request]).unwrap();
        second
    } else {
        program
            .emit_suspend(&mut body, tag, exchange, &[request])
            .unwrap()[0]
    };
    body.i64_add(sum, first, second);
    body.ret(0, &[sum]);
    program.define_function(function, body).unwrap();
    (program.build_verified().unwrap(), function)
}

fn driver() -> (VerifiedProgram, FuncId, FuncId) {
    let mut program = ProgramBuilder::new();
    let exchange = program
        .call_sig(FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let initial = program
        .call_sig(FunctionSig {
            arg_types: vec![ValueType::Tag],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let operation = program
        .control_tag(
            "ask-number",
            FunctionSig {
                arg_types: vec![ValueType::I64],
                ret_types: vec![ValueType::I64],
            },
        )
        .unwrap();
    let root = program
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::Func],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let native_handler = program
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::I64, ValueType::Continuation],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let add_nine = program
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let mut body = program.function_asm(add_nine).unwrap();
    let request = program.argument(add_nine, 0).unwrap();
    let nine = body.alloc_register(ValueType::I64);
    let response = body.alloc_register(ValueType::I64);
    body.const_i64(nine, 9)
        .i64_add(response, request, nine)
        .ret(0, &[response]);
    program.define_function(add_nine, body).unwrap();

    for entry in [root, native_handler] {
        let mut body = program.function_asm(entry).unwrap();
        let tag = body.alloc_register(ValueType::Tag);
        body.const_tag(tag, operation);
        let returned = program.result_block(&mut body, exchange).unwrap();
        let second = program.handler_block(&mut body, tag, exchange).unwrap();
        let (request, continuation) = if entry == root {
            let worker = program.argument(root, 0).unwrap();
            let computation = program
                .emit_continuation(&mut body, worker, initial)
                .unwrap();
            let first = program.handler_block(&mut body, tag, exchange).unwrap();
            program
                .emit_resume(
                    &mut body,
                    computation,
                    initial,
                    &[tag],
                    returned.label(),
                    &[first.arm()],
                )
                .unwrap();
            body.place(first.label()).unwrap();
            (first.request()[0], first.continuation())
        } else {
            (
                program.argument(native_handler, 0).unwrap(),
                program.argument(native_handler, 1).unwrap(),
            )
        };
        let [response] = program.emit_call(&mut body, add_nine, &[request]).unwrap();
        program
            .emit_resume(
                &mut body,
                continuation,
                exchange,
                &[response],
                returned.label(),
                &[second.arm()],
            )
            .unwrap();
        body.place(second.label()).unwrap();
        let increment = body.alloc_register(ValueType::I64);
        let response = body.alloc_register(ValueType::I64);
        body.const_i64(increment, 18)
            .i64_add(response, second.request()[0], increment);
        program
            .emit_resume(
                &mut body,
                second.continuation(),
                exchange,
                &[response],
                returned.label(),
                &[],
            )
            .unwrap();
        body.place(returned.label()).unwrap();
        body.ret(0, returned.results());
        program.define_function(entry, body).unwrap();
    }
    (program.build_verified().unwrap(), root, native_handler)
}

#[test]
fn independent_code_resumes_under_replacement_handlers() {
    for nested in [false, true] {
        let (worker, compute) = worker(nested);
        let (driver, entry, _) = driver();
        let mut vm = Vm::new(NoHost, Limits::default());
        let callable = vm.function(&worker, compute).unwrap();
        let outputs = vm
            .run(
                &driver,
                entry,
                &[Value::Func(callable)],
                execution_tape::trace::TraceMask::NONE,
                None,
            )
            .unwrap();
        assert_eq!(outputs, [Value::I64(30)]);
    }
}

#[test]
fn aliases_cannot_resume_a_consumed_computation() {
    let mut program = ProgramBuilder::new();
    let signature = program
        .call_sig(FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let root = program
        .declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let leaf = program
        .declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let mut body = program.function_asm(leaf).unwrap();
    let value = body.alloc_register(ValueType::I64);
    body.const_i64(value, 42).ret(0, &[value]);
    program.define_function(leaf, body).unwrap();
    let mut body = program.function_asm(root).unwrap();
    let target = body.alloc_register(ValueType::Func);
    let computation = body.alloc_register(ValueType::Continuation);
    let alias = body.alloc_register(ValueType::Continuation);
    let result = body.alloc_register(ValueType::I64);
    body.const_func(target, leaf);
    body.cont_new(computation, target, signature);
    body.mov(alias, computation);
    let first = body.block(&[result]);
    let second = body.block(&[result]);
    body.cont_resume(computation, signature, &[], first, &[]);
    body.place(first).unwrap();
    body.cont_resume(alias, signature, &[], second, &[]);
    body.place(second).unwrap();
    body.ret(0, &[result]);
    program.define_function(root, body).unwrap();
    let program = program.build_verified().unwrap();
    let error = Vm::new(NoHost, Limits::default())
        .run(
            &program,
            root,
            &[],
            execution_tape::trace::TraceMask::NONE,
            None,
        )
        .unwrap_err();
    assert!(matches!(
        error.trap,
        execution_tape::vm::Trap::Continuation(execution_tape::control::ResumeError::Consumed)
    ));
}

#[test]
fn captured_frames_still_count_against_the_execution_limit() {
    let (worker, compute) = worker(true);
    let (driver, entry, _) = driver();
    let mut vm = Vm::new(
        NoHost,
        Limits {
            max_call_depth: 3,
            ..Limits::default()
        },
    );
    let callable = vm.function(&worker, compute).unwrap();
    let error = vm
        .run(
            &driver,
            entry,
            &[Value::Func(callable)],
            execution_tape::trace::TraceMask::NONE,
            None,
        )
        .unwrap_err();
    assert!(matches!(
        error.trap,
        execution_tape::vm::Trap::CallDepthExceeded
    ));
    // A rejected handler entry cancels all parked frames, without poisoning the next run.
    let mut vm = Vm::new(NoHost, Limits::default());
    let callable = vm.function(&worker, compute).unwrap();
    let outputs = vm
        .run(
            &driver,
            entry,
            &[Value::Func(callable)],
            execution_tape::trace::TraceMask::NONE,
            None,
        )
        .unwrap();
    assert_eq!(outputs, [Value::I64(30)]);
}

fn paused_worker() -> (VerifiedProgram, FuncId, FuncId, u32, u32) {
    use execution_tape::{host::HostSig, program::HostTypeId};
    let mut program = ProgramBuilder::new();
    let exchange = program
        .call_sig(FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let initial = program
        .call_sig(FunctionSig {
            arg_types: vec![ValueType::Tag, ValueType::Obj(HostTypeId(0))],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let operation = program
        .control_tag(
            "ask-number",
            FunctionSig {
                arg_types: vec![ValueType::I64],
                ret_types: vec![ValueType::I64],
            },
        )
        .unwrap();
    let pause = program.host_sig_for(
        "inspect",
        HostSig {
            args: vec![],
            rets: vec![],
        },
    );
    let root = program
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::Obj(HostTypeId(0))],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let worker = program
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::Tag, ValueType::Obj(HostTypeId(0))],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let mut body = program.function_asm(worker).unwrap();
    let tag = program.argument(worker, 0).unwrap();
    let object_register = program.argument(worker, 1).unwrap();
    let request = body.alloc_register(ValueType::I64);
    body.const_i64(request, 42);
    let response = program
        .emit_suspend(&mut body, tag, exchange, &[request])
        .unwrap()[0];
    body.ret(0, &[response]);
    program.define_function(worker, body).unwrap();

    let mut body = program.function_asm(root).unwrap();
    let object = program.argument(root, 0).unwrap();
    let tag = body.alloc_register(ValueType::Tag);
    let target = body.alloc_register(ValueType::Func);
    body.const_tag(tag, operation).const_func(target, worker);
    let computation = program
        .emit_continuation(&mut body, target, initial)
        .unwrap();
    let returned = program.result_block(&mut body, initial).unwrap();
    let clause = program.handler_block(&mut body, tag, exchange).unwrap();
    program
        .emit_resume(
            &mut body,
            computation,
            initial,
            &[tag, object],
            returned.label(),
            &[clause.arm()],
        )
        .unwrap();
    body.place(clause.label()).unwrap();
    let [] = program.emit_call(&mut body, pause, &[]).unwrap();
    program
        .emit_resume(
            &mut body,
            clause.continuation(),
            exchange,
            clause.request(),
            returned.label(),
            &[],
        )
        .unwrap();
    body.place(returned.label()).unwrap();
    body.ret(0, returned.results());
    program.define_function(root, body).unwrap();
    (
        program.build_verified().unwrap(),
        root,
        worker,
        object_register,
        response,
    )
}

#[test]
fn parked_frames_are_inspectable_and_contribute_object_roots() {
    use execution_tape::{
        program::HostTypeId,
        value::{Obj, ObjHandle},
        vm::{InvocationOptions, ObjectRootOrigin, RunStatus},
    };
    let (program, root, worker, object_register, response) = paused_worker();
    let object = Obj {
        host_type: HostTypeId(0),
        handle: ObjHandle(77),
    };
    let args = [Value::Obj(object)];
    let mut vm = Vm::new(NoHost, Limits::default());
    let mut run = vm
        .start(
            &program,
            root,
            &args,
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    while run.resume(100).unwrap() != RunStatus::AwaitingHost {}
    let mut view = run.inspect();
    let saved = view.saved_continuations().next().unwrap();
    let worker_frame = saved
        .frames()
        .find(|frame| frame.location.func == worker)
        .unwrap();
    assert!(worker_frame.waiting_for_return);
    assert!(
        !view
            .frames()
            .any(|frame| frame.activation == worker_frame.activation)
    );
    assert!(
        matches!(view.register(&worker_frame.activation, object_register).unwrap().value, Some(ValueRef::Obj(value)) if value == object)
    );
    assert!(
        view.register(&worker_frame.activation, response)
            .unwrap()
            .value
            .is_none(),
        "suspension responses are unavailable until resumption"
    );
    let mut retained_object = false;
    view.visit_objects(|root| {
        if let ObjectRootOrigin::Register {
            activation,
            register,
        } = root.origin
        {
            retained_object |= activation == worker_frame.activation
                && register == object_register
                && root.object == object;
        }
    });
    assert!(
        retained_object,
        "a collector sees object registers in parked computations"
    );
    drop(view);
    run.pending_host_call()
        .unwrap()
        .complete(&mut [], Ok(0))
        .unwrap();
    while run.resume(100).unwrap() != RunStatus::Complete {}
    assert_eq!(run.result().unwrap().unwrap(), [Value::I64(42)]);
    assert_eq!(run.inspect().saved_continuations().len(), 0);
}

#[test]
fn returning_from_the_root_retires_every_parked_computation_before_publication() {
    use execution_tape::{
        program::HostTypeId,
        value::{Obj, ObjHandle},
        vm::{InvocationOptions, RunStatus},
    };
    let (program, entry, _, _, _) = paused_worker();
    let arguments = [Value::Obj(Obj {
        host_type: HostTypeId(0),
        handle: ObjHandle(77),
    })];
    let mut vm = Vm::new(NoHost, Limits::default());
    let mut run = vm
        .start(
            &program,
            entry,
            &arguments,
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    while run.resume(100).unwrap() != RunStatus::AwaitingHost {}
    assert_eq!(run.inspect().saved_continuations().len(), 1);
    let root = run.inspect().frames().last().unwrap().activation;
    run.pending_host_call()
        .unwrap()
        .return_from(&root, vec![Value::I64(42)])
        .unwrap();
    assert_eq!(run.resume(100).unwrap(), RunStatus::Complete);
    assert_eq!(run.result().unwrap().unwrap(), [Value::I64(42)]);
    assert_eq!(run.inspect().saved_continuations().len(), 0);
}

fn host_paused_driver() -> (VerifiedProgram, FuncId, FuncId) {
    use execution_tape::host::HostSig;
    let mut program = ProgramBuilder::new();
    let pause = program.host_sig_for(
        "schedule",
        HostSig {
            args: vec![],
            rets: vec![ValueType::I64],
        },
    );
    let signature = FunctionSig {
        arg_types: vec![],
        ret_types: vec![ValueType::I64],
    };
    let root = program.declare_function(signature.clone()).unwrap();
    let callee = program.declare_function(signature).unwrap();
    let mut body = program.function_asm(root).unwrap();
    let [answer] = program.emit_call(&mut body, pause, &[]).unwrap();
    body.ret(0, &[answer]);
    program.define_function(root, body).unwrap();
    let mut body = program.function_asm(callee).unwrap();
    let answer = body.alloc_register(ValueType::I64);
    body.const_i64(answer, 42).ret(0, &[answer]);
    program.define_function(callee, body).unwrap();
    (program.build_verified().unwrap(), root, callee)
}

#[test]
fn native_resumption_denies_foreign_and_stale_aliases_without_consuming_the_host_request() {
    use execution_tape::{
        control::ResumeError,
        vm::{ControlError, InvocationOptions, RunStatus},
    };
    let (program, root, entry) = host_paused_driver();
    let mut original_vm = Vm::new(NoHost, Limits::default());
    let callee = original_vm.function(&program, entry).unwrap();
    let mut original = original_vm
        .start(
            &program,
            root,
            &[],
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    assert_eq!(original.resume(10).unwrap(), RunStatus::AwaitingHost);
    let continuation = original
        .pending_host_call()
        .unwrap()
        .create_continuation(callee)
        .unwrap();
    let alias = continuation.clone();
    assert!(
        !original
            .inspect()
            .saved_continuations()
            .next()
            .unwrap()
            .is_started()
    );

    let mut other_vm = Vm::new(NoHost, Limits::default());
    let mut other = other_vm
        .start(
            &program,
            root,
            &[],
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    assert_eq!(other.resume(10).unwrap(), RunStatus::AwaitingHost);
    assert!(matches!(
        other
            .pending_host_call()
            .unwrap()
            .resume_continuation(&alias, vec![], &[]),
        Err(ControlError::Resumption(ResumeError::ForeignExecution))
    ));
    other
        .pending_host_call()
        .unwrap()
        .complete(&mut [Value::I64(99)], Ok(0))
        .unwrap();
    assert_eq!(other.resume(10).unwrap(), RunStatus::Complete);

    original
        .pending_host_call()
        .unwrap()
        .resume_continuation(&continuation, vec![], &[])
        .unwrap();
    while original.resume(10).unwrap() != RunStatus::AwaitingHost {}
    let mut request = original.pending_host_call().unwrap();
    assert_eq!(request.take_callback_results().unwrap(), [Value::I64(42)]);
    assert!(matches!(
        request.resume_continuation(&alias, vec![], &[]),
        Err(ControlError::Resumption(ResumeError::Consumed))
    ));
    original
        .pending_host_call()
        .unwrap()
        .complete(&mut [Value::I64(42)], Ok(0))
        .unwrap();
    assert_eq!(original.resume(10).unwrap(), RunStatus::Complete);
    assert_eq!(original.result().unwrap().unwrap(), [Value::I64(42)]);
}

#[test]
fn native_control_accepts_aggregates_staged_by_its_pending_request() {
    use execution_tape::{
        asm::FunctionAbi,
        contract::Contract,
        host::HostSig,
        value::Closure,
        vm::{InvocationOptions, RunStatus},
    };
    let mut builder = ProgramBuilder::new();
    let integer = builder.contract(Contract::Kind(ValueType::I64)).unwrap();
    let tuple = builder.contract(Contract::Tuple(&[Some(integer)])).unwrap();
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
    let read = builder
        .declare_function(FunctionAbi {
            args: vec![tuple.into()],
            rets: vec![integer.into()],
        })
        .unwrap();
    let mut body = builder.function_asm(root).unwrap();
    let [answer] = builder.emit_call(&mut body, pause, &[]).unwrap();
    body.ret(0, &[answer]);
    builder.define_function(root, body).unwrap();
    let mut body = builder.function_asm(read).unwrap();
    let argument = builder.argument(read, 0).unwrap();
    let answer = body.alloc_register(ValueType::I64);
    body.tuple_get(answer, argument, 0).ret(0, &[answer]);
    builder.define_function(read, body).unwrap();
    let program = builder.build_verified().unwrap();
    let mut vm = Vm::new(NoHost, Limits::default());
    let target = vm.function(&program, read).unwrap();
    let mut run = vm
        .start(
            &program,
            root,
            &[],
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    assert_eq!(run.resume(100).unwrap(), RunStatus::AwaitingHost);

    // A closure environment need not exist in the committed heap before control admission.
    let mut request = run.pending_host_call().unwrap();
    let environment = request.context().tuple_new(vec![Value::I64(41)]).unwrap();
    let closure = request
        .create_continuation(Closure {
            func: target.clone(),
            env: environment,
        })
        .unwrap();
    run.pending_host_call()
        .unwrap()
        .resume_continuation(&closure, vec![], &[])
        .unwrap();
    while run.resume(100).unwrap() != RunStatus::AwaitingHost {}
    assert_eq!(
        run.pending_host_call().unwrap().take_callback_results(),
        Some(vec![Value::I64(41)])
    );

    // The same staging rule applies to rich resume inputs, before their contract is checked.
    let continuation = run
        .pending_host_call()
        .unwrap()
        .create_continuation(target)
        .unwrap();
    let mut request = run.pending_host_call().unwrap();
    let argument = request.context().tuple_new(vec![Value::I64(42)]).unwrap();
    request
        .resume_continuation(&continuation, vec![Value::Agg(argument)], &[])
        .unwrap();
    while run.resume(100).unwrap() != RunStatus::AwaitingHost {}
    let mut request = run.pending_host_call().unwrap();
    let mut outputs = request.take_callback_results().unwrap();
    assert_eq!(outputs, [Value::I64(42)]);
    request.complete(&mut outputs, Ok(0)).unwrap();
    while run.resume(100).unwrap() != RunStatus::Complete {}
    assert_eq!(run.result().unwrap().unwrap(), [Value::I64(42)]);
}

#[test]
fn suspension_requests_follow_the_same_code_ownership_rules_as_ordinary_calls() {
    use execution_tape::{program::StructTypeDef, trace::TraceMask, vm::Trap};
    for nominal in [false, true] {
        let mut producer = ProgramBuilder::new();
        let schema = producer.struct_type(StructTypeDef {
            field_names: vec!["number".into()],
            field_types: vec![ValueType::I64],
        });
        let exchange = producer
            .call_sig(FunctionSig {
                arg_types: vec![ValueType::Agg],
                ret_types: vec![ValueType::I64],
            })
            .unwrap();
        let worker = producer
            .declare_function(FunctionSig {
                arg_types: vec![ValueType::Tag],
                ret_types: vec![ValueType::I64],
            })
            .unwrap();
        let mut body = producer.function_asm(worker).unwrap();
        let tag = producer.argument(worker, 0).unwrap();
        let number = body.alloc_register(ValueType::I64);
        let request = body.alloc_register(ValueType::Agg);
        body.const_i64(number, 42);
        if nominal {
            body.struct_new(request, schema, &[number]);
        } else {
            body.tuple_new(request, &[number]);
        }
        let response = producer
            .emit_suspend(&mut body, tag, exchange, &[request])
            .unwrap()[0];
        body.ret(0, &[response]);
        producer.define_function(worker, body).unwrap();
        let producer = producer.build_verified().unwrap();

        let mut consumer = ProgramBuilder::new();
        let operation = consumer
            .control_tag(
                "inspect-number",
                FunctionSig {
                    arg_types: vec![ValueType::Agg],
                    ret_types: vec![ValueType::I64],
                },
            )
            .unwrap();
        let initial = consumer
            .call_sig(FunctionSig {
                arg_types: vec![ValueType::Tag],
                ret_types: vec![ValueType::I64],
            })
            .unwrap();
        let root = consumer
            .declare_function(FunctionSig {
                arg_types: vec![ValueType::Func],
                ret_types: vec![ValueType::I64],
            })
            .unwrap();
        let mut body = consumer.function_asm(root).unwrap();
        let target = consumer.argument(root, 0).unwrap();
        let tag = body.alloc_register(ValueType::Tag);
        body.const_tag(tag, operation);
        let computation = consumer
            .emit_continuation(&mut body, target, initial)
            .unwrap();
        let returned = consumer.result_block(&mut body, initial).unwrap();
        let exchange = consumer
            .call_sig(FunctionSig {
                arg_types: vec![ValueType::Agg],
                ret_types: vec![ValueType::I64],
            })
            .unwrap();
        let handler = consumer.handler_block(&mut body, tag, exchange).unwrap();
        consumer
            .emit_resume(
                &mut body,
                computation,
                initial,
                &[tag],
                returned.label(),
                &[handler.arm()],
            )
            .unwrap();
        body.place(handler.label()).unwrap();
        let answer = body.alloc_register(ValueType::I64);
        body.const_i64(answer, 42).ret(0, &[answer]);
        body.place(returned.label()).unwrap();
        body.ret(0, returned.results());
        consumer.define_function(root, body).unwrap();
        let consumer = consumer.build_verified().unwrap();
        let mut vm = Vm::new(NoHost, Limits::default());
        let callable = vm.function(&producer, worker).unwrap();
        let result = vm.run(
            &consumer,
            root,
            &[Value::Func(callable)],
            TraceMask::NONE,
            None,
        );
        if nominal {
            assert!(matches!(
                result.unwrap_err().trap,
                Trap::CrossInstanceValue {
                    kind: ValueType::Agg
                }
            ));
        } else {
            assert_eq!(result.unwrap(), [Value::I64(42)]);
        }
    }
}

#[test]
#[cfg(panic = "unwind")]
fn panicking_return_observers_preserve_the_root_and_poison_the_transfer() {
    use execution_tape::{
        trace::{TraceContext, TraceLocation, TraceMask, TraceOutcome, TraceSink},
        vm::{InvocationOptions, Trap},
    };
    use std::panic::{AssertUnwindSafe, catch_unwind};

    struct PanicOnce {
        seen_child: bool,
        panicked: bool,
    }
    impl TraceSink for PanicOnce {
        fn instr_end(
            &mut self,
            context: TraceContext<'_>,
            _: TraceLocation,
            _: Option<TraceLocation>,
            _: TraceOutcome<'_>,
        ) {
            // Root cont.resume completes only after its captured computation and handlers.
            let segment = context.stack().unwrap().segment_ordinal();
            self.seen_child |= segment != 0;
            if segment == 0 && self.seen_child && !self.panicked {
                self.panicked = true;
                panic!("return observer failed");
            }
        }
    }
    let (worker, compute) = worker(false);
    let (driver, entry, _) = driver();
    let mut vm = Vm::new(NoHost, Limits::default());
    let callable = vm.function(&worker, compute).unwrap();
    let args = [Value::Func(callable)];
    let mut observer = PanicOnce {
        seen_child: false,
        panicked: false,
    };
    let mut run = vm
        .start(
            &driver,
            entry,
            &args,
            InvocationOptions::default().trace(TraceMask::INSTR, &mut observer),
        )
        .unwrap();
    assert!(
        catch_unwind(AssertUnwindSafe(|| {
            for _ in 0..100 {
                run.resume(100).unwrap();
            }
        }))
        .is_err()
    );
    assert_eq!(run.inspect().frames().last().unwrap().location.func, entry);
    assert!(matches!(
        run.resume(100).unwrap_err().trap,
        Trap::GuestTransferAbandoned
    ));
    let work = run.work();
    let heap_size = run.aggregates().len_u32();
    assert!(matches!(
        run.resume(100).unwrap_err().trap,
        Trap::GuestTransferAbandoned
    ));
    assert_eq!(run.work(), work);
    assert_eq!(run.aggregates().len_u32(), heap_size);
    drop(run);
    assert_eq!(vm.aggregates().len_u32(), 0);
}

#[test]
#[cfg(panic = "unwind")]
fn panicking_entry_observers_cannot_retry_entry_or_execute_the_admitted_child() {
    use execution_tape::{
        trace::{ScopeKind, TraceContext, TraceMask, TraceSink},
        vm::{InvocationOptions, Trap},
    };
    use std::panic::{AssertUnwindSafe, catch_unwind};
    struct PanicAtEntry {
        after_frame: bool,
        panicked: bool,
    }
    impl PanicAtEntry {
        fn observe(&mut self, context: TraceContext<'_>, after_frame: bool) {
            if context.stack().unwrap().segment_ordinal() == 1
                && self.after_frame == after_frame
                && !self.panicked
            {
                self.panicked = true;
                panic!("entry observer failed");
            }
        }
    }
    impl TraceSink for PanicAtEntry {
        fn run_start(&mut self, context: TraceContext<'_>, _: FuncId, _: usize) {
            self.observe(context, false);
        }
        fn scope_enter(
            &mut self,
            context: TraceContext<'_>,
            _: ScopeKind,
            _: usize,
            _: FuncId,
            _: u32,
            _: Option<u64>,
        ) {
            self.observe(context, true);
        }
    }
    for after_frame in [false, true] {
        let (worker, compute) = worker(false);
        let (driver, entry, _) = driver();
        let mut vm = Vm::new(NoHost, Limits::default());
        let callable = vm.function(&worker, compute).unwrap();
        let args = [Value::Func(callable)];
        let mut observer = PanicAtEntry {
            after_frame,
            panicked: false,
        };
        let mut run = vm
            .start(
                &driver,
                entry,
                &args,
                InvocationOptions::default().trace(TraceMask::RUN | TraceMask::CALL, &mut observer),
            )
            .unwrap();
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                for _ in 0..100 {
                    run.resume(100).unwrap();
                }
            }))
            .is_err()
        );
        assert_eq!(run.inspect().frames().last().unwrap().location.func, entry);
        assert!(run.program().shares_definition_with(&worker));
        for _ in 0..2 {
            let work = run.work();
            let error = run.resume(100).unwrap_err();
            assert!(matches!(error.trap, Trap::GuestTransferAbandoned));
            assert_eq!(error.func, compute);
            assert_eq!(run.work(), work);
        }
        drop(run);
        assert_eq!(vm.aggregates().len_u32(), 0);
    }
}

#[test]
#[cfg(panic = "unwind")]
fn cancellation_observer_panics_cannot_skip_storage_rollback_or_frame_retirement() {
    use execution_tape::{
        program::HostTypeId,
        trace::{TraceContext, TraceMask, TraceOutcome, TraceSink},
        value::{Obj, ObjHandle},
        vm::{InvocationOptions, RunStatus, Trap},
    };
    use std::panic::{AssertUnwindSafe, catch_unwind};
    struct PanicOnCancellation(bool);
    impl TraceSink for PanicOnCancellation {
        fn run_end(&mut self, _: TraceContext<'_>, outcome: TraceOutcome<'_>) {
            if matches!(outcome, TraceOutcome::Cancelled) && !self.0 {
                self.0 = true;
                panic!("cancellation observer failed");
            }
        }
    }
    let (program, entry, _, _, _) = paused_worker();
    let arguments = [Value::Obj(Obj {
        host_type: HostTypeId(0),
        handle: ObjHandle(77),
    })];
    let mut vm = Vm::new(NoHost, Limits::default());
    let callable = vm.function(&program, entry).unwrap();
    let mut observer = PanicOnCancellation(false);
    let mut run = vm
        .start(
            &program,
            entry,
            &arguments,
            InvocationOptions::default()
                .yield_host_calls()
                .trace(TraceMask::RUN, &mut observer),
        )
        .unwrap();
    while run.resume(100).unwrap() != RunStatus::AwaitingHost {}
    let mut request = run.pending_host_call().unwrap();
    request.context().tuple_new(vec![Value::I64(1)]).unwrap();
    request.create_continuation(callable).unwrap();
    assert_eq!(run.aggregates().len_u32(), 1);
    assert_eq!(run.inspect().saved_continuations().len(), 2);
    assert!(
        catch_unwind(AssertUnwindSafe(|| {
            // The pending host expects no results. This failure cancels a parked worker.
            run.pending_host_call()
                .unwrap()
                .complete(&mut [Value::I64(0)], Ok(0))
                .unwrap();
        }))
        .is_err()
    );
    assert_eq!(run.aggregates().len_u32(), 0);
    assert_eq!(run.inspect().saved_continuations().len(), 0);
    assert_eq!(run.inspect().frames().len(), 0);
    for _ in 0..2 {
        let work = run.work();
        assert!(matches!(
            run.resume(100).unwrap_err().trap,
            Trap::HostReturnArityMismatch {
                expected: 0,
                actual: 1
            }
        ));
        assert_eq!(run.work(), work);
    }
}

#[test]
fn native_resume_installs_handlers_used_by_guest_suspensions() {
    use execution_tape::{
        control::Handler,
        program::ControlTagId,
        vm::{InvocationOptions, RunStatus},
    };
    let (program, root, _) = host_paused_driver();
    let (worker, compute) = worker(true);
    let (handlers, _, first_handler) = driver();
    let mut vm = Vm::new(NoHost, Limits::default());
    let worker = vm.function(&worker, compute).unwrap();
    let handlers = vm.instantiate(&handlers).unwrap();
    let tag = handlers.control_tag(ControlTagId(0)).unwrap();
    let handler = Handler::suspend(tag.clone(), handlers.function(first_handler).unwrap());
    let mut run = vm
        .start(
            &program,
            root,
            &[],
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    assert_eq!(run.resume(10).unwrap(), RunStatus::AwaitingHost);
    let computation = run
        .pending_host_call()
        .unwrap()
        .create_continuation(worker)
        .unwrap();
    run.pending_host_call()
        .unwrap()
        .resume_continuation(&computation, vec![Value::Tag(tag)], &[handler])
        .unwrap();
    while run.resume(100).unwrap() != RunStatus::AwaitingHost {}
    let mut request = run.pending_host_call().unwrap();
    let mut answer = request.take_callback_results().unwrap();
    assert_eq!(answer, [Value::I64(30)]);
    request.complete(&mut answer, Ok(0)).unwrap();
    assert_eq!(run.resume(10).unwrap(), RunStatus::Complete);
}

#[test]
fn handler_installation_checks_its_declared_continuation_protocol() {
    use execution_tape::{
        asm::FunctionAbi,
        contract::{Contract, ContractPosition},
        control::{ControlBoundary, Handler},
        vm::{ControlError, InvocationOptions, RunStatus, Trap},
    };
    for response_kind in [ValueType::I64, ValueType::Bool] {
        let mut clauses = ProgramBuilder::new();
        let integer = clauses.contract(Contract::Kind(ValueType::I64)).unwrap();
        let response = clauses.contract(Contract::Kind(response_kind)).unwrap();
        let continuation = clauses
            .contract(Contract::Continuation {
                parameters: &[response],
                results: &[integer],
            })
            .unwrap();
        let operation = clauses
            .control_tag(
                "ask-number",
                FunctionSig {
                    arg_types: vec![ValueType::I64],
                    ret_types: vec![ValueType::I64],
                },
            )
            .unwrap();
        let handler = clauses
            .declare_function(FunctionAbi {
                args: vec![integer.into(), continuation.into()],
                rets: vec![integer.into()],
            })
            .unwrap();
        // This clause need not invoke its continuation for the signature to matter.
        let mut body = clauses.function_asm(handler).unwrap();
        let answer = body.alloc_register(ValueType::I64);
        body.const_i64(answer, 42).ret(0, &[answer]);
        clauses.define_function(handler, body).unwrap();
        let clauses = clauses.build_verified().unwrap();
        let (program, root, _) = host_paused_driver();
        let (worker, compute) = worker(false);
        let mut vm = Vm::new(NoHost, Limits::default());
        let worker = vm.function(&worker, compute).unwrap();
        let clauses = vm.instantiate(&clauses).unwrap();
        let tag = clauses.control_tag(operation).unwrap();
        let handler = Handler::suspend(tag.clone(), clauses.function(handler).unwrap());
        let mut run = vm
            .start(
                &program,
                root,
                &[],
                InvocationOptions::default().yield_host_calls(),
            )
            .unwrap();
        assert_eq!(run.resume(100).unwrap(), RunStatus::AwaitingHost);
        let computation = run
            .pending_host_call()
            .unwrap()
            .create_continuation(worker)
            .unwrap();
        let result = run.pending_host_call().unwrap().resume_continuation(
            &computation,
            vec![Value::Tag(tag)],
            &[handler],
        );
        if response_kind == ValueType::Bool {
            assert!(
                matches!(result, Err(ControlError::Trapped(error)) if matches!(
                    error.trap,
                    Trap::ContractMismatch {
                        position: ContractPosition::Control { boundary: ControlBoundary::Handler, index: Some(1) },
                        ..
                    }
                ))
            );
            assert!(run.pending_host_call().is_none());
        } else {
            result.unwrap();
            while run.resume(100).unwrap() != RunStatus::AwaitingHost {}
            let mut request = run.pending_host_call().unwrap();
            let mut result = request.take_callback_results().unwrap();
            assert_eq!(result, [Value::I64(42)]);
            request.complete(&mut result, Ok(0)).unwrap();
            assert_eq!(run.resume(100).unwrap(), RunStatus::Complete);
        }
    }
}

#[test]
fn trace_context_identifies_a_resumed_stack_without_restarting_its_logical_run() {
    use execution_tape::trace::{TraceContext, TraceMask, TraceSink};
    #[derive(Default)]
    struct Observed {
        starts: Vec<u64>,
        instructions: Vec<u64>,
    }
    impl TraceSink for Observed {
        fn run_start(&mut self, context: TraceContext<'_>, _: FuncId, _: usize) {
            self.starts.push(
                context
                    .stack()
                    .expect("VM-qualified event")
                    .segment_ordinal(),
            );
        }
        fn instr(
            &mut self,
            context: TraceContext<'_>,
            _: FuncId,
            _: u32,
            _: u32,
            _: Option<u64>,
            _: u8,
        ) {
            self.instructions.push(
                context
                    .stack()
                    .expect("VM-qualified event")
                    .segment_ordinal(),
            );
        }
    }
    let (worker, compute) = worker(true);
    let (driver, entry, _) = driver();
    let mut vm = Vm::new(NoHost, Limits::default());
    let callable = vm.function(&worker, compute).unwrap();
    let mut observed = Observed::default();
    let outputs = vm
        .run(
            &driver,
            entry,
            &[Value::Func(callable)],
            TraceMask::RUN | TraceMask::INSTR,
            Some(&mut observed),
        )
        .unwrap();
    assert_eq!(outputs, [Value::I64(30)]);
    assert_eq!(observed.starts, [0, 1]);
    observed.instructions.dedup();
    assert_eq!(observed.instructions, [0, 1, 0, 1, 0, 1, 0]);
}

#[test]
fn fatal_native_control_admission_retires_the_pending_request() {
    use execution_tape::vm::{ControlError, InvocationOptions, RunStatus, Trap};
    let (program, root, entry) = host_paused_driver();
    let mut vm = Vm::new(
        NoHost,
        Limits {
            fuel: 1,
            ..Limits::default()
        },
    );
    let callable = vm.function(&program, entry).unwrap();
    let mut run = vm
        .start(
            &program,
            root,
            &[],
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    assert_eq!(run.resume(10).unwrap(), RunStatus::AwaitingHost);
    assert!(
        matches!(run.pending_host_call().unwrap().create_continuation(callable), Err(ControlError::Trapped(error)) if matches!(error.trap, Trap::FuelExceeded))
    );
    assert!(run.pending_host_call().is_none());
    assert!(matches!(
        run.resume(10).unwrap_err().trap,
        Trap::FuelExceeded
    ));
}

#[test]
fn durable_publication_rejects_control_capabilities_without_consuming_them() {
    use execution_graph::{ExecutionGraph, GraphError, TapeError, TapeExecutor};
    use execution_tape::{
        aggregates::{AggError, AggHeap},
        vm::{InvocationOptions, RunStatus},
    };
    use std::sync::Arc;
    let (program, root, entry) = host_paused_driver();
    let mut vm = Vm::new(NoHost, Limits::default());
    let callable = vm.function(&program, entry).unwrap();
    let mut run = vm
        .start(
            &program,
            root,
            &[],
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    assert_eq!(run.resume(10).unwrap(), RunStatus::AwaitingHost);
    let continuation = run
        .pending_host_call()
        .unwrap()
        .create_continuation(callable)
        .unwrap();
    let value = Value::Continuation(continuation.clone());
    let mut heap = AggHeap::new();
    let tuple = heap.tuple_new(vec![value.clone()]).unwrap();
    assert!(matches!(
        heap.snapshot(std::slice::from_ref(&value)),
        Err(AggError::ExecutionLocalValue)
    ));
    assert!(matches!(
        heap.snapshot(&[Value::Agg(tuple)]),
        Err(AggError::ExecutionLocalValue)
    ));
    assert_eq!(heap.len_u32(), 1);
    assert!(matches!(
        heap.into_snapshot(vec![Value::Agg(tuple)]),
        Err(AggError::ExecutionLocalValue)
    ));

    let mut builder = ProgramBuilder::new();
    let consume = builder
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::Continuation],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let mut body = builder.function_asm(consume).unwrap();
    let answer = body.alloc_register(ValueType::I64);
    body.const_i64(answer, 99).ret(0, &[answer]);
    builder.define_function(consume, body).unwrap();
    let mut graph = ExecutionGraph::new(TapeExecutor::new(NoHost, Limits::default()));
    let node = graph
        .add_tape_node(
            Arc::new(builder.build_verified().unwrap()),
            consume,
            vec!["computation".into()],
        )
        .unwrap();
    graph
        .set_input_value(node, "computation", value.into())
        .unwrap();
    assert!(matches!(
        graph.run_all(),
        Err(GraphError::Node {
            source: TapeError::ExecutionLocalInput { input: 0 },
            ..
        })
    ));

    run.pending_host_call()
        .unwrap()
        .resume_continuation(&continuation, vec![], &[])
        .unwrap();
    while run.resume(10).unwrap() != RunStatus::AwaitingHost {}
    let mut request = run.pending_host_call().unwrap();
    let mut values = request.take_callback_results().unwrap();
    assert_eq!(values, [Value::I64(42)]);
    request.complete(&mut values, Ok(0)).unwrap();
    assert_eq!(run.resume(10).unwrap(), RunStatus::Complete);
}

#[test]
fn rejected_control_signatures_keep_both_definitions_and_explain_the_difference() {
    use execution_tape::{control::ControlBoundary, trace::TraceMask, vm::Trap};
    let error = {
        let mut wanted = ProgramBuilder::new();
        wanted.set_program_name("integer protocol");
        let signature = wanted
            .call_sig(FunctionSig {
                arg_types: vec![ValueType::I64],
                ret_types: vec![ValueType::I64],
            })
            .unwrap();
        let root = wanted
            .declare_function(FunctionSig {
                arg_types: vec![ValueType::Func],
                ret_types: vec![],
            })
            .unwrap();
        let mut body = wanted.function_asm(root).unwrap();
        let callee = wanted.argument(root, 0).unwrap();
        wanted
            .emit_continuation(&mut body, callee, signature)
            .unwrap();
        body.ret(0, &[]);
        wanted.define_function(root, body).unwrap();
        let wanted = wanted.build_verified().unwrap();

        let mut offered = ProgramBuilder::new();
        offered.set_program_name("boolean computation");
        let function = offered
            .declare_function(FunctionSig {
                arg_types: vec![ValueType::Bool],
                ret_types: vec![ValueType::Bool],
            })
            .unwrap();
        let mut body = offered.function_asm(function).unwrap();
        body.ret(0, &[offered.argument(function, 0).unwrap()]);
        offered.define_function(function, body).unwrap();
        let offered = offered.build_verified().unwrap();
        let mut vm = Vm::new(NoHost, Limits::default());
        let function = vm.function(&offered, function).unwrap();
        vm.run(
            &wanted,
            root,
            &[Value::Func(function)],
            TraceMask::NONE,
            None,
        )
        .unwrap_err()
    };
    let Trap::ControlSignatureMismatch(details) = &error.trap else {
        panic!("precise protocol failure: {error}")
    };
    assert_eq!(details.boundary(), ControlBoundary::Create);
    assert_eq!(details.expected_inputs().kinds(), &[ValueType::I64]);
    assert_eq!(details.actual_inputs().kinds(), &[ValueType::Bool]);
    assert_eq!(
        details.expected_inputs().program().name(),
        Some("integer protocol")
    );
    assert_eq!(
        details.actual_inputs().program().name(),
        Some("boolean computation")
    );
    assert_eq!(
        details.to_string(),
        "continuation creation contracts differ: expected (i64) → (i64), found (bool) → (bool)"
    );
}

#[test]
fn rejected_native_resume_values_identify_the_input_and_its_contract_owner() {
    use execution_tape::{
        contract::ContractPosition,
        control::ControlBoundary,
        vm::{ControlError, InvocationOptions, RunStatus, Trap},
    };
    let (root_program, root, _) = host_paused_driver();
    let mut target = ProgramBuilder::new();
    target.set_program_name("takes an integer");
    let entry = target
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let mut body = target.function_asm(entry).unwrap();
    body.ret(0, &[target.argument(entry, 0).unwrap()]);
    target.define_function(entry, body).unwrap();
    let target = target.build_verified().unwrap();
    let mut vm = Vm::new(NoHost, Limits::default());
    let callee = vm.function(&target, entry).unwrap();
    let mut run = vm
        .start(
            &root_program,
            root,
            &[],
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    assert_eq!(run.resume(10).unwrap(), RunStatus::AwaitingHost);
    let continuation = run
        .pending_host_call()
        .unwrap()
        .create_continuation(callee)
        .unwrap();
    let Err(ControlError::Trapped(trap)) = run.pending_host_call().unwrap().resume_continuation(
        &continuation,
        vec![Value::Bool(false)],
        &[],
    ) else {
        panic!("wrong resume input traps")
    };
    let Trap::ContractMismatch {
        position, error, ..
    } = &trap.trap
    else {
        panic!("typed input failure: {trap}")
    };
    assert_eq!(
        *position,
        ContractPosition::Control {
            boundary: ControlBoundary::Resume,
            index: Some(0)
        }
    );
    assert_eq!(
        error.contract_program.as_ref().unwrap().program().name(),
        Some("takes an integer")
    );
    assert!(
        trap.to_string()
            .contains("resumption input[0]: expected i64, found bool")
    );
    assert!(run.pending_host_call().is_none());
}
