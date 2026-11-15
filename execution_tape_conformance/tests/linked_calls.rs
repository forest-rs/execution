// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Links: call targets each installed unit owns and the embedding repoints.

use execution_tape::{
    asm::{Asm, CallTarget, Catch, FunctionAbi, FunctionSig, LinkBuildError, ProgramBuilder, Reg},
    contract::{Contract, ContractPosition},
    disasm::disassemble,
    format::DecodeError,
    host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef},
    program::{CallSigId, LinkId, Program, StructTypeDef, TypeId, ValueType},
    recording::{RecordLimits, Recorder, ReplayOutcome, StoreRecord},
    trace::{TraceMask, TraceSink},
    value::{FuncId, FuncRef, UnitId, Value},
    verifier::{MetadataTable, VerifiedProgram, VerifyConfig, VerifyError, verify_program},
    vm::{InvocationOptions, Limits, LinkMismatch, RelinkError, RunStatus, StoreEvent, Vm},
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

fn i64_to_i64() -> FunctionSig {
    FunctionSig {
        arg_types: vec![ValueType::I64],
        ret_types: vec![ValueType::I64],
    }
}

/// Declares `scale(x) = x * factor` in `b`.
fn scale(b: &mut ProgramBuilder, factor: i64) -> FuncId {
    let scale = b.declare_function(i64_to_i64()).unwrap();
    let mut body = b.function_asm(scale).unwrap();
    let x = b.argument(scale, 0).unwrap();
    let k = body.alloc_register(ValueType::I64);
    let y = body.alloc_register(ValueType::I64);
    body.const_i64(k, factor)
        .i64_mul(y, x, k)
        .ret(Reg::EFFECT, &[y]);
    b.define_function(scale, body).unwrap();
    scale
}

/// A program whose `CALLER` calls `SCALE` (`x * factor`) through link `"scale"`.
fn linked(factor: i64) -> Arc<VerifiedProgram> {
    let mut b = ProgramBuilder::new();
    let target = scale(&mut b, factor);
    assert_eq!(target, SCALE, "scale is the first function");
    let link = b.declare_link("scale", target).unwrap();
    assert_eq!(link, LINK, "the only link");
    let caller = b.declare_function(i64_to_i64()).unwrap();
    assert_eq!(caller, CALLER, "the caller follows scale");
    let mut body = b.function_asm(caller).unwrap();
    let x = b.argument(caller, 0).unwrap();
    let [y] = b.emit_call(&mut body, link, &[x]).unwrap();
    body.ret(Reg::EFFECT, &[y]);
    b.define_function(caller, body).unwrap();
    Arc::new(b.build_verified().unwrap())
}

const SCALE: FuncId = FuncId(0);
const CALLER: FuncId = FuncId(1);
const LINK: LinkId = LinkId(0);

/// A program with only `scale` at `FuncId(0)`.
fn scaler(factor: i64) -> Arc<VerifiedProgram> {
    let mut b = ProgramBuilder::new();
    scale(&mut b, factor);
    Arc::new(b.build_verified().unwrap())
}

fn call(vm: &mut Vm<NoHost>, unit: UnitId, x: i64) -> i64 {
    let out = vm
        .run(
            FuncRef::new(unit, CALLER),
            &[Value::I64(x)],
            TraceMask::NONE,
            None,
        )
        .unwrap();
    let [Value::I64(y)] = out[..] else {
        panic!("one i64 result, got {out:?}");
    };
    y
}

#[test]
fn a_link_starts_at_its_declared_target_in_the_installing_unit() {
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(linked(2)).unwrap();
    assert_eq!(
        vm.units().links(unit),
        Some(&[FuncRef::new(unit, SCALE)][..])
    );
    assert_eq!(call(&mut vm, unit, 21), 42);
}

#[test]
fn relinking_repoints_later_calls_at_a_function_of_another_unit() {
    let mut vm = Vm::new(NoHost, Limits::default());
    let caller = vm.install(linked(2)).unwrap();
    let newer = vm.install(scaler(3)).unwrap();
    let previous = vm
        .relink(caller, LINK, FuncRef::new(newer, FuncId(0)))
        .unwrap();
    assert_eq!(previous, FuncRef::new(caller, SCALE));
    assert_eq!(
        vm.units().links(caller),
        Some(&[FuncRef::new(newer, FuncId(0))][..])
    );
    assert_eq!(call(&mut vm, caller, 5), 15, "runs the newer unit's code");
    // Back to the unit's own function.
    vm.relink(caller, LINK, FuncRef::new(caller, SCALE))
        .unwrap();
    assert_eq!(call(&mut vm, caller, 5), 10);
}

