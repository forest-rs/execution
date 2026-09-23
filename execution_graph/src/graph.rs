// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Minimal execution graph with dirty-tracked incremental re-execution.

use core::fmt;

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use hashbrown::HashMap;

use crate::access::{Access, AccessLog, HostOpId, NodeId, ResourceKey};
use crate::dirty::{DirtyEngine, DirtyKey};
use crate::dispatch::{Dispatcher, InlineDispatcher};
use crate::executor::Executor;
use crate::node_access::{
    NodeAccess, intern_host_state_key_id, intern_input_key_id, intern_opaque_host_key_id,
};
use crate::plan::{RunPlan, RunPlanTrace};
use crate::report::{NodeRunDetail, ReportDetailMask, RunDetailReport, RunSummary};

use invalidation::TraversalScratch;
use invalidation::trace::OneParentRecorder;

/// Graph errors, parameterized by the executor's node error type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphError<E> {
    /// A node id was invalid.
    BadNodeId,
    /// A node declared the same output name more than once.
    DuplicateOutput {
        /// The repeated output name.
        name: Box<str>,
    },
    /// A named node input does not exist.
    UnknownInput {
        /// Node whose input was requested.
        node: NodeId,
        /// Input name.
        name: Box<str>,
    },
    /// A named node output does not exist.
    UnknownOutput {
        /// Node whose output was requested.
        node: NodeId,
        /// Output name.
        name: Box<str>,
    },
    /// A required input binding was missing.
    MissingInput {
        /// Node that is missing the binding.
        node: NodeId,
        /// Input name.
        name: Box<str>,
    },
    /// A required upstream output was missing.
    MissingUpstreamOutput {
        /// Upstream node.
        node: NodeId,
        /// Output name.
        name: Box<str>,
    },
    /// The node produced an unexpected number of outputs.
    BadOutputArity {
        /// Node that produced outputs.
        node: NodeId,
    },
    /// The executor rejected a node definition before it was added to the graph.
    InvalidNode(E),
    /// The executor failed while running a node.
    Node {
        /// Node being executed.
        node: NodeId,
        /// Executor failure.
        source: E,
    },
    /// A report-producing run failed after collecting a partial report.
    RunReportFailed {
        /// Original execution error.
        source: Box<Self>,
        /// Report rows collected for nodes that completed before the failure.
        partial_report: RunDetailReport,
    },
}

impl<E> GraphError<E> {
    /// Returns the partial report carried by a failed report-producing run, if present.
    #[must_use]
    pub fn partial_report(&self) -> Option<&RunDetailReport> {
        match self {
            Self::RunReportFailed { partial_report, .. } => Some(partial_report),
            _ => None,
        }
    }
}

impl<E: fmt::Display> fmt::Display for GraphError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadNodeId => write!(f, "bad node id"),
            Self::DuplicateOutput { name } => write!(
                f,
                "duplicate node output name: {name}; output names must be unique within a node"
            ),
            Self::UnknownInput { node, name } => write!(
                f,
                "unknown node input: node={} input={name}; check the input_names passed to add_node(...)",
                node.as_u64()
            ),
            Self::UnknownOutput { node, name } => write!(
                f,
                "unknown node output: node={} output={name}; check the producer's output names",
                node.as_u64()
            ),
            Self::MissingInput { node, name } => {
                write!(
                    f,
                    "missing input binding: node={} input={name}; bind it with set_input_value(...) or connect an upstream output to this input",
                    node.as_u64()
                )
            }
            Self::MissingUpstreamOutput { node, name } => {
                write!(
                    f,
                    "missing upstream output: upstream_node={} output={name}; check the connect(...) output name and the producer's output names",
                    node.as_u64()
                )
            }
            Self::BadOutputArity { node } => {
                write!(
                    f,
                    "node produced unexpected output arity: node={}; produced value count must match the node's declared output names",
                    node.as_u64()
                )
            }
            Self::InvalidNode(source) => write!(f, "invalid node definition: {source}"),
            Self::Node { node, source } => {
                write!(f, "node execution failed: node={} {source}", node.as_u64())
            }
            Self::RunReportFailed {
                source,
                partial_report,
            } => write!(
                f,
                "graph run failed after collecting {} executed and {} cut-off report rows: {source}",
                partial_report.executed.len(),
                partial_report.cut_off.len()
            ),
        }
    }
}

impl<E: core::error::Error + 'static> core::error::Error for GraphError<E> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::InvalidNode(source) | Self::Node { source, .. } => Some(source),
            Self::RunReportFailed { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}

/// Stable output map for a node run.
pub type NodeOutputs<V> = BTreeMap<Box<str>, V>;

#[derive(Clone, Debug)]
pub(crate) enum Binding<V> {
    External {
        value: V,
        read_id: DirtyKey,
    },
    FromNode {
        node: NodeId,
        output: Box<str>,
        read_id: DirtyKey,
    },
}

#[derive(Debug)]
pub(crate) struct Node<X: Executor> {
    pub(crate) body: X::Node,
    pub(crate) label: Option<Box<str>>,
    pub(crate) input_names: Vec<Box<str>>,
    pub(crate) input_slots: BTreeMap<Box<str>, Vec<usize>>,
    pub(crate) inputs: Vec<Option<Binding<X::Value>>>,
    pub(crate) output_names: Vec<Box<str>>,
    pub(crate) output_ids: Vec<DirtyKey>,
    pub(crate) outputs: NodeOutputs<X::Value>,
    pub(crate) last_access: Option<AccessLog>,
    pub(crate) last_read_ids: Vec<DirtyKey>,
    pub(crate) deps_initialized: bool,
    pub(crate) run_count: u64,
}

/// Incremental execution graph over the nodes of an [`Executor`].
///
/// The graph owns node identity, input bindings, dependency tracking, dirty propagation,
/// scheduling, and reporting. The executor owns node bodies and the values that flow between
/// them.
///
/// ## Semantics
///
/// - External inputs are identified by name. A node input binding with name `"foo"` will record
///   reads of [`ResourceKey::Input("foo")`](ResourceKey::Input) when executed.
/// - To invalidate an input, call [`ExecutionGraph::invalidate_input`] with the same name string
///   that was used when binding the value via [`ExecutionGraph::set_input_value`].
/// - Additional dependency reads/writes are recorded by the executor through the
///   [`NodeAccess`] it receives for each run.
/// - Dependencies are refined dynamically: after each run, each output key’s dependency set is
///   replaced with “all reads observed during that run, minus any key the node also wrote”. A node
///   is therefore never re-triggered by its own writes — a node that reads and writes the same key
///   (a read-modify-write) reaches a fixpoint — while *other* nodes that read the written key are
///   still invalidated. Because such a key is excluded from the writer's own dependency set, even
///   an external invalidation of it will not re-run that node: treat a key a node writes as an
///   output it owns, not an input. The [`connect`](ExecutionGraph::connect) method adds
///   conservative edges to enforce initial topological ordering before the first run.
/// - [`ExecutionGraph::run_all`] / [`ExecutionGraph::run_node`] execute dirty work and return a
///   cheap executed-node summary. With early cutoff (see [`Executor::values_equal`]), scheduled
///   nodes whose reads all turn out unchanged during the run are skipped and counted separately.
/// - If you need “why re-ran” data, use [`ExecutionGraph::run_all_with_report`] /
///   [`ExecutionGraph::run_node_with_report`] with an appropriate [`ReportDetailMask`].
///   Use [`ReportDetailMask::FULL`] for the full path-rich report.
///
/// ## Access log collection
///
/// Per-node access logs are **not** collected by default. Callers that need
/// [`node_last_access`](ExecutionGraph::node_last_access) must first call
/// [`set_collect_access_log(true)`](ExecutionGraph::set_collect_access_log).
#[derive(Debug)]
pub struct ExecutionGraph<X: Executor> {
    pub(crate) executor: X,
    dirty: DirtyEngine,
    input_ids: BTreeMap<Box<str>, DirtyKey>,
    host_state_ids: HashMap<(HostOpId, u64), DirtyKey>,
    opaque_host_ids: HashMap<HostOpId, DirtyKey>,
    pub(crate) nodes: Vec<Node<X>>,
    scratch: Scratch<X::Value>,
    collect_access: bool,
}

#[derive(Debug)]
struct Scratch<V> {
    to_run: Vec<NodeId>,
    seen_stamp: Vec<u32>,
    read_ids: Vec<DirtyKey>,
    write_ids: Vec<DirtyKey>,
    args: Vec<V>,
    outputs: Vec<V>,
    stamp: u32,
    /// Keys that changed during the current plan, stamped with `changed_epoch` and indexed by
    /// the key id. Seeded with the plan's dirty roots other than node outputs; executed nodes
    /// add the outputs they changed and the keys they wrote.
    changed: Vec<u32>,
    /// Node outputs marked dirty directly for the current plan, stamped like `changed`. Their
    /// nodes always run; whether the output then changed is decided by comparing values.
    forced: Vec<u32>,
    changed_epoch: u32,
    /// Whether any executed output compared equal to its previous value in the current plan.
    /// Until one does, every scheduled node has a forced or changed key on its dependency path,
    /// so the cutoff check can be skipped.
    saw_unchanged: bool,
    root_outputs: Vec<DirtyKey>,
    root_others: Vec<DirtyKey>,
}

impl<V> Default for Scratch<V> {
    fn default() -> Self {
        Self {
            to_run: Vec::new(),
            seen_stamp: Vec::new(),
            read_ids: Vec::new(),
            write_ids: Vec::new(),
            args: Vec::new(),
            outputs: Vec::new(),
            stamp: 0,
            changed: Vec::new(),
            forced: Vec::new(),
            changed_epoch: 0,
            saw_unchanged: false,
            root_outputs: Vec::new(),
            root_others: Vec::new(),
        }
    }
}

impl<V> Scratch<V> {
    #[inline]
    fn start_drain(&mut self, node_count: usize) {
        self.to_run.clear();

        if self.seen_stamp.len() < node_count {
            self.seen_stamp.resize(node_count, 0);
        }

        // Bump the epoch; if we wrap, clear stamps to preserve correctness.
        self.stamp = self.stamp.wrapping_add(1);
        if self.stamp == 0 {
            for s in &mut self.seen_stamp {
                *s = 0;
            }
            self.stamp = 1;
        }
    }

    #[inline]
    fn take_node(&mut self, node: NodeId) -> bool {
        let Ok(index) = usize::try_from(node.as_u64()) else {
            return false;
        };
        let Some(slot) = self.seen_stamp.get_mut(index) else {
            return false;
        };
        if *slot == self.stamp {
            return false;
        }
        *slot = self.stamp;
        self.to_run.push(node);
        true
    }

    /// Starts a new plan's change tracking: forgets earlier changes, forces the node outputs in
    /// `root_outputs`, and marks the other roots in `root_others` as changed.
    #[inline]
    fn start_changes(&mut self) {
        self.changed_epoch = self.changed_epoch.wrapping_add(1);
        if self.changed_epoch == 0 {
            self.changed.fill(0);
            self.forced.fill(0);
            self.changed_epoch = 1;
        }
        self.saw_unchanged = false;
        let epoch = self.changed_epoch;
        for &key in &self.root_outputs {
            stamp(&mut self.forced, key, epoch);
        }
        for &key in &self.root_others {
            stamp(&mut self.changed, key, epoch);
        }
        self.root_outputs.clear();
        self.root_others.clear();
    }

    #[inline]
    fn mark_changed(&mut self, key: DirtyKey) {
        stamp(&mut self.changed, key, self.changed_epoch);
    }

    #[inline]
    fn is_changed(&self, key: DirtyKey) -> bool {
        is_stamped(&self.changed, key, self.changed_epoch)
    }

    #[inline]
    fn is_forced(&self, key: DirtyKey) -> bool {
        is_stamped(&self.forced, key, self.changed_epoch)
    }

    /// Canonicalizes `read_ids` in place into the set of dependencies the caller will install for
    /// this node's outputs (via [`DirtyEngine::set_dependencies`]); this method does not touch the
    /// dirty engine itself.
    ///
    /// Reads are sorted and deduped to set semantics, so access emission order does not cause
    /// spurious dependency-set "changes" across runs. Any key the node also *wrote* this run is
    /// then removed from `read_ids`: the write already marked that key dirty (so *other* nodes
    /// that read it are still invalidated), but a node must not depend on — and so be re-triggered
    /// by — its own writes, or a read-modify-write node would never reach a fixpoint.
    #[inline]
    fn finalize_node_deps(&mut self) {
        self.read_ids.sort_unstable();
        self.read_ids.dedup();
        if !self.write_ids.is_empty() {
            self.write_ids.sort_unstable();
            self.write_ids.dedup();
            let write_ids = &self.write_ids;
            self.read_ids
                .retain(|id| write_ids.binary_search(id).is_err());
        }
    }
}

#[inline]
fn stamp(stamps: &mut Vec<u32>, key: DirtyKey, epoch: u32) {
    let index = key.as_usize();
    if stamps.len() <= index {
        stamps.resize(index + 1, 0);
    }
    stamps[index] = epoch;
}

#[inline]
fn is_stamped(stamps: &[u32], key: DirtyKey, epoch: u32) -> bool {
    stamps.get(key.as_usize()).is_some_and(|&s| s == epoch)
}

impl<X: Executor> ExecutionGraph<X> {
    /// Creates an empty graph that runs its nodes with `executor`.
    #[must_use]
    pub fn new(executor: X) -> Self {
        Self {
            executor,
            dirty: DirtyEngine::new(),
            input_ids: BTreeMap::new(),
            host_state_ids: HashMap::new(),
            opaque_host_ids: HashMap::new(),
            nodes: Vec::new(),
            scratch: Scratch::default(),
            collect_access: false,
        }
    }

    /// Returns the executor.
    #[must_use]
    #[inline]
    pub fn executor(&self) -> &X {
        &self.executor
    }

    /// Returns the executor mutably.
    ///
    /// Changing executor state here does not mark anything dirty; use
    /// [`invalidate`](ExecutionGraph::invalidate) with the keys the executor reports for that
    /// state.
    #[inline]
    pub fn executor_mut(&mut self) -> &mut X {
        &mut self.executor
    }

    /// Enables or disables collection of per-node access logs.
    ///
    /// When enabled, each node's full [`AccessLog`] (bindings, executor accesses, output writes)
    /// is stored after execution and can be retrieved with [`ExecutionGraph::node_last_access`].
    /// When disabled (the default), the access log is not built, eliminating significant per-run
    /// allocation overhead.
    pub fn set_collect_access_log(&mut self, collect: bool) {
        self.collect_access = collect;
    }

    /// Returns the most recent access log for `node`, if access log collection is enabled.
    ///
    /// Returns `None` if the node has not been run or if access log collection was disabled
    /// during the last run.
    #[must_use]
    #[inline]
    pub fn node_last_access(&self, node: NodeId) -> Option<&AccessLog> {
        let index = usize::try_from(node.as_u64()).ok()?;
        self.nodes.get(index)?.last_access.as_ref()
    }

    /// Sets an advisory debug label for `node`.
    ///
    /// Labels do not affect scheduling, dependency keys, or graph identity. They are intended for
    /// reports and DOT output, where a domain name is easier to read than a raw [`NodeId`].
    ///
    /// Returns [`GraphError::BadNodeId`] for an unknown node.
    pub fn set_node_label(
        &mut self,
        node: NodeId,
        label: impl Into<Box<str>>,
    ) -> Result<(), GraphError<X::Error>> {
        let index = usize::try_from(node.as_u64()).map_err(|_| GraphError::BadNodeId)?;
        let Some(n) = self.nodes.get_mut(index) else {
            return Err(GraphError::BadNodeId);
        };
        n.label = Some(label.into());
        Ok(())
    }

    /// Clears the advisory debug label for `node`.
    ///
    /// Returns [`GraphError::BadNodeId`] for an unknown node.
    pub fn clear_node_label(&mut self, node: NodeId) -> Result<(), GraphError<X::Error>> {
        let index = usize::try_from(node.as_u64()).map_err(|_| GraphError::BadNodeId)?;
        let Some(n) = self.nodes.get_mut(index) else {
            return Err(GraphError::BadNodeId);
        };
        n.label = None;
        Ok(())
    }

    /// Returns the advisory debug label for `node`, if one was set.
    #[must_use]
    #[inline]
    pub fn node_label(&self, node: NodeId) -> Option<&str> {
        let index = usize::try_from(node.as_u64()).ok()?;
        self.nodes.get(index)?.label.as_deref()
    }

    /// Returns the executor's advisory description of `node`, if it provides one.
    #[must_use]
    #[inline]
    pub fn node_description(&self, node: NodeId) -> Option<alloc::string::String> {
        let index = usize::try_from(node.as_u64()).ok()?;
        self.executor.describe(&self.nodes.get(index)?.body)
    }

