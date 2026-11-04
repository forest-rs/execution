// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use super::*;
use crate::{
    asm::{FunctionSig, ProgramBuilder},
    host::SigHash,
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
        panic!("rooted value fixtures do not call a host");
    }
}
fn empty(vm: &mut Vm<NoHost>) -> Instance {
    vm.instantiate(&ProgramBuilder::new().build_verified().unwrap())
        .unwrap()
}
fn aggregate(value: &Value) -> AggHandle {
    let Value::Agg(handle) = value else {
        panic!("expected aggregate")
    };
    *handle
}

#[test]
fn moving_collection_preserves_sharing_distinct_identity_and_owned_payloads() {
    let mut vm = Vm::new(NoHost, Limits::default());
    let instance = empty(&mut vm);
    vm.aggregate_builder().tuple_new(vec![]).unwrap(); // Unreachable filler forces relocation.
    let text = String::from("shared payload");
    let pointer = text.as_ptr();
    let child = vm
        .aggregate_builder()
        .tuple_new(vec![Value::Str(text)])
        .unwrap();
    let parent = vm
        .aggregate_builder()
        .tuple_new(vec![Value::Agg(child), Value::Agg(child)])
        .unwrap();
    let distinct = vm
        .aggregate_builder()
        .tuple_new(vec![Value::Str("shared payload".into())])
        .unwrap();
    let owned = String::from("owned root");
    let owned_pointer = owned.as_ptr();
    let roots = vm
        .retain_values(
            &instance,
            vec![Value::Agg(parent), Value::Agg(distinct), Value::Str(owned)],
        )
        .unwrap();
    let alias = roots.clone();
    assert!(matches!(vm.take_aggregates(), Err(RootError::HeapInUse)));
    assert_eq!(
        vm.collect().unwrap(),
        CollectionStats {
            live_groups: 1,
            released_groups: 0,
            nodes_before: 4,
            nodes_after: 3
        }
    );
    let view = vm.resolve(&roots).unwrap();
    let parent = aggregate(&view.values()[0]);
    let left = aggregate(view.aggregates().tuple_get_ref(parent, 0).unwrap());
    let right = aggregate(view.aggregates().tuple_get_ref(parent, 1).unwrap());
    assert_eq!(left, right);
    assert_ne!(
        left,
        aggregate(&view.values()[1]),
        "equal nodes remain distinct"
    );
    let Value::Str(text) = view.aggregates().tuple_get_ref(left, 0).unwrap() else {
        unreachable!()
    };
    assert_eq!(
        text.as_ptr(),
        pointer,
        "collection moves aggregate payloads"
    );
    let Value::Str(owned) = &view.values()[2] else {
        unreachable!()
    };
    assert_eq!(
        owned.as_ptr(),
        owned_pointer,
        "retention moves root payloads"
    );
    assert_eq!(view.instance(), &instance);
    drop(roots);
    assert_eq!(vm.collect().unwrap().live_groups, 1);
    assert_eq!(vm.resolve(&alias).unwrap().values().len(), 3);
    drop(alias);
    assert_eq!(
        vm.collect().unwrap(),
        CollectionStats {
            live_groups: 0,
            released_groups: 1,
            nodes_before: 3,
            nodes_after: 0
        }
    );
}

#[test]
fn replacement_histories_reclaim_obsolete_results() {
    let mut vm = Vm::new(NoHost, Limits::default());
    let instance = empty(&mut vm);
    let mut current = None;
    for revision in 0..1000 {
        let tuple = vm
            .aggregate_builder()
            .tuple_new(vec![Value::I64(revision)])
            .unwrap();
        current = Some(
            vm.retain_values(&instance, vec![Value::Agg(tuple)])
                .unwrap(),
        );
        let stats = vm.collect().unwrap();
        assert_eq!(stats.live_groups, 1);
        assert_eq!(stats.nodes_after, 1);
        let view = vm.resolve(current.as_ref().unwrap()).unwrap();
        assert_eq!(
            view.aggregates()
                .tuple_get(aggregate(&view.values()[0]), 0)
                .unwrap(),
            Value::I64(revision)
        );
        assert!(vm.roots.groups.len() <= 2, "registration slots are reused");
    }
    drop(current);
    assert_eq!(vm.collect().unwrap().nodes_after, 0);
}

