// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

// After you edit the crate's doc comment, regenerate README.md by running:
// cargo rdme --workspace-project=execution_graph --heading-base-level=0

//! Incremental execution graph with pluggable node executors.
//!
//! This crate provides a small `no_std` graph that runs nodes in dependency order and re-runs
//! only the nodes affected by a change. The graph owns node identity, input bindings, dependency
//! tracking, dirty propagation, scheduling, and reporting. What a node *is* and how it runs is
//! supplied by an [`Executor`]: the value type carried on edges, the node body type, and the
//! code that turns bound inputs into outputs.
//!
//! Two executors ship with the crate:
//! - [`FnExecutor`] runs native Rust closures ([`FnNode`]).
//! - [`TapeExecutor`] runs verified `execution_tape` programs ([`TapeNode`]); it is behind the
//!   default-on `tape` feature.
//!
//! An embedder that needs both kinds of node in one graph writes an executor whose node type is
//! an enum over the two and delegates each variant.
//!
//! ## Quick Start
//!
//! ```rust
//! use core::convert::Infallible;
//!
//! use execution_graph::{ExecutionGraph, FnExecutor, FnNode};
//!
//! fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let mut graph = ExecutionGraph::new(FnExecutor::<i64, Infallible>::new());
//!
//!     let double = graph.add_node(
//!         FnNode::named("double", |inputs, outputs, _access| {
//!             outputs.push(inputs[0] * 2);
//!             Ok(())
//!         }),
//!         vec!["x".into()],
//!         vec!["doubled".into()],
//!     )?;
//!     let increment = graph.add_node(
//!         FnNode::named("increment", |inputs, outputs, _access| {
//!             outputs.push(inputs[0] + 1);
//!             Ok(())
//!         }),
//!         vec!["value".into()],
//!         vec!["result".into()],
//!     )?;
//!
//!     graph.set_input_value(double, "x", 20)?;
//!     graph.connect(double, "doubled", increment, "value")?;
//!
//!     let summary = graph.run_all()?;
//!     assert_eq!(summary.executed_nodes, 2);
//!     assert_eq!(graph.node_outputs(increment).unwrap().get("result"), Some(&41));
//!
//!     // Nothing changed, so nothing re-runs.
//!     assert_eq!(graph.run_all()?.executed_nodes, 0);
//!
//!     // Invalidating an input by name re-runs exactly its dependents.
//!     graph.set_input_value(double, "x", 21)?;
//!     graph.invalidate_input("x");
//!     assert_eq!(graph.run_all()?.executed_nodes, 2);
//!     assert_eq!(graph.node_outputs(increment).unwrap().get("result"), Some(&43));
//!     Ok(())
//! }
//! ```
//!
//! ## Model
//!
//! - **Nodes** are executor-defined bodies with named positional inputs and named outputs.
//! - **Edges** represent data dependencies; they are recorded dynamically from each node run:
//!   - reading an external input records `ResourceKey::Input(name)`
//!   - reading another node's output records `ResourceKey::NodeOutput { node, output }`
//!   - executors record additional dependencies through the [`NodeAccess`] they receive
//! - **Invalidation** is done by name: calling `invalidate_input("foo")` marks the input key
//!   `ResourceKey::Input("foo")` dirty, which may trigger re-execution of transitive dependents.
//!
//! Input names are part of the dependency key space: the string you pass to `set_input_value(node,
//! "foo", ..)` must match the string you pass to `invalidate_input("foo")` for incremental
//! scheduling to work.
//!
//! Executor-managed state uses the same key space: if a node records a
//! [`ResourceKey::HostState { op, key }`](ResourceKey::HostState) read during execution, you can
//! invalidate that state later via [`ExecutionGraph::invalidate`].
//!
//! ## Early cutoff
//!
//! A node re-runs when something it read changed, but its new output may equal the old one (a
//! parameter nudged within a clamp, say). With early cutoff, such an unchanged output stops
//! propagation: a scheduled node whose reads all turn out unchanged is skipped instead of re-run.
//! [`RunSummary::cut_off_nodes`] counts these nodes and [`RunDetailReport::cut_off`] lists them
//! with the same cause detail as executed nodes.
//!
//! Whether an output changed is the executor's call, through [`Executor::values_equal`]. The
//! default says "changed" for every output, so cutoff is opt-in: enable it with
//! [`FnExecutor::with_value_eq`] or [`TapeExecutor::set_early_cutoff`]. Cutoff decisions are
//! per output and across runs:
//!
//! - graph inputs and executor state that were invalidated count as changed;
//! - a node output marked dirty directly forces its producer to run; its readers execute only
//!   when an output revision changes or another read changed;
//! - host writes advance resource revisions immediately, including writes from a failed attempt;
//! - pending readers outside a scoped run retain their causes and compare revisions when queried,
//!   so an unchanged output can cut off work across separate scoped runs;
//! - a node that has never run, or was rewired by `connect` or by `set_input_value` binding a
//!   different key since its last run, always runs; both rewirings also schedule the node.
//!
//! A cut-off node keeps the outputs, run count, and last access log of its last real run.
//!
//! Graph construction is checked at the public API boundary: `add_node`, `set_input_value`, and
//! `connect` return [`GraphError`] values for duplicate output names, input arity mismatches,
//! unknown input names, and unknown output names.
//!
//! ## Ports and node lifetime
//!
//! [`InputId`] and [`OutputId`] identify positional ports within a node. Resolve names once with
//! [`ExecutionGraph::input_id`] and [`ExecutionGraph::output_id`], then use
//! [`ExecutionGraph::connect_by_id`] and [`NodeOutputs::get_by_id`] for indexed access.
//! [`ExecutionGraph::input_name`] and [`ExecutionGraph::output_name`] resolve IDs for diagnostics.
//! External input keys are separate from input slots: [`ResourceKey::InputId`] identifies a
//! resource shared by any readers, while `InputId(0)` means the first slot of a particular node.
//! [`ExecutionGraph::set_input_value_by_id`] binds these explicitly.
//!
//! [`ExecutionGraph::remove_node`] retires a node identity permanently and returns ownership of
//! its body and cached outputs. Its consumers become dirty; stale connections report the reader,
//! removed producer, and requested output until reconnected. Physical node and resource slots
//! are reused without reusing public identities. [`ExecutionGraph::storage_stats`] reports live
//! counts and reusable capacities for checking create/remove workloads.
//!
//! [`ExecutionGraph::invalidate_many`] accepts and deduplicates a batch of resource keys.
//! Invalidating an unknown resource has no effect: future readers execute before they can cache
//! a value, and there is no existing consumer to invalidate.
//!
//! ## Dynamic output reads
//!
//! [`FnNode::restartable`] and custom [`Executor`] implementations can discover graph-output
//! dependencies while running. [`NodeAccess::read_node_output`] returns a cloned current value,
//! or `Ok(None)` when the producer needs execution or verification. Return [`NodeOutcome::Pending`]
//! to discard the tentative outputs/reads, update those producers, and restart the computation.
//! Missing or retired identities produce [`OutputReadError`], rather than a cached substitute.
//!
//! ```rust
//! use execution_graph::{ExecutionGraph, FnExecutor, FnNode, NodeOutcome, OutputId, OutputReadError};
//!
//! let mut graph = ExecutionGraph::new(FnExecutor::<i64, OutputReadError>::new());
//! let parent = graph.add_node(FnNode::new(|_, outputs, _| {
//!     outputs.push(10); Ok(())
//! }), vec![], vec!["out".into()])?;
//! let child = graph.add_node(FnNode::restartable(move |_, outputs, access| {
//!     let Some(value) = access.read_node_output(parent, OutputId::new(0))? else {
//!         return Ok(NodeOutcome::Pending);
//!     };
//!     outputs.push(value + 1);
//!     Ok(NodeOutcome::Complete)
//! }), vec![], vec!["out".into()])?;
//! let summary = graph.run_node(child)?;
//! assert_eq!(summary.executed_nodes, 2);
//! assert_eq!(summary.execution_attempts, 3); // child yields, parent publishes, child publishes
//! assert_eq!(graph.node_outputs(child).unwrap().get("out"), Some(&11));
//! # Ok::<(), execution_graph::GraphError<OutputReadError>>(())
//! ```
//!
//! Restarting is explicit: ordinary [`FnNode::new`] and [`FnNode::named`] closures never request
//! it. A completed attempt with unresolved reads is rejected. A pending attempt must be safe to
//! repeat and must not record host writes; arbitrary executor or external effects cannot be
//! detected or rolled back. A producer may perform effects when it completes, just as in a
//! statically wired graph. Execution errors remain failures, even after a pending read.
//!
//! Successful execution replaces the committed read set, so a conditional branch can remove a
//! dependency and discover it again later. Scheduling conservatively visits previously committed
//! dependencies before the reader; new runtime requests extend that scope. Native readers can
//! request tape outputs in a mixed executor, and tape input bindings can consume native outputs.
//! The tape adapter itself completes whole VM calls and does not suspend host calls.
//!
//! [`ExecutionGraph::node_dependencies`] exposes committed reads without enabling access logs.
//! [`ExecutionGraph::node_status`] distinguishes a current cache from pending or never-published
//! results; [`ExecutionGraph::node_outputs`] alone is a cached-value accessor. Status reflects
//! changes reported to the graph, not unreported host mutation. Run summaries and partial reports
//! count executor attempts and suspensions separately from successful publications.
//!
//! Run `cargo run -p execution_graph_examples --bin transforms` for conditional parent-world
//! reads, reset toggles, dependency inspection, and bitwise checks against a fresh evaluator.
//!
//! ## Tape programs
//!
//! With the `tape` feature, [`TapeExecutor`] runs verified `execution_tape` programs as nodes.
//! Host calls record dependency keys through `execution_tape::host::AccessSink`, and those keys
//! are translated into graph [`ResourceKey`]s.
//!
//! ```rust
//! # #[cfg(feature = "tape")]
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use std::sync::Arc;
//!
//! use execution_graph::{ExecutionGraph, TapeExecutor};
//! use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
//! use execution_tape::host::{Host, HostContext, HostError, SigHash, ValueRef};
//! use execution_tape::program::ValueType;
//! use execution_tape::value::Value;
//! use execution_tape::vm::Limits;
//!
//! struct NoHost;
//!
//! impl Host for NoHost {
//!     fn call(
//!         &mut self,
//!         _symbol: &str,
//!         _sig_hash: SigHash,
//!         _args: &[ValueRef<'_>],
//!         _rets: &mut [Value],
//!         _ctx: HostContext<'_, '_>,
//!     ) -> Result<u64, HostError> {
//!         Err(HostError::UnknownSymbol)
//!     }
//! }
//!
//! let mut asm = Asm::new();
//! asm.const_i64(2, 1);
//! asm.i64_add(3, 1, 2);
//! asm.ret(0, &[3]);
//!
//! let mut builder = ProgramBuilder::new();
//! let entry = builder.push_function_checked(
//!     asm,
//!     FunctionSig {
//!         arg_types: vec![ValueType::I64],
//!         ret_types: vec![ValueType::I64],
//!     },
//! )?;
//! builder.set_function_output_name(entry, 0, "y")?;
//! let program = Arc::new(builder.build_verified()?);
//!
//! let mut graph = ExecutionGraph::new(TapeExecutor::new(NoHost, Limits::default()));
//! let node = graph.add_tape_node(program, entry, vec!["x".into()])?;
//! graph.set_input_value(node, "x", Value::I64(41))?;
//!
//! let summary = graph.run_all()?;
//! assert_eq!(summary.executed_nodes, 1);
//! assert_eq!(graph.node_outputs(node).unwrap().get("y"), Some(&Value::I64(42)));
//! # Ok(())
//! # }
//! # #[cfg(not(feature = "tape"))]
//! # fn main() {}
//! ```
//!
//! [`TapeExecutor::set_strict_deps`] enables a debugging mode that rejects host calls which
//! record no access keys, so missing dependency reporting fails loudly instead of silently
//! reusing stale results.
//!
//! ## Execution behavior
//!
//! Each successful node publishes its outputs and recorded dependencies together, after output
//! arity and dependency validation. A failed node keeps its previous outputs, dependencies, and
//! access log and remains dirty. Earlier successful nodes remain committed if a later node fails:
//! a graph run is not a transaction. Executor and host mutations are never rolled back by the
//! graph; output handles must remain valid for as long as the graph owns them.
//!
//! Cyclic connections return [`GraphError::DependencyCycle`] without changing the graph's wiring.
//!
//! `run_node` drains and executes only the dirty work within the dependency closure of the target
//! node's outputs, leaving unrelated dirty work dirty to be handled by a later `run_all`.
//!
//! For low overhead telemetry, `run_all` / `run_node` return only an executed-node summary.
//!
//! For debugging and instrumentation:
//! - `run_all_with_report` / `run_node_with_report` accept a `ReportDetailMask` so you can choose
//!   cheaper detail levels (for example, node + immediate cause key without path tracing).
//! - Use `set_node_label` to attach advisory debug names that can appear in reports and DOT.
//! - Use `ReportDetailMask::FULL` when you want labels plus full per-node cause paths.
//! - If a report-producing run fails after some nodes complete, `GraphError::RunReportFailed`
//!   carries the partial report rows collected before the error.
//!
//! ## Demo
//!
//! Run the demo with:
//!
//! ```sh
//! cargo run -p execution_graph_examples --bin tax
//! ```
//!
//! Emit Graphviz DOT for the same graph:
//!
//! ```sh
//! cargo run -p execution_graph_examples --bin tax -- --dot
//! ```
//!
//! ## Current limitations
//!
//! - The tape executor collapses VM traps to [`TapeError::Trap`] at the graph boundary rather
//!   than source-language diagnostics.

#![no_std]

extern crate alloc;

mod access;
mod dirty;
mod dispatch;
mod executor;
#[cfg(test)]
mod freshness_tests;
mod graph;
mod key_arena;
#[cfg(test)]
mod lifecycle_tests;
mod native;
mod node_access;
mod nodes;
mod plan;
mod ports;
mod pretty;
mod report;
#[cfg(feature = "tape")]
pub mod tape;

pub use access::{Access, AccessLog, HostOpId, NodeId, ResourceKey};
pub use executor::{Executor, NodeOutcome};
pub use graph::{ExecutionGraph, GraphError, RemovedNode};
pub use native::{FnExecutor, FnNode, NodeFn, ValueEq};
pub use node_access::{NodeAccess, OutputReadError};
pub use ports::{InputId, NodeOutputs, OutputId};
pub use report::{
    GraphStorageStats, NodeRunDetail, NodeStatus, ReportDetailMask, RunDetailReport, RunSummary,
};
#[cfg(feature = "tape")]
pub use tape::{TapeError, TapeExecutor, TapeNode};

#[cfg(test)]
mod dynamic_tests;