    /// Adds a node and returns its [`NodeId`].
    ///
    /// `input_names` names the node's positional inputs; the executor receives bound values in
    /// this order. `output_names` names the values the executor must produce, in order. Both are
    /// part of the dependency key space: inputs are bound by name and outputs are wired by name.
    ///
    /// Returns [`GraphError::DuplicateOutput`] if an output name repeats. Duplicate input names
    /// are permitted and alias one binding.
    pub fn add_node(
        &mut self,
        body: X::Node,
        input_names: Vec<Box<str>>,
        output_names: Vec<Box<str>>,
    ) -> Result<NodeId, GraphError<X::Error>> {
        let node = NodeId::new(u64::try_from(self.nodes.len()).unwrap_or(u64::MAX));

        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for name in &output_names {
            if !seen.insert(name.as_ref()) {
                return Err(GraphError::DuplicateOutput { name: name.clone() });
            }
        }

        // Intern output keys once at node creation time.
        let mut output_ids: Vec<DirtyKey> = Vec::with_capacity(output_names.len());
        for out_name in output_names.iter().cloned() {
            let id = self.dirty.intern(ResourceKey::node_output(node, out_name));
            self.dirty.mark_dirty(id);
            output_ids.push(id);
        }

        let mut input_slots: BTreeMap<Box<str>, Vec<usize>> = BTreeMap::new();
        for (slot, name) in input_names.iter().enumerate() {
            input_slots.entry(name.clone()).or_default().push(slot);
        }
        let input_count = input_names.len();

        let n = Node {
            body,
            label: None,
            input_names,
            input_slots,
            inputs: alloc::vec![None; input_count],
            output_names,
            output_ids,
            outputs: BTreeMap::new(),
            last_access: None,
            last_read_ids: Vec::new(),
            deps_initialized: false,
            run_count: 0,
        };

        self.nodes.push(n);
        Ok(node)
    }

    /// Binds a named input to a concrete value.
    ///
    /// The `name` is part of the dependency key space. If you later want to trigger re-execution
    /// of nodes that read this input, call [`ExecutionGraph::invalidate_input`] with the same
    /// `name` string.
    ///
    /// If a node declares duplicate input names (for example `["x", "x"]`), those slots are
    /// treated as aliases: setting `"x"` binds all matching slots.
    ///
    /// Rebinding a slot to a different key (for example from a `connect`ed output to an external
    /// value) rewires the node as [`ExecutionGraph::connect`] does: its outputs are marked dirty,
    /// so the next run executes it with the new binding and never cuts it off, and
    /// `invalidate_input(name)` reaches it even before it has read `name`. Setting a new value
    /// under the same key does not by itself schedule the node; invalidate the input to do that.
    ///
    /// Returns [`GraphError::BadNodeId`] for an unknown node or [`GraphError::UnknownInput`] for
    /// an input name that was not declared when the node was added.
    pub fn set_input_value(
        &mut self,
        node: NodeId,
        name: impl Into<Box<str>>,
        value: X::Value,
    ) -> Result<(), GraphError<X::Error>> {
        let index = usize::try_from(node.as_u64()).map_err(|_| GraphError::BadNodeId)?;
        let name: Box<str> = name.into();
        // Validate node and slot exist before interning to avoid memory churn on bad inputs.
        let Some(slots) = self
            .nodes
            .get(index)
            .and_then(|n| n.input_slots.get(name.as_ref()))
            .cloned()
        else {
            let _ = self.nodes.get(index).ok_or(GraphError::BadNodeId)?;
            return Err(GraphError::UnknownInput { node, name });
        };
        let read_id = self.intern_input_id(name.as_ref());
        let n = &mut self.nodes[index];
        let mut rebound = false;
        for slot in slots {
            if let Some(binding) = n.inputs.get_mut(slot) {
                rebound |= !matches!(
                    binding,
                    Some(Binding::External { read_id: old, .. }) if *old == read_id
                );
                *binding = Some(Binding::External {
                    value: value.clone(),
                    read_id,
                });
            }
        }
        if rebound {
            // As in `connect`: the new key is not among the reads recorded by the node's last run,
            // so treat the node as unrun (early cutoff must not trust the stale read set), add a
            // conservative edge so `invalidate_input(name)` reaches the node before it has read
            // the key, and schedule it so its outputs reflect the new binding.
            n.deps_initialized = false;
            for output_ix in 0..n.output_ids.len() {
                let dst = self.nodes[index].output_ids[output_ix];
                self.dirty.add_dependency(dst, read_id);
                self.dirty.mark_dirty(dst);
            }
        }
        Ok(())
    }

    /// Connects `from.output` into `to.input`.
    ///
    /// If `to` declares duplicate input names, all slots matching `to.input` are connected.
    ///
    /// Returns [`GraphError::BadNodeId`] for an unknown source or target node,
    /// [`GraphError::UnknownOutput`] for an output name not produced by the source node, or
    /// [`GraphError::UnknownInput`] for an input name not declared by the target node.
    pub fn connect(
        &mut self,
        from: NodeId,
        output: impl Into<Box<str>>,
        to: NodeId,
        input: impl Into<Box<str>>,
    ) -> Result<(), GraphError<X::Error>> {
        let output: Box<str> = output.into();
        let input: Box<str> = input.into();
        let from_index = usize::try_from(from.as_u64()).map_err(|_| GraphError::BadNodeId)?;
        let from_node = self.nodes.get(from_index).ok_or(GraphError::BadNodeId)?;
        if !from_node
            .output_names
            .iter()
            .any(|candidate| candidate.as_ref() == output.as_ref())
        {
            return Err(GraphError::UnknownOutput {
                node: from,
                name: output,
            });
        }
        let to_index = usize::try_from(to.as_u64()).map_err(|_| GraphError::BadNodeId)?;
        let slots = self
            .nodes
            .get(to_index)
            .ok_or(GraphError::BadNodeId)?
            .input_slots
            .get(input.as_ref())
            .cloned()
            .ok_or(GraphError::UnknownInput {
                node: to,
                name: input,
            })?;
        let read_id = self
            .dirty
            .intern(ResourceKey::node_output(from, output.clone()));
        if let Some(n) = self.nodes.get_mut(to_index) {
            for slot in slots {
                if let Some(binding) = n.inputs.get_mut(slot) {
                    *binding = Some(Binding::FromNode {
                        node: from,
                        output: output.clone(),
                        read_id,
                    });
                }
            }
        }

        // Conservative scheduling: treat wiring as a dependency edge until the next execution run
        // refines dependencies via the observed reads.
        //
        // This ensures initial runs are topologically ordered even before dependencies have been
        // observed dynamically.
        let Some(to_node) = self.nodes.get(to_index) else {
            return Err(GraphError::BadNodeId);
        };
        let output_count = to_node.output_ids.len();
        let src = read_id;
        // The new edge is not among the reads recorded by the node's last run, so treat the node as
        // unrun: its next run then replaces its dependencies wholesale, and early cutoff does not
        // skip it on the strength of a stale read set.
        self.nodes[to_index].deps_initialized = false;
        for output_ix in 0..output_count {
            let dst = self.nodes[to_index].output_ids[output_ix];
            self.dirty.add_dependency(dst, src);
            self.dirty.mark_dirty(dst);
        }
        Ok(())
    }

    /// Marks an input key dirty (propagating to dependents after dependencies are established).
    ///
    /// This marks `ResourceKey::Input(name)` dirty. For incremental scheduling to work, `name`
    /// must match the binding name used by [`ExecutionGraph::set_input_value`] (and present in a
    /// node's `input_names` list).
    #[inline]
    pub fn invalidate_input(&mut self, name: impl AsRef<str>) {
        let id = self.intern_input_id(name.as_ref());
        self.dirty.mark_dirty(id);
    }

    /// Marks `key` dirty.
    ///
    /// This is the general invalidation mechanism: you can invalidate external inputs
    /// ([`ResourceKey::Input`]), executor-managed state ([`ResourceKey::HostState`]), or
    /// conservative opaque state ([`ResourceKey::OpaqueHost`]).
    #[inline]
    pub fn invalidate(&mut self, key: ResourceKey) {
        let id = match key {
            ResourceKey::Input(name) => self.intern_input_id(name.as_ref()),
            ResourceKey::HostState { op, key } => self.intern_host_state_id(op, key),
            ResourceKey::OpaqueHost(op) => self.intern_opaque_host_id(op),
            ResourceKey::NodeOutput { .. } => self.dirty.intern(key),
        };
        self.dirty.mark_dirty(id);
    }

    #[inline]
    fn intern_input_id(&mut self, name: &str) -> DirtyKey {
        intern_input_key_id(&mut self.dirty, &mut self.input_ids, name)
    }

    #[inline]
    fn intern_host_state_id(&mut self, op: HostOpId, key: u64) -> DirtyKey {
        intern_host_state_key_id(&mut self.dirty, &mut self.host_state_ids, op, key)
    }

    #[inline]
    fn intern_opaque_host_id(&mut self, op: HostOpId) -> DirtyKey {
        intern_opaque_host_key_id(&mut self.dirty, &mut self.opaque_host_ids, op)
    }

    /// Returns the most recent outputs for `node`, if present.
    #[must_use]
    #[inline]
    pub fn node_outputs(&self, node: NodeId) -> Option<&NodeOutputs<X::Value>> {
        let index = usize::try_from(node.as_u64()).ok()?;
        Some(&self.nodes.get(index)?.outputs)
    }

    /// Returns the number of times `node` has been executed.
    #[must_use]
    #[inline]
    pub fn node_run_count(&self, node: NodeId) -> Option<u64> {
        let index = usize::try_from(node.as_u64()).ok()?;
        Some(self.nodes.get(index)?.run_count)
    }

    /// Builds a plan from all currently affected dirty work.
    #[inline]
    fn plan_all(&mut self) -> RunPlan {
        self.scratch.start_drain(self.nodes.len());
        self.dirty.roots_into(
            &mut self.scratch.root_outputs,
            &mut self.scratch.root_others,
        );
        self.scratch.start_changes();

        for (_key_id, key) in self.dirty.drain() {
            Self::schedule_node_output_key(&mut self.scratch, key);
        }

        RunPlan::all(core::mem::take(&mut self.scratch.to_run))
    }

    /// Builds a report-capable plan from all currently affected dirty work.
    #[inline]
    fn plan_all_report(&mut self, detail_mask: ReportDetailMask) -> RunPlan {
        if detail_mask.is_empty() {
            return self.plan_all();
        }

        let collect_label = detail_mask.contains(ReportDetailMask::NODE_LABEL);
        let collect_because = detail_mask.contains(ReportDetailMask::BECAUSE_OF);
        let collect_why = detail_mask.contains(ReportDetailMask::WHY_PATH);

        self.scratch.start_drain(self.nodes.len());
        self.dirty.roots_into(
            &mut self.scratch.root_outputs,
            &mut self.scratch.root_others,
        );
        self.scratch.start_changes();
        let mut node_report: Vec<Option<NodeRunDetail>> = alloc::vec![None; self.nodes.len()];

        if collect_why {
            let mut trace_scratch = TraversalScratch::<DirtyKey>::new();
            let mut trace = OneParentRecorder::<DirtyKey>::new();
            trace.clear();

            let mut scheduled: Vec<(NodeId, DirtyKey, ResourceKey)> = Vec::new();
            for (key_id, key) in self.dirty.drain_traced(&mut trace_scratch, &mut trace) {
                let ResourceKey::NodeOutput { node, .. } = key else {
                    continue;
                };
                if !self.scratch.take_node(*node) || node_report.is_empty() {
                    continue;
                }

                let Ok(index) = usize::try_from(node.as_u64()) else {
                    continue;
                };
                if index >= node_report.len() || node_report[index].is_some() {
                    continue;
                }
                scheduled.push((*node, key_id, key.clone()));
            }

            for (node, key_id, because_of) in scheduled {
                let Ok(index) = usize::try_from(node.as_u64()) else {
                    continue;
                };
                if index >= node_report.len() || node_report[index].is_some() {
                    continue;
                }

                let (why_path, why_path_traced) =
                    self.dirty.explain_path(&trace, key_id).map_or_else(
                        || (alloc::vec![because_of.clone()], Some(false)),
                        |(path, traced)| (path, Some(traced)),
                    );

                let because_of = if collect_because {
                    Some(because_of)
                } else {
                    None
                };
                node_report[index] = Some(Self::report_node_detail(
                    &self.nodes,
                    node,
                    collect_label,
                    because_of,
                    Some(why_path),
                    why_path_traced,
                ));
            }
        } else {
            for (_key_id, key) in self.dirty.drain() {
                let ResourceKey::NodeOutput { node, .. } = key else {
                    continue;
                };
                if !self.scratch.take_node(*node) || node_report.is_empty() {
                    continue;
                }

                let Ok(index) = usize::try_from(node.as_u64()) else {
                    continue;
                };
                if index >= node_report.len() || node_report[index].is_some() {
                    continue;
                }

                let because_of = if collect_because {
                    Some(key.clone())
                } else {
                    None
                };
                node_report[index] = Some(Self::report_node_detail(
                    &self.nodes,
                    *node,
                    collect_label,
                    because_of,
                    None,
                    None,
                ));
            }
        }

        let nodes = core::mem::take(&mut self.scratch.to_run);
        RunPlan::all(nodes).with_trace(RunPlanTrace::from_node_reports(node_report))
    }

    /// Builds a plan restricted to keys within the dependency closure of `node`'s outputs.
    #[inline]
    fn plan_within_dependencies_of(
        &mut self,
        node: NodeId,
    ) -> Result<RunPlan, GraphError<X::Error>> {
        let index = usize::try_from(node.as_u64()).map_err(|_| GraphError::BadNodeId)?;
        let n = self.nodes.get(index).ok_or(GraphError::BadNodeId)?;
        let output_count = n.output_ids.len();

        self.scratch.start_drain(self.nodes.len());
        self.dirty.roots_into(
            &mut self.scratch.root_outputs,
            &mut self.scratch.root_others,
        );
        self.scratch.start_changes();
        for output_ix in 0..output_count {
            let out_id = self.nodes[index].output_ids[output_ix];
            for (_key_id, key) in self.dirty.drain_within_dependencies_of(out_id) {
                Self::schedule_node_output_key(&mut self.scratch, key);
            }
        }
        self.forward_scheduled_outputs();

        Ok(RunPlan::within_dependencies_of(
            node,
            core::mem::take(&mut self.scratch.to_run),
        ))
    }

    /// Builds a report-capable plan restricted to keys within `node`'s dependency closure.
    #[inline]
    fn plan_within_dependencies_of_report(
        &mut self,
        node: NodeId,
        detail_mask: ReportDetailMask,
    ) -> Result<RunPlan, GraphError<X::Error>> {
        if detail_mask.is_empty() {
            return self.plan_within_dependencies_of(node);
        }

        let Ok(index) = usize::try_from(node.as_u64()) else {
            return Err(GraphError::BadNodeId);
        };
        let Some(n) = self.nodes.get(index) else {
            return Err(GraphError::BadNodeId);
        };
        let output_count = n.output_ids.len();
        let collect_label = detail_mask.contains(ReportDetailMask::NODE_LABEL);
        let collect_because = detail_mask.contains(ReportDetailMask::BECAUSE_OF);
        let collect_why = detail_mask.contains(ReportDetailMask::WHY_PATH);

        self.scratch.start_drain(self.nodes.len());
        self.dirty.roots_into(
            &mut self.scratch.root_outputs,
            &mut self.scratch.root_others,
        );
        self.scratch.start_changes();
        let mut node_report: Vec<Option<NodeRunDetail>> = alloc::vec![None; self.nodes.len()];

        if collect_why {
            let mut trace_scratch = TraversalScratch::<DirtyKey>::new();
            let mut trace = OneParentRecorder::<DirtyKey>::new();

            // Drain dirty keys within the dependency closure of each output, and execute nodes
            // whose output keys are affected.
            for output_ix in 0..output_count {
                let out_id = self.nodes[index].output_ids[output_ix];

                trace.clear();
                let mut newly_scheduled: Vec<(NodeId, DirtyKey, ResourceKey)> = Vec::new();

                for (key_id, key) in self.dirty.drain_within_dependencies_of_traced(
                    out_id,
                    &mut trace_scratch,
                    &mut trace,
                ) {
                    let ResourceKey::NodeOutput { node, .. } = key else {
                        continue;
                    };
                    if !self.scratch.take_node(*node) {
                        continue;
                    }
                    newly_scheduled.push((*node, key_id, key.clone()));
                }

                for (scheduled_node, key_id, because_of) in newly_scheduled {
                    let Ok(scheduled_index) = usize::try_from(scheduled_node.as_u64()) else {
                        continue;
                    };
                    if scheduled_index >= node_report.len()
                        || node_report[scheduled_index].is_some()
                    {
                        continue;
                    }

                    let (why_path, why_path_traced) =
                        self.dirty.explain_path(&trace, key_id).map_or_else(
                            || (alloc::vec![because_of.clone()], Some(false)),
                            |(path, traced)| (path, Some(traced)),
                        );

                    let because_of = if collect_because {
                        Some(because_of)
                    } else {
                        None
                    };
                    node_report[scheduled_index] = Some(Self::report_node_detail(
                        &self.nodes,
                        scheduled_node,
                        collect_label,
                        because_of,
                        Some(why_path),
                        why_path_traced,
                    ));
                }
            }
        } else {
            // Drain dirty keys within the dependency closure of each output, and execute nodes
            // whose output keys are affected.
            for output_ix in 0..output_count {
                let out_id = self.nodes[index].output_ids[output_ix];
                for (_key_id, key) in self.dirty.drain_within_dependencies_of(out_id) {
                    let ResourceKey::NodeOutput { node, .. } = key else {
                        continue;
                    };
                    if !self.scratch.take_node(*node) {
                        continue;
                    }

                    let Ok(scheduled_index) = usize::try_from(node.as_u64()) else {
                        continue;
                    };
                    if scheduled_index >= node_report.len()
                        || node_report[scheduled_index].is_some()
                    {
                        continue;
                    }

                    let because_of = if collect_because {
                        Some(key.clone())
                    } else {
                        None
                    };
                    node_report[scheduled_index] = Some(Self::report_node_detail(
                        &self.nodes,
                        *node,
                        collect_label,
                        because_of,
                        None,
                        None,
                    ));
                }
            }
        }

        self.forward_scheduled_outputs();
        let nodes = core::mem::take(&mut self.scratch.to_run);
        Ok(RunPlan::within_dependencies_of(node, nodes)
            .with_trace(RunPlanTrace::from_node_reports(node_report)))
    }

