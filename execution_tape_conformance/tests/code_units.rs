// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Installed code units: explicit installation, unit identity and entry validation.

use execution_tape::{
    asm::{FunctionSig, ProgramBuilder},
    callable::Callable,
    host::{Host, HostContext, HostError, SigHash, ValueRef},
    program::ValueType,
    trace::TraceMask,
    value::{FuncId, FuncRef, UnitId, Value},
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
