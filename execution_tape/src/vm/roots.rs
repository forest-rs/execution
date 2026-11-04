// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Registered value ownership is separate from aggregate storage addresses.

use super::*;
use crate::aggregates::{AggBuilder, AggRemap};
use alloc::{
    collections::BTreeSet,
    sync::{Arc, Weak},
};

/// A registered group of values in one VM, retaining their exact nominal code interpretation.
///
/// Clones share one group. Dropping the last handle makes the group reclaimable at the next
/// explicit [`Vm::collect`]; drop never runs collection. The handle does not retain the VM.
/// Resolve through [`Vm::resolve`] to borrow current values after moving collection. Copying
/// an aggregate handle out of that view does not create another root. Equality and hashing
/// compare registration identity, not the contained values.
#[derive(Clone)]
pub struct RootedValues {
    owner: Arc<()>,
    witness: Arc<()>,
    slot: usize,
}

impl PartialEq for RootedValues {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.witness, &other.witness)
    }
}
impl Eq for RootedValues {}
impl core::hash::Hash for RootedValues {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        core::hash::Hash::hash(&Arc::as_ptr(&self.witness), state);
    }
}
impl fmt::Debug for RootedValues {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RootedValues")
            .field("store", &Arc::as_ptr(&self.owner))
            .field("registration", &Arc::as_ptr(&self.witness))
            .field("slot", &self.slot)
            .finish()
    }
}

/// A VM-borrowed view of registered values and their current aggregate storage.
#[derive(Clone, Copy, Debug)]
pub struct RootedView<'a> {
    instance: &'a Instance,
    values: &'a [Value],
    heap: &'a AggHeap,
}
impl<'a> RootedView<'a> {
    /// Exact instance interpreting exposed nominal schemas in these values.
    #[must_use]
    pub fn instance(self) -> &'a Instance {
        self.instance
    }
    /// Current ABI values. Raw aggregate handles are borrowed from this storage view.
    #[must_use]
    pub fn values(self) -> &'a [Value] {
        self.values
    }
    /// Aggregate storage borrowed with the values; holding this view prevents collection.
    #[must_use]
    pub fn aggregates(self) -> &'a AggHeap {
        self.heap
    }
}

/// A retained-value operation was rejected without collecting or relocating storage.
#[derive(Clone, Debug)]
pub enum RootError {
    /// The instance or handle belongs to another VM.
    ForeignStore,
    /// The group was revoked or its registration no longer exists.
    Expired,
    /// The VM's instance identity space cannot represent another retained owner.
    CapacityExceeded,
    /// Registering another group would exceed the VM's allowance.
    Limit {
        /// Maximum live groups.
        limit: usize,
        /// Currently live groups.
        used: usize,
    },
    /// A root or one of its reachable values cannot belong to this value store.
    InvalidValue {
        /// Zero-based root value containing the problem.
        index: usize,
        /// Invalid aggregate, foreign callable/tag, or execution-local continuation.
        source: Box<Trap>,
    },
    /// Transferring the entire aggregate heap would invalidate registered values.
    HeapInUse,
}
impl fmt::Display for RootError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ForeignStore => f.write_str("retained value belongs to another VM"),
            Self::Expired => f.write_str("retained value registration has expired"),
            Self::CapacityExceeded => f.write_str("retained value owner capacity exceeded"),
            Self::Limit { limit, used } => write!(
                f,
                "retained value group limit {limit} reached ({used} live groups)"
            ),
            Self::InvalidValue { index, source } => {
                write!(f, "cannot retain value #{index}: {source}")
            }
            Self::HeapInUse => {
                f.write_str("cannot transfer aggregate storage while retained values remain live")
            }
        }
    }
}
impl core::error::Error for RootError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::InvalidValue { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Exact logical counts from an explicit full aggregate collection.
///
/// Node counts do not measure bytes or allocator capacity returned to the operating system.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CollectionStats {
    /// Registered groups still held by at least one handle.
    pub live_groups: usize,
    /// Dead groups removed from the registry by this collection.
    pub released_groups: usize,
    /// Aggregate nodes present before collection.
    pub nodes_before: u32,
    /// Reachable aggregate nodes retained after collection.
    pub nodes_after: u32,
}

