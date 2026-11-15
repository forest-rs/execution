// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Host requests and typed native effects share immutable reads and budgeted construction.

use super::*;
use crate::{
    instance::Instance,
    vm::{Arguments, budget::DataBudget},
};

/// A borrowed code-qualified value context sharing the current turn's work allowance.
///
/// Reads include staged results. Construction stages immutable nodes without invalidating
/// borrowed arguments. Admitted guest entry/completion promotes the relevant nodes; hard
/// cancellation discards them. This does not reset budgets, collect storage or execute guest
/// code. Native allocation performed before passing fields remains outside VM accounting.
pub struct ValueContext<'heap, 'work> {
    instance: &'heap Instance,
    aggregates: &'heap AggHeap,
    results: &'work mut HostResults,
    budget: &'work mut DataBudget,
}
impl fmt::Debug for ValueContext<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ValueContext")
            .field("instance", self.instance)
            .field("construction_work", &self.results.work)
            .finish_non_exhaustive()
    }
}
impl<'heap, 'work> ValueContext<'heap, 'work> {
    pub(crate) fn new(
        instance: &'heap Instance,
        aggregates: &'heap AggHeap,
        results: &'work mut HostResults,
        budget: &'work mut DataBudget,
    ) -> Self {
        Self {
            instance,
            aggregates,
            results,
            budget,
        }
    }
    /// Annotates raw response arguments with this context's exact nominal owner. Registered
    /// argument groups keep their actual owner; this never overrides registered provenance.
    pub fn arguments<'a>(&self, values: impl Into<Arguments<'a>>) -> Arguments<'a> {
        Arguments::in_instance(self.instance, values)
    }
    /// Retains borrowed values in an owned snapshot for use after this host call.
    ///
    /// Aggregate and closure-environment handles in call arguments are borrowed. The snapshot
    /// copies only their reachable graph, independently of the VM's run-local storage. Read it
    /// through [`ValueSnapshot::aggregates`], or import it into a destination heap before using
    /// its aggregate roots in another run. Host objects and function IDs retain their own domains.
    pub fn retain(&self, values: &[ValueRef<'_>]) -> Result<ValueSnapshot, AggError> {
        let values = values.iter().copied().map(ValueRef::to_value).collect();
        match &self.results.delta {
            Some(delta) => delta.snapshot(self.aggregates, values),
            None => self.aggregates.snapshot_owned(values),
        }
    }

    /// The exact code instance whose host import is being dispatched.
    /// The handle is a store-bound namespace key; mutable host state stays with the VM.
    #[must_use]
    pub fn instance(&self) -> &Instance {
        self.instance
    }

    /// Returns the verified program being executed.
    #[must_use]
    pub fn program(&self) -> &Program {
        self.instance.program().program()
    }

    /// Returns the VM aggregate heap for reading aggregate argument contents.
    ///
    /// This base heap excludes staged host results. Use the context's aggregate accessors
    /// to read both arguments and newly constructed results. Borrowed aggregates stay immutable.
    #[must_use]
    pub fn aggregates(&self) -> &AggHeap {
        self.aggregates
    }

    /// Returns the kind and shape metadata for an aggregate.
    pub fn agg_type(&self, handle: AggHandle) -> Result<AggType, AggError> {
        let (heap, handle) = self.resolve(handle);
        heap.agg_type(handle)
    }

    /// Returns tuple element `index`.
    pub fn tuple_get(&self, tuple: AggHandle, index: usize) -> Result<Value, AggError> {
        let (heap, handle) = self.resolve(tuple);
        heap.tuple_get(handle, index)
    }

    /// Returns tuple element `index` as a borrowed VM value.
    pub fn tuple_get_ref(&self, tuple: AggHandle, index: usize) -> Result<ValueRef<'_>, AggError> {
        let (heap, handle) = self.resolve(tuple);
        heap.tuple_get_ref(handle, index).map(ValueRef::from_value)
    }

    /// Returns tuple length.
    pub fn tuple_len(&self, tuple: AggHandle) -> Result<usize, AggError> {
        let (heap, handle) = self.resolve(tuple);
        heap.tuple_len(handle)
    }

    /// Returns struct field `field_index`.
    pub fn struct_get(&self, st: AggHandle, field_index: usize) -> Result<Value, AggError> {
        let (heap, handle) = self.resolve(st);
        heap.struct_get(handle, field_index)
    }

    /// Returns struct field `field_index` as a borrowed VM value.
    pub fn struct_get_ref(
        &self,
        st: AggHandle,
        field_index: usize,
    ) -> Result<ValueRef<'_>, AggError> {
        let (heap, handle) = self.resolve(st);
        heap.struct_get_ref(handle, field_index)
            .map(ValueRef::from_value)
    }

    /// Returns struct field count.
    pub fn struct_field_count(&self, st: AggHandle) -> Result<usize, AggError> {
        let (heap, handle) = self.resolve(st);
        heap.struct_field_count(handle)
    }

    /// Returns array element `index`.
    pub fn array_get(&self, arr: AggHandle, index: usize) -> Result<Value, AggError> {
        let (heap, handle) = self.resolve(arr);
        heap.array_get(handle, index)
    }

    /// Returns array element `index` as a borrowed VM value.
    pub fn array_get_ref(&self, arr: AggHandle, index: usize) -> Result<ValueRef<'_>, AggError> {
        let (heap, handle) = self.resolve(arr);
        heap.array_get_ref(handle, index).map(ValueRef::from_value)
    }

    /// Returns array length.
    pub fn array_len(&self, arr: AggHandle) -> Result<usize, AggError> {
        let (heap, handle) = self.resolve(arr);
        heap.array_len(handle)
    }
}
impl ValueContext<'_, '_> {
    /// Constructs a new immutable tuple, moving its fields into staged VM storage.
    ///
    /// Borrowed inputs remain unchanged. Handles in fields must belong to this call's heap or
    /// its earlier staged results. Successful completion adopts only reachable results; failure
    /// or cancellation discards staging. Guest reentry through a yielded request first promotes
    /// all staging with unchanged handles; unused promoted nodes live until root commit/rollback.
    /// One node plus each field and immediate payload bytes are charged immediately against
    /// the execution's aggregate/payload allowance, including while results remain staged.
    /// Native allocations before this call
    /// are outside those limits. A denial allocates no node and can be handled by the host.
    /// Errors preserve an `AggError` or VM `Trap` as the typed `HostError` source.
    pub fn tuple_new(&mut self, values: Vec<Value>) -> Result<AggHandle, HostError> {
        self.results.allocate(
            self.aggregates,
            self.instance.program().program(),
            AggType::Tuple {
                arity: u32::try_from(values.len()).unwrap_or(u32::MAX),
            },
            values,
            self.budget,
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
            self.instance.program().program(),
            AggType::Struct { type_id },
            values,
            self.budget,
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
            self.instance.program().program(),
            AggType::Array { elem_type_id },
            values,
            self.budget,
        )
    }
    pub(super) fn resolve(&self, handle: AggHandle) -> (&AggHeap, AggHandle) {
        self.results.resolve(self.aggregates, handle)
    }
}
