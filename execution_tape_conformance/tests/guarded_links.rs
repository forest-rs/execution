// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Guarded links: inline caches for dynamic sends. A call reaches the link's target when its
//! keys match the calling unit's guard, and the link's fallback otherwise; the embedding fills
//! the guard while the fallback's request waits.

use execution_tape::{
    asm::{CallBuildError, FunctionSig, LinkBuildError, ProgramBuilder, Reg},
    host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef},
    program::{HostTypeId, LinkGuardEntry, LinkId, Pc, Program, ValueType},
    recording::{RecordLimits, Recorder, ReplayOutcome, StoreRecord},
    trace::{TraceMask, TraceSink},
    value::{FuncId, FuncRef, Obj, ObjHandle, UnitId, Value},
    verifier::{VerifiedProgram, VerifyConfig, VerifyError, verify_program},
    vm::{
        GuardKeys, GuardedLink, Limits, LinkGuard, PendingHostCall, RelinkError, RunStatus,
        TurnOptions, TurnOutcome, Vm,
    },
};
use std::sync::Arc;

const SHAPES: HostTypeId = HostTypeId(0x5348_4150);
/// Keys are classes; an object's handle is its class.
const CIRCLE: u64 = 7;
const SQUARE: u64 = 9;
const TRIANGLE: u64 = 11;

/// Keys objects by handle; the fallback's `miss` request is serviced by the test's driver.
struct Shapes;
impl Host for Shapes {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        _: &[ValueRef<'_>],
        _: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        panic!("every host call is yielded")
    }

    fn object_key(&self, object: Obj) -> Option<u64> {
        Some(object.handle.0)
    }
}

fn shape(class: u64) -> Value {
    Value::Obj(Obj {
        host_type: SHAPES,
        handle: ObjHandle(class),
    })
}

/// Functions of [`program`].
const CIRCLE_AREA: FuncId = FuncId(0);
const SQUARE_AREA: FuncId = FuncId(1);
const DISPATCH: FuncId = FuncId(2);
const MISS: FuncId = FuncId(3);
const SEND: FuncId = FuncId(4);
const AREA: LinkId = LinkId(0);

