// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use crate::aggregates::AggHeap;
use crate::host::ValueRef;
use crate::recording::{CaptureFault, CaptureResource, RecordLimits};
use crate::value::{AggHandle, AggType, Decimal, FuncId, Obj};
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::Range;

/// Identity of an aggregate in one recording, independent of VM heap numbering.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AggregateId(pub(crate) u32);

impl AggregateId {
    /// Returns the recording-local ordinal for inspection and references.
    pub fn get(self) -> u32 {
        self.0
    }
}

/// An owned recorded value. Aggregate references belong to its recording.
///
/// Byte/string contents are shared across equal payloads. Floating values retain IEEE bits,
/// including signed zero and NaN payloads. Object and function IDs retain their source domains.
#[derive(Clone, Debug)]
pub enum RecordedValue {
    /// Unit.
    Unit,
    /// Boolean.
    Bool(bool),
    /// Signed integer.
    I64(i64),
    /// Unsigned integer.
    U64(u64),
    /// Exact IEEE 754 bit pattern.
    F64(u64),
    /// Decimal with its original scale.
    Decimal(Decimal),
    /// Shared byte contents.
    Bytes(Arc<[u8]>),
    /// Shared UTF-8 contents.
    Str(Arc<str>),
    /// Opaque object token; external object state is not captured.
    Obj(Obj),
    /// Aggregate identity within this recording.
    Agg(AggregateId),
    /// Function in the recorded program.
    Func(FuncId),
    /// Function and captured environment.
    Closure {
        /// Closure body in the recorded program.
        func: FuncId,
        /// Environment in this recording.
        env: AggregateId,
    },
}

/// One aggregate and its immediate contents, without recursively expanding other nodes.
#[derive(Debug)]
pub struct Aggregate<'a> {
    /// Recording-local identity.
    pub id: AggregateId,
    /// Tuple/struct/array descriptor in the source program's type domain.
    pub kind: AggType,
    /// Immediate values in authored order.
    pub values: &'a [RecordedValue],
    /// Whether the node existed before guest execution began.
    pub initial: bool,
}

#[derive(Debug)]
pub(crate) struct Node {
    pub kind: Option<AggType>,
    pub fields: Range<usize>,
    pub initial: bool,
    pub created_offset: Option<u32>,
}

#[derive(Debug, Default)]
pub(crate) struct Arena {
    pub values: Vec<RecordedValue>,
    pub nodes: Vec<Node>,
    pub payload_bytes: usize,
}

impl Arena {
    pub(crate) fn slice(&self, range: &Range<usize>) -> &[RecordedValue] {
        &self.values[range.clone()]
    }
    pub(crate) fn aggregate(&self, id: AggregateId) -> Option<Aggregate<'_>> {
        let node = self.nodes.get(id.0 as usize)?;
        Some(Aggregate {
            id,
            kind: node.kind?,
            values: self.slice(&node.fields),
            initial: node.initial,
        })
    }
}

pub(crate) struct Builder {
    pub arena: Arena,
    pub limits: RecordLimits,
    pub initial_count: u32,
    handles: BTreeMap<u32, AggregateId>,
    strings: BTreeMap<Arc<str>, ()>,
    bytes: BTreeMap<Arc<[u8]>, ()>,
    pending: Vec<u32>,
}

impl Builder {
    pub(crate) fn new(limits: RecordLimits) -> Self {
        Self {
            arena: Arena::default(),
            limits,
            initial_count: 0,
            handles: BTreeMap::new(),
            strings: BTreeMap::new(),
            bytes: BTreeMap::new(),
            pending: Vec::new(),
        }
    }

