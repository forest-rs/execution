// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Aggregate heap for `execution_tape`.
//!
//! v1 aggregates are immutable, acyclic, and structural.
//! They are stored out-of-line in an arena owned by the VM/runtime.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::program::{ElemTypeId, TypeId};
use crate::value::{AggHandle, AggType, Value};

/// An aggregate heap error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AggError {
    /// Handle was out of bounds.
    BadHandle,
    /// Type mismatch (e.g. non-array passed to `array_get`).
    WrongKind,
    /// Index out of bounds.
    OutOfBounds,
    /// Struct field count mismatch.
    BadArity,
}

impl fmt::Display for AggError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadHandle => write!(f, "aggregate handle out of bounds"),
            Self::WrongKind => write!(f, "aggregate kind mismatch"),
            Self::OutOfBounds => write!(f, "index out of bounds"),
            Self::BadArity => write!(f, "arity mismatch"),
        }
    }
}

impl core::error::Error for AggError {}

/// A heap node storing an aggregate value.
#[derive(Clone, Debug, PartialEq)]
enum AggNode {
    Tuple {
        values: Vec<Value>,
    },
    Struct {
        type_id: TypeId,
        values: Vec<Value>,
    },
    Array {
        elem_type_id: ElemTypeId,
        values: Vec<Value>,
    },
}

/// An immutable aggregate heap.
///
/// v1 uses a simple `Vec`-backed store and returns stable handles.
#[derive(Clone, Debug, Default)]
pub struct AggHeap {
    nodes: Vec<AggNode>,
}

impl AggHeap {
    /// Creates an empty heap.
    #[must_use]
    pub fn new() -> Self {
        Self { nodes: Vec::new() }
    }

    /// Returns the aggregate type for `handle`.
    pub fn agg_type(&self, handle: AggHandle) -> Result<AggType, AggError> {
        match self.node(handle)? {
            AggNode::Tuple { values } => Ok(AggType::Tuple {
                arity: u32::try_from(values.len()).unwrap_or(u32::MAX),
            }),
            AggNode::Struct { type_id, .. } => Ok(AggType::Struct { type_id: *type_id }),
            AggNode::Array { elem_type_id, .. } => Ok(AggType::Array {
                elem_type_id: *elem_type_id,
            }),
        }
    }

    /// Allocates a tuple aggregate.
    pub fn tuple_new(&mut self, values: Vec<Value>) -> AggHandle {
        self.push(AggNode::Tuple { values })
    }

    /// Allocates a struct aggregate.
    pub fn struct_new(&mut self, type_id: TypeId, values: Vec<Value>) -> AggHandle {
        self.push(AggNode::Struct { type_id, values })
    }

    /// Allocates an array aggregate.
    pub fn array_new(&mut self, elem_type_id: ElemTypeId, values: Vec<Value>) -> AggHandle {
        self.push(AggNode::Array {
            elem_type_id,
            values,
        })
    }

    /// Returns tuple element `index`.
    pub fn tuple_get(&self, tuple: AggHandle, index: usize) -> Result<Value, AggError> {
        match self.node(tuple)? {
            AggNode::Tuple { values } => values.get(index).cloned().ok_or(AggError::OutOfBounds),
            _ => Err(AggError::WrongKind),
        }
    }

    /// Returns tuple length.
    pub fn tuple_len(&self, tuple: AggHandle) -> Result<usize, AggError> {
        match self.node(tuple)? {
            AggNode::Tuple { values } => Ok(values.len()),
            _ => Err(AggError::WrongKind),
        }
    }

    /// Returns struct field `field_index` (index into the stable field ordering for `type_id`).
    pub fn struct_get(&self, st: AggHandle, field_index: usize) -> Result<Value, AggError> {
        match self.node(st)? {
            AggNode::Struct { values, .. } => values
                .get(field_index)
                .cloned()
                .ok_or(AggError::OutOfBounds),
            _ => Err(AggError::WrongKind),
        }
    }

    /// Returns struct field count.
    pub fn struct_field_count(&self, st: AggHandle) -> Result<usize, AggError> {
        match self.node(st)? {
            AggNode::Struct { values, .. } => Ok(values.len()),
            _ => Err(AggError::WrongKind),
        }
    }

    /// Returns array length.
    pub fn array_len(&self, arr: AggHandle) -> Result<usize, AggError> {
        match self.node(arr)? {
            AggNode::Array { values, .. } => Ok(values.len()),
            _ => Err(AggError::WrongKind),
        }
    }

    /// Returns array element `index`.
    pub fn array_get(&self, arr: AggHandle, index: usize) -> Result<Value, AggError> {
        match self.node(arr)? {
            AggNode::Array { values, .. } => {
                values.get(index).cloned().ok_or(AggError::OutOfBounds)
            }
            _ => Err(AggError::WrongKind),
        }
    }

    /// Returns a debug view of the underlying node for diagnostics.
    pub fn debug_node(&self, handle: AggHandle) -> Result<String, AggError> {
        Ok(match self.node(handle)? {
            AggNode::Tuple { values } => format!("Tuple({})", values.len()),
            AggNode::Struct { type_id, values } => {
                format!("Struct(type_id={}, fields={})", type_id.0, values.len())
            }
            AggNode::Array {
                elem_type_id,
                values,
            } => format!(
                "Array(elem_type_id={}, len={})",
                elem_type_id.0,
                values.len()
            ),
        })
    }

    fn push(&mut self, node: AggNode) -> AggHandle {
        let idx = u32::try_from(self.nodes.len()).unwrap_or(u32::MAX);
        self.nodes.push(node);
        AggHandle(idx)
    }

    fn node(&self, handle: AggHandle) -> Result<&AggNode, AggError> {
        self.nodes.get(handle.0 as usize).ok_or(AggError::BadHandle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn tuple_roundtrip() {
        let mut h = AggHeap::new();
        let t = h.tuple_new(vec![Value::I64(1), Value::Bool(true)]);
        assert_eq!(h.tuple_get(t, 0), Ok(Value::I64(1)));
        assert_eq!(h.tuple_get(t, 1), Ok(Value::Bool(true)));
        assert_eq!(h.tuple_get(t, 2), Err(AggError::OutOfBounds));
    }

    #[test]
    fn array_len_get() {
        let mut h = AggHeap::new();
        let a = h.array_new(ElemTypeId(0), vec![Value::U64(7), Value::U64(8)]);
        assert_eq!(h.array_len(a), Ok(2));
        assert_eq!(h.array_get(a, 1), Ok(Value::U64(8)));
    }
}
