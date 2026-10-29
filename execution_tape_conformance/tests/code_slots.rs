// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Existing activations and escaped callables keep their meaning across lookup replacement.

use execution_tape::{
    asm::{FunctionAbi, FunctionSig, ProgramBuilder},
    callable::{
        Callable, CodeSlot, CodeUpdate, CodeUpdateError, SignatureDifference, SignaturePath,
    },
    contract::Contract,
    host::{Host, HostContext, HostError, SigHash, ValueRef},
    program::{StructTypeDef, ValueType},
    value::Value,
    vm::{InvocationOptions, Limits, RunStatus, Vm},
};
use std::{collections::HashSet, sync::Arc};

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
        panic!("no host calls");
    }
}
fn constant(number: i64) -> Callable {
    let mut b = ProgramBuilder::new();
    let entry = b
        .declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let mut a = b.function_asm(entry).unwrap();
    let value = a.alloc_register(ValueType::I64);
    a.const_i64(value, number).ret(0, &[value]);
    b.define_function(entry, a).unwrap();
    Callable::function(Arc::new(b.build_verified().unwrap()), entry).unwrap()
}
fn run(callable: &Callable) -> Vec<Value> {
    let mut vm = Vm::new(NoHost, Limits::default());
    let mut invocation = callable
        .start(&mut vm, vec![], InvocationOptions::default())
        .unwrap();
    invocation.resume(u64::MAX).unwrap();
    invocation.take_outputs().unwrap()
}

#[test]
fn lookup_update_does_not_patch_a_paused_frame_or_a_retained_old_target() {
    let mut slot = CodeSlot::new(constant(11));
    let old = slot.current();
    let mut vm = Vm::new(NoHost, Limits::default());
    // The lookup snapshot is temporary; the invocation itself retains the old code.
    let mut paused = slot
        .current()
        .callable()
        .start(&mut vm, vec![], InvocationOptions::default())
        .unwrap();
    assert_eq!(paused.resume(1).unwrap(), RunStatus::Suspended);
    let update = slot.replace(constant(99)).unwrap();
    let CodeUpdate::Replaced { previous, current } = update else {
        panic!("expected new publication");
    };
    assert_eq!(previous, old);
    assert_eq!(previous.revision(), 0);
    assert_eq!(current.revision(), 1);
    assert!(old.same_slot(&current));
    assert_ne!(old, current);
    assert_eq!(run(slot.current().callable()), [Value::I64(99)]);
    assert_eq!(paused.resume(u64::MAX).unwrap(), RunStatus::Complete);
    assert_eq!(paused.result().unwrap().unwrap(), [Value::I64(11)]);
    assert_eq!(run(old.callable()), [Value::I64(11)]);
}

#[test]
fn publication_identity_and_no_ops_are_scoped_and_explicit() {
    let target = constant(42);
    let mut slot = CodeSlot::new(target.clone());
    let initial = slot.current();
    let CodeUpdate::Unchanged(same) = slot.replace(target.clone()).unwrap() else {
        panic!("expected no-op");
    };
    assert_eq!(same, initial);
    assert_eq!(slot.current().revision(), 0);
    let another = CodeSlot::new(target).current();
    assert_eq!(initial.revision(), another.revision());
    assert_ne!(initial, another);
    assert!(!initial.same_slot(&another));
    let mut publications = HashSet::new();
    publications.insert(initial.clone());
    publications.insert(initial.clone());
    assert_eq!(publications.len(), 1);
    // Equal code bytes in a separate preparation do not establish code identity.
    let CodeUpdate::Replaced { current, .. } = slot.replace(constant(42)).unwrap() else {
        panic!("different owner needs a publication");
    };
    publications.insert(current);
    assert_eq!(publications.len(), 2);
}

#[test]
fn old_versions_retain_only_their_code_without_retaining_a_publication_history_chain() {
    let original = constant(11);
    let weak = Arc::downgrade(original.program());
    let mut slot = CodeSlot::new(original);
    let old = slot.current();
    slot.replace(constant(99)).unwrap();
    assert!(weak.upgrade().is_some());
    drop(old);
    assert!(
        weak.upgrade().is_none(),
        "the current publication must not retain its predecessor"
    );
    assert_eq!(run(slot.current().callable()), [Value::I64(99)]);
}

