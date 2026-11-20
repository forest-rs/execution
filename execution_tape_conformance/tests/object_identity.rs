// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `obj.eq` compares two host objects' host types and handles in the guest: no host call, no
//! host query, one unit of fuel.

use execution_tape::{
    asm::{FunctionSig, ProgramBuilder, Reg},
    host::{Host, HostContext, HostError, SigHash, ValueRef},
    opcode::{Opcode, OpcodePrefix},
    program::{HostTypeId, Pc, Program, ValueType},
    trace::{ScopeKind, TraceLocation, TraceMask, TraceSink, ValueEvent},
    value::{FuncId, FuncRef, Obj, ObjHandle, Value},
    verifier::{VerifiedProgram, VerifyConfig, VerifyError, verify_program},
    vm::{Limits, Vm},
};
use std::sync::Arc;

const SHAPES: HostTypeId = HostTypeId(0x5348_4150);
const OTHER: HostTypeId = HostTypeId(0x4f54_4852);

/// A host the guest never asks.
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
        unreachable!("obj.eq makes no host call")
    }

    fn object_key(&self, _: Obj) -> Option<u64> {
        unreachable!("obj.eq asks the host nothing")
    }
}

fn object(host_type: HostTypeId, handle: u64) -> Value {
    Value::Obj(Obj {
        host_type,
        handle: ObjHandle(handle),
    })
}

/// `same(a :: Obj(SHAPES), b :: Obj(OTHER)) -> bool`.
fn program() -> (Arc<VerifiedProgram>, FuncId) {
    let mut b = ProgramBuilder::new();
    let same = b
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::Obj(SHAPES), ValueType::Obj(OTHER)],
            ret_types: vec![ValueType::Bool],
        })
        .unwrap();
    let mut a = b.function_asm(same).unwrap();
    let out = a.alloc_register(ValueType::Bool);
    a.obj_eq(out, Reg(1), Reg(2)).ret(Reg::EFFECT, &[out]);
    b.define_function(same, a).unwrap();
    (Arc::new(b.build_verified().unwrap()), same)
}

/// Whether any host query or host call was observed.
#[derive(Default)]
struct Quiet {
    host: usize,
}
impl TraceSink for Quiet {
    fn mask(&self) -> TraceMask {
        TraceMask::VALUES | TraceMask::HOST
    }
    fn values(&mut self, _: &Program, event: ValueEvent<'_>) {
        if matches!(
            event,
            ValueEvent::HostCall { .. }
                | ValueEvent::ObjectKey { .. }
                | ValueEvent::ObjectConversion { .. }
                | ValueEvent::ObjectBytes { .. }
        ) {
            self.host += 1;
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
            self.host += 1;
        }
    }
}

#[test]
fn obj_eq_compares_host_types_and_handles_without_the_host() {
    let (program, same) = program();
    // One unit of fuel for `obj.eq` and one for `ret`, and no host call allowance.
    let mut vm = Vm::new(
        Silent,
        Limits {
            fuel: 2,
            max_host_calls: 0,
            ..Limits::default()
        },
    );
    let unit = vm.install(Arc::clone(&program)).unwrap();
    let mut run = |a, b| {
        let mut sink = Quiet::default();
        let result = vm
            .run(
                FuncRef::new(unit, same),
                &[a, b],
                TraceMask::VALUES | TraceMask::HOST,
                Some(&mut sink),
            )
            .unwrap();
        assert_eq!(sink.host, 0);
        result
    };
    assert_eq!(
        run(object(SHAPES, 3), object(OTHER, 3)),
        [Value::Bool(false)],
        "the same handle of another host type is another object"
    );
}

#[test]
fn obj_eq_is_true_exactly_for_the_same_object() {
    let mut b = ProgramBuilder::new();
    let same = b
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::Obj(SHAPES), ValueType::Obj(SHAPES)],
            ret_types: vec![ValueType::Bool],
        })
        .unwrap();
    let mut a = b.function_asm(same).unwrap();
    let out = a.alloc_register(ValueType::Bool);
    a.obj_eq(out, Reg(1), Reg(2)).ret(Reg::EFFECT, &[out]);
    b.define_function(same, a).unwrap();
    let program = Arc::new(b.build_verified().unwrap());
    let mut vm = Vm::new(Silent, Limits::default());
    let unit = vm.install(program).unwrap();
    for (left, right, expected) in [(1, 1, true), (1, 2, false), (u64::MAX, u64::MAX, true)] {
        let result = vm
            .run(
                FuncRef::new(unit, same),
                &[object(SHAPES, left), object(SHAPES, right)],
                TraceMask::NONE,
                None,
            )
            .unwrap();
        assert_eq!(result, [Value::Bool(expected)], "{left} == {right}");
    }
}

#[test]
fn the_verifier_requires_objects_and_a_bool_destination() {
    let build = |left: ValueType, dst: ValueType| {
        let mut b = ProgramBuilder::new();
        let f = b
            .declare_function(FunctionSig {
                arg_types: vec![left, ValueType::Obj(SHAPES)],
                ret_types: vec![],
            })
            .unwrap();
        let mut a = b.function_asm(f).unwrap();
        let out = a.alloc_register(dst);
        a.obj_eq(out, Reg(1), Reg(2)).ret(Reg::EFFECT, &[]);
        b.define_function(f, a).unwrap();
        b.build()
    };
    let verify = |program: &Program| verify_program(program, &VerifyConfig::default());
    assert!(verify(&build(ValueType::Obj(OTHER), ValueType::Bool)).is_ok());
    assert_eq!(
        verify(&build(ValueType::U64, ValueType::Bool)).unwrap_err(),
        VerifyError::ObjectRegisterRequired {
            func: FuncId(0),
            pc: Pc(0),
            reg: Reg(1),
            declared: ValueType::U64,
        }
    );
    assert!(matches!(
        verify(&build(ValueType::Obj(SHAPES), ValueType::U64)).unwrap_err(),
        VerifyError::RegisterTypeMismatch {
            reg: Reg(3),
            declared: ValueType::U64,
            required: ValueType::Bool,
            ..
        }
    ));
}

#[test]
fn obj_eq_is_a_prefixed_opcode_with_a_disassembly() {
    assert_eq!(Opcode::ObjEq.prefix(), Some(OpcodePrefix::Extended));
    assert_eq!(Opcode::ObjEq.code(), 0xFC0F);
    assert_eq!(Opcode::ObjEq.mnemonic(), "obj.eq");
    let (program, same) = program();
    let disassembly =
        execution_tape::disasm::disassemble_function(program.program(), same).unwrap();
    let first = disassembly.instrs().next().unwrap();
    assert!(
        first.to_string().ends_with("obj.eq r3, [r1, r2]"),
        "{first}"
    );
    assert_eq!(first.dst(), Some(Reg(3)));
    assert_eq!(first.reads().collect::<Vec<_>>(), [Reg(1), Reg(2)]);
}
