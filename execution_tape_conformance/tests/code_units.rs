// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Installed code units: explicit installation, unit identity and entry validation.

use execution_tape::{
    aggregates::AggShape,
    asm::{FunctionAbi, FunctionSig, ProgramBuilder},
    callable::Callable,
    contract::{Contract, ContractMismatch, ContractViolation},
    host::{Host, HostContext, HostError, SigHash, ValueRef},
    program::{StructTypeDef, ValueType},
    trace::TraceMask,
    value::{AggType, FuncId, FuncRef, UnitId, Value},
    verifier::VerifiedProgram,
    vm::{InvocationOptions, Limits, Trap, Vm},
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
        panic!("no host calls");
    }
}

fn constant(number: i64) -> Arc<VerifiedProgram> {
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
    Arc::new(b.build_verified().unwrap())
}

#[test]
fn every_install_creates_a_distinct_retained_unit() {
    let program = constant(7);
    let weak = Arc::downgrade(&program);
    let mut vm = Vm::new(NoHost, Limits::default());
    assert!(vm.units().is_empty());
    let first = vm.install(Arc::clone(&program)).unwrap();
    let second = vm.install(program).unwrap();
    assert_ne!(first, second, "reinstalling shared code is a new unit");
    let units = vm.units();
    assert_eq!(units.len(), 2);
    assert!(Arc::ptr_eq(
        units.program(first).unwrap(),
        units.program(second).unwrap()
    ));
    assert_eq!(
        units.iter().map(|(unit, _)| unit).collect::<Vec<_>>(),
        [first, second]
    );
    for unit in [first, second] {
        assert_eq!(
            vm.run(FuncRef::new(unit, FuncId(0)), &[], TraceMask::NONE, None)
                .unwrap(),
            [Value::I64(7)]
        );
    }
    assert!(weak.upgrade().is_some(), "the VM retains installed code");
    drop(vm);
    assert!(weak.upgrade().is_none());
}

#[test]
fn entry_rejects_units_this_vm_did_not_install() {
    let mut other = Vm::new(NoHost, Limits::default());
    let foreign = other.install(constant(1)).unwrap();
    let foreign = other.install(constant(2)).unwrap().max(foreign);
    let mut vm = Vm::new(NoHost, Limits::default());
    vm.install(constant(3)).unwrap();
    let entry = FuncRef::new(foreign, FuncId(0));
    let error = vm.run(entry, &[], TraceMask::NONE, None).unwrap_err();
    assert!(matches!(error.trap, Trap::UnknownUnit { unit } if unit == foreign));
    let Err(error) = vm.start(entry, &[], InvocationOptions::default()) else {
        panic!("unknown units cannot start");
    };
    assert!(matches!(error.trap, Trap::UnknownUnit { unit } if unit == UnitId(1)));
}

#[test]
fn callables_start_only_in_a_unit_of_their_exact_program() {
    let program = constant(5);
    let callable = Callable::function(Arc::clone(&program), FuncId(0)).unwrap();
    let mut vm = Vm::new(NoHost, Limits::default());
    let equal_copy = vm
        .install(Arc::new(VerifiedProgram::clone(&program)))
        .unwrap();
    let Err(error) = callable.start(&mut vm, equal_copy, vec![], InvocationOptions::default())
    else {
        panic!("an equal copy is different code");
    };
    assert!(matches!(error.trap, Trap::UnitProgramMismatch { unit } if unit == equal_copy));
    let unit = vm.install(program).unwrap();
    let mut call = callable
        .start(&mut vm, unit, vec![], InvocationOptions::default())
        .unwrap();
    call.resume(u64::MAX).unwrap();
    assert_eq!(call.unit(), unit);
    assert_eq!(call.take_outputs(), Some(vec![Value::I64(5)]));
}

fn point(field: &str) -> StructTypeDef {
    StructTypeDef {
        field_names: vec![field.into()],
        field_types: vec![ValueType::I64],
    }
}

/// A unit whose `make` builds a one-field struct and whose `check` accepts only that struct.
fn struct_unit(field: &str) -> (Arc<VerifiedProgram>, FuncId, FuncId) {
    let mut b = ProgramBuilder::new();
    let ty = b.struct_type(point(field));
    let integer = b.contract(Contract::Kind(ValueType::I64)).unwrap();
    let shape = b
        .contract(Contract::Struct {
            type_id: ty,
            fields: &[integer],
        })
        .unwrap();
    let make = b
        .declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::Agg],
        })
        .unwrap();
    let mut a = b.function_asm(make).unwrap();
    let value = a.alloc_register(ValueType::I64);
    let result = a.alloc_register(ValueType::Agg);
    a.const_i64(value, 7)
        .struct_new(result, ty, &[value])
        .ret(0, &[result]);
    b.define_function(make, a).unwrap();
    let check = b
        .declare_function(FunctionAbi {
            args: vec![shape.into()],
            rets: vec![ValueType::I64.into()],
        })
        .unwrap();
    let mut a = b.function_asm(check).unwrap();
    let field = a.alloc_register(ValueType::I64);
    a.struct_get(field, 1, 0).ret(0, &[field]);
    b.define_function(check, a).unwrap();
    (Arc::new(b.build_verified().unwrap()), make, check)
}

#[test]
fn equal_struct_shapes_from_independent_units_are_one_heap_shape() {
    let (first, make, _) = struct_unit("x");
    let (second, _, check) = struct_unit("x");
    let mut vm = Vm::new(NoHost, Limits::default());
    let first = vm.install(first).unwrap();
    let second = vm.install(second).unwrap();
    assert_eq!(vm.aggregates().shape_count(), 1, "one interned shape");
    let made = vm
        .run(FuncRef::new(first, make), &[], TraceMask::NONE, None)
        .unwrap();
    let [Value::Agg(handle)] = made[..] else {
        panic!("struct result");
    };
    let AggType::Struct { shape } = vm.aggregates().agg_type(handle).unwrap() else {
        panic!("struct");
    };
    assert_eq!(
        vm.aggregates().shape(shape),
        Some(&AggShape::Struct(point("x")))
    );
    assert_eq!(
        vm.run(FuncRef::new(second, check), &made, TraceMask::NONE, None)
            .unwrap(),
        [Value::I64(7)]
    );
    assert!(vm.units().metadata_bytes() > 0);
}

#[test]
fn colliding_local_type_ids_with_different_shapes_do_not_match() {
    // Both programs call their only struct type `TypeId(0)`, but the shapes differ.
    let (first, make, _) = struct_unit("x");
    let (second, _, check) = struct_unit("y");
    let mut vm = Vm::new(NoHost, Limits::default());
    let first = vm.install(first).unwrap();
    let second = vm.install(second).unwrap();
    assert_eq!(vm.aggregates().shape_count(), 2);
    let made = vm
        .run(FuncRef::new(first, make), &[], TraceMask::NONE, None)
        .unwrap();
    let error = vm
        .run(FuncRef::new(second, check), &made, TraceMask::NONE, None)
        .unwrap_err();
    assert!(matches!(
        error.trap,
        Trap::ContractMismatch {
            error: ContractViolation {
                mismatch: ContractMismatch::Aggregate { .. },
                ..
            },
            ..
        }
    ));
}
