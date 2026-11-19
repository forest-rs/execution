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
//! - The `send_` cases send a message to the object and add the method's result, the class.
//!   `send_link` calls the method through a plain link, as a send the compiler proved does, and
//!   is the floor for sends. `send_host_dispatch` asks a host import for the method and calls it
//!   with `call.indirect`, as a dynamic send without a cache does. `send_open_guard` reads the
//!   key and compares it with `u64.eq` and `br` before the plain link, a guard spelled out in
//!   instructions; `send_guarded` reads the key and calls a guarded link that holds it, the
//!   inline cache; `send_megamorphic` calls a guarded link that calls its target for any key.

use criterion::{Criterion, criterion_group, criterion_main};
use execution_tape::asm::{FunctionSig, ProgramBuilder, Reg};
use execution_tape::host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef};
use execution_tape::program::{HostTypeId, LinkId, ValueType};
use execution_tape::trace::TraceMask;
use execution_tape::value::{FuncId, FuncRef, Obj, ObjHandle, UnitId, Value};
use execution_tape::verifier::VerifiedProgram;
use execution_tape::vm::{GuardKeys, Limits, LinkGuard, Vm};
use std::hint::black_box;

const ITERS: u64 = 1_000;
const OBJECTS: HostTypeId = HostTypeId(0x4f42_4a53);
const BOXED: HostTypeId = HostTypeId(0x424f_5845);
/// Set in a boxed object's handle, as a NaN-boxing host tags it.
const BOX_TAG: u64 = 0xFFFF_0000_0000_0000;
const CLASS: u64 = 7;

/// Objects of one class, and the same objects boxed; `dispatch` selects the method of
/// `unit`.
struct Classes {
    unit: Option<UnitId>,
}

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
            (2, [ValueRef::Obj(_)]) => {
                rets[0] = Value::Func(FuncRef::new(self.unit.ok_or(HostError::Failed)?, METHOD));
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
    SendLink,
    SendHostDispatch,
    SendOpenGuard,
    SendGuarded,
    SendMegamorphic,
}

const READS: [Read; 11] = [
    Read::Loop,
    Read::HostCall,
    Read::ObjKey,
    Read::UnboxHostCall,
    Read::UnboxObjConvert,
    Read::BoxedKey,
    Read::SendLink,
    Read::SendHostDispatch,
    Read::SendOpenGuard,
    Read::SendGuarded,
    Read::SendMegamorphic,
];

/// The method every send reaches: `method(obj) -> u64` returns the class.
const METHOD: FuncId = FuncId(0);
/// The guarded links' fallback, which calls the method.
const FALLBACK: FuncId = FuncId(1);
/// The first loop; the others follow in [`READS`] order.
const FIRST_LOOP: u32 = 2;
const PLAIN: LinkId = LinkId(0);
const GUARDED: LinkId = LinkId(1);
const MEGAMORPHIC: LinkId = LinkId(2);

