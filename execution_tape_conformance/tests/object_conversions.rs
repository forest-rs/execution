// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `obj.convert` changes a host object's representation, such as unboxing a boxed value,
//! straight through the host: no host call, request or allowance, one unit of fuel, and a trap
//! when the host has no such conversion.

use execution_tape::{
    asm::{FunctionSig, ProgramBuilder, Reg},
    host::{Host, HostContext, HostError, SigHash, ValueRef},
    host_binding::{HostFunctions, HostImport},
    opcode::{Opcode, OpcodePrefix},
    program::{HostTypeId, Pc, Program, ValueType},
    recording::{ObjectAnswer, RecordLimits, Recorder, ReplayOutcome},
    trace::{TraceMask, TraceSink, ValueEvent},
    value::{FuncId, FuncRef, Obj, ObjHandle, Value},
    verifier::{VerifiedProgram, VerifyConfig, VerifyError, verify_program},
    vm::{Limits, RunStatus, Trap, TurnOptions, TurnOutcome, Vm},
};
use std::sync::Arc;

const OBJECTS: HostTypeId = HostTypeId(0x4f42_4a53);
const BOXED: HostTypeId = HostTypeId(0x424f_5845);
const OTHER: HostTypeId = HostTypeId(0x4f54_4852);
/// Set in a boxed object's handle; boxed handles without it hold no object.
const BOX_TAG: u64 = 1 << 63;

/// Boxes and unboxes objects by tagging their handles; the key of either form is its handle.
/// A `liar` answers conversions with an object of the wrong host type.
struct Boxes {
    calls: usize,
    liar: bool,
}
impl Host for Boxes {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        _: &[ValueRef<'_>],
        _: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        self.calls += 1;
        Err(HostError::UnknownSymbol)
    }

    fn object_key(&self, object: Obj) -> Option<u64> {
        Some(object.handle.0 & !BOX_TAG)
    }

    fn convert_object(&self, object: Obj, to: HostTypeId) -> Option<Obj> {
        convert(object, if self.liar { OTHER } else { to })
    }
}

fn convert(object: Obj, to: HostTypeId) -> Option<Obj> {
    let handle = object.handle.0;
    let handle = match (object.host_type, to) {
        (BOXED, OBJECTS) if handle & BOX_TAG != 0 => handle & !BOX_TAG,
        (OBJECTS, BOXED) => handle | BOX_TAG,
        (BOXED, OTHER) => handle,
        _ => return None,
    };
    Some(Obj {
        host_type: to,
        handle: ObjHandle(handle),
    })
}

fn object(host_type: HostTypeId, handle: u64) -> Obj {
    Obj {
        host_type,
        handle: ObjHandle(handle),
    }
}

/// `unbox(boxed) -> object` and `rebox(boxed) -> boxed`, which unboxes, reads the object's key
/// and boxes it again.
fn program() -> (Arc<VerifiedProgram>, FuncId, FuncId) {
    let mut b = ProgramBuilder::new();
    let unbox = b
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::Obj(BOXED)],
            ret_types: vec![ValueType::Obj(OBJECTS)],
        })
        .unwrap();
    let mut a = b.function_asm(unbox).unwrap();
    let out = a.alloc_register(ValueType::Obj(OBJECTS));
    a.obj_convert(out, Reg(1)).ret(Reg::EFFECT, &[out]);
    b.define_function(unbox, a).unwrap();

    let rebox = b
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::Obj(BOXED)],
            ret_types: vec![ValueType::Obj(BOXED), ValueType::U64],
        })
        .unwrap();
    let mut a = b.function_asm(rebox).unwrap();
    let unboxed = a.alloc_register(ValueType::Obj(OBJECTS));
    let key = a.alloc_register(ValueType::U64);
    let boxed = a.alloc_register(ValueType::Obj(BOXED));
    a.obj_convert(unboxed, Reg(1))
        .obj_key(key, unboxed)
        .obj_convert(boxed, unboxed)
        .ret(Reg::EFFECT, &[boxed, key]);
    b.define_function(rebox, a).unwrap();
    (Arc::new(b.build_verified().unwrap()), unbox, rebox)
}

fn boxes(liar: bool) -> (Vm<Boxes>, FuncRef, FuncRef) {
    let (program, unbox, rebox) = program();
    let mut vm = Vm::new(
        Boxes { calls: 0, liar },
        Limits {
            max_host_calls: 0,
            ..Limits::default()
        },
    );
    let unit = vm.install(program).unwrap();
    (vm, FuncRef::new(unit, unbox), FuncRef::new(unit, rebox))
}

