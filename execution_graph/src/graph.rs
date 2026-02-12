// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Minimal execution graph with dirty-tracked incremental re-execution.

use core::fmt;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::vec::Vec;
use core::cell::Cell;

use execution_tape::host::Host;
use execution_tape::host::ResourceKeyRef;
use execution_tape::host::SigHash;
use execution_tape::trace::{TraceMask, TraceSink};
use execution_tape::value::{FuncId, Value};
use execution_tape::verifier::VerifiedProgram;
use execution_tape::vm::{ExecutionContext, Limits, Vm};

use crate::access::{Access, AccessLog, HostOpId, NodeId, ResourceKey};
use crate::dirty::{DirtyEngine, DirtyKey};
use crate::dispatch::{Dispatcher, InlineDispatcher};
use crate::plan::{RunPlan, RunPlanTrace};
use crate::report::{NodeRunDetail, ReportDetailMask, RunDetailReport, RunSummary};
use crate::tape_access::{CountingAccessSink, StrictDepsTrace};

use understory_dirty::TraversalScratch;
use understory_dirty::trace::OneParentRecorder;

/// Graph execution errors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphError {
    /// A node id was invalid.
    BadNodeId,
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
    /// The node returned an unexpected number of outputs.
    BadOutputArity {
        /// Node that produced outputs.
        node: NodeId,
    },
    /// Strict deps mode error: a host op recorded no access keys.
    StrictDepsViolation {
        /// Node whose execution contained the violating host call.
        node: NodeId,
        /// Host call symbol.
        symbol: Box<str>,
        /// Signature hash carried in bytecode/program.
        sig_hash: SigHash,
    },
    /// VM execution trapped.
    Trap,
}

impl fmt::Display for GraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadNodeId => write!(f, "bad node id"),
            Self::MissingInput { node, name } => {
                write!(
                    f,
                    "missing input binding: node={} name={name}",
                    node.as_u64()
                )
            }
            Self::MissingUpstreamOutput { node, name } => {
                write!(
                    f,
                    "missing upstream output: upstream_node={} output={name}",
                    node.as_u64()
                )
            }
            Self::BadOutputArity { node } => {
                write!(
                    f,
                    "node produced unexpected output arity: node={}",
                    node.as_u64()
                )
            }
            Self::StrictDepsViolation {
                node,
                symbol,
                sig_hash,
            } => write!(
                f,
                "strict deps violation: node={} host_call={symbol} sig_hash={}",
                node.as_u64(),
                sig_hash.0
            ),
            Self::Trap => write!(f, "vm trapped during execution"),
        }
    }
}

impl core::error::Error for GraphError {}

/// Stable output map for a node run.
pub type NodeOutputs = BTreeMap<Box<str>, Value>;

#[derive(Clone, Debug)]
pub(crate) enum Binding {
    External {
        value: Value,
        read_id: DirtyKey,
    },
    FromNode {
        node: NodeId,
        output: Box<str>,
        read_id: DirtyKey,
    },
}

#[derive(Debug)]
pub(crate) struct Node {
    pub(crate) program: VerifiedProgram,
    pub(crate) entry: FuncId,
    pub(crate) input_names: Vec<Box<str>>,
    pub(crate) input_slots: BTreeMap<Box<str>, Vec<usize>>,
    pub(crate) inputs: Vec<Option<Binding>>,
    pub(crate) output_names: Vec<Box<str>>,
    pub(crate) output_ids: Vec<DirtyKey>,
    pub(crate) outputs: NodeOutputs,
    pub(crate) last_access: Option<AccessLog>,
    pub(crate) last_read_ids: Vec<DirtyKey>,
    pub(crate) deps_initialized: bool,
    pub(crate) run_count: u64,
}

impl Node {
    fn output_name_at(&self, index: usize) -> Box<str> {
        self.output_names
            .get(index)
            .cloned()
            .unwrap_or_else(|| format!("ret{index}").into_boxed_str())
    }
}

/// Execution graph whose nodes are `execution_tape` entrypoints.
///
/// This is an early, minimal implementation intended to support incremental scheduling work.
///
/// ## Semantics
///
/// - External inputs are identified by name. A node input binding with name `"foo"` will record
///   reads of [`ResourceKey::Input("foo")`](ResourceKey::Input) when executed.
/// - To invalidate an input, call [`ExecutionGraph::invalidate_input`] with the same name string
///   that was used when binding the value via [`ExecutionGraph::set_input_value`].
/// - Additional dependency reads/writes can be recorded by host calls via
///   `execution_tape::host::AccessSink`, and are translated into [`ResourceKey`] values.
///   If you want to invalidate using the tape key type directly, use
///   [`ExecutionGraph::invalidate_tape_key`].
/// - Dependencies are refined dynamically: after each run, each output key’s dependency set is
///   replaced with “all reads observed during that run”. The [`connect`](ExecutionGraph::connect)
///   method adds conservative edges to enforce initial topological ordering before the first run.
/// - [`ExecutionGraph::run_all`] / [`ExecutionGraph::run_node`] execute dirty work and return a
///   cheap executed-node summary.
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
pub struct ExecutionGraph<H: Host> {
    vm: Vm<H>,
    ctx: ExecutionContext,
    dirty: DirtyEngine,
    input_ids: BTreeMap<Box<str>, DirtyKey>,
    pub(crate) nodes: Vec<Node>,
    scratch: Scratch,
    strict_deps: bool,
    collect_access: bool,
}

#[derive(Debug, Default)]
struct Scratch {
    to_run: Vec<NodeId>,
    seen_stamp: Vec<u32>,
    read_ids: Vec<DirtyKey>,
    args: Vec<Value>,
    stamp: u32,
}

impl Scratch {
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
}