    pub(crate) fn roots<'a>(
        &mut self,
        values: impl IntoIterator<Item = ValueRef<'a>>,
        heap: &AggHeap,
    ) -> Result<Range<usize>, CaptureFault> {
        let start = self.arena.values.len();
        for value in values {
            let value = self.value(value, heap)?;
            self.push(value)?;
        }
        let end = self.arena.values.len();
        let mut cursor = 0;
        while cursor < self.pending.len() {
            let handle = self.pending[cursor];
            cursor += 1;
            let id = self.handles[&handle];
            let handle = AggHandle(handle);
            let kind = heap
                .agg_type(handle)
                .map_err(|_| CaptureFault::InvalidAggregate { handle: handle.0 })?;
            let count = match kind {
                AggType::Tuple { .. } => heap.tuple_len(handle),
                AggType::Struct { .. } => heap.struct_field_count(handle),
                AggType::Array { .. } => heap.array_len(handle),
            }
            .map_err(|_| CaptureFault::InvalidAggregate { handle: handle.0 })?;
            let start = self.arena.values.len();
            for index in 0..count {
                let value = match kind {
                    AggType::Tuple { .. } => heap.tuple_get_ref(handle, index),
                    AggType::Struct { .. } => heap.struct_get_ref(handle, index),
                    AggType::Array { .. } => heap.array_get_ref(handle, index),
                }
                .map_err(|_| CaptureFault::InvalidAggregate { handle: handle.0 })?;
                let value = self.value(ValueRef::from_value(value), heap)?;
                self.push(value)?;
            }
            let node = &mut self.arena.nodes[id.0 as usize];
            node.kind = Some(kind);
            node.fields = start..self.arena.values.len();
        }
        self.pending.clear();
        Ok(start..end)
    }

    fn push(&mut self, value: RecordedValue) -> Result<(), CaptureFault> {
        if self.arena.values.len() >= self.limits.values {
            return Err(CaptureFault::Budget {
                resource: CaptureResource::Values,
                limit: self.limits.values,
            });
        }
        self.arena.values.push(value);
        Ok(())
    }

    pub(crate) fn charge(&mut self, count: usize) -> Result<(), CaptureFault> {
        let total = self
            .arena
            .payload_bytes
            .checked_add(count)
            .filter(|&total| total <= self.limits.payload_bytes)
            .ok_or(CaptureFault::Budget {
                resource: CaptureResource::PayloadBytes,
                limit: self.limits.payload_bytes,
            })?;
        self.arena.payload_bytes = total;
        Ok(())
    }

    fn value(
        &mut self,
        value: ValueRef<'_>,
        heap: &AggHeap,
    ) -> Result<RecordedValue, CaptureFault> {
        if self.arena.values.len() >= self.limits.values {
            return Err(CaptureFault::Budget {
                resource: CaptureResource::Values,
                limit: self.limits.values,
            });
        }
        Ok(match value {
            ValueRef::Unit => RecordedValue::Unit,
            ValueRef::Bool(value) => RecordedValue::Bool(value),
            ValueRef::I64(value) => RecordedValue::I64(value),
            ValueRef::U64(value) => RecordedValue::U64(value),
            ValueRef::F64(value) => RecordedValue::F64(value.to_bits()),
            ValueRef::Decimal(value) => RecordedValue::Decimal(value),
            ValueRef::Obj(value) => RecordedValue::Obj(value),
            ValueRef::Func(value) => RecordedValue::Func(value),
            ValueRef::Agg(handle) => RecordedValue::Agg(self.handle(handle, heap)?),
            ValueRef::Closure(value) => RecordedValue::Closure {
                func: value.func,
                env: self.handle(value.env, heap)?,
            },
            ValueRef::Str(value) => {
                let value = if let Some((key, ())) = self.strings.get_key_value(value) {
                    Arc::clone(key)
                } else {
                    self.charge(value.len())?;
                    let value: Arc<str> = Arc::from(value);
                    self.strings.insert(Arc::clone(&value), ());
                    value
                };
                RecordedValue::Str(value)
            }
            ValueRef::Bytes(value) => {
                let value = if let Some((key, ())) = self.bytes.get_key_value(value) {
                    Arc::clone(key)
                } else {
                    self.charge(value.len())?;
                    let value: Arc<[u8]> = Arc::from(value);
                    self.bytes.insert(Arc::clone(&value), ());
                    value
                };
                RecordedValue::Bytes(value)
            }
        })
    }

    fn handle(&mut self, handle: AggHandle, heap: &AggHeap) -> Result<AggregateId, CaptureFault> {
        if let Some(&id) = self.handles.get(&handle.0) {
            return Ok(id);
        }
        if self.arena.nodes.len() >= self.limits.aggregates {
            return Err(CaptureFault::Budget {
                resource: CaptureResource::Aggregates,
                limit: self.limits.aggregates,
            });
        }
        heap.agg_type(handle)
            .map_err(|_| CaptureFault::InvalidAggregate { handle: handle.0 })?;
        let id = AggregateId(u32::try_from(self.arena.nodes.len()).map_err(|_| {
            CaptureFault::Budget {
                resource: CaptureResource::Aggregates,
                limit: self.limits.aggregates,
            }
        })?);
        self.arena.nodes.push(Node {
            kind: None,
            fields: 0..0,
            initial: handle.0 < self.initial_count,
            created_offset: handle.0.checked_sub(self.initial_count),
        });
        self.handles.insert(handle.0, id);
        self.pending.push(handle.0);
        Ok(id)
    }

    pub(crate) fn finish(self) -> (Arena, Vec<AggregateId>) {
        let initial = self
            .handles
            .range(..self.initial_count)
            .map(|(_, &id)| id)
            .collect();
        (self.arena, initial)
    }
}
