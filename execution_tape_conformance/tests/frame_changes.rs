// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The interpreter keeps the top frame's function between instructions; each way the top frame
//! changes must be seen by the very next instruction.
//!
//! Every case traps at the second instruction after a frame change (a constant, then a division
//! by zero) and checks the trap's unit, function and span, which a stale function or unit would
//! get wrong. Debug builds also check the
//! kept frame against the frame stack on every instruction.

use execution_tape::{
    asm::{Asm, Catch, FunctionSig, ProgramBuilder, Reg},
    host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef},
    program::{LinkId, SpanId, ValueType},
    trace::TraceMask,
    value::{FuncId, FuncRef, UnitId, Value},
    vm::{InvocationOptions, Limits, RunStatus, Trap, TrapInfo, Vm},
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
        Err(HostError::UnknownSymbol)
    }
}

const U32: ValueType = ValueType::U32;

fn sig(args: &[ValueType], rets: &[ValueType]) -> FunctionSig {
    FunctionSig {
        arg_types: args.to_vec(),
        ret_types: rets.to_vec(),
    }
}

/// Emits `1 / divisor` under `span`, which traps exactly when `divisor` is zero.
fn probe(body: &mut Asm, span: u64, divisor: Reg) {
    let (one, quotient) = (body.alloc_register(U32), body.alloc_register(U32));
    body.span(SpanId::new(span))
        .const_u32(one, 1)
        .u32_div(quotient, one, divisor);
}

/// Asserts `error` is a division by zero at `span` in `unit`'s `func`.
#[track_caller]
fn trapped_at(error: &TrapInfo, unit: UnitId, func: FuncId, span: u64) {
    assert!(matches!(error.trap, Trap::DivByZero), "{error}");
    assert_eq!(
        (error.unit, error.func, error.span_id),
        (unit, func, Some(SpanId::new(span))),
        "{error}"
    );
}

#[test]
fn calls_and_returns_switch_the_running_function() {
    let mut b = ProgramBuilder::new();
    let entry = b.declare_function(sig(&[U32, U32], &[])).unwrap();
    let callee = b.declare_function(sig(&[U32], &[])).unwrap();
    let mut body = b.function_asm(callee).unwrap();
    probe(&mut body, 1, b.argument(callee, 0).unwrap());
    body.ret(Reg::EFFECT, &[]);
    b.define_function(callee, body).unwrap();
    let mut body = b.function_asm(entry).unwrap();
    let [z_callee, z_entry] = [0, 1].map(|i| b.argument(entry, i).unwrap());
    let [] = b.emit_call(&mut body, callee, &[z_callee]).unwrap();
    probe(&mut body, 2, z_entry);
    body.ret(Reg::EFFECT, &[]);
    b.define_function(entry, body).unwrap();

    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(b.build_verified().unwrap()).unwrap();
    let mut run = |a: u32, b: u32| {
        vm.run(
            FuncRef::new(unit, entry),
            &[Value::U32(a), Value::U32(b)],
            TraceMask::NONE,
            None,
        )
    };
    run(1, 1).unwrap();
    trapped_at(&run(0, 1).unwrap_err(), unit, callee, 1);
    trapped_at(&run(1, 0).unwrap_err(), unit, entry, 2);
}

#[test]
fn an_exception_unwinding_frames_resumes_in_its_handler() {
    let mut b = ProgramBuilder::new();
    let failed = b.declare_tag("failed", &[ValueType::I64.into()]).unwrap();
    let entry = b.declare_function(sig(&[U32], &[])).unwrap();
    let middle = b.declare_function(sig(&[], &[])).unwrap();
    let thrower = b.declare_function(sig(&[], &[])).unwrap();

    let mut body = b.function_asm(thrower).unwrap();
    let code = body.alloc_register(ValueType::I64);
    body.const_i64(code, 7);
    b.emit_throw(&mut body, failed, &[code]).unwrap();
    b.define_function(thrower, body).unwrap();
    // Two frames to unwind: `middle` and `thrower`.
    let mut body = b.function_asm(middle).unwrap();
    let [] = b.emit_call(&mut body, thrower, &[]).unwrap();
    body.ret(Reg::EFFECT, &[]);
    b.define_function(middle, body).unwrap();

    let mut body = b.function_asm(entry).unwrap();
    let z = b.argument(entry, 0).unwrap();
    let start = body.label();
    body.jmp(start);
    let caught = body.alloc_register(ValueType::I64);
    let handler = body.block(&[caught]);
    body.place(handler).unwrap();
    probe(&mut body, 3, z);
    body.ret(Reg::EFFECT, &[]);
    body.place(start).unwrap();
    body.begin_region(&[Catch::Tag(failed, handler)]);
    let [] = b.emit_call(&mut body, middle, &[]).unwrap();
    body.end_region();
    body.ret(Reg::EFFECT, &[]);
    b.define_function(entry, body).unwrap();

    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(b.build_verified().unwrap()).unwrap();
    let entry = FuncRef::new(unit, entry);
    vm.run(entry, &[Value::U32(1)], TraceMask::NONE, None)
        .unwrap();
    let error = vm
        .run(entry, &[Value::U32(0)], TraceMask::NONE, None)
        .unwrap_err();
    trapped_at(&error, unit, entry.func, 3);
}

