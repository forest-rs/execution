// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! What a dynamic language pays to learn a host object's class.
//!
//! Each case runs a guest loop of `ITERS` iterations that read the key of a host object and add
//! it to an accumulator. `loop` adds a constant instead and is the floor; `host_call` asks a host
//! import `class_of(obj) -> u64` that the host recognizes by its ID, the cheapest host call;
//! `obj_key` reads the key with `obj.key` ([`Host::object_key`]). Subtract `loop` and divide by
//! `ITERS` for a per-read cost.

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
const CLASS: u64 = 7;

/// Objects of one class, which both the import and `obj.key` report.
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
            _ => Err(HostError::UnknownSymbol),
        }
    }

    fn object_key(&self, object: Obj) -> Option<u64> {
        (object.host_type == OBJECTS).then_some(CLASS)
    }
}

#[derive(Copy, Clone, Debug)]
enum Read {
    Loop,
    HostCall,
    ObjKey,
}

/// One `(iters, obj) -> u64` loop per way of reading the key, in [`Read`] order.
fn program() -> VerifiedProgram {
    let object = ValueType::Obj(OBJECTS);
    let mut b = ProgramBuilder::new();
    let class_of = b.host_sig_for(
        "class_of",
        HostSig {
            args: vec![object],
            rets: vec![ValueType::U64],
        },
    );
    for read in [Read::Loop, Read::HostCall, Read::ObjKey] {
        let function = b
            .declare_function(FunctionSig {
                arg_types: vec![ValueType::U64, object],
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
            Read::ObjKey => {
                a.obj_key(key, receiver);
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
    let args = [
        Value::U64(ITERS),
        Value::Obj(Obj {
            host_type: OBJECTS,
            handle: ObjHandle(1),
        }),
    ];
    let mut group = c.benchmark_group("object_dispatch");
    let mut out = Vec::new();
    for (index, read) in [Read::Loop, Read::HostCall, Read::ObjKey]
        .iter()
        .enumerate()
    {
        let entry = FuncRef::new(unit, FuncId(index as u32));
        let name = match read {
            Read::Loop => "loop",
            Read::HostCall => "host_call",
            Read::ObjKey => "obj_key",
        };
        group.bench_function(name, |b| {
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
