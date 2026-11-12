// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Exceptions raised by hosts at their `host_call`.

use execution_tape::value::FuncRef;
use execution_tape::{
    asm::{Catch, FunctionSig, ProgramBuilder},
    host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef},
    program::{TagId, ValueType},
    recording::{RecordLimits, Recorder, ReplayOutcome},
    trace::{TraceLocation, TraceMask, TraceSink},
    value::{FuncId, Value},
    verifier::VerifiedProgram,
    vm::{InvocationOptions, Limits, RunStatus, Trap, Vm},
};
use std::sync::Arc;

/// Raises `failed((code,))` with a host-built tuple, or misbehaves on request.
struct Raising {
    tag: TagId,
}
impl Host for Raising {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        args: &[ValueRef<'_>],
        _: &mut [Value],
        mut ctx: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        let ValueRef::I64(code) = args[0] else {
            unreachable!("verified argument kind")
        };
        match code {
            -1 => Err(HostError::throw(TagId(9), vec![])),
            -2 => Err(HostError::throw(self.tag, vec![])),
            code => {
                let tuple = ctx.tuple_new(vec![Value::I64(code)]).unwrap();
                Err(HostError::throw(self.tag, vec![Value::Agg(tuple)]))
            }
        }
    }
}

struct Fixture {
    program: Arc<VerifiedProgram>,
    failed: TagId,
    protected: FuncId,
    bare: FuncId,
    /// `host_call` in `bare`.
    call_pc: u32,
}

/// `protected(code)` catches `failed(tuple)` around the host call and returns the tuple's field
/// plus 100; `bare(code)` makes the same call unprotected.
fn fixture() -> Fixture {
    let mut b = ProgramBuilder::new();
    let integer = b
        .contract(execution_tape::contract::Contract::Kind(ValueType::I64))
        .unwrap();
    let pair = b
        .contract(execution_tape::contract::Contract::Tuple(&[Some(integer)]))
        .unwrap();
    let failed = b.declare_tag("failed", &[pair.into()]).unwrap();
    let import = b.host_sig_for(
        "risky",
        HostSig {
            args: vec![ValueType::I64],
            rets: vec![ValueType::I64],
        },
    );
    let signature = FunctionSig {
        arg_types: vec![ValueType::I64],
        ret_types: vec![ValueType::I64],
    };
    let protected = b.declare_function(&signature).unwrap();
    let bare = b.declare_function(&signature).unwrap();

    let mut asm = b.function_asm(protected).unwrap();
    let code = b.argument(protected, 0).unwrap();
    let caught = asm.alloc_register(ValueType::Agg);
    let field = asm.alloc_register(ValueType::I64);
    let hundred = asm.alloc_register(ValueType::I64);
    let handler = asm.block(&[caught]);
    asm.begin_region(&[Catch::Tag(failed, handler)]);
    let [result] = b.emit_call(&mut asm, import, &[code]).unwrap();
    asm.end_region();
    asm.ret(0, &[result]);
    asm.place(handler).unwrap();
    asm.tuple_get(field, caught, 0)
        .const_i64(hundred, 100)
        .i64_add(field, field, hundred)
        .ret(0, &[field]);
    b.define_function(protected, asm).unwrap();

    let mut asm = b.function_asm(bare).unwrap();
    let code = b.argument(bare, 0).unwrap();
    let call_pc = asm.pc();
    let [result] = b.emit_call(&mut asm, import, &[code]).unwrap();
    asm.ret(0, &[result]);
    b.define_function(bare, asm).unwrap();

    Fixture {
        program: Arc::new(b.build_verified().unwrap()),
        failed,
        protected,
        bare,
        call_pc,
    }
}

