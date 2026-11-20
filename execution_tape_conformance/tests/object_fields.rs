// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! `obj.field` and `obj.set_field` read and store a host object's fields straight through the
//! host: no host call, request or allowance, one unit of fuel, and a trap when the host has no
//! such field or refuses the store.

use execution_tape::{
    asm::{FunctionSig, ProgramBuilder, Reg},
    host::{Host, HostContext, HostError, SigHash, ValueRef},
    host_binding::{HostFunctions, HostImport},
    opcode::{Opcode, OpcodePrefix},
    program::{HostTypeId, Pc, Program, ValueType},
    recording::{ObjectAnswer, RecordLimits, Recorder, ReplayOutcome},
    trace::{TraceMask, TraceSink},
    value::{FuncId, FuncRef, Obj, ObjHandle, Value},
    verifier::{VerifiedProgram, VerifyConfig, VerifyError, verify_program},
    vm::{Limits, Trap, Vm},
};
use std::sync::Arc;

const POINTS: HostTypeId = HostTypeId(0x504f_494e);
const OTHER: HostTypeId = HostTypeId(0x4f54_4852);

/// Points with an `i64` field 0 and an object field 1, by handle; field 2 holds integers only.
#[derive(Default)]
struct Points {
    points: Vec<[Value; 2]>,
    stores: usize,
    calls: usize,
}
impl Points {
    fn with(count: usize) -> Self {
        Self {
            points: (0..count)
                .map(|index| [Value::I64(index as i64 * 10), point(index as u64)])
                .collect(),
            ..Self::default()
        }
    }
}
impl Host for Points {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        _: &[ValueRef<'_>],
        _: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        self.calls += 1;
        Err(HostError::Failed)
    }

    fn object_field(&self, object: Obj, field: u32) -> Option<Value> {
        if object.host_type != POINTS {
            return None;
        }
        let fields = self.points.get(usize::try_from(object.handle.0).ok()?)?;
        fields.get(usize::try_from(field).ok()?).cloned()
    }

    fn set_object_field(&mut self, object: Obj, field: u32, value: Value) -> bool {
        self.stores += 1;
        if object.host_type != POINTS {
            return false;
        }
        let (Ok(handle), Ok(field_index)) =
            (usize::try_from(object.handle.0), usize::try_from(field))
        else {
            return false;
        };
        let Some(slot) = self
            .points
            .get_mut(handle)
            .and_then(|fields| fields.get_mut(field_index))
        else {
            return false;
        };
        // Field 0 holds integers only.
        if field == 0 && !matches!(value, Value::I64(_)) {
            return false;
        }
        *slot = value;
        true
    }
}

fn point(handle: u64) -> Value {
    Value::Obj(Obj {
        host_type: POINTS,
        handle: ObjHandle(handle),
    })
}

struct Fields {
    program: Arc<VerifiedProgram>,
    /// `bump(p) -> i64`: stores field 0 plus one into field 0 and returns it.
    bump: FuncId,
    /// `next(p) -> Obj`: field 1.
    next: FuncId,
    /// `link(p, q)`: stores `q` in `p`'s field 1.
    link: FuncId,
    /// `as_float(p) -> f64`: field 0 read as a float.
    as_float: FuncId,
    /// `field_two(p) -> i64`: field 2, which no point has.
    field_two: FuncId,
}

