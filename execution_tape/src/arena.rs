// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Per-VM arena storage for alloc-backed runtime values.
//!
//! Register handles refer to owned computed payloads or immutable sources in the active run.
//! Source indices carry no borrowed pointers and are resolved only against that run's inputs,
//! program, and VM heap. Reset discards all indices before another run can use the context.

use alloc::string::String;
use alloc::vec::Vec;

use crate::aggregates::AggHeap;
use crate::program::{ConstId, Program};
use crate::value::{AggHandle, Value};

/// Immutable payload source within one active run.
#[derive(Copy, Clone, Debug)]
pub(crate) enum PayloadSource {
    Constant(ConstId),
    Argument(usize),
    Aggregate { handle: AggHandle, index: usize },
}

/// The owners against which per-run source indices are resolved.
#[derive(Copy, Clone, Debug)]
pub(crate) struct ValueSources<'a> {
    program: &'a Program,
    args: &'a [Value],
    aggregates: &'a AggHeap,
}

impl<'a> ValueSources<'a> {
    pub(crate) fn new(program: &'a Program, args: &'a [Value], aggregates: &'a AggHeap) -> Self {
        Self {
            program,
            args,
            aggregates,
        }
    }

    fn value(self, source: PayloadSource) -> Option<&'a Value> {
        match source {
            PayloadSource::Argument(index) => self.args.get(index),
            PayloadSource::Aggregate { handle, index } => {
                self.aggregates.value_ref(handle, index).ok()
            }
            PayloadSource::Constant(_) => None,
        }
    }

    fn bytes(self, source: PayloadSource) -> Option<&'a [u8]> {
        if let PayloadSource::Constant(id) = source {
            return self.program.const_bytes(id).ok();
        }
        match self.value(source)? {
            Value::Bytes(bytes) => Some(bytes),
            _ => None,
        }
    }

    fn str(self, source: PayloadSource) -> Option<&'a str> {
        if let PayloadSource::Constant(id) = source {
            return self.program.const_str(id).ok();
        }
        match self.value(source)? {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
enum Stored<T> {
    Owned(T),
    Source(PayloadSource),
}

/// Handle to a byte string stored in a [`ValueArena`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct BytesHandle(pub(crate) u32);

/// Handle to a UTF-8 string stored in a [`ValueArena`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct StrHandle(pub(crate) u32);

/// Arena storage for bytes and strings.
#[derive(Clone, Debug, Default)]
pub(crate) struct ValueArena {
    bytes: Vec<Stored<Vec<u8>>>,
    strs: Vec<Stored<String>>,
}

impl ValueArena {
    pub(crate) fn clear(&mut self) {
        self.bytes.clear();
        self.strs.clear();
    }

    pub(crate) fn alloc_bytes(&mut self, bytes: Vec<u8>) -> BytesHandle {
        let idx = u32::try_from(self.bytes.len()).unwrap_or(u32::MAX);
        self.bytes.push(Stored::Owned(bytes));
        BytesHandle(idx)
    }

    pub(crate) fn borrow_bytes(&mut self, source: PayloadSource) -> BytesHandle {
        let idx = u32::try_from(self.bytes.len()).unwrap_or(u32::MAX);
        self.bytes.push(Stored::Source(source));
        BytesHandle(idx)
    }

    pub(crate) fn alloc_str(&mut self, s: String) -> StrHandle {
        let idx = u32::try_from(self.strs.len()).unwrap_or(u32::MAX);
        self.strs.push(Stored::Owned(s));
        StrHandle(idx)
    }

    pub(crate) fn borrow_str(&mut self, source: PayloadSource) -> StrHandle {
        let idx = u32::try_from(self.strs.len()).unwrap_or(u32::MAX);
        self.strs.push(Stored::Source(source));
        StrHandle(idx)
    }

    pub(crate) fn bytes<'a>(
        &'a self,
        h: BytesHandle,
        sources: ValueSources<'a>,
    ) -> Option<&'a [u8]> {
        match self.bytes.get(h.0 as usize)? {
            Stored::Owned(bytes) => Some(bytes),
            Stored::Source(source) => sources.bytes(*source),
        }
    }

    pub(crate) fn str<'a>(&'a self, h: StrHandle, sources: ValueSources<'a>) -> Option<&'a str> {
        match self.strs.get(h.0 as usize)? {
            Stored::Owned(s) => Some(s),
            Stored::Source(source) => sources.str(*source),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asm::ProgramBuilder;
    use alloc::vec;

    #[test]
    fn borrowed_payloads_resolve_to_their_original_owners() {
        let mut pb = ProgramBuilder::new();
        let bytes = pb.const_bytes(b"constant");
        let text = pb.const_str("constant");
        let p = pb.build();
        let args = [Value::Bytes(vec![1, 2]), Value::Str("input".into())];
        let mut heap = AggHeap::new();
        let tuple = heap.tuple_new(vec![Value::Bytes(vec![3, 4]), Value::Str("field".into())]);
        let sources = ValueSources::new(&p, &args, &heap);
        let mut arena = ValueArena::default();
        let cb = arena.borrow_bytes(PayloadSource::Constant(bytes));
        let cs = arena.borrow_str(PayloadSource::Constant(text));
        assert_eq!(
            arena.bytes(cb, sources).unwrap().as_ptr(),
            p.const_bytes(bytes).unwrap().as_ptr()
        );
        assert_eq!(
            arena.str(cs, sources).unwrap().as_ptr(),
            p.const_str(text).unwrap().as_ptr()
        );
        for (source, value) in [
            (PayloadSource::Argument(0), &args[0]),
            (
                PayloadSource::Aggregate {
                    handle: tuple,
                    index: 0,
                },
                heap.tuple_get_ref(tuple, 0).unwrap(),
            ),
        ] {
            let Value::Bytes(expected) = value else {
                unreachable!()
            };
            let handle = arena.borrow_bytes(source);
            assert_eq!(
                arena.bytes(handle, sources).unwrap().as_ptr(),
                expected.as_ptr()
            );
        }
        for (source, value) in [
            (PayloadSource::Argument(1), &args[1]),
            (
                PayloadSource::Aggregate {
                    handle: tuple,
                    index: 1,
                },
                heap.tuple_get_ref(tuple, 1).unwrap(),
            ),
        ] {
            let Value::Str(expected) = value else {
                unreachable!()
            };
            let handle = arena.borrow_str(source);
            assert_eq!(
                arena.str(handle, sources).unwrap().as_ptr(),
                expected.as_ptr()
            );
        }
        let missing = arena.borrow_bytes(PayloadSource::Argument(9));
        assert_eq!(arena.bytes(missing, sources), None);
        let wrong_type = arena.borrow_bytes(PayloadSource::Argument(1));
        assert_eq!(arena.bytes(wrong_type, sources), None);
    }

    #[test]
    fn computed_payloads_move_into_the_arena_and_reset_releases_sources() {
        let p = ProgramBuilder::new().build();
        let heap = AggHeap::new();
        let sources = ValueSources::new(&p, &[], &heap);
        let mut arena = ValueArena::default();
        let bytes = vec![1, 2, 3];
        let bytes_ptr = bytes.as_ptr();
        let text = String::from("owned");
        let text_ptr = text.as_ptr();
        let b = arena.alloc_bytes(bytes);
        let s = arena.alloc_str(text);
        assert_eq!(arena.bytes(b, sources).unwrap().as_ptr(), bytes_ptr);
        assert_eq!(arena.str(s, sources).unwrap().as_ptr(), text_ptr);
        arena.clear();
        assert_eq!(arena.bytes(b, sources), None);
        assert_eq!(arena.str(s, sources), None);
    }
}
