// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! VM exception tags: defined and imported tags, per-install tags and cross-unit catches.

use execution_tape::{
    asm::{CallTarget, Catch, FunctionSig, ProgramBuilder},
    host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef},
    program::{Program, TagId, ValueType},
    recording::{RecordLimits, RecordedTag, Recorder, ReplayOutcome, StoreRecord},
    trace::{TraceMask, TraceSink},
    value::{FuncId, FuncRef, TagRef, UnitId, Value},
    verifier::{VerifiedProgram, VerifyConfig, verify_program},
    vm::{InstallError, Limits, TagError, Trap, Vm},
};
use std::sync::Arc;

/// Throws `boom(code)` for a non-negative code, `foreign(code)` when given one, and otherwise
/// returns the code.
struct Raising {
    foreign: Option<TagRef>,
}
impl Host for Raising {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        args: &[ValueRef<'_>],
        rets: &mut [Value],
        ctx: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        let ValueRef::I64(code) = args[0] else {
            unreachable!("verified argument kind")
        };
        match (code, self.foreign) {
            (0.., Some(tag)) => Err(HostError::throw(tag, vec![Value::I64(code)])),
            (0.., None) => {
                let boom = ctx
                    .tag(TagId(0))
                    .expect("the calling program declares `boom`");
                Err(HostError::throw(boom, vec![Value::I64(code)]))
            }
            _ => {
                rets[0] = Value::I64(code);
                Ok(0)
            }
        }
    }
}

fn sig(args: &[ValueType], rets: &[ValueType]) -> FunctionSig {
    FunctionSig {
        arg_types: args.to_vec(),
        ret_types: rets.to_vec(),
    }
}

/// One program's functions around its tag `boom(i64)`, declared or imported.
struct Module {
    program: Arc<VerifiedProgram>,
    boom: TagId,
    /// `fail(x)` throws `boom(x)`.
    fail: FuncId,
    /// `typed(f)` returns `f(7)`, or the payload of a caught `boom`.
    typed: FuncId,
    /// `all(f)` returns `f(7)`, or -1 for any caught exception.
    all: FuncId,
    /// `rethrows(f)` returns `f(7)`, rethrowing any exception with `throw_ref`.
    rethrows: FuncId,
    /// `host_typed(code)` calls the host inside a clause catching `boom`.
    host_typed: FuncId,
    /// `host_all(code)` calls the host inside a catch-all clause returning -1.
    host_all: FuncId,
}

fn module(import: bool) -> Module {
    let mut b = ProgramBuilder::new();
    let payload = [ValueType::I64.into()];
    let boom = if import {
        b.import_tag("boom", &payload).unwrap()
    } else {
        b.declare_tag("boom", &payload).unwrap()
    };
    let signature = b
        .call_sig(sig(&[ValueType::I64], &[ValueType::I64]))
        .unwrap();
    let raise = b.host_sig_for(
        "raise",
        HostSig {
            args: vec![ValueType::I64],
            rets: vec![ValueType::I64],
        },
    );

    let fail = b
        .declare_function(sig(&[ValueType::I64], &[ValueType::I64]))
        .unwrap();
    let mut a = b.function_asm(fail).unwrap();
    let x = b.argument(fail, 0).unwrap();
    b.emit_throw(&mut a, boom, &[x]).unwrap();
    b.define_function(fail, a).unwrap();

    // `f(7)` inside a region with `clause`, whose handler `handle` builds the result.
    let mut protect = |clause: fn(TagId, _) -> Catch, binds: ValueType, host: bool| {
        let arg = if host {
            ValueType::I64
        } else {
            ValueType::Func
        };
        let f = b.declare_function(sig(&[arg], &[ValueType::I64])).unwrap();
        let mut a = b.function_asm(f).unwrap();
        let input = b.argument(f, 0).unwrap();
        let start = a.label();
        a.jmp(start);
        let caught = a.alloc_register(binds);
        let handler = a.block(&[caught]);
        a.place(handler).unwrap();
        match binds {
            ValueType::Exn => {
                a.throw_ref(0, caught);
            }
            _ => {
                a.ret(0, &[caught]);
            }
        }
        a.place(start).unwrap();
        a.begin_region(&[clause(boom, handler)]);
        let [result] = if host {
            b.emit_call(&mut a, raise, &[input]).unwrap()
        } else {
            let seven = a.alloc_register(ValueType::I64);
            a.const_i64(seven, 7);
            b.emit_call(
                &mut a,
                CallTarget::Indirect {
                    signature,
                    callee: input,
                },
                &[seven],
            )
            .unwrap()
        };
        a.end_region();
        a.ret(0, &[result]);
        b.define_function(f, a).unwrap();
        f
    };
    let typed = protect(Catch::Tag, ValueType::I64, false);
    let rethrows = protect(|_, handler| Catch::AllRef(handler), ValueType::Exn, false);
    let host_typed = protect(Catch::Tag, ValueType::I64, true);

    // Catch-all handlers bind nothing and return -1.
    let mut catch_all = |host: bool| {
        let arg = if host {
            ValueType::I64
        } else {
            ValueType::Func
        };
        let f = b.declare_function(sig(&[arg], &[ValueType::I64])).unwrap();
        let mut a = b.function_asm(f).unwrap();
        let input = b.argument(f, 0).unwrap();
        let start = a.label();
        a.jmp(start);
        let handler = a.block(&[]);
        a.place(handler).unwrap();
        let minus = a.alloc_register(ValueType::I64);
        a.const_i64(minus, -1);
        a.ret(0, &[minus]);
        a.place(start).unwrap();
        a.begin_region(&[Catch::All(handler)]);
        let [result] = if host {
            b.emit_call(&mut a, raise, &[input]).unwrap()
        } else {
            let seven = a.alloc_register(ValueType::I64);
            a.const_i64(seven, 7);
            b.emit_call(
                &mut a,
                CallTarget::Indirect {
                    signature,
                    callee: input,
                },
                &[seven],
            )
            .unwrap()
        };
        a.end_region();
        a.ret(0, &[result]);
        b.define_function(f, a).unwrap();
        f
    };
    let all = catch_all(false);
    let host_all = catch_all(true);

    Module {
        program: Arc::new(b.build_verified().unwrap()),
        boom,
        fail,
        typed,
        all,
        rethrows,
        host_typed,
        host_all,
    }
}