fn fields() -> Fields {
    let mut b = ProgramBuilder::new();
    let object = ValueType::Obj(POINTS);
    let sig = |args: &[ValueType], rets: &[ValueType]| FunctionSig {
        arg_types: args.to_vec(),
        ret_types: rets.to_vec(),
    };
    let bump = b
        .declare_function(sig(&[object], &[ValueType::I64]))
        .unwrap();
    let next = b.declare_function(sig(&[object], &[object])).unwrap();
    let link = b.declare_function(sig(&[object, object], &[])).unwrap();
    let as_float = b
        .declare_function(sig(&[object], &[ValueType::F64]))
        .unwrap();
    let field_two = b
        .declare_function(sig(&[object], &[ValueType::I64]))
        .unwrap();

    let mut a = b.function_asm(bump).unwrap();
    let (n, one) = (
        a.alloc_register(ValueType::I64),
        a.alloc_register(ValueType::I64),
    );
    a.obj_field(n, Reg(1), 0)
        .const_i64(one, 1)
        .i64_add(n, n, one)
        .obj_set_field(Reg(1), 0, n)
        .ret(Reg::EFFECT, &[n]);
    b.define_function(bump, a).unwrap();

    let mut a = b.function_asm(next).unwrap();
    let out = a.alloc_register(object);
    a.obj_field(out, Reg(1), 1).ret(Reg::EFFECT, &[out]);
    b.define_function(next, a).unwrap();

    let mut a = b.function_asm(link).unwrap();
    a.obj_set_field(Reg(1), 1, Reg(2)).ret(Reg::EFFECT, &[]);
    b.define_function(link, a).unwrap();

    let mut a = b.function_asm(as_float).unwrap();
    let out = a.alloc_register(ValueType::F64);
    a.obj_field(out, Reg(1), 0).ret(Reg::EFFECT, &[out]);
    b.define_function(as_float, a).unwrap();

    let mut a = b.function_asm(field_two).unwrap();
    let out = a.alloc_register(ValueType::I64);
    a.obj_field(out, Reg(1), 2).ret(Reg::EFFECT, &[out]);
    b.define_function(field_two, a).unwrap();

    Fields {
        program: Arc::new(b.build_verified().unwrap()),
        bump,
        next,
        link,
        as_float,
        field_two,
    }
}

fn run(vm: &mut Vm<Points>, func: FuncId, args: &[Value]) -> Result<Vec<Value>, Trap> {
    let unit = vm.units().iter().next().expect("installed").0;
    vm.run(FuncRef::new(unit, func), args, TraceMask::NONE, None)
        .map_err(|error| error.trap)
}

#[test]
fn fields_are_read_and_stored_without_a_host_call() {
    let f = fields();
    // One unit of fuel per instruction, and no host-call allowance.
    let mut vm = Vm::new(
        Points::with(3),
        Limits {
            fuel: 5,
            max_host_calls: 0,
            ..Limits::default()
        },
    );
    vm.install(Arc::clone(&f.program)).unwrap();
    assert_eq!(run(&mut vm, f.bump, &[point(2)]).unwrap(), [Value::I64(21)]);
    assert_eq!(run(&mut vm, f.bump, &[point(2)]).unwrap(), [Value::I64(22)]);
    assert_eq!(run(&mut vm, f.link, &[point(0), point(2)]).unwrap(), []);
    assert_eq!(run(&mut vm, f.next, &[point(0)]).unwrap(), [point(2)]);
    let host = vm.host();
    assert_eq!(host.calls, 0);
    assert_eq!(host.stores, 3);
    assert_eq!(host.points[2][0], Value::I64(22));
}

#[test]
fn a_missing_field_a_refused_store_and_a_wrong_type_trap() {
    let f = fields();
    let mut vm = Vm::new(Points::with(2), Limits::default());
    vm.install(Arc::clone(&f.program)).unwrap();
    let missing = Obj {
        host_type: POINTS,
        handle: ObjHandle(5),
    };
    assert!(matches!(
        run(&mut vm, f.next, &[Value::Obj(missing)]),
        Err(Trap::NoObjectField { object, field: 1 }) if object == missing
    ));
    assert!(matches!(
        run(&mut vm, f.field_two, &[point(1)]),
        Err(Trap::NoObjectField { field: 2, .. })
    ));
    // The host refuses an object in its integer field.
    let mut b = ProgramBuilder::new();
    let store = b
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::Obj(POINTS)],
            ret_types: vec![],
        })
        .unwrap();
    let mut a = b.function_asm(store).unwrap();
    a.obj_set_field(Reg(1), 0, Reg(1)).ret(Reg::EFFECT, &[]);
    b.define_function(store, a).unwrap();
    let mut other = Vm::new(Points::with(2), Limits::default());
    other
        .install(Arc::new(b.build_verified().unwrap()))
        .unwrap();
    assert!(matches!(
        run(&mut other, store, &[point(1)]),
        Err(Trap::NoObjectField { field: 0, .. })
    ));
    // The value a field holds must have the register's type.
    assert!(matches!(
        run(&mut vm, f.as_float, &[point(1)]),
        Err(Trap::TypeMismatch {
            expected: ValueType::F64,
            actual: ValueType::I64
        })
    ));
}

