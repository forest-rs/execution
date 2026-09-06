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
//! [`TapeExecutor`] runs verified `execution_tape` programs ([`TapeNode`]) as nodes; it is
//! behind the default-on `tape` feature. Embedders with their own node kinds implement
//! [`Executor`] directly.
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
//! Graph construction is checked at the public API boundary: `add_node`, `set_input_value`, and
//! `connect` return [`GraphError`] values for duplicate output names, input arity mismatches,
//! unknown input names, and unknown output names.
//!
//! ## Quick Start
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
//! - Planning drains all affected dirty work before execution starts, so a node whose re-run
//!   produces an unchanged output still re-runs its dependents (no early cutoff).
//! - The tape executor collapses VM traps to [`TapeError::Trap`] at the graph boundary rather
//!   than source-language diagnostics.

#![no_std]

extern crate alloc;

mod access;
mod dirty;
mod dispatch;
mod executor;
mod graph;
mod node_access;
mod plan;
mod pretty;
mod report;
#[cfg(feature = "tape")]
pub mod tape;

pub use access::{Access, AccessLog, HostOpId, NodeId, ResourceKey};
pub use executor::Executor;
pub use graph::{ExecutionGraph, GraphError, NodeOutputs};
pub use node_access::NodeAccess;
pub use report::{NodeRunDetail, ReportDetailMask, RunDetailReport, RunSummary};
#[cfg(feature = "tape")]
pub use tape::{TapeError, TapeExecutor, TapeNode};
