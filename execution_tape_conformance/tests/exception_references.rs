// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Exception references: `catch_ref`, `catch_all_ref` and `throw_ref`.

use execution_tape::value::FuncRef;
use execution_tape::{
    asm::{Asm, Catch, FunctionSig, ProgramBuilder},
    disasm::disassemble,
    host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef},
    program::{FunctionDef, Program, TagId, TypeTableDef, ValueType},
    recording::{RecordLimits, Recorder, ReplayOutcome},
    trace::{TraceLocation, TraceMask, TraceSink},
    value::{FuncId, Value},
    verifier::{
        ExceptionPlace, RegionError, VerifiedProgram, VerifyConfig, VerifyError, verify_program,
    },
    vm::{Limits, Trap, TrapInfo, Vm},
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
        panic!("fixture makes no host calls")
    }
}

fn sig(args: &[ValueType], rets: &[ValueType]) -> FunctionSig {
    FunctionSig {
        arg_types: args.to_vec(),
        ret_types: rets.to_vec(),
    }
}

struct Fixture {
    program: Arc<VerifiedProgram>,
    failed: TagId,
    other: TagId,
    body: FuncId,
    cleanup: FuncId,
    rethrow: FuncId,
    protected: FuncId,
    outer: FuncId,
    /// `throw` in `body`.
    throw_pc: u32,
    /// `throw` in `cleanup`.
    cleanup_pc: u32,
    /// `throw_ref` in `rethrow`.
    rethrow_pc: u32,
}

/// `protected(code, fail)` calls `body(code)`, which throws `failed(code)`. Its `catch_all_ref`
/// handler runs `cleanup(fail)` and then passes the reference to `rethrow`, which rethrows it.
/// `cleanup(true)` throws `other(9)` instead. `outer` catches `failed` around `protected`.
fn fixture() -> Fixture {
    let mut b = ProgramBuilder::new();
    let failed = b.declare_tag("failed", &[ValueType::I64.into()]).unwrap();
    let other = b.declare_tag("other", &[ValueType::I64.into()]).unwrap();
    let body = b
        .declare_function(sig(&[ValueType::I64], &[ValueType::I64]))
        .unwrap();
    let cleanup = b.declare_function(sig(&[ValueType::Bool], &[])).unwrap();
    let rethrow = b
        .declare_function(sig(&[ValueType::Exn], &[ValueType::I64]))
        .unwrap();
    let both = sig(&[ValueType::I64, ValueType::Bool], &[ValueType::I64]);
    let protected = b.declare_function(&both).unwrap();
    let outer = b.declare_function(&both).unwrap();

    let mut asm = b.function_asm(body).unwrap();
    let code = b.argument(body, 0).unwrap();
    let throw_pc = asm.pc();
    b.emit_throw(&mut asm, failed, &[code]).unwrap();
    b.define_function(body, asm).unwrap();

    let mut asm = b.function_asm(cleanup).unwrap();
    let fail = b.argument(cleanup, 0).unwrap();
    let (raise, done) = (asm.label(), asm.label());
    asm.br(fail, raise, done);
    asm.place(done).unwrap();
    asm.ret(0, &[]);
    asm.place(raise).unwrap();
    let nine = asm.alloc_register(ValueType::I64);
    asm.const_i64(nine, 9);
    let cleanup_pc = asm.pc();
    b.emit_throw(&mut asm, other, &[nine]).unwrap();
    b.define_function(cleanup, asm).unwrap();

    let mut asm = b.function_asm(rethrow).unwrap();
    let exn = b.argument(rethrow, 0).unwrap();
    let rethrow_pc = asm.pc();
    asm.throw_ref(0, exn);
    b.define_function(rethrow, asm).unwrap();

    let mut asm = b.function_asm(protected).unwrap();
    let (code, fail) = (
        b.argument(protected, 0).unwrap(),
        b.argument(protected, 1).unwrap(),
    );
    let caught = asm.alloc_register(ValueType::Exn);
    let handler = asm.block(&[caught]);
    asm.begin_region(&[Catch::AllRef(handler)]);
    let [result] = b.emit_call(&mut asm, body, &[code]).unwrap();
    asm.end_region();
    asm.ret(0, &[result]);
    asm.place(handler).unwrap();
    let [] = b.emit_call(&mut asm, cleanup, &[fail]).unwrap();
    let [never] = b.emit_call(&mut asm, rethrow, &[caught]).unwrap();
    asm.ret(0, &[never]);
    b.define_function(protected, asm).unwrap();

    let mut asm = b.function_asm(outer).unwrap();
    let (code, fail) = (b.argument(outer, 0).unwrap(), b.argument(outer, 1).unwrap());
    let payload = asm.alloc_register(ValueType::I64);
    let hundred = asm.alloc_register(ValueType::I64);
    let handler = asm.block(&[payload]);
    asm.begin_region(&[Catch::Tag(failed, handler)]);
    let [result] = b.emit_call(&mut asm, protected, &[code, fail]).unwrap();
    asm.end_region();
    asm.ret(0, &[result]);
    asm.place(handler).unwrap();
    asm.const_i64(hundred, 100)
        .i64_add(payload, payload, hundred)
        .ret(0, &[payload]);
    b.define_function(outer, asm).unwrap();

    Fixture {
        program: Arc::new(b.build_verified().unwrap()),
        failed,
        other,
        body,
        cleanup,
        rethrow,
        protected,
        outer,
        throw_pc,
        cleanup_pc,
        rethrow_pc,
    }
}

