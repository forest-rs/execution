// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

extern crate std;
use super::*;
use crate::{
    asm::{FunctionSig, ProgramBuilder},
    host::{HostSig, SigHash},
    program::StructTypeDef,
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
        panic!("tail tests do not dispatch a native host");
    }
}

fn bouncing(name: &str, outer: bool) -> (VerifiedProgram, FuncId) {
    let mut builder = ProgramBuilder::new();
    builder.set_program_name(name);
    let abi = FunctionSig {
        arg_types: vec![
            ValueType::I64,
            ValueType::Func,
            ValueType::Func,
            ValueType::Str,
        ],
        ret_types: vec![ValueType::Str],
    };
    let signature = builder.call_sig(abi.clone()).unwrap();
    let bounce = builder.declare_function(abi.clone()).unwrap();
    let mut body = builder.function_asm(bounce).unwrap();
    let zero = body.alloc_register(ValueType::I64);
    let one = body.alloc_register(ValueType::I64);
    let remaining = body.alloc_register(ValueType::I64);
    let again = body.alloc_register(ValueType::Bool);
    let empty = body.alloc_register(ValueType::Str);
    let copied = body.alloc_register(ValueType::Str);
    let empty_text = builder.const_str("");
    let next = body.label();
    let done = body.label();
    body.const_i64(zero, 0)
        .const_i64(one, 1)
        .const_pool(empty, empty_text)
        .i64_lt(again, zero, 1)
        .br(again, next, done);
    body.place(next).unwrap();
    body.i64_sub(remaining, 1, one)
        .str_concat(copied, 4, empty)
        .tail_call(signature, 2, &[remaining, 3, 2, copied]);
    body.place(done).unwrap();
    body.ret(0, &[4]);
    builder.define_function(bounce, body).unwrap();
    if outer {
        let entry = builder.declare_function(abi).unwrap();
        let mut body = builder.function_asm(entry).unwrap();
        let result = body.alloc_register(ValueType::Str);
        body.call(0, bounce, 0, &[1, 2, 3, 4], &[result])
            .ret(0, &[result]);
        builder.define_function(entry, body).unwrap();
        (builder.build_verified().unwrap(), entry)
    } else {
        (builder.build_verified().unwrap(), bounce)
    }
}

#[test]
fn long_cross_code_tail_chains_bound_frames_registers_and_owned_payloads() {
    for outer in [false, true] {
        let (left, entry) = bouncing("left", outer);
        let (right, bounce) = bouncing("right", false);
        let mut vm = Vm::new(
            NoHost,
            Limits {
                max_call_depth: if outer { 2 } else { 1 },
                ..Limits::default()
            },
        );
        let a = vm.instantiate(&left).unwrap();
        let b = vm.instantiate(&right).unwrap();
        let args = [
            Value::I64(1000),
            Value::Func(b.function(bounce).unwrap()),
            Value::Func(a.function(FuncId(0)).unwrap()),
            Value::Str("x".repeat(4096)),
        ];
        let mut run = a
            .start(&mut vm, entry, &args, InvocationOptions::default())
            .unwrap();
        while run.resume(100).unwrap() != RunStatus::Complete {
            assert!(run.inspect().frames().len() <= if outer { 2 } else { 1 });
            assert!(
                run.parents.len() <= usize::from(outer),
                "no forwarding segments"
            );
            let payload = core::iter::once(&run.continuation)
                .chain(&run.parents)
                .map(|stack| {
                    stack.ctx.arena.owned_payload_bytes()
                        + stack
                            .args
                            .iter()
                            .map(|value| match value {
                                Value::Str(text) => text.len(),
                                Value::Bytes(bytes) => bytes.len(),
                                _ => 0,
                            })
                            .sum::<usize>()
                })
                .sum::<usize>();
            assert!(
                payload <= 4 * 4096,
                "retired payloads must be released: {payload}"
            );
        }
        assert_eq!(
            run.result().unwrap().unwrap(),
            [Value::Str("x".repeat(4096))]
        );
    }
}

