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
use crate::report::{NodeRunReport, RunReport};
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
enum Binding {
    External(Value),
    FromNode { node: NodeId, output: Box<str> },
}

#[derive(Debug)]
struct Node {
    program: VerifiedProgram,
    entry: FuncId,
    input_names: Vec<Box<str>>,
    inputs: BTreeMap<Box<str>, Binding>,
    output_names: Vec<Box<str>>,
    outputs: NodeOutputs,
    last_access: AccessLog,
    run_count: u64,
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
/// - `run_all` / `run_node` track *whether* something must re-run. If you also need “why re-ran”
///   reporting, use [`ExecutionGraph::run_all_with_report`] or
///   [`ExecutionGraph::run_node_with_report`].
#[derive(Debug)]
pub struct ExecutionGraph<H: Host> {
    vm: Vm<H>,
    ctx: ExecutionContext,
    dirty: DirtyEngine,
    input_ids: BTreeMap<Box<str>, DirtyKey>,
    nodes: Vec<Node>,
    scratch: Scratch,
    strict_deps: bool,
}

#[derive(Debug, Default)]
struct Scratch {
    to_run: Vec<NodeId>,
    seen_stamp: Vec<u32>,
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

        let n = Node {
            program,
            entry,
            input_names,
            inputs: BTreeMap::new(),
            output_names,
            outputs: BTreeMap::new(),
            last_access: AccessLog::new(),
            run_count: 0,
        };

        // Force an initial run by marking all outputs dirty.
        for out in n.output_names.iter().cloned() {
            let key = ResourceKey::tape_output(node, out);
            let id = self.dirty.intern(key);
            self.dirty.mark_dirty(id);
        }

        self.nodes.push(n);
        node
    }

    /// Binds a named input to a concrete value.
    ///
    /// The `name` is part of the dependency key space. If you later want to trigger re-execution
    /// of nodes that read this input, call [`ExecutionGraph::invalidate_input`] with the same
    /// `name` string.
    pub fn set_input_value(&mut self, node: NodeId, name: impl Into<Box<str>>, value: Value) {
        let Ok(index) = usize::try_from(node.as_u64()) else {
            return;
        };
        if let Some(n) = self.nodes.get_mut(index) {
            n.inputs.insert(name.into(), Binding::External(value));
        }
    }