impl Read {
    fn name(self) -> &'static str {
        match self {
            Self::Loop => "loop",
            Self::HostCall => "host_call",
            Self::ObjKey => "obj_key",
            Self::UnboxHostCall => "unbox_host_call",
            Self::UnboxObjConvert => "unbox_obj_convert",
            Self::BoxedKey => "boxed_key",
            Self::SendLink => "send_link",
            Self::SendHostDispatch => "send_host_dispatch",
            Self::SendOpenGuard => "send_open_guard",
            Self::SendGuarded => "send_guarded",
            Self::SendMegamorphic => "send_megamorphic",
        }
    }

    /// The host type of the object the loop receives.
    fn receives(self) -> HostTypeId {
        match self {
            Self::UnboxHostCall | Self::UnboxObjConvert | Self::BoxedKey => BOXED,
            _ => OBJECTS,
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
    let dispatch = b.host_sig_for(
        "dispatch",
        HostSig {
            args: vec![object],
            rets: vec![ValueType::Func],
        },
    );
    let method_sig = FunctionSig {
        arg_types: vec![object],
        ret_types: vec![ValueType::U64],
    };
    let method = b.declare_function(method_sig.clone()).unwrap();
    assert_eq!(method, METHOD);
    let mut a = b.function_asm(method).unwrap();
    let class = a.alloc_register(ValueType::U64);
    a.const_u64(class, CLASS).ret(Reg::EFFECT, &[class]);
    b.define_function(method, a).unwrap();
    let fallback = b
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::U64, ValueType::U64, object],
            ret_types: vec![ValueType::U64],
        })
        .unwrap();
    assert_eq!(fallback, FALLBACK);
    let mut a = b.function_asm(fallback).unwrap();
    let class = a.alloc_register(ValueType::U64);
    a.call(Reg::EFFECT, METHOD, Reg::EFFECT, &[Reg(3)], &[class])
        .ret(Reg::EFFECT, &[class]);
    b.define_function(fallback, a).unwrap();
    assert_eq!(b.declare_link("plain", METHOD).unwrap(), PLAIN);
    assert_eq!(
        b.declare_guarded_link("guarded", METHOD, 1, FALLBACK)
            .unwrap(),
        GUARDED
    );
    assert_eq!(
        b.declare_guarded_link("megamorphic", METHOD, 1, FALLBACK)
            .unwrap(),
        MEGAMORPHIC
    );
    let method_sig = b.call_sig(method_sig).unwrap();
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
        let expected = a.alloc_register(ValueType::U64);
        let hit = a.alloc_register(ValueType::Bool);
        let callee = a.alloc_register(ValueType::Func);
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
            Read::SendLink => {
                a.call_link(Reg::EFFECT, PLAIN, Reg::EFFECT, &[receiver], &[key]);
            }
            Read::SendHostDispatch => {
                a.host_call_sig_id(Reg::EFFECT, dispatch, Reg::EFFECT, &[receiver], &[callee])
                    .call_indirect(
                        Reg::EFFECT,
                        method_sig,
                        callee,
                        Reg::EFFECT,
                        &[receiver],
                        &[key],
                    );
            }
            Read::SendOpenGuard => {
                let (cached, missed, join) = (a.label(), a.label(), a.label());
                a.obj_key(key, receiver)
                    .const_u64(expected, CLASS)
                    .u64_eq(hit, key, expected)
                    .br(hit, cached, missed);
                a.place(cached).unwrap();
                a.call_link(Reg::EFFECT, PLAIN, Reg::EFFECT, &[receiver], &[key])
                    .jmp(join);
                a.place(missed).unwrap();
                a.call(Reg::EFFECT, METHOD, Reg::EFFECT, &[receiver], &[key])
                    .jmp(join);
                a.place(join).unwrap();
            }
            Read::SendGuarded | Read::SendMegamorphic => {
                let link = if matches!(read, Read::SendGuarded) {
                    GUARDED
                } else {
                    MEGAMORPHIC
                };
                a.obj_key(key, receiver).call_link_guard(
                    Reg::EFFECT,
                    link,
                    Reg::EFFECT,
                    &[key],
                    &[receiver],
                    &[key],
                );
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
    let mut vm = Vm::new(Classes { unit: None }, limits);
    let unit = vm.install(program()).unwrap();
    vm.host_mut().unit = Some(unit);
    let method = FuncRef::new(unit, METHOD);
    let keys = GuardKeys::new(&[CLASS]).unwrap();
    vm.relink_guarded(unit, GUARDED, LinkGuard::Keys(keys), method)
        .unwrap();
    vm.relink_guarded(unit, MEGAMORPHIC, LinkGuard::Megamorphic, method)
        .unwrap();
    let mut group = c.benchmark_group("object_dispatch");
    let mut out = Vec::new();
    for (index, read) in READS.iter().enumerate() {
        let entry = FuncRef::new(unit, FuncId(FIRST_LOOP + index as u32));
        let receiver = Obj {
            host_type: OBJECTS,
            handle: ObjHandle(1),
        };
        let receiver = if read.receives() == BOXED {
            vm.host().convert_object(receiver, BOXED).unwrap()
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
