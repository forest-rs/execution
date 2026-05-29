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
    /// Aggregate handle arithmetic overflow.
    HandleOverflow,
    /// Staged-encoded handle does not resolve to any reachable merged mapping.
    UnresolvedStagedHandle,
}

impl fmt::Display for AggError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadHandle => write!(f, "aggregate handle out of bounds"),
            Self::WrongKind => write!(f, "aggregate kind mismatch"),
            Self::OutOfBounds => write!(f, "index out of bounds"),
            Self::BadArity => write!(f, "arity mismatch"),
            Self::HandleOverflow => write!(f, "aggregate handle overflow"),
            Self::UnresolvedStagedHandle => write!(f, "unresolved staged aggregate handle"),
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
#[derive(Clone, Debug)]
pub struct AggDelta {
    base_len: u32,
    staged: AggHeap,
}

impl AggDelta {
    /// Creates an empty delta for an explicit base snapshot length (low-level; see
    /// [`Self::for_base`]).
    fn new(base_len: u32) -> Self {
        Self {
            base_len,
            staged: AggHeap::new(),
        }
    }

    /// Creates an empty delta snapshotting `base`'s current length.
    ///
    /// Deriving `base_len` from `base` keeps the encode ([`Self::overlay`]) and decode
    /// ([`Self::merge_into`]) sides on one snapshot; prefer this in execution code.
    #[must_use]
    pub fn for_base(base: &AggHeap) -> Self {
        Self::new(base.len_u32())
    }

    /// Returns the base snapshot length used by this delta.
    #[must_use]
    pub const fn base_len(&self) -> u32 {
        self.base_len
    }

    /// Borrows `base` (read-only) and this delta's staged heap as an [`AggOverlay`].
    ///
    /// The overlay encodes handles with the delta's own `base_len`, so they decode in
    /// [`Self::merge_into`] with no second `base_len` to keep in sync. `base` must be the heap
    /// this delta was snapshotted from.
    pub fn overlay<'a>(&'a mut self, base: &'a AggHeap) -> AggOverlay<'a> {
        debug_assert_eq!(
            self.base_len,
            base.len_u32(),
            "delta base_len must match the base it overlays"
        );
        AggOverlay::new(base, self.base_len, &mut self.staged)
    }

    /// Deterministically merges reachable staged aggregates into `base`.
    ///
    /// Consumes the delta to enforce single-use commit semantics.
    ///
    /// Reachability is traced from `roots`. Only staged nodes reachable from at least one root are
    /// appended, and append order is increasing staged index.
    ///
    /// This is atomic: on error `base` is left unchanged.
    ///
    /// Returns [`AggError::UnresolvedStagedHandle`] if `roots` (or nested values in reachable
    /// staged nodes) contain a staged-domain handle that does not resolve to this delta.
    pub fn merge_into(self, base: &mut AggHeap, roots: &[Value]) -> Result<AggRemap, AggError> {
        let reachable = self.reachable_staged_nodes(roots)?;
        let remap = AggRemap::from_reachable(self.base_len, base.len_u32(), &reachable)?;

        // All validation ran above, before `base` is touched, so this append loop cannot fail
        // and a rejected delta never partially mutates `base`.
        let reachable_count = reachable.iter().filter(|&&r| r).count();
        base.nodes.reserve(reachable_count);
        for (idx, mut node) in self.staged.nodes.into_iter().enumerate() {
            if !reachable[idx] {
                continue;
            }

            remap.remap_values_in_place(node.values_mut())?;
            let pushed = base.push(node);
            // push and from_reachable assign this base handle independently; they agree only
            // because reachable nodes are appended in staged-index order.
            debug_assert_eq!(
                Some(pushed),
                remap.staged_to_base[idx],
                "push/remap handle assignment desynced"
            );
        }

        Ok(remap)
    }

    fn reachable_staged_nodes(&self, roots: &[Value]) -> Result<Vec<bool>, AggError> {
        let mut reachable = vec![false; self.staged.nodes.len()];
        let mut stack: Vec<usize> = Vec::new();

        for root in roots {
            self.mark_reachable(root, &mut reachable, &mut stack)?;
        }

        while let Some(idx) = stack.pop() {
            let Some(node) = self.staged.nodes.get(idx) else {
                continue;
            };
            for value in node.values() {
                self.mark_reachable(value, &mut reachable, &mut stack)?;
            }
        }

        Ok(reachable)
    }

