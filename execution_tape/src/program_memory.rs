// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Program-scoped runtime memory.
//!
//! Owns aggregate storage for a VM or scheduler. VM runs append temporary nodes, then retain
//! only the new graph reachable from returned values: committing a run compacts its suffix,
//! a young-generation collection rooted at its outputs. A trap discards all nodes from that run.
//! Existing retained nodes stay valid until the owner explicitly clears storage or collects it.
//!
//! Both compactions share one plan-then-apply primitive. An explicit collection
//! ([`crate::vm::Vm::collect`]) plans over the whole heap from every root and preserves node
//! order, so retained nodes still never refer to a later run's suffix, and a run's start
//! checkpoint moves to just after the retained nodes that survived.

use crate::aggregates::{AggError, AggHeap, Compaction, aggregate_handle};
use crate::value::Value;
use crate::vm::HeapStats;

/// Private boundary between retained nodes and the current run's allocations.
#[derive(Clone, Copy)]
pub(crate) struct Checkpoint(u32);

impl Checkpoint {
    /// The same boundary after a collection compacted the heap with `plan`.
    pub(crate) fn relocated(self, plan: &Compaction) -> Self {
        Self(plan.boundary(self.0))
    }
}

/// Shared runtime memory for program execution.
#[derive(Clone, Debug)]
pub struct ProgramMemory {
    aggs: AggHeap,
    /// Explicit collections so far.
    collections: u64,
    /// Nodes that survived the latest collection.
    survivors: u32,
    /// Where collections wrap the handle window; lowered only by tests.
    window_limit: u32,
}

impl Default for ProgramMemory {
    fn default() -> Self {
        Self::new()
    }
}

impl ProgramMemory {
    /// Creates empty program memory.
    #[must_use]
    pub fn new() -> Self {
        Self {
            aggs: AggHeap::new(),
            collections: 0,
            survivors: 0,
            window_limit: crate::aggregates::WINDOW_LIMIT,
        }
    }

    /// Returns aggregate storage as a read-only view.
    #[must_use]
    pub fn aggs(&self) -> &AggHeap {
        &self.aggs
    }

    /// Returns mutable aggregate storage.
    pub fn aggs_mut(&mut self) -> &mut AggHeap {
        &mut self.aggs
    }

    /// Applies a whole-heap collection plan. It cannot fail.
    ///
    /// `checkpoint` is where a waiting run's aggregates begin; only survivors below it are
    /// counted as retained, since the run's commit or rollback decides the rest.
    pub(crate) fn apply_collection(&mut self, plan: &Compaction, checkpoint: Option<Checkpoint>) {
        let retained =
            checkpoint.map(|checkpoint| plan.boundary(checkpoint.0) - plan.window().start);
        self.aggs.apply_compaction(plan);
        self.collections += 1;
        self.survivors = retained.unwrap_or_else(|| self.aggs.len_u32());
    }

    /// Where collections wrap the handle window.
    pub(crate) fn window_limit(&self) -> u32 {
        self.window_limit
    }

    /// Lowers the wrap point so a test can reach it.
    #[cfg(test)]
    pub(crate) fn set_window_limit(&mut self, limit: u32) {
        self.window_limit = limit;
    }

    pub(crate) fn stats(&self, root_groups: usize) -> HeapStats {
        HeapStats {
            aggregates: self.aggs.len_u32(),
            live_after_collection: self.survivors,
            collections: self.collections,
            root_groups,
            scratch_bytes: 0,
        }
    }

    pub(crate) fn checkpoint(&self) -> Checkpoint {
        Checkpoint(self.aggs.handle_end())
    }

    pub(crate) fn rollback(&mut self, checkpoint: Checkpoint) {
        self.aggs.truncate(checkpoint.0);
    }

    pub(crate) fn commit(
        &mut self,
        checkpoint: Checkpoint,
        roots: &mut [Value],
    ) -> Result<(), AggError> {
        // Retained nodes cannot refer to future allocations. Scalar and retained-root returns
        // therefore need no graph traversal, marking storage, or handle remapping.
        if !roots
            .iter()
            .filter_map(aggregate_handle)
            .any(|handle| handle.0 >= checkpoint.0)
        {
            self.rollback(checkpoint);
            return Ok(());
        }
        self.aggs.retain_since(checkpoint.0, roots)
    }
}
