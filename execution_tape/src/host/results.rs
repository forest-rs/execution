// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Host allocations remain separate while argument payloads borrow the VM heap.

use super::*;
use crate::aggregates::AggDelta;
use crate::vm::{DataWork, Trap};
use core::ops::Range;

pub(crate) struct HostResults {
    pub(crate) delta: Option<AggDelta>,
    pub(crate) work: DataWork,
    used: DataWork,
    limits: DataWork,
}
impl HostResults {
    #[inline]
    pub(crate) fn new(used: DataWork, limits: DataWork) -> Self {
        Self {
            delta: None,
            work: DataWork::default(),
            used,
            limits,
        }
    }
    pub(crate) fn resolve<'a>(
        &'a self,
        base: &'a AggHeap,
        handle: AggHandle,
    ) -> (&'a AggHeap, AggHandle) {
        self.delta
            .as_ref()
            .map_or((base, handle), |delta| delta.resolve(base, handle))
    }
    fn check(&self, work: DataWork) -> Result<(), Trap> {
        let used = self.used.aggregate + self.work.aggregate;
        if work.aggregate > self.limits.aggregate - used {
            return Err(Trap::AggregateWorkExceeded {
                limit: self.limits.aggregate,
                used,
                requested: work.aggregate,
            });
        }
        let used = self.used.payload + self.work.payload;
        if work.payload > self.limits.payload - used {
            return Err(Trap::PayloadWorkExceeded {
                limit: self.limits.payload,
                used,
                requested: work.payload,
            });
        }
        Ok(())
    }
    pub(crate) fn prepaid(&mut self, work: DataWork) -> Result<(), Trap> {
        self.check(work)?;
        self.work.aggregate += work.aggregate;
        self.work.payload += work.payload;
        Ok(())
    }
    pub(crate) fn allocate(
        &mut self,
        base: &AggHeap,
        program: &Program,
        kind: AggType,
        values: Vec<Value>,
    ) -> Result<AggHandle, HostError> {
        let aggregate = u64::try_from(values.len())
            .ok()
            .and_then(|n| n.checked_add(1))
            .ok_or_else(|| HostError::custom(Trap::AggregateSizeOverflow))?;
        // Reject oversized field vectors before inspecting every payload or schema member.
        self.check(DataWork {
            aggregate,
            payload: 0,
        })
        .map_err(HostError::custom)?;
        let payload = values
            .iter()
            .try_fold(0_u64, |sum, value| {
                let len = match value {
                    Value::Bytes(b) => b.len(),
                    Value::Str(s) => s.len(),
                    _ => 0,
                };
                sum.checked_add(u64::try_from(len).ok()?)
            })
            .ok_or_else(|| HostError::custom(Trap::PayloadSizeOverflow))?;
        let work = DataWork { aggregate, payload };
        self.check(work).map_err(HostError::custom)?;
        match kind {
            AggType::Tuple { .. } => {}
            AggType::Struct { type_id } => {
                let table = &program.types;
                let ty = table
                    .structs
                    .get(type_id.0 as usize)
                    .ok_or_else(|| HostError::custom(Trap::TypeIdOutOfBounds))?;
                let types = table
                    .struct_field_types(ty)
                    .map_err(|_| HostError::custom(Trap::TypeIdOutOfBounds))?;
                if types.len() != values.len() {
                    return Err(HostError::custom(Trap::ArityMismatch));
                }
                for (value, ty) in values.iter().zip(types) {
                    value.check_type(*ty).map_err(HostError::custom)?;
                }
            }
            AggType::Array { elem_type_id } => {
                let ty = program
                    .types
                    .array_elems
                    .get(elem_type_id.0 as usize)
                    .ok_or_else(|| HostError::custom(Trap::TypeIdOutOfBounds))?;
                for value in &values {
                    value.check_type(*ty).map_err(HostError::custom)?;
                }
            }
        }
        let handle = self
            .restore_node(base, kind, values)
            .map_err(HostError::custom)?;
        self.prepaid(work)
            .expect("construction work checked before allocation");
        Ok(handle)
    }
    // Replay restores only admitted nodes; the recorded debit also includes discarded work.
    pub(crate) fn restore_node(
        &mut self,
        base: &AggHeap,
        kind: AggType,
        values: Vec<Value>,
    ) -> Result<AggHandle, AggError> {
        let delta = self.delta.get_or_insert_with(|| AggDelta::for_base(base));
        let mut overlay = delta.overlay(base);
        match kind {
            AggType::Tuple { .. } => overlay.tuple_new(values),
            AggType::Struct { type_id } => overlay.struct_new(type_id, values),
            AggType::Array { elem_type_id } => overlay.array_new(elem_type_id, values),
        }
    }
    // Keep only the empty check inline. It also proves staging is gone before HostResults
    // drops; merging actual results stays outside the ordinary host-call fast path.
    #[inline(always)]
    pub(crate) fn adopt(
        &mut self,
        heap: &mut AggHeap,
        values: &mut [Value],
        returned: bool,
    ) -> Result<Range<u32>, AggError> {
        match self.delta.take() {
            None => Ok(0..0),
            Some(delta) => adopt_delta(delta, heap, values, returned),
        }
    }
}

#[inline(never)]
fn adopt_delta(
    delta: AggDelta,
    heap: &mut AggHeap,
    values: &mut [Value],
    returned: bool,
) -> Result<Range<u32>, AggError> {
    if !returned {
        return Ok(0..0);
    }
    let start = heap.len_u32();
    let remap = delta.merge_into(heap, values)?;
    remap.remap_values_in_place(values)?;
    Ok(start..heap.len_u32())
}

impl HostContext<'_, '_> {
    /// Constructs a new immutable tuple, moving its fields into staged VM storage.
    ///
    /// Borrowed inputs remain unchanged. Handles in fields must belong to this call's heap or
    /// its earlier staged results. Successful completion adopts only reachable results; failure
    /// or cancellation discards staging. One node plus each field and immediate payload bytes
    /// share the invocation's aggregate/payload work limits. Native allocations before this call
    /// are outside those limits. A denial allocates no node and can be handled by the host.
    /// Errors preserve an `AggError` or VM `Trap` as the typed `HostError` source.
    pub fn tuple_new(&mut self, values: Vec<Value>) -> Result<AggHandle, HostError> {
        self.results.allocate(
            self.aggregates,
            self.program,
            AggType::Tuple {
                arity: u32::try_from(values.len()).unwrap_or(u32::MAX),
            },
            values,
        )
    }
    /// Constructs a struct using a nominal type from this call's program.
    /// Field arity and declared storage kinds are checked before allocation; nested contracts
    /// are checked at normal guest boundaries. Other semantics and budgets match `tuple_new`.
    pub fn struct_new(
        &mut self,
        type_id: crate::program::TypeId,
        values: Vec<Value>,
    ) -> Result<AggHandle, HostError> {
        self.results.allocate(
            self.aggregates,
            self.program,
            AggType::Struct { type_id },
            values,
        )
    }
    /// Constructs an array using an element type from this call's program.
    /// Element storage kinds are checked before allocation; other semantics match `tuple_new`.
    pub fn array_new(
        &mut self,
        elem_type_id: crate::program::ElemTypeId,
        values: Vec<Value>,
    ) -> Result<AggHandle, HostError> {
        self.results.allocate(
            self.aggregates,
            self.program,
            AggType::Array { elem_type_id },
            values,
        )
    }
    pub(super) fn resolve(&self, handle: AggHandle) -> (&AggHeap, AggHandle) {
        self.results.resolve(self.aggregates, handle)
    }
}