#[test]
fn reusable_root_outputs_preserve_nominal_prefixes_across_tail_replacement() {
    let (left, entry) = bouncing("left", false);
    let (right, bounce) = bouncing("right", false);
    let mut vm = Vm::new(
        NoHost,
        Limits {
            max_call_depth: 1,
            ..Limits::default()
        },
    );
    let a = vm.instantiate(&left).unwrap();
    let b = vm.instantiate(&right).unwrap();
    let prior = vm
        .aggregates_mut()
        .struct_new(crate::program::TypeId(99), vec![Value::I64(7)])
        .unwrap();
    let mut outputs = vec![Value::Agg(prior)];
    let mut scratch = ExecutionContext::default();
    for remaining in [1, 3] {
        let args = [
            Value::I64(remaining),
            Value::Func(b.function(bounce).unwrap()),
            Value::Func(a.function(FuncId(0)).unwrap()),
            Value::Str("preserved".into()),
        ];
        vm.run_with_ctx(
            &mut scratch,
            &left,
            entry,
            &args,
            &mut outputs,
            TraceMask::NONE,
            None,
            None,
        )
        .unwrap();
    }
    assert_eq!(
        outputs,
        [
            Value::Agg(prior),
            Value::Str("preserved".into()),
            Value::Str("preserved".into())
        ]
    );
    assert_eq!(vm.aggregates().struct_get(prior, 0).unwrap(), Value::I64(7));
}

#[test]
fn tail_resumption_can_replace_an_execution_root_without_a_forwarder() {
    let mut builder = ProgramBuilder::new();
    let abi = FunctionSig {
        arg_types: vec![],
        ret_types: vec![ValueType::I64],
    };
    let signature = builder.call_sig(abi.clone()).unwrap();
    let root = builder.declare_function(abi.clone()).unwrap();
    let leaf = builder.declare_function(abi).unwrap();
    let mut body = builder.function_asm(root).unwrap();
    let target = body.alloc_register(ValueType::Func);
    body.const_func(target, leaf);
    let continuation = builder
        .emit_continuation(&mut body, target, signature)
        .unwrap();
    body.tail_resume(signature, continuation, &[]);
    builder.define_function(root, body).unwrap();
    let mut body = builder.function_asm(leaf).unwrap();
    let answer = body.alloc_register(ValueType::I64);
    body.const_i64(answer, 42).ret(0, &[answer]);
    builder.define_function(leaf, body).unwrap();
    let program = builder.build_verified().unwrap();
    let mut vm = Vm::new(
        NoHost,
        Limits {
            max_call_depth: 1,
            ..Limits::default()
        },
    );
    assert_eq!(
        vm.run(&program, root, &[], TraceMask::NONE, None).unwrap(),
        [Value::I64(42)]
    );
}