/// A program importing `boom` with a payload of `kind`.
fn importer(kind: ValueType) -> Arc<VerifiedProgram> {
    let mut b = ProgramBuilder::new();
    b.import_tag("boom", &[kind.into()]).unwrap();
    Arc::new(b.build_verified().unwrap())
}

fn vm() -> Vm<Raising> {
    Vm::new(Raising { foreign: None }, Limits::default())
}

fn call(vm: &mut Vm<Raising>, unit: UnitId, entry: FuncId, arg: Value) -> Result<Vec<Value>, Trap> {
    vm.run(FuncRef::new(unit, entry), &[arg], TraceMask::NONE, None)
        .map_err(|error| error.trap)
}

fn func(unit: UnitId, func: FuncId) -> Value {
    Value::Func(FuncRef::new(unit, func))
}

#[test]
fn units_importing_one_tag_catch_each_others_exceptions() {
    let (a, b) = (module(true), module(true));
    let mut vm = vm();
    let boom = vm.define_tag("boom", &[ValueType::I64]).unwrap();
    let catcher = vm.install(Arc::clone(&a.program)).unwrap();
    let thrower = vm.install(Arc::clone(&b.program)).unwrap();
    assert_eq!(vm.units().tag(catcher, a.boom), Some(boom));
    assert_eq!(vm.units().tag(thrower, b.boom), Some(boom));
    assert_eq!(vm.units().defined_tag("boom"), Some(boom));
    assert_eq!(vm.units().tag_name(boom), Some("boom"));
    assert_eq!(
        call(&mut vm, catcher, a.typed, func(thrower, b.fail)).unwrap(),
        [Value::I64(7)],
        "a clause naming the imported tag catches another unit's throw"
    );
    // A recording rebuilds the defined tag and both importers, then catches again.
    let mut recorder = Recorder::new(Arc::clone(&a.program), RecordLimits::default());
    vm.run(
        FuncRef::new(catcher, a.typed),
        &[func(thrower, b.fail)],
        recorder.mask(),
        Some(&mut recorder),
    )
    .unwrap();
    let recording = recorder.finish();
    recording.status().unwrap();
    assert!(matches!(
        recording.store().next(),
        Some(StoreRecord::DefinedTag(RecordedTag { tag, name: "boom", .. })) if tag == boom
    ));
    assert_eq!(recording.unit(thrower).unwrap().tags, [boom]);
    let ReplayOutcome::Returned(values) = recording.replay().unwrap() else {
        panic!("replay returns like the recorded run");
    };
    assert_eq!(values.values(), [Value::I64(7)]);
}

