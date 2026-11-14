// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `Vm::replace_host` lends installed code to a host that borrows per-call state.

use std::sync::Arc;

use execution_tape::asm::{FunctionSig, ProgramBuilder};
use execution_tape::host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef};
use execution_tape::program::ValueType;
use execution_tape::trace::TraceMask;
use execution_tape::value::{FuncId, FuncRef, Value};
use execution_tape::verifier::VerifiedProgram;
use execution_tape::vm::{Limits, Trap, Vm};

/// Appends each argument to a log it borrows.
struct Lent<'a>(&'a mut Vec<i64>);
impl Host for Lent<'_> {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        args: &[ValueRef<'_>],
        rets: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        let [ValueRef::I64(x)] = args else {
            return Err(HostError::Failed);
        };
        self.0.push(*x);
        rets[0] = Value::I64(i64::try_from(self.0.len()).unwrap());
        Ok(0)
    }
}

/// Rejects every call, counting them.
#[derive(Default)]
struct Parked(u32);
impl Host for Parked {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        _: &[ValueRef<'_>],
        _: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        self.0 += 1;
        Err(HostError::UnknownSymbol)
    }
}

/// `f(x: i64) -> (i64, (i64,))`: logs `x` and returns the log length and a tuple holding `x`.
fn program() -> Arc<VerifiedProgram> {
    let mut builder = ProgramBuilder::new();
    let entry = builder
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::I64, ValueType::Agg],
        })
        .unwrap();
    let log = builder.host_sig_for(
        "log",
        HostSig {
            args: vec![ValueType::I64],
            rets: vec![ValueType::I64],
        },
    );
    let mut asm = builder.function_asm(entry).unwrap();
    let len = asm.alloc_register(ValueType::I64);
    let tuple = asm.alloc_register(ValueType::Agg);
    asm.host_call_sig_id(0, log, 0, &[1], &[len])
        .tuple_new(tuple, &[1])
        .ret(0, &[len, tuple]);
    builder.define_function(entry, asm).unwrap();
    Arc::new(builder.build_verified().unwrap())
}

#[test]
fn units_and_aggregates_move_with_the_vm_and_the_old_host_comes_back() {
    let mut parked = Vm::new(Parked::default(), Limits::default());
    let unit = parked.install(program()).unwrap();
    let entry = FuncRef::new(unit, FuncId(0));

    // The parked host is really in place: a call reaches it and fails.
    let trap = parked
        .run(entry, &[Value::I64(1)], TraceMask::NONE, None)
        .unwrap_err();
    assert!(matches!(trap.trap, Trap::HostCallFailed { .. }), "{trap:?}");

    let mut first = Vec::new();
    let (mut vm, idle) = parked.replace_host(Lent(&mut first));
    assert_eq!(idle.0, 1);
    let out = vm
        .run(entry, &[Value::I64(7)], TraceMask::NONE, None)
        .unwrap();
    let Value::Agg(kept) = out[1] else {
        panic!("{out:?}");
    };
    let (parked, lent) = vm.replace_host(idle);
    assert_eq!(*lent.0, [7]);
    assert_eq!(first, [7]);

    // The same unit runs under the next borrow, and the aggregate from the last run is still
    // there.
    let mut second = vec![1, 2];
    let (mut vm, idle) = parked.replace_host(Lent(&mut second));
    let out = vm
        .run(entry, &[Value::I64(9)], TraceMask::NONE, None)
        .unwrap();
    assert_eq!(out[0], Value::I64(3));
    assert_eq!(vm.aggregates().tuple_get(kept, 0).unwrap(), Value::I64(7));
    assert_eq!(vm.units().len(), 1);
    let (_, lent) = vm.replace_host(idle);
    assert_eq!(*lent.0, [1, 2, 9]);
}