/// `area(<circle>) = 1` and `area(<square>) = 2`; `dispatch(obj)` selects one by key;
/// `miss(link, key, obj)` asks the host to fill the guard, then dispatches; `send(obj)` sends
/// `area` through guarded link `AREA`, keyed by the receiver's class.
fn program() -> Arc<VerifiedProgram> {
    let object = ValueType::Obj(SHAPES);
    let unary = FunctionSig {
        arg_types: vec![object],
        ret_types: vec![ValueType::I64],
    };
    let mut b = ProgramBuilder::new();
    for (expected, area) in [(CIRCLE_AREA, 1), (SQUARE_AREA, 2)] {
        let method = b.declare_function(unary.clone()).unwrap();
        assert_eq!(method, expected, "methods come first");
        let mut a = b.function_asm(method).unwrap();
        let out = a.alloc_register(ValueType::I64);
        a.const_i64(out, area).ret(Reg::EFFECT, &[out]);
        b.define_function(method, a).unwrap();
    }

    let dispatch = b.declare_function(unary.clone()).unwrap();
    assert_eq!(dispatch, DISPATCH, "the dispatcher follows the methods");
    let mut a = b.function_asm(dispatch).unwrap();
    let (key, circle, is_circle, out) = (
        a.alloc_register(ValueType::U64),
        a.alloc_register(ValueType::U64),
        a.alloc_register(ValueType::Bool),
        a.alloc_register(ValueType::I64),
    );
    let (circle_label, other) = (a.label(), a.label());
    a.obj_key(key, Reg(1))
        .const_u64(circle, CIRCLE)
        .u64_eq(is_circle, key, circle)
        .br(is_circle, circle_label, other);
    a.place(circle_label).unwrap();
    a.call(Reg::EFFECT, CIRCLE_AREA, Reg::EFFECT, &[Reg(1)], &[out])
        .ret(Reg::EFFECT, &[out]);
    a.place(other).unwrap();
    a.call(Reg::EFFECT, SQUARE_AREA, Reg::EFFECT, &[Reg(1)], &[out])
        .ret(Reg::EFFECT, &[out]);
    b.define_function(dispatch, a).unwrap();

    let miss_request = b.host_sig_for(
        "miss",
        HostSig {
            args: vec![ValueType::U64, ValueType::U64],
            rets: vec![],
        },
    );
    let miss = b
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::U64, ValueType::U64, object],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    assert_eq!(miss, MISS, "the fallback follows the dispatcher");
    let mut a = b.function_asm(miss).unwrap();
    let out = a.alloc_register(ValueType::I64);
    a.host_call_sig_id(
        Reg::EFFECT,
        miss_request,
        Reg::EFFECT,
        &[Reg(1), Reg(2)],
        &[],
    )
    .call(Reg::EFFECT, DISPATCH, Reg::EFFECT, &[Reg(3)], &[out])
    .ret(Reg::EFFECT, &[out]);
    b.define_function(miss, a).unwrap();

    let link = b
        .declare_guarded_link("area", CIRCLE_AREA, 1, MISS)
        .unwrap();
    assert_eq!(link, AREA, "the only link");
    let send = b.declare_function(unary).unwrap();
    assert_eq!(send, SEND, "the send comes last");
    let mut a = b.function_asm(send).unwrap();
    let key = a.alloc_register(ValueType::U64);
    a.obj_key(key, Reg(1));
    let [out] = b
        .emit_guarded_call(&mut a, AREA, &[key], &[Reg(1)])
        .unwrap();
    a.ret(Reg::EFFECT, &[out]);
    b.define_function(send, a).unwrap();
    Arc::new(b.build_verified().unwrap())
}

/// The method `key` selects.
fn method(unit: UnitId, key: u64) -> FuncRef {
    FuncRef::new(
        unit,
        if key == CIRCLE {
            CIRCLE_AREA
        } else {
            SQUARE_AREA
        },
    )
}

/// Fills a guard while the miss request waits: `(request, caller unit, link, key)`.
type Fill<'f> = dyn FnMut(&mut PendingHostCall<'_, '_, Shapes>, UnitId, LinkId, u64) + 'f;

/// What the driver saw of one send.
#[derive(Debug, PartialEq, Eq)]
struct Sent {
    area: i64,
    /// `(link, key)` of each miss request.
    misses: Vec<(u64, u64)>,
}

/// Sends `area` to a shape of `class`, filling the guard with each miss's key through `fill`.
fn send(vm: &mut Vm<Shapes>, unit: UnitId, class: u64, fill: &mut Fill<'_>) -> Sent {
    let args = [shape(class)];
    let mut turn = vm
        .start(
            FuncRef::new(unit, SEND),
            &args,
            TurnOptions::default().yield_host_calls(),
        )
        .unwrap();
    let mut misses = Vec::new();
    loop {
        match turn.drive(u64::MAX).unwrap() {
            RunStatus::Complete => break,
            RunStatus::AwaitingHost => {
                let mut request = turn.pending_host_call().unwrap();
                let arguments: Vec<_> = request.arguments().map(|v| v.to_value()).collect();
                let [Value::U64(link), Value::U64(key)] = arguments[..] else {
                    panic!("miss(link, key)");
                };
                misses.push((link, key));
                let caller = request.location().unit;
                fill(
                    &mut request,
                    caller,
                    LinkId(u32::try_from(link).unwrap()),
                    key,
                );
                request.complete(&mut [], Ok(0)).unwrap();
            }
            status => panic!("{status:?}"),
        }
    }
    let Ok(TurnOutcome::Returned(values)) = turn.finish() else {
        panic!("the send returned");
    };
    let [Value::I64(area)] = values[..] else {
        panic!("one area");
    };
    Sent { area, misses }
}