#[test]
fn eliminated_return_edges_keep_nominal_checks_but_hide_private_closure_captures() {
    for encapsulated in [false, true] {
        let result_kind = if encapsulated {
            ValueType::Closure
        } else {
            ValueType::Agg
        };
        let takes_function = FunctionSig {
            arg_types: vec![ValueType::Func],
            ret_types: vec![result_kind],
        };
        let produces_value = FunctionSig {
            arg_types: vec![],
            ret_types: vec![result_kind],
        };
        let mut left = ProgramBuilder::new();
        let next = left.call_sig(takes_function.clone()).unwrap();
        let root = left.declare_function(takes_function.clone()).unwrap();
        let finish = left.declare_function(produces_value.clone()).unwrap();
        let body_function = left
            .declare_function(FunctionSig {
                arg_types: vec![ValueType::Agg],
                ret_types: vec![ValueType::I64],
            })
            .unwrap();
        let schema = left.struct_type(StructTypeDef {
            field_names: vec!["number".into()],
            field_types: vec![ValueType::I64],
        });
        let mut body = left.function_asm(root).unwrap();
        let target = body.alloc_register(ValueType::Func);
        body.const_func(target, finish)
            .tail_call(next, 1, &[target]);
        left.define_function(root, body).unwrap();
        let mut body = left.function_asm(finish).unwrap();
        let number = body.alloc_register(ValueType::I64);
        let aggregate = body.alloc_register(ValueType::Agg);
        body.const_i64(number, 42)
            .struct_new(aggregate, schema, &[number]);
        if encapsulated {
            let function = body.alloc_register(ValueType::Func);
            let closure = body.alloc_register(ValueType::Closure);
            body.const_func(function, body_function)
                .closure_new(closure, function, aggregate)
                .ret(0, &[closure]);
        } else {
            body.ret(0, &[aggregate]);
        }
        left.define_function(finish, body).unwrap();
        let mut body = left.function_asm(body_function).unwrap();
        let answer = body.alloc_register(ValueType::I64);
        body.const_i64(answer, 42).ret(0, &[answer]);
        left.define_function(body_function, body).unwrap();
        let left = left.build_verified().unwrap();
        let mut right = ProgramBuilder::new();
        let signature = right.call_sig(produces_value).unwrap();
        let middle = right.declare_function(takes_function).unwrap();
        let mut body = right.function_asm(middle).unwrap();
        body.tail_call(signature, 1, &[]);
        right.define_function(middle, body).unwrap();
        let right = right.build_verified().unwrap();
        let mut vm = Vm::new(
            NoHost,
            Limits {
                max_call_depth: 1,
                ..Limits::default()
            },
        );
        let a = vm.instantiate(&left).unwrap();
        let b = vm.instantiate(&right).unwrap();
        let args = [Value::Func(b.function(middle).unwrap())];
        {
            let mut run = a
                .start(&mut vm, root, &args, InvocationOptions::default())
                .unwrap();
            let result = loop {
                match run.resume(100) {
                    Ok(RunStatus::Suspended) => {}
                    result => break result,
                }
            };
            if encapsulated {
                assert_eq!(result.unwrap(), RunStatus::Complete);
                assert!(matches!(
                    run.result().unwrap().unwrap(),
                    [Value::Closure(_)]
                ));
            } else {
                assert!(matches!(
                    result.unwrap_err().trap,
                    Trap::CrossInstanceValue {
                        kind: ValueType::Agg
                    }
                ));
            }
        }
        assert_eq!(vm.aggregates().len_u32(), u32::from(encapsulated));
    }
}