#[test]
fn every_install_owns_its_link_table() {
    let program = linked(2);
    let mut vm = Vm::new(NoHost, Limits::default());
    let first = vm.install(Arc::clone(&program)).unwrap();
    let second = vm.install(program).unwrap();
    let other = vm.install(scaler(7)).unwrap();
    vm.relink(first, LINK, FuncRef::new(other, FuncId(0)))
        .unwrap();
    assert_eq!(call(&mut vm, first, 1), 7);
    assert_eq!(
        call(&mut vm, second, 1),
        2,
        "the other install is unchanged"
    );
    assert_eq!(
        vm.units().links(second),
        Some(&[FuncRef::new(second, SCALE)][..])
    );
}

#[test]
fn relinking_keeps_no_history() {
    let mut vm = Vm::new(NoHost, Limits::default());
    let caller = vm.install(linked(2)).unwrap();
    let newer = vm.install(scaler(3)).unwrap();
    let (log, bytes) = (vm.units().log().len(), vm.units().metadata_bytes());
    let (old, new) = (FuncRef::new(caller, SCALE), FuncRef::new(newer, FuncId(0)));
    for _ in 0..1000 {
        vm.relink(caller, LINK, new).unwrap();
        vm.relink(caller, LINK, old).unwrap();
    }
    assert_eq!(vm.relink(caller, LINK, old), Ok(old), "already there");
    assert_eq!(vm.units().log().len(), log, "relinks are not logged");
    assert_eq!(vm.units().metadata_bytes(), bytes, "nor stored");
    assert_eq!(
        vm.units().log().last(),
        Some(&StoreEvent::Installed(newer)),
        "the log lists installs and tags"
    );
}

#[test]
fn relinking_rejects_anything_but_an_installed_function_of_the_exact_signature() {
    let mut vm = Vm::new(NoHost, Limits::default());
    let caller = vm.install(linked(2)).unwrap();
    // `f0` takes and returns `u64`: the same arity, a different signature.
    let mut b = ProgramBuilder::new();
    let wrong = b
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::U64],
            ret_types: vec![ValueType::U64],
        })
        .unwrap();
    let mut body = b.function_asm(wrong).unwrap();
    let x = b.argument(wrong, 0).unwrap();
    body.ret(Reg::EFFECT, &[x]);
    b.define_function(wrong, body).unwrap();
    let other = vm.install(b.build_verified().unwrap()).unwrap();

    let target = FuncRef::new(other, wrong);
    let error = vm.relink(caller, LINK, target).unwrap_err();
    assert_eq!(
        error,
        RelinkError::SignatureMismatch {
            unit: caller,
            link: LINK,
            target,
            difference: LinkMismatch::Argument(0),
        }
    );
    assert_eq!(
        error.to_string(),
        "link#0 of unit 0 cannot name f0 of unit 1: argument 0 has a different contract"
    );
    let missing = FuncRef::new(other, FuncId(9));
    assert_eq!(
        vm.relink(caller, LINK, missing),
        Err(RelinkError::UnknownTarget(missing))
    );
    let foreign = FuncRef::new(UnitId(9), FuncId(0));
    assert_eq!(
        vm.relink(caller, LINK, foreign),
        Err(RelinkError::UnknownTarget(foreign))
    );
    assert_eq!(
        vm.relink(caller, LinkId(1), FuncRef::new(caller, SCALE)),
        Err(RelinkError::UnknownLink {
            unit: caller,
            link: LinkId(1),
        })
    );
    assert_eq!(
        vm.relink(UnitId(9), LINK, FuncRef::new(caller, SCALE)),
        Err(RelinkError::UnknownUnit(UnitId(9)))
    );
    assert_eq!(
        vm.units().links(caller),
        Some(&[FuncRef::new(caller, SCALE)][..]),
        "rejected relinks change nothing"
    );
    assert_eq!(vm.units().log().len(), 2);
    assert_eq!(call(&mut vm, caller, 4), 8);
}

#[test]
fn builders_reject_duplicate_names_and_unknown_targets() {
    let mut b = ProgramBuilder::new();
    let target = scale(&mut b, 1);
    let first = b.declare_link("send", target).unwrap();
    assert_eq!(
        b.declare_link("send", target),
        Err(LinkBuildError::DuplicateName {
            name: "send".into(),
            existing: first,
        })
    );
    assert_eq!(
        b.declare_link("other", FuncId(5)),
        Err(LinkBuildError::UnknownFunction(FuncId(5)))
    );
    // Signature-aware emission checks a link's arity like any other target.
    let caller = b.declare_function(i64_to_i64()).unwrap();
    let mut body = b.function_asm(caller).unwrap();
    assert!(
        b.emit_call::<1>(&mut body, CallTarget::Link(first), &[])
            .is_err()
    );
    assert!(
        b.emit_call::<1>(&mut body, CallTarget::Link(LinkId(4)), &[Reg(1)])
            .is_err()
    );
}