    /// Marks the staged node referenced by `value` reachable (if any) and queues it.
    ///
    /// Base-domain handles (`< base_len`) are ignored; an out-of-range staged handle is a
    /// malformed delta and is rejected here, before `merge_into` appends anything, keeping the
    /// commit atomic.
    fn mark_reachable(
        &self,
        value: &Value,
        reachable: &mut [bool],
        stack: &mut Vec<usize>,
    ) -> Result<(), AggError> {
        let handle = match value {
            Value::Agg(handle) => *handle,
            Value::Closure(closure) => closure.env,
            _ => return Ok(()),
        };
        if handle.0 < self.base_len {
            return Ok(());
        }

        let local = handle.0 - self.base_len;
        let idx = usize::try_from(local)
            .ok()
            .filter(|&idx| idx < self.staged.nodes.len())
            .ok_or(AggError::UnresolvedStagedHandle)?;
        if !reachable[idx] {
            reachable[idx] = true;
            stack.push(idx);
        }
        Ok(())
    }
}

/// Handle remapping produced by [`AggDelta::merge_into`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AggRemap {
    base_len: u32,
    staged_to_base: Vec<Option<AggHandle>>,
}

impl AggRemap {
    fn from_reachable(
        base_len: u32,
        base_start_len: u32,
        reachable: &[bool],
    ) -> Result<Self, AggError> {
        let reachable_count = reachable.iter().filter(|&&r| r).count();
        let reachable_count_u32 =
            u32::try_from(reachable_count).map_err(|_| AggError::HandleOverflow)?;
        let _ = base_start_len
            .checked_add(reachable_count_u32)
            .ok_or(AggError::HandleOverflow)?;

        let mut staged_to_base = vec![None; reachable.len()];
        let mut assigned = 0_u32;

        for (idx, is_reachable) in reachable.iter().copied().enumerate() {
            if !is_reachable {
                continue;
            }
            let mapped = base_start_len + assigned;
            staged_to_base[idx] = Some(AggHandle(mapped));
            assigned = assigned.checked_add(1).ok_or(AggError::HandleOverflow)?;
        }

        Ok(Self {
            base_len,
            staged_to_base,
        })
    }

    /// Remaps one value from staged/base snapshot space to merged-base space.
    pub fn remap_value(&self, value: &Value) -> Result<Value, AggError> {
        let mut value = value.clone();
        self.remap_values_in_place(core::slice::from_mut(&mut value))?;
        Ok(value)
    }

    /// Remaps values in place.
    ///
    /// Returns [`AggError::UnresolvedStagedHandle`] if any staged-domain handle has no resolved
    /// mapping.
    pub fn remap_values_in_place(&self, values: &mut [Value]) -> Result<(), AggError> {
        for value in values {
            match value {
                Value::Agg(handle) => *handle = self.remap_handle(*handle)?,
                Value::Closure(closure) => closure.env = self.remap_handle(closure.env)?,
                _ => {}
            }
        }
        Ok(())
    }

    fn remap_handle(&self, handle: AggHandle) -> Result<AggHandle, AggError> {
        if handle.0 < self.base_len {
            return Ok(handle);
        }

        let local = handle.0 - self.base_len;
        let Some(idx) = usize::try_from(local).ok() else {
            return Err(AggError::UnresolvedStagedHandle);
        };
        let Some(mapped) = self.staged_to_base.get(idx).and_then(|h| *h) else {
            return Err(AggError::UnresolvedStagedHandle);
        };
        Ok(mapped)
    }
}

#[derive(Copy, Clone, Debug)]
enum OverlayHandle {
    Base(AggHandle),
    Staged(AggHandle),
}