impl<H: Host> ExecutionGraph<H> {
    /// Creates an empty graph.
    #[must_use]
    pub fn new(host: H, limits: Limits) -> Self {
        Self {
            vm: Vm::new(host, limits),
            ctx: ExecutionContext::new(),
            dirty: DirtyEngine::new(),
            input_ids: BTreeMap::new(),
            nodes: Vec::new(),
            scratch: Scratch::default(),
            strict_deps: false,
            collect_access: false,
        }
    }

    /// Enables or disables strict dependency tracking for host calls.
    ///
    /// When enabled, each host call is required to record at least one access key via the access
    /// sink. This is a debugging mode intended to prevent silently unsound incremental execution
    /// caused by missing access reporting.
    pub fn set_strict_deps(&mut self, strict: bool) {
        self.strict_deps = strict;
    }

    /// Enables or disables collection of per-node access logs.
    ///
    /// When enabled, each node's full [`AccessLog`] (bindings, tape accesses, output writes) is
    /// stored after execution and can be retrieved with [`ExecutionGraph::node_last_access`].
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

    /// Adds a node and returns its [`NodeId`].
    ///
    /// `input_names` defines the mapping from per-node binding names to positional function args.
    pub fn add_node(
        &mut self,
        program: VerifiedProgram,
        entry: FuncId,
        input_names: Vec<Box<str>>,
    ) -> NodeId {
        let node = NodeId::new(u64::try_from(self.nodes.len()).unwrap_or(u64::MAX));

        let program_ref = program.program();
        let ret_count = program_ref
            .functions
            .get(entry.0 as usize)
            .map(|f| f.ret_count as usize)
            .unwrap_or(0);

        let mut output_names: Vec<Box<str>> = Vec::with_capacity(ret_count);
        for i in 0..ret_count {
            let ret = u32::try_from(i).unwrap_or(u32::MAX);
            let name = program_ref
                .function_output_name(entry.0, ret)
                // Advisory: output names are optional in the tape format.
                // Use a predictable fallback so tooling can still function.
                // Callers that need stable wiring should set names explicitly.
                .unwrap_or("ret");
            if name == "ret" {
                output_names.push(format!("ret{i}").into_boxed_str());
            } else {
                output_names.push(name.into());
            }
        }

        // Intern output keys once at node creation time.
        let mut output_ids: Vec<DirtyKey> = Vec::with_capacity(output_names.len());
        for out_name in output_names.iter().cloned() {
            let id = self.dirty.intern(ResourceKey::tape_output(node, out_name));
            self.dirty.mark_dirty(id);
            output_ids.push(id);
        }

        let mut input_slots: BTreeMap<Box<str>, Vec<usize>> = BTreeMap::new();
        for (slot, name) in input_names.iter().enumerate() {
            input_slots.entry(name.clone()).or_default().push(slot);
        }
        let input_count = input_names.len();

        let n = Node {
            program,
            entry,
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
        node
    }

    /// Binds a named input to a concrete value.
    ///
    /// The `name` is part of the dependency key space. If you later want to trigger re-execution
    /// of nodes that read this input, call [`ExecutionGraph::invalidate_input`] with the same
    /// `name` string.
    ///
    /// If a node declares duplicate input names (for example `["x", "x"]`), those slots are
    /// treated as aliases: setting `"x"` binds all matching slots.
    pub fn set_input_value(&mut self, node: NodeId, name: impl Into<Box<str>>, value: Value) {
        let Ok(index) = usize::try_from(node.as_u64()) else {
            return;
        };
        let name: Box<str> = name.into();
        // Validate node and slot exist before interning to avoid memory churn on bad inputs.
        if self
            .nodes
            .get(index)
            .and_then(|n| n.input_slots.get(name.as_ref()))
            .is_none()
        {
            return;
        }
        let read_id = self.dirty.intern(ResourceKey::input(name.clone()));
        let n = &mut self.nodes[index];
        for &slot in n.input_slots.get(name.as_ref()).unwrap() {
            if let Some(binding) = n.inputs.get_mut(slot) {
                *binding = Some(Binding::External {
                    value: value.clone(),
                    read_id,
                });
            }
        }
    }

    /// Connects `from.output` into `to.input`.
    ///
    /// If `to` declares duplicate input names, all slots matching `to.input` are connected.
    pub fn connect(
        &mut self,
        from: NodeId,
        output: impl Into<Box<str>>,
        to: NodeId,
        input: impl Into<Box<str>>,
    ) {
        let output: Box<str> = output.into();
        let input: Box<str> = input.into();
        let Ok(index) = usize::try_from(to.as_u64()) else {
            return;
        };
        // Validate target node exists before interning to avoid memory churn on bad inputs.
        if self.nodes.get(index).is_none() {
            return;
        }
        let read_id = self
            .dirty
            .intern(ResourceKey::tape_output(from, output.clone()));
        if let Some(n) = self.nodes.get_mut(index)
            && let Some(slots) = n.input_slots.get(input.as_ref())
        {
            for &slot in slots {
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
        // refines dependencies via `AccessLog`.
        //
        // This ensures initial runs are topologically ordered even before dependencies have been
        // observed dynamically.
        let Ok(to_index) = usize::try_from(to.as_u64()) else {
            return;
        };
        let Some(to_node) = self.nodes.get(to_index) else {
            return;
        };
        let output_count = to_node.output_ids.len();
        let src = read_id;
        for output_ix in 0..output_count {
            let dst = self.nodes[to_index].output_ids[output_ix];
            self.dirty.add_dependency(dst, src);
            self.dirty.mark_dirty(dst);
        }
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
    /// ([`ResourceKey::Input`]), host-managed state ([`ResourceKey::HostState`]), or conservative
    /// opaque host state ([`ResourceKey::OpaqueHost`]).
    #[inline]
    pub fn invalidate(&mut self, key: ResourceKey) {
        let id = self.dirty.intern(key);
        self.dirty.mark_dirty(id);
    }

    /// Marks a tape host key dirty.
    ///
    /// This accepts the borrowed key type used by `execution_tape` host access reporting.
    /// - `Input` keys are routed through [`ExecutionGraph::invalidate_input`].
    /// - `HostState` and `OpaqueHost` keys are mapped into their owned [`ResourceKey`] form.
    #[inline]
    pub fn invalidate_tape_key(&mut self, key: ResourceKeyRef<'_>) {
        match key {
            ResourceKeyRef::Input(name) => self.invalidate_input(name),
            ResourceKeyRef::HostState { op, key } => {
                self.invalidate(ResourceKey::host_state(HostOpId::new(op.0), key));
            }
            ResourceKeyRef::OpaqueHost { op } => {
                self.invalidate(ResourceKey::opaque_host(HostOpId::new(op.0)));
            }
        }
    }

    #[inline]
    fn intern_input_id(&mut self, name: &str) -> DirtyKey {
        if let Some(&id) = self.input_ids.get(name) {
            return id;
        }

        // Note: we may allocate twice on first use (once for the lookup table key and once for
        // the `ResourceKey::Input` stored in the interner). Subsequent invalidations are
        // allocation-free.
        let boxed: Box<str> = name.into();
        let id = self.dirty.intern(ResourceKey::Input(boxed.clone()));
        self.input_ids.insert(boxed, id);
        id
    }

    /// Returns the most recent outputs for `node`, if present.
    #[must_use]
    #[inline]
    pub fn node_outputs(&self, node: NodeId) -> Option<&NodeOutputs> {
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

        for (_key_id, key) in self.dirty.drain() {
            Self::schedule_tape_output_key(&mut self.scratch, key);
        }

        RunPlan::all(core::mem::take(&mut self.scratch.to_run))
    }

    /// Builds a report-capable plan from all currently affected dirty work.
    #[inline]
    fn plan_all_report(&mut self, detail_mask: ReportDetailMask) -> RunPlan {
        let collect_because = detail_mask.contains(ReportDetailMask::BECAUSE_OF)
            || detail_mask.contains(ReportDetailMask::WHY_PATH);
        let collect_why = detail_mask.contains(ReportDetailMask::WHY_PATH);

        self.scratch.start_drain(self.nodes.len());
        let mut node_report: Vec<Option<NodeRunDetail>> = alloc::vec![None; self.nodes.len()];

        if collect_why {
            let mut trace_scratch = TraversalScratch::<DirtyKey>::new();
            let mut trace = OneParentRecorder::<DirtyKey>::new();
            trace.clear();

            let mut scheduled: Vec<(NodeId, DirtyKey, ResourceKey)> = Vec::new();
            for (key_id, key) in self.dirty.drain_traced(&mut trace_scratch, &mut trace) {
                let ResourceKey::TapeOutput { node, .. } = key else {
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

                let why_path = self
                    .dirty
                    .explain_path(&trace, key_id)
                    .unwrap_or_else(|| alloc::vec![because_of.clone()]);

                node_report[index] = Some(NodeRunDetail {
                    node,
                    because_of: if collect_because {
                        Some(because_of)
                    } else {
                        None
                    },
                    why_path: Some(why_path),
                });
            }
        } else {
            for (_key_id, key) in self.dirty.drain() {
                let ResourceKey::TapeOutput { node, .. } = key else {
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

                node_report[index] = Some(NodeRunDetail {
                    node: *node,
                    because_of: if collect_because {
                        Some(key.clone())
                    } else {
                        None
                    },
                    why_path: None,
                });
            }
        }

        let nodes = core::mem::take(&mut self.scratch.to_run);
        RunPlan::all(nodes).with_trace(RunPlanTrace::from_node_reports(node_report))
    }

    /// Builds a plan restricted to keys within the dependency closure of `node`'s outputs.
    #[inline]
    fn plan_within_dependencies_of(&mut self, node: NodeId) -> Result<RunPlan, GraphError> {
        let index = usize::try_from(node.as_u64()).map_err(|_| GraphError::BadNodeId)?;
        let n = self.nodes.get(index).ok_or(GraphError::BadNodeId)?;
        let output_count = n.output_ids.len();

        self.scratch.start_drain(self.nodes.len());
        for output_ix in 0..output_count {
            let out_id = self.nodes[index].output_ids[output_ix];
            for (_key_id, key) in self.dirty.drain_within_dependencies_of(out_id) {
                Self::schedule_tape_output_key(&mut self.scratch, key);
            }
        }

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
    ) -> Result<RunPlan, GraphError> {
        let Ok(index) = usize::try_from(node.as_u64()) else {
            return Err(GraphError::BadNodeId);
        };
        let Some(n) = self.nodes.get(index) else {
            return Err(GraphError::BadNodeId);
        };
        let output_count = n.output_ids.len();
        let collect_because = detail_mask.contains(ReportDetailMask::BECAUSE_OF)
            || detail_mask.contains(ReportDetailMask::WHY_PATH);
        let collect_why = detail_mask.contains(ReportDetailMask::WHY_PATH);

        self.scratch.start_drain(self.nodes.len());
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
                    let ResourceKey::TapeOutput { node, .. } = key else {
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

                    let why_path = self
                        .dirty
                        .explain_path(&trace, key_id)
                        .unwrap_or_else(|| alloc::vec![because_of.clone()]);

                    node_report[scheduled_index] = Some(NodeRunDetail {
                        node: scheduled_node,
                        because_of: if collect_because {
                            Some(because_of)
                        } else {
                            None
                        },
                        why_path: Some(why_path),
                    });
                }
            }
        } else {
            // Drain dirty keys within the dependency closure of each output, and execute nodes
            // whose output keys are affected.
            for output_ix in 0..output_count {
                let out_id = self.nodes[index].output_ids[output_ix];
                for (_key_id, key) in self.dirty.drain_within_dependencies_of(out_id) {
                    let ResourceKey::TapeOutput { node, .. } = key else {
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

                    node_report[scheduled_index] = Some(NodeRunDetail {
                        node: *node,
                        because_of: if collect_because {
                            Some(key.clone())
                        } else {
                            None
                        },
                        why_path: None,
                    });
                }
            }
        }

        let nodes = core::mem::take(&mut self.scratch.to_run);
        Ok(RunPlan::within_dependencies_of(node, nodes)
            .with_trace(RunPlanTrace::from_node_reports(node_report)))
    }

    #[inline]
    fn schedule_tape_output_key(scratch: &mut Scratch, key: &ResourceKey) {
        let ResourceKey::TapeOutput { node, .. } = key else {
            return;
        };
        let _ = scratch.take_node(*node);
    }

    /// Executes a pre-built run plan without traced reporting.
    #[inline]
    fn run_plan(&mut self, plan: RunPlan) -> Result<RunSummary, GraphError> {
        let executed_nodes = plan.node_count();
        let mut dispatcher = InlineDispatcher;
        let to_run = dispatcher.dispatch(self, plan)?;
        // Reclaim the drained schedule buffer to reuse its capacity on the next planning pass.
        self.scratch.to_run = to_run;
        Ok(RunSummary { executed_nodes })
    }

    /// Executes a pre-built run plan and returns traced reporting data if attached.
    #[inline]
    fn run_plan_with_report(&mut self, plan: RunPlan) -> Result<RunDetailReport, GraphError> {
        let mut dispatcher = InlineDispatcher;
        let (to_run, report) = dispatcher.dispatch_with_report(self, plan)?;
        // Reclaim the drained schedule buffer to reuse its capacity on the next planning pass.
        self.scratch.to_run = to_run;
        Ok(report)
    }

    /// Runs all currently dirty work in dependency order and returns a cheap summary.
    pub fn run_all(&mut self) -> Result<RunSummary, GraphError> {
        let plan = self.plan_all();
        self.run_plan(plan)
    }

    /// Runs all currently dirty work and returns a structured report.
    ///
    /// Detail payloads are selected by `detail_mask`; this keeps heavy cause-path construction
    /// opt-in. Use [`ReportDetailMask::FULL`] for the full path-rich report.
    pub fn run_all_with_report(
        &mut self,
        detail_mask: ReportDetailMask,
    ) -> Result<RunDetailReport, GraphError> {
        let plan = self.plan_all_report(detail_mask);
        self.run_plan_with_report(plan)
    }

    /// Runs the subgraph needed to (re)compute `node`, executing only what is currently dirty.
    ///
    /// This drains only dirty keys that are within the dependency closure of `node`'s outputs.
    /// Unrelated dirty work remains dirty and is not drained.
    pub fn run_node(&mut self, node: NodeId) -> Result<RunSummary, GraphError> {
        let plan = self.plan_within_dependencies_of(node)?;
        self.run_plan(plan)
    }

    /// Runs the subgraph needed to (re)compute `node` and returns a structured report.
    ///
    /// Detail payloads are selected by `detail_mask`; this keeps heavy cause-path construction
    /// opt-in. Use [`ReportDetailMask::FULL`] for the full path-rich report.
    pub fn run_node_with_report(
        &mut self,
        node: NodeId,
        detail_mask: ReportDetailMask,
    ) -> Result<RunDetailReport, GraphError> {
        let plan = self.plan_within_dependencies_of_report(node, detail_mask)?;
        self.run_plan_with_report(plan)
    }

    /// Internal dispatch hook: executes one already-scheduled node.
    #[inline]
    pub(crate) fn execute_scheduled_node(&mut self, node: NodeId) -> Result<(), GraphError> {
        self.run_node_internal(node)
    }

    fn run_node_internal(&mut self, node: NodeId) -> Result<(), GraphError> {
        let node_index = usize::try_from(node.as_u64()).map_err(|_| GraphError::BadNodeId)?;
        let Some(n) = self.nodes.get(node_index) else {
            return Err(GraphError::BadNodeId);
        };

        let collect_access = self.collect_access;

        // Build args and (optionally) access log. In the fast path we build read_ids directly.
        // Take args out of scratch to allow disjoint borrows of self.vm / self.ctx.
        let mut args = core::mem::take(&mut self.scratch.args);
        args.clear();
        let mut log = AccessLog::new();

        self.scratch.read_ids.clear();

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
                    if collect_access {
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
                    if collect_access {
                        log.push(Access::Read(ResourceKey::tape_output(*up, output.clone())));
                    }
                    args.push(v.clone());
                }
            }
        }

        // Execute, capturing host accesses.
        let access_count: Cell<usize> = Cell::new(0);
        let mut tape_access = CountingAccessSink::new(&access_count);
        let mut strict = StrictDepsTrace::new(&access_count);

        let (trace_mask, trace) = if self.strict_deps {
            (TraceMask::HOST, Some(&mut strict as &mut dyn TraceSink))
        } else {
            (TraceMask::NONE, None)
        };

        let out = self
            .vm
            .run_with_ctx(
                &mut self.ctx,
                &self.nodes[node_index].program,
                self.nodes[node_index].entry,
                &args,
                trace_mask,
                trace,
                Some(&mut tape_access),
            )
            .map_err(|_| GraphError::Trap)?;

        // Restore args buffer to scratch for reuse on next run.
        self.scratch.args = args;

        if self.strict_deps
            && let Some(v) = strict.violation()
        {
            return Err(GraphError::StrictDepsViolation {
                node,
                symbol: v.symbol.clone(),
                sig_hash: v.sig_hash,
            });
        }

        // Merge tape-recorded accesses (host state, opaque ops, etc).
        for a in tape_access.log().iter() {
            if let Access::Read(k) = a {
                self.scratch.read_ids.push(self.dirty.intern(k.clone()));
            }
            if collect_access {
                log.push(a.clone());
            }
        }

        // Map outputs.
        let retc = out.len();
        if retc != self.nodes[node_index].output_names.len() {
            return Err(GraphError::BadOutputArity { node });
        }

        // Update outputs in-place when the BTreeMap is already populated (subsequent runs).
        {
            let n = &mut self.nodes[node_index];
            let first_run = n.outputs.is_empty();
            for (i, v) in out.into_iter().enumerate() {
                if first_run {
                    let name = n.output_name_at(i);
                    if collect_access {
                        log.push(Access::Write(ResourceKey::tape_output(node, name.clone())));
                    }
                    n.outputs.insert(name, v);
                } else {
                    if collect_access {
                        let name = n.output_names[i].clone();
                        log.push(Access::Write(ResourceKey::tape_output(node, name)));
                    }
                    let slot = n.outputs.get_mut(n.output_names[i].as_ref());
                    debug_assert!(
                        slot.is_some(),
                        "output key invariant broken: output_names[{i}] not found in outputs map"
                    );
                    if let Some(slot) = slot {
                        *slot = v;
                    }
                }
            }
        }

        // Update dirty dependencies: each output depends on all reads observed during the run.
        // Canonicalize to set semantics so host/read emission order does not cause
        // spurious dependency-set "changes" across runs.
        self.scratch.read_ids.sort_unstable();
        self.scratch.read_ids.dedup();

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
        self.nodes[node_index].last_access = if collect_access { Some(log) } else { None };
        self.nodes[node_index].run_count = self.nodes[node_index].run_count.saturating_add(1);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::access::HostOpId;
    use alloc::vec;
    use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
    use execution_tape::host::{AccessSink, HostError, SigHash, ValueRef};
    use execution_tape::host::{HostSig, ResourceKeyRef, sig_hash};
    use execution_tape::program::ValueType;
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
            _access: Option<&mut dyn AccessSink>,
        ) -> Result<(Vec<Value>, u64), HostError> {
            Err(HostError::UnknownSymbol)
        }
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
                    reg_count: 2,
                },
            )
            .unwrap();
        pb.set_function_output_name(a_node, 0, "value").unwrap();

        let a_prog = pb.build_verified().unwrap();

        let mut g = ExecutionGraph::new(HostNoop, Limits::default());
        let na = g.add_node(a_prog, a_node, vec![]);
        g.run_all().unwrap();
        let first = g.node_run_count(na).unwrap();
        g.run_all().unwrap();
        let second = g.node_run_count(na).unwrap();
        assert_eq!(first, 1);
        assert_eq!(second, 1);
    }

