// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Aggregate heap for `execution_tape`.
//!
//! v1 aggregates are immutable, acyclic, and structural.
//! They are stored out-of-line in an arena owned by the VM/runtime.

use alloc::format;
use alloc::string::String;
use alloc::vec;
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

impl AggNode {
    #[inline]
    fn values(&self) -> &[Value] {
        match self {
            Self::Tuple { values } | Self::Struct { values, .. } | Self::Array { values, .. } => {
                values
            }
        }
    }

    #[inline]
    fn values_mut(&mut self) -> &mut [Value] {
        match self {
            Self::Tuple { values } | Self::Struct { values, .. } | Self::Array { values, .. } => {
                values
            }
        }
    }
}

/// An immutable aggregate heap.
///
/// v1 uses a simple `Vec`-backed store and returns stable handles.
#[derive(Clone, Debug, Default)]
pub struct AggHeap {
    nodes: Vec<AggNode>,
}

/// Base+staged aggregate view used by staged execution.
///
/// Reads resolve handles against a fixed `base_len` snapshot:
/// - `h < base_len`: read from `base`
/// - `h >= base_len`: read from `staged` at `h - base_len`
///
/// New allocations are always appended to `staged` and returned in staged-encoded form
/// (`base_len + local_index`).
#[derive(Debug)]
pub struct AggOverlay<'a> {
    base: &'a AggHeap,
    base_len: u32,
    staged: &'a mut AggHeap,
}

/// Staged aggregate allocations produced by one execute pass.
///
/// `base_len` is the base snapshot used to encode staged handles (`base_len + local_index`).
#[derive(Clone, Debug, Default)]
pub struct AggDelta {
    base_len: u32,
    staged: AggHeap,
}

impl AggDelta {
    /// Creates an empty delta for a base snapshot length.
    #[must_use]
    pub fn new(base_len: u32) -> Self {
        Self {
            base_len,
            staged: AggHeap::new(),
        }
    }

    /// Returns the base snapshot length used by this delta.
    #[must_use]
    pub const fn base_len(&self) -> u32 {
        self.base_len
    }

    /// Returns staged aggregate storage.
    #[must_use]
    pub fn staged(&self) -> &AggHeap {
        &self.staged
    }

    /// Returns mutable staged aggregate storage.
    pub fn staged_mut(&mut self) -> &mut AggHeap {
        &mut self.staged
    }

    /// Deterministically merges reachable staged aggregates into `base`.
    ///
    /// Reachability is traced from `roots`. Only staged nodes reachable from at least one root are
    /// appended, and append order is increasing staged index.
    ///
    /// Handles that appear staged-encoded but resolve outside this delta's staged range are treated
    /// as unresolved and left unchanged by the returned remap.
    #[must_use]
    pub fn merge_into(&self, base: &mut AggHeap, roots: &[Value]) -> AggRemap {
        let reachable = self.reachable_staged_nodes(roots);
        let remap = AggRemap::from_reachable(self.base_len, base.len_u32(), &reachable);

        for (idx, node) in self.staged.nodes.iter().enumerate() {
            if !reachable[idx] {
                continue;
            }

            let mut remapped = node.clone();
            remap.remap_values_in_place(remapped.values_mut());
            let _ = base.push(remapped);
        }

        remap
    }

    fn reachable_staged_nodes(&self, roots: &[Value]) -> Vec<bool> {
        let mut reachable = vec![false; self.staged.nodes.len()];
        let mut stack: Vec<usize> = Vec::new();

        for root in roots {
            self.push_reachable_from_value(root, &mut reachable, &mut stack);
        }

        while let Some(idx) = stack.pop() {
            let Some(node) = self.staged.nodes.get(idx) else {
                continue;
            };
            for value in node.values() {
                self.push_reachable_from_value(value, &mut reachable, &mut stack);
            }
        }

        reachable
    }