#[test]
fn tail_calls_keep_the_enclosing_handler_and_tail_resume_retires_its_actual_frame() {
    let mut builder = ProgramBuilder::new();
    let abi = FunctionSig {
        arg_types: vec![],
        ret_types: vec![ValueType::I64],
    };
    let initial = builder.call_sig(abi.clone()).unwrap();
    let response = builder
        .call_sig(FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let operation = builder.control_tag("answer", abi.clone()).unwrap();
    let root = builder.declare_function(abi.clone()).unwrap();
    let worker = builder.declare_function(abi.clone()).unwrap();
    let leaf = builder.declare_function(abi).unwrap();
    let resume = builder
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::Continuation, ValueType::I64],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let pause = builder.host_sig_for(
        "pause",
        HostSig {
            args: vec![ValueType::I64],
            rets: vec![ValueType::I64],
        },
    );
    let mut body = builder.function_asm(root).unwrap();
    let target = body.alloc_register(ValueType::Func);
    let tag = body.alloc_register(ValueType::Tag);
    body.const_func(target, worker).const_tag(tag, operation);
    let computation = builder
        .emit_continuation(&mut body, target, initial)
        .unwrap();
    let returned = builder.result_block(&mut body, initial).unwrap();
    let clause = builder.handler_block(&mut body, tag, initial).unwrap();
    builder
        .emit_resume(
            &mut body,
            computation,
            initial,
            &[],
            returned.label(),
            &[clause.arm()],
        )
        .unwrap();
    body.place(clause.label()).unwrap();
    let answer = body.alloc_register(ValueType::I64);
    let result = body.alloc_register(ValueType::I64);
    body.const_i64(answer, 42)
        .call(0, resume, 0, &[clause.continuation(), answer], &[result])
        .ret(0, &[result]);
    body.place(returned.label()).unwrap();
    body.ret(0, returned.results());
    builder.define_function(root, body).unwrap();
    let mut body = builder.function_asm(worker).unwrap();
    let target = body.alloc_register(ValueType::Func);
    body.const_func(target, leaf)
        .tail_call(initial, target, &[]);
    builder.define_function(worker, body).unwrap();
    let mut body = builder.function_asm(leaf).unwrap();
    let tag = body.alloc_register(ValueType::Tag);
    body.const_tag(tag, operation);
    let answer = builder.emit_suspend(&mut body, tag, initial, &[]).unwrap();
    let [result] = builder.emit_call(&mut body, pause, &answer).unwrap();
    body.ret(0, &[result]);
    builder.define_function(leaf, body).unwrap();
    let mut body = builder.function_asm(resume).unwrap();
    body.tail_resume(response, 1, &[2]);
    builder.define_function(resume, body).unwrap();
    let program = builder.build_verified().unwrap();
    let mut vm = Vm::new(
        NoHost,
        Limits {
            max_call_depth: 3,
            max_continuations: 1,
            ..Limits::default()
        },
    );
    let mut run = vm
        .start(
            &program,
            root,
            &[],
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    let mut retired = None;
    loop {
        match run.resume(1).unwrap() {
            RunStatus::Suspended => {
                let inspection = run.inspect();
                if let Some(frame) = inspection.frames().next()
                    && frame.location.func == resume
                {
                    retired = Some(frame.activation);
                }
            }
            RunStatus::AwaitingHost => {
                assert_eq!(run.inspect().frames().len(), 2);
                assert_eq!(run.inspect().saved_continuations().len(), 0);
                let request = run.pending_host_call().unwrap();
                assert!(matches!(
                    request.return_from(
                        retired.as_ref().expect("observed retiring frame"),
                        vec![Value::I64(0)]
                    ),
                    Err(ControlError::ExpiredActivation)
                ));
                let request = run
                    .pending_host_call()
                    .expect("denial preserves the real host request");
                assert_eq!(request.symbol(), "pause");
                request.complete(&mut [Value::I64(42)], Ok(0)).unwrap();
            }
            RunStatus::Complete => break,
        }
    }
    assert_eq!(run.result().unwrap().unwrap(), [Value::I64(42)]);
}
#[test]
fn tail_chains_return_to_the_original_pending_native_request() {
    let (left, entry) = bouncing("left", false);
    let (right, bounce) = bouncing("right", false);
    let mut builder = ProgramBuilder::new();
    let root = builder
        .declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::Str],
        })
        .unwrap();
    let join = builder.host_sig_for(
        "join",
        HostSig {
            args: vec![],
            rets: vec![ValueType::Str],
        },
    );
    let mut body = builder.function_asm(root).unwrap();
    let [answer] = builder.emit_call(&mut body, join, &[]).unwrap();
    body.ret(0, &[answer]);
    builder.define_function(root, body).unwrap();
    let parent = builder.build_verified().unwrap();
    let mut vm = Vm::new(
        NoHost,
        Limits {
            max_call_depth: 2,
            ..Limits::default()
        },
    );
    let a = vm.instantiate(&left).unwrap();
    let b = vm.instantiate(&right).unwrap();
    let mut run = vm
        .start(
            &parent,
            root,
            &[],
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    assert_eq!(run.resume(100).unwrap(), RunStatus::AwaitingHost);
    let home = run.inspect().frames().next().unwrap().activation;
    run.pending_host_call()
        .unwrap()
        .call_guest(
            a.function(entry).unwrap(),
            vec![
                Value::I64(100),
                Value::Func(b.function(bounce).unwrap()),
                Value::Func(a.function(entry).unwrap()),
                Value::Str("done".into()),
            ],
        )
        .unwrap();
    loop {
        match run.resume(100).unwrap() {
            RunStatus::Suspended => {
                assert_eq!(run.inspect().frames().last().unwrap().activation, home);
            }
            RunStatus::AwaitingHost => break,
            RunStatus::Complete => {
                panic!("a callback cannot complete the original host request")
            }
        }
    }
    let mut request = run.pending_host_call().unwrap();
    assert_eq!(request.symbol(), "join");
    let mut returned = request.take_callback_results().unwrap();
    assert_eq!(returned, [Value::Str("done".into())]);
    request.complete(&mut returned, Ok(0)).unwrap();
    assert_eq!(run.resume(100).unwrap(), RunStatus::Complete);
    assert_eq!(run.result().unwrap().unwrap(), [Value::Str("done".into())]);
}

