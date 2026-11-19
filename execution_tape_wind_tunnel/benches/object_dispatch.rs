// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! What a dynamic language pays to learn a host object's class and change its representation.
//!
//! Each case runs a guest loop of `ITERS` iterations that each read the key of a host object
//! and add it to an accumulator. Subtract `loop` and divide by `ITERS` for a per-iteration cost.
//!
//! - `loop` adds a constant instead and is the floor.
//! - `host_call` asks a host import `class_of(obj) -> u64` that the host recognizes by its ID,
//!   the cheapest host call; `obj_key` reads the key with `obj.key` ([`Host::object_key`]).
//! - The rest receive the object boxed, as a dynamic language passes an untyped value, and unbox
//!   it before reading its key, as a method that takes the unboxed object would. `unbox_host_call`
//!   unboxes with a host import, `unbox_obj_convert` with `obj.convert`
//!   ([`Host::convert_object`]), and `boxed_key` reads the key of the boxed value directly: the
//!   floor for any scheme in which methods accept the boxed form.

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
const BOXED: HostTypeId = HostTypeId(0x424f_5845);
/// Set in a boxed object's handle, as a NaN-boxing host tags it.
const BOX_TAG: u64 = 0xFFFF_0000_0000_0000;
const CLASS: u64 = 7;

/// Objects of one class, and the same objects boxed.
struct Classes;

impl Host for Classes {
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
                rets[0] = Value::U64(self.object_key(*object).ok_or(HostError::Failed)?);
                Ok(0)
            }
            (1, [ValueRef::Obj(boxed)]) => {
                rets[0] = Value::Obj(
                    self.convert_object(*boxed, OBJECTS)
                        .ok_or(HostError::Failed)?,
                );
                Ok(0)
            }
            _ => Err(HostError::UnknownSymbol),
        }
    }

    fn object_key(&self, object: Obj) -> Option<u64> {
        (object.host_type == OBJECTS || object.host_type == BOXED).then_some(CLASS)
    }

    fn convert_object(&self, object: Obj, to: HostTypeId) -> Option<Obj> {
        let handle = match (object.host_type, to) {
            (BOXED, OBJECTS) => object.handle.0 & !BOX_TAG,
            (OBJECTS, BOXED) => object.handle.0 | BOX_TAG,
            _ => return None,
        };
        Some(Obj {
            host_type: to,
            handle: ObjHandle(handle),
        })
    }
}

#[derive(Copy, Clone, Debug)]
enum Read {
    Loop,
    HostCall,
    ObjKey,
    UnboxHostCall,
    UnboxObjConvert,
    BoxedKey,
}

const READS: [Read; 6] = [
    Read::Loop,
    Read::HostCall,
    Read::ObjKey,
    Read::UnboxHostCall,
    Read::UnboxObjConvert,
    Read::BoxedKey,
];

impl Read {
    fn name(self) -> &'static str {
        match self {
            Self::Loop => "loop",
            Self::HostCall => "host_call",
            Self::ObjKey => "obj_key",
            Self::UnboxHostCall => "unbox_host_call",
            Self::UnboxObjConvert => "unbox_obj_convert",
            Self::BoxedKey => "boxed_key",
        }
    }

    /// The host type of the object the loop receives.
    fn receives(self) -> HostTypeId {
        match self {
            Self::Loop | Self::HostCall | Self::ObjKey => OBJECTS,
            Self::UnboxHostCall | Self::UnboxObjConvert | Self::BoxedKey => BOXED,
        }
    }
}

/// One `(iters, obj) -> u64` loop per [`Read`], in [`READS`] order.
fn program() -> VerifiedProgram {
    let object = ValueType::Obj(OBJECTS);
    let boxed = ValueType::Obj(BOXED);
    let mut b = ProgramBuilder::new();
    let class_of = b.host_sig_for(
        "class_of",
        HostSig {
            args: vec![object],
            rets: vec![ValueType::U64],
        },
    );
    let unbox = b.host_sig_for(
        "unbox",
        HostSig {
            args: vec![boxed],
            rets: vec![object],
        },
    );
    for read in READS {
        let function = b
            .declare_function(FunctionSig {
                arg_types: vec![ValueType::U64, ValueType::Obj(read.receives())],
                ret_types: vec![ValueType::U64],
            })
            .unwrap();
        let mut a = b.function_asm(function).unwrap();
        let (limit, receiver) = (Reg(1), Reg(2));
        let counter = a.alloc_register(ValueType::U64);
        let one = a.alloc_register(ValueType::U64);
        let done = a.alloc_register(ValueType::Bool);
        let acc = a.alloc_register(ValueType::U64);
        let key = a.alloc_register(ValueType::U64);
        let unboxed = a.alloc_register(object);
        let (head, body, exit) = (a.label(), a.label(), a.label());
        a.const_u64(counter, 0)
            .const_u64(one, 1)
            .const_u64(acc, 0)
            .const_u64(key, CLASS)
            .jmp(head);
        a.place(head).unwrap();
        a.u64_eq(done, counter, limit).br(done, exit, body);
        a.place(body).unwrap();
        match read {
            Read::Loop => {}
            Read::HostCall => {
                a.host_call_sig_id(Reg::EFFECT, class_of, Reg::EFFECT, &[receiver], &[key]);
            }
            Read::ObjKey | Read::BoxedKey => {
                a.obj_key(key, receiver);
            }
            Read::UnboxHostCall => {
                a.host_call_sig_id(Reg::EFFECT, unbox, Reg::EFFECT, &[receiver], &[unboxed])
                    .obj_key(key, unboxed);
            }
            Read::UnboxObjConvert => {
                a.obj_convert(unboxed, receiver).obj_key(key, unboxed);
            }
        }
        a.u64_add(acc, acc, key)
            .u64_add(counter, counter, one)
            .jmp(head);
        a.place(exit).unwrap();
        a.ret(Reg::EFFECT, &[acc]);
        b.define_function(function, a).unwrap();
    }
    b.build_verified().unwrap()
}

fn object_dispatch(c: &mut Criterion) {
    let limits = Limits {
        fuel: u64::MAX,
        ..Limits::default()
    };
    let mut vm = Vm::new(Classes, limits);
    let unit = vm.install(program()).unwrap();
    let mut group = c.benchmark_group("object_dispatch");
    let mut out = Vec::new();
    for (index, read) in READS.iter().enumerate() {
        let entry = FuncRef::new(unit, FuncId(index as u32));
        let receiver = Obj {
            host_type: OBJECTS,
            handle: ObjHandle(1),
        };
        let receiver = if read.receives() == BOXED {
            Classes.convert_object(receiver, BOXED).unwrap()
        } else {
            receiver
        };
        let args = [Value::U64(ITERS), Value::Obj(receiver)];
        group.bench_function(read.name(), |b| {
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
                assert_eq!(out[0], Value::U64(ITERS * CLASS), "every read is the class");
            });
        });
    }
    group.finish();
}

criterion_group!(benches, object_dispatch);
criterion_main!(benches);