    fn push_reachable_from_value(
        &self,
        value: &Value,
        reachable: &mut [bool],
        stack: &mut Vec<usize>,
    ) {
        let Value::Agg(handle) = value else {
            return;
        };
        let Some(idx) = self.staged_index(*handle) else {
            return;
        };
        if reachable[idx] {
            return;
        }

        reachable[idx] = true;
        stack.push(idx);
    }

    fn staged_index(&self, handle: AggHandle) -> Option<usize> {
        if handle.0 < self.base_len {
            return None;
        }
        let local = handle.0 - self.base_len;
        let idx = usize::try_from(local).ok()?;
        (idx < self.staged.nodes.len()).then_some(idx)
    }
}

/// Handle remapping produced by [`AggDelta::merge_into`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AggRemap {
    base_len: u32,
    staged_to_base: Vec<Option<AggHandle>>,
}

impl AggRemap {
    fn from_reachable(base_len: u32, base_start_len: u32, reachable: &[bool]) -> Self {
        let mut staged_to_base = vec![None; reachable.len()];
        let mut next = base_start_len;

        for (idx, is_reachable) in reachable.iter().copied().enumerate() {
            if !is_reachable {
                continue;
            }
            staged_to_base[idx] = Some(AggHandle(next));
            next = next.saturating_add(1);
        }

        Self {
            base_len,
            staged_to_base,
        }
    }

    /// Remaps one value from staged/base snapshot space to merged-base space.
    ///
    /// Non-aggregate values and unresolved staged handles are returned unchanged.
    #[must_use]
    pub fn remap_value(&self, value: &Value) -> Value {
        match value {
            Value::Agg(handle) => Value::Agg(self.remap_handle(*handle)),
            _ => value.clone(),
        }
    }

    /// Remaps values in place.
    pub fn remap_values_in_place(&self, values: &mut [Value]) {
        for value in values {
            *value = self.remap_value(value);
        }
    }

    fn remap_handle(&self, handle: AggHandle) -> AggHandle {
        if handle.0 < self.base_len {
            return handle;
        }

        let local = handle.0 - self.base_len;
        let Some(idx) = usize::try_from(local).ok() else {
            return handle;
        };
        let Some(mapped) = self.staged_to_base.get(idx).and_then(|h| *h) else {
            return handle;
        };
        mapped
    }
}

#[derive(Copy, Clone, Debug)]
enum OverlayHandle {
    Base(AggHandle),
    Staged(AggHandle),
}

impl<'a> AggOverlay<'a> {
    /// Creates an overlay from a read-only base and writable staged heap.
    #[must_use]
    pub fn new(base: &'a AggHeap, base_len: u32, staged: &'a mut AggHeap) -> Self {
        Self {
            base,
            base_len,
            staged,
        }
    }

    /// Returns the base-length snapshot used for handle routing.
    #[must_use]
    pub const fn base_len(&self) -> u32 {
        self.base_len
    }

    /// Returns the aggregate type for `handle`.
    pub fn agg_type(&self, handle: AggHandle) -> Result<AggType, AggError> {
        match self.classify(handle) {
            OverlayHandle::Base(h) => self.base.agg_type(h),
            OverlayHandle::Staged(h) => self.staged.agg_type(h),
        }
    }

    /// Allocates a tuple in staged storage and returns a staged-encoded handle.
    #[must_use]
    pub fn tuple_new(&mut self, values: Vec<Value>) -> AggHandle {
        let local = self.staged.tuple_new(values);
        self.encode_staged_handle(local)
    }

    /// Returns tuple element `index`.
    pub fn tuple_get(&self, tuple: AggHandle, index: usize) -> Result<Value, AggError> {
        match self.classify(tuple) {
            OverlayHandle::Base(h) => self.base.tuple_get(h, index),
            OverlayHandle::Staged(h) => self.staged.tuple_get(h, index),
        }
    }

    /// Returns tuple length.
    pub fn tuple_len(&self, tuple: AggHandle) -> Result<usize, AggError> {
        match self.classify(tuple) {
            OverlayHandle::Base(h) => self.base.tuple_len(h),
            OverlayHandle::Staged(h) => self.staged.tuple_len(h),
        }
    }