    /// Forwards pending marks on outputs of nodes this scoped plan schedules to those outputs'
    /// readers; see `DirtyEngine::forward_scheduled_outputs`.
    fn forward_scheduled_outputs(&mut self) {
        let scratch = &self.scratch;
        self.dirty.forward_scheduled_outputs(|key| {
            let ResourceKey::NodeOutput { node, .. } = key else {
                return false;
            };
            usize::try_from(node.as_u64())
                .ok()
                .and_then(|index| scratch.seen_stamp.get(index))
                .is_some_and(|&stamp| stamp == scratch.stamp)
        });
    }

    #[inline]
    fn report_node_detail(
        nodes: &[Node<X>],
        node: NodeId,
        collect_label: bool,
        because_of: Option<ResourceKey>,
        why_path: Option<Vec<ResourceKey>>,
        why_path_traced: Option<bool>,
    ) -> NodeRunDetail {
        NodeRunDetail {
            node,
            node_label: Self::report_node_label(nodes, node, collect_label),
            because_of,
            why_path,
            why_path_traced,
        }
    }

    #[inline]
    fn report_node_label(nodes: &[Node<X>], node: NodeId, collect_label: bool) -> Option<Box<str>> {
        if !collect_label {
            return None;
        }
        let index = usize::try_from(node.as_u64()).ok()?;
        nodes.get(index)?.label.clone()
    }

    #[inline]
    fn schedule_node_output_key(scratch: &mut Scratch<X::Value>, key: &ResourceKey) {
        let ResourceKey::NodeOutput { node, .. } = key else {
            return;
        };
        let _ = scratch.take_node(*node);
    }

    /// Executes a pre-built run plan without traced reporting.
    #[inline]
    fn run_plan(&mut self, plan: RunPlan) -> Result<RunSummary, GraphError<X::Error>> {
        let mut dispatcher = InlineDispatcher;
        dispatcher.dispatch(self, plan)
    }

    /// Executes a pre-built run plan and returns traced reporting data if attached.
    #[inline]
    fn run_plan_with_report(
        &mut self,
        plan: RunPlan,
    ) -> Result<RunDetailReport, GraphError<X::Error>> {
        let mut dispatcher = InlineDispatcher;
        dispatcher.dispatch_with_report(self, plan)
    }

    /// Runs all currently dirty work in dependency order and returns a cheap summary.
    ///
    /// Execution is fail-fast: if a node errors, the run stops and returns that error, but the
    /// dirty state of any not-yet-executed scheduled work is preserved so a subsequent run
    /// re-attempts it.
    pub fn run_all(&mut self) -> Result<RunSummary, GraphError<X::Error>> {
        let plan = self.plan_all();
        self.run_plan(plan)
    }

    /// Runs all currently dirty work and returns a structured report.
    ///
    /// Detail payloads are selected by `detail_mask`; this keeps heavy cause-path construction
    /// opt-in. Use [`ReportDetailMask::FULL`] for the full path-rich report.
    ///
    /// If execution fails after some nodes complete, this returns
    /// [`GraphError::RunReportFailed`] with the original error and the partial report.
    pub fn run_all_with_report(
        &mut self,
        detail_mask: ReportDetailMask,
    ) -> Result<RunDetailReport, GraphError<X::Error>> {
        let plan = self.plan_all_report(detail_mask);
        self.run_plan_with_report(plan)
    }

    /// Runs the subgraph needed to (re)compute `node`, executing only what is currently dirty.
    ///
    /// This drains only dirty keys that are within the dependency closure of `node`'s outputs.
    /// Work outside the closure stays pending for a later run, including nodes that depend on
    /// keys this run drains (a sibling reading the same invalidated input, or another reader of
    /// an output this run recomputes). A later report explains that deferred work from its
    /// original root when this run was traced; after an untraced run its path starts at the
    /// drained key it depends on and is reported as not traced.
    ///
    /// Any node this run schedules recomputes all of its outputs. An output that is still
    /// marked dirty outside the closure (a multi-output node straddling the boundary, a mark
    /// kept by an earlier scoped run, or a direct invalidation) is therefore handed to its
    /// readers, which stay pending for the next run; the node is not run again for it.
    ///
    /// Deferred work stays dirty even when the drained key it depends on turns out unchanged,
    /// so early cutoff is conservative for it: those readers run on the next run.
    ///
    /// Execution is fail-fast: if a node errors, the run stops and returns that error, but the
    /// dirty state of not-yet-executed work in the closure is preserved for a subsequent run.
    pub fn run_node(&mut self, node: NodeId) -> Result<RunSummary, GraphError<X::Error>> {
        let plan = self.plan_within_dependencies_of(node)?;
        self.run_plan(plan)
    }

    /// Runs the subgraph needed to (re)compute `node` and returns a structured report.
    ///
    /// Detail payloads are selected by `detail_mask`; this keeps heavy cause-path construction
    /// opt-in. Use [`ReportDetailMask::FULL`] for the full path-rich report.
    ///
    /// If execution fails after some nodes complete, this returns
    /// [`GraphError::RunReportFailed`] with the original error and the partial report.
    pub fn run_node_with_report(
        &mut self,
        node: NodeId,
        detail_mask: ReportDetailMask,
    ) -> Result<RunDetailReport, GraphError<X::Error>> {
        let plan = self.plan_within_dependencies_of_report(node, detail_mask)?;
        self.run_plan_with_report(plan)
    }

    /// Internal dispatch hook: executes one already-scheduled node, unless it is cut off.
    ///
    /// Returns `Ok(true)` when the node ran and `Ok(false)` when early cutoff skipped it: it has
    /// run since it was last wired, none of its outputs was marked dirty directly, and nothing
    /// it read changed during this plan.
    #[inline]
    pub(crate) fn execute_scheduled_node(
        &mut self,
        node: NodeId,
    ) -> Result<bool, GraphError<X::Error>> {
        if self.is_cut_off(node) {
            return Ok(false);
        }
        self.run_node_internal(node)?;
        Ok(true)
    }

    /// Returns whether scheduled `node` can be skipped because none of its causes changed.
    #[inline]
    fn is_cut_off(&self, node: NodeId) -> bool {
        let Some(n) = usize::try_from(node.as_u64())
            .ok()
            .and_then(|index| self.nodes.get(index))
        else {
            return false;
        };
        self.scratch.saw_unchanged
            && n.deps_initialized
            && !n.output_ids.iter().any(|&id| self.scratch.is_forced(id))
            && !n
                .last_read_ids
                .iter()
                .any(|&id| self.scratch.is_changed(id))
    }

    /// Internal dispatch hook: re-marks the output keys of `nodes` dirty.
    ///
    /// Planning drains (and clears) the scheduled dirty set up front, so when dispatch stops
    /// fail-fast on an error the un-run nodes would otherwise be left permanently clean and their
    /// pending work silently dropped. Re-marking their outputs keeps that work recoverable on the
    /// next run.
    #[inline]
    pub(crate) fn remark_scheduled_dirty(&mut self, nodes: &[NodeId]) {
        for &node in nodes {
            let Ok(index) = usize::try_from(node.as_u64()) else {
                continue;
            };
            if index >= self.nodes.len() {
                continue;
            }
            for &out_id in self.nodes[index].output_ids.iter() {
                self.dirty.mark_dirty(out_id);
            }
        }
    }

    /// Internal dispatch hook: returns a spent scheduling buffer to the scratch workspace.
    ///
    /// Dispatch takes the schedule out of the plan to execute it; handing the (cleared) buffer
    /// back here on every exit path lets the next planning pass reuse its capacity.
    #[inline]
    pub(crate) fn reclaim_schedule_buffer(&mut self, mut buf: Vec<NodeId>) {
        buf.clear();
        self.scratch.to_run = buf;
    }

    fn run_node_internal(&mut self, node: NodeId) -> Result<(), GraphError<X::Error>> {
        let node_index = usize::try_from(node.as_u64()).map_err(|_| GraphError::BadNodeId)?;
        let Some(n) = self.nodes.get(node_index) else {
            return Err(GraphError::BadNodeId);
        };

        let collect_access = self.collect_access;

        // Build args and (optionally) access log. Take the buffers out of scratch so the
        // remaining scratch fields can be borrowed disjointly by the executor's `NodeAccess`.
        let mut args = core::mem::take(&mut self.scratch.args);
        args.clear();
        let mut outputs = core::mem::take(&mut self.scratch.outputs);
        outputs.clear();
        let mut log = collect_access.then(AccessLog::new);

        self.scratch.read_ids.clear();
        self.scratch.write_ids.clear();

        for (slot, name) in n.input_names.iter().enumerate() {
            let b = n.inputs.get(slot).and_then(Option::as_ref).ok_or_else(|| {
                GraphError::MissingInput {
                    node,
                    name: name.clone(),
                }
            })?;

            match b {
                Binding::External { value: v, read_id } => {
                    self.scratch.read_ids.push(*read_id);
                    if let Some(log) = log.as_mut() {
                        log.push(Access::Read(ResourceKey::input(name.clone())));
                    }
                    args.push(v.clone());
                }
                Binding::FromNode {
                    node: up,
                    output,
                    read_id,
                } => {
                    let up_index =
                        usize::try_from(up.as_u64()).map_err(|_| GraphError::BadNodeId)?;
                    let Some(up_node) = self.nodes.get(up_index) else {
                        return Err(GraphError::BadNodeId);
                    };
                    let v = up_node.outputs.get(output).ok_or_else(|| {
                        GraphError::MissingUpstreamOutput {
                            node: *up,
                            name: output.clone(),
                        }
                    })?;
                    self.scratch.read_ids.push(*read_id);
                    if let Some(log) = log.as_mut() {
                        log.push(Access::Read(ResourceKey::node_output(*up, output.clone())));
                    }
                    args.push(v.clone());
                }
            }
        }

        // Execute, capturing the executor's accesses.
        let result = {
            let mut access = NodeAccess::new(
                &mut self.dirty,
                &mut self.input_ids,
                &mut self.host_state_ids,
                &mut self.opaque_host_ids,
                &mut self.scratch.read_ids,
                &mut self.scratch.write_ids,
                log.as_mut(),
            );
            self.executor.execute(
                &mut self.nodes[node_index].body,
                &args,
                &mut outputs,
                &mut access,
            )
        };

        // Restore the args buffer to scratch for reuse on the next run.
        self.scratch.args = args;

        if let Err(source) = result {
            outputs.clear();
            self.scratch.outputs = outputs;
            return Err(GraphError::Node { node, source });
        }

        // Map outputs.
        if outputs.len() != self.nodes[node_index].output_names.len() {
            outputs.clear();
            self.scratch.outputs = outputs;
            return Err(GraphError::BadOutputArity { node });
        }

        // Update outputs in-place when the BTreeMap is already populated (subsequent runs), and
        // record which outputs changed for early cutoff of this plan's later nodes.
        {
            let n = &mut self.nodes[node_index];
            let first_run = n.outputs.is_empty();
            for (i, v) in outputs.drain(..).enumerate() {
                let name = n.output_names[i].clone();
                if let Some(log) = log.as_mut() {
                    log.push(Access::Write(ResourceKey::node_output(node, name.clone())));
                }
                let changed = if first_run {
                    n.outputs.insert(name, v);
                    true
                } else {
                    let slot = n.outputs.get_mut(name.as_ref());
                    debug_assert!(
                        slot.is_some(),
                        "output key invariant broken: output_names[{i}] not found in outputs map"
                    );
                    match slot {
                        Some(slot) => {
                            let changed = !self.executor.values_equal(slot, &v);
                            *slot = v;
                            if !changed {
                                self.scratch.saw_unchanged = true;
                            }
                            changed
                        }
                        None => true,
                    }
                };
                if changed {
                    self.scratch.mark_changed(n.output_ids[i]);
                }
            }
        }
        // Keys this node wrote changed too: a later node in this plan that reads one must run.
        let write_ids = core::mem::take(&mut self.scratch.write_ids);
        for &id in &write_ids {
            self.scratch.mark_changed(id);
        }
        self.scratch.write_ids = write_ids;
        self.scratch.outputs = outputs;

        // Refine this node's dependency set from the reads observed during the run (dedup to set
        // semantics, then drop any key the node also wrote — see `Scratch::finalize_node_deps`).
        self.scratch.finalize_node_deps();

        let deps_changed = !self.nodes[node_index].deps_initialized
            || self.nodes[node_index].last_read_ids != self.scratch.read_ids;
        if deps_changed {
            for &out_id in self.nodes[node_index].output_ids.iter() {
                self.dirty
                    .set_dependencies(out_id, self.scratch.read_ids.iter().copied());
            }
            self.nodes[node_index].last_read_ids.clear();
            self.nodes[node_index]
                .last_read_ids
                .extend(self.scratch.read_ids.iter().copied());
            self.nodes[node_index].deps_initialized = true;
        }

        // Commit log.
        self.nodes[node_index].last_access = log;
        self.nodes[node_index].run_count = self.nodes[node_index].run_count.saturating_add(1);

        Ok(())
    }
}

#[cfg(test)]
#[path = "cutoff_tests.rs"]
mod cutoff_tests;

#[cfg(all(test, feature = "tape"))]
mod tape_tests {
    extern crate std;

    use super::*;
    use crate::access::HostOpId;
    use crate::tape::{TapeError, TapeExecutor};
    use alloc::string::ToString;
    use alloc::sync::Arc;
    use alloc::vec;
    use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
    use execution_tape::host::Host;
    use execution_tape::host::{HostContext, HostError, SigHash, ValueRef};
    use execution_tape::host::{HostSig, ResourceKeyRef, sig_hash};
    use execution_tape::program::ValueType;
    use execution_tape::value::{FuncId, Value};
    use execution_tape::verifier::VerifiedProgram;
    use execution_tape::vm::Limits;
    use execution_tape::vm::Trap;
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::rc::Rc;

    #[derive(Debug, Default)]
    struct HostNoop;

    impl Host for HostNoop {
        fn call(
            &mut self,
            _symbol: &str,
            _sig_hash: SigHash,
            _args: &[ValueRef<'_>],
            _rets: &mut [Value],
            _ctx: HostContext<'_, '_>,
        ) -> Result<u64, HostError> {
            Err(HostError::UnknownSymbol)
        }
    }

    /// A no-input program that traps at runtime (divide-by-zero).
    fn trap_program() -> (Arc<VerifiedProgram>, FuncId) {
        let mut pb = ProgramBuilder::new();
        let mut a = Asm::new();
        a.const_i64(1, 1);
        a.const_i64(2, 0);
        a.i64_div(3, 1, 2);
        a.ret(0, &[3]);
        let f = pb
            .push_function_checked(
                a,
                FunctionSig {
                    arg_types: vec![],
                    ret_types: vec![ValueType::I64],
                },
            )
            .unwrap();
        pb.set_function_output_name(f, 0, "value").unwrap();
        (Arc::new(pb.build_verified().unwrap()), f)
    }

