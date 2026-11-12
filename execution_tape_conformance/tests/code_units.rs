// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Installed code units: explicit installation, unit identity and entry validation.

use execution_tape::{
    aggregates::AggShape,
    asm::{CallTarget, Catch, FunctionAbi, FunctionSig, ProgramBuilder},
    callable::Callable,
    contract::{Contract, ContractMismatch, ContractViolation},
    host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef},
    program::{StructTypeDef, ValueType},
    recording::{CaptureFault, RecordLimits, Recorder},
    trace::{TraceMask, TraceSink},
    value::{AggType, Closure, FuncId, FuncRef, UnitId, Value},
    verifier::VerifiedProgram,
    vm::{InvocationOptions, Limits, RunStatus, Trap, Vm},
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

fn sig(args: &[ValueType], rets: &[ValueType]) -> FunctionSig {
    FunctionSig {
        arg_types: args.to_vec(),
        ret_types: rets.to_vec(),
    }
}

/// A unit whose only function adds `amount` to its argument.
fn adds(amount: i64) -> Arc<VerifiedProgram> {
    let mut b = ProgramBuilder::new();
    let f = b
        .declare_function(sig(&[ValueType::I64], &[ValueType::I64]))
        .unwrap();
    let mut a = b.function_asm(f).unwrap();
    let x = b.argument(f, 0).unwrap();
    let k = a.alloc_register(ValueType::I64);
    a.const_i64(k, amount).i64_add(x, x, k).ret(0, &[x]);
    b.define_function(f, a).unwrap();
    Arc::new(b.build_verified().unwrap())
}

/// A unit whose `apply(f, x)` calls a function value with `x`; `kind` is `Func` or `Closure`.
fn applies(kind: ValueType) -> Arc<VerifiedProgram> {
    let mut b = ProgramBuilder::new();
    let signature = b
        .call_sig(sig(&[ValueType::I64], &[ValueType::I64]))
        .unwrap();
    let f = b
        .declare_function(sig(&[kind, ValueType::I64], &[ValueType::I64]))
        .unwrap();
    let mut a = b.function_asm(f).unwrap();
    let callee = b.argument(f, 0).unwrap();
    let x = b.argument(f, 1).unwrap();
    let [result] = b
        .emit_call(&mut a, CallTarget::Indirect { signature, callee }, &[x])
        .unwrap();
    a.ret(0, &[result]);
    b.define_function(f, a).unwrap();
    Arc::new(b.build_verified().unwrap())
}

#[test]
fn function_values_size_stays_small() {
    assert_eq!(size_of::<FuncRef>(), 8);
    assert_eq!(size_of::<Closure>(), 12);
    // `Value`'s size is pinned for the 64-bit layout; 32-bit targets lay it out smaller.
    #[cfg(target_pointer_width = "64")]
    assert_eq!(size_of::<Value>(), 32);
}

#[test]
fn a_function_value_runs_its_own_unit_from_another_unit() {
    let mut vm = Vm::new(NoHost, Limits::default());
    let caller = vm.install(applies(ValueType::Func)).unwrap();
    let callee = vm.install(adds(1)).unwrap();
    let target = Value::Func(FuncRef::new(callee, FuncId(0)));
    assert_eq!(
        vm.run(
            FuncRef::new(caller, FuncId(0)),
            &[target, Value::I64(41)],
            TraceMask::NONE,
            None
        )
        .unwrap(),
        [Value::I64(42)]
    );
    // The callee's own unit runs it, not the caller's: `FuncId(0)` of the caller is `apply`.
    let missing = Value::Func(FuncRef::new(UnitId(9), FuncId(0)));
    let error = vm
        .run(
            FuncRef::new(caller, FuncId(0)),
            &[missing, Value::I64(1)],
            TraceMask::NONE,
            None,
        )
        .unwrap_err();
    assert!(matches!(error.trap, Trap::UnknownUnit { unit } if unit == UnitId(9)));
    assert_eq!(error.unit, caller, "the call site reports the trap");
}