/// A one-function program whose body is `body` and which declares `links`.
fn with_links(body: Asm, links: impl FnOnce(&mut ProgramBuilder)) -> Program {
    let mut b = ProgramBuilder::new();
    b.push_function_checked(body, i64_to_i64()).unwrap();
    links(&mut b);
    b.build()
}

#[test]
fn verification_requires_the_target_to_have_the_link_signature() {
    // Repoint the declared link at a `u64 -> u64` function of the same program.
    let mut b = ProgramBuilder::new();
    let wrong = b
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::U64],
            ret_types: vec![ValueType::U64],
        })
        .unwrap();
    let mut body = b.function_asm(wrong).unwrap();
    let x = b.argument(wrong, 0).unwrap();
    body.ret(Reg::EFFECT, &[x]);
    b.define_function(wrong, body).unwrap();
    let right = b.declare_function(i64_to_i64()).unwrap();
    let mut body = b.function_asm(right).unwrap();
    let x = b.argument(right, 0).unwrap();
    body.ret(Reg::EFFECT, &[x]);
    b.define_function(right, body).unwrap();
    b.declare_link("right", right).unwrap();
    let mut program = b.build();
    verify_program(&program, &VerifyConfig::default()).unwrap();
    program.links[0].target = wrong;
    let error = verify_program(&program, &VerifyConfig::default()).unwrap_err();
    assert!(
        matches!(
            error,
            VerifyError::LinkSignatureMismatch { link: LinkId(0), target, .. } if target == wrong
        ),
        "{error:?}"
    );
}

#[test]
fn verification_rejects_duplicate_link_names_and_unknown_links() {
    let mut body = Asm::new();
    body.set_register_types(vec![ValueType::Unit, ValueType::I64]);
    body.ret(Reg::EFFECT, &[Reg(1)]);
    let mut program = with_links(body, |b| {
        b.declare_link("a", FuncId(0)).unwrap();
        b.declare_link("b", FuncId(0)).unwrap();
    });
    program.links[1].name = program.links[0].name;
    let error = verify_program(&program, &VerifyConfig::default()).unwrap_err();
    assert!(matches!(
        error,
        VerifyError::DuplicateLinkName {
            link: LinkId(1),
            first: LinkId(0),
            ..
        }
    ));

    let mut body = Asm::new();
    body.set_register_types(vec![ValueType::Unit, ValueType::I64, ValueType::I64]);
    body.call_link(Reg::EFFECT, LinkId(3), Reg::EFFECT, &[Reg(1)], &[Reg(2)])
        .ret(Reg::EFFECT, &[Reg(2)]);
    let program = with_links(body, |b| {
        b.declare_link("a", FuncId(0)).unwrap();
    });
    let error = verify_program(&program, &VerifyConfig::default()).unwrap_err();
    assert!(matches!(
        error,
        VerifyError::LinkOutOfBounds {
            link: LinkId(3),
            ..
        }
    ));
}

#[test]
fn verification_checks_linked_calls_against_the_link_signature() {
    // Calls an `i64 -> i64` link with a `u64` argument.
    let mut body = Asm::new();
    body.set_register_types(vec![
        ValueType::Unit,
        ValueType::I64,
        ValueType::U64,
        ValueType::I64,
    ]);
    body.const_u64(Reg(2), 1)
        .call_link(Reg::EFFECT, LinkId(0), Reg::EFFECT, &[Reg(2)], &[Reg(3)])
        .ret(Reg::EFFECT, &[Reg(3)]);
    let program = with_links(body, |b| {
        b.declare_link("self", FuncId(0)).unwrap();
    });
    let error = verify_program(&program, &VerifyConfig::default()).unwrap_err();
    assert!(
        matches!(error, VerifyError::RegisterTypeMismatch { .. }),
        "{error:?}"
    );
}

#[test]
fn links_survive_encoding_and_appear_in_disassembly() {
    let program = linked(2);
    let source = program.program();
    let decoded = Program::decode(&source.encode()).unwrap();
    assert_eq!(&decoded, source);
    assert_eq!(decoded.link_name(LINK), Some("scale"));
    assert_eq!(decoded.link(LINK).unwrap().target, SCALE);
    let text = disassemble(source).to_string();
    assert!(
        text.contains("link#0 \"scale\" -> f0 ; sig=[I64] -> [I64]"),
        "{text}"
    );
    assert!(
        text.contains("call.link eff_out=r0, link#0(\"scale\")"),
        "{text}"
    );
}