/// Fills the guard with the missed key and the method it selects.
fn fill_keys(request: &mut PendingHostCall<'_, '_, Shapes>, unit: UnitId, link: LinkId, key: u64) {
    let keys = GuardKeys::new(&[key]).unwrap();
    request
        .relink_guarded(unit, link, LinkGuard::Keys(keys), method(unit, key))
        .unwrap();
}

fn guarded(vm: &Vm<Shapes>, unit: UnitId) -> GuardedLink {
    vm.units().guarded_link(unit, AREA).unwrap()
}

#[test]
fn a_miss_fills_the_guard_and_matching_keys_then_reach_the_target() {
    let mut vm = Vm::new(Shapes, Limits::default());
    let unit = vm.install(program()).unwrap();
    let start = guarded(&vm, unit);
    assert_eq!(
        (start.link, start.key_count, start.guard, start.misses),
        (AREA, 1, LinkGuard::Unfilled, 0)
    );
    assert_eq!(start.target, FuncRef::new(unit, CIRCLE_AREA));
    assert_eq!(start.fallback, FuncRef::new(unit, MISS));

    // The first send misses: the fallback reports the link and key, then dispatches.
    assert_eq!(
        send(&mut vm, unit, CIRCLE, &mut fill_keys),
        Sent {
            area: 1,
            misses: vec![(u64::from(AREA.0), CIRCLE)],
        }
    );
    let filled = guarded(&vm, unit);
    assert_eq!(
        filled.guard,
        LinkGuard::Keys(GuardKeys::new(&[CIRCLE]).unwrap())
    );
    assert_eq!(filled.misses, 1);

    // The same class hits the guard without a request.
    for _ in 0..3 {
        assert_eq!(
            send(&mut vm, unit, CIRCLE, &mut fill_keys),
            Sent {
                area: 1,
                misses: vec![],
            }
        );
    }
    // Another class misses, and refilling moves the guard to it.
    assert_eq!(
        send(&mut vm, unit, SQUARE, &mut fill_keys),
        Sent {
            area: 2,
            misses: vec![(0, SQUARE)],
        }
    );
    let refilled = guarded(&vm, unit);
    assert_eq!(
        (refilled.guard, refilled.target, refilled.misses),
        (
            LinkGuard::Keys(GuardKeys::new(&[SQUARE]).unwrap()),
            FuncRef::new(unit, SQUARE_AREA),
            2
        )
    );
    assert_eq!(
        vm.units().guarded_links(unit).collect::<Vec<_>>(),
        [refilled]
    );
    assert_eq!(
        vm.units().link_guards(unit).unwrap(),
        [Some(refilled.guard)]
    );
}

#[test]
fn a_megamorphic_guard_calls_the_target_for_every_key() {
    let mut vm = Vm::new(Shapes, Limits::default());
    let unit = vm.install(program()).unwrap();
    vm.relink_guarded(
        unit,
        AREA,
        LinkGuard::Megamorphic,
        FuncRef::new(unit, DISPATCH),
    )
    .unwrap();
    let mut never = |_: &mut PendingHostCall<'_, '_, Shapes>, _, _, _| panic!("no misses");
    for (class, area) in [(CIRCLE, 1), (SQUARE, 2), (TRIANGLE, 2)] {
        assert_eq!(
            send(&mut vm, unit, class, &mut never),
            Sent {
                area,
                misses: vec![]
            }
        );
    }
    assert_eq!(guarded(&vm, unit).misses, 0);

    // Unfilling sends every call back to the fallback, as after new code replaces methods.
    vm.relink_guarded(
        unit,
        AREA,
        LinkGuard::Unfilled,
        FuncRef::new(unit, DISPATCH),
    )
    .unwrap();
    let mut keep = |_: &mut PendingHostCall<'_, '_, Shapes>, _, _, _| {};
    for class in [CIRCLE, CIRCLE] {
        assert_eq!(send(&mut vm, unit, class, &mut keep).misses, [(0, class)]);
    }
    assert_eq!(guarded(&vm, unit).misses, 2);
}