#[test]
fn foreign_limit_and_invalid_value_denials_preserve_storage() {
    let mut vm = Vm::new(
        NoHost,
        Limits {
            max_value_roots: 1,
            ..Limits::default()
        },
    );
    let instance = empty(&mut vm);
    let mut other = Vm::new(NoHost, Limits::default());
    let foreign = empty(&mut other);
    assert!(matches!(
        vm.retain_values(&foreign, vec![Value::Agg(AggHandle(u32::MAX))]),
        Err(RootError::ForeignStore)
    ));
    assert!(
        vm.roots.owner.is_none(),
        "foreign denial creates no registration domain"
    );
    assert!(matches!(
        vm.retain_values(
            &instance,
            vec![Value::I64(1), Value::Agg(AggHandle(u32::MAX))]
        ),
        Err(RootError::InvalidValue { index: 1, .. })
    ));
    let tuple = vm
        .aggregate_builder()
        .tuple_new(vec![Value::I64(42)])
        .unwrap();
    let roots = vm
        .retain_values(&instance, vec![Value::Agg(tuple)])
        .unwrap();
    assert!(matches!(
        other.resolve(&roots),
        Err(RootError::ForeignStore)
    ));
    assert!(matches!(
        vm.retain_values(&instance, vec![]),
        Err(RootError::Limit { limit: 1, used: 1 })
    ));
    assert_eq!(vm.aggregates().len_u32(), 1);
    assert_eq!(vm.resolve(&roots).unwrap().values(), [Value::Agg(tuple)]);
    drop(roots);
    assert!(
        vm.retain_values(&instance, vec![]).is_ok(),
        "last-drop releases the allowance before collection"
    );
}

#[test]
fn nested_foreign_callables_are_rejected_at_the_offending_root() {
    let mut builder = ProgramBuilder::new();
    let function = builder
        .declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![],
        })
        .unwrap();
    let mut body = builder.function_asm(function).unwrap();
    body.ret(0, &[]);
    builder.define_function(function, body).unwrap();
    let program = builder.build_verified().unwrap();
    let mut vm = Vm::new(NoHost, Limits::default());
    let instance = vm.instantiate(&program).unwrap();
    let mut other = Vm::new(NoHost, Limits::default());
    let function = other.function(&program, function).unwrap();
    let nested = vm
        .aggregate_builder()
        .tuple_new(vec![Value::Func(function)])
        .unwrap();
    let error = vm
        .retain_values(&instance, vec![Value::I64(1), Value::Agg(nested)])
        .unwrap_err();
    assert!(
        matches!(error, RootError::InvalidValue { index: 1, source } if matches!(*source, Trap::ForeignStoreValue { kind: ValueType::Func }))
    );
    assert!(vm.roots.owner.is_none());
}

#[test]
fn retaining_outputs_uses_the_actual_tail_result_owner_and_preserves_denied_results() {
    let mut target = ProgramBuilder::new();
    target.set_program_name("replacement");
    let abi = FunctionSig {
        arg_types: vec![],
        ret_types: vec![ValueType::Str],
    };
    let entry = target.declare_function(abi.clone()).unwrap();
    let mut body = target.function_asm(entry).unwrap();
    let answer = body.alloc_register(ValueType::Str);
    let text = target.const_str("replacement result");
    body.const_pool(answer, text).ret(0, &[answer]);
    target.define_function(entry, body).unwrap();
    let target = target.build_verified().unwrap();
    let mut caller = ProgramBuilder::new();
    let signature = caller.call_sig(abi).unwrap();
    let root = caller
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::Func],
            ret_types: vec![ValueType::Str],
        })
        .unwrap();
    let mut body = caller.function_asm(root).unwrap();
    body.tail_call(signature, 1, &[]);
    caller.define_function(root, body).unwrap();
    let caller = caller.build_verified().unwrap();
    for limit in [0, 1] {
        let mut vm = Vm::new(
            NoHost,
            Limits {
                max_value_roots: limit,
                ..Limits::default()
            },
        );
        let target_function = vm.function(&target, entry).unwrap();
        let inputs = [Value::Func(target_function)];
        let roots = {
            let mut run = vm
                .start(&caller, root, &inputs, InvocationOptions::default())
                .unwrap();
            while run.resume(100).unwrap() != RunStatus::Complete {}
            if limit == 0 {
                assert!(matches!(
                    run.retain_outputs(),
                    Err(RootError::Limit { limit: 0, used: 0 })
                ));
                assert_eq!(
                    run.result().unwrap().unwrap(),
                    [Value::Str("replacement result".into())]
                );
                None
            } else {
                run.retain_outputs().unwrap()
            }
        };
        if let Some(roots) = roots {
            vm.collect().unwrap();
            let view = vm.resolve(&roots).unwrap();
            assert!(view.instance().program().shares_definition_with(&target));
            assert_eq!(view.values(), [Value::Str("replacement result".into())]);
        }
    }
}