#[test]
fn recordings_replay_relinks_made_before_the_invocation() {
    let program = linked(2);
    let mut vm = Vm::new(NoHost, Limits::default());
    let caller = vm.install(Arc::clone(&program)).unwrap();
    let newer = vm.install(scaler(5)).unwrap();
    let target = FuncRef::new(newer, FuncId(0));
    // An earlier relink that the recording does not need to repeat.
    let interim = vm.install(scaler(9)).unwrap();
    vm.relink(caller, LINK, FuncRef::new(interim, FuncId(0)))
        .unwrap();
    vm.relink(caller, LINK, target).unwrap();
    let mut recorder = Recorder::new(program, RecordLimits::default());
    let out = vm
        .run(
            FuncRef::new(caller, CALLER),
            &[Value::I64(3)],
            recorder.mask(),
            Some(&mut recorder),
        )
        .unwrap();
    assert_eq!(out, [Value::I64(15)]);
    let recording = recorder.finish();
    recording.status().unwrap();
    // The recording keeps where each link pointed at entry, not how it got there.
    assert_eq!(recording.unit(caller).unwrap().links, [target]);
    assert_eq!(recording.unit(newer).unwrap().links, []);
    let ReplayOutcome::Returned(values) = recording.replay().unwrap() else {
        panic!("replay returns like the recorded run");
    };
    assert_eq!(
        values.values(),
        [Value::I64(15)],
        "replay runs the relinked target"
    );
}

/// A program with `f0(value) -> i64` whose argument has contract `argument`.
fn takes(argument: impl FnOnce(&mut ProgramBuilder) -> FunctionAbi) -> Arc<VerifiedProgram> {
    let mut b = ProgramBuilder::new();
    let abi = argument(&mut b);
    let function = b.declare_function(abi).unwrap();
    let mut body = b.function_asm(function).unwrap();
    let zero = body.alloc_register(ValueType::I64);
    body.const_i64(zero, 0).ret(Reg::EFFECT, &[zero]);
    b.define_function(function, body).unwrap();
    b.declare_link("takes", function).unwrap();
    Arc::new(b.build_verified().unwrap())
}

#[test]
fn relinking_compares_full_contracts_not_just_kinds() {
    let opaque = |_: &mut ProgramBuilder| FunctionAbi {
        args: vec![ValueType::Agg.into()],
        rets: vec![ValueType::I64.into()],
    };
    let pair = |b: &mut ProgramBuilder| {
        let integer = b.contract(Contract::Kind(ValueType::I64)).unwrap();
        let pair = b
            .contract(Contract::Tuple(&[Some(integer), Some(integer)]))
            .unwrap();
        FunctionAbi {
            args: vec![pair.into()],
            rets: vec![ValueType::I64.into()],
        }
    };
    let mut vm = Vm::new(NoHost, Limits::default());
    let caller = vm.install(takes(opaque)).unwrap();
    let refined = vm.install(takes(pair)).unwrap();
    let same_kinds = FuncRef::new(refined, FuncId(0));
    assert_eq!(
        vm.relink(caller, LinkId(0), same_kinds),
        Err(RelinkError::SignatureMismatch {
            unit: caller,
            link: LinkId(0),
            target: same_kinds,
            difference: LinkMismatch::Argument(0),
        }),
        "a refined argument contract is a different signature"
    );
    // An independently compiled program with the same contracts matches exactly.
    let equal = vm.install(takes(opaque)).unwrap();
    let target = FuncRef::new(equal, FuncId(0));
    vm.relink(caller, LinkId(0), target).unwrap();
    // And so does the refined program's own link, from another unit of it.
    let twin = vm.install(takes(pair)).unwrap();
    vm.relink(twin, LinkId(0), same_kinds).unwrap();
}

#[test]
fn programs_find_links_by_name() {
    let program = linked(2);
    let program = program.program();
    assert_eq!(program.link_by_name("scale"), Some(LINK));
    assert_eq!(program.link_by_name("missing"), None);
}

#[test]
fn an_exception_from_a_link_target_unwinds_to_the_caller() {
    let mut b = ProgramBuilder::new();
    let failed = b.declare_tag("failed", &[ValueType::I64.into()]).unwrap();
    let raise = b
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![],
        })
        .unwrap();
    let mut body = b.function_asm(raise).unwrap();
    let value = b.argument(raise, 0).unwrap();
    b.emit_throw(&mut body, failed, &[value]).unwrap();
    b.define_function(raise, body).unwrap();
    let link = b.declare_link("raise", raise).unwrap();
    let entry = b.declare_function(i64_to_i64()).unwrap();
    let mut body = b.function_asm(entry).unwrap();
    let value = b.argument(entry, 0).unwrap();
    let start = body.label();
    body.jmp(start);
    let caught = body.alloc_register(ValueType::I64);
    let handler = body.block(&[caught]);
    body.place(handler).unwrap();
    let offset = body.alloc_register(ValueType::I64);
    body.const_i64(offset, 1000)
        .i64_add(caught, caught, offset)
        .ret(Reg::EFFECT, &[caught]);
    body.place(start).unwrap();
    body.begin_region(&[Catch::Tag(failed, handler)]);
    let [] = b.emit_call(&mut body, link, &[value]).unwrap();
    body.end_region();
    body.ret(Reg::EFFECT, &[value]);
    b.define_function(entry, body).unwrap();
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(b.build_verified().unwrap()).unwrap();
    let out = vm
        .run(
            FuncRef::new(unit, entry),
            &[Value::I64(7)],
            TraceMask::NONE,
            None,
        )
        .unwrap();
    assert_eq!(out, [Value::I64(1007)], "the caller's region caught it");
}