#[test]
fn a_field_read_keeps_the_registers_host_type() {
    let mut b = ProgramBuilder::new();
    let read = b
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::Obj(POINTS)],
            ret_types: vec![ValueType::Obj(OTHER)],
        })
        .unwrap();
    let mut a = b.function_asm(read).unwrap();
    let out = a.alloc_register(ValueType::Obj(OTHER));
    a.obj_field(out, Reg(1), 1).ret(Reg::EFFECT, &[out]);
    b.define_function(read, a).unwrap();
    let mut vm = Vm::new(Points::with(2), Limits::default());
    vm.install(Arc::new(b.build_verified().unwrap())).unwrap();
    assert!(matches!(
        run(&mut vm, read, &[point(1)]),
        Err(Trap::TypeMismatch {
            expected: ValueType::Obj(OTHER),
            actual: ValueType::Obj(POINTS)
        })
    ));
}

#[test]
fn the_verifier_requires_an_object_and_an_inline_value() {
    let build = |object: ValueType, value: ValueType| {
        let mut b = ProgramBuilder::new();
        let f = b
            .declare_function(FunctionSig {
                arg_types: vec![object, value],
                ret_types: vec![],
            })
            .unwrap();
        let mut a = b.function_asm(f).unwrap();
        a.obj_set_field(Reg(1), 0, Reg(2)).ret(Reg::EFFECT, &[]);
        b.define_function(f, a).unwrap();
        b.build()
    };
    let verify = |program: &Program| verify_program(program, &VerifyConfig::default());
    assert!(verify(&build(ValueType::Obj(OTHER), ValueType::F32)).is_ok());
    assert_eq!(
        verify(&build(ValueType::I64, ValueType::I64)).unwrap_err(),
        VerifyError::ObjectRegisterRequired {
            func: FuncId(0),
            pc: Pc(0),
            reg: Reg(1),
            declared: ValueType::I64,
        }
    );
    assert_eq!(
        verify(&build(ValueType::Obj(POINTS), ValueType::Str)).unwrap_err(),
        VerifyError::InlineRegisterRequired {
            func: FuncId(0),
            pc: Pc(0),
            reg: Reg(2),
            declared: ValueType::Str,
        }
    );
}

#[test]
fn field_opcodes_are_prefixed_with_a_disassembly() {
    for (opcode, code, mnemonic) in [
        (Opcode::ObjField, 0xFC27, "obj.field"),
        (Opcode::ObjSetField, 0xFC28, "obj.set_field"),
    ] {
        assert_eq!(opcode.prefix(), Some(OpcodePrefix::Extended));
        assert_eq!(opcode.code(), code);
        assert_eq!(opcode.mnemonic(), mnemonic);
    }
    let f = fields();
    let disassembly =
        execution_tape::disasm::disassemble_function(f.program.program(), f.link).unwrap();
    let store = disassembly.instrs().next().unwrap();
    assert!(store.to_string().contains("obj.set_field"), "{store}");
    assert_eq!(
        store.input_index(),
        Some(execution_tape::disasm::InputIndex::Index(1))
    );
    assert_eq!(store.dst(), None);
    assert_eq!(store.reads().collect::<Vec<_>>(), [Reg(1), Reg(2)]);
}

#[test]
fn recordings_keep_reads_and_stores_and_replay_them_without_the_host() {
    let f = fields();
    let mut vm = Vm::new(Points::with(3), Limits::default());
    let unit = vm.install(Arc::clone(&f.program)).unwrap();
    let mut recorder = Recorder::new(Arc::clone(&f.program), RecordLimits::default());
    let result = vm
        .run(
            FuncRef::new(unit, f.bump),
            &[point(1)],
            recorder.mask(),
            Some(&mut recorder),
        )
        .unwrap();
    assert_eq!(result, [Value::I64(11)]);
    let recording = recorder.finish();
    recording.status().unwrap();
    let answers = recording
        .object_queries()
        .iter()
        .map(|query| query.answer)
        .collect::<Vec<_>>();
    assert_eq!(
        answers,
        [
            ObjectAnswer::Field {
                field: 0,
                value: Some(0)
            },
            ObjectAnswer::FieldSet {
                field: 0,
                value: 1,
                accepted: true
            }
        ]
    );
    assert_eq!(recording.object_values(), [Value::I64(10), Value::I64(11)]);
    match recording.replay().unwrap() {
        ReplayOutcome::Returned(values) => assert_eq!(values.values(), &result[..]),
        ReplayOutcome::Trapped(error) => panic!("replay trapped: {error:?}"),
    }
    // The replay changed nothing in the host.
    assert_eq!(vm.host().points[1][0], Value::I64(11));
}

