// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Dirty-tracking integration built on `understory_dirty`.
//!
//! This module is a thin adapter around [`understory_dirty`] that:
//! - interns owned [`ResourceKey`] values into small `Copy` ids (required by `understory_dirty`)
//! - manages a single [`Channel`] namespace for the execution graph
//! - provides helpers for marking and draining dirty keys in a deterministic order
//!
//! ## Policy and invariants
//!
//! - **Propagation is lazy by default.** Dirty marks are recorded immediately, while propagation
//!   to dependents happens during draining.
//! - **Cycles are rejected.** Graph updates that would introduce cycles are treated as errors.
//! - Keys are compared by simple structural equality at the [`ResourceKey`] level (and by id
//!   equality after interning). If you record too few dependencies, incremental execution can
//!   reuse stale results (unsound). If you record extra dependencies, incremental execution may
//!   re-run more than necessary (conservative but correct).
//!
//! This module is crate-internal and intentionally small; higher-level scheduling/reporting lives
//! in `graph.rs`.

use alloc::vec::Vec;

use understory_dirty::intern::Interner;
use understory_dirty::trace::OneParentRecorder;
use understory_dirty::{
    Channel, CycleHandling, DirtySet, DirtyTracker, EagerPolicy, InternId, LazyPolicy,
    TraversalScratch,
};

use crate::access::ResourceKey;

const EXECUTION_GRAPH_CHANNEL: Channel = Channel::new(0);

/// Interned key id for dirty-tracking.
///
/// `understory_dirty` operates on `Copy` keys. We intern [`ResourceKey`] values and use the
/// resulting compact id for all operations.
pub(crate) type DirtyKey = InternId;

/// Dirty engine keyed by interned [`ResourceKey`] values.
///
/// `understory_dirty` requires keys to be `Copy`, so this type uses an interner to translate
/// owned keys into compact ids.
///
/// The interner grows monotonically for the lifetime of the graph: keys are not removed.
#[derive(Debug, Default)]
pub(crate) struct DirtyEngine {
    tracker: DirtyTracker<DirtyKey>,
    keys: Interner<ResourceKey>,
}

impl DirtyEngine {
    /// Creates a new dirty engine.
    ///
    /// The engine uses a single channel (`0`) and rejects dependency cycles.
    #[must_use]
    #[inline]
    pub(crate) fn new() -> Self {
        let tracker = DirtyTracker::with_cycle_handling(CycleHandling::Error);
        Self {
            tracker,
            keys: Interner::new(),
        }
    }

    /// Interns `key` and returns its compact id.
    ///
    /// If the key was previously interned, returns the existing id.
    #[inline]
    pub(crate) fn intern(&mut self, key: ResourceKey) -> DirtyKey {
        self.keys.intern(key)
    }

    /// Marks `key` dirty (lazy propagation).
    ///
    /// This records the root dirty mark; dependents become eligible for execution during drain.
    #[inline]
    pub(crate) fn mark_dirty(&mut self, key: DirtyKey) {
        self.tracker
            .mark_with(key, EXECUTION_GRAPH_CHANNEL, &LazyPolicy);
    }

    /// Drains dirty work in a deterministic order.
    ///
    /// The returned iterator yields key ids that are either explicitly marked dirty, or are
    /// affected by those marks via dependency propagation in the channel.
    ///
    /// The order is deterministic so callers can build stable scheduling and tests on top.
    #[inline]
    pub(crate) fn drain(&mut self) -> impl Iterator<Item = (DirtyKey, &ResourceKey)> + '_ {
        let keys = &self.keys;
        self.tracker
            .drain(EXECUTION_GRAPH_CHANNEL)
            .affected()
            .deterministic()
            .run()
            .filter_map(move |id| keys.get(id).map(|k| (id, k)))
    }

    /// Drains dirty work, restricted to keys within the dependency closure of `key`.
    ///
    /// This yields only dirty/affected keys that are (transitively) upstream dependencies of
    /// `key` (including `key` itself if it is affected). This is used to support targeted
    /// execution of a single node’s dependency closure without draining unrelated dirty work.
    #[inline]
    pub(crate) fn drain_within_dependencies_of(
        &mut self,
        key: DirtyKey,
    ) -> impl Iterator<Item = (DirtyKey, &ResourceKey)> + '_ {
        let keys = &self.keys;
        self.tracker
            .drain(EXECUTION_GRAPH_CHANNEL)
            .affected()
            .within_dependencies_of(key)
            .deterministic()
            .run()
            .filter_map(move |id| keys.get(id).map(|k| (id, k)))
    }

    /// Replaces `from`'s dependency set with `to`.
    ///
    /// This rejects cycles. If a cycle is detected, the dependency set is left unchanged (as
    /// implemented by `understory_dirty`).
    #[inline]
    pub(crate) fn set_dependencies(
        &mut self,
        from: DirtyKey,
        to: impl IntoIterator<Item = DirtyKey>,
    ) {
        let _ = self.tracker.graph_mut().replace_dependencies(
            from,
            EXECUTION_GRAPH_CHANNEL,
            to,
            CycleHandling::Error,
        );
    }

    /// Adds a single dependency edge `from -> to`.
    ///
    /// This is a small helper used for conservative wiring before dynamic accesses refine the
    /// dependency set.
    #[inline]
    pub(crate) fn add_dependency(&mut self, from: DirtyKey, to: DirtyKey) {
        let _ = self
            .tracker
            .add_dependency(from, to, EXECUTION_GRAPH_CHANNEL);
    }

    #[allow(dead_code, reason = "used by follow-up PRs (why-reran)")]
    /// Records one “parent pointer” per visited key for explaining propagation.
    ///
    /// This is intended for “why re-ran” reporting: given a set of root dirty keys, compute the
    /// set of affected keys (using eager propagation) and record *one* plausible parent edge per
    /// visited key. The resulting spanning forest can be queried for a single cause path, not all
    /// causes.
    ///
    /// This does not mutate the live dirty state; it only traverses the current dependency graph.
    pub(crate) fn record_one_parent_causes(
        &self,
        roots: &[DirtyKey],
    ) -> OneParentRecorder<DirtyKey> {
        let mut roots: Vec<DirtyKey> = roots.to_vec();
        roots.sort();
        roots.dedup();

        let mut dirty = DirtySet::<DirtyKey>::new();
        let mut scratch = TraversalScratch::new();
        let mut rec = OneParentRecorder::<DirtyKey>::new();

        for r in roots {
            EagerPolicy.propagate_with_trace(
                r,
                EXECUTION_GRAPH_CHANNEL,
                self.tracker.graph(),
                &mut dirty,
                &mut scratch,
                &mut rec,
            );
        }

        rec
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::access::{NodeId, ResourceKey};
    use alloc::vec;

    #[test]
    fn dirty_propagates_to_dependents() {
        let mut e = DirtyEngine::new();
        let in_key = e.intern(ResourceKey::input("in"));
        let out_key = e.intern(ResourceKey::tape_output(NodeId::new(1), "out"));

        e.set_dependencies(out_key, [in_key]);

        e.mark_dirty(in_key);

        let order: Vec<_> = e.drain().map(|(id, _)| id).collect();
        assert_eq!(order, vec![in_key, out_key]);
    }
}