#[test]
fn rejected_updates_return_both_owners_and_a_recoverable_signature_difference() {
    let mut slot = CodeSlot::new(constant(11));
    let old = slot.current();
    let mut b = ProgramBuilder::new();
    let entry = b
        .declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::Bool],
        })
        .unwrap();
    let mut a = b.function_asm(entry).unwrap();
    let value = a.alloc_register(ValueType::Bool);
    a.const_bool(value, true).ret(0, &[value]);
    b.define_function(entry, a).unwrap();
    let proposed = Callable::function(Arc::new(b.build_verified().unwrap()), entry).unwrap();
    let error = slot.replace(proposed.clone()).unwrap_err();
    let CodeUpdateError::Signature {
        current,
        proposed: rejected,
        difference,
    } = &error
    else {
        panic!("expected signature failure");
    };
    assert_eq!(current, &old);
    assert_eq!(rejected, &proposed);
    let SignatureDifference::Contract {
        path,
        expected: Some(expected),
        proposed: Some(actual),
    } = difference
    else {
        panic!("expected scalar mismatch");
    };
    assert_eq!(path, &[SignaturePath::Result(0)]);
    assert_eq!(
        current
            .callable()
            .program()
            .program()
            .contracts
            .kind(*expected),
        Some(ValueType::I64)
    );
    assert_eq!(
        rejected.program().program().contracts.kind(*actual),
        Some(ValueType::Bool)
    );
    assert_eq!(slot.current(), old);
    let text = error.to_string();
    assert!(text.contains("result[0]"));
    assert!(text.contains("expected I64, got Bool"));
    assert_eq!(run(slot.current().callable()), [Value::I64(11)]);
    slot.replace(constant(99)).unwrap();
    assert_eq!(run(slot.current().callable()), [Value::I64(99)]);
}

fn shared_signature(depth: usize, boolean: bool, extra_contract: bool) -> Callable {
    let mut b = ProgramBuilder::new();
    if extra_contract {
        b.contract(Contract::Kind(ValueType::Bytes)).unwrap();
    }
    let mut contract = b
        .contract(Contract::Kind(if boolean {
            ValueType::Bool
        } else {
            ValueType::I64
        }))
        .unwrap();
    for _ in 0..depth {
        contract = b
            .contract(Contract::Tuple(&[Some(contract), Some(contract)]))
            .unwrap();
    }
    let entry = b
        .declare_function(FunctionAbi {
            args: vec![contract.into()],
            rets: vec![],
        })
        .unwrap();
    let mut a = b.function_asm(entry).unwrap();
    a.ret(0, &[]);
    b.define_function(entry, a).unwrap();
    Callable::function(Arc::new(b.build_verified().unwrap()), entry).unwrap()
}

#[test]
fn signature_comparison_is_iterative_and_does_not_expand_shared_graphs_into_trees() {
    let depth = 2048; // Expanded shape would contain 2^2048 leaves.
    let mut slot = CodeSlot::new(shared_signature(depth, false, false));
    let replacement = shared_signature(depth, false, true);
    assert_ne!(
        slot.current().callable().signature().arg_contracts(),
        replacement.signature().arg_contracts()
    );
    slot.replace(replacement).unwrap();
    let old = slot.current();
    let error = slot
        .replace(shared_signature(depth, true, true))
        .unwrap_err();
    let CodeUpdateError::Signature {
        difference: SignatureDifference::Contract { path, .. },
        ..
    } = error
    else {
        panic!("expected nested leaf mismatch");
    };
    assert_eq!(path.len(), depth + 1);
    assert_eq!(path[0], SignaturePath::Argument(0));
    assert!(path[1..].iter().all(|p| *p == SignaturePath::Member(0)));
    assert_eq!(slot.current(), old);
}

fn nominal_signature() -> Callable {
    let mut b = ProgramBuilder::new();
    let integer = b.contract(Contract::Kind(ValueType::I64)).unwrap();
    let nominal = b.struct_type(StructTypeDef {
        field_names: vec!["value".into()],
        field_types: vec![ValueType::I64],
    });
    let contract = b
        .contract(Contract::Struct {
            type_id: nominal,
            fields: &[integer],
        })
        .unwrap();
    let entry = b
        .declare_function(FunctionAbi {
            args: vec![contract.into()],
            rets: vec![],
        })
        .unwrap();
    let mut a = b.function_asm(entry).unwrap();
    a.ret(0, &[]);
    b.define_function(entry, a).unwrap();
    Callable::function(Arc::new(b.build_verified().unwrap()), entry).unwrap()
}

