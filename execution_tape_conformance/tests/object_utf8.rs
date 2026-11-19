// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `obj.utf8_next` and `obj.utf8_prev` decode one scalar of a host object's bytes at a byte
//! position, without a host call, and report a position that starts or ends no well-formed
//! scalar through a flag the guest branches on, instead of trapping.

use execution_tape::{
    asm::{FunctionSig, ProgramBuilder, Reg},
    host::{Host, HostContext, HostError, SigHash, ValueRef},
    opcode::{Opcode, OpcodePrefix},
    program::{HostTypeId, Program, ValueType},
    recording::{ObjectAnswer, RecordLimits, Recorder, ReplayOutcome},
    trace::{TraceMask, TraceSink},
    value::{FuncId, FuncRef, Obj, ObjHandle, Value},
    verifier::{VerifiedProgram, VerifyConfig, VerifyError, verify_program},
    vm::{Limits, Trap, Vm},
};
use std::sync::Arc;

const TEXTS: HostTypeId = HostTypeId(0x5445_5854);

/// Objects whose handle indexes a byte buffer, which need not be UTF-8.
struct Texts {
    texts: Vec<Vec<u8>>,
}
impl Host for Texts {
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

    fn object_bytes<R>(&self, object: Obj, read: impl FnOnce(Option<&[u8]>) -> R) -> R {
        read(
            usize::try_from(object.handle.0)
                .ok()
                .and_then(|index| self.texts.get(index))
                .map(Vec::as_slice),
        )
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
    /// `next(obj, pos) -> (u32, u64, bool)`.
    next: FuncId,
    /// `prev(obj, pos) -> (u32, u64, bool)`.
    prev: FuncId,
    /// `count(obj) -> (u64, u64)`: the number of scalars from the start and the position where
    /// decoding stopped, scanning forward until a position is missing.
    count: FuncId,
    /// `count_back(obj, end) -> (u64, u64)`: the same scanning backward from `end`.
    count_back: FuncId,
}

fn program() -> Functions {
    let text = ValueType::Obj(TEXTS);
    let mut b = ProgramBuilder::new();
    let step = |b: &mut ProgramBuilder, forward: bool| {
        let f = b
            .declare_function(FunctionSig {
                arg_types: vec![text, ValueType::U64],
                ret_types: vec![ValueType::U32, ValueType::U64, ValueType::Bool],
            })
            .unwrap();
        let mut a = b.function_asm(f).unwrap();
        let (scalar, pos, missing) = (
            a.alloc_register(ValueType::U32),
            a.alloc_register(ValueType::U64),
            a.alloc_register(ValueType::Bool),
        );
        if forward {
            a.obj_utf8_next(scalar, pos, missing, Reg(1), Reg(2));
        } else {
            a.obj_utf8_prev(scalar, pos, missing, Reg(1), Reg(2));
        }
        a.ret(Reg::EFFECT, &[scalar, pos, missing]);
        b.define_function(f, a).unwrap();
        f
    };
    let next = step(&mut b, true);
    let prev = step(&mut b, false);

    let scan = |b: &mut ProgramBuilder, forward: bool| {
        let f = b
            .declare_function(FunctionSig {
                arg_types: if forward {
                    vec![text]
                } else {
                    vec![text, ValueType::U64]
                },
                ret_types: vec![ValueType::U64, ValueType::U64],
            })
            .unwrap();
        let mut a = b.function_asm(f).unwrap();
        let (pos, count, one, scalar, missing) = (
            a.alloc_register(ValueType::U64),
            a.alloc_register(ValueType::U64),
            a.alloc_register(ValueType::U64),
            a.alloc_register(ValueType::U32),
            a.alloc_register(ValueType::Bool),
        );
        let (head, body, exit) = (a.label(), a.label(), a.label());
        if forward {
            a.const_u64(pos, 0);
        } else {
            a.mov(pos, Reg(2));
        }
        a.const_u64(count, 0).const_u64(one, 1).jmp(head);
        a.place(head).unwrap();
        if forward {
            a.obj_utf8_next(scalar, pos, missing, Reg(1), pos);
        } else {
            a.obj_utf8_prev(scalar, pos, missing, Reg(1), pos);
        }
        a.br(missing, exit, body);
        a.place(body).unwrap();
        a.u64_add(count, count, one).jmp(head);
        a.place(exit).unwrap();
        a.ret(Reg::EFFECT, &[count, pos]);
        b.define_function(f, a).unwrap();
        f
    };
    let count = scan(&mut b, true);
    let count_back = scan(&mut b, false);
    Functions {
        program: Arc::new(b.build_verified().unwrap()),
        next,
        prev,
        count,
        count_back,
    }
}

/// `a`, `é` (2 bytes), `€` (3 bytes) and `😀` (4 bytes).
const MIXED: &str = "a\u{e9}\u{20ac}\u{1f600}";

fn vm(texts: Vec<Vec<u8>>) -> (Vm<Texts>, Functions, execution_tape::value::UnitId) {
    let f = program();
    let mut vm = Vm::new(
        Texts { texts },
        Limits {
            max_host_calls: 0,
            ..Limits::default()
        },
    );
    let unit = vm.install(Arc::clone(&f.program)).unwrap();
    (vm, f, unit)
}

fn step(
    vm: &mut Vm<Texts>,
    unit: execution_tape::value::UnitId,
    func: FuncId,
    handle: u64,
    pos: u64,
) -> (u32, u64, bool) {
    match vm
        .run(
            FuncRef::new(unit, func),
            &[text(handle), Value::U64(pos)],
            TraceMask::NONE,
            None,
        )
        .unwrap()[..]
    {
        [Value::U32(scalar), Value::U64(pos), Value::Bool(missing)] => (scalar, pos, missing),
        ref values => panic!("unexpected results {values:?}"),
    }
}

#[test]
fn next_and_prev_decode_each_width() {
    let (mut vm, f, unit) = vm(vec![MIXED.as_bytes().to_vec()]);
    let boundaries: Vec<(usize, char)> = MIXED.char_indices().collect();
    for &(at, c) in &boundaries {
        let after = (at + c.len_utf8()) as u64;
        assert_eq!(
            step(&mut vm, unit, f.next, 0, at as u64),
            (u32::from(c), after, false)
        );
        assert_eq!(
            step(&mut vm, unit, f.prev, 0, after),
            (u32::from(c), at as u64, false)
        );
    }
}

#[test]
fn positions_that_start_or_end_no_scalar_are_missing() {
    let (mut vm, f, unit) = vm(vec![MIXED.as_bytes().to_vec()]);
    let len = MIXED.len() as u64;
    // Inside a scalar, at the ends, and past them.
    for pos in [2, 4, 5, 7, 8, 9, len, len + 1, u64::MAX] {
        assert_eq!(
            step(&mut vm, unit, f.next, 0, pos),
            (u32::MAX, pos, true),
            "next at {pos}"
        );
    }
    for pos in [0, 2, 4, 5, 7, 8, 9, len + 1, u64::MAX] {
        assert_eq!(
            step(&mut vm, unit, f.prev, 0, pos),
            (u32::MAX, pos, true),
            "prev at {pos}"
        );
    }
}

#[test]
fn malformed_bytes_are_missing() {
    let malformed: [&[u8]; 8] = [
        b"\x80",             // a lone continuation byte
        b"\xc0\x80",         // an overlong NUL
        b"\xe0\x80\x80",     // an overlong encoding
        b"\xed\xa0\x80",     // a surrogate
        b"\xf4\x90\x80\x80", // past U+10FFFF
        b"\xf5\x80\x80\x80", // an invalid lead byte
        b"\xe2\x82",         // a truncated scalar
        b"\xe2\x41\xac",     // a lead byte followed by ASCII
    ];
    let (mut vm, f, unit) = vm(malformed.iter().map(|bytes| bytes.to_vec()).collect());
    for (handle, bytes) in malformed.iter().enumerate() {
        let handle = handle as u64;
        assert!(
            step(&mut vm, unit, f.next, handle, 0).2,
            "next of {bytes:x?}"
        );
        let end = bytes.len() as u64;
        if bytes == b"\xe2\x41\xac" {
            // The `A` before the end is well-formed; the scalar ending at the end is not.
            assert_eq!(step(&mut vm, unit, f.next, handle, 1), (0x41, 2, false));
        }
        assert!(
            step(&mut vm, unit, f.prev, handle, end).2,
            "prev of {bytes:x?}"
        );
    }
}

#[test]
fn guest_loops_scan_forward_and_backward() {
    let texts = vec![
        MIXED.as_bytes().to_vec(),
        Vec::new(),
        "plain ascii".as_bytes().to_vec(),
        // Decoding stops at the malformed byte, wherever the loop meets it.
        b"ab\xffcd".to_vec(),
    ];
    let (mut vm, f, unit) = vm(texts.clone());
    let mut run = |func, args: &[Value]| {
        vm.run(FuncRef::new(unit, func), args, TraceMask::NONE, None)
            .unwrap()
    };
    for (handle, (scalars, stop, back, back_stop)) in [
        (4, MIXED.len() as u64, 4, 0),
        (0, 0, 0, 0),
        (11, 11, 11, 0),
        (2, 2, 2, 3),
    ]
    .into_iter()
    .enumerate()
    {
        let end = texts[handle].len() as u64;
        let handle = handle as u64;
        assert_eq!(
            run(f.count, &[text(handle)]),
            [Value::U64(scalars), Value::U64(stop)]
        );
        assert_eq!(
            run(f.count_back, &[text(handle), Value::U64(end)]),
            [Value::U64(back), Value::U64(back_stop)]
        );
    }
}

#[test]
fn each_decode_costs_one_unit_of_fuel_and_needs_bytes() {
    let f = program();
    for (fuel, ok) in [(1, false), (2, true)] {
        let mut vm = Vm::new(
            Texts {
                texts: vec![b"a".to_vec()],
            },
            Limits {
                fuel,
                ..Limits::default()
            },
        );
        let unit = vm.install(Arc::clone(&f.program)).unwrap();
        let result = vm.run(
            FuncRef::new(unit, f.next),
            &[text(0), Value::U64(0)],
            TraceMask::NONE,
            None,
        );
        assert_eq!(result.is_ok(), ok, "{result:?}");
    }
    let (mut vm, f, unit) = vm(Vec::new());
    for func in [f.next, f.prev] {
        assert!(matches!(
            vm.run(
                FuncRef::new(unit, func),
                &[text(0), Value::U64(0)],
                TraceMask::NONE,
                None
            )
            .unwrap_err()
            .trap,
            Trap::NoObjectBytes { .. }
        ));
    }
}

#[test]
fn the_verifier_types_scalars_positions_and_the_flag() {
    let build_with = |types: [ValueType; 5], forward: bool| {
        let mut b = ProgramBuilder::new();
        let f = b
            .declare_function(FunctionSig {
                arg_types: vec![types[3], types[4]],
                ret_types: vec![],
            })
            .unwrap();
        let mut a = b.function_asm(f).unwrap();
        let (scalar, pos, missing) = (
            a.alloc_register(types[0]),
            a.alloc_register(types[1]),
            a.alloc_register(types[2]),
        );
        if forward {
            a.obj_utf8_next(scalar, pos, missing, Reg(1), Reg(2));
        } else {
            a.obj_utf8_prev(scalar, pos, missing, Reg(1), Reg(2));
        }
        a.ret(Reg::EFFECT, &[]);
        b.define_function(f, a).unwrap();
        b.build()
    };
    let build = |types| build_with(types, true);
    let verify = |program: &Program| verify_program(program, &VerifyConfig::default());
    let good = [
        ValueType::U32,
        ValueType::U64,
        ValueType::Bool,
        ValueType::Obj(TEXTS),
        ValueType::U64,
    ];
    assert!(verify(&build(good)).is_ok());
    assert!(verify(&build_with(good, false)).is_ok());
    let mut object = good;
    object[3] = ValueType::Str;
    assert!(matches!(
        verify(&build(object)).unwrap_err(),
        VerifyError::ObjectRegisterRequired { reg: Reg(1), .. }
    ));
    for (slot, wrong, reg) in [
        (4, ValueType::I64, Reg(2)),
        (0, ValueType::U64, Reg(3)),
        (1, ValueType::I64, Reg(4)),
        (2, ValueType::U64, Reg(5)),
    ] {
        let mut types = good;
        types[slot] = wrong;
        for forward in [true, false] {
            let error = verify(&build_with(types, forward)).unwrap_err();
            assert!(
                matches!(error, VerifyError::RegisterTypeMismatch { reg: r, declared, .. } if r == reg && declared == wrong),
                "{slot}, forward {forward}: {error:?}"
            );
        }
    }
}

#[test]
fn decodes_are_prefixed_opcodes_with_a_disassembly() {
    for (opcode, code, mnemonic) in [
        (Opcode::ObjUtf8Next, 0xFC22, "obj.utf8_next"),
        (Opcode::ObjUtf8Prev, 0xFC23, "obj.utf8_prev"),
    ] {
        assert_eq!(opcode.prefix(), Some(OpcodePrefix::Extended));
        assert_eq!(opcode.code(), code);
        assert_eq!(opcode.mnemonic(), mnemonic);
    }
    let f = program();
    for (func, mnemonic) in [(f.next, "obj.utf8_next"), (f.prev, "obj.utf8_prev")] {
        let disassembly =
            execution_tape::disasm::disassemble_function(f.program.program(), func).unwrap();
        let first = disassembly.instrs().next().unwrap();
        assert!(
            first
                .to_string()
                .contains(&format!("{mnemonic} r3, r4, r5, [r1, r2]")),
            "{first}"
        );
        assert_eq!(first.writes().collect::<Vec<_>>(), [Reg(3), Reg(4), Reg(5)]);
        assert_eq!(first.reads().collect::<Vec<_>>(), [Reg(1), Reg(2)]);
    }
}

#[test]
fn recordings_replay_a_scan_without_the_host() {
    let f = program();
    let mut vm = Vm::new(
        Texts {
            texts: vec![MIXED.as_bytes().to_vec()],
        },
        Limits::default(),
    );
    let unit = vm.install(Arc::clone(&f.program)).unwrap();
    let mut recorder = Recorder::new(Arc::clone(&f.program), RecordLimits::default());
    let live = vm
        .run(
            FuncRef::new(unit, f.count),
            &[text(0)],
            recorder.mask(),
            Some(&mut recorder),
        )
        .unwrap();
    let recording = recorder.finish();
    recording.status().unwrap();
    // Five decodes, four scalars and the missing position after them, with no host code
    // between them: one recorded read answers them all.
    let [query] = recording.object_queries() else {
        panic!("one recorded read");
    };
    assert_eq!(query.answer, ObjectAnswer::Bytes(Some(0)));
    assert_eq!(&*recording.object_bytes()[0], MIXED.as_bytes());
    let ReplayOutcome::Returned(snapshot) = recording.replay().unwrap() else {
        panic!("the replay returned");
    };
    assert_eq!(snapshot.values(), live);
}