#[test]
fn two_installs_of_one_definition_throw_distinct_tags() {
    let m = module(false);
    let mut vm = vm();
    let catcher = vm.install(Arc::clone(&m.program)).unwrap();
    let thrower = vm.install(Arc::clone(&m.program)).unwrap();
    let (mine, theirs) = (
        vm.units().tag(catcher, m.boom).unwrap(),
        vm.units().tag(thrower, m.boom).unwrap(),
    );
    assert_ne!(mine, theirs, "each install defines its own tags");
    assert_eq!(
        vm.units().defined_tag("boom"),
        None,
        "declared tags are not importable"
    );
    assert_eq!(
        call(&mut vm, catcher, m.typed, func(catcher, m.fail)).unwrap(),
        [Value::I64(7)],
        "a unit's clause catches its own tag"
    );
    let Err(Trap::Uncaught(exception)) = call(&mut vm, catcher, m.typed, func(thrower, m.fail))
    else {
        panic!("another install's `boom` is a different tag");
    };
    assert_eq!(exception.tag(), theirs);
    assert_eq!(
        exception.local_tag(),
        Some(m.boom),
        "named in the thrower's program"
    );
    assert_eq!(exception.origin().unit, thrower);
    assert_eq!(exception.name(), "boom");
    assert_eq!(
        call(&mut vm, catcher, m.all, func(thrower, m.fail)).unwrap(),
        [Value::I64(-1)],
        "catch-all reaches every tag"
    );
}

#[test]
fn throw_ref_across_units_keeps_the_original_tag_and_origin() {
    let (a, b) = (module(true), module(true));
    let mut vm = vm();
    let boom = vm.define_tag("boom", &[ValueType::I64]).unwrap();
    let rethrower = vm.install(Arc::clone(&a.program)).unwrap();
    let thrower = vm.install(Arc::clone(&b.program)).unwrap();
    let error = vm
        .run(
            FuncRef::new(rethrower, a.rethrows),
            &[func(thrower, b.fail)],
            TraceMask::NONE,
            None,
        )
        .unwrap_err();
    assert_eq!(error.unit, rethrower, "the trap reports the last raise");
    let Trap::Uncaught(exception) = &error.trap else {
        panic!("expected the rethrown exception, got {error}");
    };
    assert_eq!(exception.tag(), boom);
    assert_eq!(
        exception.origin().unit,
        thrower,
        "the origin is the first throw"
    );
    assert_eq!(exception.origin().func, b.fail);
    assert_eq!(exception.payload().values(), [Value::I64(7)]);
}

#[test]
fn imports_resolve_by_name_and_payload_at_install() {
    let mut vm = vm();
    assert_eq!(
        vm.install(importer(ValueType::I64)).unwrap_err(),
        InstallError::UnresolvedTag {
            name: "boom".into()
        }
    );
    let boom = vm.define_tag("boom", &[ValueType::I64]).unwrap();
    assert_eq!(
        vm.install(importer(ValueType::Bool)).unwrap_err(),
        InstallError::TagPayloadMismatch {
            name: "boom".into()
        }
    );
    assert!(vm.units().is_empty(), "rejected installs leave no unit");
    let unit = vm.install(importer(ValueType::I64)).unwrap();
    assert_eq!(vm.units().tag(unit, TagId(0)), Some(boom));
    assert_eq!(vm.units().tag_payload(boom), Some(&[ValueType::I64][..]));
    assert_eq!(
        vm.define_tag("boom", &[ValueType::I64]).unwrap_err(),
        TagError::DuplicateName {
            name: "boom".into()
        }
    );
    assert_eq!(
        vm.define_tag("exn", &[ValueType::Exn]).unwrap_err(),
        TagError::ExceptionPayload
    );
}

#[test]
fn a_host_throw_of_a_tag_the_caller_cannot_name_reaches_only_catch_all() {
    let m = module(false);
    let mut vm = vm();
    let foreign = vm.define_tag("foreign", &[ValueType::I64]).unwrap();
    let unit = vm.install(Arc::clone(&m.program)).unwrap();
    assert_eq!(
        call(&mut vm, unit, m.host_typed, Value::I64(5)).unwrap(),
        [Value::I64(5)],
        "the program's own tag, resolved through the host context, is caught by name"
    );
    vm.host_mut().foreign = Some(foreign);
    let Err(Trap::Uncaught(exception)) = call(&mut vm, unit, m.host_typed, Value::I64(5)) else {
        panic!("a tagged clause cannot catch a tag its program does not bind");
    };
    assert_eq!(exception.tag(), foreign);
    assert_eq!(exception.local_tag(), None);
    assert_eq!(exception.name(), "foreign");
    assert_eq!(
        call(&mut vm, unit, m.host_all, Value::I64(5)).unwrap(),
        [Value::I64(-1)]
    );
    // Recordings keep VM tags, so replay delivers a tag the calling program cannot name.
    let mut recorder = Recorder::new(Arc::clone(&m.program), RecordLimits::default());
    let _ = vm.run(
        FuncRef::new(unit, m.host_all),
        &[Value::I64(5)],
        recorder.mask(),
        Some(&mut recorder),
    );
    let recording = recorder.finish();
    recording.status().unwrap();
    assert_eq!(recording.tag_name(foreign), Some("foreign"));
    let ReplayOutcome::Returned(values) = recording.replay().unwrap() else {
        panic!("replay catches the foreign tag with catch-all again");
    };
    assert_eq!(values.values(), [Value::I64(-1)]);
}

