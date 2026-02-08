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

#![no_std]

extern crate alloc;

mod access;
mod dirty;
mod graph;
mod report;
mod tape_access;

pub use access::{Access, AccessLog, HostOpId, NodeId, ResourceKey};
pub use graph::{ExecutionGraph, GraphError, NodeOutputs};
pub use report::{NodeRunReport, RunReport};