fn run(f: &Fixture, entry: FuncId, code: i64, fail: bool) -> Result<Vec<Value>, TrapInfo> {
    {
        let mut vm = Vm::new(NoHost, Limits::default());
        let unit = vm.install(f.program.clone()).unwrap();
        vm.run(
            FuncRef::new(unit, entry),
            &[Value::I64(code), Value::Bool(fail)],
            TraceMask::NONE,
            None,
        )
    }
}

#[test]
fn a_rethrow_keeps_the_original_tag_payload_and_throw_site() {
    let f = fixture();
    let error = run(&f, f.protected, 7, false).unwrap_err();
    assert_eq!(
        (error.func, error.pc),
        (f.rethrow, f.rethrow_pc),
        "the trap reports the last raise"
    );
    let Trap::Uncaught(exception) = &error.trap else {
        panic!("expected an uncaught exception, got {error}");
    };
    assert_eq!(exception.local_tag(), f.failed);
    assert_eq!(exception.payload().values(), &[Value::I64(7)]);
    assert_eq!(
        exception.origin(),
        TraceLocation {
            func: f.body,
            pc: f.throw_pc
        }
    );
}

#[test]
fn a_rethrown_exception_reaches_an_outer_catch() {
    let f = fixture();
    assert_eq!(run(&f, f.outer, 7, false).unwrap(), [Value::I64(107)]);
}

#[test]
fn an_exception_raised_during_cleanup_replaces_the_original() {
    let f = fixture();
    let error = run(&f, f.protected, 7, true).unwrap_err();
    let Trap::Uncaught(exception) = &error.trap else {
        panic!("expected an uncaught exception, got {error}");
    };
    assert_eq!(exception.local_tag(), f.other);
    assert_eq!(exception.payload().values(), &[Value::I64(9)]);
    let cleanup = TraceLocation {
        func: f.cleanup,
        pc: f.cleanup_pc,
    };
    assert_eq!(exception.origin(), cleanup);
    assert_eq!((error.func, error.pc), (cleanup.func, cleanup.pc));
    // `outer` only catches `failed`, so the replacement still escapes.
    let error = run(&f, f.outer, 7, true).unwrap_err();
    assert!(matches!(&error.trap, Trap::Uncaught(e) if e.local_tag() == f.other));
}