#[test]
fn closures_keep_their_body_unit_and_environment_across_units() {
    let mut b = ProgramBuilder::new();
    let integer = b.contract(Contract::Kind(ValueType::I64)).unwrap();
    let captured = b.contract(Contract::Tuple(&[Some(integer)])).unwrap();
    let body = b
        .declare_function(FunctionAbi {
            args: vec![captured.into(), ValueType::I64.into()],
            rets: vec![ValueType::I64.into()],
        })
        .unwrap();
    let mut a = b.function_asm(body).unwrap();
    let env = b.argument(body, 0).unwrap();
    let x = b.argument(body, 1).unwrap();
    let k = a.alloc_register(ValueType::I64);
    a.tuple_get(k, env, 0).i64_add(x, x, k).ret(0, &[x]);
    b.define_function(body, a).unwrap();
    let make = b
        .declare_function(sig(&[ValueType::I64], &[ValueType::Closure]))
        .unwrap();
    let mut a = b.function_asm(make).unwrap();
    let k = b.argument(make, 0).unwrap();
    let (function, env, closure) = (
        a.alloc_register(ValueType::Func),
        a.alloc_register(ValueType::Agg),
        a.alloc_register(ValueType::Closure),
    );
    a.const_func(function, body)
        .tuple_new(env, &[k])
        .closure_new(closure, function, env)
        .ret(0, &[closure]);
    b.define_function(make, a).unwrap();
    let maker = Arc::new(b.build_verified().unwrap());

    let mut vm = Vm::new(NoHost, Limits::default());
    let caller = vm.install(applies(ValueType::Closure)).unwrap();
    let maker = vm.install(maker).unwrap();
    let made = vm
        .run(
            FuncRef::new(maker, make),
            &[Value::I64(10)],
            TraceMask::NONE,
            None,
        )
        .unwrap();
    let [Value::Closure(closure)] = made[..] else {
        panic!("closure result");
    };
    assert_eq!(closure.func, FuncRef::new(maker, body));
    assert_eq!(
        vm.run(
            FuncRef::new(caller, FuncId(0)),
            &[Value::Closure(closure), Value::I64(1)],
            TraceMask::NONE,
            None
        )
        .unwrap(),
        [Value::I64(11)]
    );
}

#[test]
fn indirect_calls_check_signatures_across_units() {
    let mut b = ProgramBuilder::new();
    let f = b
        .declare_function(sig(&[ValueType::I64], &[ValueType::Bool]))
        .unwrap();
    let mut a = b.function_asm(f).unwrap();
    let flag = a.alloc_register(ValueType::Bool);
    a.const_bool(flag, true).ret(0, &[flag]);
    b.define_function(f, a).unwrap();
    let mut vm = Vm::new(NoHost, Limits::default());
    let caller = vm.install(applies(ValueType::Func)).unwrap();
    let wrong = vm.install(b.build_verified().unwrap()).unwrap();
    let error = vm
        .run(
            FuncRef::new(caller, FuncId(0)),
            &[Value::Func(FuncRef::new(wrong, f)), Value::I64(1)],
            TraceMask::NONE,
            None,
        )
        .unwrap_err();
    assert!(matches!(
        error.trap,
        Trap::TypeMismatch {
            expected: ValueType::I64,
            actual: ValueType::Bool
        }
    ));

    // Equal storage kinds but different contracts: the exact signature must match.
    let mut b = ProgramBuilder::new();
    let signature = b
        .call_sig(FunctionAbi {
            args: vec![ValueType::Agg.into()],
            rets: vec![ValueType::Agg.into()],
        })
        .unwrap();
    let f = b
        .declare_function(sig(&[ValueType::Func, ValueType::Agg], &[ValueType::Agg]))
        .unwrap();
    let mut a = b.function_asm(f).unwrap();
    let callee = b.argument(f, 0).unwrap();
    let x = b.argument(f, 1).unwrap();
    let [result] = b
        .emit_call(&mut a, CallTarget::Indirect { signature, callee }, &[x])
        .unwrap();
    a.ret(0, &[result]);
    b.define_function(f, a).unwrap();
    let opaque = vm.install(b.build_verified().unwrap()).unwrap();
    let mut b = ProgramBuilder::new();
    let integer = b.contract(Contract::Kind(ValueType::I64)).unwrap();
    let boxed = b.contract(Contract::Tuple(&[Some(integer)])).unwrap();
    let identity = b
        .declare_function(FunctionAbi {
            args: vec![boxed.into()],
            rets: vec![boxed.into()],
        })
        .unwrap();
    let mut a = b.function_asm(identity).unwrap();
    a.ret(0, &[1]);
    b.define_function(identity, a).unwrap();
    let refined = vm.install(b.build_verified().unwrap()).unwrap();
    let tuple = vm.aggregates_mut().tuple_new(vec![Value::I64(1)]).unwrap();
    let target = FuncRef::new(refined, identity);
    let error = vm
        .run(
            FuncRef::new(opaque, f),
            &[Value::Func(target), Value::Agg(tuple)],
            TraceMask::NONE,
            None,
        )
        .unwrap_err();
    assert!(matches!(
        error.trap,
        Trap::CallableSignatureMismatch { function, .. } if function == target
    ));
}