#[test]
fn every_install_starts_unfilled_and_guards_are_per_unit() {
    let program = program();
    let mut vm = Vm::new(Shapes, Limits::default());
    let first = vm.install(Arc::clone(&program)).unwrap();
    send(&mut vm, first, CIRCLE, &mut fill_keys);
    let second = vm.install(program).unwrap();
    assert_eq!(guarded(&vm, second).guard, LinkGuard::Unfilled);
    assert_ne!(guarded(&vm, first).guard, LinkGuard::Unfilled);
    // A guard may name another unit's method; the signature is what is checked.
    vm.relink_guarded(
        first,
        AREA,
        LinkGuard::Keys(GuardKeys::new(&[CIRCLE]).unwrap()),
        FuncRef::new(second, CIRCLE_AREA),
    )
    .unwrap();
    assert_eq!(
        send(&mut vm, first, CIRCLE, &mut fill_keys),
        Sent {
            area: 1,
            misses: vec![]
        }
    );
}

#[test]
fn relinks_refuse_the_wrong_kind_of_link_keys_or_target() {
    let mut b = ProgramBuilder::new();
    let unary = FunctionSig {
        arg_types: vec![ValueType::I64],
        ret_types: vec![ValueType::I64],
    };
    let target = b.declare_function(unary.clone()).unwrap();
    let mut a = b.function_asm(target).unwrap();
    a.ret(Reg::EFFECT, &[Reg(1)]);
    b.define_function(target, a).unwrap();
    let plain = b.declare_link("plain", target).unwrap();
    let _ = unary;
    let program = Arc::new(b.build_verified().unwrap());
    let mut vm = Vm::new(Shapes, Limits::default());
    let unit = vm.install(program).unwrap();
    let guarded_unit = vm.install(self::program()).unwrap();
    let target = FuncRef::new(unit, target);
    let keys = |keys: &[u64]| LinkGuard::Keys(GuardKeys::new(keys).unwrap());

    assert_eq!(
        vm.relink_guarded(unit, plain, keys(&[1]), target),
        Err(RelinkError::Unguarded { unit, link: plain })
    );
    let unit = guarded_unit;
    assert_eq!(
        vm.relink(unit, AREA, FuncRef::new(unit, SQUARE_AREA)),
        Err(RelinkError::Guarded { unit, link: AREA })
    );
    assert_eq!(
        vm.relink_guarded(unit, AREA, keys(&[1, 2]), FuncRef::new(unit, SQUARE_AREA)),
        Err(RelinkError::GuardKeys {
            unit,
            link: AREA,
            expected: 1,
            actual: 2,
        })
    );
    assert!(matches!(
        vm.relink_guarded(unit, AREA, keys(&[1]), target),
        Err(RelinkError::SignatureMismatch { .. })
    ));
    // Refusals change nothing.
    assert_eq!(guarded(&vm, unit).guard, LinkGuard::Unfilled);
    assert_eq!(guarded(&vm, unit).target, FuncRef::new(unit, CIRCLE_AREA));

    assert_eq!(GuardKeys::new(&[]), None);
    assert_eq!(GuardKeys::new(&[1; 5]), None);
    assert_eq!(GuardKeys::new(&[3, 4]).unwrap().as_slice(), [3, 4]);
}