    /// A no-input program returning the constant `v` as output "value".
    fn const_program(v: i64) -> (Arc<VerifiedProgram>, FuncId) {
        let mut pb = ProgramBuilder::new();
        let mut a = Asm::new();
        a.const_i64(1, v);
        a.ret(0, &[1]);
        let f = pb
            .push_function_checked(
                a,
                FunctionSig {
                    arg_types: vec![],
                    ret_types: vec![ValueType::I64],
                },
            )
            .unwrap();
        pb.set_function_output_name(f, 0, "value").unwrap();
        (Arc::new(pb.build_verified().unwrap()), f)
    }

    #[test]
    fn node_labels_are_advisory_metadata() {
        let (prog, entry) = const_program(7);
        let mut g = ExecutionGraph::new(TapeExecutor::new(HostNoop, Limits::default()));
        let node = g.add_tape_node(prog, entry, vec![]).unwrap();

        assert_eq!(g.node_label(node), None);
        g.set_node_label(node, "total").unwrap();
        assert_eq!(g.node_label(node), Some("total"));
        g.clear_node_label(node).unwrap();
        assert_eq!(g.node_label(node), None);

        assert_eq!(
            g.set_node_label(NodeId::new(99), "missing"),
            Err(GraphError::BadNodeId)
        );
        assert_eq!(
            g.clear_node_label(NodeId::new(99)),
            Err(GraphError::BadNodeId)
        );
    }

    #[test]
    fn graph_error_display_includes_actionable_context() {
        let bad_entry =
            GraphError::InvalidNode(TapeError::BadEntryFunc { func: FuncId(99) }).to_string();
        assert!(bad_entry.contains("f99"));
        assert!(bad_entry.contains("not in the node program"));

        let bad_arity = GraphError::InvalidNode(TapeError::BadInputArity {
            func: FuncId(1),
            expected: 2,
            actual: 1,
        })
        .to_string();
        assert!(bad_arity.contains("entry=f1"));
        assert!(bad_arity.contains("expected 2 inputs"));
        assert!(bad_arity.contains("got 1"));

        let unknown_input = GraphError::<TapeError>::UnknownInput {
            node: NodeId::new(5),
            name: "qty".into(),
        }
        .to_string();
        assert!(unknown_input.contains("node=5"));
        assert!(unknown_input.contains("input=qty"));
        assert!(unknown_input.contains("add_node"));

        let unknown_output = GraphError::<TapeError>::UnknownOutput {
            node: NodeId::new(6),
            name: "subtotal".into(),
        }
        .to_string();
        assert!(unknown_output.contains("node=6"));
        assert!(unknown_output.contains("output=subtotal"));
        assert!(unknown_output.contains("output names"));

        let missing_input = GraphError::<TapeError>::MissingInput {
            node: NodeId::new(7),
            name: "subtotal".into(),
        }
        .to_string();
        assert!(missing_input.contains("node=7"));
        assert!(missing_input.contains("input=subtotal"));
        assert!(missing_input.contains("set_input_value"));
        assert!(missing_input.contains("connect"));

        let missing_output = GraphError::<TapeError>::MissingUpstreamOutput {
            node: NodeId::new(3),
            name: "total".into(),
        }
        .to_string();
        assert!(missing_output.contains("upstream_node=3"));
        assert!(missing_output.contains("output=total"));
        assert!(missing_output.contains("output names"));

        let strict = GraphError::Node {
            node: NodeId::new(11),
            source: TapeError::StrictDepsViolation {
                symbol: "read_price".into(),
                sig_hash: SigHash(42),
            },
        }
        .to_string();
        assert!(strict.contains("node=11"));
        assert!(strict.contains("host_call=read_price"));
        assert!(strict.contains("recorded no access keys"));
        assert!(strict.contains("cannot know what invalidates it"));

        let partial_report = RunDetailReport {
            executed: vec![NodeRunDetail {
                node: NodeId::new(1),
                node_label: Some("subtotal".into()),
                because_of: Some(ResourceKey::node_output(NodeId::new(1), "value")),
                why_path: None,
                why_path_traced: None,
            }],
            cut_off: Vec::new(),
        };
        let wrapped = GraphError::<TapeError>::RunReportFailed {
            source: Box::new(GraphError::MissingInput {
                node: NodeId::new(2),
                name: "tax".into(),
            }),
            partial_report,
        };
        let wrapped_display = wrapped.to_string();
        assert!(wrapped_display.contains("1 executed and 0 cut-off report rows"));
        assert!(wrapped_display.contains("missing input binding"));
        assert_eq!(wrapped.partial_report().unwrap().executed.len(), 1);
    }

    #[test]
    fn rerun_without_invalidation_does_not_reexecute() {
        // Node A: returns constant 7 (named output "value").
        let mut pb = ProgramBuilder::new();
        let mut a = Asm::new();
        a.const_i64(1, 7);
        a.ret(0, &[1]);
        let a_node = pb
            .push_function_checked(
                a,
                FunctionSig {
                    arg_types: vec![],
                    ret_types: vec![ValueType::I64],
                },
            )
            .unwrap();
        pb.set_function_output_name(a_node, 0, "value").unwrap();

        let a_prog = Arc::new(pb.build_verified().unwrap());

        let mut g = ExecutionGraph::new(TapeExecutor::new(HostNoop, Limits::default()));
        let na = g.add_tape_node(a_prog, a_node, vec![]).unwrap();
        g.run_all().unwrap();
        let first = g.node_run_count(na).unwrap();
        g.run_all().unwrap();
        let second = g.node_run_count(na).unwrap();
        assert_eq!(first, 1);
        assert_eq!(second, 1);
    }

    #[test]
    fn run_node_leaves_unrelated_dirty_work_dirty() {
        fn make_identity_program(output_name: &str) -> (Arc<VerifiedProgram>, FuncId) {
            let mut pb = ProgramBuilder::new();
            let mut a = Asm::new();
            a.ret(0, &[1]);
            let f = pb
                .push_function_checked(
                    a,
                    FunctionSig {
                        arg_types: vec![ValueType::I64],
                        ret_types: vec![ValueType::I64],
                    },
                )
                .unwrap();
            pb.set_function_output_name(f, 0, output_name).unwrap();
            (Arc::new(pb.build_verified().unwrap()), f)
        }

        let mut g = ExecutionGraph::new(TapeExecutor::new(HostNoop, Limits::default()));
        let (a_prog, a_entry) = make_identity_program("value");
        let (b_prog, b_entry) = make_identity_program("value");

        // Target chain: A -> B
        let na = g.add_tape_node(a_prog, a_entry, vec!["a".into()]).unwrap();
        let nb = g.add_tape_node(b_prog, b_entry, vec!["b".into()]).unwrap();
        g.set_input_value(na, "a", Value::I64(1)).unwrap();
        g.connect(na, "value", nb, "b").unwrap();

        // Many unrelated chains: X_i -> Y_i
        let mut unrelated_leaves: Vec<NodeId> = Vec::new();
        for i in 0..32_u64 {
            let (x_prog, x_entry) = make_identity_program("value");
            let (y_prog, y_entry) = make_identity_program("value");
            let nx = g.add_tape_node(x_prog, x_entry, vec!["x".into()]).unwrap();
            let ny = g.add_tape_node(y_prog, y_entry, vec!["y".into()]).unwrap();
            g.set_input_value(
                nx,
                "x",
                Value::I64(10 + i64::try_from(i).unwrap_or(i64::MAX)),
            )
            .unwrap();
            g.connect(nx, "value", ny, "y").unwrap();
            unrelated_leaves.push(ny);
        }

        g.run_all().unwrap();
        assert_eq!(g.node_run_count(nb), Some(1));
        for &ny in &unrelated_leaves {
            assert_eq!(g.node_run_count(ny), Some(1));
        }

        // Dirty target chain and all unrelated chains.
        g.set_input_value(na, "a", Value::I64(2)).unwrap();
        g.invalidate_input("a");

        // This invalidates the shared input key for all unrelated chains. The key property we
        // care about is that `run_node(nb)` must not drain or run unrelated dirty work.
        g.invalidate_input("x");

        // Run only the A->B closure; unrelated chains should remain dirty and not execute.
        g.run_node(nb).unwrap();
        assert_eq!(
            g.node_outputs(nb).unwrap().get("value"),
            Some(&Value::I64(2))
        );
        assert_eq!(g.node_run_count(nb), Some(2));
        for &ny in &unrelated_leaves {
            assert_eq!(g.node_run_count(ny), Some(1));
        }

        // Unrelated dirty work should still be present.
        g.run_all().unwrap();
        for &ny in &unrelated_leaves {
            assert_eq!(g.node_run_count(ny), Some(2));
        }
    }

    #[test]
    fn run_node_with_report_includes_cause_paths() {
        fn make_identity_program(output_name: &str) -> (Arc<VerifiedProgram>, FuncId) {
            let mut pb = ProgramBuilder::new();
            let mut a = Asm::new();
            a.ret(0, &[1]);
            let f = pb
                .push_function_checked(
                    a,
                    FunctionSig {
                        arg_types: vec![ValueType::I64],
                        ret_types: vec![ValueType::I64],
                    },
                )
                .unwrap();
            pb.set_function_output_name(f, 0, output_name).unwrap();
            (Arc::new(pb.build_verified().unwrap()), f)
        }

        let mut g = ExecutionGraph::new(TapeExecutor::new(HostNoop, Limits::default()));
        let (a_prog, a_entry) = make_identity_program("value");
        let (b_prog, b_entry) = make_identity_program("value");

        let na = g.add_tape_node(a_prog, a_entry, vec!["a".into()]).unwrap();
        let nb = g.add_tape_node(b_prog, b_entry, vec!["b".into()]).unwrap();
        g.set_input_value(na, "a", Value::I64(1)).unwrap();
        g.connect(na, "value", nb, "b").unwrap();

        g.run_all().unwrap();

        g.set_input_value(na, "a", Value::I64(2)).unwrap();
        g.invalidate_input("a");

        let r = g.run_node_with_report(nb, ReportDetailMask::FULL).unwrap();
        assert_eq!(r.executed.len(), 2);
        assert_eq!(r.executed[0].node, na);
        assert_eq!(r.executed[1].node, nb);

        assert_eq!(
            r.executed[0]
                .why_path
                .as_ref()
                .expect("full report should include why_path")
                .first(),
            Some(&ResourceKey::input("a"))
        );
        assert_eq!(
            r.executed[0]
                .why_path
                .as_ref()
                .expect("full report should include why_path")
                .last(),
            Some(&ResourceKey::node_output(na, "value"))
        );
        assert_eq!(r.executed[0].why_path_traced, Some(true));

        assert_eq!(
            r.executed[1]
                .why_path
                .as_ref()
                .expect("full report should include why_path")
                .first(),
            Some(&ResourceKey::input("a"))
        );
        assert_eq!(
            r.executed[1]
                .why_path
                .as_ref()
                .expect("full report should include why_path")
                .last(),
            Some(&ResourceKey::node_output(nb, "value"))
        );
        assert_eq!(r.executed[1].why_path_traced, Some(true));
    }

    #[test]
    fn run_all_counts_executed_nodes() {
        fn make_identity_program(output_name: &str) -> (Arc<VerifiedProgram>, FuncId) {
            let mut pb = ProgramBuilder::new();
            let mut a = Asm::new();
            a.ret(0, &[1]);
            let f = pb
                .push_function_checked(
                    a,
                    FunctionSig {
                        arg_types: vec![ValueType::I64],
                        ret_types: vec![ValueType::I64],
                    },
                )
                .unwrap();
            pb.set_function_output_name(f, 0, output_name).unwrap();
            (Arc::new(pb.build_verified().unwrap()), f)
        }

        let mut g = ExecutionGraph::new(TapeExecutor::new(HostNoop, Limits::default()));
        let (a_prog, a_entry) = make_identity_program("value");
        let (b_prog, b_entry) = make_identity_program("value");

        let na = g.add_tape_node(a_prog, a_entry, vec!["a".into()]).unwrap();
        let nb = g.add_tape_node(b_prog, b_entry, vec!["b".into()]).unwrap();
        g.set_input_value(na, "a", Value::I64(1)).unwrap();
        g.connect(na, "value", nb, "b").unwrap();

        let first = g.run_all().unwrap();
        assert_eq!(first.executed_nodes, 2);

        let second = g.run_all().unwrap();
        assert_eq!(second.executed_nodes, 0);
    }

    #[test]
    fn run_node_with_report_can_skip_why_paths() {
        fn make_identity_program(output_name: &str) -> (Arc<VerifiedProgram>, FuncId) {
            let mut pb = ProgramBuilder::new();
            let mut a = Asm::new();
            a.ret(0, &[1]);
            let f = pb
                .push_function_checked(
                    a,
                    FunctionSig {
                        arg_types: vec![ValueType::I64],
                        ret_types: vec![ValueType::I64],
                    },
                )
                .unwrap();
            pb.set_function_output_name(f, 0, output_name).unwrap();
            (Arc::new(pb.build_verified().unwrap()), f)
        }

        let mut g = ExecutionGraph::new(TapeExecutor::new(HostNoop, Limits::default()));
        let (a_prog, a_entry) = make_identity_program("value");
        let (b_prog, b_entry) = make_identity_program("value");

        let na = g.add_tape_node(a_prog, a_entry, vec!["a".into()]).unwrap();
        let nb = g.add_tape_node(b_prog, b_entry, vec!["b".into()]).unwrap();
        g.set_input_value(na, "a", Value::I64(1)).unwrap();
        g.connect(na, "value", nb, "b").unwrap();

        g.run_all().unwrap();
        g.set_input_value(na, "a", Value::I64(2)).unwrap();
        g.invalidate_input("a");

        let minimal = g.run_node_with_report(nb, ReportDetailMask::NONE).unwrap();
        assert_eq!(minimal.executed.len(), 2);
        for e in &minimal.executed {
            assert!(e.because_of.is_none());
            assert!(e.why_path.is_none());
            assert!(e.why_path_traced.is_none());
        }

        g.set_input_value(na, "a", Value::I64(3)).unwrap();
        g.invalidate_input("a");

        let because_only = g
            .run_node_with_report(nb, ReportDetailMask::BECAUSE_OF)
            .unwrap();
        assert_eq!(because_only.executed.len(), 2);
        for e in &because_only.executed {
            assert!(e.because_of.is_some());
            assert!(e.why_path.is_none());
            assert!(e.why_path_traced.is_none());
        }

        g.set_input_value(na, "a", Value::I64(4)).unwrap();
        g.invalidate_input("a");

        let why_only = g
            .run_node_with_report(nb, ReportDetailMask::WHY_PATH)
            .unwrap();
        assert_eq!(why_only.executed.len(), 2);
        for e in &why_only.executed {
            assert!(e.because_of.is_none());
            assert!(e.why_path.is_some());
            assert_eq!(e.why_path_traced, Some(true));
        }

        g.set_input_value(na, "a", Value::I64(5)).unwrap();
        g.invalidate_input("a");

        let full = g
            .run_node_with_report(
                nb,
                ReportDetailMask::BECAUSE_OF | ReportDetailMask::WHY_PATH,
            )
            .unwrap();
        assert_eq!(full.executed.len(), 2);
        for e in &full.executed {
            assert!(e.because_of.is_some());
            assert!(e.why_path.is_some());
            assert_eq!(e.why_path_traced, Some(true));
        }
    }

    #[test]
    fn run_node_with_report_includes_node_labels_when_requested() {
        fn make_identity_program(output_name: &str) -> (Arc<VerifiedProgram>, FuncId) {
            let mut pb = ProgramBuilder::new();
            let mut a = Asm::new();
            a.ret(0, &[1]);
            let f = pb
                .push_function_checked(
                    a,
                    FunctionSig {
                        arg_types: vec![ValueType::I64],
                        ret_types: vec![ValueType::I64],
                    },
                )
                .unwrap();
            pb.set_function_output_name(f, 0, output_name).unwrap();
            (Arc::new(pb.build_verified().unwrap()), f)
        }

        let mut g = ExecutionGraph::new(TapeExecutor::new(HostNoop, Limits::default()));
        let (a_prog, a_entry) = make_identity_program("value");
        let (b_prog, b_entry) = make_identity_program("value");

        let na = g.add_tape_node(a_prog, a_entry, vec!["a".into()]).unwrap();
        let nb = g.add_tape_node(b_prog, b_entry, vec!["b".into()]).unwrap();
        g.set_node_label(na, "source").unwrap();
        g.set_node_label(nb, "sink").unwrap();
        g.set_input_value(na, "a", Value::I64(1)).unwrap();
        g.connect(na, "value", nb, "b").unwrap();

        g.run_all().unwrap();
        g.set_input_value(na, "a", Value::I64(2)).unwrap();
        g.invalidate_input("a");

        let labels_only = g
            .run_node_with_report(nb, ReportDetailMask::NODE_LABEL)
            .unwrap();
        assert_eq!(labels_only.executed.len(), 2);
        assert_eq!(labels_only.executed[0].node, na);
        assert_eq!(
            labels_only.executed[0].node_label.as_deref(),
            Some("source")
        );
        assert!(labels_only.executed[0].because_of.is_none());
        assert!(labels_only.executed[0].why_path.is_none());
        assert!(labels_only.executed[0].why_path_traced.is_none());
        assert_eq!(labels_only.executed[1].node, nb);
        assert_eq!(labels_only.executed[1].node_label.as_deref(), Some("sink"));
        assert!(labels_only.executed[1].because_of.is_none());
        assert!(labels_only.executed[1].why_path.is_none());
        assert!(labels_only.executed[1].why_path_traced.is_none());

        g.set_input_value(na, "a", Value::I64(3)).unwrap();
        g.invalidate_input("a");

        let why_only = g
            .run_node_with_report(nb, ReportDetailMask::WHY_PATH)
            .unwrap();
        assert_eq!(why_only.executed.len(), 2);
        for e in &why_only.executed {
            assert!(e.node_label.is_none());
            assert!(e.because_of.is_none());
            assert!(e.why_path.is_some());
            assert_eq!(e.why_path_traced, Some(true));
        }
    }