    /// Connects `from.output` into `to.input`.
    pub fn connect(
        &mut self,
        from: NodeId,
        output: impl Into<Box<str>>,
        to: NodeId,
        input: impl Into<Box<str>>,
    ) {
        let output: Box<str> = output.into();
        let Ok(index) = usize::try_from(to.as_u64()) else {
            return;
        };
        if let Some(n) = self.nodes.get_mut(index) {
            n.inputs.insert(
                input.into(),
                Binding::FromNode {
                    node: from,
                    output: output.clone(),
                },
            );
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
        let src = self.dirty.intern(ResourceKey::tape_output(from, output));
        for out_name in to_node.output_names.iter().cloned() {
            let dst = self.dirty.intern(ResourceKey::tape_output(to, out_name));
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

    /// Runs all currently dirty work in dependency order.
    pub fn run_all(&mut self) -> Result<(), GraphError> {
        self.scratch.start_drain(self.nodes.len());

        for (_key_id, key) in self.dirty.drain() {
            let ResourceKey::TapeOutput { node, .. } = key else {
                continue;
            };
            let _ = self.scratch.take_node(*node);
        }

        let mut to_run = core::mem::take(&mut self.scratch.to_run);
        for node in to_run.drain(..) {
            self.run_node_internal(node)?;
        }
        self.scratch.to_run = to_run;

        Ok(())
    }

    /// Runs all currently dirty work and returns a report including “why re-ran” cause paths.
    ///
    /// The report records one plausible cause path per executed node (a spanning forest), not all
    /// possible causes.
    pub fn run_all_with_report(&mut self) -> Result<RunReport, GraphError> {
        self.scratch.start_drain(self.nodes.len());

        let mut node_report: Vec<Option<NodeRunReport>> = alloc::vec![None; self.nodes.len()];

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
            if index >= node_report.len() {
                continue;
            }
            if node_report[index].is_some() {
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
            node_report[index] = Some(NodeRunReport {
                node,
                because_of,
                why_path,
            });
        }

        let mut report = RunReport::default();
        let mut to_run = core::mem::take(&mut self.scratch.to_run);
        for node in to_run.drain(..) {
            self.run_node_internal(node)?;
            let Ok(index) = usize::try_from(node.as_u64()) else {
                continue;
            };
            if index >= node_report.len() {
                continue;
            }
            if let Some(r) = node_report[index].take() {
                report.executed.push(r);
            }
        }
        self.scratch.to_run = to_run;

        Ok(report)
    }

    /// Runs the subgraph needed to (re)compute `node`, executing only what is currently dirty.
    ///
    /// This drains only dirty keys that are within the dependency closure of `node`'s outputs.
    /// Unrelated dirty work remains dirty and is not drained.
    pub fn run_node(&mut self, node: NodeId) -> Result<(), GraphError> {
        let Ok(index) = usize::try_from(node.as_u64()) else {
            return Err(GraphError::BadNodeId);
        };
        let Some(n) = self.nodes.get(index) else {
            return Err(GraphError::BadNodeId);
        };

        // Drain dirty keys within the dependency closure of each output, and execute nodes whose
        // output keys are affected.
        self.scratch.start_drain(self.nodes.len());
        for out_name in n.output_names.iter().cloned() {
            let out_id = self.dirty.intern(ResourceKey::tape_output(node, out_name));
            for (_key_id, key) in self.dirty.drain_within_dependencies_of(out_id) {
                let ResourceKey::TapeOutput { node, .. } = key else {
                    continue;
                };
                let _ = self.scratch.take_node(*node);
            }
        }

        let mut to_run = core::mem::take(&mut self.scratch.to_run);
        for node in to_run.drain(..) {
            self.run_node_internal(node)?;
        }
        self.scratch.to_run = to_run;

        Ok(())
    }

    /// Runs the subgraph needed to (re)compute `node` and returns a report including “why re-ran”
    /// cause paths.
    ///
    /// The report records one plausible cause path per executed node (a spanning forest), not all
    /// possible causes.
    pub fn run_node_with_report(&mut self, node: NodeId) -> Result<RunReport, GraphError> {
        let Ok(index) = usize::try_from(node.as_u64()) else {
            return Err(GraphError::BadNodeId);
        };
        let Some(n) = self.nodes.get(index) else {
            return Err(GraphError::BadNodeId);
        };

        self.scratch.start_drain(self.nodes.len());
        let mut node_report: Vec<Option<NodeRunReport>> = alloc::vec![None; self.nodes.len()];

        let mut trace_scratch = TraversalScratch::<DirtyKey>::new();
        let mut trace = OneParentRecorder::<DirtyKey>::new();

        // Drain dirty keys within the dependency closure of each output, and execute nodes whose
        // output keys are affected.
        for out_name in n.output_names.iter().cloned() {
            let out_id = self.dirty.intern(ResourceKey::tape_output(node, out_name));

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
                if scheduled_index >= node_report.len() || node_report[scheduled_index].is_some() {
                    continue;
                }

                let why_path = self
                    .dirty
                    .explain_path(&trace, key_id)
                    .unwrap_or_else(|| alloc::vec![because_of.clone()]);

                node_report[scheduled_index] = Some(NodeRunReport {
                    node: scheduled_node,
                    because_of,
                    why_path,
                });
            }
        }

        let mut report = RunReport::default();
        let mut to_run = core::mem::take(&mut self.scratch.to_run);
        for node in to_run.drain(..) {
            self.run_node_internal(node)?;
            let Ok(index) = usize::try_from(node.as_u64()) else {
                continue;
            };
            if index >= node_report.len() {
                continue;
            }
            if let Some(r) = node_report[index].take() {
                report.executed.push(r);
            }
        }
        self.scratch.to_run = to_run;

        Ok(report)
    }

    fn run_node_internal(&mut self, node: NodeId) -> Result<(), GraphError> {
        let node_index = usize::try_from(node.as_u64()).map_err(|_| GraphError::BadNodeId)?;
        let Some(n) = self.nodes.get(node_index) else {
            return Err(GraphError::BadNodeId);
        };

        // Build args + access log.
        let mut args: Vec<Value> = Vec::with_capacity(n.input_names.len());
        let mut log = AccessLog::new();

        for name in n.input_names.iter() {
            let b = n.inputs.get(name).ok_or_else(|| GraphError::MissingInput {
                node,
                name: name.clone(),
            })?;

            match b {
                Binding::External(v) => {
                    log.push(Access::Read(ResourceKey::input(name.clone())));
                    args.push(v.clone());
                }
                Binding::FromNode { node: up, output } => {
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
                    log.push(Access::Read(ResourceKey::tape_output(*up, output.clone())));
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
        for a in tape_access.log().iter().cloned() {
            log.push(a);
        }

        // Map outputs.
        let retc = out.len();
        if retc != self.nodes[node_index].output_names.len() {
            return Err(GraphError::BadOutputArity { node });
        }

        let mut outputs: NodeOutputs = BTreeMap::new();
        for (i, v) in out.into_iter().enumerate() {
            let name = self.nodes[node_index].output_name_at(i);
            outputs.insert(name.clone(), v);
            log.push(Access::Write(ResourceKey::tape_output(node, name)));
        }

        // Update dirty dependencies: each output depends on all reads observed during the run.
        let reads: Vec<_> = log
            .iter()
            .filter_map(|a| match a {
                Access::Read(k) => Some(k.clone()),
                Access::Write(_) => None,
            })
            .map(|k| self.dirty.intern(k))
            .collect();

        for out_name in self.nodes[node_index].output_names.iter().cloned() {
            let out_id = self.dirty.intern(ResourceKey::tape_output(node, out_name));
            self.dirty.set_dependencies(out_id, reads.iter().copied());
        }

        // Commit outputs/log.
        self.nodes[node_index].outputs = outputs;
        self.nodes[node_index].last_access = log;
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

        let r = g.run_node_with_report(nb).unwrap();
        assert_eq!(r.executed.len(), 2);
        assert_eq!(r.executed[0].node, na);
        assert_eq!(r.executed[1].node, nb);

        assert_eq!(
            r.executed[0].why_path.first(),
            Some(&ResourceKey::input("a"))
        );
        assert_eq!(
            r.executed[0].why_path.last(),
            Some(&ResourceKey::tape_output(na, "value"))
        );

        assert_eq!(
            r.executed[1].why_path.first(),
            Some(&ResourceKey::input("a"))
        );
        assert_eq!(
            r.executed[1].why_path.last(),
            Some(&ResourceKey::tape_output(nb, "value"))
        );
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
    fn run_node_errors_on_bad_node_id() {
        let mut g = ExecutionGraph::new(HostNoop, Limits::default());
        assert_eq!(g.run_node(NodeId::new(999)), Err(GraphError::BadNodeId));
    }
}
