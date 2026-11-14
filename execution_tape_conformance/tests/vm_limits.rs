// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `Vm::set_limits` changes the budgets of later runs without disturbing what the VM holds.

use std::sync::Arc;

use execution_tape::asm::{FunctionSig, ProgramBuilder};
use execution_tape::host::{Host, HostContext, HostError, SigHash, ValueRef};
use execution_tape::program::ValueType;
use execution_tape::trace::TraceMask;
use execution_tape::value::{FuncId, FuncRef, Value};
use execution_tape::verifier::VerifiedProgram;
use execution_tape::vm::{Limits, Trap, Vm};

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
        Err(HostError::UnknownSymbol)
    }
}

/// `f(x: i64) -> (x,)`: two instructions and one aggregate node with one field.
fn program() -> Arc<VerifiedProgram> {
    let mut builder = ProgramBuilder::new();
    let entry = builder
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::Agg],
        })
        .unwrap();
    let mut asm = builder.function_asm(entry).unwrap();
    let tuple = asm.alloc_register(ValueType::Agg);
    asm.tuple_new(tuple, &[1]).ret(0, &[tuple]);
    builder.define_function(entry, asm).unwrap();
    Arc::new(builder.build_verified().unwrap())
}

fn run(vm: &mut Vm<NoHost>, entry: FuncRef, x: i64) -> Result<Value, Trap> {
    vm.run(entry, &[Value::I64(x)], TraceMask::NONE, None)
        .map(|mut out| out.remove(0))
        .map_err(|info| info.trap)
}

#[test]
fn later_runs_use_the_new_limits_and_held_state_survives_lower_ones() {
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(program()).unwrap();
    let entry = FuncRef::new(unit, FuncId(0));
    let Value::Agg(kept) = run(&mut vm, entry, 7).unwrap() else {
        panic!("expected a tuple");
    };

    // Lower than the two instructions a run needs.
    vm.set_limits(Limits {
        fuel: 1,
        ..Limits::default()
    });
    assert_eq!(vm.limits().fuel, 1);
    assert!(matches!(run(&mut vm, entry, 8), Err(Trap::FuelExceeded)));

    // Lower than the aggregate work the retained tuple already cost: the tuple stays readable and
    // only new construction is refused.
    vm.set_limits(Limits {
        max_aggregate_work: 0,
        ..Limits::default()
    });
    assert!(matches!(
        run(&mut vm, entry, 9),
        Err(Trap::AggregateWorkExceeded {
            limit: 0,
            used: 0,
            requested: 2,
        })
    ));
    assert_eq!(vm.aggregates().tuple_get(kept, 0).unwrap(), Value::I64(7));
    assert_eq!(vm.units().len(), 1);

    // Raising them again needs no reinstall.
    vm.set_limits(Limits::default());
    let Value::Agg(fresh) = run(&mut vm, entry, 10).unwrap() else {
        panic!("expected a tuple");
    };
    assert_eq!(vm.aggregates().tuple_get(fresh, 0).unwrap(), Value::I64(10));
    assert_eq!(vm.aggregates().tuple_get(kept, 0).unwrap(), Value::I64(7));
}

#[test]
fn limits_set_on_a_parked_vm_move_with_replace_host() {
    let mut parked = Vm::new(NoHost, Limits::default());
    let unit = parked.install(program()).unwrap();
    let entry = FuncRef::new(unit, FuncId(0));
    parked.set_limits(Limits {
        fuel: 1,
        ..Limits::default()
    });

    let (mut vm, idle) = parked.replace_host(NoHost);
    assert_eq!(vm.limits().fuel, 1);
    assert!(matches!(run(&mut vm, entry, 1), Err(Trap::FuelExceeded)));
    vm.set_limits(Limits::default());
    let (parked, _) = vm.replace_host(idle);
    assert_eq!(parked.limits().fuel, Limits::default().fuel);
}
