// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Incremental execution graph built on `execution_tape`.
//!
//! This crate will provide a graph whose nodes are verified "tapes" (program entrypoints) and
//! whose edges represent data dependencies, enabling sound incremental re-execution via dirty
//! tracking.
//!
//! ## Input key semantics
//!
//! Incremental invalidation is keyed by [`ResourceKey::Input`]. The same input name string must be
//! used consistently: the name passed to [`ExecutionGraph::set_input_value`] must match the name
//! passed to [`ExecutionGraph::invalidate_input`] (otherwise the invalidation will not affect the
//! reads recorded by runs).
//!
//! ## Example
//!
//! ```
//! use std::sync::Arc;
//!
//! use execution_graph::ExecutionGraph;
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
//! let mut graph = ExecutionGraph::new(NoHost, Limits::default());
//! let node = graph.add_node(program, entry, vec!["x".into()])?;
//! graph.set_input_value(node, "x", Value::I64(41))?;
//!
//! let summary = graph.run_all()?;
//! assert_eq!(summary.executed_nodes, 1);
//! assert_eq!(
//!     graph.node_outputs(node).unwrap().get("y"),
//!     Some(&Value::I64(42))
//! );
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

#![no_std]

extern crate alloc;

mod access;
mod dirty;
mod dispatch;
mod graph;
mod plan;
mod pretty;
mod report;
mod tape_access;

pub use access::{Access, AccessLog, HostOpId, NodeId, ResourceKey};
pub use graph::{ExecutionGraph, GraphError, NodeOutputs};
pub use report::{NodeRunDetail, ReportDetailMask, RunDetailReport, RunSummary};