    #[test]
    fn strict_deps_rejects_host_calls_without_accesses() {
        #[derive(Debug, Default)]
        struct HostNoAccess;

        impl Host for HostNoAccess {
            fn call(
                &mut self,
                symbol: &str,
                _sig_hash: SigHash,
                _args: &[ValueRef<'_>],
                rets: &mut [Value],
                _ctx: HostContext<'_, '_>,
            ) -> Result<u64, HostError> {
                if symbol != "no_access" {
                    return Err(HostError::UnknownSymbol);
                }
                rets[0] = Value::I64(7);
                Ok(0)
            }
        }

        let mut pb = ProgramBuilder::new();
        let host_sig = pb.host_sig_for(
            "no_access",
            HostSig {
                args: vec![ValueType::I64],
                rets: vec![ValueType::I64],
            },
        );

        let mut a = Asm::new();
        a.const_i64(1, 42);
        a.host_call(0, host_sig, 0, &[1], &[2]);
        a.ret(0, &[2]);

        let f = pb
            .push_function_checked(
                a,
                FunctionSig {
                    arg_types: vec![],
                    ret_types: vec![ValueType::I64],
                },
            )
            .unwrap();
        pb.set_function_output_name(f, 0, "value").unwrap();

        let prog = Arc::new(pb.build_verified().unwrap());

        let mut g = ExecutionGraph::new(TapeExecutor::new(HostNoAccess, Limits::default()));
        let n = g.add_tape_node(prog, f, vec![]).unwrap();
        g.executor_mut().set_strict_deps(true);

        assert_eq!(
            g.run_all(),
            Err(GraphError::Node {
                node: n,
                source: TapeError::StrictDepsViolation {
                    symbol: "no_access".into(),
                    sig_hash: sig_hash(&HostSig {
                        args: vec![ValueType::I64],
                        rets: vec![ValueType::I64],
                    }),
                },
            })
        );
    }

    #[test]
    fn strict_deps_rejects_host_call_whose_only_access_is_an_ignored_input_write() {
        // Writes to graph-owned Input keys are ignored (no dependency, no invalidation, no log).
        // In strict-deps mode such a write must NOT count as "this host call recorded an access":
        // a call whose only event is an ignored Input write reports nothing usable and must trip a
        // StrictDepsViolation, just like a call that records nothing at all.
        #[derive(Debug, Default)]
        struct InputWriteOnly;

        impl Host for InputWriteOnly {
            fn call(
                &mut self,
                symbol: &str,
                _sig_hash: SigHash,
                _args: &[ValueRef<'_>],
                rets: &mut [Value],
                mut ctx: HostContext<'_, '_>,
            ) -> Result<u64, HostError> {
                if symbol != "write_input" {
                    return Err(HostError::UnknownSymbol);
                }
                ctx.record_write(ResourceKeyRef::Input("x"));
                rets[0] = Value::I64(7);
                Ok(0)
            }
        }

        let mut pb = ProgramBuilder::new();
        let host_sig = pb.host_sig_for(
            "write_input",
            HostSig {
                args: vec![ValueType::I64],
                rets: vec![ValueType::I64],
            },
        );

        let mut a = Asm::new();
        a.const_i64(1, 42);
        a.host_call(0, host_sig, 0, &[1], &[2]);
        a.ret(0, &[2]);

        let f = pb
            .push_function_checked(
                a,
                FunctionSig {
                    arg_types: vec![],
                    ret_types: vec![ValueType::I64],
                },
            )
            .unwrap();
        pb.set_function_output_name(f, 0, "value").unwrap();

        let prog = Arc::new(pb.build_verified().unwrap());

        let mut g = ExecutionGraph::new(TapeExecutor::new(InputWriteOnly, Limits::default()));
        let n = g.add_tape_node(prog, f, vec![]).unwrap();
        g.executor_mut().set_strict_deps(true);

        assert_eq!(
            g.run_all(),
            Err(GraphError::Node {
                node: n,
                source: TapeError::StrictDepsViolation {
                    symbol: "write_input".into(),
                    sig_hash: sig_hash(&HostSig {
                        args: vec![ValueType::I64],
                        rets: vec![ValueType::I64],
                    }),
                },
            })
        );
    }

    #[test]
    fn run_all_errors_on_missing_input_binding() {
        let mut pb = ProgramBuilder::new();
        let mut a = Asm::new();
        a.ret(0, &[1]);
        let f = pb
            .push_function_checked(
                a,
                FunctionSig {
                    arg_types: vec![ValueType::I64],
                    ret_types: vec![ValueType::I64],
                },
            )
            .unwrap();
        pb.set_function_output_name(f, 0, "value").unwrap();
        let prog = Arc::new(pb.build_verified().unwrap());

        let mut g = ExecutionGraph::new(TapeExecutor::new(HostNoop, Limits::default()));
        let n = g.add_tape_node(prog, f, vec!["in".into()]).unwrap();

        assert_eq!(
            g.run_all(),
            Err(GraphError::MissingInput {
                node: n,
                name: "in".into()
            })
        );
    }

    #[test]
    fn run_all_preserves_vm_trap_info() {
        let (prog, f) = trap_program();

        let mut g = ExecutionGraph::new(TapeExecutor::new(HostNoop, Limits::default()));
        let n = g.add_tape_node(prog, f, vec![]).unwrap();

        let Err(GraphError::Node {
            node,
            source: TapeError::Trap(trap),
        }) = g.run_all()
        else {
            panic!("divide-by-zero should surface as a graph trap");
        };
        assert_eq!(node, n);
        assert_eq!(trap.func, f);
        assert_eq!(trap.trap, Trap::DivByZero);
    }

    #[test]
    fn run_all_trap_keeps_independent_node_recoverable() {
        // node0 traps; node1 is an independent constant scheduled after it. A trap mid-pass must
        // not silently discard node1's pending dirty work.
        let (trap_prog, trap_f) = trap_program();
        let (const_prog, const_f) = const_program(42);

        let mut g = ExecutionGraph::new(TapeExecutor::new(HostNoop, Limits::default()));
        let node0 = g.add_tape_node(trap_prog, trap_f, vec![]).unwrap();
        let node1 = g.add_tape_node(const_prog, const_f, vec![]).unwrap();

        // node0 is scheduled first and traps; fail-fast leaves node1 unrun.
        assert!(matches!(g.run_all(), Err(GraphError::Node { .. })));
        assert_eq!(g.node_run_count(node0), Some(0));
        assert_eq!(
            g.node_run_count(node1),
            Some(0),
            "precondition: node1 must not have run in the trapping pass"
        );

        // node1's dirty state survived: a targeted re-run executes it and produces its value.
        let summary = g.run_node(node1).unwrap();
        assert_eq!(summary.executed_nodes, 1);
        assert_eq!(
            g.node_outputs(node1).and_then(|o| o.get("value")),
            Some(&Value::I64(42))
        );
    }

    #[test]
    fn run_all_trap_does_not_remark_already_executed_nodes() {
        // Scheduled order is [node_a, node0, node_c]: node_a runs, node0 traps, node_c is unrun.
        // The fix must re-mark only the failed node and the unrun tail, never the node that
        // already executed successfully.
        let (a_prog, a_f) = const_program(7);
        let (trap_prog, trap_f) = trap_program();
        let (c_prog, c_f) = const_program(99);

        let mut g = ExecutionGraph::new(TapeExecutor::new(HostNoop, Limits::default()));
        let node_a = g.add_tape_node(a_prog, a_f, vec![]).unwrap();
        let _node0 = g.add_tape_node(trap_prog, trap_f, vec![]).unwrap();
        let node_c = g.add_tape_node(c_prog, c_f, vec![]).unwrap();

        assert!(matches!(g.run_all(), Err(GraphError::Node { .. })));
        assert_eq!(
            g.node_run_count(node_a),
            Some(1),
            "precondition: node_a must have executed before the trap"
        );

        // node_a already ran and must not have been re-marked, so a targeted re-run is a no-op.
        assert_eq!(g.run_node(node_a).unwrap().executed_nodes, 0);
        assert_eq!(g.node_run_count(node_a), Some(1));

        // node_c was unrun and re-marked, so it remains recoverable.
        assert_eq!(g.run_node(node_c).unwrap().executed_nodes, 1);
        assert_eq!(
            g.node_outputs(node_c).and_then(|o| o.get("value")),
            Some(&Value::I64(99))
        );
    }

    #[test]
    fn run_node_trap_keeps_closure_sibling_recoverable() {
        // target depends on node0 (traps) and node_sib (independent constant). Running target's
        // closure traps on node0; node_sib must remain recoverable rather than being dropped.
        fn passthrough2_program() -> (Arc<VerifiedProgram>, FuncId) {
            let mut pb = ProgramBuilder::new();
            let mut a = Asm::new();
            a.i64_add(3, 1, 2);
            a.ret(0, &[3]);
            let f = pb
                .push_function_checked(
                    a,
                    FunctionSig {
                        arg_types: vec![ValueType::I64, ValueType::I64],
                        ret_types: vec![ValueType::I64],
                    },
                )
                .unwrap();
            pb.set_function_output_name(f, 0, "value").unwrap();
            (Arc::new(pb.build_verified().unwrap()), f)
        }

        let (trap_prog, trap_f) = trap_program();
        let (sib_prog, sib_f) = const_program(42);
        let (tgt_prog, tgt_f) = passthrough2_program();

        let mut g = ExecutionGraph::new(TapeExecutor::new(HostNoop, Limits::default()));
        let node0 = g.add_tape_node(trap_prog, trap_f, vec![]).unwrap();
        let node_sib = g.add_tape_node(sib_prog, sib_f, vec![]).unwrap();
        let target = g
            .add_tape_node(tgt_prog, tgt_f, vec!["x".into(), "y".into()])
            .unwrap();
        g.connect(node0, "value", target, "x").unwrap();
        g.connect(node_sib, "value", target, "y").unwrap();

        // node0 is scheduled before node_sib inside target's closure and traps.
        assert!(matches!(g.run_node(target), Err(GraphError::Node { .. })));
        assert_eq!(
            g.node_run_count(node_sib),
            Some(0),
            "precondition: node_sib must not have run in the trapping pass"
        );

        // node_sib's dirty state survived the closure trap and re-runs cleanly.
        let summary = g.run_node(node_sib).unwrap();
        assert_eq!(summary.executed_nodes, 1);
        assert_eq!(
            g.node_outputs(node_sib).and_then(|o| o.get("value")),
            Some(&Value::I64(42))
        );
    }

    #[test]
    fn graph_builder_errors_on_bad_entry_func() {
        let mut pb = ProgramBuilder::new();
        let mut a = Asm::new();
        a.ret(0, &[]);
        pb.push_function_checked(
            a,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![],
            },
        )
        .unwrap();
        let prog = Arc::new(pb.build_verified().unwrap());

        let mut g = ExecutionGraph::new(TapeExecutor::new(HostNoop, Limits::default()));
        assert_eq!(
            g.add_tape_node(prog, FuncId(99), vec![]),
            Err(GraphError::InvalidNode(TapeError::BadEntryFunc {
                func: FuncId(99)
            }))
        );
    }

    #[test]
    fn graph_builder_errors_on_input_arity_mismatch() {
        let mut pb = ProgramBuilder::new();
        let mut a = Asm::new();
        a.ret(0, &[1]);
        let f = pb
            .push_function_checked(
                a,
                FunctionSig {
                    arg_types: vec![ValueType::I64],
                    ret_types: vec![ValueType::I64],
                },
            )
            .unwrap();
        let prog = Arc::new(pb.build_verified().unwrap());

        let mut g = ExecutionGraph::new(TapeExecutor::new(HostNoop, Limits::default()));
        assert_eq!(
            g.add_tape_node(prog, f, vec![]),
            Err(GraphError::InvalidNode(TapeError::BadInputArity {
                func: f,
                expected: 1,
                actual: 0,
            }))
        );
    }

    #[test]
    fn set_input_value_errors_on_unknown_input() {
        let mut pb = ProgramBuilder::new();
        let mut a = Asm::new();
        a.ret(0, &[1]);
        let f = pb
            .push_function_checked(
                a,
                FunctionSig {
                    arg_types: vec![ValueType::I64],
                    ret_types: vec![ValueType::I64],
                },
            )
            .unwrap();
        let prog = Arc::new(pb.build_verified().unwrap());

        let mut g = ExecutionGraph::new(TapeExecutor::new(HostNoop, Limits::default()));
        let n = g.add_tape_node(prog, f, vec!["qty".into()]).unwrap();

        assert_eq!(
            g.set_input_value(n, "unit_price", Value::I64(10)),
            Err(GraphError::UnknownInput {
                node: n,
                name: "unit_price".into(),
            })
        );
    }

    #[test]
    fn connect_errors_on_unknown_names() {
        fn make_const_program(output_name: &str, v: i64) -> (Arc<VerifiedProgram>, FuncId) {
            let mut pb = ProgramBuilder::new();
            let mut a = Asm::new();
            a.const_i64(1, v);
            a.ret(0, &[1]);
            let f = pb
                .push_function_checked(
                    a,
                    FunctionSig {
                        arg_types: vec![],
                        ret_types: vec![ValueType::I64],
                    },
                )
                .unwrap();
            pb.set_function_output_name(f, 0, output_name).unwrap();
            (Arc::new(pb.build_verified().unwrap()), f)
        }

        fn make_identity_program(output_name: &str) -> (Arc<VerifiedProgram>, FuncId) {
            let mut pb = ProgramBuilder::new();
            let mut a = Asm::new();
            a.ret(0, &[1]);
            let f = pb
                .push_function_checked(
                    a,
                    FunctionSig {
                        arg_types: vec![ValueType::I64],
                        ret_types: vec![ValueType::I64],
                    },
                )
                .unwrap();
            pb.set_function_output_name(f, 0, output_name).unwrap();
            (Arc::new(pb.build_verified().unwrap()), f)
        }

        let (a_prog, a_entry) = make_const_program("value", 7);
        let (b_prog, b_entry) = make_identity_program("value");

        let mut g = ExecutionGraph::new(TapeExecutor::new(HostNoop, Limits::default()));
        let na = g.add_tape_node(a_prog, a_entry, vec![]).unwrap();
        let nb = g.add_tape_node(b_prog, b_entry, vec!["x".into()]).unwrap();

        assert_eq!(
            g.connect(na, "does_not_exist", nb, "x"),
            Err(GraphError::UnknownOutput {
                node: na,
                name: "does_not_exist".into(),
            })
        );

        assert_eq!(
            g.connect(na, "value", nb, "does_not_exist"),
            Err(GraphError::UnknownInput {
                node: nb,
                name: "does_not_exist".into()
            })
        );
    }

    #[test]
    fn invalidating_host_state_reruns_dependent_nodes() {
        #[derive(Clone)]
        struct KvHost {
            kv: Rc<RefCell<BTreeMap<u64, i64>>>,
            get_sig: SigHash,
        }

        impl Host for KvHost {
            fn call(
                &mut self,
                symbol: &str,
                sig_hash: SigHash,
                args: &[ValueRef<'_>],
                rets: &mut [Value],
                mut ctx: HostContext<'_, '_>,
            ) -> Result<u64, HostError> {
                if symbol != "kv.get" {
                    return Err(HostError::UnknownSymbol);
                }
                if sig_hash != self.get_sig {
                    return Err(HostError::SignatureMismatch);
                }
                let [ValueRef::U64(key)] = args else {
                    return Err(HostError::Failed);
                };
                ctx.record_read(ResourceKeyRef::HostState {
                    op: sig_hash,
                    key: *key,
                });
                let v = *self.kv.borrow().get(key).unwrap_or(&0);
                rets[0] = Value::I64(v);
                Ok(0)
            }
        }

        // Program: return kv.get(1)
        let get_sig = HostSig {
            args: vec![ValueType::U64],
            rets: vec![ValueType::I64],
        };
        let get_hash = sig_hash(&get_sig);

        let mut pb = ProgramBuilder::new();
        let get_host = pb.host_sig_for("kv.get", get_sig);

        let mut a = Asm::new();
        a.const_u64(1, 1);
        a.host_call(0, get_host, 0, &[1], &[2]);
        a.ret(0, &[2]);

        let f = pb
            .push_function_checked(
                a,
                FunctionSig {
                    arg_types: vec![],
                    ret_types: vec![ValueType::I64],
                },
            )
            .unwrap();
        pb.set_function_output_name(f, 0, "value").unwrap();
        let prog = Arc::new(pb.build_verified().unwrap());

        let kv = Rc::new(RefCell::new(BTreeMap::new()));
        kv.borrow_mut().insert(1, 7);
        let host = KvHost {
            kv: kv.clone(),
            get_sig: get_hash,
        };

        let mut g = ExecutionGraph::new(TapeExecutor::new(host, Limits::default()));
        let n = g.add_tape_node(prog, f, vec![]).unwrap();

        g.run_all().unwrap();
        assert_eq!(
            g.node_outputs(n).unwrap().get("value"),
            Some(&Value::I64(7))
        );
        assert_eq!(g.node_run_count(n), Some(1));

        // No invalidation => no additional work.
        g.run_all().unwrap();
        assert_eq!(g.node_run_count(n), Some(1));

        // Mutate host state out-of-band and invalidate the corresponding key.
        kv.borrow_mut().insert(1, 8);
        g.invalidate(ResourceKey::host_state(HostOpId::new(get_hash.0), 1));
        g.run_all().unwrap();

        assert_eq!(
            g.node_outputs(n).unwrap().get("value"),
            Some(&Value::I64(8))
        );
        assert_eq!(g.node_run_count(n), Some(2));
    }

    #[test]
    fn host_write_invalidates_prior_readers_of_same_key() {
        #[derive(Clone)]
        struct KvHost {
            kv: Rc<RefCell<BTreeMap<u64, i64>>>,
            get_sig: SigHash,
            set_sig: SigHash,
        }

        impl Host for KvHost {
            fn call(
                &mut self,
                symbol: &str,
                sig_hash: SigHash,
                args: &[ValueRef<'_>],
                rets: &mut [Value],
                mut ctx: HostContext<'_, '_>,
            ) -> Result<u64, HostError> {
                match symbol {
                    "kv.get" => {
                        if sig_hash != self.get_sig {
                            return Err(HostError::SignatureMismatch);
                        }
                        let [ValueRef::U64(key)] = args else {
                            return Err(HostError::Failed);
                        };
                        ctx.record_read(ResourceKeyRef::HostState {
                            op: self.get_sig,
                            key: *key,
                        });
                        let v = *self.kv.borrow().get(key).unwrap_or(&0);
                        rets[0] = Value::I64(v);
                        Ok(0)
                    }
                    "kv.set" => {
                        if sig_hash != self.set_sig {
                            return Err(HostError::SignatureMismatch);
                        }
                        let [ValueRef::U64(key), ValueRef::I64(value)] = args else {
                            return Err(HostError::Failed);
                        };
                        self.kv.borrow_mut().insert(*key, *value);
                        // Use the reader's key namespace so this write invalidates prior reads.
                        ctx.record_write(ResourceKeyRef::HostState {
                            op: self.get_sig,
                            key: *key,
                        });
                        rets[0] = Value::Unit;
                        Ok(0)
                    }
                    _ => Err(HostError::UnknownSymbol),
                }
            }
        }

        let get_sig = HostSig {
            args: vec![ValueType::U64],
            rets: vec![ValueType::I64],
        };
        let set_sig = HostSig {
            args: vec![ValueType::U64, ValueType::I64],
            rets: vec![ValueType::Unit],
        };
        let get_hash = sig_hash(&get_sig);
        let set_hash = sig_hash(&set_sig);

        let mut get_builder = ProgramBuilder::new();
        let get_host = get_builder.host_sig_for("kv.get", get_sig);
        let mut get_asm = Asm::new();
        get_asm.const_u64(1, 1);
        get_asm.host_call(0, get_host, 0, &[1], &[2]);
        get_asm.ret(0, &[2]);
        let get_entry = get_builder
            .push_function_checked(
                get_asm,
                FunctionSig {
                    arg_types: vec![],
                    ret_types: vec![ValueType::I64],
                },
            )
            .unwrap();
        get_builder
            .set_function_output_name(get_entry, 0, "value")
            .unwrap();
        let get_prog = Arc::new(get_builder.build_verified().unwrap());

        let mut set_builder = ProgramBuilder::new();
        let set_host = set_builder.host_sig_for("kv.set", set_sig);
        let mut set_asm = Asm::new();
        set_asm.const_u64(1, 1);
        set_asm.const_i64(2, 8);
        set_asm.host_call(0, set_host, 0, &[1, 2], &[3]);
        set_asm.ret(0, &[3]);
        let set_entry = set_builder
            .push_function_checked(
                set_asm,
                FunctionSig {
                    arg_types: vec![],
                    ret_types: vec![ValueType::Unit],
                },
            )
            .unwrap();
        set_builder
            .set_function_output_name(set_entry, 0, "done")
            .unwrap();
        let set_prog = Arc::new(set_builder.build_verified().unwrap());

        let kv = Rc::new(RefCell::new(BTreeMap::new()));
        kv.borrow_mut().insert(1, 7);
        let host = KvHost {
            kv,
            get_sig: get_hash,
            set_sig: set_hash,
        };

        let mut g = ExecutionGraph::new(TapeExecutor::new(host, Limits::default()));
        let reader = g.add_tape_node(get_prog, get_entry, vec![]).unwrap();

        g.run_all().unwrap();
        assert_eq!(
            g.node_outputs(reader).unwrap().get("value"),
            Some(&Value::I64(7))
        );
        assert_eq!(g.node_run_count(reader), Some(1));

        let writer = g.add_tape_node(set_prog, set_entry, vec![]).unwrap();
        g.run_node(writer).unwrap();
        assert_eq!(g.node_run_count(reader), Some(1));

        g.run_all().unwrap();
        assert_eq!(
            g.node_outputs(reader).unwrap().get("value"),
            Some(&Value::I64(8))
        );
        assert_eq!(g.node_run_count(reader), Some(2));
    }

    #[test]
    fn node_that_reads_and_writes_same_key_reaches_fixpoint() {
        // A single node whose host call both reads and writes the SAME host-state key
        // (a read-modify-write). The write marks the key dirty so other readers would be
        // invalidated, but the node must not invalidate *itself*: excluding self-written keys
        // from its own dependency set keeps it convergent. Without that exclusion the node
        // re-runs on every run_all() forever (run_count would grow 1, 2, 3, ...).
        #[derive(Clone)]
        struct BumpHost {
            kv: Rc<RefCell<BTreeMap<u64, i64>>>,
            sig: SigHash,
        }

        impl Host for BumpHost {
            fn call(
                &mut self,
                symbol: &str,
                sig_hash: SigHash,
                args: &[ValueRef<'_>],
                rets: &mut [Value],
                mut ctx: HostContext<'_, '_>,
            ) -> Result<u64, HostError> {
                if symbol != "kv.bump" {
                    return Err(HostError::UnknownSymbol);
                }
                if sig_hash != self.sig {
                    return Err(HostError::SignatureMismatch);
                }
                let [ValueRef::U64(key)] = args else {
                    return Err(HostError::Failed);
                };
                // Read the current value (records a dependency on the key)...
                ctx.record_read(ResourceKeyRef::HostState {
                    op: self.sig,
                    key: *key,
                });
                let next = self.kv.borrow().get(key).unwrap_or(&0) + 1;
                self.kv.borrow_mut().insert(*key, next);
                // ...then write the bumped value back under the SAME key.
                ctx.record_write(ResourceKeyRef::HostState {
                    op: self.sig,
                    key: *key,
                });
                rets[0] = Value::I64(next);
                Ok(0)
            }
        }

        let bump_sig = HostSig {
            args: vec![ValueType::U64],
            rets: vec![ValueType::I64],
        };
        let bump_hash = sig_hash(&bump_sig);

        let mut pb = ProgramBuilder::new();
        let bump_host = pb.host_sig_for("kv.bump", bump_sig);
        let mut asm = Asm::new();
        asm.const_u64(1, 1);
        asm.host_call(0, bump_host, 0, &[1], &[2]);
        asm.ret(0, &[2]);
        let entry = pb
            .push_function_checked(
                asm,
                FunctionSig {
                    arg_types: vec![],
                    ret_types: vec![ValueType::I64],
                },
            )
            .unwrap();
        pb.set_function_output_name(entry, 0, "value").unwrap();
        let prog = Arc::new(pb.build_verified().unwrap());

        let kv = Rc::new(RefCell::new(BTreeMap::new()));
        let host = BumpHost { kv, sig: bump_hash };

        let mut g = ExecutionGraph::new(TapeExecutor::new(host, Limits::default()));
        let n = g.add_tape_node(prog, entry, vec![]).unwrap();

        g.run_all().unwrap();
        assert_eq!(g.node_run_count(n), Some(1));
        assert_eq!(
            g.node_outputs(n).unwrap().get("value"),
            Some(&Value::I64(1))
        );

        // No external invalidation between calls: the node's own write must not re-trigger it,
        // so repeated run_all() calls are no-ops and the bumped value stays put.
        g.run_all().unwrap();
        g.run_all().unwrap();
        assert_eq!(
            g.node_run_count(n),
            Some(1),
            "a node that reads and writes the same key must not re-run itself"
        );
        assert_eq!(
            g.node_outputs(n).unwrap().get("value"),
            Some(&Value::I64(1))
        );
    }

    #[test]
    fn host_write_to_input_key_does_not_drop_graph_input_dependency() {
        // A host call writes a graph `Input` key whose name matches the node's own input binding.
        // `Input` keys are graph-owned, so the write must be ignored — otherwise it would intern
        // to the same id as the binding dependency and the self-write filter would strip it,
        // leaving the node stale after a later `invalidate_input`.
        struct PublishHost;

        impl Host for PublishHost {
            fn call(
                &mut self,
                symbol: &str,
                _sig_hash: SigHash,
                args: &[ValueRef<'_>],
                rets: &mut [Value],
                mut ctx: HostContext<'_, '_>,
            ) -> Result<u64, HostError> {
                if symbol != "publish" {
                    return Err(HostError::UnknownSymbol);
                }
                let [ValueRef::I64(v)] = args else {
                    return Err(HostError::Failed);
                };
                // Host misuses a graph-owned Input key as a write target.
                ctx.record_write(ResourceKeyRef::Input("x"));
                rets[0] = Value::I64(*v);
                Ok(0)
            }
        }

        let publish_sig = HostSig {
            args: vec![ValueType::I64],
            rets: vec![ValueType::I64],
        };

        let mut pb = ProgramBuilder::new();
        let publish = pb.host_sig_for("publish", publish_sig);
        let mut asm = Asm::new();
        asm.host_call(0, publish, 0, &[1], &[2]);
        asm.ret(0, &[2]);
        let entry = pb
            .push_function_checked(
                asm,
                FunctionSig {
                    arg_types: vec![ValueType::I64],
                    ret_types: vec![ValueType::I64],
                },
            )
            .unwrap();
        pb.set_function_output_name(entry, 0, "value").unwrap();
        let prog = Arc::new(pb.build_verified().unwrap());

        let mut g = ExecutionGraph::new(TapeExecutor::new(PublishHost, Limits::default()));
        let n = g.add_tape_node(prog, entry, vec!["x".into()]).unwrap();
        g.set_input_value(n, "x", Value::I64(1)).unwrap();

        g.run_all().unwrap();
        assert_eq!(g.node_run_count(n), Some(1));
        assert_eq!(
            g.node_outputs(n).unwrap().get("value"),
            Some(&Value::I64(1))
        );

        // The host's Input-key write is ignored, so the node keeps its "x" binding dependency:
        // changing and invalidating "x" must still rerun the node and refresh its output.
        g.set_input_value(n, "x", Value::I64(2)).unwrap();
        g.invalidate_input("x");
        g.run_all().unwrap();
        assert_eq!(
            g.node_run_count(n),
            Some(2),
            "graph input dependency must survive a host write to the same Input key"
        );
        assert_eq!(
            g.node_outputs(n).unwrap().get("value"),
            Some(&Value::I64(2))
        );
    }

    #[test]
    fn host_read_order_changes_do_not_change_last_read_ids() {
        #[derive(Clone)]
        struct FlippingReadHost {
            flip: Rc<RefCell<bool>>,
            op_sig: SigHash,
        }

        impl Host for FlippingReadHost {
            fn call(
                &mut self,
                symbol: &str,
                sig_hash: SigHash,
                _args: &[ValueRef<'_>],
                rets: &mut [Value],
                mut ctx: HostContext<'_, '_>,
            ) -> Result<u64, HostError> {
                if symbol != "flip.reads" {
                    return Err(HostError::UnknownSymbol);
                }
                if sig_hash != self.op_sig {
                    return Err(HostError::SignatureMismatch);
                }

                let mut flip = self.flip.borrow_mut();
                let (a, b) = if *flip {
                    (2_u64, 1_u64)
                } else {
                    (1_u64, 2_u64)
                };
                *flip = !*flip;

                ctx.record_read(ResourceKeyRef::HostState {
                    op: sig_hash,
                    key: a,
                });
                ctx.record_read(ResourceKeyRef::HostState {
                    op: sig_hash,
                    key: b,
                });
                rets[0] = Value::I64(0);
                Ok(0)
            }
        }

        let host_sig = HostSig {
            args: vec![],
            rets: vec![ValueType::I64],
        };
        let op_hash = sig_hash(&host_sig);

        let mut pb = ProgramBuilder::new();
        let op = pb.host_sig_for("flip.reads", host_sig);
        let mut a = Asm::new();
        a.host_call(0, op, 0, &[], &[1]);
        a.ret(0, &[1]);
        let f = pb
            .push_function_checked(
                a,
                FunctionSig {
                    arg_types: vec![],
                    ret_types: vec![ValueType::I64],
                },
            )
            .unwrap();
        pb.set_function_output_name(f, 0, "value").unwrap();
        let prog = Arc::new(pb.build_verified().unwrap());

        let mut g = ExecutionGraph::new(TapeExecutor::new(
            FlippingReadHost {
                flip: Rc::new(RefCell::new(false)),
                op_sig: op_hash,
            },
            Limits::default(),
        ));
        let n = g.add_tape_node(prog, f, vec![]).unwrap();

        g.run_all().unwrap();
        let first_ids = g.nodes[usize::try_from(n.as_u64()).unwrap()]
            .last_read_ids
            .clone();

        g.invalidate(ResourceKey::host_state(HostOpId::new(op_hash.0), 1));
        g.run_all().unwrap();
        let second_ids = g.nodes[usize::try_from(n.as_u64()).unwrap()]
            .last_read_ids
            .clone();

        assert_eq!(first_ids, second_ids);
    }

    #[test]
    fn invalidating_opaque_host_reruns_dependent_nodes() {
        #[derive(Clone)]
        struct KvHost {
            kv: Rc<RefCell<BTreeMap<u64, i64>>>,
            get_sig: SigHash,
        }

        impl Host for KvHost {
            fn call(
                &mut self,
                symbol: &str,
                sig_hash: SigHash,
                args: &[ValueRef<'_>],
                rets: &mut [Value],
                mut ctx: HostContext<'_, '_>,
            ) -> Result<u64, HostError> {
                if symbol != "kv.get" {
                    return Err(HostError::UnknownSymbol);
                }
                if sig_hash != self.get_sig {
                    return Err(HostError::SignatureMismatch);
                }
                let [ValueRef::U64(key)] = args else {
                    return Err(HostError::Failed);
                };
                ctx.record_read(ResourceKeyRef::OpaqueHost { op: sig_hash });
                let v = *self.kv.borrow().get(key).unwrap_or(&0);
                rets[0] = Value::I64(v);
                Ok(0)
            }
        }

        // Program: return kv.get(1)
        let get_sig = HostSig {
            args: vec![ValueType::U64],
            rets: vec![ValueType::I64],
        };
        let get_hash = sig_hash(&get_sig);

        let mut pb = ProgramBuilder::new();
        let get_host = pb.host_sig_for("kv.get", get_sig);

        let mut a = Asm::new();
        a.const_u64(1, 1);
        a.host_call(0, get_host, 0, &[1], &[2]);
        a.ret(0, &[2]);

        let f = pb
            .push_function_checked(
                a,
                FunctionSig {
                    arg_types: vec![],
                    ret_types: vec![ValueType::I64],
                },
            )
            .unwrap();
        pb.set_function_output_name(f, 0, "value").unwrap();
        let prog = Arc::new(pb.build_verified().unwrap());

        let kv = Rc::new(RefCell::new(BTreeMap::new()));
        kv.borrow_mut().insert(1, 7);
        let host = KvHost {
            kv: kv.clone(),
            get_sig: get_hash,
        };

        let mut g = ExecutionGraph::new(TapeExecutor::new(host, Limits::default()));
        let n = g.add_tape_node(prog, f, vec![]).unwrap();

        g.run_all().unwrap();
        assert_eq!(
            g.node_outputs(n).unwrap().get("value"),
            Some(&Value::I64(7))
        );
        assert_eq!(g.node_run_count(n), Some(1));

        // Mutate host state out-of-band and invalidate the conservative opaque key.
        kv.borrow_mut().insert(1, 8);
        g.invalidate_tape_key(ResourceKeyRef::OpaqueHost { op: get_hash });
        g.run_all().unwrap();

        assert_eq!(
            g.node_outputs(n).unwrap().get("value"),
            Some(&Value::I64(8))
        );
        assert_eq!(g.node_run_count(n), Some(2));
    }

    #[test]
    fn invalidating_an_input_reruns_transitive_dependents_only_when_needed() {
        fn make_identity_program(output_name: &str) -> (Arc<VerifiedProgram>, FuncId) {
            let mut pb = ProgramBuilder::new();
            let mut a = Asm::new();
            a.ret(0, &[1]);
            let f = pb
                .push_function_checked(
                    a,
                    FunctionSig {
                        arg_types: vec![ValueType::I64],
                        ret_types: vec![ValueType::I64],
                    },
                )
                .unwrap();
            pb.set_function_output_name(f, 0, output_name).unwrap();
            (Arc::new(pb.build_verified().unwrap()), f)
        }

        let (a_prog, a_entry) = make_identity_program("value");
        let (b_prog, b_entry) = make_identity_program("value");
        let (c_prog, c_entry) = make_identity_program("value");

        let mut g = ExecutionGraph::new(TapeExecutor::new(HostNoop, Limits::default()));
        let na = g.add_tape_node(a_prog, a_entry, vec!["in".into()]).unwrap();
        let nb = g.add_tape_node(b_prog, b_entry, vec!["x".into()]).unwrap();
        let nc = g.add_tape_node(c_prog, c_entry, vec!["y".into()]).unwrap();

        g.set_input_value(na, "in", Value::I64(7)).unwrap();
        g.connect(na, "value", nb, "x").unwrap();
        g.connect(nb, "value", nc, "y").unwrap();

        g.run_all().unwrap();
        assert_eq!(
            g.node_outputs(nc).unwrap().get("value"),
            Some(&Value::I64(7))
        );
        assert_eq!(g.node_run_count(na), Some(1));
        assert_eq!(g.node_run_count(nb), Some(1));
        assert_eq!(g.node_run_count(nc), Some(1));

        // No invalidation => no additional work.
        g.run_all().unwrap();
        assert_eq!(g.node_run_count(na), Some(1));
        assert_eq!(g.node_run_count(nb), Some(1));
        assert_eq!(g.node_run_count(nc), Some(1));

        // Change the external input and invalidate its key.
        g.set_input_value(na, "in", Value::I64(8)).unwrap();
        g.invalidate_input("in");
        g.run_all().unwrap();

        assert_eq!(
            g.node_outputs(nc).unwrap().get("value"),
            Some(&Value::I64(8))
        );
        assert_eq!(g.node_run_count(na), Some(2));
        assert_eq!(g.node_run_count(nb), Some(2));
        assert_eq!(g.node_run_count(nc), Some(2));
    }

    #[test]
    fn first_run_sync_clears_conservative_deps_for_zero_read_node() {
        fn make_identity_program(output_name: &str) -> (Arc<VerifiedProgram>, FuncId) {
            let mut pb = ProgramBuilder::new();
            let mut a = Asm::new();
            a.ret(0, &[1]);
            let f = pb
                .push_function_checked(
                    a,
                    FunctionSig {
                        arg_types: vec![ValueType::I64],
                        ret_types: vec![ValueType::I64],
                    },
                )
                .unwrap();
            pb.set_function_output_name(f, 0, output_name).unwrap();
            (Arc::new(pb.build_verified().unwrap()), f)
        }

        fn make_const_program(output_name: &str, v: i64) -> (Arc<VerifiedProgram>, FuncId) {
            let mut pb = ProgramBuilder::new();
            let mut a = Asm::new();
            a.const_i64(1, v);
            a.ret(0, &[1]);
            let f = pb
                .push_function_checked(
                    a,
                    FunctionSig {
                        arg_types: vec![],
                        ret_types: vec![ValueType::I64],
                    },
                )
                .unwrap();
            pb.set_function_output_name(f, 0, output_name).unwrap();
            (Arc::new(pb.build_verified().unwrap()), f)
        }

        let (a_prog, a_entry) = make_identity_program("value");
        let (b_prog, b_entry) = make_const_program("value", 9);

        let mut g = ExecutionGraph::new(TapeExecutor::new(HostNoop, Limits::default()));
        let na = g.add_tape_node(a_prog, a_entry, vec!["in".into()]).unwrap();
        let nb = g.add_tape_node(b_prog, b_entry, vec![]).unwrap();

        // Seed the same conservative dirty edge that `connect` creates before dynamic access
        // refinement, but keep B input-free so its first run observes zero reads.
        let na_index = usize::try_from(na.as_u64()).unwrap();
        let nb_index = usize::try_from(nb.as_u64()).unwrap();
        let src = g.nodes[na_index].output_ids[0];
        let dst = g.nodes[nb_index].output_ids[0];
        g.dirty.add_dependency(dst, src);
        g.dirty.mark_dirty(dst);
        g.set_input_value(na, "in", Value::I64(1)).unwrap();

        g.run_all().unwrap();
        assert_eq!(g.node_run_count(na), Some(1));
        assert_eq!(g.node_run_count(nb), Some(1));

        // If conservative deps were not replaced on first run, this would spuriously rerun B.
        g.set_input_value(na, "in", Value::I64(2)).unwrap();
        g.invalidate_input("in");
        g.run_all().unwrap();

        assert_eq!(g.node_run_count(na), Some(2));
        assert_eq!(g.node_run_count(nb), Some(1));
    }

    #[test]
    fn run_node_errors_on_bad_node_id() {
        let mut g = ExecutionGraph::new(TapeExecutor::new(HostNoop, Limits::default()));
        assert_eq!(g.run_node(NodeId::new(999)), Err(GraphError::BadNodeId));
    }

    #[test]
    fn duplicate_input_names_alias_same_binding() {
        let mut pb = ProgramBuilder::new();
        let mut a = Asm::new();
        // Return arg1 where both args are named "x". If aliasing is broken, run fails with
        // MissingInput for the second slot.
        a.ret(0, &[1]);
        let f = pb
            .push_function_checked(
                a,
                FunctionSig {
                    arg_types: vec![ValueType::I64, ValueType::I64],
                    ret_types: vec![ValueType::I64],
                },
            )
            .unwrap();
        pb.set_function_output_name(f, 0, "value").unwrap();
        let prog = Arc::new(pb.build_verified().unwrap());

        let mut g = ExecutionGraph::new(TapeExecutor::new(HostNoop, Limits::default()));
        let n = g
            .add_tape_node(prog, f, vec!["x".into(), "x".into()])
            .unwrap();
        g.set_input_value(n, "x", Value::I64(7)).unwrap();

        g.run_all().unwrap();
        assert_eq!(
            g.node_outputs(n).unwrap().get("value"),
            Some(&Value::I64(7))
        );
    }

    #[test]
    fn node_last_access_returns_some_when_collection_enabled() {
        // A constant node with no inputs (zero reads, output writes only).
        // Nodes with zero outputs cannot be tested here because they have no dirty keys
        // and are never scheduled by plan_all.
        let mut pb = ProgramBuilder::new();
        let mut a = Asm::new();
        a.const_i64(1, 42);
        a.ret(0, &[1]);
        let f = pb
            .push_function_checked(
                a,
                FunctionSig {
                    arg_types: vec![],
                    ret_types: vec![ValueType::I64],
                },
            )
            .unwrap();
        pb.set_function_output_name(f, 0, "value").unwrap();
        let prog = Arc::new(pb.build_verified().unwrap());

        let mut g = ExecutionGraph::new(TapeExecutor::new(HostNoop, Limits::default()));
        g.set_collect_access_log(true);
        let n = g.add_tape_node(prog, f, vec![]).unwrap();
        g.run_all().unwrap();

        let log = g.node_last_access(n);
        assert!(
            log.is_some(),
            "access log should be Some when collection is enabled"
        );
    }

    #[test]
    fn node_last_access_returns_none_after_collection_disabled_rerun() {
        fn make_identity_program(output_name: &str) -> (Arc<VerifiedProgram>, FuncId) {
            let mut pb = ProgramBuilder::new();
            let mut a = Asm::new();
            a.ret(0, &[1]);
            let f = pb
                .push_function_checked(
                    a,
                    FunctionSig {
                        arg_types: vec![ValueType::I64],
                        ret_types: vec![ValueType::I64],
                    },
                )
                .unwrap();
            pb.set_function_output_name(f, 0, output_name).unwrap();
            (Arc::new(pb.build_verified().unwrap()), f)
        }

        let (prog, entry) = make_identity_program("value");
        let mut g = ExecutionGraph::new(TapeExecutor::new(HostNoop, Limits::default()));
        let n = g.add_tape_node(prog, entry, vec!["in".into()]).unwrap();
        g.set_input_value(n, "in", Value::I64(1)).unwrap();

        // Run with collection enabled — should produce a log.
        g.set_collect_access_log(true);
        g.run_all().unwrap();
        assert!(g.node_last_access(n).is_some());

        // Disable collection, rerun — stale log must be cleared.
        g.set_collect_access_log(false);
        g.set_input_value(n, "in", Value::I64(2)).unwrap();
        g.invalidate_input("in");
        g.run_all().unwrap();
        assert!(
            g.node_last_access(n).is_none(),
            "stale access log should be cleared after rerun with collection disabled"
        );
    }

    #[test]
    fn in_place_output_update_preserves_values_across_reruns() {
        fn make_identity_program(output_name: &str) -> (Arc<VerifiedProgram>, FuncId) {
            let mut pb = ProgramBuilder::new();
            let mut a = Asm::new();
            a.ret(0, &[1]);
            let f = pb
                .push_function_checked(
                    a,
                    FunctionSig {
                        arg_types: vec![ValueType::I64],
                        ret_types: vec![ValueType::I64],
                    },
                )
                .unwrap();
            pb.set_function_output_name(f, 0, output_name).unwrap();
            (Arc::new(pb.build_verified().unwrap()), f)
        }

        let (prog, entry) = make_identity_program("value");
        let mut g = ExecutionGraph::new(TapeExecutor::new(HostNoop, Limits::default()));
        let n = g.add_tape_node(prog, entry, vec!["in".into()]).unwrap();

        // First run populates the output map.
        g.set_input_value(n, "in", Value::I64(10)).unwrap();
        g.run_all().unwrap();
        assert_eq!(
            g.node_outputs(n).unwrap().get("value"),
            Some(&Value::I64(10))
        );

        // Second run uses in-place update path.
        g.set_input_value(n, "in", Value::I64(20)).unwrap();
        g.invalidate_input("in");
        g.run_all().unwrap();
        assert_eq!(
            g.node_outputs(n).unwrap().get("value"),
            Some(&Value::I64(20))
        );

        // Third run confirms stability.
        g.set_input_value(n, "in", Value::I64(30)).unwrap();
        g.invalidate_input("in");
        g.run_all().unwrap();
        assert_eq!(
            g.node_outputs(n).unwrap().get("value"),
            Some(&Value::I64(30))
        );
        assert_eq!(g.node_run_count(n), Some(3));
    }
}

#[cfg(test)]
mod native_tests {
    extern crate std;

    use alloc::rc::Rc;
    use alloc::string::ToString;
    use alloc::vec;
    use core::cell::Cell;
    use core::convert::Infallible;

    use super::*;
    use crate::native::{FnExecutor, FnNode};

    type Graph = ExecutionGraph<FnExecutor<i64, &'static str>>;

    fn const_node(value: i64) -> FnNode<i64, &'static str> {
        FnNode::new(move |_inputs, outputs, _access| {
            outputs.push(value);
            Ok(())
        })
    }

    fn sum_node() -> FnNode<i64, &'static str> {
        FnNode::new(|inputs, outputs, _access| {
            outputs.push(inputs.iter().sum());
            Ok(())
        })
    }

    #[test]
    fn closures_run_in_dependency_order_and_only_when_dirty() {
        let mut g = Graph::new(FnExecutor::new());
        let a = g
            .add_node(const_node(2), vec![], vec!["value".into()])
            .unwrap();
        let b = g
            .add_node(
                sum_node(),
                vec!["lhs".into(), "rhs".into()],
                vec!["sum".into()],
            )
            .unwrap();
        g.set_input_value(b, "rhs", 40).unwrap();
        g.connect(a, "value", b, "lhs").unwrap();

        assert_eq!(g.run_all().unwrap().executed_nodes, 2);
        assert_eq!(g.node_outputs(b).unwrap().get("sum"), Some(&42));
        assert_eq!(g.run_all().unwrap().executed_nodes, 0);

        g.set_input_value(b, "rhs", 41).unwrap();
        g.invalidate_input("rhs");
        let report = g.run_all_with_report(ReportDetailMask::FULL).unwrap();
        assert_eq!(report.executed.len(), 1);
        assert_eq!(report.executed[0].node, b);
        assert_eq!(
            report.executed[0].because_of,
            Some(ResourceKey::node_output(b, "sum"))
        );
        assert_eq!(g.node_outputs(b).unwrap().get("sum"), Some(&43));
        assert_eq!(g.node_run_count(a), Some(1));
    }

    #[test]
    fn executor_errors_are_wrapped_with_the_node_id() {
        let mut g = Graph::new(FnExecutor::new());
        let ok = g
            .add_node(const_node(1), vec![], vec!["value".into()])
            .unwrap();
        let failing = g
            .add_node(
                FnNode::new(|_inputs, _outputs, _access| Err("boom")),
                vec![],
                vec!["value".into()],
            )
            .unwrap();

        let err = g.run_all().unwrap_err();
        assert_eq!(
            err,
            GraphError::Node {
                node: failing,
                source: "boom",
            }
        );
        assert!(err.to_string().contains("node execution failed"));
        assert!(err.to_string().contains("boom"));

        // The failed node's pending work survives for a later run; the healthy node ran once.
        assert_eq!(g.node_run_count(ok), Some(1));
        assert_eq!(g.node_run_count(failing), Some(0));
        assert!(matches!(g.run_all(), Err(GraphError::Node { .. })));
        assert_eq!(g.node_run_count(ok), Some(1));
    }

    #[test]
    fn output_arity_is_checked_against_declared_names() {
        let mut g = Graph::new(FnExecutor::new());
        let n = g
            .add_node(const_node(1), vec![], vec!["a".into(), "b".into()])
            .unwrap();
        assert_eq!(g.run_all(), Err(GraphError::BadOutputArity { node: n }));
        assert_eq!(g.node_run_count(n), Some(0));
        assert!(g.node_outputs(n).unwrap().is_empty());
    }

    #[test]
    fn duplicate_output_names_are_rejected() {
        let mut g = Graph::new(FnExecutor::new());
        assert_eq!(
            g.add_node(const_node(1), vec![], vec!["a".into(), "a".into()])
                .unwrap_err(),
            GraphError::DuplicateOutput { name: "a".into() }
        );
        assert!(g.node_outputs(NodeId::new(0)).is_none());
    }

    #[test]
    fn node_access_reads_make_executor_state_invalidatable() {
        let rate = Rc::new(Cell::new(10));
        let op = HostOpId::new(7);
        const RATE_KEY: u64 = 3;

        let mut g = Graph::new(FnExecutor::new());
        let captured = rate.clone();
        let scaled = g
            .add_node(
                FnNode::named("scaled", move |inputs, outputs, access| {
                    access.read_host_state(op, RATE_KEY);
                    outputs.push(inputs[0] * captured.get());
                    Ok(())
                }),
                vec!["x".into()],
                vec!["value".into()],
            )
            .unwrap();
        let untouched = g
            .add_node(const_node(5), vec![], vec!["value".into()])
            .unwrap();
        g.set_input_value(scaled, "x", 2).unwrap();
        g.set_collect_access_log(true);

        g.run_all().unwrap();
        assert_eq!(g.node_outputs(scaled).unwrap().get("value"), Some(&20));
        assert_eq!(
            g.node_last_access(scaled).unwrap().as_slice(),
            &[
                Access::Read(ResourceKey::input("x")),
                Access::Read(ResourceKey::host_state(op, RATE_KEY)),
                Access::Write(ResourceKey::node_output(scaled, "value")),
            ]
        );

        // Mutating executor state without invalidation is invisible to the graph, by design.
        rate.set(100);
        assert_eq!(g.run_all().unwrap().executed_nodes, 0);

        g.invalidate(ResourceKey::host_state(op, RATE_KEY));
        let report = g.run_all_with_report(ReportDetailMask::FULL).unwrap();
        assert_eq!(report.executed.len(), 1);
        assert_eq!(report.executed[0].node, scaled);
        assert_eq!(
            report.executed[0].why_path.as_deref(),
            Some(
                &[
                    ResourceKey::host_state(op, RATE_KEY),
                    ResourceKey::node_output(scaled, "value"),
                ][..]
            )
        );
        assert_eq!(g.node_outputs(scaled).unwrap().get("value"), Some(&200));
        assert_eq!(g.node_run_count(untouched), Some(1));
        assert_eq!(g.node_description(scaled).as_deref(), Some("scaled"));
    }

    #[test]
    fn node_access_writes_invalidate_other_readers_but_not_the_writer() {
        let op = HostOpId::new(1);
        let mut g = ExecutionGraph::new(FnExecutor::<i64, Infallible>::new());
        let reader = g
            .add_node(
                FnNode::new(move |_inputs, outputs, access| {
                    access.read_opaque_host(op);
                    outputs.push(1);
                    Ok(())
                }),
                vec![],
                vec!["value".into()],
            )
            .unwrap();
        let writer = g
            .add_node(
                FnNode::new(move |_inputs, outputs, access| {
                    access.read_opaque_host(op);
                    access.write_opaque_host(op);
                    outputs.push(2);
                    Ok(())
                }),
                vec![],
                vec!["value".into()],
            )
            .unwrap();

        g.run_all().unwrap();
        assert_eq!(g.node_run_count(reader), Some(1));
        assert_eq!(g.node_run_count(writer), Some(1));

        // The writer dirtied the opaque key during the first run: the reader re-runs, the
        // read-modify-write node has reached its fixpoint.
        g.run_all().unwrap();
        assert_eq!(g.node_run_count(reader), Some(2));
        assert_eq!(g.node_run_count(writer), Some(1));
        assert_eq!(g.run_all().unwrap().executed_nodes, 0);
    }

    #[test]
    fn node_access_input_reads_join_the_named_input_key_space() {
        let mut g = ExecutionGraph::new(FnExecutor::<i64, Infallible>::new());
        let n = g
            .add_node(
                FnNode::new(|_inputs, outputs, access| {
                    access.read_input("ambient");
                    outputs.push(0);
                    Ok(())
                }),
                vec![],
                vec!["value".into()],
            )
            .unwrap();

        g.run_all().unwrap();
        g.invalidate_input("unrelated");
        assert_eq!(g.run_all().unwrap().executed_nodes, 0);
        g.invalidate_input("ambient");
        assert_eq!(g.run_all().unwrap().executed_nodes, 1);
        assert_eq!(g.node_run_count(n), Some(2));
    }

    /// `a` and `b` both read input `x`; `c` reads `a.value`, and `e` reads `a.value` too.
    fn shared_input_graph(g: &mut Graph) -> [NodeId; 4] {
        let a = g
            .add_node(sum_node(), vec!["x".into()], vec!["value".into()])
            .unwrap();
        let b = g
            .add_node(sum_node(), vec!["x".into()], vec!["value".into()])
            .unwrap();
        let c = g
            .add_node(sum_node(), vec!["v".into()], vec!["value".into()])
            .unwrap();
        let e = g
            .add_node(sum_node(), vec!["v".into()], vec!["value".into()])
            .unwrap();
        g.set_input_value(a, "x", 1).unwrap();
        g.set_input_value(b, "x", 1).unwrap();
        g.connect(a, "value", c, "v").unwrap();
        g.connect(a, "value", e, "v").unwrap();
        [a, b, c, e]
    }

    fn output(g: &Graph, node: NodeId) -> i64 {
        *g.node_outputs(node).unwrap().get("value").unwrap()
    }

    #[test]
    fn run_node_keeps_work_for_dependents_outside_the_closure() {
        let mut g = Graph::new(FnExecutor::new());
        let [a, b, c, e] = shared_input_graph(&mut g);
        g.run_all().unwrap();

        g.set_input_value(a, "x", 2).unwrap();
        g.set_input_value(b, "x", 2).unwrap();
        g.invalidate_input("x");
        // Runs `a` and `c`. `b` shares the invalidated input and `e` reads the recomputed
        // `a.value`; both lie outside `c`'s closure and must stay pending.
        assert_eq!(g.run_node(c).unwrap().executed_nodes, 2);
        assert_eq!(g.run_all().unwrap().executed_nodes, 2);
        for node in [a, b, c, e] {
            assert_eq!(output(&g, node), 2);
        }
        for (node, runs) in [(a, 2), (b, 2), (c, 2), (e, 2)] {
            assert_eq!(g.node_run_count(node), Some(runs));
        }
    }

    #[test]
    fn traced_run_node_keeps_work_for_dependents_outside_the_closure() {
        let mut g = Graph::new(FnExecutor::new());
        let [a, b, c, e] = shared_input_graph(&mut g);
        g.run_all().unwrap();

        g.set_input_value(a, "x", 2).unwrap();
        g.set_input_value(b, "x", 2).unwrap();
        g.invalidate_input("x");
        g.run_node_with_report(c, ReportDetailMask::FULL).unwrap();
        assert_eq!(g.run_all().unwrap().executed_nodes, 2);
        assert_eq!(output(&g, b), 2);
        assert_eq!(output(&g, e), 2);
    }

    fn why_path(report: &RunDetailReport, node: NodeId) -> (Vec<ResourceKey>, Option<bool>) {
        let row = report
            .executed
            .iter()
            .find(|row| row.node == node)
            .expect("node ran");
        (
            row.why_path.clone().expect("full report has why_path"),
            row.why_path_traced,
        )
    }

    #[test]
    fn retained_work_is_explained_from_its_real_root() {
        let mut g = Graph::new(FnExecutor::new());
        let [a, b, c, e] = shared_input_graph(&mut g);
        g.run_all().unwrap();

        g.set_input_value(a, "x", 2).unwrap();
        g.set_input_value(b, "x", 2).unwrap();
        g.invalidate_input("x");
        g.run_node_with_report(c, ReportDetailMask::FULL).unwrap();
        let report = g.run_all_with_report(ReportDetailMask::FULL).unwrap();
        assert_eq!(report.executed.len(), 2);
        let x = ResourceKey::input("x");
        let a_value = ResourceKey::node_output(a, "value");
        assert_eq!(
            why_path(&report, b),
            (
                vec![x.clone(), ResourceKey::node_output(b, "value")],
                Some(true)
            )
        );
        assert_eq!(
            why_path(&report, e),
            (
                vec![x, a_value, ResourceKey::node_output(e, "value")],
                Some(true)
            )
        );
    }

    #[test]
    fn retained_work_after_an_untraced_scoped_run_is_not_claimed_traced() {
        let mut g = Graph::new(FnExecutor::new());
        let [a, b, c, e] = shared_input_graph(&mut g);
        g.run_all().unwrap();

        g.set_input_value(a, "x", 2).unwrap();
        g.set_input_value(b, "x", 2).unwrap();
        g.invalidate_input("x");
        g.run_node(c).unwrap();
        let report = g.run_all_with_report(ReportDetailMask::FULL).unwrap();
        // Only the drained key each mark depends on is known.
        assert_eq!(
            why_path(&report, e),
            (
                vec![
                    ResourceKey::node_output(a, "value"),
                    ResourceKey::node_output(e, "value")
                ],
                Some(false)
            )
        );
        assert_eq!(why_path(&report, b).1, Some(false));
    }

    #[test]
    fn chained_scoped_runs_keep_the_original_root() {
        // x -> a -> t1; a -> b -> t2; b -> q. The first scoped run retains `b`; the second
        // consumes that mark and retains `q`, which must still be explained from `x`.
        let mut g = Graph::new(FnExecutor::new());
        let a = g
            .add_node(sum_node(), vec!["x".into()], vec!["value".into()])
            .unwrap();
        let reader = |g: &mut Graph, from: NodeId| {
            let node = g
                .add_node(sum_node(), vec!["v".into()], vec!["value".into()])
                .unwrap();
            g.connect(from, "value", node, "v").unwrap();
            node
        };
        g.set_input_value(a, "x", 1).unwrap();
        let t1 = reader(&mut g, a);
        let b = reader(&mut g, a);
        let t2 = reader(&mut g, b);
        let q = reader(&mut g, b);
        g.run_all().unwrap();

        g.set_input_value(a, "x", 2).unwrap();
        g.invalidate_input("x");
        g.run_node_with_report(t1, ReportDetailMask::FULL).unwrap();
        g.run_node_with_report(t2, ReportDetailMask::FULL).unwrap();
        let report = g.run_all_with_report(ReportDetailMask::FULL).unwrap();
        assert_eq!(report.executed.len(), 1);
        assert_eq!(
            why_path(&report, q),
            (
                vec![
                    ResourceKey::input("x"),
                    ResourceKey::node_output(a, "value"),
                    ResourceKey::node_output(b, "value"),
                    ResourceKey::node_output(q, "value"),
                ],
                Some(true)
            )
        );
        assert_eq!(output(&g, q), 2);
    }

    #[test]
    fn run_node_keeps_readers_of_the_target_and_transitive_chains_pending() {
        // x -> a -> t (target); t.value -> r (reader of the target's own output);
        // a.value -> p -> q (a chain behind the closure boundary).
        let mut g = Graph::new(FnExecutor::new());
        let a = g
            .add_node(sum_node(), vec!["x".into()], vec!["value".into()])
            .unwrap();
        let t = g
            .add_node(sum_node(), vec!["v".into()], vec!["value".into()])
            .unwrap();
        let r = g
            .add_node(sum_node(), vec!["v".into()], vec!["value".into()])
            .unwrap();
        let p = g
            .add_node(sum_node(), vec!["v".into()], vec!["value".into()])
            .unwrap();
        let q = g
            .add_node(sum_node(), vec!["v".into()], vec!["value".into()])
            .unwrap();
        g.set_input_value(a, "x", 1).unwrap();
        g.connect(a, "value", t, "v").unwrap();
        g.connect(t, "value", r, "v").unwrap();
        g.connect(a, "value", p, "v").unwrap();
        g.connect(p, "value", q, "v").unwrap();
        g.run_all().unwrap();

        g.set_input_value(a, "x", 5).unwrap();
        g.invalidate_input("x");
        assert_eq!(g.run_node(t).unwrap().executed_nodes, 2);
        assert_eq!(g.run_all().unwrap().executed_nodes, 3);
        for node in [a, t, r, p, q] {
            assert_eq!(output(&g, node), 5);
            assert_eq!(g.node_run_count(node), Some(2));
        }
    }

    #[test]
    fn multi_output_nodes_straddling_the_closure_run_once() {
        // `m` has outputs `in` (read by the target) and `out` (read by `o`); both depend on
        // `x`, so `m.out` is a dependent of the drained `x` outside the closure. The scoped run
        // recomputes `m.out` already, so the retained mark moves to its reader `o`, and the
        // next run runs only `o`.
        let mut g = Graph::new(FnExecutor::new());
        let m = g
            .add_node(
                FnNode::new(|inputs, outputs, _access| {
                    let x: i64 = inputs.iter().sum();
                    outputs.push(x);
                    outputs.push(x * 10);
                    Ok(())
                }),
                vec!["x".into()],
                vec!["in".into(), "out".into()],
            )
            .unwrap();
        let t = g
            .add_node(sum_node(), vec!["v".into()], vec!["value".into()])
            .unwrap();
        let o = g
            .add_node(sum_node(), vec!["v".into()], vec!["value".into()])
            .unwrap();
        g.set_input_value(m, "x", 1).unwrap();
        g.connect(m, "in", t, "v").unwrap();
        g.connect(m, "out", o, "v").unwrap();
        g.run_all().unwrap();

        g.set_input_value(m, "x", 2).unwrap();
        g.invalidate_input("x");
        assert_eq!(g.run_node(t).unwrap().executed_nodes, 2);
        assert_eq!(g.run_all().unwrap().executed_nodes, 1);
        assert_eq!(output(&g, t), 2);
        assert_eq!(output(&g, o), 20);
        assert_eq!(g.node_run_count(m), Some(2));
    }
}

/// Mixed-executor test: one graph whose nodes are either native closures or tape programs.
#[cfg(all(test, feature = "tape"))]
mod mixed_tests {
    extern crate std;

    use alloc::string::String;
    use alloc::sync::Arc;
    use alloc::vec;

    use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
    use execution_tape::host::{Host, HostContext, HostError, SigHash, ValueRef};
    use execution_tape::program::ValueType;
    use execution_tape::value::Value;
    use execution_tape::vm::Limits;

    use super::*;
    use crate::executor::Executor;
    use crate::native::{FnExecutor, FnNode};
    use crate::node_access::NodeAccess;
    use crate::tape::{TapeError, TapeExecutor, TapeNode};

    #[derive(Debug)]
    struct HostNoop;

    impl Host for HostNoop {
        fn call(
            &mut self,
            _symbol: &str,
            _sig_hash: SigHash,
            _args: &[ValueRef<'_>],
            _rets: &mut [Value],
            _ctx: HostContext<'_, '_>,
        ) -> Result<u64, HostError> {
            Err(HostError::UnknownSymbol)
        }
    }

    #[derive(Debug)]
    enum MixedNode {
        Native(FnNode<Value, TapeError>),
        Tape(TapeNode),
    }

    #[derive(Debug)]
    struct MixedExecutor {
        native: FnExecutor<Value, TapeError>,
        tape: TapeExecutor<HostNoop>,
    }

    impl Executor for MixedExecutor {
        type Value = Value;
        type Node = MixedNode;
        type Error = TapeError;

        fn execute(
            &mut self,
            node: &mut Self::Node,
            inputs: &[Self::Value],
            outputs: &mut Vec<Self::Value>,
            access: &mut NodeAccess<'_>,
        ) -> Result<(), Self::Error> {
            match node {
                MixedNode::Native(body) => self.native.execute(body, inputs, outputs, access),
                MixedNode::Tape(body) => self.tape.execute(body, inputs, outputs, access),
            }
        }

        fn describe(&self, node: &Self::Node) -> Option<String> {
            match node {
                MixedNode::Native(body) => self.native.describe(body),
                MixedNode::Tape(body) => self.tape.describe(body),
            }
        }
    }

    #[test]
    fn native_and_tape_nodes_share_one_graph() {
        // Tape: fn double(x: i64) -> i64 { x + x }
        let mut pb = ProgramBuilder::new();
        let mut a = Asm::new();
        a.i64_add(2, 1, 1);
        a.ret(0, &[2]);
        let entry = pb
            .push_function_checked(
                a,
                FunctionSig {
                    arg_types: vec![ValueType::I64],
                    ret_types: vec![ValueType::I64],
                },
            )
            .unwrap();
        pb.set_function_output_name(entry, 0, "doubled").unwrap();
        let program = Arc::new(pb.build_verified().unwrap());
        let tape_node = TapeNode::new(program, entry).unwrap();

        let mut g = ExecutionGraph::new(MixedExecutor {
            native: FnExecutor::new(),
            tape: TapeExecutor::new(HostNoop, Limits::default()),
        });
        let output_names = tape_node.output_names();
        let double = g
            .add_node(MixedNode::Tape(tape_node), vec!["x".into()], output_names)
            .unwrap();
        let increment = g
            .add_node(
                MixedNode::Native(FnNode::named("increment", |inputs, outputs, _access| {
                    let Value::I64(v) = inputs[0] else {
                        unreachable!("tape output is typed i64");
                    };
                    outputs.push(Value::I64(v + 1));
                    Ok(())
                })),
                vec!["value".into()],
                vec!["result".into()],
            )
            .unwrap();
        g.set_input_value(double, "x", Value::I64(20)).unwrap();
        g.connect(double, "doubled", increment, "value").unwrap();

        assert_eq!(g.run_all().unwrap().executed_nodes, 2);
        assert_eq!(
            g.node_outputs(increment).unwrap().get("result"),
            Some(&Value::I64(41))
        );
        assert_eq!(g.node_description(increment).as_deref(), Some("increment"));
        assert_eq!(g.node_description(double).as_deref(), Some("entry=f0"));

        g.set_input_value(double, "x", Value::I64(21)).unwrap();
        g.invalidate_input("x");
        assert_eq!(g.run_all().unwrap().executed_nodes, 2);
        assert_eq!(
            g.node_outputs(increment).unwrap().get("result"),
            Some(&Value::I64(43))
        );
    }
}