#[test]
fn a_failed_replacement_preserves_output_prefix_and_reusable_scratch() {
    let (left, entry) = bouncing("left", false);
    let (right, bounce) = bouncing("right", false);
    let mut vm = Vm::new(
        NoHost,
        Limits {
            fuel: 9,
            max_call_depth: 1,
            ..Limits::default()
        },
    );
    let a = vm.instantiate(&left).unwrap();
    let b = vm.instantiate(&right).unwrap();
    let prior = vm
        .aggregates_mut()
        .struct_new(crate::program::TypeId(99), vec![Value::I64(7)])
        .unwrap();
    let mut outputs = vec![Value::Agg(prior)];
    let mut scratch = ExecutionContext::default();
    let mut args = [
        Value::I64(3),
        Value::Func(b.function(bounce).unwrap()),
        Value::Func(a.function(entry).unwrap()),
        Value::Str("preserved".into()),
    ];
    let error = vm
        .run_with_ctx(
            &mut scratch,
            &left,
            entry,
            &args,
            &mut outputs,
            TraceMask::NONE,
            None,
            None,
        )
        .unwrap_err();
    assert!(matches!(error.trap, Trap::FuelExceeded));
    assert_eq!(
        scratch.registers.instance.as_ref(),
        Some(&b),
        "the failure happened after root replacement"
    );
    assert_eq!(outputs, [Value::Agg(prior)]);
    args[0] = Value::I64(0);
    vm.run_with_ctx(
        &mut scratch,
        &left,
        entry,
        &args,
        &mut outputs,
        TraceMask::NONE,
        None,
        None,
    )
    .unwrap();
    assert_eq!(outputs, [Value::Agg(prior), Value::Str("preserved".into())]);
}