#[test]
fn bound_hosts_answer_fields_from_their_state() {
    const PING: HostImport = HostImport::new("ping", &[], &[]);
    let f = fields();
    let mut functions = HostFunctions::<Vec<i64>>::new();
    functions.implement(PING, |_, _, _, _| Ok(0)).unwrap();
    let bindings = functions.bind(&f.program).unwrap();
    let host = bindings.with_state(vec![4, 5]).with_object_fields(
        |values, object, field| {
            (field == 0)
                .then(|| values.get(usize::try_from(object.handle.0).ok()?).copied())
                .flatten()
                .map(Value::I64)
        },
        |values, object, field, value| match (field, value) {
            (0, Value::I64(value)) => values
                .get_mut(usize::try_from(object.handle.0).unwrap_or(usize::MAX))
                .map(|slot| *slot = value)
                .is_some(),
            _ => false,
        },
    );
    let mut vm = Vm::new(host, Limits::default());
    let unit = vm.install(Arc::clone(&f.program)).unwrap();
    let result = vm
        .run(
            FuncRef::new(unit, f.bump),
            &[point(1)],
            TraceMask::NONE,
            None,
        )
        .unwrap();
    assert_eq!(result, [Value::I64(6)]);
    assert_eq!(vm.host().state(), &vec![4, 6]);
    // Without callbacks no object has fields.
    let mut bare = Vm::new(bindings.with_state(vec![]), Limits::default());
    let unit = bare.install(Arc::clone(&f.program)).unwrap();
    assert!(matches!(
        bare.run(
            FuncRef::new(unit, f.bump),
            &[point(1)],
            TraceMask::NONE,
            None
        )
        .unwrap_err()
        .trap,
        Trap::NoObjectField { .. }
    ));
}

#[test]
fn replay_compares_float_fields_by_their_bits() {
    // `swap(p)` reads field 0 and stores it back, which holds whatever float the host has.
    let mut b = ProgramBuilder::new();
    let swap = b
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::Obj(POINTS)],
            ret_types: vec![ValueType::F64],
        })
        .unwrap();
    let mut a = b.function_asm(swap).unwrap();
    let value = a.alloc_register(ValueType::F64);
    a.obj_field(value, Reg(1), 0)
        .obj_set_field(Reg(1), 0, value)
        .ret(Reg::EFFECT, &[value]);
    b.define_function(swap, a).unwrap();
    let program = Arc::new(b.build_verified().unwrap());

    /// One point whose field 0 holds any `f64`.
    struct Floats(f64);
    impl Host for Floats {
        fn call(
            &mut self,
            _: &str,
            _: SigHash,
            _: &[ValueRef<'_>],
            _: &mut [Value],
            _: HostContext<'_, '_>,
        ) -> Result<u64, HostError> {
            Err(HostError::Failed)
        }
        fn object_field(&self, _: Obj, field: u32) -> Option<Value> {
            (field == 0).then_some(Value::F64(self.0))
        }
        fn set_object_field(&mut self, _: Obj, field: u32, value: Value) -> bool {
            match (field, value) {
                (0, Value::F64(value)) => {
                    self.0 = value;
                    true
                }
                _ => false,
            }
        }
    }
    for float in [f64::NAN, -0.0, 0.0] {
        let mut vm = Vm::new(Floats(float), Limits::default());
        let unit = vm.install(Arc::clone(&program)).unwrap();
        let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
        vm.run(
            FuncRef::new(unit, swap),
            &[point(0)],
            recorder.mask(),
            Some(&mut recorder),
        )
        .unwrap();
        let recording = recorder.finish();
        recording.status().unwrap();
        assert!(
            matches!(recording.replay().unwrap(), ReplayOutcome::Returned(_)),
            "a recording of {float} replays"
        );
    }
}
