// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! What a dynamic language pays to read and store an object's fields, compare objects and read
//! a value it computed once.
//!
//! Each case runs a guest loop of `ITERS` iterations that each do one operation and add a
//! `u64` from it to an accumulator. Subtract `loop` and divide by `ITERS` for a
//! per-iteration cost.
//!
//! - `loop` adds a constant instead and is the floor.
//! - `field_host_call` reads a field with a host import `field(obj) -> u64` that the host
//!   recognizes by its ID, the cheapest host call; `obj_field` reads it with `obj.field`
//!   ([`Host::object_field`]).
//! - `store_host_call` stores the counter in the field with a host import, and `obj_set_field`
//!   with `obj.set_field` ([`Host::set_object_field`]).
//! - `same_host_call` asks a host import whether two objects are the same, and `obj_eq`
//!   compares them with `obj.eq`.
//! - `constant_host_call` asks a host import for a value the host keeps, as a language reads a
//!   constant it computed once; `global_get` reads it from a global with `global.get` after
//!   one `global.set` before the loop.

use criterion::{Criterion, criterion_group, criterion_main};
use execution_tape::asm::{FunctionSig, ProgramBuilder, Reg};
use execution_tape::host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef};
use execution_tape::program::{HostTypeId, ValueType};
use execution_tape::trace::TraceMask;
use execution_tape::value::{FuncId, FuncRef, Obj, ObjHandle, Value};
use execution_tape::verifier::VerifiedProgram;
use execution_tape::vm::{Limits, Vm};
use std::hint::black_box;

const ITERS: u64 = 1_000;
const OBJECTS: HostTypeId = HostTypeId(0x4f42_4a53);
const VALUE: u64 = 7;

/// Objects with one `u64` field each.
struct Fields {
    fields: Vec<u64>,
}

impl Host for Fields {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        args: &[ValueRef<'_>],
        rets: &mut [Value],
        ctx: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        match (ctx.host_sig_id().0, args) {
            (0, [ValueRef::Obj(object)]) => {
                rets[0] = self.object_field(*object, 0).ok_or(HostError::Failed)?;
                Ok(0)
            }
            (1, [ValueRef::Obj(object), ValueRef::U64(value)]) => self
                .set_object_field(*object, 0, Value::U64(*value))
                .then_some(0)
                .ok_or(HostError::Failed),
            (2, [ValueRef::Obj(a), ValueRef::Obj(b)]) => {
                rets[0] = Value::Bool(a == b);
                Ok(0)
            }
            (3, []) => {
                rets[0] = Value::U64(VALUE);
                Ok(0)
            }
            _ => Err(HostError::UnknownSymbol),
        }
    }

    fn object_field(&self, object: Obj, field: u32) -> Option<Value> {
        if object.host_type != OBJECTS || field != 0 {
            return None;
        }
        self.fields
            .get(usize::try_from(object.handle.0).ok()?)
            .map(|value| Value::U64(*value))
    }

    fn set_object_field(&mut self, object: Obj, field: u32, value: Value) -> bool {
        match (object.host_type == OBJECTS, field, value) {
            (true, 0, Value::U64(value)) => self
                .fields
                .get_mut(usize::try_from(object.handle.0).unwrap_or(usize::MAX))
                .map(|slot| *slot = value)
                .is_some(),
            _ => false,
        }
    }
}

#[derive(Copy, Clone, Debug)]
enum Op {
    Loop,
    FieldHostCall,
    ObjField,
    StoreHostCall,
    ObjSetField,
    SameHostCall,
    ObjEq,
    ConstantHostCall,
    GlobalGet,
}

const OPS: [Op; 9] = [
    Op::Loop,
    Op::FieldHostCall,
    Op::ObjField,
    Op::StoreHostCall,
    Op::ObjSetField,
    Op::SameHostCall,
    Op::ObjEq,
    Op::ConstantHostCall,
    Op::GlobalGet,
];

impl Op {
    fn name(self) -> &'static str {
        match self {
            Self::Loop => "loop",
            Self::FieldHostCall => "field_host_call",
            Self::ObjField => "obj_field",
            Self::StoreHostCall => "store_host_call",
            Self::ObjSetField => "obj_set_field",
            Self::SameHostCall => "same_host_call",
            Self::ObjEq => "obj_eq",
            Self::ConstantHostCall => "constant_host_call",
            Self::GlobalGet => "global_get",
        }
    }
}