#[test]
#[cfg(panic = "unwind")]
fn interrupted_tail_observation_cannot_retry_or_start_the_unentered_replacement() {
    use alloc::rc::Rc;
    use core::cell::RefCell;
    #[derive(Default)]
    struct Observer {
        starts: usize,
        ends: usize,
        tails: usize,
    }
    impl TraceSink for Observer {
        fn run_start(&mut self, _: TraceContext<'_>, _: FuncId, _: usize) {
            self.starts += 1;
        }
        fn run_end(&mut self, _: TraceContext<'_>, _: TraceOutcome<'_>) {
            self.ends += 1;
        }
        fn control(&mut self, _: TraceContext<'_>, event: ControlEvent<'_>) {
            if matches!(event, ControlEvent::TailTransfer { .. }) {
                self.tails += 1;
                panic!("injected tail observer failure");
            }
        }
    }
    for outer in [false, true] {
        let (left, entry) = bouncing("left", outer);
        let (right, bounce) = bouncing("right", false);
        let mut vm = Vm::new(
            NoHost,
            Limits {
                max_call_depth: if outer { 2 } else { 1 },
                ..Limits::default()
            },
        );
        let a = vm.instantiate(&left).unwrap();
        let b = vm.instantiate(&right).unwrap();
        let args = [
            Value::I64(100),
            Value::Func(b.function(bounce).unwrap()),
            Value::Func(a.function(FuncId(0)).unwrap()),
            Value::Str("owned".into()),
        ];
        let observer = Rc::new(RefCell::new(Observer::default()));
        let mut run = a
            .start(
                &mut vm,
                entry,
                &args,
                InvocationOptions::default()
                    .trace_owned(TraceMask::RUN | TraceMask::CONTROL, observer.clone()),
            )
            .unwrap();
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run.resume(100))).is_err()
        );
        for _ in 0..2 {
            assert!(matches!(
                run.resume(100).unwrap_err().trap,
                Trap::GuestTransferAbandoned
            ));
        }
        assert_eq!(run.budget.frames, 0);
        assert_eq!(
            observer.borrow().starts,
            1,
            "no replacement entry was observed"
        );
        assert_eq!(observer.borrow().tails, 1);
        assert_eq!(
            observer.borrow().ends,
            usize::from(outer),
            "unentered replacement has no fabricated run end"
        );
        drop(run);
        assert_eq!(observer.borrow().tails, 1);
    }
}
#[test]
fn tail_declarations_reject_bad_protocols_even_in_unreachable_code() {
    use crate::{
        asm::{BuildError, FunctionAbi},
        contract::Contract,
        program::CallSigId,
        verifier::VerifyError,
    };
    for resume in [false, true] {
        let mut builder = ProgramBuilder::new();
        let root = builder
            .declare_function(FunctionSig {
                arg_types: vec![if resume {
                    ValueType::Continuation
                } else {
                    ValueType::Func
                }],
                ret_types: vec![ValueType::I64],
            })
            .unwrap();
        let mut body = builder.function_asm(root).unwrap();
        if resume {
            body.tail_resume(CallSigId(999), 1, &[]);
        } else {
            body.tail_call(CallSigId(999), 1, &[]);
        }
        builder.define_function(root, body).unwrap();
        assert!(matches!(
            builder.build_verified(),
            Err(BuildError::Verify(VerifyError::CallSigOutOfBounds {
                call_sig: 999,
                ..
            }))
        ));
    }
    let mut builder = ProgramBuilder::new();
    let integer = builder.contract(Contract::Kind(ValueType::I64)).unwrap();
    let boolean = builder.contract(Contract::Kind(ValueType::Bool)).unwrap();
    let ints = builder.contract(Contract::Tuple(&[Some(integer)])).unwrap();
    let bools = builder.contract(Contract::Tuple(&[Some(boolean)])).unwrap();
    let incompatible = builder
        .call_sig(FunctionAbi {
            args: vec![],
            rets: vec![bools.into()],
        })
        .unwrap();
    let root = builder
        .declare_function(FunctionAbi {
            args: vec![ValueType::Func.into()],
            rets: vec![ints.into()],
        })
        .unwrap();
    let mut body = builder.function_asm(root).unwrap();
    let number = body.alloc_register(ValueType::I64);
    let tuple = body.alloc_register(ValueType::Agg);
    body.const_i64(number, 42)
        .tuple_new(tuple, &[number])
        .ret(0, &[tuple]);
    let unreachable_pc = body.pc();
    builder
        .emit_tail_call(&mut body, 1, incompatible, &[])
        .unwrap();
    builder.define_function(root, body).unwrap();
    assert!(
        matches!(builder.build_verified(), Err(BuildError::Verify(VerifyError::TailResultMismatch { pc, .. })) if pc == unreachable_pc)
    );
}