/// A caller unit whose `main(x)` asks the host for a function twice and calls each with `x`.
fn dispatcher() -> Arc<VerifiedProgram> {
    let mut b = ProgramBuilder::new();
    let lookup = b.host_sig_for(
        "lookup",
        HostSig {
            args: vec![],
            rets: vec![ValueType::Func],
        },
    );
    let signature = b
        .call_sig(sig(&[ValueType::I64], &[ValueType::I64]))
        .unwrap();
    let main = b
        .declare_function(sig(&[ValueType::I64], &[ValueType::Agg]))
        .unwrap();
    let mut a = b.function_asm(main).unwrap();
    let x = b.argument(main, 0).unwrap();
    let [first] = b.emit_call(&mut a, lookup, &[]).unwrap();
    let [one] = b
        .emit_call(
            &mut a,
            CallTarget::Indirect {
                signature,
                callee: first,
            },
            &[x],
        )
        .unwrap();
    let [second] = b.emit_call(&mut a, lookup, &[]).unwrap();
    let [two] = b
        .emit_call(
            &mut a,
            CallTarget::Indirect {
                signature,
                callee: second,
            },
            &[x],
        )
        .unwrap();
    let pair = a.alloc_register(ValueType::Agg);
    a.tuple_new(pair, &[one, two]).ret(0, &[pair]);
    b.define_function(main, a).unwrap();
    Arc::new(b.build_verified().unwrap())
}

/// A method unit that pauses at a host request before returning `x + 1`.
fn pausing_method() -> Arc<VerifiedProgram> {
    let mut b = ProgramBuilder::new();
    let pause = b.host_sig_for(
        "pause",
        HostSig {
            args: vec![],
            rets: vec![],
        },
    );
    let f = b
        .declare_function(sig(&[ValueType::I64], &[ValueType::I64]))
        .unwrap();
    let mut a = b.function_asm(f).unwrap();
    let x = b.argument(f, 0).unwrap();
    let [] = b.emit_call(&mut a, pause, &[]).unwrap();
    let k = a.alloc_register(ValueType::I64);
    a.const_i64(k, 1).i64_add(x, x, k).ret(0, &[x]);
    b.define_function(f, a).unwrap();
    Arc::new(b.build_verified().unwrap())
}