#[test]
fn catch_ref_binds_the_payload_and_the_reference() {
    let mut b = ProgramBuilder::new();
    let failed = b.declare_tag("failed", &[ValueType::I64.into()]).unwrap();
    let function = b
        .declare_function(sig(&[ValueType::I64], &[ValueType::I64]))
        .unwrap();
    let mut asm = b.function_asm(function).unwrap();
    let code = b.argument(function, 0).unwrap();
    let payload = asm.alloc_register(ValueType::I64);
    let reference = asm.alloc_register(ValueType::Exn);
    let copy = asm.alloc_register(ValueType::Exn);
    let zero = asm.alloc_register(ValueType::I64);
    let positive = asm.alloc_register(ValueType::Bool);
    let handler = asm.block(&[payload, reference]);
    let (again, keep) = (asm.label(), asm.label());
    asm.begin_region(&[Catch::TagRef(failed, handler)]);
    b.emit_throw(&mut asm, failed, &[code]).unwrap();
    asm.end_region();
    asm.place(handler).unwrap();
    asm.mov(copy, reference)
        .const_i64(zero, 0)
        .i64_gt(positive, payload, zero)
        .br(positive, again, keep);
    asm.place(keep).unwrap();
    asm.ret(0, &[payload]);
    asm.place(again).unwrap();
    asm.throw_ref(0, copy);
    b.define_function(function, asm).unwrap();
    let program = b.build_verified().unwrap();
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(program.clone()).unwrap();
    assert_eq!(
        vm.run(
            FuncRef::new(unit, function),
            &[Value::I64(-3)],
            TraceMask::NONE,
            None
        )
        .unwrap(),
        [Value::I64(-3)]
    );
    let unit = vm.install(program.clone()).unwrap();
    let error = vm
        .run(
            FuncRef::new(unit, function),
            &[Value::I64(3)],
            TraceMask::NONE,
            None,
        )
        .unwrap_err();
    assert!(matches!(&error.trap, Trap::Uncaught(e) if e.payload().values() == [Value::I64(3)]));
    let text = disassemble(program.program()).to_string();
    assert!(
        text.contains(": catch_ref tag#0(\"failed\") -> @L"),
        "{text}"
    );
    assert!(text.contains("throw_ref eff=r0, exn=r"), "{text}");
}

#[test]
fn exception_references_cannot_cross_the_invocation_boundary() {
    let f = fixture();
    let error = {
        let mut vm = Vm::new(NoHost, Limits::default());
        let unit = vm.install(f.program.clone()).unwrap();
        vm.run(FuncRef::new(unit, f.rethrow), &[], TraceMask::NONE, None)
    }
    .unwrap_err();
    assert!(matches!(error.trap, Trap::ExceptionBoundary), "{error}");
}

#[test]
fn recordings_link_a_rethrow_to_its_original_exception() {
    let f = fixture();
    let mut recorder = Recorder::new(Arc::clone(&f.program), RecordLimits::default());
    let result = {
        let mut vm = Vm::new(NoHost, Limits::default());
        let unit = vm.install(f.program.clone()).unwrap();
        vm.run(
            FuncRef::new(unit, f.outer),
            &[Value::I64(5), Value::Bool(false)],
            recorder.mask(),
            Some(&mut recorder),
        )
    }
    .unwrap();
    assert_eq!(result, [Value::I64(105)]);
    let recording = recorder.finish();
    recording.status().unwrap();
    assert!(matches!(
        recording.replay(),
        Ok(ReplayOutcome::Returned(values)) if values.values() == [Value::I64(105)]
    ));

    let mut recorder = Recorder::new(Arc::clone(&f.program), RecordLimits::default());
    let error = {
        let mut vm = Vm::new(NoHost, Limits::default());
        let unit = vm.install(f.program.clone()).unwrap();
        vm.run(
            FuncRef::new(unit, f.protected),
            &[Value::I64(5), Value::Bool(false)],
            recorder.mask(),
            Some(&mut recorder),
        )
    }
    .unwrap_err();
    let recording = recorder.finish();
    recording.status().unwrap();
    let Ok(ReplayOutcome::Trapped(replayed)) = recording.replay() else {
        panic!("replay should reproduce the uncaught rethrow");
    };
    assert_eq!((replayed.func, replayed.pc), (error.func, error.pc));
}