#[test]
fn matching_nominal_numbers_and_shapes_do_not_conflate_different_code_owners() {
    let mut slot = CodeSlot::new(nominal_signature());
    let old = slot.current();
    let replacement = nominal_signature();
    assert_eq!(
        old.callable().signature().arg_contracts(),
        replacement.signature().arg_contracts()
    );
    let error = slot.replace(replacement).unwrap_err();
    assert!(
        matches!(error, CodeUpdateError::Signature { difference: SignatureDifference::ProgramIdentity { ref path, .. }, .. } if path == &[SignaturePath::Argument(0)])
    );
    assert!(error.to_string().contains("different prepared program"));
    assert_eq!(slot.current(), old);
}

#[test]
fn exposed_signature_compatibility_hides_each_closures_concrete_environment_layout() {
    use execution_tape::{aggregates::AggHeap, value::Closure};
    let mut b = ProgramBuilder::new();
    let integer = b.contract(Contract::Kind(ValueType::I64)).unwrap();
    let boolean = b.contract(Contract::Kind(ValueType::Bool)).unwrap();
    let mut functions = Vec::new();
    for (member, answer) in [(integer, 11), (boolean, 99)] {
        let env = b.contract(Contract::Tuple(&[Some(member)])).unwrap();
        let body = b
            .declare_function(FunctionAbi {
                args: vec![env.into()],
                rets: vec![integer.into()],
            })
            .unwrap();
        let mut a = b.function_asm(body).unwrap();
        let value = a.alloc_register(ValueType::I64);
        a.const_i64(value, answer).ret(0, &[value]);
        b.define_function(body, a).unwrap();
        functions.push(body);
    }
    let program = Arc::new(b.build_verified().unwrap());
    let mut heap = AggHeap::new();
    let first = heap.tuple_new(vec![Value::I64(4)]).unwrap();
    let second = heap.tuple_new(vec![Value::Bool(true)]).unwrap();
    let old = Callable::retain(
        Arc::clone(&program),
        &heap,
        ValueRef::Closure(Closure {
            func: functions[0],
            env: first,
        }),
    )
    .unwrap();
    let new = Callable::retain(
        program,
        &heap,
        ValueRef::Closure(Closure {
            func: functions[1],
            env: second,
        }),
    )
    .unwrap();
    let mut slot = CodeSlot::new(old);
    let previous = slot.current();
    slot.replace(new).unwrap();
    assert_eq!(run(previous.callable()), [Value::I64(11)]);
    assert_eq!(run(slot.current().callable()), [Value::I64(99)]);
}

fn with_input(
    build: impl FnOnce(&mut ProgramBuilder) -> execution_tape::contract::ContractId,
) -> Callable {
    let mut b = ProgramBuilder::new();
    let input = build(&mut b);
    let entry = b
        .declare_function(FunctionAbi {
            args: vec![input.into()],
            rets: vec![],
        })
        .unwrap();
    let mut a = b.function_asm(entry).unwrap();
    a.ret(0, &[]);
    b.define_function(entry, a).unwrap();
    Callable::function(Arc::new(b.build_verified().unwrap()), entry).unwrap()
}

#[test]
fn higher_order_signature_counts_and_unknown_members_have_precise_paths() {
    let higher = |arguments: usize, results: usize| {
        with_input(|b| {
            let i = b.contract(Contract::Kind(ValueType::I64)).unwrap();
            b.contract(Contract::Closure {
                args: &vec![i; arguments],
                rets: &vec![i; results],
            })
            .unwrap()
        })
    };
    let mut slot = CodeSlot::new(higher(1, 1));
    for (arguments, results) in [(2, 1), (1, 2)] {
        let error = slot.replace(higher(arguments, results)).unwrap_err();
        let CodeUpdateError::Signature { difference, .. } = error else {
            panic!("signature expected");
        };
        match difference {
            SignatureDifference::ArgumentCount {
                path,
                expected: 1,
                proposed: 2,
            } if arguments == 2 => assert_eq!(path, [SignaturePath::Argument(0)]),
            SignatureDifference::ResultCount {
                path,
                expected: 1,
                proposed: 2,
            } if results == 2 => assert_eq!(path, [SignaturePath::Argument(0)]),
            _ => panic!("wrong higher-order signature diagnostic"),
        }
    }
    let tuple = |known: bool, members: usize| {
        with_input(|b| {
            let i = b.contract(Contract::Kind(ValueType::I64)).unwrap();
            b.contract(Contract::Tuple(&vec![known.then_some(i); members]))
                .unwrap()
        })
    };
    let mut slot = CodeSlot::new(tuple(false, 1));
    let error = slot.replace(tuple(true, 1)).unwrap_err();
    assert!(error.to_string().contains("no member guarantee"));
    assert!(
        matches!(error, CodeUpdateError::Signature { difference: SignatureDifference::Contract { ref path, expected: None, proposed: Some(_), .. }, .. } if path == &[SignaturePath::Argument(0), SignaturePath::Member(0)])
    );
    let error = slot.replace(tuple(false, 2)).unwrap_err();
    assert!(
        matches!(error, CodeUpdateError::Signature { difference: SignatureDifference::MemberCount { ref path, expected: 1, proposed: 2 }, .. } if path == &[SignaturePath::Argument(0)])
    );
}

