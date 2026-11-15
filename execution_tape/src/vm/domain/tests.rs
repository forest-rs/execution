// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use super::*;
use crate::{
    asm::{FunctionSig, ProgramBuilder},
    host::{HostSig, SigHash},
};
use alloc::borrow::Cow;

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
        panic!("yielded fixture");
    }
}

fn paused(vm: &mut Vm<NoHost>) -> Chain<'static> {
    let mut builder = ProgramBuilder::new();
    let pause = builder.host_sig_for(
        "pause",
        HostSig {
            args: vec![],
            rets: vec![],
        },
    );
    let entry = builder
        .declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::Str, ValueType::Bytes, ValueType::Str],
        })
        .unwrap();
    let mut body = builder.function_asm(entry).unwrap();
    let text = body.alloc_register(ValueType::Str);
    let bytes = body.alloc_register(ValueType::Bytes);
    let tuple = body.alloc_register(ValueType::Agg);
    let projected_text = body.alloc_register(ValueType::Str);
    let projected_bytes = body.alloc_register(ValueType::Bytes);
    let alias = body.alloc_register(ValueType::Str);
    let computed = body.alloc_register(ValueType::Str);
    let empty = body.alloc_register(ValueType::Str);
    body.alloc_register(ValueType::Agg); // Uninitialized reference slots must not root entry zero.
    let text_source = builder.const_str("source");
    let bytes_source = builder.const_bytes(&[1, 2, 3]);
    let empty_source = builder.const_str("");
    body.const_pool(text, text_source)
        .const_pool(bytes, bytes_source)
        .tuple_new(tuple, &[text, bytes])
        .tuple_get(projected_text, tuple, 0)
        .tuple_get(projected_bytes, tuple, 1)
        .mov(alias, projected_text)
        .tuple_new(tuple, &[])
        .const_pool(empty, empty_source);
    for _ in 0..3 {
        body.str_concat(computed, projected_text, empty);
    }
    body.host_call_sig_id(0, pause, 0, &[], &[])
        .ret(0, &[alias, projected_bytes, computed]);
    builder.define_function(entry, body).unwrap();
    let program = builder.build_verified().unwrap();
    let instance = vm.instantiate(&program).unwrap();
    let mut stack = continuation::Continuation::new(
        continuation::CodeOwner::Instance(instance),
        entry,
        Cow::Owned(vec![]),
        TraceMask::NONE,
        ExecutionControl::start(true),
    );
    let mut budget = Budget::new(&vm.limits);
    stack
        .initialize(
            vm,
            &mut budget,
            DataWork::default(),
            &mut Observers::default(),
        )
        .unwrap();
    assert!(matches!(
        stack.advance(vm, &mut budget, 100),
        Ok(Progress::AwaitingHost)
    ));
    Chain {
        continuation: stack,
        parents: vec![],
    }
}

#[test]
fn parked_payload_sources_and_aliases_survive_collection_without_retaining_obsolete_payloads() {
    let mut vm = Vm::new(NoHost, Limits::default());
    vm.aggregate_builder().tuple_new(vec![]).unwrap();
    let chain = paused(&mut vm);
    let stack = &chain.continuation;
    let sources = ValueSources::new(stack.program.program(), &stack.args, vm.aggregates());
    let base = stack.ctx.frames[0].base;
    let source_pointer = stack
        .ctx
        .read_str(&StrReg(1), base, sources)
        .unwrap()
        .as_ptr();
    let owned_pointer = stack
        .ctx
        .read_str(&StrReg(3), base, sources)
        .unwrap()
        .as_ptr();
    assert_eq!(stack.ctx.arena.owned_payload_bytes(), 3 * "source".len());
    vm.domain.as_mut().unwrap().paused = Some(chain.into_owned());
    assert!(matches!(vm.take_aggregates(), Err(RootError::HeapInUse)));
    let counts = vm.collect().unwrap();
    assert_eq!((counts.nodes_before, counts.nodes_after), (3, 2));
    let stack = &vm
        .domain
        .as_ref()
        .unwrap()
        .paused
        .as_ref()
        .unwrap()
        .continuation;
    let sources = ValueSources::new(stack.program.program(), &stack.args, vm.aggregates());
    assert_eq!(stack.ctx.aggs, [AggHandle(1), Registers::UNINITIALIZED_AGG]);
    assert_eq!(
        stack
            .ctx
            .read_str(&StrReg(1), base, sources)
            .unwrap()
            .as_ptr(),
        source_pointer
    );
    assert_eq!(
        stack
            .ctx
            .read_str(&StrReg(2), base, sources)
            .unwrap()
            .as_ptr(),
        source_pointer
    );
    assert_eq!(
        stack
            .ctx
            .read_str(&StrReg(3), base, sources)
            .unwrap()
            .as_ptr(),
        owned_pointer
    );
    assert_eq!(
        stack.ctx.read_bytes(&BytesReg(1), base, sources).unwrap(),
        [1, 2, 3]
    );
    assert_eq!(stack.ctx.arena.owned_payload_bytes(), "source".len());

    let mut chain = vm.domain.as_mut().unwrap().paused.take().unwrap();
    let mut budget = Budget::new(&vm.limits);
    budget.frames = 1;
    let checkpoint = vm.memory.checkpoint();
    invocation::Running {
        vm: &mut vm,
        continuation: &mut chain.continuation,
        parents: &mut chain.parents,
        observers: &mut Observers::default(),
        budget: &mut budget,
        checkpoint,
        next_segment: &mut 1,
        resumptions: &mut Resumptions::default(),
        epoch: 0,
        defer_publication: false,
    }
    .pending_host_call()
    .unwrap()
    .complete(&mut [], Ok(0))
    .unwrap();
    assert!(matches!(
        chain.continuation.advance(&mut vm, &mut budget, 100),
        Ok(Progress::Returned(_))
    ));
    assert_eq!(
        chain.continuation.outputs,
        [
            Value::Str("source".into()),
            Value::Bytes(vec![1, 2, 3]),
            Value::Str("source".into())
        ]
    );
}

#[test]
fn collection_planning_failure_does_not_move_heap_values_or_other_stacks() {
    let mut vm = Vm::new(NoHost, Limits::default());
    vm.aggregate_builder().tuple_new(vec![]).unwrap();
    let mut chain = paused(&mut vm);
    let original = chain.continuation.ctx.aggs.clone();
    chain.continuation.ctx.strs[0] = StrHandle(77);
    vm.domain.as_mut().unwrap().paused = Some(chain);
    assert!(matches!(
        vm.collect(),
        Err(RootError::Collection(AggError::BadHandle))
    ));
    assert_eq!(vm.aggregates().len_u32(), 3);
    assert_eq!(
        vm.domain
            .as_ref()
            .unwrap()
            .paused
            .as_ref()
            .unwrap()
            .continuation
            .ctx
            .aggs,
        original
    );
}