fn rejected(edit: impl FnOnce(&mut ProgramBuilder) -> FuncId) -> VerifyError {
    let mut b = ProgramBuilder::new();
    let _ = edit(&mut b);
    verify_program(&b.build(), &VerifyConfig::default()).unwrap_err()
}

#[test]
fn verifier_keeps_exception_references_inside_guest_registers() {
    // Host signatures.
    let error = rejected(|b| {
        b.host_sig_for(
            "leak",
            HostSig {
                args: vec![ValueType::Exn],
                rets: vec![],
            },
        );
        FuncId(0)
    });
    assert_eq!(
        error,
        VerifyError::ExceptionReference {
            place: ExceptionPlace::HostSig { host_sig: 0 }
        }
    );
    // Tag payloads.
    let error = rejected(|b| {
        b.declare_tag("nested", &[ValueType::Exn.into()]).unwrap();
        FuncId(0)
    });
    assert_eq!(
        error,
        VerifyError::ExceptionReference {
            place: ExceptionPlace::TagPayload { tag: 0 }
        }
    );
    // Tuples.
    let error = rejected(|b| {
        let function = b.declare_function(sig(&[ValueType::Exn], &[])).unwrap();
        let mut asm = b.function_asm(function).unwrap();
        let tuple = asm.alloc_register(ValueType::Agg);
        asm.tuple_new(tuple, &[1]).ret(0, &[]);
        b.define_function(function, asm).unwrap();
        function
    });
    assert!(
        matches!(
            error,
            VerifyError::ExceptionReference {
                place: ExceptionPlace::Aggregate {
                    func: 0,
                    reg: 1,
                    ..
                }
            }
        ),
        "{error}"
    );
    assert_eq!(error.function(), Some(FuncId(0)));
    // Struct fields, through the raw program constructor.
    let mut types = TypeTableDef::default();
    types.structs.push(execution_tape::program::StructTypeDef {
        field_names: vec!["e".into()],
        field_types: vec![ValueType::Exn],
    });
    let program = Program::new(
        vec![],
        vec![],
        vec![],
        types,
        vec![FunctionDef {
            arg_types: vec![],
            ret_types: vec![],
            register_types: vec![ValueType::Unit],
            bytecode: {
                let mut asm = Asm::new();
                asm.ret(0, &[]);
                asm.finish().unwrap()
            },
            spans: vec![],
        }],
    );
    assert_eq!(
        verify_program(&program, &VerifyConfig::default()),
        Err(VerifyError::ExceptionReference {
            place: ExceptionPlace::StructField {
                type_id: 0,
                field: 0
            }
        })
    );
}

#[test]
fn reference_clauses_need_a_final_exn_parameter_and_throw_ref_an_exn_register() {
    let error = rejected(|b| {
        let function = b.declare_function(sig(&[], &[])).unwrap();
        let mut asm = b.function_asm(function).unwrap();
        let wrong = asm.alloc_register(ValueType::I64);
        let handler = asm.block(&[wrong]);
        let failed = b.declare_tag("failed", &[]).unwrap();
        asm.begin_region(&[Catch::AllRef(handler)]);
        b.emit_throw(&mut asm, failed, &[]).unwrap();
        asm.end_region();
        asm.place(handler).unwrap();
        asm.ret(0, &[]);
        b.define_function(function, asm).unwrap();
        function
    });
    assert_eq!(
        error,
        VerifyError::InvalidRegion {
            func: 0,
            region: 0,
            error: RegionError::ClauseException {
                clause: 0,
                actual: ValueType::I64
            }
        }
    );
    let error = rejected(|b| {
        let function = b.declare_function(sig(&[ValueType::I64], &[])).unwrap();
        let mut asm = b.function_asm(function).unwrap();
        asm.throw_ref(0, 1);
        b.define_function(function, asm).unwrap();
        function
    });
    assert!(
        matches!(
            error,
            VerifyError::RegisterTypeMismatch {
                reg: 1,
                required: ValueType::Exn,
                ..
            }
        ),
        "{error}"
    );
}
