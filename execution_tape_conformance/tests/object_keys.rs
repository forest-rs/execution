// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `obj.key` reads a host object's key, such as its class, straight from the host: no host
//! call, request or allowance, one unit of fuel, and a trap when the host has no key.

use execution_tape::{
    asm::{FunctionSig, ProgramBuilder, Reg},
    host::{Host, HostContext, HostError, SigHash, ValueRef},
    host_binding::{HostFunctions, HostImport},
    opcode::{Opcode, OpcodePrefix},
    program::{HostTypeId, Pc, Program, ValueType},
    recording::{ObjectAnswer, RecordLimits, Recorder, ReplayOutcome},
    trace::{ScopeKind, TraceLocation, TraceMask, TraceSink, ValueEvent},
    value::{FuncId, FuncRef, Obj, ObjHandle, Value},
    verifier::{VerifiedProgram, VerifyConfig, VerifyError, verify_program},
    vm::{Limits, RunStatus, Trap, TurnOptions, TurnOutcome, Vm},
};
use std::sync::Arc;

const SHAPES: HostTypeId = HostTypeId(0x5348_4150);
const OTHER: HostTypeId = HostTypeId(0x4f54_4852);
const CIRCLE: u64 = 7;
const SQUARE: u64 = 9;

/// Objects whose handle names their class: handle 1 is a circle, 2 a square; nothing else
/// has a key.
struct Shapes {
    calls: usize,
}
impl Shapes {
    fn class(object: Obj) -> Option<u64> {
        match (object.host_type, object.handle.0) {
            (SHAPES, 1) => Some(CIRCLE),
            (SHAPES, 2) => Some(SQUARE),
            _ => None,
        }
    }
}
impl Host for Shapes {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        _: &[ValueRef<'_>],
        rets: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        self.calls += 1;
        rets[0] = Value::Unit;
        Ok(0)
    }

    fn object_key(&self, object: Obj) -> Option<u64> {
        Self::class(object)
    }
}

fn shape(handle: u64) -> Value {
    Value::Obj(Obj {
        host_type: SHAPES,
        handle: ObjHandle(handle),
    })
}

/// `key(obj) -> u64` returns the object's key; `is_circle(obj) -> bool` tests it.
fn program() -> (Arc<VerifiedProgram>, FuncId, FuncId) {
    let mut b = ProgramBuilder::new();
    let key = b
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::Obj(SHAPES)],
            ret_types: vec![ValueType::U64],
        })
        .unwrap();
    let mut a = b.function_asm(key).unwrap();
    let out = a.alloc_register(ValueType::U64);
    a.obj_key(out, Reg(1)).ret(Reg::EFFECT, &[out]);
    b.define_function(key, a).unwrap();

    let is_circle = b
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::Obj(SHAPES)],
            ret_types: vec![ValueType::Bool],
        })
        .unwrap();
    let mut a = b.function_asm(is_circle).unwrap();
    let class = a.alloc_register(ValueType::U64);
    let circle = a.alloc_register(ValueType::U64);
    let hit = a.alloc_register(ValueType::Bool);
    a.obj_key(class, Reg(1))
        .const_u64(circle, CIRCLE)
        .u64_eq(hit, class, circle)
        .ret(Reg::EFFECT, &[hit]);
    b.define_function(is_circle, a).unwrap();
    (Arc::new(b.build_verified().unwrap()), key, is_circle)
}

#[test]
fn obj_key_reads_the_hosts_key_without_a_host_call() {
    let (program, key, is_circle) = program();
    // `obj.key` costs one unit of fuel like any instruction, and no host-call allowance.
    let new_vm = |fuel| {
        let mut vm = Vm::new(
            Shapes { calls: 0 },
            Limits {
                fuel,
                max_host_calls: 0,
                ..Limits::default()
            },
        );
        let unit = vm.install(Arc::clone(&program)).unwrap();
        (vm, unit)
    };
    let (mut vm, unit) = new_vm(2);
    let mut run = |func, handle| {
        vm.run(
            FuncRef::new(unit, func),
            &[shape(handle)],
            TraceMask::NONE,
            None,
        )
    };
    assert_eq!(run(key, 1).unwrap(), [Value::U64(CIRCLE)]);
    assert_eq!(run(key, 2).unwrap(), [Value::U64(SQUARE)]);
    assert!(matches!(
        run(is_circle, 1).unwrap_err().trap,
        Trap::FuelExceeded
    ));
    assert_eq!(vm.host().calls, 0);

    let (mut vm, unit) = new_vm(4);
    for (handle, expected) in [(1, true), (2, false)] {
        assert_eq!(
            vm.run(
                FuncRef::new(unit, is_circle),
                &[shape(handle)],
                TraceMask::NONE,
                None
            )
            .unwrap(),
            [Value::Bool(expected)]
        );
    }
    assert_eq!(vm.host().calls, 0);
}

