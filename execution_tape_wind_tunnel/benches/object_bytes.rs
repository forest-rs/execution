// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! What a guest pays to scan a host object's bytes, such as a string's UTF-8.
//!
//! Each case runs a guest loop over a `LEN`-byte ASCII text that reads every byte and adds it
//! to an accumulator. Subtract `loop` and divide by `LEN` for a per-byte cost.
//!
//! - `loop` adds a constant instead and is the floor.
//! - `host_call` asks a host import `byte_at(obj, index) -> u64` that the host recognizes by its
//!   ID, the cheapest host call; `obj_bytes_get` reads the byte with `obj.bytes_get`
//!   ([`Host::object_bytes`]).
//! - `bytes_get` reads the same text passed as a tape `Bytes` value with `bytes.get`: the cost
//!   of keeping text in the tape's own values instead of host objects.

use criterion::{Criterion, criterion_group, criterion_main};
use execution_tape::asm::{FunctionSig, ProgramBuilder, Reg};
use execution_tape::host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef};
use execution_tape::program::{HostTypeId, ValueType};
use execution_tape::trace::TraceMask;
use execution_tape::value::{FuncId, FuncRef, Obj, ObjHandle, Value};
use execution_tape::verifier::VerifiedProgram;
use execution_tape::vm::{Limits, Vm};
use std::hint::black_box;

const LEN: usize = 1_000;
const TEXTS: HostTypeId = HostTypeId(0x5445_5854);

/// One text object, handle 0.
struct Texts {
    text: Vec<u8>,
}

impl Host for Texts {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        args: &[ValueRef<'_>],
        rets: &mut [Value],
        ctx: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        match (ctx.host_sig_id().0, args) {
            (0, [ValueRef::Obj(object), ValueRef::U64(index)]) => {
                let index = usize::try_from(*index).map_err(|_| HostError::Failed)?;
                let byte = (*object == text_object())
                    .then(|| self.text.get(index))
                    .flatten()
                    .ok_or(HostError::Failed)?;
                rets[0] = Value::U64(u64::from(*byte));
                Ok(0)
            }
            _ => Err(HostError::UnknownSymbol),
        }
    }

    fn object_bytes<R>(&self, object: Obj, read: impl FnOnce(Option<&[u8]>) -> R) -> R {
        read((object == text_object()).then_some(self.text.as_slice()))
    }
}

fn text_object() -> Obj {
    Obj {
        host_type: TEXTS,
        handle: ObjHandle(0),
    }
}

#[derive(Copy, Clone, Debug)]
enum Read {
    Loop,
    HostCall,
    ObjBytesGet,
    BytesGet,
}

const READS: [Read; 4] = [
    Read::Loop,
    Read::HostCall,
    Read::ObjBytesGet,
    Read::BytesGet,
];

impl Read {
    fn name(self) -> &'static str {
        match self {
            Self::Loop => "loop",
            Self::HostCall => "host_call",
            Self::ObjBytesGet => "obj_bytes_get",
            Self::BytesGet => "bytes_get",
        }
    }

    /// The type of the text the loop receives.
    fn receives(self) -> ValueType {
        match self {
            Self::BytesGet => ValueType::Bytes,
            Self::Loop | Self::HostCall | Self::ObjBytesGet => ValueType::Obj(TEXTS),
        }
    }
}

/// One `(len, text) -> u64` loop per [`Read`], in [`READS`] order.
fn program() -> VerifiedProgram {
    let mut b = ProgramBuilder::new();
    let byte_at = b.host_sig_for(
        "byte_at",
        HostSig {
            args: vec![ValueType::Obj(TEXTS), ValueType::U64],
            rets: vec![ValueType::U64],
        },
    );
    for read in READS {
        let function = b
            .declare_function(FunctionSig {
                arg_types: vec![ValueType::U64, read.receives()],
                ret_types: vec![ValueType::U64],
            })
            .unwrap();
        let mut a = b.function_asm(function).unwrap();
        let (len, text) = (Reg(1), Reg(2));
        let index = a.alloc_register(ValueType::U64);
        let one = a.alloc_register(ValueType::U64);
        let more = a.alloc_register(ValueType::Bool);
        let acc = a.alloc_register(ValueType::U64);
        let byte = a.alloc_register(ValueType::U64);
        let (head, body, exit) = (a.label(), a.label(), a.label());
        a.const_u64(index, 0)
            .const_u64(one, 1)
            .const_u64(acc, 0)
            .const_u64(byte, u64::from(b'a'))
            .jmp(head);
        a.place(head).unwrap();
        a.u64_lt(more, index, len).br(more, body, exit);
        a.place(body).unwrap();
        match read {
            Read::Loop => {}
            Read::HostCall => {
                a.host_call_sig_id(Reg::EFFECT, byte_at, Reg::EFFECT, &[text, index], &[byte]);
            }
            Read::ObjBytesGet => {
                a.obj_bytes_get(byte, text, index);
            }
            Read::BytesGet => {
                a.bytes_get(byte, text, index);
            }
        }
        a.u64_add(acc, acc, byte)
            .u64_add(index, index, one)
            .jmp(head);
        a.place(exit).unwrap();
        a.ret(Reg::EFFECT, &[acc]);
        b.define_function(function, a).unwrap();
    }
    b.build_verified().unwrap()
}

fn object_bytes(c: &mut Criterion) {
    let text = vec![b'a'; LEN];
    let limits = Limits {
        fuel: u64::MAX,
        ..Limits::default()
    };
    let mut vm = Vm::new(Texts { text: text.clone() }, limits);
    let unit = vm.install(program()).unwrap();
    let expected = Value::U64(LEN as u64 * u64::from(b'a'));
    let mut group = c.benchmark_group("object_bytes");
    let mut out = Vec::new();
    for (index, read) in READS.iter().enumerate() {
        let entry = FuncRef::new(unit, FuncId(index as u32));
        let receiver = match read {
            Read::BytesGet => Value::Bytes(text.clone()),
            _ => Value::Obj(text_object()),
        };
        let args = [Value::U64(LEN as u64), receiver];
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
                assert_eq!(out[0], expected, "every read is an `a`");
            });
        });
    }
    group.finish();
}

criterion_group!(benches, object_bytes);
criterion_main!(benches);