/// A program with `f(x: i64) -> i64`, a fallback of `fallback` arguments, and one guarded
/// link from `f` comparing `keys` keys; `call` emits the call site through `emit`.
fn guarded_program(key_count: u32, fallback: &[ValueType]) -> Program {
    let mut b = ProgramBuilder::new();
    let f = b
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let mut a = b.function_asm(f).unwrap();
    a.ret(Reg::EFFECT, &[Reg(1)]);
    b.define_function(f, a).unwrap();
    let miss = b
        .declare_function(FunctionSig {
            arg_types: fallback.to_vec(),
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let mut a = b.function_asm(miss).unwrap();
    let out = a.alloc_register(ValueType::I64);
    a.const_i64(out, 0).ret(Reg::EFFECT, &[out]);
    b.define_function(miss, a).unwrap();
    b.declare_guarded_link("f", f, 1, miss).unwrap();
    let mut program = b.build();
    program.links[0].guard = Some(LinkGuardEntry {
        key_count,
        fallback: miss,
    });
    program
}

#[test]
fn the_verifier_checks_guards_and_their_fallbacks() {
    use ValueType::{I64, U64};
    let verify = |program: &Program| verify_program(program, &VerifyConfig::default());
    assert!(verify(&guarded_program(1, &[U64, U64, I64])).is_ok());
    assert!(verify(&guarded_program(2, &[U64, U64, U64, I64])).is_ok());
    for keys in [0, 5] {
        assert_eq!(
            verify(&guarded_program(keys, &[U64, U64, I64])).unwrap_err(),
            VerifyError::LinkGuardInvalid { link: LinkId(0) }
        );
    }
    for fallback in [&[U64, I64][..], &[U64, U64, U64, I64], &[I64, U64, I64]] {
        assert_eq!(
            verify(&guarded_program(1, fallback)).unwrap_err(),
            VerifyError::LinkFallbackMismatch {
                link: LinkId(0),
                fallback: FuncId(1),
            },
            "{fallback:?}"
        );
    }

    // `call.link` cannot bypass a guard, and `call.link.guard` needs a guarded link and its keys.
    let mut b = ProgramBuilder::new();
    let unary = FunctionSig {
        arg_types: vec![I64],
        ret_types: vec![I64],
    };
    let f = b.declare_function(unary.clone()).unwrap();
    let mut a = b.function_asm(f).unwrap();
    a.ret(Reg::EFFECT, &[Reg(1)]);
    b.define_function(f, a).unwrap();
    let miss = b
        .declare_function(FunctionSig {
            arg_types: vec![U64, U64, I64],
            ret_types: vec![I64],
        })
        .unwrap();
    let mut a = b.function_asm(miss).unwrap();
    a.ret(Reg::EFFECT, &[Reg(3)]);
    b.define_function(miss, a).unwrap();
    let plain = b.declare_link("plain", f).unwrap();
    let guarded = b.declare_guarded_link("guarded", f, 1, miss).unwrap();
    assert_eq!(
        b.declare_guarded_link("none", f, 0, miss),
        Err(LinkBuildError::GuardKeys(0))
    );
    let caller = b.declare_function(unary).unwrap();
    let mut a = b.function_asm(caller).unwrap();
    let key = a.alloc_register(U64);
    assert_eq!(
        b.emit_call::<1>(&mut a, guarded, &[Reg(1)]),
        Err(CallBuildError::GuardKeys {
            link: Some(guarded),
            expected: 1,
            actual: 0,
        })
    );
    assert_eq!(
        b.emit_guarded_call::<1>(&mut a, plain, &[key], &[Reg(1)]),
        Err(CallBuildError::GuardKeys {
            link: None,
            expected: 0,
            actual: 1,
        })
    );
    assert_eq!(
        b.emit_guarded_call::<1>(&mut a, guarded, &[Reg(1)], &[Reg(1)]),
        Err(CallBuildError::GuardKeyKind {
            register: Reg(1),
            actual: I64,
        })
    );
    let out = a.alloc_register(I64);
    let cases: [(LinkId, &[Reg], usize, usize); 3] = [
        (plain, &[key], 0, 1),
        (guarded, &[], 1, 0),
        (guarded, &[key, key], 1, 2),
    ];
    for (link, keys, expected, actual) in cases {
        let mut site = a.clone();
        site.const_u64(key, 1);
        if keys.is_empty() && link == guarded {
            site.call_link(Reg::EFFECT, link, Reg::EFFECT, &[Reg(1)], &[out]);
        } else {
            site.call_link_guard(Reg::EFFECT, link, Reg::EFFECT, keys, &[Reg(1)], &[out]);
        }
        site.ret(Reg::EFFECT, &[out]);
        let mut program = b.clone();
        program.define_function(caller, site).unwrap();
        let error = verify(&program.build()).unwrap_err();
        assert!(
            matches!(
                error,
                VerifyError::LinkGuardKeys { link: l, expected: e, actual: x, .. }
                    if l == link && e == expected && x == actual
            ),
            "{error:?}"
        );
    }
}

#[test]
fn guards_survive_encoding_and_disassemble_with_their_keys() {
    let program = program();
    let bytes = program.program().encode();
    let decoded = Program::decode(&bytes).unwrap();
    assert_eq!(decoded.links, program.program().links);
    assert_eq!(
        decoded.link(AREA).unwrap().guard,
        Some(LinkGuardEntry {
            key_count: 1,
            fallback: MISS
        })
    );
    let send = execution_tape::disasm::disassemble_function(program.program(), SEND).unwrap();
    let call = send.instrs().nth(1).unwrap().to_string();
    assert!(call.contains("call.link.guard"), "{call}");
    assert!(call.contains("keys=[r2]"), "{call}");
}

#[test]
fn recordings_repeat_guard_fills_and_restore_guards_at_entry() {
    let program = program();
    let mut vm = Vm::new(Shapes, Limits::default());
    let unit = vm.install(Arc::clone(&program)).unwrap();
    // A guard filled before the recorded turn is restored at entry.
    send(&mut vm, unit, SQUARE, &mut fill_keys);

    let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
    let mask = recorder.mask();
    let args = [shape(CIRCLE)];
    let mut turn = vm
        .start(
            FuncRef::new(unit, SEND),
            &args,
            TurnOptions::default()
                .yield_host_calls()
                .trace(mask, &mut recorder),
        )
        .unwrap();
    assert!(matches!(
        turn.drive(u64::MAX).unwrap(),
        RunStatus::AwaitingHost
    ));
    let mut request = turn.pending_host_call().unwrap();
    fill_keys(&mut request, unit, AREA, CIRCLE);
    request.complete(&mut [], Ok(0)).unwrap();
    assert!(matches!(turn.drive(u64::MAX).unwrap(), RunStatus::Complete));
    assert!(matches!(turn.finish(), Ok(TurnOutcome::Returned(_))));
    let recording = recorder.finish();
    recording.status().unwrap();
    let recorded = recording.unit(unit).unwrap();
    assert_eq!(
        recorded.guards,
        [Some(LinkGuard::Keys(GuardKeys::new(&[SQUARE]).unwrap()))]
    );
    let relinks: Vec<_> = recording
        .store()
        .filter_map(|record| match record {
            StoreRecord::Relinked(relink) => Some((relink.link, relink.guard, relink.target)),
            _ => None,
        })
        .collect();
    assert_eq!(
        relinks,
        [(
            AREA,
            Some(LinkGuard::Keys(GuardKeys::new(&[CIRCLE]).unwrap())),
            FuncRef::new(unit, CIRCLE_AREA)
        )]
    );
    let ReplayOutcome::Returned(snapshot) = recording.replay().unwrap() else {
        panic!("the replay returned");
    };
    assert_eq!(snapshot.values(), [Value::I64(1)]);
}

#[test]
fn proved_arguments_are_proved_for_target_and_fallback_alike() {
    let program = program();
    let send = program.function(SEND).unwrap();
    let call = send
        .instructions()
        .find(|info| info.opcode == execution_tape::opcode::Opcode::CallLinkGuard)
        .unwrap();
    assert_eq!(call.pc, Pc(4));
    assert!(call.boundary_proved);
}

#[test]
fn a_guard_changed_while_the_computation_is_saved_replays() {
    let program = program();
    let cases = [
        (
            LinkGuard::Keys(GuardKeys::new(&[CIRCLE]).unwrap()),
            SQUARE_AREA,
            2,
        ),
        // Only the guard changes; the target stays.
        (LinkGuard::Megamorphic, CIRCLE_AREA, 1),
    ];
    for (guard, target, area) in cases {
        let mut vm = Vm::new(Shapes, Limits::default());
        let unit = vm.install(Arc::clone(&program)).unwrap();
        let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
        let mask = recorder.mask();
        let args = [shape(CIRCLE)];
        // The first send misses; the turn saves while its request waits.
        let mut turn = vm
            .start(
                FuncRef::new(unit, SEND),
                &args,
                TurnOptions::default()
                    .yield_host_calls()
                    .trace(mask, &mut recorder),
            )
            .unwrap();
        assert!(matches!(
            turn.drive(u64::MAX).unwrap(),
            RunStatus::AwaitingHost
        ));
        let Ok(TurnOutcome::Saved(saved)) = turn.finish() else {
            panic!("the turn saved its computation");
        };
        vm.relink_guarded(unit, AREA, guard, FuncRef::new(unit, target))
            .unwrap();
        let mut turn = vm
            .resume(
                saved,
                TurnOptions::default()
                    .yield_host_calls()
                    .trace(mask, &mut recorder),
            )
            .unwrap();
        turn.pending_host_call()
            .unwrap()
            .complete(&mut [], Ok(0))
            .unwrap();
        assert!(matches!(turn.drive(u64::MAX).unwrap(), RunStatus::Complete));
        assert!(matches!(turn.finish(), Ok(TurnOutcome::Returned(_))));
        // A second send in the same recording sees the changed guard.
        let mut recorded = Recorder::new(Arc::clone(&program), RecordLimits::default());
        let mask = recorded.mask();
        let mut turn = vm
            .start(
                FuncRef::new(unit, SEND),
                &args,
                TurnOptions::default()
                    .yield_host_calls()
                    .trace(mask, &mut recorded),
            )
            .unwrap();
        assert!(matches!(turn.drive(u64::MAX).unwrap(), RunStatus::Complete));
        let Ok(TurnOutcome::Returned(values)) = turn.finish() else {
            panic!("the send returned");
        };
        assert_eq!(values, [Value::I64(area)]);
        for recording in [recorder.finish(), recorded.finish()] {
            recording.status().unwrap();
            assert!(matches!(
                recording.replay().unwrap(),
                ReplayOutcome::Returned(_)
            ));
        }
    }
}

#[test]
fn a_guard_compares_every_key() {
    // `pair(a, b) = f(a, b)` through a link guarded on both keys.
    let object = ValueType::Obj(SHAPES);
    let binary = FunctionSig {
        arg_types: vec![object, object],
        ret_types: vec![ValueType::I64],
    };
    let mut b = ProgramBuilder::new();
    let f = b.declare_function(binary.clone()).unwrap();
    let mut a = b.function_asm(f).unwrap();
    let out = a.alloc_register(ValueType::I64);
    a.const_i64(out, 1).ret(Reg::EFFECT, &[out]);
    b.define_function(f, a).unwrap();
    let miss = b
        .declare_function(FunctionSig {
            arg_types: vec![
                ValueType::U64,
                ValueType::U64,
                ValueType::U64,
                object,
                object,
            ],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let mut a = b.function_asm(miss).unwrap();
    let out = a.alloc_register(ValueType::I64);
    a.const_i64(out, 0).ret(Reg::EFFECT, &[out]);
    b.define_function(miss, a).unwrap();
    let link = b.declare_guarded_link("pair", f, 2, miss).unwrap();
    let pair = b.declare_function(binary).unwrap();
    let mut a = b.function_asm(pair).unwrap();
    let (first, second) = (
        a.alloc_register(ValueType::U64),
        a.alloc_register(ValueType::U64),
    );
    a.obj_key(first, Reg(1)).obj_key(second, Reg(2));
    let [out] = b
        .emit_guarded_call(&mut a, link, &[first, second], &[Reg(1), Reg(2)])
        .unwrap();
    a.ret(Reg::EFFECT, &[out]);
    b.define_function(pair, a).unwrap();
    let mut vm = Vm::new(Shapes, Limits::default());
    let unit = vm.install(b.build_verified().unwrap()).unwrap();
    vm.relink_guarded(
        unit,
        link,
        LinkGuard::Keys(GuardKeys::new(&[CIRCLE, SQUARE]).unwrap()),
        FuncRef::new(unit, f),
    )
    .unwrap();
    // 1 from the target, 0 from the fallback.
    for (keys, expected) in [
        ((CIRCLE, SQUARE), 1),
        ((CIRCLE, CIRCLE), 0),
        ((SQUARE, SQUARE), 0),
        ((SQUARE, CIRCLE), 0),
    ] {
        assert_eq!(
            vm.run(
                FuncRef::new(unit, pair),
                &[shape(keys.0), shape(keys.1)],
                TraceMask::NONE,
                None
            )
            .unwrap(),
            [Value::I64(expected)],
            "{keys:?}"
        );
    }
    assert_eq!(vm.units().guarded_link(unit, link).unwrap().misses, 3);
}

/// The payload range of section `tag` of an encoded program.
fn section(bytes: &[u8], tag: u8) -> Option<core::ops::Range<usize>> {
    // Magic, then two `u16le` version numbers.
    let mut at = 12;
    while at < bytes.len() {
        let found = bytes[at];
        let (mut len, mut shift, mut next) = (0, 0, at + 1);
        loop {
            let byte = bytes[next];
            next += 1;
            len |= usize::from(byte & 0x7f) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                break;
            }
        }
        if found == tag {
            return Some(next..next + len);
        }
        at = next + len;
    }
    None
}

#[test]
fn malformed_guard_sections_are_rejected() {
    let bytes = program().program().encode();
    let payload = section(&bytes, 18).expect("a link guard section");
    // One guard: link 0, one key, fallback MISS.
    assert_eq!(&bytes[payload.clone()], [1, 0, 1, 3]);
    let changed = |offset: usize, byte: u8| {
        let mut bytes = bytes.clone();
        bytes[payload.start + offset] = byte;
        Program::decode(&bytes)
    };
    let verify = |program: Program| verify_program(&program, &VerifyConfig::default());
    assert!(Program::decode(&bytes).is_ok());
    assert!(
        changed(1, 5).is_err(),
        "a guard for a link that does not exist"
    );
    assert_eq!(
        verify(changed(2, 0).unwrap()).unwrap_err(),
        VerifyError::LinkGuardInvalid { link: AREA }
    );
    assert_eq!(
        verify(changed(3, 99).unwrap()).unwrap_err(),
        VerifyError::LinkGuardInvalid { link: AREA }
    );
    assert_eq!(
        verify(changed(3, u8::try_from(SEND.0).unwrap()).unwrap()).unwrap_err(),
        VerifyError::LinkFallbackMismatch {
            link: AREA,
            fallback: SEND,
        }
    );
    // Without the section the link is plain, and its guarded call site is refused.
    let mut unguarded = bytes[..payload.start - 2].to_vec();
    unguarded.extend_from_slice(&bytes[payload.end..]);
    assert!(matches!(
        verify(Program::decode(&unguarded).unwrap()).unwrap_err(),
        VerifyError::LinkGuardKeys {
            link: AREA,
            expected: 0,
            actual: 1,
            ..
        }
    ));
}