impl<'a> AggOverlay<'a> {
    /// Creates an overlay from a base, an encode-snapshot `base_len`, and a staged heap
    /// (low-level; see [`AggDelta::overlay`]).
    fn new(base: &'a AggHeap, base_len: u32, staged: &'a mut AggHeap) -> Self {
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
    pub fn tuple_new(&mut self, values: Vec<Value>) -> Result<AggHandle, AggError> {
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
    pub fn struct_new(
        &mut self,
        type_id: TypeId,
        values: Vec<Value>,
    ) -> Result<AggHandle, AggError> {
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
    pub fn array_new(
        &mut self,
        elem_type_id: ElemTypeId,
        values: Vec<Value>,
    ) -> Result<AggHandle, AggError> {
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
    fn encode_staged_handle(&self, local: AggHandle) -> Result<AggHandle, AggError> {
        let encoded = self
            .base_len
            .checked_add(local.0)
            .ok_or(AggError::HandleOverflow)?;
        Ok(AggHandle(encoded))
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
    use crate::value::{Closure, FuncId};
    use alloc::vec;

    fn agg_handle(v: Value) -> AggHandle {
        match v {
            Value::Agg(h) => h,
            _ => panic!("expected Value::Agg"),
        }
    }

    /// Builds a delta snapshotting `base` and stages aggregates through an overlay, returning the
    /// delta plus whatever `build` returns (typically the staged-encoded handles it allocated).
    fn staged_delta<R>(
        base: &AggHeap,
        build: impl FnOnce(&mut AggOverlay<'_>) -> R,
    ) -> (AggDelta, R) {
        let mut delta = AggDelta::for_base(base);
        let result = {
            let mut overlay = delta.overlay(base);
            build(&mut overlay)
        };
        (delta, result)
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
        let staged_tuple = overlay
            .tuple_new(vec![Value::I64(22)])
            .expect("tuple alloc should succeed");
        let staged_array = overlay
            .array_new(ElemTypeId(4), vec![Value::U64(9)])
            .expect("array alloc should succeed");

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

        let staged_struct = overlay
            .struct_new(TypeId(12), vec![Value::I64(33)])
            .expect("struct alloc should succeed");

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

        let h0 = overlay.tuple_new(vec![]).expect("alloc should succeed");
        let h1 = overlay.tuple_new(vec![]).expect("alloc should succeed");

        assert_eq!(h0.0, base_len);
        assert_eq!(h1.0, base_len + 1);
        assert_eq!(overlay.base_len(), base_len);
    }

    #[test]
    fn overlay_allocations_round_trip_through_merge() {
        let mut base = AggHeap::new();
        let _ = base.tuple_new(vec![Value::I64(1)]); // base node 0

        let mut delta = AggDelta::for_base(&base);
        let root = {
            let mut overlay = delta.overlay(&base);
            let leaf = overlay.tuple_new(vec![Value::I64(42)]).expect("alloc");
            overlay
                .struct_new(TypeId(3), vec![Value::Agg(leaf), Value::Agg(AggHandle(0))])
                .expect("alloc")
        };

        let mut outputs = vec![Value::Agg(root)];
        let remap = delta.merge_into(&mut base, &outputs).expect("merge");
        remap.remap_values_in_place(&mut outputs).expect("remap");

        let root_h = agg_handle(outputs[0].clone());
        let leaf_h = agg_handle(base.struct_get(root_h, 0).expect("field 0"));
        assert!(root_h.0 < base.len_u32() && leaf_h.0 < base.len_u32());
        assert_eq!(base.tuple_get(leaf_h, 0), Ok(Value::I64(42))); // staged leaf survived
        assert_eq!(base.struct_get(root_h, 1), Ok(Value::Agg(AggHandle(0)))); // base ref kept
    }

    #[test]
    fn overlay_staged_handle_encoding_overflow_returns_error() {
        let base = AggHeap::new();
        let mut staged = AggHeap::new();
        let mut overlay = AggOverlay::new(&base, u32::MAX, &mut staged);

        let first = overlay
            .tuple_new(vec![Value::I64(1)])
            .expect("first staged allocation should fit at u32::MAX");
        assert_eq!(first, AggHandle(u32::MAX));
        assert_eq!(overlay.tuple_len(first), Ok(1));

        let second = overlay.tuple_new(vec![Value::I64(2)]);
        assert_eq!(second, Err(AggError::HandleOverflow));
    }

    #[test]
    fn delta_merge_is_deterministic_for_identical_inputs() {
        let mut base_a = AggHeap::new();
        let _ = base_a.tuple_new(vec![Value::I64(7)]);
        let mut base_b = base_a.clone();

        let (delta, root) = staged_delta(&base_a, |o| {
            let leaf = o.tuple_new(vec![Value::I64(1)]).expect("alloc");
            o.array_new(ElemTypeId(9), vec![Value::Agg(leaf)])
                .expect("alloc")
        });
        let roots = vec![Value::Agg(root)];

        let remap_a = delta
            .clone()
            .merge_into(&mut base_a, &roots)
            .expect("merge should succeed");
        let remap_b = delta
            .merge_into(&mut base_b, &roots)
            .expect("merge should succeed");

        assert_eq!(remap_a, remap_b);
        let mapped_root_a = agg_handle(remap_a.remap_value(&roots[0]).expect("remap should work"));
        let mapped_root_b = agg_handle(remap_b.remap_value(&roots[0]).expect("remap should work"));
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

        let (delta, root) = staged_delta(&base, |o| {
            let leaf = o.tuple_new(vec![Value::I64(3)]).expect("alloc");
            o.tuple_new(vec![Value::Agg(leaf)]).expect("alloc")
        });
        let roots = vec![Value::Agg(root)];

        let remap = delta
            .merge_into(&mut base, &roots)
            .expect("merge should succeed");
        let mapped_root = agg_handle(remap.remap_value(&roots[0]).expect("remap should work"));
        assert_eq!(mapped_root, AggHandle(1));
        assert_eq!(base.tuple_get(mapped_root, 0), Ok(Value::Agg(AggHandle(0))));
        assert_eq!(base.tuple_get(AggHandle(0), 0), Ok(Value::I64(3)));
    }

    #[test]
    fn delta_merge_filters_unreachable_staged_nodes() {
        let mut base = AggHeap::new();

        let (delta, root) = staged_delta(&base, |o| {
            let keep = o.tuple_new(vec![Value::I64(10)]).expect("alloc");
            let _unreachable = o.tuple_new(vec![Value::I64(20)]).expect("alloc");
            o.tuple_new(vec![Value::Agg(keep)]).expect("alloc")
        });
        let roots = vec![Value::Agg(root)];

        let remap = delta
            .merge_into(&mut base, &roots)
            .expect("merge should succeed");
        let mapped_root = agg_handle(remap.remap_value(&roots[0]).expect("remap should work"));

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
    fn delta_merge_rejects_out_of_range_staged_root_handle() {
        let mut base = AggHeap::new();
        let _ = base.tuple_new(vec![Value::I64(7)]);
        let base_len = base.len_u32();
        let (delta, _filler) =
            staged_delta(&base, |o| o.tuple_new(vec![Value::I64(8)]).expect("alloc"));

        let malformed = Value::Agg(AggHandle(base_len + 10));
        let remap = delta.merge_into(&mut base, core::slice::from_ref(&malformed));
        assert_eq!(remap, Err(AggError::UnresolvedStagedHandle));
        assert_eq!(base.len_u32(), base_len);
    }

    #[test]
    fn delta_merge_rejects_reachable_node_with_unresolved_nested_staged_handle() {
        let mut base = AggHeap::new();
        let base_len = base.len_u32();
        let (delta, root) = staged_delta(&base, |o| {
            o.tuple_new(vec![Value::Agg(AggHandle(base_len + 99))])
                .expect("alloc")
        });
        let roots = vec![Value::Agg(root)];

        let remap = delta.merge_into(&mut base, &roots);
        assert_eq!(remap, Err(AggError::UnresolvedStagedHandle));
        assert_eq!(base.len_u32(), base_len);
    }

    #[test]
    fn delta_merge_does_not_mutate_base_when_a_later_reachable_node_is_malformed() {
        // node 0 -> node 1 -> out-of-range handle: the failure surfaces only on the second
        // (reachable) node, so a non-atomic merge would have already appended node 0 to `base`.
        let mut base = AggHeap::new();
        let base_len = base.len_u32();
        assert_eq!(base_len, 0);

        let (delta, node0) = staged_delta(&base, |o| {
            // node 0 forward-references node 1, which isn't allocated yet, so this handle is
            // necessarily hand-encoded; node 1 then references an out-of-range handle.
            let n0 = o
                .tuple_new(vec![Value::Agg(AggHandle(base_len + 1))])
                .expect("alloc");
            let _n1 = o
                .tuple_new(vec![Value::Agg(AggHandle(base_len + 99))])
                .expect("alloc");
            n0
        });
        let roots = vec![Value::Agg(node0)];

        let result = delta.merge_into(&mut base, &roots);

        assert_eq!(result, Err(AggError::UnresolvedStagedHandle));
        assert_eq!(base.len_u32(), 0, "rejected merge must not mutate base");
    }

    #[test]
    fn delta_merge_handles_u32_max_boundary_for_staged_decode() {
        let mut base = AggHeap::new();
        // A u32::MAX base snapshot can't come from a real base (so `for_base`/`overlay` can't
        // build it); construct the boundary delta directly.
        let mut staged = AggHeap::new();
        let _ = staged.tuple_new(vec![Value::I64(44)]);
        let delta = AggDelta {
            base_len: u32::MAX,
            staged,
        };
        let root = Value::Agg(AggHandle(u32::MAX));

        let remap = delta
            .merge_into(&mut base, core::slice::from_ref(&root))
            .expect("merge should succeed at u32 max decode boundary");
        let mut remapped_values = vec![root.clone(), Value::I64(9)];
        remap
            .remap_values_in_place(&mut remapped_values)
            .expect("remap should succeed");

        assert_eq!(base.len_u32(), 1);
        assert_eq!(remapped_values[0], Value::Agg(AggHandle(0)));
        assert_eq!(remapped_values[1], Value::I64(9));
        assert_eq!(base.tuple_get(AggHandle(0), 0), Ok(Value::I64(44)));
    }

    #[test]
    fn remap_overflow_with_multiple_reachable_nodes_returns_error() {
        let remap = AggRemap::from_reachable(0, u32::MAX, &[true, true]);
        assert_eq!(remap, Err(AggError::HandleOverflow));
    }

    #[test]
    fn remapped_outputs_do_not_contain_staged_handles() {
        let mut base = AggHeap::new();
        let _ = base.tuple_new(vec![Value::I64(100)]);

        let (delta, (leaf, root)) = staged_delta(&base, |o| {
            let leaf = o.tuple_new(vec![Value::I64(1)]).expect("alloc");
            let root = o.tuple_new(vec![Value::Agg(leaf)]).expect("alloc");
            (leaf, root)
        });

        let outputs = vec![
            Value::Agg(AggHandle(0)), // base node 0
            Value::Agg(root),         // staged node referencing the leaf
            Value::Agg(leaf),         // staged leaf
        ];

        let remap = delta
            .merge_into(&mut base, &outputs)
            .expect("merge should succeed");
        let mut remapped = outputs.clone();
        remap
            .remap_values_in_place(&mut remapped)
            .expect("remap should succeed");

        let merged_len = base.len_u32();
        for value in &remapped {
            if let Value::Agg(handle) = value {
                assert!(
                    handle.0 < merged_len,
                    "remapped output handle {} must be in merged base domain < {}",
                    handle.0,
                    merged_len
                );
            }
        }

        let remapped_root = agg_handle(remapped[1].clone());
        assert_eq!(
            base.tuple_get(remapped_root, 0),
            Ok(Value::Agg(agg_handle(remapped[2].clone())))
        );
    }

    #[test]
    fn delta_merge_traces_and_remaps_root_closure_env() {
        let mut base = AggHeap::new();
        let _ = base.tuple_new(vec![Value::I64(7)]); // base node 0

        // An unreachable filler before the env makes the env's staged encoding (2) differ from
        // its post-merge base index (1), so this proves the closure env is actually rewritten,
        // not just left at a coincidentally-valid handle.
        let (delta, env) = staged_delta(&base, |o| {
            let _unreachable = o.tuple_new(vec![Value::I64(1)]).expect("alloc");
            o.tuple_new(vec![Value::I64(42)]).expect("alloc")
        });
        let mut roots = vec![Value::Closure(Closure {
            func: FuncId(5),
            env,
        })];

        let remap = delta
            .merge_into(&mut base, &roots)
            .expect("merge should succeed");
        remap
            .remap_values_in_place(&mut roots)
            .expect("remap should succeed");

        let Value::Closure(merged) = &roots[0] else {
            panic!("expected closure root");
        };
        assert_eq!(merged.func, FuncId(5)); // function id preserved
        assert_eq!(merged.env, AggHandle(1)); // staged env remapped into base domain
        assert_eq!(base.tuple_get(merged.env, 0), Ok(Value::I64(42)));
    }

    #[test]
    fn delta_merge_rewrites_closure_env_nested_in_staged_aggregate() {
        let mut base = AggHeap::new();

        // The unreachable filler at staged index 0 makes the env's remap non-trivial (encoded 1
        // -> base index 0): a closure left un-remapped would point at the wrong committed node.
        let (delta, root) = staged_delta(&base, |o| {
            let _unreachable = o.tuple_new(vec![Value::I64(7)]).expect("alloc");
            let env = o.tuple_new(vec![Value::I64(3)]).expect("alloc");
            let closure = Value::Closure(Closure {
                func: FuncId(8),
                env,
            });
            o.tuple_new(vec![closure]).expect("alloc")
        });
        let roots = vec![Value::Agg(root)];

        let remap = delta
            .merge_into(&mut base, &roots)
            .expect("merge should succeed");
        let mapped_root = agg_handle(remap.remap_value(&roots[0]).expect("remap should work"));

        let field = base.tuple_get(mapped_root, 0).expect("field 0");
        let Value::Closure(merged) = field else {
            panic!("expected closure field");
        };
        assert_eq!(merged.func, FuncId(8));
        assert!(merged.env.0 < base.len_u32()); // env rewritten into base domain
        assert_eq!(base.tuple_get(merged.env, 0), Ok(Value::I64(3)));
    }
}