#[test]
fn a_borrowed_root_view_does_not_need_to_borrow_its_handle() {
    let mut vm = Vm::new(NoHost, Limits::default());
    let instance = empty(&mut vm);
    let roots = vm.retain_values(&instance, vec![Value::I64(42)]).unwrap();
    let view = vm.resolve(&roots).unwrap();
    drop(roots);
    assert_eq!(
        view.values(),
        [Value::I64(42)],
        "the VM borrow prevents reclamation"
    );
    assert_eq!(vm.collect().unwrap().released_groups, 1);
}

#[test]
fn failed_execution_rolls_back_its_suffix_without_losing_older_roots() {
    let mut builder = ProgramBuilder::new();
    let entry = builder
        .declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![],
        })
        .unwrap();
    let mut body = builder.function_asm(entry).unwrap();
    let tuple = body.alloc_register(ValueType::Agg);
    body.tuple_new(tuple, &[]).trap(77);
    builder.define_function(entry, body).unwrap();
    let program = builder.build_verified().unwrap();
    let mut vm = Vm::new(NoHost, Limits::default());
    let instance = empty(&mut vm);
    let tuple = vm
        .aggregate_builder()
        .tuple_new(vec![Value::I64(42)])
        .unwrap();
    let roots = vm
        .retain_values(&instance, vec![Value::Agg(tuple)])
        .unwrap();
    assert!(vm.run(&program, entry, &[], TraceMask::NONE, None).is_err());
    assert_eq!(vm.aggregates().len_u32(), 1);
    assert_eq!(vm.collect().unwrap().nodes_after, 1);
    let view = vm.resolve(&roots).unwrap();
    assert_eq!(
        view.aggregates()
            .tuple_get(aggregate(&view.values()[0]), 0)
            .unwrap(),
        Value::I64(42)
    );
}

#[test]
fn independent_groups_share_storage_and_remap_closure_environments() {
    let mut builder = ProgramBuilder::new();
    let function = builder
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::Agg],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let mut body = builder.function_asm(function).unwrap();
    let answer = body.alloc_register(ValueType::I64);
    body.const_i64(answer, 42).ret(0, &[answer]);
    builder.define_function(function, body).unwrap();
    let program = builder.build_verified().unwrap();
    let mut vm = Vm::new(NoHost, Limits::default());
    let instance = vm.instantiate(&program).unwrap();
    vm.aggregate_builder().tuple_new(vec![]).unwrap();
    let shared = vm
        .aggregate_builder()
        .tuple_new(vec![Value::I64(42)])
        .unwrap();
    let parent = vm
        .aggregate_builder()
        .tuple_new(vec![Value::Agg(shared)])
        .unwrap();
    let ordinary = vm
        .retain_values(&instance, vec![Value::Agg(parent)])
        .unwrap();
    let callable = vm
        .retain_values(
            &instance,
            vec![Value::Closure(Closure {
                func: instance.function(function).unwrap(),
                env: shared,
            })],
        )
        .unwrap();
    assert_ne!(ordinary, callable);
    assert_eq!(vm.collect().unwrap().nodes_after, 2);
    let view = vm.resolve(&ordinary).unwrap();
    let shared = aggregate(
        view.aggregates()
            .tuple_get_ref(aggregate(&view.values()[0]), 0)
            .unwrap(),
    );
    let view = vm.resolve(&callable).unwrap();
    let Value::Closure(closure) = &view.values()[0] else {
        unreachable!()
    };
    assert_eq!(closure.env, shared);
    drop(ordinary);
    assert_eq!(
        vm.collect().unwrap(),
        CollectionStats {
            live_groups: 1,
            released_groups: 1,
            nodes_before: 2,
            nodes_after: 1
        }
    );
    let view = vm.resolve(&callable).unwrap();
    let Value::Closure(closure) = &view.values()[0] else {
        unreachable!()
    };
    assert_eq!(
        view.aggregates().tuple_get(closure.env, 0).unwrap(),
        Value::I64(42)
    );
    assert_eq!(closure.func.instance(), &instance);
    drop(callable);
    assert_eq!(vm.collect().unwrap().nodes_after, 0);
}