fn field(name: &str) -> StructTypeDef {
    StructTypeDef {
        field_names: vec![name.into()],
        field_types: vec![ValueType::I64],
    }
}

/// Declares `read(point) -> i64 = point.0 * factor`, with `point` a struct of `field(name)`
/// declared after `decoys` other struct types, so its `TypeId` is `decoys`.
fn reader(b: &mut ProgramBuilder, name: &str, decoys: usize, factor: i64) -> (FuncId, TypeId) {
    for decoy in 0..decoys {
        b.struct_type(field(&format!("decoy{decoy}")));
    }
    let ty = b.struct_type(field(name));
    let integer = b.contract(Contract::Kind(ValueType::I64)).unwrap();
    let point = b
        .contract(Contract::Struct {
            type_id: ty,
            fields: &[integer],
        })
        .unwrap();
    let read = b
        .declare_function(FunctionAbi {
            args: vec![point.into()],
            rets: vec![ValueType::I64.into()],
        })
        .unwrap();
    let mut a = b.function_asm(read).unwrap();
    let (value, k, result) = (
        a.alloc_register(ValueType::I64),
        a.alloc_register(ValueType::I64),
        a.alloc_register(ValueType::I64),
    );
    a.struct_get(value, Reg(1), 0)
        .const_i64(k, factor)
        .i64_mul(result, value, k)
        .ret(Reg::EFFECT, &[result]);
    b.define_function(read, a).unwrap();
    (read, ty)
}

#[test]
fn relinking_matches_struct_types_by_shape_across_programs() {
    // The caller's point is `TypeId(1)`; the replacement's equal point is `TypeId(0)`.
    let mut b = ProgramBuilder::new();
    let (read, ty) = reader(&mut b, "x", 1, 1);
    let link = b.declare_link("read", read).unwrap();
    let entry = b
        .declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let mut a = b.function_asm(entry).unwrap();
    let (seven, point) = (
        a.alloc_register(ValueType::I64),
        a.alloc_register(ValueType::Agg),
    );
    a.const_i64(seven, 7).struct_new(point, ty, &[seven]);
    let [result] = b.emit_call(&mut a, link, &[point]).unwrap();
    a.ret(Reg::EFFECT, &[result]);
    b.define_function(entry, a).unwrap();
    let mut vm = Vm::new(NoHost, Limits::default());
    let caller = vm.install(b.build_verified().unwrap()).unwrap();

    let replacement = |name: &str| {
        let mut b = ProgramBuilder::new();
        reader(&mut b, name, 0, 10);
        Arc::new(b.build_verified().unwrap())
    };
    let equal = vm.install(replacement("x")).unwrap();
    vm.relink(caller, link, FuncRef::new(equal, FuncId(0)))
        .unwrap();
    let out = vm
        .run(FuncRef::new(caller, entry), &[], TraceMask::NONE, None)
        .unwrap();
    assert_eq!(
        out,
        [Value::I64(70)],
        "the replacement read the caller's point"
    );

    let renamed = vm.install(replacement("y")).unwrap();
    let target = FuncRef::new(renamed, FuncId(0));
    assert_eq!(
        vm.relink(caller, link, target),
        Err(RelinkError::SignatureMismatch {
            unit: caller,
            link,
            target,
            difference: LinkMismatch::Argument(0),
        }),
        "a different field name is a different shape"
    );
}