#[test]
fn resuming_and_suspending_a_continuation_switch_the_running_function() {
    let mut b = ProgramBuilder::new();
    let tag = b.declare_tag("yield", &[]).unwrap();
    let start_sig = b.call_sig(sig(&[U32], &[])).unwrap();
    let next_sig = b.call_sig(sig(&[], &[])).unwrap();
    let generator = b.declare_function(sig(&[U32], &[])).unwrap();
    let entry = b.declare_function(sig(&[U32, U32], &[])).unwrap();

    let mut body = b.function_asm(generator).unwrap();
    let z_generator = b.argument(generator, 0).unwrap();
    body.suspend(Reg::EFFECT, tag, Reg::EFFECT, &[], &[]);
    probe(&mut body, 4, z_generator);
    body.ret(Reg::EFFECT, &[]);
    b.define_function(generator, body).unwrap();

    let mut body = b.function_asm(entry).unwrap();
    let [z_entry, z_generator] = [0, 1].map(|i| b.argument(entry, i).unwrap());
    let f = body.alloc_register(ValueType::Func);
    let cont = body.alloc_register(ValueType::Cont);
    let rest = body.alloc_register(ValueType::Cont);
    let suspended = body.block(&[rest]);
    body.const_func(f, generator);
    body.cont_new(cont, start_sig, f);
    body.begin_region(&[Catch::On(tag, suspended)]);
    body.resume(
        Reg::EFFECT,
        start_sig,
        cont,
        Reg::EFFECT,
        &[z_generator],
        &[],
    );
    body.end_region();
    body.ret(Reg::EFFECT, &[]);
    body.place(suspended).unwrap();
    probe(&mut body, 5, z_entry);
    body.begin_region(&[Catch::On(tag, suspended)]);
    body.resume(Reg::EFFECT, next_sig, rest, Reg::EFFECT, &[], &[]);
    body.end_region();
    body.ret(Reg::EFFECT, &[]);
    b.define_function(entry, body).unwrap();

    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(b.build_verified().unwrap()).unwrap();
    let mut run = |a: u32, b: u32| {
        vm.run(
            FuncRef::new(unit, entry),
            &[Value::U32(a), Value::U32(b)],
            TraceMask::NONE,
            None,
        )
    };
    run(1, 1).unwrap();
    // Back in `entry` after the generator suspends.
    trapped_at(&run(0, 1).unwrap_err(), unit, entry, 5);
    // Back in the generator after `entry` resumes it.
    trapped_at(&run(1, 0).unwrap_err(), unit, generator, 4);
}

/// `entry(z)` asks the host and then divides by `z`; `callback(z)` divides by `z`.
fn asks_and_calls_back() -> (
    Arc<execution_tape::verifier::VerifiedProgram>,
    FuncId,
    FuncId,
) {
    let mut b = ProgramBuilder::new();
    let ask = b.host_sig_for(
        "ask",
        HostSig {
            args: vec![],
            rets: vec![],
        },
    );
    let entry = b.declare_function(sig(&[U32], &[])).unwrap();
    let callback = b.declare_function(sig(&[U32], &[])).unwrap();
    let mut body = b.function_asm(callback).unwrap();
    probe(&mut body, 6, b.argument(callback, 0).unwrap());
    body.ret(Reg::EFFECT, &[]);
    b.define_function(callback, body).unwrap();
    let mut body = b.function_asm(entry).unwrap();
    body.host_call_sig_id(Reg::EFFECT, ask, Reg::EFFECT, &[], &[]);
    probe(&mut body, 7, b.argument(entry, 0).unwrap());
    body.ret(Reg::EFFECT, &[]);
    b.define_function(entry, body).unwrap();
    (Arc::new(b.build_verified().unwrap()), entry, callback)
}

