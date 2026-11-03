// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Local clauses keep iterative handling in the resumer's own lexical frame.

use execution_tape::{
    asm::{FunctionSig, ProgramBuilder},
    host::{Host, HostContext, HostError, SigHash, ValueRef},
    program::ValueType,
    trace::TraceMask,
    value::Value,
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
        panic!("local control runs entirely in guest code");
    }
}

#[test]
fn repeated_effects_use_constant_handler_depth() {
    let mut program = ProgramBuilder::new();
    let produces_integer = FunctionSig {
        arg_types: vec![],
        ret_types: vec![ValueType::I64],
    };
    let initial = program.call_sig(produces_integer.clone()).unwrap();
    let operation = program
        .control_tag("next-number", produces_integer.clone())
        .unwrap();
    let response = program
        .call_sig(FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let root = program.declare_function(produces_integer.clone()).unwrap();
    let worker = program.declare_function(produces_integer).unwrap();

    let mut body = program.function_asm(worker).unwrap();
    let tag = body.alloc_register(ValueType::Tag);
    let remaining = body.alloc_register(ValueType::I64);
    let one = body.alloc_register(ValueType::I64);
    let zero = body.alloc_register(ValueType::I64);
    let again = body.alloc_register(ValueType::Bool);
    let loop_body = body.label();
    let done = body.label();
    body.const_tag(tag, operation)
        .const_i64(remaining, 10_000)
        .const_i64(one, 1)
        .const_i64(zero, 0);
    body.jmp(loop_body);
    body.place(loop_body).unwrap();
    let number = program.emit_suspend(&mut body, tag, initial, &[]).unwrap()[0];
    body.i64_sub(remaining, remaining, one)
        .i64_lt(again, zero, remaining);
    body.br(again, loop_body, done);
    body.place(done).unwrap();
    body.ret(0, &[number]);
    program.define_function(worker, body).unwrap();

    let mut body = program.function_asm(root).unwrap();
    let target = body.alloc_register(ValueType::Func);
    let tag = body.alloc_register(ValueType::Tag);
    let number = body.alloc_register(ValueType::I64);
    body.const_func(target, worker)
        .const_tag(tag, operation)
        .const_i64(number, 42);
    let computation = program
        .emit_continuation(&mut body, target, initial)
        .unwrap();
    let returned = program.result_block(&mut body, initial).unwrap();
    let next_number = program.handler_block(&mut body, tag, initial).unwrap();
    program
        .emit_resume(
            &mut body,
            computation,
            initial,
            &[],
            returned.label(),
            &[next_number.arm()],
        )
        .unwrap();
    body.place(next_number.label()).unwrap();
    // The clause can use lexical registers and reinstall itself without another call frame.
    program
        .emit_resume(
            &mut body,
            next_number.continuation(),
            response,
            &[number],
            returned.label(),
            &[next_number.arm()],
        )
        .unwrap();
    body.place(returned.label()).unwrap();
    body.ret(0, returned.results());
    program.define_function(root, body).unwrap();
    let program = program.build_verified().unwrap();

    let mut vm = Vm::new(
        NoHost,
        Limits {
            max_call_depth: 2,
            fuel: 1_000_000,
            ..Limits::default()
        },
    );
    assert_eq!(
        vm.run(&program, root, &[], TraceMask::NONE, None).unwrap(),
        [Value::I64(42)]
    );
}

#[test]
fn suspension_bindings_preserve_rich_contracts_and_report_only_the_selected_outputs() {
    use execution_tape::{
        asm::FunctionAbi,
        contract::Contract,
        trace::{RegisterEvent, RegisterPhase, TraceContext, TraceLocation, TraceSink},
        value::FuncId,
        verifier::{EdgeValueSource, ProgramAnalysis, ValueFact},
    };
    let mut program = ProgramBuilder::new();
    let integer = program.contract(Contract::Kind(ValueType::I64)).unwrap();
    let tuple = program.contract(Contract::Tuple(&[Some(integer)])).unwrap();
    let exchange = FunctionAbi {
        args: vec![tuple.into()],
        rets: vec![integer.into()],
    };
    let operation = program
        .control_tag("inspect-number", exchange.clone())
        .unwrap();
    let protocol = program.call_sig(exchange).unwrap();
    let initial = program
        .call_sig(FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let response = program
        .call_sig(FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let root = program
        .declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let worker = program
        .declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let mut body = program.function_asm(worker).unwrap();
    let tag = body.alloc_register(ValueType::Tag);
    let number = body.alloc_register(ValueType::I64);
    let request = body.alloc_register(ValueType::Agg);
    body.const_tag(tag, operation)
        .const_i64(number, 42)
        .tuple_new(request, &[number]);
    let answer = program
        .emit_suspend(&mut body, tag, protocol, &[request])
        .unwrap();
    let worker_return = body.pc();
    body.ret(0, &answer);
    program.define_function(worker, body).unwrap();

    let mut body = program.function_asm(root).unwrap();
    let tag = body.alloc_register(ValueType::Tag);
    let target = body.alloc_register(ValueType::Func);
    body.const_tag(tag, operation).const_func(target, worker);
    let computation = program
        .emit_continuation(&mut body, target, initial)
        .unwrap();
    let returned = program.result_block(&mut body, initial).unwrap();
    let clause = program.handler_block(&mut body, tag, protocol).unwrap();
    let first_resume = body.pc();
    program
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
    let projection = body.pc();
    let answer = body.alloc_register(ValueType::I64);
    body.tuple_get(answer, clause.request()[0], 0);
    let second_resume = body.pc();
    program
        .emit_resume(
            &mut body,
            clause.continuation(),
            response,
            &[answer],
            returned.label(),
            &[],
        )
        .unwrap();
    body.place(returned.label()).unwrap();
    let completion = body.pc();
    body.ret(0, returned.results());
    program.define_function(root, body).unwrap();
    let program = program.build_verified().unwrap();

    let mut analysis = ProgramAnalysis::from(&program).function(root).unwrap();
    let facts = analysis.instruction(projection).unwrap();
    let bindings: Vec<_> = facts.incoming_edges().next().unwrap().bindings().collect();
    assert_eq!(bindings[0].source, EdgeValueSource::Request(0));
    assert_eq!(bindings[1].source, EdgeValueSource::Continuation);
    let ValueFact::Contract(request) = bindings[0].value else {
        panic!("known request contract")
    };
    assert!(
        matches!(facts.contracts().get(request), Some(Contract::Tuple(members)) if members.len() == 1)
    );
    let ValueFact::Contract(continuation) = bindings[1].value else {
        panic!("known resumption contract")
    };
    assert!(
        matches!(facts.contracts().get(continuation), Some(Contract::Continuation { parameters, results })
        if parameters.len() == 1 && results.len() == 1
        && facts.contracts().get(parameters[0]) == Some(Contract::Kind(ValueType::I64))
        && facts.contracts().get(results[0]) == Some(Contract::Kind(ValueType::I64)))
    );
    let facts = analysis.instruction(completion).unwrap();
    for edge in facts.incoming_edges() {
        assert_eq!(
            edge.bindings().next().unwrap().source,
            EdgeValueSource::Result(0)
        );
    }

    struct Outputs {
        root: FuncId,
        sites: [u32; 2],
        observed: Vec<(u32, Vec<(u32, ValueType)>)>,
    }
    impl TraceSink for Outputs {
        fn wants_registers(
            &self,
            _: TraceContext<'_>,
            location: TraceLocation,
            phase: RegisterPhase,
        ) -> bool {
            phase == RegisterPhase::Outputs
                && location.func == self.root
                && self.sites.contains(&location.pc)
        }
        fn registers(&mut self, _: TraceContext<'_>, event: RegisterEvent<'_>) {
            self.observed.push((
                event.location.pc,
                event
                    .values
                    .iter()
                    .map(|(register, value)| {
                        (register, value.expect("bound parameter").value_type())
                    })
                    .collect(),
            ));
        }
    }
    let mut outputs = Outputs {
        root,
        sites: [first_resume, second_resume],
        observed: vec![],
    };
    let mut vm = Vm::new(NoHost, Limits::default());
    let mut saw_resumed_parent = false;
    {
        let mut run = vm
            .start(
                &program,
                root,
                &[],
                execution_tape::vm::InvocationOptions::default()
                    .trace(TraceMask::REGISTERS, &mut outputs),
            )
            .unwrap();
        while run.resume(1).unwrap() != execution_tape::vm::RunStatus::Complete {
            let inspection = run.inspect();
            for saved in inspection.saved_continuations() {
                if let Some(frame) = saved.frames().last() {
                    assert!(
                        frame.parent.is_none(),
                        "the matching resumer is outside the saved suffix"
                    );
                }
            }
            let mut frames = inspection.frames();
            if let Some(frame) = frames.next()
                && frame.location.func == worker
                && frame.location.pc == worker_return
            {
                let parent = frames.next().expect("actual resumer");
                assert_eq!(frame.parent, Some(parent.activation));
                assert_eq!(
                    frame.caller,
                    Some(TraceLocation {
                        func: root,
                        pc: second_resume
                    }),
                    "resumption ancestry follows the new resume site"
                );
                saw_resumed_parent = true;
            }
        }
        assert_eq!(run.result().unwrap().unwrap(), [Value::I64(42)]);
    }
    assert!(saw_resumed_parent);
    assert_eq!(
        outputs.observed,
        [
            (
                first_resume,
                vec![
                    (clause.request()[0], ValueType::Agg),
                    (clause.continuation(), ValueType::Continuation)
                ]
            ),
            (second_resume, vec![(returned.results()[0], ValueType::I64)]),
        ]
    );
}

#[test]
fn handler_edges_cannot_read_values_available_only_on_normal_return() {
    use execution_tape::{asm::BuildError, verifier::VerifyError};
    let mut program = ProgramBuilder::new();
    let signature = FunctionSig {
        arg_types: vec![],
        ret_types: vec![ValueType::I64],
    };
    let protocol = program.call_sig(signature.clone()).unwrap();
    let operation = program
        .control_tag("unused-operation", signature.clone())
        .unwrap();
    let root = program.declare_function(signature.clone()).unwrap();
    let worker = program.declare_function(signature).unwrap();
    let mut body = program.function_asm(worker).unwrap();
    let answer = body.alloc_register(ValueType::I64);
    body.const_i64(answer, 42).ret(0, &[answer]);
    program.define_function(worker, body).unwrap();
    let mut body = program.function_asm(root).unwrap();
    let target = body.alloc_register(ValueType::Func);
    let tag = body.alloc_register(ValueType::Tag);
    body.const_func(target, worker).const_tag(tag, operation);
    let computation = program
        .emit_continuation(&mut body, target, protocol)
        .unwrap();
    let returned = program.result_block(&mut body, protocol).unwrap();
    let clause = program.handler_block(&mut body, tag, protocol).unwrap();
    program
        .emit_resume(
            &mut body,
            computation,
            protocol,
            &[],
            returned.label(),
            &[clause.arm()],
        )
        .unwrap();
    body.place(clause.label()).unwrap();
    body.ret(0, returned.results());
    body.place(returned.label()).unwrap();
    body.ret(0, returned.results());
    program.define_function(root, body).unwrap();
    assert!(
        matches!(program.build_verified(), Err(BuildError::Verify(VerifyError::UninitializedRead { reg, .. })) if reg == returned.results()[0])
    );
}

#[test]
fn an_inner_nonmatching_table_survives_capture_and_matching_tables_are_outside_clauses() {
    let mut program = ProgramBuilder::new();
    let result = FunctionSig {
        arg_types: vec![],
        ret_types: vec![ValueType::I64],
    };
    let protocol = FunctionSig {
        arg_types: vec![ValueType::I64],
        ret_types: vec![ValueType::I64],
    };
    let initial = program.call_sig(result.clone()).unwrap();
    let exchange = program.call_sig(protocol.clone()).unwrap();
    let outer = program.control_tag("outer", protocol.clone()).unwrap();
    let inner = program.control_tag("inner", protocol).unwrap();
    let root = program.declare_function(result.clone()).unwrap();
    let middle = program.declare_function(result.clone()).unwrap();
    let worker = program.declare_function(result).unwrap();

    let mut body = program.function_asm(worker).unwrap();
    let a = body.alloc_register(ValueType::Tag);
    let b = body.alloc_register(ValueType::Tag);
    let request = body.alloc_register(ValueType::I64);
    let answer = body.alloc_register(ValueType::I64);
    body.const_tag(a, outer)
        .const_tag(b, inner)
        .const_i64(request, 1);
    let first = program
        .emit_suspend(&mut body, a, exchange, &[request])
        .unwrap()[0];
    body.const_i64(request, 2);
    let second = program
        .emit_suspend(&mut body, b, exchange, &[request])
        .unwrap()[0];
    body.i64_add(answer, first, second).ret(0, &[answer]);
    program.define_function(worker, body).unwrap();

    for (entry, callee, operation, increment) in
        [(root, middle, outer, 9), (middle, worker, inner, 18)]
    {
        let mut body = program.function_asm(entry).unwrap();
        let tag = body.alloc_register(ValueType::Tag);
        let target = body.alloc_register(ValueType::Func);
        let increment_register = body.alloc_register(ValueType::I64);
        let response = body.alloc_register(ValueType::I64);
        body.const_tag(tag, operation)
            .const_func(target, callee)
            .const_i64(increment_register, increment);
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
                &[],
                returned.label(),
                &[clause.arm()],
            )
            .unwrap();
        body.place(clause.label()).unwrap();

        // A's whole matching table is outside its clause. B's inner table is parked inside
        // the captured computation, so A cannot see it either. B becomes available on resume.
        let a = body.alloc_register(ValueType::Tag);
        let b = body.alloc_register(ValueType::Tag);
        let handled = body.alloc_register(ValueType::Bool);
        let check_inner = body.label();
        let respond = body.label();
        let invalid = body.label();
        body.const_tag(a, outer)
            .const_tag(b, inner)
            .tag_is_handled(handled, a);
        body.br(handled, invalid, check_inner);
        body.place(check_inner).unwrap();
        body.tag_is_handled(handled, b)
            .br(handled, invalid, respond);
        body.place(respond).unwrap();
        body.i64_add(response, clause.request()[0], increment_register);
        program
            .emit_resume(
                &mut body,
                clause.continuation(),
                exchange,
                &[response],
                returned.label(),
                &[],
            )
            .unwrap();
        body.place(invalid).unwrap();
        body.trap(91);
        body.place(returned.label()).unwrap();
        body.ret(0, returned.results());
        program.define_function(entry, body).unwrap();
    }
    let program = program.build_verified().unwrap();
    let mut vm = Vm::new(
        NoHost,
        Limits {
            max_call_depth: 3,
            ..Limits::default()
        },
    );
    assert_eq!(
        vm.run(&program, root, &[], TraceMask::NONE, None).unwrap(),
        [Value::I64(30)]
    );
}

#[test]
fn every_arm_of_a_wide_handler_table_has_its_parameter_kinds_verified() {
    use execution_tape::{
        asm::{BuildError, HandlerArm},
        verifier::{BlockEdgeError, VerifyError},
    };
    let mut program = ProgramBuilder::new();
    let signature = FunctionSig {
        arg_types: vec![],
        ret_types: vec![ValueType::I64],
    };
    let initial = program.call_sig(signature.clone()).unwrap();
    let root = program.declare_function(signature.clone()).unwrap();
    let worker = program.declare_function(signature.clone()).unwrap();
    let mut body = program.function_asm(worker).unwrap();
    let answer = body.alloc_register(ValueType::I64);
    body.const_i64(answer, 42).ret(0, &[answer]);
    program.define_function(worker, body).unwrap();
    let mut body = program.function_asm(root).unwrap();
    let target = body.alloc_register(ValueType::Func);
    let answer = body.alloc_register(ValueType::I64);
    body.const_func(target, worker).const_i64(answer, 42);
    let computation = program
        .emit_continuation(&mut body, target, initial)
        .unwrap();
    let returned = program.result_block(&mut body, initial).unwrap();
    let mut handlers = Vec::new();
    for index in 0..4 {
        let operation = program
            .control_tag(&format!("operation-{index}"), signature.clone())
            .unwrap();
        let tag = body.alloc_register(ValueType::Tag);
        body.const_tag(tag, operation);
        let parameter = body.alloc_register(if index == 3 {
            ValueType::Bool
        } else {
            ValueType::Continuation
        });
        handlers.push(HandlerArm::Suspend {
            tag,
            signature: initial,
            target: body.block(&[parameter]),
        });
    }
    // Use raw assembly to exercise verification; the signature-aware emitter rejects this first.
    body.cont_resume(computation, initial, &[], returned.label(), &handlers);
    for handler in handlers {
        let HandlerArm::Suspend { target, .. } = handler;
        body.place(target).unwrap();
        body.ret(0, &[answer]);
    }
    body.place(returned.label()).unwrap();
    body.ret(0, returned.results());
    program.define_function(root, body).unwrap();
    assert!(matches!(
        program.build_verified(),
        Err(BuildError::Verify(VerifyError::BlockEdge {
            arm: 4,
            error: BlockEdgeError::Type {
                expected: ValueType::Bool,
                actual: Some(ValueType::Continuation),
                ..
            },
            ..
        }))
    ));
}

#[test]
fn handler_protocols_are_checked_statically_when_known_and_before_entry_when_opaque() {
    use execution_tape::{
        asm::BuildError,
        control::ControlBoundary,
        verifier::VerifyError,
        vm::{InvocationOptions, Trap},
    };
    for known in [true, false] {
        let mut builder = ProgramBuilder::new();
        let signature = FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
        };
        let initial = builder.call_sig(signature.clone()).unwrap();
        let wrong_operation = builder
            .control_tag(
                "expects-boolean",
                FunctionSig {
                    arg_types: vec![],
                    ret_types: vec![ValueType::Bool],
                },
            )
            .unwrap();
        let worker = builder.declare_function(signature.clone()).unwrap();
        let root = builder
            .declare_function(FunctionSig {
                arg_types: if known { vec![] } else { vec![ValueType::Tag] },
                ret_types: signature.ret_types,
            })
            .unwrap();
        let mut body = builder.function_asm(worker).unwrap();
        // Any entry into the child is a bug: its delimiter must be validated first.
        body.trap(92);
        builder.define_function(worker, body).unwrap();
        let mut body = builder.function_asm(root).unwrap();
        let tag = if known {
            let tag = body.alloc_register(ValueType::Tag);
            body.const_tag(tag, wrong_operation);
            tag
        } else {
            builder.argument(root, 0).unwrap()
        };
        let target = body.alloc_register(ValueType::Func);
        body.const_func(target, worker);
        let computation = builder
            .emit_continuation(&mut body, target, initial)
            .unwrap();
        let returned = builder.result_block(&mut body, initial).unwrap();
        let clause = builder.handler_block(&mut body, tag, initial).unwrap();
        let resume_pc = body.pc();
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
        body.trap(93);
        body.place(returned.label()).unwrap();
        body.ret(0, returned.results());
        builder.define_function(root, body).unwrap();
        if known {
            assert!(matches!(builder.build_verified(), Err(BuildError::Verify(
                VerifyError::HandlerProtocolMismatch { func, pc, arm: 0, tag: actual_tag, signature }
            )) if func == root.0 && pc == resume_pc && actual_tag == tag && signature == initial));
        } else {
            let program = builder.build_verified().unwrap();
            let mut vm = Vm::new(NoHost, Limits::default());
            let instance = vm.instantiate(&program).unwrap();
            let args = [Value::Tag(instance.control_tag(wrong_operation).unwrap())];
            let mut turn = instance
                .start(&mut vm, root, &args, InvocationOptions::default())
                .unwrap();
            let error = loop {
                match turn.resume(u64::MAX) {
                    Ok(execution_tape::vm::RunStatus::Suspended) => continue,
                    Ok(status) => panic!("invalid protocol entered its child: {status:?}"),
                    Err(error) => break error,
                }
            };
            assert!(
                matches!(error.trap, Trap::ControlSignatureMismatch(ref mismatch)
                if mismatch.boundary() == ControlBoundary::Handler)
            );
            assert_eq!(error.pc, resume_pc);
        }
    }
}