    /// Allocates a struct in staged storage and returns a staged-encoded handle.
    #[must_use]
    pub fn struct_new(&mut self, type_id: TypeId, values: Vec<Value>) -> AggHandle {
        let local = self.staged.struct_new(type_id, values);
        self.encode_staged_handle(local)
    }

    /// Returns struct field `field_index`.
    pub fn struct_get(&self, st: AggHandle, field_index: usize) -> Result<Value, AggError> {
        match self.classify(st) {
            OverlayHandle::Base(h) => self.base.struct_get(h, field_index),
            OverlayHandle::Staged(h) => self.staged.struct_get(h, field_index),
        }
    }

    /// Returns struct field count.
    pub fn struct_field_count(&self, st: AggHandle) -> Result<usize, AggError> {
        match self.classify(st) {
            OverlayHandle::Base(h) => self.base.struct_field_count(h),
            OverlayHandle::Staged(h) => self.staged.struct_field_count(h),
        }
    }

    /// Allocates an array in staged storage and returns a staged-encoded handle.
    #[must_use]
    pub fn array_new(&mut self, elem_type_id: ElemTypeId, values: Vec<Value>) -> AggHandle {
        let local = self.staged.array_new(elem_type_id, values);
        self.encode_staged_handle(local)
    }

    /// Returns array element `index`.
    pub fn array_get(&self, arr: AggHandle, index: usize) -> Result<Value, AggError> {
        match self.classify(arr) {
            OverlayHandle::Base(h) => self.base.array_get(h, index),
            OverlayHandle::Staged(h) => self.staged.array_get(h, index),
        }
    }

    /// Returns array length.
    pub fn array_len(&self, arr: AggHandle) -> Result<usize, AggError> {
        match self.classify(arr) {
            OverlayHandle::Base(h) => self.base.array_len(h),
            OverlayHandle::Staged(h) => self.staged.array_len(h),
        }
    }

    #[inline]
    fn classify(&self, handle: AggHandle) -> OverlayHandle {
        if handle.0 < self.base_len {
            OverlayHandle::Base(handle)
        } else {
            OverlayHandle::Staged(AggHandle(handle.0 - self.base_len))
        }
    }

    #[inline]
    fn encode_staged_handle(&self, local: AggHandle) -> AggHandle {
        // Saturating arithmetic preserves the staged-range invariant (`>= base_len`) even when
        // indices approach the u32 handle ceiling.
        let encoded = self.base_len.saturating_add(local.0);
        debug_assert!(encoded >= self.base_len);
        AggHandle(encoded)
    }
}

impl AggHeap {
    /// Creates an empty heap.
    #[must_use]
    pub fn new() -> Self {
        Self { nodes: Vec::new() }
    }

    /// Returns the current heap length as `u32`.
    ///
    /// This mirrors the handle index domain used by [`AggHandle`].
    #[must_use]
    pub fn len_u32(&self) -> u32 {
        u32::try_from(self.nodes.len()).unwrap_or(u32::MAX)
    }