#[test]
fn installing_replacement_code_leaves_a_suspended_old_frame_running_its_unit() {
    let mut vm = Vm::new(NoHost, Limits::default());
    let caller = vm.install(dispatcher()).unwrap();
    let old = vm.install(pausing_method()).unwrap();
    let options = InvocationOptions::default().yield_host_calls();
    let mut run = vm
        .start(FuncRef::new(caller, FuncId(0)), &[Value::I64(10)], options)
        .unwrap();
    // The language's method table answers the first lookup with the old method.
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    let request = run.pending_host_call().unwrap();
    assert_eq!((request.symbol(), request.unit()), ("lookup", caller));
    request
        .complete(&mut [Value::Func(FuncRef::new(old, FuncId(0)))], Ok(0))
        .unwrap();
    // The old method is now suspended inside its own unit.
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    let units: Vec<_> = run
        .inspect()
        .frames()
        .map(|frame| frame.location.unit)
        .collect();
    assert_eq!(units, [old, caller], "innermost first");
    let mut request = run.pending_host_call().unwrap();
    assert_eq!((request.symbol(), request.unit()), ("pause", old));
    // An edit installs replacement code while the old frame waits.
    let new = request.install(adds(100)).unwrap();
    assert_ne!(new, old);
    request.complete(&mut [], Ok(0)).unwrap();
    // The next lookup answers with the replacement; the old frame already returned `11`.
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    run.pending_host_call()
        .unwrap()
        .complete(&mut [Value::Func(FuncRef::new(new, FuncId(0)))], Ok(0))
        .unwrap();
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::Complete);
    let [Value::Agg(pair)] = run.result().unwrap().unwrap()[..] else {
        panic!("pair result");
    };
    let results = [0, 1].map(|index| run.aggregates().tuple_get(pair, index).unwrap());
    assert_eq!(results, [Value::I64(11), Value::I64(110)]);
    drop(run);
    assert_eq!(vm.units().len(), 3, "replaced code stays installed");
}

#[test]
fn a_host_request_can_call_back_into_another_unit() {
    let mut b = ProgramBuilder::new();
    let ask = b.host_sig_for(
        "ask",
        HostSig {
            args: vec![],
            rets: vec![ValueType::I64],
        },
    );
    let main = b.declare_function(sig(&[], &[ValueType::I64])).unwrap();
    let mut a = b.function_asm(main).unwrap();
    let [answer] = b.emit_call(&mut a, ask, &[]).unwrap();
    a.ret(0, &[answer]);
    b.define_function(main, a).unwrap();
    let mut vm = Vm::new(NoHost, Limits::default());
    let caller = vm.install(b.build_verified().unwrap()).unwrap();
    let callee = vm.install(adds(1)).unwrap();
    let mut run = vm
        .start(
            FuncRef::new(caller, main),
            &[],
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    run.pending_host_call()
        .unwrap()
        .call_guest(
            Value::Func(FuncRef::new(callee, FuncId(0))),
            vec![Value::I64(41)],
        )
        .unwrap();
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    let mut request = run.pending_host_call().unwrap();
    let mut results = request.take_callback_results().unwrap();
    assert_eq!(results, [Value::I64(42)]);
    request.complete(&mut results, Ok(0)).unwrap();
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::Complete);
    assert_eq!(run.take_outputs(), Some(vec![Value::I64(42)]));
}

/// A unit declaring `boom(i64)`. `fail(x)` throws it; `typed(f)` and `all(f)` call `f(7)` inside
/// a region catching `boom` or anything, returning the payload or `-1`.
struct Exceptions {
    program: Arc<VerifiedProgram>,
    /// `fail(x)` throws `boom(x)`.
    fail: FuncId,
    /// `all(f)` returns `f(7)`, or -1 for any caught exception.
    all: FuncId,
}