#[test]
fn verification_names_the_link_when_an_argument_breaks_its_contract() {
    let mut b = ProgramBuilder::new();
    let integer = b.contract(Contract::Kind(ValueType::I64)).unwrap();
    let pair = b.contract(Contract::Tuple(&[Some(integer)])).unwrap();
    let first = b
        .declare_function(FunctionAbi {
            args: vec![pair.into()],
            rets: vec![ValueType::I64.into()],
        })
        .unwrap();
    let mut a = b.function_asm(first).unwrap();
    let value = a.alloc_register(ValueType::I64);
    a.tuple_get(value, Reg(1), 0).ret(Reg::EFFECT, &[value]);
    b.define_function(first, a).unwrap();
    let link = b.declare_link("first", first).unwrap();
    let entry = b
        .declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let mut a = b.function_asm(entry).unwrap();
    let (flag, tuple) = (
        a.alloc_register(ValueType::Bool),
        a.alloc_register(ValueType::Agg),
    );
    // A tuple of `bool` where the link takes a tuple of `i64`.
    a.const_bool(flag, true).tuple_new(tuple, &[flag]);
    let [result] = b.emit_call(&mut a, link, &[tuple]).unwrap();
    a.ret(Reg::EFFECT, &[result]);
    b.define_function(entry, a).unwrap();
    let error = verify_program(&b.build(), &VerifyConfig::default()).unwrap_err();
    assert!(
        matches!(
            error,
            VerifyError::ContractMismatch {
                position: ContractPosition::LinkArgument { link: found, index: 0 },
                ..
            } if found == link
        ),
        "{error:?}"
    );
}

#[test]
fn malformed_target_signatures_report_the_function_not_the_link() {
    let mut body = Asm::new();
    body.set_register_types(vec![ValueType::Unit, ValueType::I64]);
    body.ret(Reg::EFFECT, &[Reg(1)]);
    let mut program = with_links(body, |b| {
        b.declare_link("self", FuncId(0)).unwrap();
    });
    program.functions[0].arg_types.offset = u32::MAX - 1;
    let error = verify_program(&program, &VerifyConfig::default()).unwrap_err();
    assert!(
        !matches!(
            error,
            VerifyError::LinkSignatureMismatch { .. }
                | VerifyError::InvalidMetadata {
                    table: MetadataTable::Links,
                    ..
                }
        ),
        "{error:?}"
    );
}

/// The sections of an encoded program, after its 12-byte header.
fn sections(bytes: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut rest = &bytes[12..];
    let mut out = Vec::new();
    while let Some((&tag, tail)) = rest.split_first() {
        let (mut len, mut shift, mut used) = (0_usize, 0, 0);
        for &byte in tail {
            used += 1;
            len |= usize::from(byte & 0x7f) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                break;
            }
        }
        out.push((tag, tail[used..used + len].to_vec()));
        rest = &tail[used + len..];
    }
    out
}

fn assemble(header: &[u8], sections: &[(u8, Vec<u8>)]) -> Vec<u8> {
    let mut out = header[..12].to_vec();
    for (tag, payload) in sections {
        out.push(*tag);
        let mut len = payload.len();
        loop {
            let byte = u8::try_from(len & 0x7f).unwrap();
            len >>= 7;
            if len == 0 {
                out.push(byte);
                break;
            }
            out.push(byte | 0x80);
        }
        out.extend_from_slice(payload);
    }
    out
}

const LINKS: u8 = 17;

#[test]
fn the_link_section_decodes_strictly() {
    let bytes = linked(2).program().encode();
    let original = sections(&bytes);
    let links = original
        .iter()
        .find(|(tag, _)| *tag == LINKS)
        .map(|(_, payload)| payload.clone())
        .unwrap();
    let with = |payload: Vec<u8>| {
        let replaced: Vec<_> = original
            .iter()
            .map(|(tag, old)| {
                (
                    *tag,
                    if *tag == LINKS {
                        payload.clone()
                    } else {
                        old.clone()
                    },
                )
            })
            .collect();
        Program::decode(&assemble(&bytes, &replaced))
    };
    assert!(with(links.clone()).is_ok(), "the unchanged section decodes");

    let mut zero_name = links.clone();
    zero_name[1] = 0;
    assert_eq!(
        with(zero_name),
        Err(DecodeError::OutOfBounds),
        "names are non-zero"
    );

    let mut trailing = links.clone();
    trailing.push(0);
    assert_eq!(with(trailing), Err(DecodeError::OutOfBounds));

    let mut twice = original.clone();
    twice.push((LINKS, links));
    assert_eq!(
        Program::decode(&assemble(&bytes, &twice)),
        Err(DecodeError::DuplicateSection)
    );
}

#[test]
fn decoded_links_must_name_existing_signatures_and_functions() {
    let source = linked(2).program().clone();
    let reject = |program: &Program| {
        let decoded = Program::decode(&program.encode()).unwrap();
        verify_program(&decoded, &VerifyConfig::default()).unwrap_err()
    };
    let bad_links = |error: &VerifyError| {
        matches!(
            error,
            VerifyError::InvalidMetadata {
                table: MetadataTable::Links,
                entry: 0,
                ..
            }
        )
    };
    let mut program = source.clone();
    program.links[0].call_sig = CallSigId(99);
    let error = reject(&program);
    assert!(bad_links(&error), "{error:?}");
    let mut program = source.clone();
    program.links[0].target = FuncId(99);
    let error = reject(&program);
    assert!(bad_links(&error), "{error:?}");

    // The caller's `call.link` names a link the program no longer declares.
    let mut program = source;
    program.links.clear();
    let error = reject(&program);
    assert!(
        matches!(
            error,
            VerifyError::LinkOutOfBounds {
                link: LinkId(0),
                ..
            }
        ),
        "{error:?}"
    );
}