    /// Removes all aggregate nodes while preserving allocated capacity.
    pub fn clear(&mut self) {
        self.nodes.clear();
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

    fn agg_handle(v: Value) -> AggHandle {
        match v {
            Value::Agg(h) => h,
            _ => panic!("expected Value::Agg"),
        }
    }

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

    #[test]
    fn len_u32_tracks_nodes() {
        let mut h = AggHeap::new();
        assert_eq!(h.len_u32(), 0);
        let _ = h.tuple_new(vec![]);
        assert_eq!(h.len_u32(), 1);
        let _ = h.array_new(ElemTypeId(0), vec![]);
        assert_eq!(h.len_u32(), 2);
    }

    #[test]
    fn clear_empties_heap_and_preserves_capacity() {
        let mut h = AggHeap::new();
        let _ = h.tuple_new(vec![Value::I64(1)]);
        let _ = h.struct_new(TypeId(7), vec![Value::Bool(true)]);
        let cap_before = h.nodes.capacity();
        assert!(cap_before > 0);

        h.clear();

        assert_eq!(h.len_u32(), 0);
        assert_eq!(h.nodes.capacity(), cap_before);
    }

    #[test]
    fn overlay_reads_base_and_staged_by_handle_domain() {
        let mut base = AggHeap::new();
        let base_tuple = base.tuple_new(vec![Value::I64(11)]);
        let base_array = base.array_new(ElemTypeId(3), vec![Value::U64(1), Value::U64(2)]);
        let base_len = base.len_u32();
        let mut staged = AggHeap::new();

        let mut overlay = AggOverlay::new(&base, base_len, &mut staged);
        let staged_tuple = overlay.tuple_new(vec![Value::I64(22)]);
        let staged_array = overlay.array_new(ElemTypeId(4), vec![Value::U64(9)]);

        assert!(staged_tuple.0 >= base_len);
        assert!(staged_array.0 >= base_len);

        assert_eq!(overlay.tuple_get(base_tuple, 0), Ok(Value::I64(11)));
        assert_eq!(overlay.array_len(base_array), Ok(2));
        assert_eq!(overlay.tuple_get(staged_tuple, 0), Ok(Value::I64(22)));
        assert_eq!(overlay.array_get(staged_array, 0), Ok(Value::U64(9)));
    }

    #[test]
    fn overlay_supports_struct_and_agg_type_across_spaces() {
        let mut base = AggHeap::new();
        let base_struct = base.struct_new(TypeId(10), vec![Value::Bool(true), Value::I64(8)]);
        let base_len = base.len_u32();
        let mut staged = AggHeap::new();
        let mut overlay = AggOverlay::new(&base, base_len, &mut staged);

        let staged_struct = overlay.struct_new(TypeId(12), vec![Value::I64(33)]);

        assert_eq!(overlay.struct_field_count(base_struct), Ok(2));
        assert_eq!(overlay.struct_get(base_struct, 1), Ok(Value::I64(8)));
        assert_eq!(overlay.struct_field_count(staged_struct), Ok(1));
        assert_eq!(overlay.struct_get(staged_struct, 0), Ok(Value::I64(33)));

        assert_eq!(
            overlay.agg_type(base_struct),
            Ok(AggType::Struct {
                type_id: TypeId(10)
            })
        );
        assert_eq!(
            overlay.agg_type(staged_struct),
            Ok(AggType::Struct {
                type_id: TypeId(12)
            })
        );
    }

    #[test]
    fn overlay_new_allocations_are_staged_encoded() {
        let mut base = AggHeap::new();
        let _ = base.tuple_new(vec![Value::I64(1)]);
        let _ = base.tuple_new(vec![Value::I64(2)]);
        let base_len = base.len_u32();
        let mut staged = AggHeap::new();
        let mut overlay = AggOverlay::new(&base, base_len, &mut staged);

        let h0 = overlay.tuple_new(vec![]);
        let h1 = overlay.tuple_new(vec![]);

        assert_eq!(h0.0, base_len);
        assert_eq!(h1.0, base_len + 1);
        assert_eq!(overlay.base_len(), base_len);
    }

    #[test]
    fn delta_merge_is_deterministic_for_identical_inputs() {
        let mut base_a = AggHeap::new();
        let _ = base_a.tuple_new(vec![Value::I64(7)]);
        let mut base_b = base_a.clone();
        let base_len = base_a.len_u32();

        let mut delta = AggDelta::new(base_len);
        let _ = delta.staged_mut().tuple_new(vec![Value::I64(1)]);
        let _ = delta
            .staged_mut()
            .array_new(ElemTypeId(9), vec![Value::Agg(AggHandle(base_len))]);
        let roots = vec![Value::Agg(AggHandle(base_len + 1))];

        let remap_a = delta.merge_into(&mut base_a, &roots);
        let remap_b = delta.merge_into(&mut base_b, &roots);

        assert_eq!(remap_a, remap_b);
        let mapped_root_a = agg_handle(remap_a.remap_value(&roots[0]));
        let mapped_root_b = agg_handle(remap_b.remap_value(&roots[0]));
        assert_eq!(mapped_root_a, mapped_root_b);
        assert_eq!(base_a.len_u32(), base_b.len_u32());
        assert_eq!(
            base_a.array_get(mapped_root_a, 0),
            base_b.array_get(mapped_root_b, 0)
        );
    }

    #[test]
    fn delta_merge_rewrites_nested_staged_handles() {
        let mut base = AggHeap::new();
        let base_len = base.len_u32();
        let mut delta = AggDelta::new(base_len);

        let _ = delta.staged_mut().tuple_new(vec![Value::I64(3)]);
        let _ = delta
            .staged_mut()
            .tuple_new(vec![Value::Agg(AggHandle(base_len))]);
        let roots = vec![Value::Agg(AggHandle(base_len + 1))];

        let remap = delta.merge_into(&mut base, &roots);
        let mapped_root = agg_handle(remap.remap_value(&roots[0]));
        assert_eq!(mapped_root, AggHandle(1));
        assert_eq!(base.tuple_get(mapped_root, 0), Ok(Value::Agg(AggHandle(0))));
        assert_eq!(base.tuple_get(AggHandle(0), 0), Ok(Value::I64(3)));
    }

    #[test]
    fn delta_merge_filters_unreachable_staged_nodes() {
        let mut base = AggHeap::new();
        let base_len = base.len_u32();
        let mut delta = AggDelta::new(base_len);

        let _ = delta.staged_mut().tuple_new(vec![Value::I64(10)]);
        let _ = delta.staged_mut().tuple_new(vec![Value::I64(20)]);
        let _ = delta
            .staged_mut()
            .tuple_new(vec![Value::Agg(AggHandle(base_len))]);
        let roots = vec![Value::Agg(AggHandle(base_len + 2))];

        let remap = delta.merge_into(&mut base, &roots);
        let mapped_root = agg_handle(remap.remap_value(&roots[0]));

        assert_eq!(base.len_u32(), 2);
        assert_eq!(mapped_root, AggHandle(1));
        assert_eq!(base.tuple_get(AggHandle(0), 0), Ok(Value::I64(10)));
        assert_eq!(
            base.tuple_get(AggHandle(1), 0),
            Ok(Value::Agg(AggHandle(0)))
        );
        assert_eq!(base.tuple_get(AggHandle(2), 0), Err(AggError::BadHandle));
    }

    #[test]
    fn delta_merge_leaves_malformed_staged_handles_unresolved() {
        let mut base = AggHeap::new();
        let _ = base.tuple_new(vec![Value::I64(7)]);
        let base_len = base.len_u32();
        let mut delta = AggDelta::new(base_len);
        let _ = delta.staged_mut().tuple_new(vec![Value::I64(8)]);

        let malformed = Value::Agg(AggHandle(base_len + 10));
        let remap = delta.merge_into(&mut base, core::slice::from_ref(&malformed));
        assert_eq!(base.len_u32(), base_len);
        assert_eq!(remap.remap_value(&malformed), malformed);
    }

    #[test]
    fn delta_merge_handles_u32_max_boundary_for_staged_decode() {
        let mut base = AggHeap::new();
        let mut delta = AggDelta::new(u32::MAX);
        let _ = delta.staged_mut().tuple_new(vec![Value::I64(44)]);
        let root = Value::Agg(AggHandle(u32::MAX));

        let remap = delta.merge_into(&mut base, core::slice::from_ref(&root));
        let mut remapped_values = vec![root.clone(), Value::I64(9)];
        remap.remap_values_in_place(&mut remapped_values);

        assert_eq!(base.len_u32(), 1);
        assert_eq!(remapped_values[0], Value::Agg(AggHandle(0)));
        assert_eq!(remapped_values[1], Value::I64(9));
        assert_eq!(base.tuple_get(AggHandle(0), 0), Ok(Value::I64(44)));
    }
}