    #[test]
    fn run_node_leaves_unrelated_dirty_work_dirty() {
        fn make_identity_program(output_name: &str) -> (VerifiedProgram, FuncId) {
            let mut pb = ProgramBuilder::new();
            let mut a = Asm::new();
            a.ret(0, &[1]);
            let f = pb
                .push_function_checked(
                    a,
                    FunctionSig {
                        arg_types: vec![ValueType::I64],
                        ret_types: vec![ValueType::I64],
                        reg_count: 2,
                    },
                )
                .unwrap();
            pb.set_function_output_name(f, 0, output_name).unwrap();
            (pb.build_verified().unwrap(), f)
        }

        let mut g = ExecutionGraph::new(HostNoop, Limits::default());
        let (a_prog, a_entry) = make_identity_program("value");
        let (b_prog, b_entry) = make_identity_program("value");

        // Target chain: A -> B
        let na = g.add_node(a_prog, a_entry, vec!["a".into()]);
        let nb = g.add_node(b_prog, b_entry, vec!["b".into()]);
        g.set_input_value(na, "a", Value::I64(1));
        g.connect(na, "value", nb, "b");

        // Many unrelated chains: X_i -> Y_i
        let mut unrelated_leaves: Vec<NodeId> = Vec::new();
        for i in 0..32_u64 {
            let (x_prog, x_entry) = make_identity_program("value");
            let (y_prog, y_entry) = make_identity_program("value");
            let nx = g.add_node(x_prog, x_entry, vec!["x".into()]);
            let ny = g.add_node(y_prog, y_entry, vec!["y".into()]);
            g.set_input_value(
                nx,
                "x",
                Value::I64(10 + i64::try_from(i).unwrap_or(i64::MAX)),
            );
            g.connect(nx, "value", ny, "y");
            unrelated_leaves.push(ny);
        }

        g.run_all().unwrap();
        assert_eq!(g.node_run_count(nb), Some(1));
        for &ny in &unrelated_leaves {
            assert_eq!(g.node_run_count(ny), Some(1));
        }

        // Dirty target chain and all unrelated chains.
        g.set_input_value(na, "a", Value::I64(2));
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
        fn make_identity_program(output_name: &str) -> (VerifiedProgram, FuncId) {
            let mut pb = ProgramBuilder::new();
            let mut a = Asm::new();
            a.ret(0, &[1]);
            let f = pb
                .push_function_checked(
                    a,
                    FunctionSig {
                        arg_types: vec![ValueType::I64],
                        ret_types: vec![ValueType::I64],
                        reg_count: 2,
                    },
                )
                .unwrap();
            pb.set_function_output_name(f, 0, output_name).unwrap();
            (pb.build_verified().unwrap(), f)
        }

        let mut g = ExecutionGraph::new(HostNoop, Limits::default());
        let (a_prog, a_entry) = make_identity_program("value");
        let (b_prog, b_entry) = make_identity_program("value");

        let na = g.add_node(a_prog, a_entry, vec!["a".into()]);
        let nb = g.add_node(b_prog, b_entry, vec!["b".into()]);
        g.set_input_value(na, "a", Value::I64(1));
        g.connect(na, "value", nb, "b");

        g.run_all().unwrap();

        g.set_input_value(na, "a", Value::I64(2));
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
            Some(&ResourceKey::tape_output(na, "value"))
        );

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
            Some(&ResourceKey::tape_output(nb, "value"))
        );
    }

    #[test]
    fn run_all_counts_executed_nodes() {
        fn make_identity_program(output_name: &str) -> (VerifiedProgram, FuncId) {
            let mut pb = ProgramBuilder::new();
            let mut a = Asm::new();
            a.ret(0, &[1]);
            let f = pb
                .push_function_checked(
                    a,
                    FunctionSig {
                        arg_types: vec![ValueType::I64],
                        ret_types: vec![ValueType::I64],
                        reg_count: 2,
                    },
                )
                .unwrap();
            pb.set_function_output_name(f, 0, output_name).unwrap();
            (pb.build_verified().unwrap(), f)
        }

        let mut g = ExecutionGraph::new(HostNoop, Limits::default());
        let (a_prog, a_entry) = make_identity_program("value");
        let (b_prog, b_entry) = make_identity_program("value");

        let na = g.add_node(a_prog, a_entry, vec!["a".into()]);
        let nb = g.add_node(b_prog, b_entry, vec!["b".into()]);
        g.set_input_value(na, "a", Value::I64(1));
        g.connect(na, "value", nb, "b");

        let first = g.run_all().unwrap();
        assert_eq!(first.executed_nodes, 2);

        let second = g.run_all().unwrap();
        assert_eq!(second.executed_nodes, 0);
    }

    #[test]
    fn run_node_with_report_can_skip_why_paths() {
        fn make_identity_program(output_name: &str) -> (VerifiedProgram, FuncId) {
            let mut pb = ProgramBuilder::new();
            let mut a = Asm::new();
            a.ret(0, &[1]);
            let f = pb
                .push_function_checked(
                    a,
                    FunctionSig {
                        arg_types: vec![ValueType::I64],
                        ret_types: vec![ValueType::I64],
                        reg_count: 2,
                    },
                )
                .unwrap();
            pb.set_function_output_name(f, 0, output_name).unwrap();
            (pb.build_verified().unwrap(), f)
        }

        let mut g = ExecutionGraph::new(HostNoop, Limits::default());
        let (a_prog, a_entry) = make_identity_program("value");
        let (b_prog, b_entry) = make_identity_program("value");

        let na = g.add_node(a_prog, a_entry, vec!["a".into()]);
        let nb = g.add_node(b_prog, b_entry, vec!["b".into()]);
        g.set_input_value(na, "a", Value::I64(1));
        g.connect(na, "value", nb, "b");

        g.run_all().unwrap();
        g.set_input_value(na, "a", Value::I64(2));
        g.invalidate_input("a");

        let minimal = g.run_node_with_report(nb, ReportDetailMask::NONE).unwrap();
        assert_eq!(minimal.executed.len(), 2);
        for e in &minimal.executed {
            assert!(e.because_of.is_none());
            assert!(e.why_path.is_none());
        }

        g.set_input_value(na, "a", Value::I64(3));
        g.invalidate_input("a");

        let because_only = g
            .run_node_with_report(nb, ReportDetailMask::BECAUSE_OF)
            .unwrap();
        assert_eq!(because_only.executed.len(), 2);
        for e in &because_only.executed {
            assert!(e.because_of.is_some());
            assert!(e.why_path.is_none());
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
                _access: Option<&mut dyn AccessSink>,
            ) -> Result<(Vec<Value>, u64), HostError> {
                if symbol != "no_access" {
                    return Err(HostError::UnknownSymbol);
                }
                Ok((vec![Value::I64(7)], 0))
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
                    reg_count: 3,
                },
            )
            .unwrap();
        pb.set_function_output_name(f, 0, "value").unwrap();

        let prog = pb.build_verified().unwrap();

        let mut g = ExecutionGraph::new(HostNoAccess, Limits::default());
        let n = g.add_node(prog, f, vec![]);
        g.set_strict_deps(true);

        assert_eq!(
            g.run_all(),
            Err(GraphError::StrictDepsViolation {
                node: n,
                symbol: "no_access".into(),
                sig_hash: sig_hash(&HostSig {
                    args: vec![ValueType::I64],
                    rets: vec![ValueType::I64],
                }),
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
                    reg_count: 2,
                },
            )
            .unwrap();
        pb.set_function_output_name(f, 0, "value").unwrap();
        let prog = pb.build_verified().unwrap();

        let mut g = ExecutionGraph::new(HostNoop, Limits::default());
        let n = g.add_node(prog, f, vec!["in".into()]);

        assert_eq!(
            g.run_all(),
            Err(GraphError::MissingInput {
                node: n,
                name: "in".into()
            })
        );
    }

    #[test]
    fn run_all_errors_on_missing_upstream_output() {
        fn make_const_program(output_name: &str, v: i64) -> (VerifiedProgram, FuncId) {
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
                        reg_count: 2,
                    },
                )
                .unwrap();
            pb.set_function_output_name(f, 0, output_name).unwrap();
            (pb.build_verified().unwrap(), f)
        }

        fn make_identity_program(output_name: &str) -> (VerifiedProgram, FuncId) {
            let mut pb = ProgramBuilder::new();
            let mut a = Asm::new();
            a.ret(0, &[1]);
            let f = pb
                .push_function_checked(
                    a,
                    FunctionSig {
                        arg_types: vec![ValueType::I64],
                        ret_types: vec![ValueType::I64],
                        reg_count: 2,
                    },
                )
                .unwrap();
            pb.set_function_output_name(f, 0, output_name).unwrap();
            (pb.build_verified().unwrap(), f)
        }

        let (a_prog, a_entry) = make_const_program("value", 7);
        let (b_prog, b_entry) = make_identity_program("value");

        let mut g = ExecutionGraph::new(HostNoop, Limits::default());
        let na = g.add_node(a_prog, a_entry, vec![]);
        let nb = g.add_node(b_prog, b_entry, vec!["x".into()]);

        // Wire a non-existent output name.
        g.connect(na, "does_not_exist", nb, "x");

        assert_eq!(
            g.run_all(),
            Err(GraphError::MissingUpstreamOutput {
                node: na,
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
                access: Option<&mut dyn AccessSink>,
            ) -> Result<(Vec<Value>, u64), HostError> {
                if symbol != "kv.get" {
                    return Err(HostError::UnknownSymbol);
                }
                if sig_hash != self.get_sig {
                    return Err(HostError::SignatureMismatch);
                }
                let [ValueRef::U64(key)] = args else {
                    return Err(HostError::Failed);
                };
                if let Some(a) = access {
                    a.read(ResourceKeyRef::HostState {
                        op: sig_hash,
                        key: *key,
                    });
                }
                let v = *self.kv.borrow().get(key).unwrap_or(&0);
                Ok((vec![Value::I64(v)], 0))
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
                    reg_count: 3,
                },
            )
            .unwrap();
        pb.set_function_output_name(f, 0, "value").unwrap();
        let prog = pb.build_verified().unwrap();

        let kv = Rc::new(RefCell::new(BTreeMap::new()));
        kv.borrow_mut().insert(1, 7);
        let host = KvHost {
            kv: kv.clone(),
            get_sig: get_hash,
        };

        let mut g = ExecutionGraph::new(host, Limits::default());
        let n = g.add_node(prog, f, vec![]);

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
                access: Option<&mut dyn AccessSink>,
            ) -> Result<(Vec<Value>, u64), HostError> {
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

                if let Some(sink) = access {
                    sink.read(ResourceKeyRef::HostState {
                        op: sig_hash,
                        key: a,
                    });
                    sink.read(ResourceKeyRef::HostState {
                        op: sig_hash,
                        key: b,
                    });
                }
                Ok((vec![Value::I64(0)], 0))
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
                    reg_count: 2,
                },
            )
            .unwrap();
        pb.set_function_output_name(f, 0, "value").unwrap();
        let prog = pb.build_verified().unwrap();

        let mut g = ExecutionGraph::new(
            FlippingReadHost {
                flip: Rc::new(RefCell::new(false)),
                op_sig: op_hash,
            },
            Limits::default(),
        );
        let n = g.add_node(prog, f, vec![]);

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
                access: Option<&mut dyn AccessSink>,
            ) -> Result<(Vec<Value>, u64), HostError> {
                if symbol != "kv.get" {
                    return Err(HostError::UnknownSymbol);
                }
                if sig_hash != self.get_sig {
                    return Err(HostError::SignatureMismatch);
                }
                let [ValueRef::U64(key)] = args else {
                    return Err(HostError::Failed);
                };
                if let Some(a) = access {
                    a.read(ResourceKeyRef::OpaqueHost { op: sig_hash });
                }
                let v = *self.kv.borrow().get(key).unwrap_or(&0);
                Ok((vec![Value::I64(v)], 0))
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
                    reg_count: 3,
                },
            )
            .unwrap();
        pb.set_function_output_name(f, 0, "value").unwrap();
        let prog = pb.build_verified().unwrap();

        let kv = Rc::new(RefCell::new(BTreeMap::new()));
        kv.borrow_mut().insert(1, 7);
        let host = KvHost {
            kv: kv.clone(),
            get_sig: get_hash,
        };

        let mut g = ExecutionGraph::new(host, Limits::default());
        let n = g.add_node(prog, f, vec![]);

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
        fn make_identity_program(output_name: &str) -> (VerifiedProgram, FuncId) {
            let mut pb = ProgramBuilder::new();
            let mut a = Asm::new();
            a.ret(0, &[1]);
            let f = pb
                .push_function_checked(
                    a,
                    FunctionSig {
                        arg_types: vec![ValueType::I64],
                        ret_types: vec![ValueType::I64],
                        reg_count: 2,
                    },
                )
                .unwrap();
            pb.set_function_output_name(f, 0, output_name).unwrap();
            (pb.build_verified().unwrap(), f)
        }

        let (a_prog, a_entry) = make_identity_program("value");
        let (b_prog, b_entry) = make_identity_program("value");
        let (c_prog, c_entry) = make_identity_program("value");

        let mut g = ExecutionGraph::new(HostNoop, Limits::default());
        let na = g.add_node(a_prog, a_entry, vec!["in".into()]);
        let nb = g.add_node(b_prog, b_entry, vec!["x".into()]);
        let nc = g.add_node(c_prog, c_entry, vec!["y".into()]);

        g.set_input_value(na, "in", Value::I64(7));
        g.connect(na, "value", nb, "x");
        g.connect(nb, "value", nc, "y");

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
        g.set_input_value(na, "in", Value::I64(8));
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
        fn make_identity_program(output_name: &str) -> (VerifiedProgram, FuncId) {
            let mut pb = ProgramBuilder::new();
            let mut a = Asm::new();
            a.ret(0, &[1]);
            let f = pb
                .push_function_checked(
                    a,
                    FunctionSig {
                        arg_types: vec![ValueType::I64],
                        ret_types: vec![ValueType::I64],
                        reg_count: 2,
                    },
                )
                .unwrap();
            pb.set_function_output_name(f, 0, output_name).unwrap();
            (pb.build_verified().unwrap(), f)
        }

        fn make_const_program(output_name: &str, v: i64) -> (VerifiedProgram, FuncId) {
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
                        reg_count: 2,
                    },
                )
                .unwrap();
            pb.set_function_output_name(f, 0, output_name).unwrap();
            (pb.build_verified().unwrap(), f)
        }

        let (a_prog, a_entry) = make_identity_program("value");
        let (b_prog, b_entry) = make_const_program("value", 9);

        let mut g = ExecutionGraph::new(HostNoop, Limits::default());
        let na = g.add_node(a_prog, a_entry, vec!["in".into()]);
        let nb = g.add_node(b_prog, b_entry, vec![]);

        // This creates conservative dirty edges from A -> B, but B has zero observed reads.
        g.connect(na, "value", nb, "not_an_input");
        g.set_input_value(na, "in", Value::I64(1));

        g.run_all().unwrap();
        assert_eq!(g.node_run_count(na), Some(1));
        assert_eq!(g.node_run_count(nb), Some(1));

        // If conservative deps were not replaced on first run, this would spuriously rerun B.
        g.set_input_value(na, "in", Value::I64(2));
        g.invalidate_input("in");
        g.run_all().unwrap();

        assert_eq!(g.node_run_count(na), Some(2));
        assert_eq!(g.node_run_count(nb), Some(1));
    }

    #[test]
    fn run_node_errors_on_bad_node_id() {
        let mut g = ExecutionGraph::new(HostNoop, Limits::default());
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
                    reg_count: 3,
                },
            )
            .unwrap();
        pb.set_function_output_name(f, 0, "value").unwrap();
        let prog = pb.build_verified().unwrap();

        let mut g = ExecutionGraph::new(HostNoop, Limits::default());
        let n = g.add_node(prog, f, vec!["x".into(), "x".into()]);
        g.set_input_value(n, "x", Value::I64(7));

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
                    reg_count: 2,
                },
            )
            .unwrap();
        pb.set_function_output_name(f, 0, "value").unwrap();
        let prog = pb.build_verified().unwrap();

        let mut g = ExecutionGraph::new(HostNoop, Limits::default());
        g.set_collect_access_log(true);
        let n = g.add_node(prog, f, vec![]);
        g.run_all().unwrap();

        let log = g.node_last_access(n);
        assert!(
            log.is_some(),
            "access log should be Some when collection is enabled"
        );
    }

    #[test]
    fn node_last_access_returns_none_after_collection_disabled_rerun() {
        fn make_identity_program(output_name: &str) -> (VerifiedProgram, FuncId) {
            let mut pb = ProgramBuilder::new();
            let mut a = Asm::new();
            a.ret(0, &[1]);
            let f = pb
                .push_function_checked(
                    a,
                    FunctionSig {
                        arg_types: vec![ValueType::I64],
                        ret_types: vec![ValueType::I64],
                        reg_count: 2,
                    },
                )
                .unwrap();
            pb.set_function_output_name(f, 0, output_name).unwrap();
            (pb.build_verified().unwrap(), f)
        }

        let (prog, entry) = make_identity_program("value");
        let mut g = ExecutionGraph::new(HostNoop, Limits::default());
        let n = g.add_node(prog, entry, vec!["in".into()]);
        g.set_input_value(n, "in", Value::I64(1));

        // Run with collection enabled — should produce a log.
        g.set_collect_access_log(true);
        g.run_all().unwrap();
        assert!(g.node_last_access(n).is_some());

        // Disable collection, rerun — stale log must be cleared.
        g.set_collect_access_log(false);
        g.set_input_value(n, "in", Value::I64(2));
        g.invalidate_input("in");
        g.run_all().unwrap();
        assert!(
            g.node_last_access(n).is_none(),
            "stale access log should be cleared after rerun with collection disabled"
        );
    }

    #[test]
    fn in_place_output_update_preserves_values_across_reruns() {
        fn make_identity_program(output_name: &str) -> (VerifiedProgram, FuncId) {
            let mut pb = ProgramBuilder::new();
            let mut a = Asm::new();
            a.ret(0, &[1]);
            let f = pb
                .push_function_checked(
                    a,
                    FunctionSig {
                        arg_types: vec![ValueType::I64],
                        ret_types: vec![ValueType::I64],
                        reg_count: 2,
                    },
                )
                .unwrap();
            pb.set_function_output_name(f, 0, output_name).unwrap();
            (pb.build_verified().unwrap(), f)
        }

        let (prog, entry) = make_identity_program("value");
        let mut g = ExecutionGraph::new(HostNoop, Limits::default());
        let n = g.add_node(prog, entry, vec!["in".into()]);

        // First run populates the output map.
        g.set_input_value(n, "in", Value::I64(10));
        g.run_all().unwrap();
        assert_eq!(
            g.node_outputs(n).unwrap().get("value"),
            Some(&Value::I64(10))
        );

        // Second run uses in-place update path.
        g.set_input_value(n, "in", Value::I64(20));
        g.invalidate_input("in");
        g.run_all().unwrap();
        assert_eq!(
            g.node_outputs(n).unwrap().get("value"),
            Some(&Value::I64(20))
        );

        // Third run confirms stability.
        g.set_input_value(n, "in", Value::I64(30));
        g.invalidate_input("in");
        g.run_all().unwrap();
        assert_eq!(
            g.node_outputs(n).unwrap().get("value"),
            Some(&Value::I64(30))
        );
        assert_eq!(g.node_run_count(n), Some(3));
    }
}