struct Group {
    witness: Weak<()>,
    instance: Instance,
    values: Vec<Value>,
}
#[derive(Default)]
pub(super) struct Roots {
    owner: Option<Arc<()>>,
    groups: Vec<Option<Group>>,
}
impl Roots {
    fn prune(&mut self) -> usize {
        let mut released = 0;
        for group in &mut self.groups {
            if group
                .as_ref()
                .is_some_and(|group| group.witness.strong_count() == 0)
            {
                *group = None;
                released += 1;
            }
        }
        released
    }
    fn live(&self) -> usize {
        self.groups
            .iter()
            .flatten()
            .filter(|group| group.witness.strong_count() != 0)
            .count()
    }
    fn get(&self, roots: &RootedValues) -> Result<&Group, RootError> {
        if !self
            .owner
            .as_ref()
            .is_some_and(|owner| Arc::ptr_eq(owner, &roots.owner))
        {
            return Err(RootError::ForeignStore);
        }
        self.groups
            .get(roots.slot)
            .and_then(Option::as_ref)
            .filter(|group| group.witness.ptr_eq(&Arc::downgrade(&roots.witness)))
            .ok_or(RootError::Expired)
    }
    fn insert(
        &mut self,
        instance: Instance,
        values: Vec<Value>,
        limit: usize,
    ) -> Result<RootedValues, RootError> {
        let used = self.live();
        if used >= limit {
            return Err(RootError::Limit { limit, used });
        }
        self.prune();
        let owner = self.owner.get_or_insert_with(|| Arc::new(()));
        let witness = Arc::new(());
        let group = Group {
            witness: Arc::downgrade(&witness),
            instance,
            values,
        };
        let slot = self
            .groups
            .iter()
            .position(Option::is_none)
            .unwrap_or(self.groups.len());
        if slot == self.groups.len() {
            self.groups.push(Some(group));
        } else {
            self.groups[slot] = Some(group);
        }
        Ok(RootedValues {
            owner: owner.clone(),
            witness,
            slot,
        })
    }
    fn handles(&self, out: &mut Vec<Value>) {
        for value in self.groups.iter().flatten().flat_map(|group| &group.values) {
            if let Some(handle) = crate::aggregates::aggregate_handle(value) {
                out.push(Value::Agg(handle));
            }
        }
    }
    fn remap(&mut self, remap: &AggRemap) -> Result<(), AggError> {
        for group in self.groups.iter_mut().flatten() {
            remap.remap_values_in_place(&mut group.values)?;
        }
        Ok(())
    }
}

impl<H: Host> Vm<H> {
    /// Registers owned ABI values in this VM without copying their payloads or collecting.
    ///
    /// `instance` supplies the nominal interpretation of raw aggregate handles. Low-level
    /// callers must use their actual value owner; a recycled stale raw handle cannot reliably
    /// be diagnosed. Qualified callables retain their own code. Use independent snapshots to
    /// export values beyond this VM. Registration and validation are explicit embedding work.
    pub fn retain_values(
        &mut self,
        instance: &Instance,
        values: Vec<Value>,
    ) -> Result<RootedValues, RootError> {
        self.validate_retention(instance, &values)?;
        self.roots
            .insert(instance.clone(), values, self.limits.max_value_roots)
    }

    pub(super) fn validate_retention(
        &self,
        instance: &Instance,
        values: &[Value],
    ) -> Result<(), RootError> {
        if !self.owns_instance(instance) {
            return Err(RootError::ForeignStore);
        }
        self.check_retention_limit()?;
        for (index, value) in values.iter().enumerate() {
            self.validate_root(value)
                .map_err(|source| RootError::InvalidValue {
                    index,
                    source: Box::new(source),
                })?;
        }
        Ok(())
    }

    pub(super) fn check_retention_limit(&self) -> Result<(), RootError> {
        let used = self.roots.live();
        if used >= self.limits.max_value_roots {
            return Err(RootError::Limit {
                limit: self.limits.max_value_roots,
                used,
            });
        }
        Ok(())
    }