#[test]
fn a_host_throw_of_an_unknown_tag_traps() {
    let m = module(false);
    let mut vm = Vm::new(
        Raising {
            foreign: Some(TagRef(99)),
        },
        Limits::default(),
    );
    let unit = vm.install(Arc::clone(&m.program)).unwrap();
    assert!(matches!(
        call(&mut vm, unit, m.host_all, Value::I64(1)),
        Err(Trap::UnknownTag { tag: TagRef(99) })
    ));
}

#[test]
fn recordings_of_imported_tags_replay_in_a_fresh_vm() {
    let m = module(true);
    let mut vm = vm();
    // Shift the recorded VM's tag numbering away from the replay VM's.
    vm.define_tag("unrelated", &[]).unwrap();
    vm.define_tag("boom", &[ValueType::I64]).unwrap();
    let unit = vm.install(Arc::clone(&m.program)).unwrap();
    let mut recorder = Recorder::new(Arc::clone(&m.program), RecordLimits::default());
    let result = vm
        .run(
            FuncRef::new(unit, m.host_typed),
            &[Value::I64(3)],
            recorder.mask(),
            Some(&mut recorder),
        )
        .unwrap();
    assert_eq!(result, [Value::I64(3)]);
    let recording = recorder.finish();
    recording.status().unwrap();
    let ReplayOutcome::Returned(values) = recording.replay().unwrap() else {
        panic!("replay returns like the recorded run");
    };
    assert_eq!(values.values(), [Value::I64(3)]);
}

#[test]
fn tag_imports_round_trip_through_the_wire_format() {
    let m = module(true);
    let program = m.program.program();
    assert_eq!(program.tag_imports, [m.boom]);
    let text = execution_tape::disasm::disassemble(program).to_string();
    assert!(
        text.starts_with("tag#0 import \"boom\" ; payload=[I64]\n"),
        "{text}"
    );
    let decoded = Program::decode(&program.encode()).unwrap();
    assert_eq!(decoded.tag_imports, [m.boom]);
    verify_program(&decoded, &VerifyConfig::default()).unwrap();
    let mut forged = decoded.clone();
    forged.tag_imports = vec![TagId(3)];
    assert!(
        verify_program(&forged, &VerifyConfig::default()).is_err(),
        "imports name declared tags"
    );
}

#[test]
fn tag_results_round_trip_and_must_match_on_import() {
    let mut b = ProgramBuilder::new();
    let ask = b
        .declare_tag_with_results("ask", &[ValueType::Bool.into()], &[ValueType::I64.into()])
        .unwrap();
    let plain = b.declare_tag("plain", &[]).unwrap();
    let program = b.build_verified().unwrap();
    let decoded = Program::decode(&program.program().encode()).unwrap();
    assert_eq!(&decoded, program.program());
    let kinds = |tag: TagId| {
        let entry = decoded.tag(tag).unwrap();
        decoded
            .tag_results(entry)
            .unwrap()
            .iter()
            .copied()
            .collect::<Vec<_>>()
    };
    assert_eq!(kinds(ask), [ValueType::I64]);
    assert!(kinds(plain).is_empty());

    let importer = |results: &[ValueType]| {
        let mut b = ProgramBuilder::new();
        let results: Vec<_> = results.iter().map(|&kind| kind.into()).collect();
        b.import_tag_with_results("ask", &[], &results).unwrap();
        Arc::new(b.build_verified().unwrap())
    };
    let mut vm = vm();
    let tag = vm
        .define_tag_with_results("ask", &[], &[ValueType::I64])
        .unwrap();
    assert_eq!(vm.units().tag_results(tag), Some(&[ValueType::I64][..]));
    assert!(vm.install(importer(&[ValueType::I64])).is_ok());
    assert!(matches!(
        vm.install(importer(&[])),
        Err(InstallError::TagPayloadMismatch { .. })
    ));
    assert_eq!(
        vm.define_tag_with_results("leak", &[], &[ValueType::Exn]),
        Err(TagError::ExceptionPayload)
    );
}