#[test]
fn array_nominal_identity_is_program_local_even_for_primitive_element_types() {
    let target = || {
        with_input(|b| {
            let i = b.contract(Contract::Kind(ValueType::I64)).unwrap();
            let element = b.array_elem(ValueType::I64);
            b.contract(Contract::Array {
                elem_type_id: element,
                element: i,
            })
            .unwrap()
        })
    };
    let mut slot = CodeSlot::new(target());
    let old = slot.current();
    assert!(matches!(
        slot.replace(target()),
        Err(CodeUpdateError::Signature {
            difference: SignatureDifference::ProgramIdentity { .. },
            ..
        })
    ));
    assert_eq!(slot.current(), old);
}

#[test]
fn same_owner_nominal_refinements_are_compared_below_the_nominal_boundary() {
    for array in [false, true] {
        let mut b = ProgramBuilder::new();
        let integer = b.contract(Contract::Kind(ValueType::I64)).unwrap();
        let boolean = b.contract(Contract::Kind(ValueType::Bool)).unwrap();
        let nominal = b.struct_type(StructTypeDef {
            field_names: vec!["item".into()],
            field_types: vec![ValueType::Agg],
        });
        let element = b.array_elem(ValueType::Agg);
        let mut functions = Vec::new();
        for leaf in [integer, boolean] {
            let tuple = b.contract(Contract::Tuple(&[Some(leaf)])).unwrap();
            let contract = if array {
                b.contract(Contract::Array {
                    elem_type_id: element,
                    element: tuple,
                })
                .unwrap()
            } else {
                b.contract(Contract::Struct {
                    type_id: nominal,
                    fields: &[tuple],
                })
                .unwrap()
            };
            let body = b
                .declare_function(FunctionAbi {
                    args: vec![contract.into()],
                    rets: vec![],
                })
                .unwrap();
            let mut a = b.function_asm(body).unwrap();
            a.ret(0, &[]);
            b.define_function(body, a).unwrap();
            functions.push(body);
        }
        let program = Arc::new(b.build_verified().unwrap());
        let mut slot =
            CodeSlot::new(Callable::function(Arc::clone(&program), functions[0]).unwrap());
        let old = slot.current();
        let error = slot
            .replace(Callable::function(program, functions[1]).unwrap())
            .unwrap_err();
        let CodeUpdateError::Signature {
            difference: SignatureDifference::Contract { path, .. },
            ..
        } = error
        else {
            panic!("expected refined member mismatch");
        };
        assert_eq!(
            path,
            [
                SignaturePath::Argument(0),
                if array {
                    SignaturePath::Element
                } else {
                    SignaturePath::Member(0)
                },
                SignaturePath::Member(0)
            ]
        );
        assert_eq!(slot.current(), old);
    }
}

#[test]
fn root_arity_changes_are_rejected_before_publication() {
    let mut slot = CodeSlot::new(constant(11));
    let old = slot.current();
    let input = with_input(|b| b.contract(Contract::Kind(ValueType::I64)).unwrap());
    assert!(
        matches!(slot.replace(input), Err(CodeUpdateError::Signature { difference: SignatureDifference::ArgumentCount { ref path, expected: 0, proposed: 1 }, .. }) if path.is_empty())
    );
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
    let proposed =
        Callable::function(Arc::new(builder.build_verified().unwrap()), function).unwrap();
    assert!(
        matches!(slot.replace(proposed), Err(CodeUpdateError::Signature { difference: SignatureDifference::ResultCount { ref path, expected: 1, proposed: 0 }, .. }) if path.is_empty())
    );
    assert_eq!(slot.current(), old);
}
