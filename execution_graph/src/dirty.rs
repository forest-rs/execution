// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Dirty-tracking integration built on `invalidation`.
//!
//! This module is a thin adapter around [`invalidation`] that:
//! - interns owned [`ResourceKey`] values into small `Copy` ids (required by `invalidation`)
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

use hashbrown::HashMap;
use invalidation::intern::Interner;
use invalidation::trace::OneParentRecorder;
use invalidation::{
    Channel, CycleHandling, InternId, InvalidationTracker, LazyPolicy, TraversalScratch,
};

use crate::access::ResourceKey;

const EXECUTION_GRAPH_CHANNEL: Channel = Channel::new(0);

/// Interned key id for dirty-tracking.
///
/// `invalidation` operates on `Copy` keys. We intern [`ResourceKey`] values and use the
/// resulting compact id for all operations.
pub(crate) type DirtyKey = InternId;

/// Dirty engine keyed by interned [`ResourceKey`] values.
///
/// `invalidation` requires keys to be `Copy`, so this type uses an interner to translate
/// owned keys into compact ids.
///
/// The interner grows monotonically for the lifetime of the graph: keys are not removed.
#[derive(Debug)]
pub(crate) struct DirtyEngine {
    tracker: InvalidationTracker<DirtyKey>,
    keys: Interner<ResourceKey>,
    // Reused by non-traced drains to avoid rebuilding traversal buffers each run.
    drain_scratch: TraversalScratch<DirtyKey>,
    // Reused by scoped drains: the keys drained, and the same keys sorted for membership.
    scoped: Vec<DirtyKey>,
    scoped_sorted: Vec<DirtyKey>,
    // Reused by scoped drains: (out-of-scope dependent, drained key it depends on).
    scoped_marks: Vec<(DirtyKey, DirtyKey)>,
    // Causes of marks a scoped drain re-made outside its closure, not yet drained.
    retained: HashMap<DirtyKey, RetainedCause>,
    // Causes of retained marks taken by the most recent drain, for `explain_path`.
    consumed: HashMap<DirtyKey, RetainedCause>,
}

/// Why a scoped drain re-marked a key outside its closure.
#[derive(Clone, Debug)]
struct RetainedCause {
    /// Cause path from its root to the drained key the mark depends on, inclusive.
    prefix: Vec<DirtyKey>,
    /// Whether `prefix` was traced; `false` when only the drained key itself is known.
    traced: bool,
}

