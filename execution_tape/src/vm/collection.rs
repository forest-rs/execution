// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Explicit, planned, moving collection of a VM's value storage.
//!
//! A collection runs only when the embedding asks for one, with [`Vm::collect`]. It has two
//! phases.
//!
//! **Planning** reads everything that can hold a value and mutates nothing. It finds the roots,
//! the values of registered groups, and traces the aggregate heap from them. Every handle is
//! validated on the way; anything it cannot account for refuses the collection.
//!
//! **Applying** the plan cannot fail. It slides surviving aggregates, in their original order,
//! into a fresh handle window, which preserves sharing and distinct identity and leaves every
//! handle issued before it naming nothing, and rewrites every root it planned.

use super::*;
use crate::aggregates::{Compaction, PlanError, aggregate_handle};

/// Why a collection was refused. A refused collection moves and reclaims nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CollectError {
    /// A root, or a field reachable from one, names an aggregate the heap does not hold.
    ///
    /// The heap was changed behind the VM, for example through [`Vm::aggregates_mut`], or a
    /// value carried a handle from another heap.
    DanglingHandle {
        /// The missing aggregate.
        handle: AggHandle,
    },
    /// The surviving aggregates cannot be addressed.
    Aggregate(AggError),
}

impl fmt::Display for CollectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DanglingHandle { handle } => write!(
                f,
                "cannot collect: a root reaches aggregate {} which the heap does not hold",
                handle.0
            ),
            Self::Aggregate(error) => write!(f, "cannot collect: {error}"),
        }
    }
}

impl core::error::Error for CollectError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Aggregate(error) => Some(error),
            _ => None,
        }
    }
}

impl From<PlanError> for CollectError {
    fn from(error: PlanError) -> Self {
        match error {
            PlanError::Dangling(handle) => Self::DanglingHandle { handle },
            PlanError::Aggregate(error) => Self::Aggregate(error),
        }
    }
}

/// What survived a collection and what it reclaimed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Retention {
    /// Kept.
    pub live: u64,
    /// Reclaimed.
    pub reclaimed: u64,
}

impl Retention {
    fn new((live, reclaimed): (u64, u64)) -> Self {
        Self { live, reclaimed }
    }
}

/// What one collection did.
///
/// Byte counts are logical: aggregate node headers, field slots and byte/string contents. They
/// exclude allocator overhead and spare capacity, and freed capacity may stay with the VM for
/// reuse rather than return to the allocator.
///
/// The report carries no duration, since the core has no clock. Time a collection around the
/// call; its cost is proportional to the aggregates before collection (`aggregates.live +
/// aggregates.reclaimed`), the fields of the live ones and [`Self::roots`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct CollectionReport {
    /// Aggregate nodes.
    pub aggregates: Retention,
    /// Logical bytes of aggregate nodes.
    pub aggregate_bytes: Retention,
    /// Surviving aggregates whose handle changed.
    pub moved: u64,
    /// Registered groups still held by a handle.
    pub root_groups: u64,
    /// Groups whose handle was dropped, unregistered by this collection.
    pub released_groups: u64,
    /// Root references traced: registered values.
    pub roots: u64,
}

/// A VM's value storage, for deciding when to collect.
///
/// Every field is read without walking the heap.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct HeapStats {
    /// Aggregate nodes the heap holds, reachable or not.
    pub aggregates: u32,
    /// Aggregate nodes that survived the most recent collection, or zero before the first.
    ///
    /// `aggregates.saturating_sub(live_after_collection)` is what was allocated since, less what
    /// runs discarded, until the heap is cleared directly.
    pub live_after_collection: u32,
    /// Collections run so far.
    pub collections: u64,
    /// Registered groups, including ones whose handle was dropped since the last collection.
    pub root_groups: usize,
}

/// A planned collection of a whole VM: its heap compaction and how many roots it found.
pub(super) struct HeapPlan {
    compaction: Compaction,
    roots: u64,
}

impl<H: Host> Vm<H> {
    /// Reclaims every aggregate no registered group reaches, moving the survivors together.
    ///
    /// Registered groups ([`Self::root`]) are the only roots between runs. Their values are
    /// updated where aggregates move; read them with [`Self::resolve`]. Every other aggregate
    /// handle copied out of the VM before the call, including a previous run's outputs and an
    /// earlier resolution, is invalid afterwards. Groups whose handle was dropped are
    /// unregistered first, so what only they kept alive is reclaimed.
    ///
    /// Nothing collects implicitly: runs only append aggregates and keep what their outputs
    /// reach. Call this when [`Self::heap_stats`] shows enough growth. The work is explicit
    /// embedding work outside every guest budget, proportional to the heap; it has no latency
    /// bound.
    ///
    /// A refused collection moves and reclaims nothing, not even dropped groups.
    pub fn collect(&mut self) -> Result<CollectionReport, CollectError> {
        let mut handles = Vec::new();
        let plan = self.plan_collection(&mut handles, 0)?;
        Ok(self.apply_collection(&plan))
    }

    /// Plans a collection rooted at registered groups and `handles`, which other root holders
    /// contributed with `roots` references. Mutates nothing.
    pub(super) fn plan_collection(
        &self,
        handles: &mut Vec<AggHandle>,
        mut roots: u64,
    ) -> Result<HeapPlan, CollectError> {
        for values in self.roots.held() {
            roots += u64::try_from(values.len()).unwrap_or(u64::MAX);
            handles.extend(values.iter().filter_map(aggregate_handle));
        }
        let compaction = self
            .memory
            .aggs()
            .plan_collection(handles.iter().copied(), self.memory.window_limit())?;
        Ok(HeapPlan { compaction, roots })
    }

    /// Applies a heap plan and relocates registered groups. It cannot fail.
    pub(super) fn apply_collection(&mut self, plan: &HeapPlan) -> CollectionReport {
        let compaction = &plan.compaction;
        let bytes = self.memory.aggs().measure(compaction);
        self.memory.apply_collection(compaction);
        for values in self.roots.held_mut() {
            compaction
                .remap()
                .remap_values_in_place(values)
                .expect("planning traced every registered value");
        }
        let released_groups = self.roots.prune();
        CollectionReport {
            aggregates: Retention::new((
                u64::from(compaction.live()),
                u64::from(compaction.reclaimed()),
            )),
            aggregate_bytes: Retention::new(bytes),
            moved: u64::from(compaction.moved()),
            root_groups: u64::try_from(self.roots.held().count()).unwrap_or(u64::MAX),
            released_groups: u64::try_from(released_groups).unwrap_or(u64::MAX),
            roots: plan.roots,
        }
    }

    /// Reports the heap's size and collection history without walking it.
    #[must_use]
    pub fn heap_stats(&self) -> HeapStats {
        self.memory.stats(self.roots.len())
    }
}