/// Declares `scale(x) = { wait(); x * factor }`, which pauses on a host request.
fn pausing_scale(b: &mut ProgramBuilder, factor: i64) -> FuncId {
    let wait = b.host_sig_for(
        "wait",
        HostSig {
            args: vec![],
            rets: vec![],
        },
    );
    let scale = b.declare_function(i64_to_i64()).unwrap();
    let mut body = b.function_asm(scale).unwrap();
    let x = b.argument(scale, 0).unwrap();
    let k = body.alloc_register(ValueType::I64);
    let y = body.alloc_register(ValueType::I64);
    body.host_call(Reg::EFFECT, wait, Reg::EFFECT, &[], &[])
        .const_i64(k, factor)
        .i64_mul(y, x, k)
        .ret(Reg::EFFECT, &[y]);
    b.define_function(scale, body).unwrap();
    scale
}

/// `CALLER(x)` returns `(scale(x), scale(x))`, both through link `"scale"`.
fn twice(factor: i64) -> Arc<VerifiedProgram> {
    let mut b = ProgramBuilder::new();
    let target = pausing_scale(&mut b, factor);
    let link = b.declare_link("scale", target).unwrap();
    let caller = b
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::I64, ValueType::I64],
        })
        .unwrap();
    assert_eq!(caller, CALLER, "the caller follows scale");
    let mut body = b.function_asm(caller).unwrap();
    let x = b.argument(caller, 0).unwrap();
    let [first] = b.emit_call(&mut body, link, &[x]).unwrap();
    let [second] = b.emit_call(&mut body, link, &[x]).unwrap();
    body.ret(Reg::EFFECT, &[first, second]);
    b.define_function(caller, body).unwrap();
    Arc::new(b.build_verified().unwrap())
}

fn pausing_scaler(factor: i64) -> Arc<VerifiedProgram> {
    let mut b = ProgramBuilder::new();
    pausing_scale(&mut b, factor);
    Arc::new(b.build_verified().unwrap())
}

/// Runs `CALLER(4)` of `program`, a `twice(2)`, relinking to `pausing_scaler(3)` while the first call waits.
fn relink_mid_call(
    program: Arc<VerifiedProgram>,
    recorder: Option<&mut Recorder>,
) -> (Vec<Value>, UnitId, UnitId) {
    let mut vm = Vm::new(NoHost, Limits::default());
    let caller = vm.install(program).unwrap();
    let mut options = InvocationOptions::default().yield_host_calls();
    if let Some(recorder) = recorder {
        options = options.trace(recorder.mask(), recorder);
    }
    let mut run = vm
        .start(FuncRef::new(caller, CALLER), &[Value::I64(4)], options)
        .unwrap();
    assert_eq!(
        run.resume(u64::MAX).unwrap(),
        RunStatus::AwaitingHost,
        "the first call waits"
    );
    let mut request = run.pending_host_call().unwrap();
    assert_eq!(
        request.unit(),
        caller,
        "the first call entered the old method"
    );
    let newer = request.install(pausing_scaler(3)).unwrap();
    let previous = request
        .relink(caller, LINK, FuncRef::new(newer, FuncId(0)))
        .unwrap();
    assert_eq!(previous, FuncRef::new(caller, SCALE), "the declared target");
    request.complete(&mut [], Ok(0)).unwrap();
    assert_eq!(
        run.resume(u64::MAX).unwrap(),
        RunStatus::AwaitingHost,
        "the second call waits"
    );
    let request = run.pending_host_call().unwrap();
    assert_eq!(
        request.unit(),
        newer,
        "the second call reaches the new method"
    );
    request.complete(&mut [], Ok(0)).unwrap();
    assert_eq!(
        run.resume(u64::MAX).unwrap(),
        RunStatus::Complete,
        "the caller returns"
    );
    (run.take_outputs().unwrap(), caller, newer)
}

#[test]
fn a_relink_while_a_call_waits_leaves_that_activation_on_its_old_target() {
    let (outputs, _, _) = relink_mid_call(twice(2), None);
    // The waiting activation finishes the old method; the next call runs the new one.
    assert_eq!(outputs, [Value::I64(8), Value::I64(12)]);
}