impl Default for DirtyEngine {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl DirtyEngine {
    /// Creates a new dirty engine.
    ///
    /// The engine uses a single channel (`0`) and rejects dependency cycles.
    #[must_use]
    #[inline]
    pub(crate) fn new() -> Self {
        let tracker = InvalidationTracker::with_cycle_handling(CycleHandling::Error);
        Self {
            tracker,
            keys: Interner::new(),
            drain_scratch: TraversalScratch::new(),
            scoped: Vec::new(),
            scoped_sorted: Vec::new(),
            scoped_marks: Vec::new(),
            retained: HashMap::new(),
            consumed: HashMap::new(),
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

    /// Appends every explicitly marked key (the roots of pending dirty work): node outputs to
    /// `outputs`, every other key to `others`.
    ///
    /// Marks are lazy, so the invalidated set holds exactly the keys that were marked directly;
    /// keys that are only affected through dependencies are not included.
    ///
    /// The invalidated set is a hash set, so the append order is unspecified; callers only stamp
    /// these keys, so the order does not affect results.
    #[inline]
    pub(crate) fn roots_into(&self, outputs: &mut Vec<DirtyKey>, others: &mut Vec<DirtyKey>) {
        for id in self.tracker.invalidated().iter(EXECUTION_GRAPH_CHANNEL) {
            match self.keys.get(id) {
                Some(ResourceKey::NodeOutput { .. }) => outputs.push(id),
                _ => others.push(id),
            }
        }
    }

    /// Drains dirty work in a deterministic order.
    ///
    /// The returned iterator yields key ids that are either explicitly marked dirty, or are
    /// affected by those marks via dependency propagation in the channel.
    ///
    /// The order is deterministic so callers can build stable scheduling and tests on top.
    ///
    /// Internally this reuses a retained traversal scratch buffer to reduce per-drain
    /// allocation churn on hot rerun paths.
    #[inline]
    pub(crate) fn drain(&mut self) -> impl Iterator<Item = (DirtyKey, &ResourceKey)> + '_ {
        self.consume_all_retained();
        let keys = &self.keys;
        let tracker = &mut self.tracker;
        let scratch = &mut self.drain_scratch;
        tracker
            .drain(EXECUTION_GRAPH_CHANNEL)
            .affected()
            .deterministic()
            .scratch(scratch)
            .run()
            .filter_map(move |id| keys.get(id).map(|k| (id, k)))
    }

    /// Drains dirty work in a deterministic order, recording one plausible cause path.
    ///
    /// The provided `scratch` and `trace` are reused for traversal and recording.
    #[inline]
    pub(crate) fn drain_traced<'a>(
        &'a mut self,
        scratch: &'a mut TraversalScratch<DirtyKey>,
        trace: &'a mut OneParentRecorder<DirtyKey>,
    ) -> impl Iterator<Item = (DirtyKey, &'a ResourceKey)> + 'a {
        self.consume_all_retained();
        let keys = &self.keys;
        self.tracker
            .drain(EXECUTION_GRAPH_CHANNEL)
            .affected()
            .deterministic()
            .trace(scratch, trace)
            .run()
            .filter_map(move |id| keys.get(id).map(|k| (id, k)))
    }

    /// Drains dirty work, restricted to keys within the dependency closure of `key`.
    ///
    /// This yields only dirty/affected keys that are (transitively) upstream dependencies of
    /// `key` (including `key` itself if it is affected). This is used to support targeted
    /// execution of a single node’s dependency closure without draining unrelated dirty work.
    ///
    /// Dirty work outside the closure stays pending, including dependents of drained keys; see
    /// [`Self::retain_out_of_scope`].
    ///
    /// Internally this reuses retained buffers to reduce per-drain allocation churn on hot rerun
    /// paths.
    #[inline]
    pub(crate) fn drain_within_dependencies_of(
        &mut self,
        key: DirtyKey,
    ) -> impl Iterator<Item = (DirtyKey, &ResourceKey)> + '_ {
        let mut scoped = core::mem::take(&mut self.scoped);
        scoped.clear();
        scoped.extend(
            self.tracker
                .drain(EXECUTION_GRAPH_CHANNEL)
                .affected()
                .within_dependencies_of(key)
                .deterministic()
                .scratch(&mut self.drain_scratch)
                .run(),
        );
        self.consume_retained(&scoped);
        self.retain_out_of_scope(&scoped, None);
        self.scoped = scoped;
        let keys = &self.keys;
        self.scoped
            .iter()
            .filter_map(move |&id| keys.get(id).map(|k| (id, k)))
    }

