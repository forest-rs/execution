// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use super::*;
use crate::{
    asm::{FunctionSig, ProgramBuilder},
    recording::{InstructionCapture, RecordLimits, Recorder},
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
        panic!("neither capture nor replay dispatches this host");
    }
}

#[test]
fn baseline_fallback_checks_contents_and_global_aliases_before_returning_a_value() {
    use crate::program::ValueType;
    let mut builder = ProgramBuilder::new();
    let entry = builder
        .declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::Agg],
        })
        .unwrap();
    let mut body = builder.function_asm(entry).unwrap();
    let number = body.alloc_register(ValueType::I64);
    let member = body.alloc_register(ValueType::Agg);
    let root = body.alloc_register(ValueType::Agg);
    body.const_i64(number, 42)
        .tuple_new(member, &[number])
        .tuple_new(root, &[member, member])
        .ret(0, &[root]);
    builder.define_function(entry, body).unwrap();
    let program = Arc::new(builder.build_verified().unwrap());
    let mut vm = Vm::new(NoHost, crate::vm::Limits::default());
    vm.aggregate_builder()
        .tuple_new(vec![Value::I64(-1)])
        .unwrap();
    let mut recorder = Recorder::new(program.clone(), RecordLimits::default());
    vm.run(&program, entry, &[], recorder.mask(), Some(&mut recorder))
        .unwrap();
    let mut record = recorder.finish();
    let root = record.arena.values[record.candidate.as_ref().unwrap().1.start].clone();
    // A later segment regards both nodes as preexisting. It has no local creation offsets
    // or established bindings, so materialization must use the execution-wide baseline.
    for node in &mut record.arena.nodes {
        node.created_offset = None;
    }
    for change in 0..3 {
        let mut heap = AggHeap::new();
        let member = heap
            .tuple_new(vec![Value::I64(if change == 1 { 99 } else { 42 })])
            .unwrap();
        let actual = heap
            .tuple_new(vec![Value::Agg(member), Value::Agg(member)])
            .unwrap();
        let mut identities = Identities {
            heap_base: record.heap_base,
            ..Identities::default()
        };
        if change == 2 {
            assert!(identities.bind(999, actual));
        }
        let mut state = State {
            global: Some(Rc::new(RefCell::new(identities))),
            origins: record.arena.nodes.iter().map(|node| node.origin).collect(),
            ..State::default()
        };
        assert!(state.bindings.is_empty());
        let result = state.materialize_value(&record, &root, &heap);
        if change == 0 {
            assert_eq!(result.unwrap(), Value::Agg(actual));
        } else {
            assert!(result.is_err(), "fallback must check corruption {change}");
        }
    }
}

fn capture(nonlocal: bool) -> Recording {
    let build = |parent: bool| {
        let mut builder = ProgramBuilder::new();
        builder.set_program_name(if parent { "caller" } else { "child" });
        let entry = builder
            .declare_function(FunctionSig {
                arg_types: vec![crate::program::ValueType::Agg],
                ret_types: vec![crate::program::ValueType::Agg],
            })
            .unwrap();
        let pause = builder.host_sig_for(
            "pause",
            crate::host::HostSig {
                args: vec![crate::program::ValueType::Agg],
                rets: vec![crate::program::ValueType::Agg],
            },
        );
        let mut body = builder.function_asm(entry).unwrap();
        let answer = if parent || nonlocal {
            let [answer] = builder.emit_call(&mut body, pause, &[1]).unwrap();
            answer
        } else {
            1
        };
        body.ret(0, &[answer]);
        builder.define_function(entry, body).unwrap();
        (builder.build_verified().unwrap(), entry)
    };
    let (parent, entry) = build(true);
    let (child, method) = build(false);
    let parent = Arc::new(parent);
    let mut vm = Vm::new(NoHost, crate::vm::Limits::default());
    let instance = vm.instantiate(&child).unwrap();
    let aggregate = vm
        .aggregate_builder()
        .tuple_new(vec![Value::I64(42)])
        .unwrap();
    let mut recorder = Recorder::new(parent.clone(), RecordLimits::default())
        .with_instructions(InstructionCapture::default());
    {
        let mask = recorder.mask();
        let args = [Value::Agg(aggregate)];
        let mut run = vm
            .start(
                &parent,
                entry,
                &args,
                InvocationOptions::default()
                    .yield_host_calls()
                    .trace(mask, &mut recorder),
            )
            .unwrap();
        let home = run.inspect().frames().next().unwrap().activation;
        run.resume(100).unwrap();
        run.pending_host_call()
            .unwrap()
            .call_guest(
                instance.function(method).expect("verified guest target"),
                vec![Value::Agg(aggregate)],
            )
            .unwrap();
        run.resume(100).unwrap();
        if nonlocal {
            run.pending_host_call()
                .unwrap()
                .return_from(&home, vec![Value::Agg(aggregate)])
                .unwrap();
        } else {
            let mut request = run.pending_host_call().unwrap();
            let mut values = request.take_callback_results().unwrap();
            request.complete(&mut values, Ok(0)).unwrap();
        }
        run.resume(100).unwrap();
    }
    let record = recorder.finish();
    assert_eq!(record.status(), Ok(()));
    record
}

