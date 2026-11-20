// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `global.has`, `global.get` and `global.set` keep scalars and host objects in a unit's globals
//! for the rest of a computation: a guest caches what it computed once, such as a constant's
//! value or an object the host made, without a host call per read.

use execution_tape::{
    asm::{FunctionSig, ProgramBuilder, Reg},
    host::{Host, HostContext, HostError, SigHash, ValueRef},
    opcode::{Opcode, OpcodePrefix},
    program::{HostTypeId, Pc, Program, ValueType},
    recording::{RecordLimits, Recorder, ReplayOutcome},
    trace::{TraceMask, TraceSink},
    value::{FuncId, FuncRef, Obj, ObjHandle, Value},
    verifier::{MAX_GLOBALS, VerifiedProgram, VerifyConfig, VerifyError, verify_program},
    vm::{Limits, Trap, Vm},
};
use std::sync::Arc;

const SHAPES: HostTypeId = HostTypeId(0x5348_4150);
const OTHER: HostTypeId = HostTypeId(0x4f54_4852);

/// A host the guest never calls.
struct Silent;
impl Host for Silent {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        _: &[ValueRef<'_>],
        _: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        unreachable!("globals make no host call")
    }
}

fn shape(handle: u64) -> Value {
    Value::Obj(Obj {
        host_type: SHAPES,
        handle: ObjHandle(handle),
    })
}

struct Globals {
    program: Arc<VerifiedProgram>,
    /// `remember(obj) -> bool`: whether global 0 was set before; then sets it to `obj`.
    remember: FuncId,
    /// `recall() -> Obj(SHAPES)`: global 0.
    recall: FuncId,
    /// `twice(obj) -> Obj(SHAPES)`: `remember(obj)` and then `recall()`.
    twice: FuncId,
    /// `as_other() -> Obj(OTHER)`: global 0 read as an object of another host type.
    as_other: FuncId,
    /// `count() -> i64`: global 1 plus one, stored back; 1 the first time.
    count: FuncId,
}

fn globals() -> Globals {
    let mut b = ProgramBuilder::new();
    let object = ValueType::Obj(SHAPES);
    let remember = b
        .declare_function(FunctionSig {
            arg_types: vec![object],
            ret_types: vec![ValueType::Bool],
        })
        .unwrap();
    let recall = b
        .declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![object],
        })
        .unwrap();
    let twice = b
        .declare_function(FunctionSig {
            arg_types: vec![object],
            ret_types: vec![object],
        })
        .unwrap();
    let as_other = b
        .declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::Obj(OTHER)],
        })
        .unwrap();
    let count = b
        .declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();

    let mut a = b.function_asm(remember).unwrap();
    let was = a.alloc_register(ValueType::Bool);
    a.global_has(was, 0)
        .global_set(0, Reg(1))
        .ret(Reg::EFFECT, &[was]);
    b.define_function(remember, a).unwrap();

    let mut a = b.function_asm(recall).unwrap();
    let out = a.alloc_register(object);
    a.global_get(out, 0).ret(Reg::EFFECT, &[out]);
    b.define_function(recall, a).unwrap();

    let mut a = b.function_asm(twice).unwrap();
    let [_] = b.emit_call(&mut a, remember, &[Reg(1)]).unwrap();
    let [out] = b.emit_call(&mut a, recall, &[]).unwrap();
    a.ret(Reg::EFFECT, &[out]);
    b.define_function(twice, a).unwrap();

    let mut a = b.function_asm(as_other).unwrap();
    let out = a.alloc_register(ValueType::Obj(OTHER));
    a.global_get(out, 0).ret(Reg::EFFECT, &[out]);
    b.define_function(as_other, a).unwrap();

    let mut a = b.function_asm(count).unwrap();
    let (set, n, one) = (
        a.alloc_register(ValueType::Bool),
        a.alloc_register(ValueType::I64),
        a.alloc_register(ValueType::I64),
    );
    let (stored, fresh, done) = (a.label(), a.label(), a.label());
    a.const_i64(one, 1)
        .global_has(set, 1)
        .br(set, stored, fresh);
    a.place(stored).unwrap();
    a.global_get(n, 1).i64_add(n, n, one).jmp(done);
    a.place(fresh).unwrap();
    a.const_i64(n, 1).jmp(done);
    a.place(done).unwrap();
    a.global_set(1, n).ret(Reg::EFFECT, &[n]);
    b.define_function(count, a).unwrap();

    Globals {
        program: Arc::new(b.build_verified().unwrap()),
        remember,
        recall,
        twice,
        as_other,
        count,
    }
}

