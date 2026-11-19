// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `obj.bytes_len` and `obj.bytes_get` read a host object's bytes, such as a string's UTF-8,
//! straight from the host: no host call, request or allowance, one unit of fuel each, and a
//! trap when the host has no bytes for the object or the index is out of bounds.

use execution_tape::{
    asm::{FunctionSig, ProgramBuilder, Reg},
    host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef},
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

const TEXTS: HostTypeId = HostTypeId(0x5445_5854);
const OTHER: HostTypeId = HostTypeId(0x4f54_4852);

/// Objects whose handle indexes a byte buffer; handles past the end have no bytes.
///
/// The host import `reuse(obj) -> obj` takes the object back, frees it and returns a new
/// object, `b"reused"`, under the same handle, and `compact()` moves every buffer, as a moving
/// collector would.
struct Texts {
    texts: Vec<Vec<u8>>,
    calls: usize,
}
impl Texts {
    fn new() -> Self {
        Self {
            texts: vec![b"hello".to_vec(), b"".to_vec(), b"hello".to_vec()],
            calls: 0,
        }
    }
}
impl Texts {
    fn bytes(&self, object: Obj) -> Option<&[u8]> {
        if object.host_type != TEXTS {
            return None;
        }
        self.texts
            .get(usize::try_from(object.handle.0).ok()?)
            .map(Vec::as_slice)
    }
}
impl Host for Texts {
    fn call(
        &mut self,
        symbol: &str,
        _: SigHash,
        args: &[ValueRef<'_>],
        rets: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        self.calls += 1;
        match (symbol, args) {
            ("reuse", [ValueRef::Obj(object)]) => {
                let index = usize::try_from(object.handle.0).map_err(|_| HostError::Failed)?;
                self.texts[index] = b"reused".to_vec();
                rets[0] = Value::Obj(*object);
            }
            ("compact", []) => {
                // Fresh allocations, so every buffer moves.
                self.texts = self.texts.to_vec();
            }
            _ => return Err(HostError::UnknownSymbol),
        }
        Ok(0)
    }

    fn object_bytes<R>(&self, object: Obj, read: impl FnOnce(Option<&[u8]>) -> R) -> R {
        read(self.bytes(object))
    }
}

fn text(handle: u64) -> Value {
    Value::Obj(Obj {
        host_type: TEXTS,
        handle: ObjHandle(handle),
    })
}

struct Functions {
    program: Arc<VerifiedProgram>,
    /// `len(obj) -> u64`.
    len: FuncId,
    /// `get(obj, index) -> u64`.
    get: FuncId,
    /// `sum(obj) -> u64`: the sum of the object's bytes, read in a loop.
    sum: FuncId,
    /// `around(obj, other) -> u64`: `obj`'s first two bytes, plus `256 *` the first byte of
    /// the object the host returns when `obj` is given back to `reuse` and the buffers are
    /// compacted, plus `other`'s first byte.
    around: FuncId,
}

fn program() -> Functions {
    let text = ValueType::Obj(TEXTS);
    let mut b = ProgramBuilder::new();
    let reuse = b.host_sig_for(
        "reuse",
        HostSig {
            args: vec![text],
            rets: vec![text],
        },
    );
    let compact = b.host_sig_for(
        "compact",
        HostSig {
            args: vec![],
            rets: vec![],
        },
    );

    let len = b
        .declare_function(FunctionSig {
            arg_types: vec![text],
            ret_types: vec![ValueType::U64],
        })
        .unwrap();
    let mut a = b.function_asm(len).unwrap();
    let out = a.alloc_register(ValueType::U64);
    a.obj_bytes_len(out, Reg(1)).ret(Reg::EFFECT, &[out]);
    b.define_function(len, a).unwrap();

    let get = b
        .declare_function(FunctionSig {
            arg_types: vec![text, ValueType::U64],
            ret_types: vec![ValueType::U64],
        })
        .unwrap();
    let mut a = b.function_asm(get).unwrap();
    let out = a.alloc_register(ValueType::U64);
    a.obj_bytes_get(out, Reg(1), Reg(2))
        .ret(Reg::EFFECT, &[out]);
    b.define_function(get, a).unwrap();

    let sum = b
        .declare_function(FunctionSig {
            arg_types: vec![text],
            ret_types: vec![ValueType::U64],
        })
        .unwrap();
    let mut a = b.function_asm(sum).unwrap();
    let (end, index, one, more, byte, total) = (
        a.alloc_register(ValueType::U64),
        a.alloc_register(ValueType::U64),
        a.alloc_register(ValueType::U64),
        a.alloc_register(ValueType::Bool),
        a.alloc_register(ValueType::U64),
        a.alloc_register(ValueType::U64),
    );
    let (head, body, exit) = (a.label(), a.label(), a.label());
    a.obj_bytes_len(end, Reg(1))
        .const_u64(index, 0)
        .const_u64(one, 1)
        .const_u64(total, 0)
        .jmp(head);
    a.place(head).unwrap();
    a.u64_lt(more, index, end).br(more, body, exit);
    a.place(body).unwrap();
    a.obj_bytes_get(byte, Reg(1), index)
        .u64_add(total, total, byte)
        .u64_add(index, index, one)
        .jmp(head);
    a.place(exit).unwrap();
    a.ret(Reg::EFFECT, &[total]);
    b.define_function(sum, a).unwrap();

    let around = b
        .declare_function(FunctionSig {
            arg_types: vec![text, text],
            ret_types: vec![ValueType::U64],
        })
        .unwrap();
    let mut a = b.function_asm(around).unwrap();
    let (zero, one, before, second, after, other, shift) = (
        a.alloc_register(ValueType::U64),
        a.alloc_register(ValueType::U64),
        a.alloc_register(ValueType::U64),
        a.alloc_register(ValueType::U64),
        a.alloc_register(ValueType::U64),
        a.alloc_register(ValueType::U64),
        a.alloc_register(ValueType::U64),
    );
    let fresh = a.alloc_register(text);
    a.const_u64(zero, 0)
        .const_u64(one, 1)
        .const_u64(shift, 8)
        .obj_bytes_get(before, Reg(1), zero)
        .obj_bytes_get(second, Reg(1), one)
        .u64_add(before, before, second)
        .host_call_sig_id(Reg::EFFECT, reuse, Reg::EFFECT, &[Reg(1)], &[fresh])
        .host_call_sig_id(Reg::EFFECT, compact, Reg::EFFECT, &[], &[])
        .obj_bytes_get(after, fresh, zero)
        .obj_bytes_get(other, Reg(2), zero)
        .u64_shl(after, after, shift)
        .u64_add(before, before, after)
        .u64_add(before, before, other)
        .ret(Reg::EFFECT, &[before]);
    b.define_function(around, a).unwrap();

    Functions {
        program: Arc::new(b.build_verified().unwrap()),
        len,
        get,
        sum,
        around,
    }
}

fn hello_sum() -> u64 {
    b"hello".iter().map(|&byte| u64::from(byte)).sum()
}

#[test]
fn guests_read_host_bytes_without_a_host_call() {
    let f = program();
    let mut vm = Vm::new(
        Texts::new(),
        Limits {
            max_host_calls: 0,
            ..Limits::default()
        },
    );
    let unit = vm.install(Arc::clone(&f.program)).unwrap();
    let mut run =
        |func, args: &[Value]| vm.run(FuncRef::new(unit, func), args, TraceMask::NONE, None);
    assert_eq!(run(f.len, &[text(0)]).unwrap(), [Value::U64(5)]);
    assert_eq!(run(f.len, &[text(1)]).unwrap(), [Value::U64(0)]);
    assert_eq!(
        run(f.get, &[text(0), Value::U64(1)]).unwrap(),
        [Value::U64(u64::from(b'e'))]
    );
    assert_eq!(run(f.sum, &[text(0)]).unwrap(), [Value::U64(hello_sum())]);
    assert_eq!(run(f.sum, &[text(1)]).unwrap(), [Value::U64(0)]);
    assert_eq!(vm.host().calls, 0);
}

#[test]
fn each_read_costs_one_unit_of_fuel() {
    let f = program();
    for (fuel, ok) in [(1, false), (2, true)] {
        let mut vm = Vm::new(
            Texts::new(),
            Limits {
                fuel,
                ..Limits::default()
            },
        );
        let unit = vm.install(Arc::clone(&f.program)).unwrap();
        let result = vm.run(FuncRef::new(unit, f.len), &[text(0)], TraceMask::NONE, None);
        match result {
            Ok(values) => {
                assert!(ok);
                assert_eq!(values, [Value::U64(5)]);
            }
            Err(trap) => {
                assert!(!ok);
                assert!(matches!(trap.trap, Trap::FuelExceeded));
            }
        }
    }
}

#[test]
fn a_yielding_turn_reads_bytes_without_stopping() {
    let f = program();
    let mut vm = Vm::new(Texts::new(), Limits::default());
    let unit = vm.install(f.program).unwrap();
    let args = [text(0)];
    let mut turn = vm
        .start(
            FuncRef::new(unit, f.sum),
            &args,
            TurnOptions::default().yield_host_calls(),
        )
        .unwrap();
    assert!(matches!(turn.drive(u64::MAX).unwrap(), RunStatus::Complete));
    let Ok(TurnOutcome::Returned(values)) = turn.finish() else {
        panic!("the turn returned");
    };
    assert_eq!(values, [Value::U64(hello_sum())]);
}

#[test]
fn an_index_past_the_end_traps() {
    let f = program();
    let mut vm = Vm::new(Texts::new(), Limits::default());
    let unit = vm.install(f.program).unwrap();
    for (handle, index) in [(0, 5), (0, u64::MAX), (1, 0)] {
        let trap = vm
            .run(
                FuncRef::new(unit, f.get),
                &[text(handle), Value::U64(index)],
                TraceMask::NONE,
                None,
            )
            .unwrap_err();
        assert!(matches!(trap.trap, Trap::IndexOutOfBounds), "{trap:?}");
        assert_eq!((trap.func, trap.pc), (f.get, Pc(0)));
    }
}

#[test]
fn an_object_without_bytes_traps_naming_it() {
    let f = program();
    let mut vm = Vm::new(Texts::new(), Limits::default());
    let unit = vm.install(Arc::clone(&f.program)).unwrap();
    for (func, args) in [
        (f.len, vec![text(3)]),
        (f.get, vec![text(3), Value::U64(0)]),
    ] {
        let trap = vm
            .run(FuncRef::new(unit, func), &args, TraceMask::NONE, None)
            .unwrap_err();
        let Trap::NoObjectBytes { object } = trap.trap else {
            panic!("expected NoObjectBytes, got {:?}", trap.trap);
        };
        assert_eq!(Value::Obj(object), text(3));
        assert_eq!(
            Trap::NoObjectBytes { object }.to_string(),
            "the host has no bytes for object 0x3 of host type 0x54455854"
        );
    }

    // A host that exposes no bytes, the default, traps on every object.
    struct Opaque;
    impl Host for Opaque {
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
    let mut vm = Vm::new(Opaque, Limits::default());
    let unit = vm.install(f.program).unwrap();
    assert!(matches!(
        vm.run(FuncRef::new(unit, f.len), &[text(0)], TraceMask::NONE, None)
            .unwrap_err()
            .trap,
        Trap::NoObjectBytes { .. }
    ));
}

#[test]
fn offsets_survive_a_host_moving_its_buffers() {
    // `around` reads, lets the host reuse the handle and move the buffers, and reads again:
    // the guest holds only objects and offsets, so it sees the new bytes at their new address.
    let f = program();
    let mut vm = Vm::new(Texts::new(), Limits::default());
    let unit = vm.install(f.program).unwrap();
    assert_eq!(
        vm.run(
            FuncRef::new(unit, f.around),
            &[text(0), text(2)],
            TraceMask::NONE,
            None
        )
        .unwrap(),
        [Value::U64(
            u64::from(b'h') + u64::from(b'e') + (u64::from(b'r') << 8) + u64::from(b'h')
        )]
    );
    assert_eq!(vm.host().calls, 2);
}

#[test]
fn the_verifier_requires_an_object_and_u64_index_and_destination() {
    let build = |object: ValueType, index: ValueType, dst: ValueType| {
        let mut b = ProgramBuilder::new();
        let f = b
            .declare_function(FunctionSig {
                arg_types: vec![object, index],
                ret_types: vec![],
            })
            .unwrap();
        let mut a = b.function_asm(f).unwrap();
        let out = a.alloc_register(dst);
        a.obj_bytes_get(out, Reg(1), Reg(2)).ret(Reg::EFFECT, &[]);
        b.define_function(f, a).unwrap();
        b.build()
    };
    let verify = |program: &Program| verify_program(program, &VerifyConfig::default());

    // Any host type will do.
    assert!(
        verify(&build(
            ValueType::Obj(OTHER),
            ValueType::U64,
            ValueType::U64
        ))
        .is_ok()
    );

    // `obj.bytes_len` checks its object and destination the same way.
    let build_len = |object: ValueType, dst: ValueType| {
        let mut b = ProgramBuilder::new();
        let f = b
            .declare_function(FunctionSig {
                arg_types: vec![object],
                ret_types: vec![],
            })
            .unwrap();
        let mut a = b.function_asm(f).unwrap();
        let out = a.alloc_register(dst);
        a.obj_bytes_len(out, Reg(1)).ret(Reg::EFFECT, &[]);
        b.define_function(f, a).unwrap();
        b.build()
    };
    assert!(verify(&build_len(ValueType::Obj(OTHER), ValueType::U64)).is_ok());
    assert!(matches!(
        verify(&build_len(ValueType::Str, ValueType::U64)).unwrap_err(),
        VerifyError::ObjectRegisterRequired { reg: Reg(1), .. }
    ));
    assert!(matches!(
        verify(&build_len(ValueType::Obj(TEXTS), ValueType::I64)).unwrap_err(),
        VerifyError::RegisterTypeMismatch {
            reg: Reg(2),
            declared: ValueType::I64,
            required: ValueType::U64,
            ..
        }
    ));
    assert_eq!(
        verify(&build(ValueType::Bytes, ValueType::U64, ValueType::U64)).unwrap_err(),
        VerifyError::ObjectRegisterRequired {
            func: FuncId(0),
            pc: Pc(0),
            reg: Reg(1),
            declared: ValueType::Bytes,
        }
    );
    assert!(matches!(
        verify(&build(
            ValueType::Obj(TEXTS),
            ValueType::I64,
            ValueType::U64
        ))
        .unwrap_err(),
        VerifyError::RegisterTypeMismatch {
            reg: Reg(2),
            declared: ValueType::I64,
            required: ValueType::U64,
            ..
        }
    ));
    assert!(matches!(
        verify(&build(
            ValueType::Obj(TEXTS),
            ValueType::U64,
            ValueType::U32
        ))
        .unwrap_err(),
        VerifyError::RegisterTypeMismatch {
            reg: Reg(3),
            declared: ValueType::U32,
            required: ValueType::U64,
            ..
        }
    ));
}

#[test]
fn byte_reads_are_prefixed_opcodes_with_a_disassembly() {
    for (opcode, code, mnemonic) in [
        (Opcode::ObjBytesLen, 0xFC20, "obj.bytes_len"),
        (Opcode::ObjBytesGet, 0xFC21, "obj.bytes_get"),
    ] {
        assert_eq!(opcode.prefix(), Some(OpcodePrefix::Extended));
        assert_eq!(opcode.code(), code);
        assert_eq!(opcode.mnemonic(), mnemonic);
    }
    let f = program();
    let disassemble = |func| {
        execution_tape::disasm::disassemble_function(f.program.program(), func)
            .unwrap()
            .instrs()
            .next()
            .unwrap()
            .to_string()
    };
    let len = disassemble(f.len);
    assert!(len.ends_with("obj.bytes_len r2, [r1]"), "{len}");
    let get = disassemble(f.get);
    assert!(get.ends_with("obj.bytes_get r3, [r1, r2]"), "{get}");
}

/// Every bytes answer, and whether a host call scope opened.
#[derive(Default)]
struct Answers {
    answers: Vec<(TraceLocation, Obj, Option<Vec<u8>>)>,
    host_scopes: usize,
}
impl TraceSink for Answers {
    fn mask(&self) -> TraceMask {
        TraceMask::VALUES | TraceMask::HOST
    }
    fn values(&mut self, _: &Program, event: ValueEvent<'_>) {
        if let ValueEvent::ObjectBytes {
            location,
            object,
            bytes,
        } = event
        {
            self.answers
                .push((location, object, bytes.map(<[u8]>::to_vec)));
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
    let f = program();
    let mut vm = Vm::new(Texts::new(), Limits::default());
    let unit = vm.install(f.program).unwrap();
    let mut answers = Answers::default();
    let entry = FuncRef::new(unit, f.len);
    vm.run(entry, &[text(0)], answers.mask(), Some(&mut answers))
        .unwrap();
    vm.run(entry, &[text(4)], answers.mask(), Some(&mut answers))
        .unwrap_err();
    let at = TraceLocation {
        unit,
        func: f.len,
        pc: Pc(0),
    };
    let object = |handle| Obj {
        host_type: TEXTS,
        handle: ObjHandle(handle),
    };
    assert_eq!(
        answers.answers,
        [
            (at, object(0), Some(b"hello".to_vec())),
            (at, object(4), None)
        ]
    );
    assert_eq!(answers.host_scopes, 0);
}

#[test]
fn recordings_copy_bytes_once_and_replay_them_without_the_host() {
    let f = program();
    let mut vm = Vm::new(Texts::new(), Limits::default());
    let unit = vm.install(Arc::clone(&f.program)).unwrap();
    let mut recorder = Recorder::new(Arc::clone(&f.program), RecordLimits::default());
    let live = vm
        .run(
            FuncRef::new(unit, f.sum),
            &[text(0)],
            recorder.mask(),
            Some(&mut recorder),
        )
        .unwrap();
    let recording = recorder.finish();
    recording.status().unwrap();
    // The length is recorded; the five byte reads after it, with no host code between, are
    // answered by the same copy.
    assert_eq!(recording.stats().object_queries, 1);
    assert_eq!(recording.stats().payload_bytes, 5);
    assert_eq!(
        recording
            .object_bytes()
            .iter()
            .map(|bytes| &**bytes)
            .collect::<Vec<_>>(),
        [b"hello"]
    );
    assert!(
        recording
            .object_queries()
            .iter()
            .all(|query| query.answer == ObjectAnswer::Bytes(Some(0)))
    );
    let ReplayOutcome::Returned(snapshot) = recording.replay().unwrap() else {
        panic!("the replay returned");
    };
    assert_eq!(snapshot.values(), live);
}

#[test]
fn recordings_read_bytes_again_after_host_code_runs() {
    // `around` reads object 0 twice, but only the first read is recorded. Then the host
    // reuses handle 0 for another object, so its next read is recorded again with the new
    // bytes, and the read of the other object, whose bytes equal the first's, shares its copy.
    let f = program();
    let mut vm = Vm::new(Texts::new(), Limits::default());
    let unit = vm.install(Arc::clone(&f.program)).unwrap();
    let mut recorder = Recorder::new(Arc::clone(&f.program), RecordLimits::default());
    let live = vm
        .run(
            FuncRef::new(unit, f.around),
            &[text(0), text(2)],
            recorder.mask(),
            Some(&mut recorder),
        )
        .unwrap();
    let recording = recorder.finish();
    recording.status().unwrap();
    assert_eq!(
        recording
            .object_bytes()
            .iter()
            .map(|bytes| &**bytes)
            .collect::<Vec<&[u8]>>(),
        [&b"hello"[..], b"reused"]
    );
    let answers: Vec<_> = recording
        .object_queries()
        .iter()
        .map(|query| (query.object.handle.0, query.answer))
        .collect();
    assert_eq!(
        answers,
        [
            (0, ObjectAnswer::Bytes(Some(0))),
            (0, ObjectAnswer::Bytes(Some(1))),
            (2, ObjectAnswer::Bytes(Some(0))),
        ]
    );
    let ReplayOutcome::Returned(snapshot) = recording.replay().unwrap() else {
        panic!("the replay returned");
    };
    assert_eq!(snapshot.values(), live);
}

#[test]
fn recordings_replay_a_missing_answer_as_the_same_trap() {
    let f = program();
    let mut vm = Vm::new(Texts::new(), Limits::default());
    let unit = vm.install(Arc::clone(&f.program)).unwrap();
    let mut recorder = Recorder::new(Arc::clone(&f.program), RecordLimits::default());
    vm.run(
        FuncRef::new(unit, f.len),
        &[text(7)],
        recorder.mask(),
        Some(&mut recorder),
    )
    .unwrap_err();
    let recording = recorder.finish();
    recording.status().unwrap();
    let [query] = recording.object_queries() else {
        panic!("one read");
    };
    assert_eq!(query.answer, ObjectAnswer::Bytes(None));
    let ReplayOutcome::Trapped(trap) = recording.replay().unwrap() else {
        panic!("the replay trapped");
    };
    assert!(matches!(trap.trap, Trap::NoObjectBytes { .. }));
}

#[test]
fn a_recording_over_its_payload_budget_is_incomplete() {
    let f = program();
    let mut vm = Vm::new(Texts::new(), Limits::default());
    let unit = vm.install(Arc::clone(&f.program)).unwrap();
    let limits = RecordLimits {
        payload_bytes: 4,
        ..RecordLimits::default()
    };
    let mut recorder = Recorder::new(Arc::clone(&f.program), limits);
    vm.run(
        FuncRef::new(unit, f.len),
        &[text(0)],
        recorder.mask(),
        Some(&mut recorder),
    )
    .unwrap();
    assert_eq!(
        recorder.finish().status().unwrap_err(),
        &execution_tape::recording::CaptureFault::Budget {
            resource: execution_tape::recording::CaptureResource::PayloadBytes,
            limit: 4,
        }
    );
}

#[test]
fn bound_hosts_answer_bytes_from_their_state() {
    const REUSE: HostImport =
        HostImport::new("reuse", &[ValueType::Obj(TEXTS)], &[ValueType::Obj(TEXTS)]);
    const COMPACT: HostImport = HostImport::new("compact", &[], &[]);
    let f = program();
    let mut functions = HostFunctions::<Vec<u8>>::new();
    functions.implement(REUSE, |_, _, _, _| Ok(0)).unwrap();
    functions.implement(COMPACT, |_, _, _, _| Ok(0)).unwrap();
    let bindings = functions.bind(&f.program).unwrap();
    let run = |host| {
        let mut vm = Vm::new(host, Limits::default());
        let unit = vm.install(Arc::clone(&f.program)).unwrap();
        vm.run(FuncRef::new(unit, f.len), &[text(9)], TraceMask::NONE, None)
    };
    let exposed = bindings
        .with_state(b"bound".to_vec())
        .with_object_bytes(|bytes, _| Some(bytes));
    assert_eq!(run(exposed).unwrap(), [Value::U64(5)]);
    // Without a callback no object has bytes.
    assert!(matches!(
        run(bindings.with_state(b"bound".to_vec()))
            .unwrap_err()
            .trap,
        Trap::NoObjectBytes { .. }
    ));
}

/// `outer() -> u64` is a yielded host request; `callback(obj)` reads a length.
fn program_with_requests() -> (Arc<VerifiedProgram>, FuncId, FuncId) {
    let mut b = ProgramBuilder::new();
    let text = ValueType::Obj(TEXTS);
    let outer = b.host_sig_for(
        "outer",
        HostSig {
            args: vec![],
            rets: vec![ValueType::U64],
        },
    );
    let signature = FunctionSig {
        arg_types: vec![text],
        ret_types: vec![ValueType::U64],
    };
    // entry(obj) = len(obj) + outer() + len(obj) + len(obj)
    let entry = b.declare_function(signature.clone()).unwrap();
    let mut a = b.function_asm(entry).unwrap();
    let (total, len) = (
        a.alloc_register(ValueType::U64),
        a.alloc_register(ValueType::U64),
    );
    a.obj_bytes_len(total, Reg(1))
        .host_call_sig_id(Reg::EFFECT, outer, Reg::EFFECT, &[], &[len])
        .u64_add(total, total, len);
    for _ in 0..2 {
        a.obj_bytes_len(len, Reg(1)).u64_add(total, total, len);
    }
    a.ret(Reg::EFFECT, &[total]);
    b.define_function(entry, a).unwrap();
    let callback = b.declare_function(signature).unwrap();
    let mut a = b.function_asm(callback).unwrap();
    let len = a.alloc_register(ValueType::U64);
    a.obj_bytes_len(len, Reg(1)).ret(Reg::EFFECT, &[len]);
    b.define_function(callback, a).unwrap();
    (Arc::new(b.build_verified().unwrap()), entry, callback)
}

#[test]
fn reads_record_again_after_callbacks_requests_and_resumption() {
    // Host code can run at a yielded request, a guest callback's entry and return, and between
    // turns, so the first read after each is recorded; replay applies the same rule.
    let (program, entry, callback) = program_with_requests();
    fn returned(turn: execution_tape::vm::Turn<'_, Texts>) -> Vec<Value> {
        match turn.finish() {
            Ok(TurnOutcome::Returned(values)) => values,
            _ => panic!("the turn returned"),
        }
    }
    for resume in [false, true] {
        let mut vm = Vm::new(Texts::new(), Limits::default());
        let unit = vm.install(Arc::clone(&program)).unwrap();
        let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
        let mask = recorder.mask();
        let args = [text(0)];
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
        // The request calls back into the guest, which reads the entry's text again, and
        // answers with its length.
        let request = turn.pending_host_call().unwrap();
        request
            .call_guest(Value::Func(FuncRef::new(unit, callback)), vec![text(0)])
            .unwrap();
        assert!(matches!(
            turn.drive(u64::MAX).unwrap(),
            RunStatus::CallbackReturned
        ));
        let mut request = turn.pending_host_call().unwrap();
        let mut values = request.take_callback_results().unwrap();
        request.complete(&mut values, Ok(0)).unwrap();
        let live = if resume {
            // Save between the remaining reads, then resume in a new turn.
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
        // Four reads of `hello`, one in the callback.
        assert_eq!(live, [Value::U64(4 * 5)]);
        let recording = recorder.finish();
        recording.status().unwrap();
        let read: Vec<_> = recording
            .object_queries()
            .iter()
            .map(|query| (query.location.func, query.object.handle.0))
            .collect();
        // Each read after a request, callback entry or return, or resumption is recorded.
        let mut expected = vec![(entry, 0), (callback, 0), (entry, 0)];
        if resume {
            expected.push((entry, 0));
        }
        assert_eq!(read, expected, "resumed: {resume}");
        let ReplayOutcome::Returned(snapshot) = recording.replay().unwrap() else {
            panic!("the replay returned");
        };
        assert_eq!(snapshot.values(), live);
    }
}