/// One `(iters, obj) -> u64` loop per [`Op`], in [`OPS`] order, each returning `ITERS * VALUE`
/// when the object's field holds `VALUE`.
fn program() -> VerifiedProgram {
    let object = ValueType::Obj(OBJECTS);
    let mut b = ProgramBuilder::new();
    let field = b.host_sig_for(
        "field",
        HostSig {
            args: vec![object],
            rets: vec![ValueType::U64],
        },
    );
    let store = b.host_sig_for(
        "store",
        HostSig {
            args: vec![object, ValueType::U64],
            rets: vec![],
        },
    );
    let same = b.host_sig_for(
        "same",
        HostSig {
            args: vec![object, object],
            rets: vec![ValueType::Bool],
        },
    );
    let constant = b.host_sig_for(
        "constant",
        HostSig {
            args: vec![],
            rets: vec![ValueType::U64],
        },
    );
    for op in OPS {
        let function = b
            .declare_function(FunctionSig {
                arg_types: vec![ValueType::U64, object],
                ret_types: vec![ValueType::U64],
            })
            .unwrap();
        let mut a = b.function_asm(function).unwrap();
        let (iters, receiver) = (Reg(1), Reg(2));
        let (counter, acc, one, value, same_flag) = (
            a.alloc_register(ValueType::U64),
            a.alloc_register(ValueType::U64),
            a.alloc_register(ValueType::U64),
            a.alloc_register(ValueType::U64),
            a.alloc_register(ValueType::Bool),
        );
        let (head, body, exit) = (a.label(), a.label(), a.label());
        a.const_u64(counter, 0)
            .const_u64(acc, 0)
            .const_u64(one, 1)
            .const_u64(value, VALUE);
        if matches!(op, Op::GlobalGet) {
            a.global_set(0, value);
        }
        a.jmp(head);
        a.place(head).unwrap();
        let more = a.alloc_register(ValueType::Bool);
        a.u64_lt(more, counter, iters).br(more, body, exit);
        a.place(body).unwrap();
        match op {
            Op::Loop => {}
            Op::FieldHostCall => {
                a.host_call(Reg::EFFECT, field, Reg::EFFECT, &[receiver], &[value]);
            }
            Op::ObjField => {
                a.obj_field(value, receiver, 0);
            }
            Op::StoreHostCall => {
                a.host_call(Reg::EFFECT, store, Reg::EFFECT, &[receiver, value], &[]);
            }
            Op::ObjSetField => {
                a.obj_set_field(receiver, 0, value);
            }
            Op::SameHostCall => {
                a.host_call(
                    Reg::EFFECT,
                    same,
                    Reg::EFFECT,
                    &[receiver, receiver],
                    &[same_flag],
                );
            }
            Op::ObjEq => {
                a.obj_eq(same_flag, receiver, receiver);
            }
            Op::ConstantHostCall => {
                a.host_call(Reg::EFFECT, constant, Reg::EFFECT, &[], &[value]);
            }
            Op::GlobalGet => {
                a.global_get(value, 0);
            }
        }
        a.u64_add(acc, acc, value)
            .u64_add(counter, counter, one)
            .jmp(head);
        a.place(exit).unwrap();
        a.ret(Reg::EFFECT, &[acc]);
        b.define_function(function, a).unwrap();
    }
    b.build_verified().unwrap()
}

fn object_state(c: &mut Criterion) {
    let limits = Limits {
        fuel: u64::MAX,
        max_host_calls: u64::MAX,
        ..Limits::default()
    };
    let mut vm = Vm::new(
        Fields {
            fields: vec![VALUE],
        },
        limits,
    );
    let unit = vm.install(program()).unwrap();
    let receiver = Obj {
        host_type: OBJECTS,
        handle: ObjHandle(0),
    };
    let args = [Value::U64(ITERS), Value::Obj(receiver)];
    let mut group = c.benchmark_group("object_state");
    let mut out = Vec::new();
    for (index, op) in OPS.iter().enumerate() {
        let entry = FuncRef::new(unit, FuncId(index as u32));
        group.bench_function(op.name(), |b| {
            b.iter(|| {
                out.clear();
                vm.run_into(
                    entry,
                    black_box(&args),
                    &mut out,
                    TraceMask::NONE,
                    None,
                    None,
                )
                .unwrap();
                assert_eq!(out[0], Value::U64(ITERS * VALUE), "every case adds VALUE");
            });
        });
    }
    group.finish();
}

criterion_group!(benches, object_state);
criterion_main!(benches);