#[test]
fn obj_convert_unboxes_and_boxes_without_a_host_call() {
    let (mut vm, unbox, rebox) = boxes(false);
    let boxed = Value::Obj(object(BOXED, BOX_TAG | 5));
    assert_eq!(
        vm.run(unbox, std::slice::from_ref(&boxed), TraceMask::NONE, None)
            .unwrap(),
        [Value::Obj(object(OBJECTS, 5))]
    );
    assert_eq!(
        vm.run(rebox, std::slice::from_ref(&boxed), TraceMask::NONE, None)
            .unwrap(),
        [boxed.clone(), Value::U64(5)]
    );
    assert_eq!(vm.host().calls, 0);

    // A yielding turn does not stop at a conversion either.
    let args = [boxed];
    let mut turn = vm
        .start(unbox, &args, TurnOptions::default().yield_host_calls())
        .unwrap();
    assert!(matches!(turn.drive(u64::MAX).unwrap(), RunStatus::Complete));
    assert!(matches!(turn.finish(), Ok(TurnOutcome::Returned(_))));
}

#[test]
fn a_missing_or_mistyped_conversion_traps() {
    let (mut vm, unbox, _) = boxes(false);
    // A boxed value that holds no object.
    let trap = vm
        .run(
            unbox,
            &[Value::Obj(object(BOXED, 5))],
            TraceMask::NONE,
            None,
        )
        .unwrap_err();
    assert_eq!((trap.func, trap.pc), (unbox.func, Pc(0)));
    let Trap::NoObjectConversion {
        object: from,
        to,
        converted,
    } = trap.trap
    else {
        panic!("expected NoObjectConversion, got {:?}", trap.trap);
    };
    assert_eq!((from, to, converted), (object(BOXED, 5), OBJECTS, None));
    assert_eq!(
        trap.trap.to_string(),
        "the host cannot convert object 0x5 of host type 0x424f5845 to host type 0x4f424a53"
    );

    // A host that answers with an object of another host type cannot put it in the register.
    let (mut vm, unbox, _) = boxes(true);
    let trap = vm
        .run(
            unbox,
            &[Value::Obj(object(BOXED, BOX_TAG | 5))],
            TraceMask::NONE,
            None,
        )
        .unwrap_err()
        .trap;
    assert!(matches!(
        trap,
        Trap::NoObjectConversion { to: OBJECTS, converted: Some(answer), .. }
            if answer == object(OTHER, BOX_TAG | 5)
    ));
    assert_eq!(
        trap.to_string(),
        "the host cannot convert object 0x8000000000000005 of host type 0x424f5845 to host \
         type 0x4f424a53: it answered with host type 0x4f544852"
    );
}

#[test]
fn the_verifier_requires_objects_on_both_sides() {
    let build = |dst: ValueType, src: ValueType| {
        let mut b = ProgramBuilder::new();
        let f = b
            .declare_function(FunctionSig {
                arg_types: vec![src],
                ret_types: vec![],
            })
            .unwrap();
        let mut a = b.function_asm(f).unwrap();
        let out = a.alloc_register(dst);
        a.obj_convert(out, Reg(1)).ret(Reg::EFFECT, &[]);
        b.define_function(f, a).unwrap();
        b.build()
    };
    let verify = |program: &Program| verify_program(program, &VerifyConfig::default());
    assert!(verify(&build(ValueType::Obj(OTHER), ValueType::Obj(BOXED))).is_ok());
    assert_eq!(
        verify(&build(ValueType::U64, ValueType::Obj(BOXED))).unwrap_err(),
        VerifyError::ObjectRegisterRequired {
            func: FuncId(0),
            pc: Pc(0),
            reg: Reg(2),
            declared: ValueType::U64,
        }
    );
    assert_eq!(
        verify(&build(ValueType::Obj(OBJECTS), ValueType::Str)).unwrap_err(),
        VerifyError::ObjectRegisterRequired {
            func: FuncId(0),
            pc: Pc(0),
            reg: Reg(1),
            declared: ValueType::Str,
        }
    );
}

