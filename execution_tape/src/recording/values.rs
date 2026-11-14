// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use crate::aggregates::AggHeap;
use crate::host::ValueRef;
use crate::recording::{CaptureFault, CaptureResource, RecordLimits};
use crate::value::{AggHandle, AggType, Decimal, FuncRef, Obj};
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;
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
///
/// `Debug` shows a float's value with its bits, as in `F32(0.1 [0x3dcccccd])`.
#[derive(Clone)]
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
    /// Signed 32-bit integer.
    I32(i32),
    /// Unsigned 32-bit integer.
    U32(u32),
    /// Exact IEEE 754 single-precision bit pattern.
    F32(u32),
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
    /// Function of a recorded unit.
    Func(FuncRef),
    /// Function and captured environment.
    Closure {
        /// Closure body and its unit.
        func: FuncRef,
        /// Environment in this recording.
        env: AggregateId,
    },
}

impl fmt::Debug for RecordedValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unit => f.write_str("Unit"),
            Self::Bool(value) => f.debug_tuple("Bool").field(value).finish(),
            Self::I64(value) => f.debug_tuple("I64").field(value).finish(),
            Self::U64(value) => f.debug_tuple("U64").field(value).finish(),
            Self::F64(bits) => f.debug_tuple("F64").field(&F64Bits(*bits)).finish(),
            Self::I32(value) => f.debug_tuple("I32").field(value).finish(),
            Self::U32(value) => f.debug_tuple("U32").field(value).finish(),
            Self::F32(bits) => f.debug_tuple("F32").field(&F32Bits(*bits)).finish(),
            Self::Decimal(value) => f.debug_tuple("Decimal").field(value).finish(),
            Self::Bytes(value) => f.debug_tuple("Bytes").field(value).finish(),
            Self::Str(value) => f.debug_tuple("Str").field(value).finish(),
            Self::Obj(value) => f.debug_tuple("Obj").field(value).finish(),
            Self::Agg(value) => f.debug_tuple("Agg").field(value).finish(),
            Self::Func(value) => f.debug_tuple("Func").field(value).finish(),
            Self::Closure { func, env } => f
                .debug_struct("Closure")
                .field("func", func)
                .field("env", env)
                .finish(),
        }
    }
}

/// `f32` bits shown as `value [0xbits]`.
struct F32Bits(u32);

impl fmt::Debug for F32Bits {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} [{:#010x}]", f32::from_bits(self.0), self.0)
    }
}

/// `f64` bits shown as `value [0xbits]`.
struct F64Bits(u64);

impl fmt::Debug for F64Bits {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} [{:#018x}]", f64::from_bits(self.0), self.0)
    }
}

/// One aggregate and its immediate contents, without recursively expanding other nodes.
#[derive(Debug)]
pub struct Aggregate<'a> {
    /// Recording-local identity.
    pub id: AggregateId,
    /// Tuple/struct/array descriptor. Struct and array shapes are this recording's own;
    /// resolve them with [`super::Recording::shape`].
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
    /// Shapes of recorded struct/array nodes, independent of the recorded VM heap.
    pub shapes: crate::aggregates::ShapeTable,
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
            let invalid = || CaptureFault::InvalidAggregate { handle: handle.0 };
            let kind = heap.agg_type(handle).map_err(|_| invalid())?;
            // Record the structural shape, so the recording keeps its meaning without the heap.
            let recorded = match kind {
                AggType::Tuple { .. } => kind,
                AggType::Struct { shape } | AggType::Array { shape } => {
                    let shape = heap.shape(shape).ok_or_else(invalid)?;
                    let shape = self.arena.shapes.intern(shape).ok_or_else(invalid)?;
                    match kind {
                        AggType::Struct { .. } => AggType::Struct { shape },
                        _ => AggType::Array { shape },
                    }
                }
            };
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
            node.kind = Some(recorded);
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
            ValueRef::I32(value) => RecordedValue::I32(value),
            ValueRef::U32(value) => RecordedValue::U32(value),
            ValueRef::F32(value) => RecordedValue::F32(value.to_bits()),
            ValueRef::Decimal(value) => RecordedValue::Decimal(*value),
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

    pub(crate) fn constructed(
        &self,
        handles: Range<u32>,
    ) -> Result<Vec<AggregateId>, CaptureFault> {
        // All adopted host nodes are reachable from the raw returned roots just captured.
        // Keep original construction order, independent of discovery order in the recording.
        handles
            .map(|handle| {
                self.handles
                    .get(&handle)
                    .copied()
                    .ok_or(CaptureFault::InvalidAggregate { handle })
            })
            .collect()
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

#[cfg(test)]
mod tests {
    use super::RecordedValue;
    use alloc::format;

    #[test]
    fn debug_shows_float_values_with_their_bits() {
        let shown = |value: RecordedValue| format!("{value:?}");
        assert_eq!(
            shown(RecordedValue::F32(0x3DCC_CCCD)),
            "F32(0.1 [0x3dcccccd])"
        );
        assert_eq!(
            shown(RecordedValue::F32(0x7FA0_0001)),
            "F32(NaN [0x7fa00001])"
        );
        assert_eq!(
            shown(RecordedValue::F64((-0.0_f64).to_bits())),
            "F64(-0.0 [0x8000000000000000])"
        );
        assert_eq!(shown(RecordedValue::U32(7)), "U32(7)");
    }
}
