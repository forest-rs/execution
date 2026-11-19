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
//!
//! The `utf8_scan` group decodes `SCALARS` scalars of a text instead, and sums them; subtract
//! `loop` and divide by `SCALARS` for a per-scalar cost.
//!
//! - `loop` steps a position without reading and is the floor.
//! - `host_call` asks a host import `next_scalar(obj, pos) -> (u32, u64)` for the scalar and the
//!   position after it, a read-and-advance host call.
//! - `next_1`..`next_4` decode with `obj.utf8_next` texts whose scalars are 1 to 4 bytes long,
//!   ending the loop on its `missing` flag; `prev_1` scans the ASCII text backward with
//!   `obj.utf8_prev`.

use criterion::{Criterion, criterion_group, criterion_main};
use execution_tape::asm::{FunctionSig, ProgramBuilder, Reg};
use execution_tape::host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef};
use execution_tape::program::{HostTypeId, ValueType};
use execution_tape::trace::TraceMask;
use execution_tape::value::{FuncId, FuncRef, Obj, ObjHandle, Value};
use execution_tape::verifier::VerifiedProgram;
use execution_tape::vm::{Limits, Vm};
use std::hint::black_box;
use std::sync::Arc;

const LEN: usize = 1_000;
const SCALARS: usize = 1_000;
const TEXTS: HostTypeId = HostTypeId(0x5445_5854);

/// One text object, handle 0.
struct Texts {
    text: String,
}

/// One scalar of each UTF-8 length, by length.
const WIDTHS: [char; 4] = ['a', '\u{e9}', '\u{20ac}', '\u{1f600}'];

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
            (1, [ValueRef::Obj(object), ValueRef::U64(pos)]) => {
                let pos = usize::try_from(*pos).map_err(|_| HostError::Failed)?;
                let c = (*object == text_object())
                    .then(|| self.text.get(pos..))
                    .flatten()
                    .and_then(|rest| rest.chars().next())
                    .ok_or(HostError::Failed)?;
                rets[0] = Value::U32(u32::from(c));
                rets[1] = Value::U64((pos + c.len_utf8()) as u64);
                Ok(0)
            }
            (0, [ValueRef::Obj(object), ValueRef::U64(index)]) => {
                let index = usize::try_from(*index).map_err(|_| HostError::Failed)?;
                let byte = (*object == text_object())
                    .then(|| self.text.as_bytes().get(index))
                    .flatten()
                    .ok_or(HostError::Failed)?;
                rets[0] = Value::U64(u64::from(*byte));
                Ok(0)
            }
            _ => Err(HostError::UnknownSymbol),
        }
    }

    fn object_bytes<R>(&self, object: Obj, read: impl FnOnce(Option<&[u8]>) -> R) -> R {
        read((object == text_object()).then_some(self.text.as_bytes()))
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
    let mut vm = Vm::new(
        Texts {
            text: String::from_utf8(text.clone()).unwrap(),
        },
        limits,
    );
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

#[derive(Copy, Clone, Debug)]
enum Scan {
    Loop,
    HostCall,
    Next,
    Prev,
}

/// `(len, text) -> u32` loops: the sum of the text's scalars, in [`Scan`] order.
fn scan_program() -> VerifiedProgram {
    let text = ValueType::Obj(TEXTS);
    let mut b = ProgramBuilder::new();
    // Host signature 0 is unused here; keep `next_scalar` at 1 as the host expects.
    let _ = b.host_sig_for(
        "byte_at",
        HostSig {
            args: vec![text, ValueType::U64],
            rets: vec![ValueType::U64],
        },
    );
    let next_scalar = b.host_sig_for(
        "next_scalar",
        HostSig {
            args: vec![text, ValueType::U64],
            rets: vec![ValueType::U32, ValueType::U64],
        },
    );
    for scan in [Scan::Loop, Scan::HostCall, Scan::Next, Scan::Prev] {
        let function = b
            .declare_function(FunctionSig {
                arg_types: vec![ValueType::U64, text],
                ret_types: vec![ValueType::U32],
            })
            .unwrap();
        let mut a = b.function_asm(function).unwrap();
        let (len, text) = (Reg(1), Reg(2));
        let pos = a.alloc_register(ValueType::U64);
        let one = a.alloc_register(ValueType::U64);
        let more = a.alloc_register(ValueType::Bool);
        let missing = a.alloc_register(ValueType::Bool);
        let scalar = a.alloc_register(ValueType::U32);
        let acc = a.alloc_register(ValueType::U32);
        let (head, body, exit) = (a.label(), a.label(), a.label());
        match scan {
            Scan::Prev => a.mov(pos, len),
            _ => a.const_u64(pos, 0),
        };
        a.const_u64(one, 1)
            .const_u32(acc, 0)
            .const_u32(scalar, u32::from('a'))
            .jmp(head);
        a.place(head).unwrap();
        match scan {
            Scan::Loop => {
                a.u64_lt(more, pos, len).br(more, body, exit);
                a.place(body).unwrap();
                a.u64_add(pos, pos, one);
            }
            Scan::HostCall => {
                a.u64_lt(more, pos, len).br(more, body, exit);
                a.place(body).unwrap();
                a.host_call_sig_id(
                    Reg::EFFECT,
                    next_scalar,
                    Reg::EFFECT,
                    &[text, pos],
                    &[scalar, pos],
                );
            }
            Scan::Next => {
                a.obj_utf8_next(scalar, pos, missing, text, pos)
                    .br(missing, exit, body);
                a.place(body).unwrap();
            }
            Scan::Prev => {
                a.obj_utf8_prev(scalar, pos, missing, text, pos)
                    .br(missing, exit, body);
                a.place(body).unwrap();
            }
        }
        a.u32_add(acc, acc, scalar).jmp(head);
        a.place(exit).unwrap();
        a.ret(Reg::EFFECT, &[acc]);
        b.define_function(function, a).unwrap();
    }
    b.build_verified().unwrap()
}

fn utf8_scan(c: &mut Criterion) {
    let limits = Limits {
        fuel: u64::MAX,
        ..Limits::default()
    };
    let program = Arc::new(scan_program());
    let mut group = c.benchmark_group("utf8_scan");
    let mut out = Vec::new();
    // (name, function, scalar width)
    let cases = [
        ("loop", 0, 1),
        ("host_call", 1, 1),
        ("next_1", 2, 1),
        ("next_2", 2, 2),
        ("next_3", 2, 3),
        ("next_4", 2, 4),
        ("prev_1", 3, 1),
    ];
    for (name, function, width) in cases {
        let scalar = WIDTHS[width - 1];
        let text: String = core::iter::repeat_n(scalar, SCALARS).collect();
        let expected = if name == "loop" {
            u32::from('a').wrapping_mul(SCALARS as u32)
        } else {
            u32::from(scalar).wrapping_mul(SCALARS as u32)
        };
        let mut vm = Vm::new(Texts { text: text.clone() }, limits.clone());
        let unit = vm.install(Arc::clone(&program)).unwrap();
        let entry = FuncRef::new(unit, FuncId(function));
        let args = [Value::U64(text.len() as u64), Value::Obj(text_object())];
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
                assert_eq!(out[0], Value::U32(expected), "every scalar is read");
            });
        });
    }
    group.finish();
}

criterion_group!(benches, object_bytes, utf8_scan);
criterion_main!(benches);