fn exceptions() -> Exceptions {
    let mut b = ProgramBuilder::new();
    let boom = b.declare_tag("boom", &[ValueType::I64.into()]).unwrap();
    let signature = b
        .call_sig(sig(&[ValueType::I64], &[ValueType::I64]))
        .unwrap();
    let fail = b
        .declare_function(sig(&[ValueType::I64], &[ValueType::I64]))
        .unwrap();
    let mut a = b.function_asm(fail).unwrap();
    let x = b.argument(fail, 0).unwrap();
    b.emit_throw(&mut a, boom, &[x]).unwrap();
    b.define_function(fail, a).unwrap();

    let all = b
        .declare_function(sig(&[ValueType::Func], &[ValueType::I64]))
        .unwrap();
    let mut a = b.function_asm(all).unwrap();
    let callee = b.argument(all, 0).unwrap();
    let start = a.label();
    a.jmp(start);
    let handler = a.block(&[]);
    a.place(handler).unwrap();
    let minus = a.alloc_register(ValueType::I64);
    a.const_i64(minus, -1);
    a.ret(0, &[minus]);
    a.place(start).unwrap();
    let seven = a.alloc_register(ValueType::I64);
    a.const_i64(seven, 7);
    a.begin_region(&[Catch::All(handler)]);
    let [result] = b
        .emit_call(&mut a, CallTarget::Indirect { signature, callee }, &[seven])
        .unwrap();
    a.end_region();
    a.ret(0, &[result]);
    b.define_function(all, a).unwrap();
    Exceptions {
        program: Arc::new(b.build_verified().unwrap()),
        fail,
        all,
    }
}

#[test]
fn recordings_describe_one_unit_and_fault_when_execution_leaves_it() {
    let fixture = exceptions();
    let mut vm = Vm::new(NoHost, Limits::default());
    let first = vm.install(Arc::clone(&fixture.program)).unwrap();
    let second = vm.install(Arc::clone(&fixture.program)).unwrap();
    // Within its unit, a recording of a later-installed unit replays as unit 0.
    let mut recorder = Recorder::new(Arc::clone(&fixture.program), RecordLimits::default());
    let target = Value::Func(FuncRef::new(second, fixture.fail));
    vm.run(
        FuncRef::new(second, fixture.all),
        core::slice::from_ref(&target),
        recorder.mask(),
        Some(&mut recorder),
    )
    .unwrap();
    let recording = recorder.finish();
    recording.status().unwrap();
    recording.replay().unwrap();
    // Calling another unit cannot be recorded yet.
    let mut recorder = Recorder::new(Arc::clone(&fixture.program), RecordLimits::default());
    let foreign = Value::Func(FuncRef::new(first, fixture.fail));
    let _ = vm.run(
        FuncRef::new(second, fixture.all),
        core::slice::from_ref(&foreign),
        recorder.mask(),
        Some(&mut recorder),
    );
    assert_eq!(
        recorder.finish().status().unwrap_err(),
        &CaptureFault::ForeignUnit
    );
}

#[test]
fn function_contracts_compare_signatures_of_other_units() {
    let mut b = ProgramBuilder::new();
    let integer = b.contract(Contract::Kind(ValueType::I64)).unwrap();
    let increment = b
        .contract(Contract::Function {
            args: &[integer],
            rets: &[integer],
        })
        .unwrap();
    let accept = b
        .declare_function(FunctionAbi {
            args: vec![increment.into()],
            rets: vec![],
        })
        .unwrap();
    let mut a = b.function_asm(accept).unwrap();
    a.ret(0, &[]);
    b.define_function(accept, a).unwrap();
    let mut vm = Vm::new(NoHost, Limits::default());
    let checker = vm.install(b.build_verified().unwrap()).unwrap();
    let matching = vm.install(adds(1)).unwrap();
    vm.run(
        FuncRef::new(checker, accept),
        &[Value::Func(FuncRef::new(matching, FuncId(0)))],
        TraceMask::NONE,
        None,
    )
    .unwrap();
    // `apply` takes a function and an integer: a different exposed signature.
    let other = vm.install(applies(ValueType::Func)).unwrap();
    let target = FuncRef::new(other, FuncId(0));
    let error = vm
        .run(
            FuncRef::new(checker, accept),
            &[Value::Func(target)],
            TraceMask::NONE,
            None,
        )
        .unwrap_err();
    assert!(matches!(
        error.trap,
        Trap::ContractMismatch {
            error: ContractViolation {
                mismatch: ContractMismatch::Callable { function: Some(function) },
                ..
            },
            ..
        } if function == target
    ));
}