fn run(
    vm: &mut Vm<Silent>,
    unit: execution_tape::value::UnitId,
    func: FuncId,
    args: &[Value],
) -> Result<Vec<Value>, Trap> {
    vm.run(FuncRef::new(unit, func), args, TraceMask::NONE, None)
        .map_err(|error| error.trap)
}

#[test]
fn a_global_holds_its_value_for_the_rest_of_the_computation() {
    let g = globals();
    let mut vm = Vm::new(
        Silent,
        Limits {
            max_host_calls: 0,
            ..Limits::default()
        },
    );
    let unit = vm.install(Arc::clone(&g.program)).unwrap();
    // Stored by one function, read by another.
    assert_eq!(
        run(&mut vm, unit, g.twice, &[shape(5)]).unwrap(),
        [shape(5)]
    );
    // A later computation starts with every global unset.
    assert_eq!(
        run(&mut vm, unit, g.remember, &[shape(6)]).unwrap(),
        [Value::Bool(false)]
    );
    assert!(matches!(
        run(&mut vm, unit, g.recall, &[]),
        Err(Trap::GlobalUnset { index: 0 })
    ));
    assert_eq!(run(&mut vm, unit, g.count, &[]).unwrap(), [Value::I64(1)]);
    assert_eq!(run(&mut vm, unit, g.count, &[]).unwrap(), [Value::I64(1)]);
}

#[test]
fn a_global_keeps_its_type() {
    let g = globals();
    let mut b = ProgramBuilder::new();
    // A unit of its own, whose global 0 is an integer: each unit has its own globals.
    let f = b
        .declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::F64],
        })
        .unwrap();
    let mut a = b.function_asm(f).unwrap();
    let (n, out) = (
        a.alloc_register(ValueType::I64),
        a.alloc_register(ValueType::F64),
    );
    a.const_i64(n, 7)
        .global_set(0, n)
        .global_get(out, 0)
        .ret(Reg::EFFECT, &[out]);
    b.define_function(f, a).unwrap();
    let mut vm = Vm::new(Silent, Limits::default());
    let other = vm.install(Arc::new(b.build_verified().unwrap())).unwrap();
    assert!(matches!(
        run(&mut vm, other, f, &[]),
        Err(Trap::TypeMismatch {
            expected: ValueType::F64,
            actual: ValueType::I64
        })
    ));

    // An object register holds only objects of its own host type.
    let mut b = ProgramBuilder::new();
    let both = b
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::Obj(SHAPES)],
            ret_types: vec![ValueType::Obj(OTHER)],
        })
        .unwrap();
    let mut a = b.function_asm(both).unwrap();
    let out = a.alloc_register(ValueType::Obj(OTHER));
    a.global_set(0, Reg(1))
        .global_get(out, 0)
        .ret(Reg::EFFECT, &[out]);
    b.define_function(both, a).unwrap();
    let unit = vm.install(Arc::new(b.build_verified().unwrap())).unwrap();
    assert!(matches!(
        run(&mut vm, unit, both, &[shape(1)]),
        Err(Trap::TypeMismatch {
            expected: ValueType::Obj(OTHER),
            actual: ValueType::Obj(SHAPES)
        })
    ));
    let unit = vm.install(Arc::clone(&g.program)).unwrap();
    assert!(matches!(
        run(&mut vm, unit, g.as_other, &[]),
        Err(Trap::GlobalUnset { index: 0 })
    ));
}