    pub(super) fn retain_validated(
        &mut self,
        instance: Instance,
        values: Vec<Value>,
    ) -> RootedValues {
        self.roots
            .insert(instance, values, self.limits.max_value_roots)
            .expect("exclusive prevalidated retention")
    }

    /// Resolves a durable group to its current values. Never collects or clones payloads.
    /// A foreign handle is denied before its slot is inspected. The returned borrow prevents
    /// moving collection; its lifetime does not depend on retaining the handle borrow.
    pub fn resolve(&self, roots: &RootedValues) -> Result<RootedView<'_>, RootError> {
        let group = self.roots.get(roots)?;
        Ok(RootedView {
            instance: &group.instance,
            values: &group.values,
            heap: self.memory.aggs(),
        })
    }

    /// Reclaims unrooted aggregate storage and remaps every live registered value.
    ///
    /// Raw handles copied from earlier results/views are invalid after collection. Durable
    /// `RootedValues` handles remain valid and resolve to the moved values. Collection requires
    /// exclusive VM access, so no invocation is active. This full traversal is explicit
    /// embedding work, outside guest fuel; it does not promise a latency bound. Heap capacity
    /// can be reused rather than returned to the allocator.
    pub fn collect(&mut self) -> Result<CollectionStats, AggError> {
        let released_groups = self.roots.prune();
        let nodes_before = self.memory.aggs().len_u32();
        let mut handles = Vec::new();
        self.roots.handles(&mut handles);
        if handles.is_empty() {
            self.memory.aggs_mut().clear();
        } else {
            let remap = self
                .memory
                .aggs_mut()
                .compact_since(0, &mut handles, true)?
                .expect("requested remap");
            self.roots.remap(&remap)?;
        }
        Ok(CollectionStats {
            live_groups: self.roots.live(),
            released_groups,
            nodes_before,
            nodes_after: self.memory.aggs().len_u32(),
        })
    }

    /// Append-only construction/import authority for this VM's aggregate storage.
    /// It cannot clear, replace or relocate nodes behind retained values.
    pub fn aggregate_builder(&mut self) -> AggBuilder<'_> {
        AggBuilder::new(self.memory.aggs_mut())
    }

    /// Transfers unrooted aggregate storage to an independently owned consumer.
    ///
    /// Denies the transfer while a retained group is live. Success invalidates all raw handles
    /// into this VM's old heap; the returned heap preserves those handles. This operation never
    /// copies payloads. Most embeddings should use registered values or independent snapshots.
    pub fn take_aggregates(&mut self) -> Result<AggHeap, RootError> {
        if self.roots.live() != 0 {
            return Err(RootError::HeapInUse);
        }
        self.roots.prune();
        Ok(core::mem::take(self.memory.aggs_mut()))
    }

    fn validate_root(&self, value: &Value) -> Result<(), Trap> {
        let mut seen = BTreeSet::new();
        let mut pending = vec![value];
        while let Some(value) = pending.pop() {
            match value {
                Value::Func(function) if !self.owns_instance(function.instance()) => {
                    return Err(Trap::ForeignStoreValue {
                        kind: ValueType::Func,
                    });
                }
                Value::Tag(tag) if !self.owns_instance(tag.instance()) => {
                    return Err(Trap::ForeignStoreValue {
                        kind: ValueType::Tag,
                    });
                }
                Value::Continuation(_) => {
                    return Err(Trap::AggError(AggError::ExecutionLocalValue));
                }
                Value::Closure(closure) => {
                    if !self.owns_instance(closure.func.instance()) {
                        return Err(Trap::ForeignStoreValue {
                            kind: ValueType::Closure,
                        });
                    }
                    if seen.insert(closure.env.0) {
                        pending.extend(
                            self.memory
                                .aggs()
                                .fields(closure.env)
                                .map_err(Trap::AggError)?,
                        );
                    }
                }
                Value::Agg(handle) if seen.insert(handle.0) => {
                    pending.extend(self.memory.aggs().fields(*handle).map_err(Trap::AggError)?);
                }
                _ => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