#[test]
fn replay_rejects_a_child_arena_that_claims_a_different_shared_identity() {
    let mut record = capture(false);
    record.execution.as_mut().unwrap().children[0]
        .as_mut()
        .unwrap()
        .arena
        .nodes[0]
        .origin += 1;
    assert!(matches!(
        record.replay(),
        Err(ReplayError::InSegment { segment: 1, .. })
    ));
}

#[test]
fn replay_rejects_unresolved_targets_without_unwinding_a_different_activation() {
    let mut record = capture(true);
    let data = record.execution.as_mut().unwrap();
    let TransferAction::ReturnFrom { target, .. } = &mut data.transfers[1].action else {
        panic!("non-local command");
    };
    target.activation = u64::MAX;
    assert!(matches!(
        record.replay(),
        Err(ReplayError::InSegment { segment: 1, .. })
    ));
}

#[test]
fn replay_requires_actual_child_return_evidence() {
    let mut record = capture(false);
    record.execution.as_mut().unwrap().transfers.pop();
    assert!(
        record.replay().is_err(),
        "a recorded host result cannot stand in for guest execution"
    );
}

#[test]
fn replay_checks_ownership_and_ranges_before_reading_journal_values() {
    let mut record = capture(false);
    let TransferAction::Enter { args, .. } =
        &mut record.execution.as_mut().unwrap().transfers[0].action
    else {
        panic!("entry");
    };
    args.end = usize::MAX;
    assert!(record.replay().is_err());

    let mut record = capture(false);
    record.execution.as_mut().unwrap().children[0] = None;
    assert!(record.replay().is_err());

    let mut record = capture(false);
    record.execution.as_mut().unwrap().segments[1].program = Arc::clone(&record.program);
    assert!(
        record.replay().is_err(),
        "overlapping function IDs cannot replace a child's definition"
    );
}

#[test]
fn replay_restores_initial_nodes_observed_only_inside_a_child() {
    use crate::program::ValueType;
    let build = |parent: bool| {
        let mut builder = ProgramBuilder::new();
        let entry = builder
            .declare_function(FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
            })
            .unwrap();
        let pause = builder.host_sig_for(
            "pause",
            crate::host::HostSig {
                args: vec![],
                rets: vec![if parent {
                    ValueType::I64
                } else {
                    ValueType::Agg
                }],
            },
        );
        let mut body = builder.function_asm(entry).unwrap();
        let [answer] = builder.emit_call(&mut body, pause, &[]).unwrap();
        let answer = if parent {
            answer
        } else {
            let scalar = body.alloc_register(ValueType::I64);
            body.const_i64(scalar, 7);
            scalar
        };
        body.ret(0, &[answer]);
        builder.define_function(entry, body).unwrap();
        (builder.build_verified().unwrap(), entry)
    };
    let (parent, entry) = build(true);
    let (child, method) = build(false);
    let parent = Arc::new(parent);
    let mut vm = Vm::new(NoHost, crate::vm::Limits::default());
    let member = vm
        .aggregate_builder()
        .tuple_new(vec![Value::I64(42)])
        .unwrap();
    let hidden = vm
        .aggregate_builder()
        .tuple_new(vec![Value::Agg(member)])
        .unwrap();
    let instance = vm.instantiate(&child).unwrap();
    let mut recorder = Recorder::new(parent.clone(), RecordLimits::default());
    {
        let mask = recorder.mask();
        let mut run = vm
            .start(
                &parent,
                entry,
                &[],
                InvocationOptions::default()
                    .yield_host_calls()
                    .trace(mask, &mut recorder),
            )
            .unwrap();
        run.resume(100).unwrap();
        run.pending_host_call()
            .unwrap()
            .call_guest(
                instance.function(method).expect("verified guest target"),
                vec![],
            )
            .unwrap();
        run.resume(100).unwrap();
        run.pending_host_call()
            .unwrap()
            .complete(&mut [Value::Agg(hidden)], Ok(0))
            .unwrap();
        run.resume(100).unwrap();
        let mut request = run.pending_host_call().unwrap();
        let mut values = request.take_callback_results().unwrap();
        request.complete(&mut values, Ok(0)).unwrap();
        run.resume(100).unwrap();
    }
    let record = recorder.finish();
    assert!(
        record.arena.nodes.is_empty(),
        "the parent never observes the discarded aggregate"
    );
    assert_eq!(
        record.stats().aggregates,
        2,
        "only the child owns the initial graph evidence"
    );
    let ReplayOutcome::Returned(values) = record.replay().unwrap() else {
        panic!("actual scalar return");
    };
    assert_eq!(values.values(), &[Value::I64(7)]);
}