#[test]
fn recordings_replay_relinks_made_while_the_invocation_waits() {
    let program = twice(2);
    let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
    let (outputs, caller, newer) = relink_mid_call(program, Some(&mut recorder));
    let recording = recorder.finish();
    recording.status().unwrap();
    let relinks: Vec<_> = recording
        .store()
        .filter_map(|record| match record {
            StoreRecord::Relinked(relink) => Some(relink),
            _ => None,
        })
        .collect();
    let [relink] = relinks[..] else {
        panic!("one relink, got {relinks:?}");
    };
    assert_eq!(
        (relink.unit, relink.link, relink.target),
        (caller, LINK, FuncRef::new(newer, FuncId(0)))
    );
    let ReplayOutcome::Returned(values) = recording.replay().unwrap() else {
        panic!("replay returns like the recorded run");
    };
    assert_eq!(values.values(), &outputs[..]);
}

/// `CALLER(x)` suspends on `ask(x)`, then calls `SCALE` (`answer * factor`) through `"scale"`.
fn asking(factor: i64) -> Arc<VerifiedProgram> {
    let mut b = ProgramBuilder::new();
    let ask = b
        .declare_tag_with_results("ask", &[ValueType::I64.into()], &[ValueType::I64.into()])
        .unwrap();
    let target = scale(&mut b, factor);
    let link = b.declare_link("scale", target).unwrap();
    let caller = b.declare_function(i64_to_i64()).unwrap();
    assert_eq!(
        (target, link, caller),
        (SCALE, LINK, CALLER),
        "fixture layout"
    );
    let mut body = b.function_asm(caller).unwrap();
    let x = b.argument(caller, 0).unwrap();
    let answer = body.alloc_register(ValueType::I64);
    body.suspend(Reg::EFFECT, ask, Reg::EFFECT, &[x], &[answer]);
    let [y] = b.emit_call(&mut body, link, &[answer]).unwrap();
    body.ret(Reg::EFFECT, &[y]);
    b.define_function(caller, body).unwrap();
    Arc::new(b.build_verified().unwrap())
}

/// Runs `CALLER(4)` of `program`, an `asking(2)`, answering its suspension with 5 after
/// relinking to a `scaler(3)` installed while it waits.
fn relink_at_suspension(
    program: Arc<VerifiedProgram>,
    recorder: Option<&mut Recorder>,
) -> (Vec<Value>, UnitId, UnitId) {
    let mut vm = Vm::new(NoHost, Limits::default());
    let caller = vm.install(program).unwrap();
    let mut options = InvocationOptions::default().yield_suspensions();
    if let Some(recorder) = recorder {
        options = options.trace(recorder.mask(), recorder);
    }
    let mut run = vm
        .start(FuncRef::new(caller, CALLER), &[Value::I64(4)], options)
        .unwrap();
    assert_eq!(
        run.resume(u64::MAX).unwrap(),
        RunStatus::AwaitingHandler,
        "the caller suspends before the linked call"
    );
    let mut pending = run.pending_suspension().unwrap();
    let newer = pending.install(scaler(3)).unwrap();
    pending
        .relink(caller, LINK, FuncRef::new(newer, FuncId(0)))
        .unwrap();
    assert_eq!(
        pending.units().links(caller),
        Some(&[FuncRef::new(newer, FuncId(0))][..]),
        "the suspension sees the new target"
    );
    pending.resume(vec![Value::I64(5)]).unwrap();
    assert_eq!(
        run.resume(u64::MAX).unwrap(),
        RunStatus::Complete,
        "the caller returns"
    );
    (run.take_outputs().unwrap(), caller, newer)
}

#[test]
fn a_relink_while_a_suspension_waits_reaches_the_resumed_frame() {
    let (outputs, _, _) = relink_at_suspension(asking(2), None);
    assert_eq!(
        outputs,
        [Value::I64(15)],
        "the resumed caller calls the new target"
    );
}

#[test]
fn recordings_replay_relinks_made_while_a_suspension_waits() {
    let program = asking(2);
    let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
    let (outputs, caller, newer) = relink_at_suspension(program, Some(&mut recorder));
    let recording = recorder.finish();
    recording.status().unwrap();
    let relinks: Vec<_> = recording
        .store()
        .filter_map(|record| match record {
            StoreRecord::Relinked(relink) => Some(relink),
            _ => None,
        })
        .collect();
    let [relink] = relinks[..] else {
        panic!("one relink, got {relinks:?}");
    };
    assert_eq!(
        (relink.unit, relink.link, relink.target),
        (caller, LINK, FuncRef::new(newer, FuncId(0)))
    );
    let ReplayOutcome::Returned(values) = recording.replay().unwrap() else {
        panic!("replay returns like the recorded run");
    };
    assert_eq!(values.values(), &outputs[..]);
}