fn run(f: &Fixture, entry: FuncId, code: i64) -> Result<Vec<Value>, Trap> {
    {
        let mut vm = Vm::new(Raising { tag: f.failed }, Limits::default());
        let unit = vm.install(f.program.clone()).unwrap();
        vm.run(
            FuncRef::new(unit, entry),
            &[Value::I64(code)],
            TraceMask::NONE,
            None,
        )
    }
    .map_err(|error| error.trap)
}

#[test]
fn a_synchronous_host_throw_is_caught_like_a_guest_throw() {
    let f = fixture();
    assert_eq!(run(&f, f.protected, 5).unwrap(), [Value::I64(105)]);
}

#[test]
fn an_uncaught_host_throw_reports_the_host_call() {
    let f = fixture();
    let error = {
        let mut vm = Vm::new(Raising { tag: f.failed }, Limits::default());
        let unit = vm.install(f.program.clone()).unwrap();
        vm.run(
            FuncRef::new(unit, f.bare),
            &[Value::I64(5)],
            TraceMask::NONE,
            None,
        )
    }
    .unwrap_err();
    assert_eq!((error.func, error.pc), (f.bare, f.call_pc));
    let Trap::Uncaught(exception) = &error.trap else {
        panic!("expected an uncaught exception, got {error}");
    };
    assert_eq!(exception.local_tag(), f.failed);
    assert_eq!(
        exception.origin(),
        TraceLocation {
            func: f.bare,
            pc: f.call_pc
        }
    );
    let [Value::Agg(tuple)] = exception.payload().values() else {
        panic!("tuple payload");
    };
    assert_eq!(
        exception
            .payload()
            .aggregates()
            .tuple_get(*tuple, 0)
            .unwrap(),
        Value::I64(5)
    );
}

#[test]
fn invalid_host_throws_trap_instead_of_raising() {
    let f = fixture();
    assert!(matches!(
        run(&f, f.protected, -1),
        Err(Trap::TagOutOfBounds { tag: TagId(9) })
    ));
    assert!(matches!(run(&f, f.protected, -2), Err(Trap::ArityMismatch)));
}

#[test]
fn a_yielded_request_can_complete_by_throwing() {
    let f = fixture();
    let mut vm = Vm::new(Raising { tag: f.failed }, Limits::default());
    let args = [Value::I64(0)];
    let unit = vm.install(f.program.clone()).unwrap();
    let mut run = vm
        .start(
            FuncRef::new(unit, f.protected),
            &args,
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    let mut request = run.pending_host_call().unwrap();
    let tuple = request.context().tuple_new(vec![Value::I64(7)]).unwrap();
    request.throw(f.failed, vec![Value::Agg(tuple)]).unwrap();
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::Complete);
    assert!(matches!(run.result(), Some(Ok(values)) if values == [Value::I64(107)]));
}

#[test]
fn recordings_capture_and_replay_host_throws() {
    let f = fixture();
    for (entry, caught) in [(f.protected, true), (f.bare, false)] {
        let mut recorder = Recorder::new(Arc::clone(&f.program), RecordLimits::default());
        let result = {
            let mut vm = Vm::new(Raising { tag: f.failed }, Limits::default());
            let unit = vm.install(f.program.clone()).unwrap();
            vm.run(
                FuncRef::new(unit, entry),
                &[Value::I64(3)],
                recorder.mask(),
                Some(&mut recorder),
            )
        };
        assert_eq!(result.is_ok(), caught);
        let recording = recorder.finish();
        recording.status().unwrap();
        let call = recording.host_calls().next().unwrap();
        assert!(matches!(
            call.outcome,
            execution_tape::recording::HostOutcome::Threw { tag, payload } if tag == f.failed && payload.len() == 1
        ));
        match recording.replay().unwrap() {
            ReplayOutcome::Returned(values) => {
                assert!(caught);
                assert_eq!(values.values(), &[Value::I64(103)]);
            }
            ReplayOutcome::Trapped(error) => {
                assert!(!caught);
                assert!(matches!(error.trap, Trap::Uncaught(_)));
            }
        }
    }
}