    /// Drains dirty work within the dependency closure of `key`, recording one plausible cause
    /// path.
    ///
    /// The provided `scratch` and `trace` are reused for traversal and recording. Like
    /// [`Self::drain_within_dependencies_of`], this keeps pending work outside the closure.
    #[inline]
    pub(crate) fn drain_within_dependencies_of_traced<'a>(
        &'a mut self,
        key: DirtyKey,
        scratch: &'a mut TraversalScratch<DirtyKey>,
        trace: &'a mut OneParentRecorder<DirtyKey>,
    ) -> impl Iterator<Item = (DirtyKey, &'a ResourceKey)> + 'a {
        let mut scoped = core::mem::take(&mut self.scoped);
        scoped.clear();
        scoped.extend(
            self.tracker
                .drain(EXECUTION_GRAPH_CHANNEL)
                .affected()
                .within_dependencies_of(key)
                .deterministic()
                .trace(scratch, trace)
                .run(),
        );
        self.consume_retained(&scoped);
        self.retain_out_of_scope(&scoped, Some(trace));
        self.scoped = scoped;
        let keys = &self.keys;
        self.scoped
            .iter()
            .filter_map(move |&id| keys.get(id).map(|k| (id, k)))
    }

    /// Re-marks dependents that a scoped drain would otherwise orphan.
    ///
    /// A scoped drain takes every dirty root inside the closure and expands only within it, so
    /// a key outside the closure that depends on a drained key (a sibling reading the same
    /// input, or a reader of an output the scoped run recomputes) loses its pending work. Such
    /// direct dependents are marked dirty again (see [`retain_out_of_scope_dependents`]); lazy
    /// propagation from those marks covers their own dependents on the next drain.
    ///
    /// Each new mark remembers the drained key it depends on and, when the drain was traced, that
    /// key's cause path, so a later traced drain explains the mark from its real root rather than
    /// as a root of its own. A path that starts at a retained mark this drain consumed is
    /// extended with that mark's own cause, so chained scoped drains keep the original root.
    /// When several drained keys apply, the least one wins, matching the sorted pairs; a key that
    /// is already retained keeps its earlier (older) cause.
    fn retain_out_of_scope(
        &mut self,
        drained: &[DirtyKey],
        trace: Option<&OneParentRecorder<DirtyKey>>,
    ) {
        self.scoped_marks.clear();
        retain_out_of_scope_dependents(
            &mut self.tracker,
            drained,
            &mut self.scoped_sorted,
            &mut self.scoped_marks,
        );
        for &(key, parent) in &self.scoped_marks {
            if self.retained.contains_key(&key) {
                continue;
            }
            let (path, traced) =
                match trace.and_then(|t| t.explain_path(parent, EXECUTION_GRAPH_CHANNEL)) {
                    Some(path) => (path, true),
                    None => (alloc::vec![parent], false),
                };
            // `path` starts at a root of this drain; if that root was itself a retained mark,
            // prepend the cause it carried.
            let cause = match path.first().and_then(|root| self.consumed.get(root)) {
                Some(origin) => {
                    let mut prefix = origin.prefix.clone();
                    prefix.extend_from_slice(&path);
                    // The consumed root's path is exact up to it in both cases, so the whole
                    // path is traced exactly when the origin's was.
                    RetainedCause {
                        prefix,
                        traced: origin.traced,
                    }
                }
                None => RetainedCause {
                    prefix: path,
                    traced,
                },
            };
            self.retained.insert(key, cause);
        }
    }

    /// Moves pending marks on outputs of nodes the plan will run to those outputs' readers.
    ///
    /// After the scoped drains, an output can still be marked dirty although its node is
    /// scheduled: a scoped drain retained it (a multi-output node whose other output lies inside
    /// the closure), an earlier scoped plan retained it, or it was invalidated directly. The node
    /// recomputes every output in this run, so leaving the mark would run the node again later,
    /// and with early cutoff that rerun compares equal to the value this run already wrote and
    /// hides the change from the output's readers. Each such mark is taken, and the output's
    /// dependents are marked instead, with the output appended to their cause (the retained cause
    /// when there is one, otherwise an untraced path starting at the output).
    ///
    /// One pass suffices: the dependents of an output outside the closure are themselves outside
    /// it (every output of a node shares that node's dependency set), so none of them is
    /// scheduled in this plan. `is_scheduled_output` decides which keys qualify.
    pub(crate) fn forward_scheduled_outputs(
        &mut self,
        mut is_scheduled_output: impl FnMut(&ResourceKey) -> bool,
    ) {
        let mut outputs: Vec<DirtyKey> = self
            .tracker
            .invalidated()
            .iter(EXECUTION_GRAPH_CHANNEL)
            .filter(|&key| self.keys.get(key).is_some_and(&mut is_scheduled_output))
            .collect();
        if outputs.is_empty() {
            return;
        }
        outputs.sort_unstable();
        // Take the marks: the scheduled nodes recompute these outputs in this run.
        self.tracker
            .drain(EXECUTION_GRAPH_CHANNEL)
            .invalidated_only()
            .within_keys(&outputs)
            .run()
            .for_each(drop);
        let mut dependents = Vec::new();
        for &key in &outputs {
            let cause = self.retained.remove(&key).unwrap_or_else(|| RetainedCause {
                prefix: Vec::new(),
                traced: false,
            });
            dependents.clear();
            dependents.extend(
                self.tracker
                    .graph()
                    .dependents(key, EXECUTION_GRAPH_CHANNEL),
            );
            dependents.sort_unstable();
            for &dependent in &dependents {
                self.tracker
                    .mark_with(dependent, EXECUTION_GRAPH_CHANNEL, &LazyPolicy);
                if self.retained.contains_key(&dependent) {
                    continue;
                }
                let mut prefix = cause.prefix.clone();
                prefix.push(key);
                self.retained.insert(
                    dependent,
                    RetainedCause {
                        prefix,
                        traced: cause.traced,
                    },
                );
            }
        }
    }

    /// A full drain takes every pending root, including every retained mark.
    ///
    /// Causes move to `consumed` when a drain takes their marks, before the plan runs. If the run
    /// then fails, the unrun nodes are marked dirty again as plain roots and later reports show
    /// them as their own root: the original cause of such re-marked work is not kept.
    fn consume_all_retained(&mut self) {
        self.consumed = core::mem::take(&mut self.retained);
    }

    /// A scoped drain takes the retained marks inside its closure.
    fn consume_retained(&mut self, drained: &[DirtyKey]) {
        self.consumed.clear();
        if self.retained.is_empty() {
            return;
        }
        for id in drained {
            if let Some(cause) = self.retained.remove(id) {
                self.consumed.insert(*id, cause);
            }
        }
    }

    /// Replaces `from`'s dependency set with `to`.
    ///
    /// This rejects cycles. If a cycle is detected, the dependency set is left unchanged (as
    /// implemented by `invalidation`).
    #[inline]
    pub(crate) fn set_dependencies(
        &mut self,
        from: DirtyKey,
        to: impl IntoIterator<Item = DirtyKey>,
    ) {
        let _ = self
            .tracker
            .replace_dependencies(from, EXECUTION_GRAPH_CHANNEL, to);
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

    /// Translates a traced cause path into owned [`ResourceKey`] values.
    ///
    /// Returns the path and whether it is fully traced. A path that starts at a mark retained by
    /// an earlier scoped drain is extended with that mark's recorded cause; it is fully traced
    /// only if the scoped drain was traced too.
    #[must_use]
    pub(crate) fn explain_path(
        &self,
        trace: &OneParentRecorder<DirtyKey>,
        key: DirtyKey,
    ) -> Option<(Vec<ResourceKey>, bool)> {
        let ids = trace.explain_path(key, EXECUTION_GRAPH_CHANNEL)?;
        let cause = ids.first().and_then(|root| self.consumed.get(root));
        let prefix = cause.map_or(&[][..], |c| c.prefix.as_slice());
        let mut out = Vec::with_capacity(prefix.len() + ids.len());
        for &id in prefix.iter().chain(&ids) {
            out.push(self.keys.get(id)?.clone());
        }
        Some((out, cause.is_none_or(|c| c.traced)))
    }
}

/// Marks and reports dependents of `drained` that lie outside a scoped drain.
///
/// This mirrors `invalidation`'s `DrainBuilder::retain_out_of_scope` exactly
/// (forest-rs/invalidation#5): every direct dependent of a drained key that is not itself
/// drained is marked invalidated, and each `(dependent, drained key)` edge is appended to
/// `out`, sorted. For an affected drain scoped to a dependency closure, "not drained" and
/// "outside the closure" coincide for dependents of drained keys, because the drain expands
/// every dependent inside the closure. `sorted` is scratch for membership tests.
///
/// Switch to `.retain_out_of_scope(out)` on the scoped drain once an `invalidation` release
/// with that option publishes, and delete this function.
fn retain_out_of_scope_dependents(
    tracker: &mut InvalidationTracker<DirtyKey>,
    drained: &[DirtyKey],
    sorted: &mut Vec<DirtyKey>,
    out: &mut Vec<(DirtyKey, DirtyKey)>,
) {
    sorted.clear();
    sorted.extend_from_slice(drained);
    sorted.sort_unstable();
    let start = out.len();
    let graph = tracker.graph();
    for &because in drained {
        for dependent in graph.dependents(because, EXECUTION_GRAPH_CHANNEL) {
            if sorted.binary_search(&dependent).is_err() {
                out.push((dependent, because));
            }
        }
    }
    out[start..].sort_unstable();
    for &(dependent, _) in &out[start..] {
        tracker.mark_with(dependent, EXECUTION_GRAPH_CHANNEL, &LazyPolicy);
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
        let out_key = e.intern(ResourceKey::node_output(NodeId::new(1), "out"));

        e.set_dependencies(out_key, [in_key]);

        e.mark_dirty(in_key);

        let order: Vec<_> = e.drain().map(|(id, _)| id).collect();
        assert_eq!(order, vec![in_key, out_key]);
    }
}