#[test]
fn obj_convert_is_a_prefixed_opcode_with_a_disassembly() {
    assert_eq!(Opcode::ObjConvert.prefix(), Some(OpcodePrefix::Extended));
    assert_eq!(Opcode::ObjConvert.code(), 0xFC0E);
    assert_eq!(Opcode::ObjConvert.mnemonic(), "obj.convert");
    let (program, unbox, _) = program();
    let disassembly =
        execution_tape::disasm::disassemble_function(program.program(), unbox).unwrap();
    let first = disassembly.instrs().next().unwrap();
    assert!(
        first.to_string().ends_with("obj.convert r2, [r1]"),
        "{first}"
    );
}

#[derive(Default)]
struct Conversions(Vec<(Obj, HostTypeId, Option<Obj>)>);
impl TraceSink for Conversions {
    fn mask(&self) -> TraceMask {
        TraceMask::VALUES
    }
    fn values(&mut self, _: &Program, event: ValueEvent<'_>) {
        if let ValueEvent::ObjectConversion {
            object,
            to,
            converted,
            ..
        } = event
        {
            self.0.push((object, to, converted));
        }
    }
}

#[test]
fn observers_see_each_conversion_including_refusals() {
    let (mut vm, unbox, _) = boxes(false);
    let mut seen = Conversions::default();
    let boxed = object(BOXED, BOX_TAG | 3);
    vm.run(unbox, &[Value::Obj(boxed)], seen.mask(), Some(&mut seen))
        .unwrap();
    vm.run(
        unbox,
        &[Value::Obj(object(BOXED, 3))],
        seen.mask(),
        Some(&mut seen),
    )
    .unwrap_err();
    assert_eq!(
        seen.0,
        [
            (boxed, OBJECTS, Some(object(OBJECTS, 3))),
            (object(BOXED, 3), OBJECTS, None),
        ]
    );
}

#[test]
fn recordings_keep_conversions_with_keys_and_replay_them_in_order() {
    let (program, _, rebox) = program();
    for handle in [BOX_TAG | 9, 9] {
        let mut vm = Vm::new(
            Boxes {
                calls: 0,
                liar: false,
            },
            Limits::default(),
        );
        let unit = vm.install(Arc::clone(&program)).unwrap();
        let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
        let live = vm.run(
            FuncRef::new(unit, rebox),
            &[Value::Obj(object(BOXED, handle))],
            recorder.mask(),
            Some(&mut recorder),
        );
        let recording = recorder.finish();
        recording.status().unwrap();
        let answers: Vec<_> = recording
            .object_queries()
            .iter()
            .map(|query| query.answer)
            .collect();
        match (live, recording.replay().unwrap()) {
            (Ok(values), ReplayOutcome::Returned(snapshot)) => {
                assert_eq!(
                    answers,
                    [
                        ObjectAnswer::Converted {
                            to: OBJECTS,
                            converted: Some(object(OBJECTS, 9)),
                        },
                        ObjectAnswer::Key(Some(9)),
                        ObjectAnswer::Converted {
                            to: BOXED,
                            converted: Some(object(BOXED, BOX_TAG | 9)),
                        },
                    ]
                );
                assert_eq!(snapshot.values(), values);
            }
            (Err(_), ReplayOutcome::Trapped(trap)) => {
                assert_eq!(
                    answers,
                    [ObjectAnswer::Converted {
                        to: OBJECTS,
                        converted: None,
                    }]
                );
                assert!(matches!(trap.trap, Trap::NoObjectConversion { .. }));
            }
            (live, replayed) => panic!("live {live:?}, replayed {replayed:?}"),
        }
    }
}

#[test]
fn bound_hosts_convert_from_their_state() {
    const PING: HostImport = HostImport::new("ping", &[], &[]);
    let (program, unbox, _) = program();
    let mut functions = HostFunctions::<u64>::new();
    functions.implement(PING, |_, _, _, _| Ok(0)).unwrap();
    let bindings = functions.bind(&program).unwrap();
    let run = |host| {
        let mut vm = Vm::new(host, Limits::default());
        let unit = vm.install(Arc::clone(&program)).unwrap();
        vm.run(
            FuncRef::new(unit, unbox),
            &[Value::Obj(object(BOXED, 1))],
            TraceMask::NONE,
            None,
        )
    };
    let converting = bindings
        .with_state(10)
        .with_object_conversion(|offset, object, to| {
            Some(Obj {
                host_type: to,
                handle: ObjHandle(object.handle.0 + offset),
            })
        });
    assert_eq!(run(converting).unwrap(), [Value::Obj(object(OBJECTS, 11))]);
    // Without a callback nothing converts.
    assert!(matches!(
        run(bindings.with_state(10)).unwrap_err().trap,
        Trap::NoObjectConversion { .. }
    ));
}