#[test]
fn a_yielding_turn_reads_keys_without_stopping() {
    let (program, key, _) = program();
    let mut vm = Vm::new(Shapes { calls: 0 }, Limits::default());
    let unit = vm.install(program).unwrap();
    let args = [shape(2)];
    let mut turn = vm
        .start(
            FuncRef::new(unit, key),
            &args,
            TurnOptions::default().yield_host_calls(),
        )
        .unwrap();
    assert!(matches!(turn.drive(u64::MAX).unwrap(), RunStatus::Complete));
    let Ok(TurnOutcome::Returned(values)) = turn.finish() else {
        panic!("the turn returned");
    };
    assert_eq!(values, [Value::U64(SQUARE)]);
}

#[test]
fn an_object_without_a_key_traps_naming_it() {
    let (program, key, _) = program();
    let unit_program = Arc::clone(&program);
    let mut vm = Vm::new(Shapes { calls: 0 }, Limits::default());
    let unit = vm.install(program).unwrap();
    let trap = vm
        .run(FuncRef::new(unit, key), &[shape(3)], TraceMask::NONE, None)
        .unwrap_err();
    assert_eq!((trap.func, trap.pc), (key, Pc(0)));
    let Trap::NoObjectKey { object } = trap.trap else {
        panic!("expected NoObjectKey, got {:?}", trap.trap);
    };
    assert_eq!(Value::Obj(object), shape(3));
    assert_eq!(
        Trap::NoObjectKey { object }.to_string(),
        "the host has no key for object 0x3 of host type 0x53484150"
    );

    // A host that keys nothing, the default, traps on every object.
    struct Keyless;
    impl Host for Keyless {
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
    let mut vm = Vm::new(Keyless, Limits::default());
    let unit = vm.install(unit_program).unwrap();
    assert!(matches!(
        vm.run(FuncRef::new(unit, key), &[shape(1)], TraceMask::NONE, None)
            .unwrap_err()
            .trap,
        Trap::NoObjectKey { .. }
    ));
}

#[test]
fn the_verifier_requires_an_object_and_a_u64_destination() {
    let build = |object: ValueType, dst: ValueType| {
        let mut b = ProgramBuilder::new();
        let f = b
            .declare_function(FunctionSig {
                arg_types: vec![object],
                ret_types: vec![],
            })
            .unwrap();
        let mut a = b.function_asm(f).unwrap();
        let out = a.alloc_register(dst);
        a.obj_key(out, Reg(1)).ret(Reg::EFFECT, &[]);
        b.define_function(f, a).unwrap();
        b.build()
    };
    let verify = |program: &Program| verify_program(program, &VerifyConfig::default());

    // Any host type will do.
    assert!(verify(&build(ValueType::Obj(OTHER), ValueType::U64)).is_ok());
    let error = verify(&build(ValueType::I64, ValueType::U64)).unwrap_err();
    assert_eq!(
        error,
        VerifyError::ObjectRegisterRequired {
            func: FuncId(0),
            pc: Pc(0),
            reg: Reg(1),
            declared: ValueType::I64,
        }
    );
    assert_eq!(
        error.to_string(),
        "function #0, pc 0: r1 declared I64, required a host object"
    );
    assert!(matches!(
        verify(&build(ValueType::Obj(SHAPES), ValueType::I64)).unwrap_err(),
        VerifyError::RegisterTypeMismatch {
            reg: Reg(2),
            declared: ValueType::I64,
            required: ValueType::U64,
            ..
        }
    ));
}

#[test]
fn obj_key_is_a_prefixed_opcode_with_a_disassembly() {
    assert_eq!(Opcode::ObjKey.prefix(), Some(OpcodePrefix::Extended));
    assert_eq!(Opcode::ObjKey.code(), 0xFC0D);
    assert_eq!(Opcode::ObjKey.mnemonic(), "obj.key");
    let (program, key, _) = program();
    let disassembly = execution_tape::disasm::disassemble_function(program.program(), key).unwrap();
    let first = disassembly.instrs().next().unwrap();
    assert!(first.to_string().ends_with("obj.key r2, [r1]"), "{first}");
    assert_eq!(first.dst(), Some(Reg(2)));
    assert_eq!(first.reads().collect::<Vec<_>>(), [Reg(1)]);
}

/// Every key answer, and whether a host call scope opened.
#[derive(Default)]
struct Keys {
    answers: Vec<(TraceLocation, Obj, Option<u64>)>,
    host_scopes: usize,
}
impl TraceSink for Keys {
    fn mask(&self) -> TraceMask {
        TraceMask::VALUES | TraceMask::HOST
    }
    fn values(&mut self, _: &Program, event: ValueEvent<'_>) {
        if let ValueEvent::ObjectKey {
            location,
            object,
            key,
        } = event
        {
            self.answers.push((location, object, key));
        }
    }
    fn scope_enter(
        &mut self,
        _: &Program,
        kind: ScopeKind,
        _: usize,
        _: TraceLocation,
        _: Option<execution_tape::program::SpanId>,
    ) {
        if matches!(kind, ScopeKind::HostCall { .. }) {
            self.host_scopes += 1;
        }
    }
}

#[test]
fn observers_see_each_answer_including_none() {
    let (program, key, _) = program();
    let mut vm = Vm::new(Shapes { calls: 0 }, Limits::default());
    let unit = vm.install(program).unwrap();
    let mut keys = Keys::default();
    let entry = FuncRef::new(unit, key);
    vm.run(entry, &[shape(1)], keys.mask(), Some(&mut keys))
        .unwrap();
    vm.run(entry, &[shape(4)], keys.mask(), Some(&mut keys))
        .unwrap_err();
    let at = TraceLocation {
        unit,
        func: key,
        pc: Pc(0),
    };
    let object = |handle| Obj {
        host_type: SHAPES,
        handle: ObjHandle(handle),
    };
    assert_eq!(
        keys.answers,
        [(at, object(1), Some(CIRCLE)), (at, object(4), None)]
    );
    assert_eq!(keys.host_scopes, 0);
}

#[test]
fn recordings_keep_keys_and_replay_them_without_the_host() {
    let (program, _, is_circle) = program();
    for (handle, expected) in [(1, Some(true)), (2, Some(false)), (5, None)] {
        let mut vm = Vm::new(Shapes { calls: 0 }, Limits::default());
        let unit = vm.install(Arc::clone(&program)).unwrap();
        let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
        let live = vm.run(
            FuncRef::new(unit, is_circle),
            &[shape(handle)],
            recorder.mask(),
            Some(&mut recorder),
        );
        let recording = recorder.finish();
        recording.status().unwrap();
        assert_eq!(recording.stats().object_queries, 1);
        let [answer] = recording.object_queries() else {
            panic!("one key was read");
        };
        assert_eq!(
            answer.object,
            Obj {
                host_type: SHAPES,
                handle: ObjHandle(handle),
            }
        );
        assert_eq!(
            answer.answer,
            ObjectAnswer::Key(Shapes::class(answer.object))
        );
        match (expected, live, recording.replay().unwrap()) {
            (Some(expected), Ok(values), ReplayOutcome::Returned(snapshot)) => {
                assert_eq!(values, [Value::Bool(expected)]);
                assert_eq!(snapshot.values(), [Value::Bool(expected)]);
            }
            (None, Err(_), ReplayOutcome::Trapped(trap)) => {
                assert!(matches!(trap.trap, Trap::NoObjectKey { .. }));
            }
            (expected, live, replayed) => {
                panic!("{expected:?}: live {live:?}, replayed {replayed:?}")
            }
        }
    }
}

#[test]
fn a_recording_over_its_key_budget_is_incomplete() {
    let (program, key, _) = program();
    let mut vm = Vm::new(Shapes { calls: 0 }, Limits::default());
    let unit = vm.install(Arc::clone(&program)).unwrap();
    let limits = RecordLimits {
        object_queries: 0,
        ..RecordLimits::default()
    };
    let mut recorder = Recorder::new(program, limits);
    vm.run(
        FuncRef::new(unit, key),
        &[shape(1)],
        recorder.mask(),
        Some(&mut recorder),
    )
    .unwrap();
    let recording = recorder.finish();
    assert_eq!(
        recording.status().unwrap_err(),
        &execution_tape::recording::CaptureFault::Budget {
            resource: execution_tape::recording::CaptureResource::ObjectQueries,
            limit: 0,
        }
    );
}

#[test]
fn bound_hosts_answer_keys_from_their_state() {
    const PING: HostImport = HostImport::new("ping", &[], &[]);
    let (program, key, _) = program();
    let mut functions = HostFunctions::<u64>::new();
    functions.implement(PING, |_, _, _, _| Ok(0)).unwrap();
    let bindings = functions.bind(&program).unwrap();
    let run = |host| {
        let mut vm = Vm::new(host, Limits::default());
        let unit = vm.install(Arc::clone(&program)).unwrap();
        vm.run(FuncRef::new(unit, key), &[shape(5)], TraceMask::NONE, None)
    };
    let keyed = bindings
        .with_state(100)
        .with_object_key(|offset, object| Some(object.handle.0 + offset));
    assert_eq!(run(keyed).unwrap(), [Value::U64(105)]);
    // Without a callback no object has a key.
    assert!(matches!(
        run(bindings.with_state(100)).unwrap_err().trap,
        Trap::NoObjectKey { .. }
    ));
}

/// `outer() -> u64` is a yielded host request; `callback(obj)` reads a key.
fn program_with_requests() -> (Arc<VerifiedProgram>, FuncId, FuncId) {
    let mut b = ProgramBuilder::new();
    let object = ValueType::Obj(SHAPES);
    let outer = b.host_sig_for(
        "outer",
        execution_tape::host::HostSig {
            args: vec![],
            rets: vec![ValueType::U64],
        },
    );
    let signature = FunctionSig {
        arg_types: vec![object],
        ret_types: vec![ValueType::U64],
    };
    // entry(obj) = key(obj) + outer() + key(obj) + key(obj)
    let entry = b.declare_function(signature.clone()).unwrap();
    let mut a = b.function_asm(entry).unwrap();
    let (total, key) = (
        a.alloc_register(ValueType::U64),
        a.alloc_register(ValueType::U64),
    );
    a.obj_key(total, Reg(1))
        .host_call_sig_id(Reg::EFFECT, outer, Reg::EFFECT, &[], &[key])
        .u64_add(total, total, key);
    for _ in 0..2 {
        a.obj_key(key, Reg(1)).u64_add(total, total, key);
    }
    a.ret(Reg::EFFECT, &[total]);
    b.define_function(entry, a).unwrap();
    let callback = b.declare_function(signature).unwrap();
    let mut a = b.function_asm(callback).unwrap();
    let key = a.alloc_register(ValueType::U64);
    a.obj_key(key, Reg(1)).ret(Reg::EFFECT, &[key]);
    b.define_function(callback, a).unwrap();
    (Arc::new(b.build_verified().unwrap()), entry, callback)
}

#[test]
fn keys_read_in_guest_callbacks_and_resumed_turns_replay() {
    let (program, entry, callback) = program_with_requests();
    fn returned(turn: execution_tape::vm::Turn<'_, Shapes>) -> Vec<Value> {
        match turn.finish() {
            Ok(TurnOutcome::Returned(values)) => values,
            _ => panic!("the turn returned"),
        }
    }
    for resume in [false, true] {
        let mut vm = Vm::new(Shapes { calls: 0 }, Limits::default());
        let unit = vm.install(Arc::clone(&program)).unwrap();
        let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
        let mask = recorder.mask();
        let args = [shape(1)];
        let mut turn = vm
            .start(
                FuncRef::new(unit, entry),
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
        // The request calls back into the guest, which reads a key, and answers with it.
        let request = turn.pending_host_call().unwrap();
        request
            .call_guest(Value::Func(FuncRef::new(unit, callback)), vec![shape(2)])
            .unwrap();
        assert!(matches!(
            turn.drive(u64::MAX).unwrap(),
            RunStatus::CallbackReturned
        ));
        let mut request = turn.pending_host_call().unwrap();
        let mut values = request.take_callback_results().unwrap();
        request.complete(&mut values, Ok(0)).unwrap();
        let live = if resume {
            // Save between the remaining key reads, then resume in a new turn.
            assert!(matches!(turn.drive(3).unwrap(), RunStatus::Suspended));
            let Ok(TurnOutcome::Saved(saved)) = turn.finish() else {
                panic!("the turn saved its computation");
            };
            let mut turn = vm
                .resume(
                    saved,
                    TurnOptions::default()
                        .yield_host_calls()
                        .trace(mask, &mut recorder),
                )
                .unwrap();
            assert!(matches!(turn.drive(u64::MAX).unwrap(), RunStatus::Complete));
            returned(turn)
        } else {
            assert!(matches!(turn.drive(u64::MAX).unwrap(), RunStatus::Complete));
            returned(turn)
        };
        assert_eq!(live, [Value::U64(CIRCLE + SQUARE + 2 * CIRCLE)]);
        let recording = recorder.finish();
        recording.status().unwrap();
        let read: Vec<_> = recording
            .object_queries()
            .iter()
            .map(|query| (query.location.func, query.object.handle.0))
            .collect();
        assert_eq!(
            read,
            [(entry, 1), (callback, 2), (entry, 1), (entry, 1)],
            "resumed: {resume}"
        );
        let ReplayOutcome::Returned(snapshot) = recording.replay().unwrap() else {
            panic!("the replay returned");
        };
        assert_eq!(snapshot.values(), live);
    }
}

#[test]
fn replay_delivers_several_keys_in_order() {
    // `sum(a, b, c) -> u64` adds the keys of three objects.
    let mut b = ProgramBuilder::new();
    let object = ValueType::Obj(SHAPES);
    let sum = b
        .declare_function(FunctionSig {
            arg_types: vec![object, object, object],
            ret_types: vec![ValueType::U64],
        })
        .unwrap();
    let mut a = b.function_asm(sum).unwrap();
    let (total, key) = (
        a.alloc_register(ValueType::U64),
        a.alloc_register(ValueType::U64),
    );
    a.obj_key(total, Reg(1));
    for arg in [Reg(2), Reg(3)] {
        a.obj_key(key, arg).u64_add(total, total, key);
    }
    a.ret(Reg::EFFECT, &[total]);
    b.define_function(sum, a).unwrap();
    let program = Arc::new(b.build_verified().unwrap());

    let mut vm = Vm::new(Shapes { calls: 0 }, Limits::default());
    let unit = vm.install(Arc::clone(&program)).unwrap();
    let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
    let args = [shape(2), shape(1), shape(2)];
    let live = vm
        .run(
            FuncRef::new(unit, sum),
            &args,
            recorder.mask(),
            Some(&mut recorder),
        )
        .unwrap();
    assert_eq!(live, [Value::U64(SQUARE + CIRCLE + SQUARE)]);
    let recording = recorder.finish();
    let read: Vec<_> = recording
        .object_queries()
        .iter()
        .map(|query| (query.location.pc, query.object.handle.0, query.answer))
        .collect();
    assert_eq!(
        read,
        [
            (Pc(0), 2, ObjectAnswer::Key(Some(SQUARE))),
            (Pc(4), 1, ObjectAnswer::Key(Some(CIRCLE))),
            (Pc(12), 2, ObjectAnswer::Key(Some(SQUARE))),
        ]
    );
    let ReplayOutcome::Returned(snapshot) = recording.replay().unwrap() else {
        panic!("the replay returned");
    };
    assert_eq!(snapshot.values(), live);
}