#[test]
fn a_guest_callback_and_its_return_switch_the_running_function() {
    let (program, entry, callback) = asks_and_calls_back();
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(program).unwrap();
    fn options<'a>() -> InvocationOptions<'a> {
        InvocationOptions::default().yield_host_calls()
    }
    // The callback traps on its first instruction.
    let mut invocation = vm
        .start(FuncRef::new(unit, entry), &[Value::U32(1)], options())
        .unwrap();
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::AwaitingHost
    );
    invocation
        .pending_host_call()
        .unwrap()
        .call_guest(
            Value::Func(FuncRef::new(unit, callback)),
            vec![Value::U32(0)],
        )
        .unwrap();
    trapped_at(&invocation.resume(u64::MAX).unwrap_err(), unit, callback, 6);
    drop(invocation);

    // The callback returns, the request completes, and `entry` traps on its next instruction.
    let mut invocation = vm
        .start(FuncRef::new(unit, entry), &[Value::U32(0)], options())
        .unwrap();
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::AwaitingHost
    );
    invocation
        .pending_host_call()
        .unwrap()
        .call_guest(
            Value::Func(FuncRef::new(unit, callback)),
            vec![Value::U32(1)],
        )
        .unwrap();
    assert_eq!(
        invocation.resume(u64::MAX).unwrap(),
        RunStatus::CallbackReturned
    );
    let mut request = invocation.pending_host_call().unwrap();
    assert_eq!(request.take_callback_results().unwrap(), []);
    request.complete(&mut [], Ok(0)).unwrap();
    trapped_at(&invocation.resume(u64::MAX).unwrap_err(), unit, entry, 7);
}

/// A unit whose `FuncId(1)` divides by its argument under `span`; `FuncId(0)` links to it.
fn linked(span: u64) -> execution_tape::verifier::VerifiedProgram {
    let mut b = ProgramBuilder::new();
    let entry = b.declare_function(sig(&[U32], &[])).unwrap();
    let target = b.declare_function(sig(&[U32], &[])).unwrap();
    let mut body = b.function_asm(target).unwrap();
    probe(&mut body, span, b.argument(target, 0).unwrap());
    body.ret(Reg::EFFECT, &[]);
    b.define_function(target, body).unwrap();
    let link = b.declare_link("target", target).unwrap();
    let mut body = b.function_asm(entry).unwrap();
    let z = b.argument(entry, 0).unwrap();
    body.call_link(Reg::EFFECT, link, Reg::EFFECT, &[z], &[]);
    body.ret(Reg::EFFECT, &[]);
    b.define_function(entry, body).unwrap();
    b.build_verified().unwrap()
}

#[test]
fn a_linked_call_runs_the_relinked_unit_even_at_the_same_function_id() {
    let mut vm = Vm::new(NoHost, Limits::default());
    let first = vm.install(linked(8)).unwrap();
    let second = vm.install(linked(9)).unwrap();
    let entry = FuncRef::new(first, FuncId(0));
    let run = |vm: &mut Vm<NoHost>| {
        vm.run(entry, &[Value::U32(0)], TraceMask::NONE, None)
            .unwrap_err()
    };
    trapped_at(&run(&mut vm), first, FuncId(1), 8);
    vm.relink(first, LinkId(0), FuncRef::new(second, FuncId(1)))
        .unwrap();
    // The same function ID in another unit: only the unit tells them apart.
    trapped_at(&run(&mut vm), second, FuncId(1), 9);
}

#[test]
fn a_computation_resumed_after_another_ran_continues_in_its_own_function() {
    let mut vm = Vm::new(NoHost, Limits::default());
    let first = vm.install(linked(10)).unwrap();
    let second = vm.install(linked(11)).unwrap();
    let mut invocation = vm
        .start(
            FuncRef::new(first, FuncId(1)),
            &[Value::U32(0)],
            InvocationOptions::default(),
        )
        .unwrap();
    // Stop between the probe's two instructions, then save.
    assert_eq!(invocation.resume(1).unwrap(), RunStatus::Suspended);
    let id = invocation.save().ok().unwrap();
    // Another computation runs the same function ID of another unit to completion.
    vm.run(
        FuncRef::new(second, FuncId(1)),
        &[Value::U32(1)],
        TraceMask::NONE,
        None,
    )
    .unwrap();
    let mut invocation = vm.resume(id, InvocationOptions::default()).unwrap();
    trapped_at(
        &invocation.resume(u64::MAX).unwrap_err(),
        first,
        FuncId(1),
        10,
    );
}