#[test]
fn a_known_tail_target_must_match_its_full_signature() {
    use crate::{asm::BuildError, verifier::VerifyError};
    let mut builder = ProgramBuilder::new();
    let integers = FunctionSig {
        arg_types: vec![],
        ret_types: vec![ValueType::I64],
    };
    let signature = builder.call_sig(integers.clone()).unwrap();
    let root = builder.declare_function(integers).unwrap();
    let target = builder
        .declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::Bool],
        })
        .unwrap();
    let mut body = builder.function_asm(root).unwrap();
    let callee = body.alloc_register(ValueType::Func);
    body.const_func(callee, target);
    builder
        .emit_tail_call(&mut body, callee, signature, &[])
        .unwrap();
    builder.define_function(root, body).unwrap();
    let mut body = builder.function_asm(target).unwrap();
    let answer = body.alloc_register(ValueType::Bool);
    body.const_bool(answer, true).ret(0, &[answer]);
    builder.define_function(target, body).unwrap();
    assert!(matches!(
        builder.build_verified(),
        Err(BuildError::Verify(
            VerifyError::CallIndirectContractMismatch { .. }
        ))
    ));
}
#[test]
fn run_only_observers_get_exact_stack_identities_without_instruction_tracing() {
    #[derive(Default)]
    struct Runs {
        starts: Vec<ActivationId>,
        ends: Vec<ActivationId>,
    }
    impl TraceSink for Runs {
        fn run_start(&mut self, context: TraceContext<'_>, _: FuncId, _: usize) {
            self.starts
                .push(context.stack().expect("VM run identity").clone());
        }
        fn run_end(&mut self, context: TraceContext<'_>, _: TraceOutcome<'_>) {
            self.ends
                .push(context.stack().expect("VM run identity").clone());
        }
    }
    let (program, entry) = bouncing("run-only", false);
    let mut vm = Vm::new(NoHost, Limits::default());
    let instance = vm.instantiate(&program).unwrap();
    let mut observer = Runs::default();
    let mut scratch = ExecutionContext::default();
    let mut outputs = Vec::new();
    let args = [
        Value::I64(0),
        Value::Func(instance.function(entry).unwrap()),
        Value::Func(instance.function(entry).unwrap()),
        Value::Str("done".into()),
    ];
    for _ in 0..2 {
        vm.run_with_ctx(
            &mut scratch,
            &program,
            entry,
            &args,
            &mut outputs,
            TraceMask::RUN,
            Some(&mut observer),
            None,
        )
        .unwrap();
    }
    assert_eq!(observer.starts, observer.ends);
    assert_eq!(observer.starts.len(), 2);
    assert_ne!(observer.starts[0], observer.starts[1]);
}
#[test]
fn encoded_tail_operands_roundtrip_and_disassemble_without_inventing_a_successor() {
    use crate::{
        disasm::{Operands, disassemble_function},
        opcode::Opcode,
        program::{CallSigId, Program},
    };
    for resume in [false, true] {
        let mut builder = ProgramBuilder::new();
        let signature = builder
            .call_sig(FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
            })
            .unwrap();
        let root = builder
            .declare_function(FunctionSig {
                arg_types: vec![if resume {
                    ValueType::Continuation
                } else {
                    ValueType::Func
                }],
                ret_types: vec![ValueType::I64],
            })
            .unwrap();
        let mut body = builder.function_asm(root).unwrap();
        if resume {
            builder
                .emit_tail_resume(&mut body, 1, signature, &[])
                .unwrap();
        } else {
            builder
                .emit_tail_call(&mut body, 1, signature, &[])
                .unwrap();
        }
        builder.define_function(root, body).unwrap();
        let program = builder.build_verified().unwrap();
        let bytes = program.program().encode();
        let decoded = Program::decode(&bytes).unwrap();
        assert_eq!(decoded.encode(), bytes);
        let disassembly = disassemble_function(&decoded, root).unwrap();
        let instruction = disassembly.instrs().next().unwrap();
        assert!(instruction.is_terminator());
        assert_eq!(
            instruction.opcode(),
            if resume {
                Opcode::TailResume
            } else {
                Opcode::TailCall
            }
        );
        assert!(matches!(
            instruction.operands(),
            Operands::Tail {
                target: 1,
                signature: CallSigId(0),
                args: [],
                ..
            }
        ));
        let display = alloc::format!("{instruction}");
        assert!(
            display.contains(if resume {
                "tail.resume r1, args=[]"
            } else {
                "tail.call r1, args=[]"
            }),
            "{display}"
        );
        assert!(display.contains("sig=[] -> [I64]"), "{display}");
    }
}