#[test]
fn the_verifier_requires_inline_registers_and_bounded_indexes() {
    let build = |kind: ValueType, index: u32| {
        let mut b = ProgramBuilder::new();
        let f = b
            .declare_function(FunctionSig {
                arg_types: vec![kind],
                ret_types: vec![],
            })
            .unwrap();
        let mut a = b.function_asm(f).unwrap();
        a.global_set(index, Reg(1)).ret(Reg::EFFECT, &[]);
        b.define_function(f, a).unwrap();
        b.build()
    };
    let verify = |program: &Program| verify_program(program, &VerifyConfig::default());
    for kind in [
        ValueType::Bool,
        ValueType::I64,
        ValueType::U64,
        ValueType::F64,
        ValueType::I32,
        ValueType::U32,
        ValueType::F32,
        ValueType::Decimal,
        ValueType::Func,
        ValueType::Obj(SHAPES),
    ] {
        assert!(kind.is_inline());
        assert!(verify(&build(kind, MAX_GLOBALS - 1)).is_ok(), "{kind:?}");
    }
    for kind in [
        ValueType::Str,
        ValueType::Bytes,
        ValueType::Agg,
        ValueType::Closure,
    ] {
        assert!(!kind.is_inline());
        let error = verify(&build(kind, 0)).unwrap_err();
        assert_eq!(
            error,
            VerifyError::InlineRegisterRequired {
                func: FuncId(0),
                pc: Pc(0),
                reg: Reg(1),
                declared: kind,
            }
        );
    }
    assert_eq!(
        verify(&build(ValueType::I64, MAX_GLOBALS)).unwrap_err(),
        VerifyError::GlobalOutOfRange {
            func: FuncId(0),
            pc: Pc(0),
            index: MAX_GLOBALS,
        }
    );
    assert_eq!(
        verify(&build(ValueType::Str, 0)).unwrap_err().to_string(),
        "function #0, pc 0: r1 declared Str, required a scalar or host object"
    );
}

#[test]
fn global_opcodes_are_prefixed_with_a_disassembly() {
    for (opcode, code, mnemonic) in [
        (Opcode::GlobalHas, 0xFC24, "global.has"),
        (Opcode::GlobalGet, 0xFC25, "global.get"),
        (Opcode::GlobalSet, 0xFC26, "global.set"),
    ] {
        assert_eq!(opcode.prefix(), Some(OpcodePrefix::Extended));
        assert_eq!(opcode.code(), code);
        assert_eq!(opcode.mnemonic(), mnemonic);
    }
    let g = globals();
    let disassembly =
        execution_tape::disasm::disassemble_function(g.program.program(), g.remember).unwrap();
    let mut instrs = disassembly.instrs();
    let has = instrs.next().unwrap();
    assert!(has.to_string().contains("global.has r2"), "{has}");
    assert_eq!(has.dst(), Some(Reg(2)));
    let set = instrs.next().unwrap();
    assert!(set.to_string().contains("global.set"), "{set}");
    assert_eq!(set.reads().collect::<Vec<_>>(), [Reg(1)]);
}

#[test]
fn recordings_replay_globals_without_the_host() {
    let g = globals();
    let mut vm = Vm::new(Silent, Limits::default());
    let unit = vm.install(Arc::clone(&g.program)).unwrap();
    let mut recorder = Recorder::new(Arc::clone(&g.program), RecordLimits::default());
    let result = vm
        .run(
            FuncRef::new(unit, g.twice),
            &[shape(9)],
            recorder.mask(),
            Some(&mut recorder),
        )
        .unwrap();
    let recording = recorder.finish();
    recording.status().unwrap();
    match recording.replay().unwrap() {
        ReplayOutcome::Returned(values) => assert_eq!(values.values(), &result[..]),
        ReplayOutcome::Trapped(error) => panic!("replay trapped: {error:?}"),
    }
}
