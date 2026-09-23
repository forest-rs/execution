// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The executor seam: what a node is and how it runs.
//!
//! [`ExecutionGraph`](crate::ExecutionGraph) owns scheduling, dependency tracking, invalidation,
//! and reporting. It does not know what a node body is. An [`Executor`] supplies that: the value
//! type carried on edges, the node body type, and the code that turns bound inputs into outputs.
//!
//! Two executors ship with this crate:
//! - [`FnExecutor`](crate::FnExecutor) runs native Rust closures.
//! - [`TapeExecutor`](crate::TapeExecutor) runs verified `execution_tape` programs (behind the
//!   `tape` feature).
//!
//! An embedder that needs both kinds of node in one graph writes an executor whose node type is an
//! enum over the two and delegates each variant.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use crate::node_access::NodeAccess;

/// Runs node bodies on behalf of an [`ExecutionGraph`](crate::ExecutionGraph).
///
/// The graph calls [`Executor::execute`] once per scheduled node, in dependency order, with the
/// node's bound input values. The executor must push exactly one value per declared output; the
/// graph reports [`GraphError::BadOutputArity`](crate::GraphError::BadOutputArity) otherwise.
///
/// ## Dependency reporting
///
/// Input bindings are recorded by the graph before the executor runs. Any *other* state a node
/// consults must be reported through the supplied [`NodeAccess`], otherwise incremental
/// re-execution can reuse a stale result. When in doubt, record a conservative
/// [`read_opaque_host`](NodeAccess::read_opaque_host); extra dependencies cost re-runs, missing
/// ones cost correctness.
pub trait Executor {
    /// Value carried on edges, bound to inputs, and stored as node outputs.
    ///
    /// Values are cloned when bound to a downstream node's inputs, so embedders with large
    /// payloads should carry cheap handles (for example reference-counted pointers).
    type Value: Clone + fmt::Debug;
    /// Node body.
    type Node: fmt::Debug;
    /// Failure produced while executing one node.
    type Error: fmt::Debug;

    /// Runs `node` with its bound `inputs`, pushing one value per declared output into `outputs`.
    ///
    /// `outputs` is empty on entry and its capacity is retained by the graph between runs.
    fn execute(
        &mut self,
        node: &mut Self::Node,
        inputs: &[Self::Value],
        outputs: &mut Vec<Self::Value>,
        access: &mut NodeAccess<'_>,
    ) -> Result<(), Self::Error>;

    /// Returns whether a reader could tell `previous` and `next` apart.
    ///
    /// This drives *early cutoff*: when a re-run node produces an output for which this returns
    /// `true`, the output counts as unchanged, and dependents scheduled only because of it are
    /// cut off instead of re-run. Return `true` only when every reader would observe the same
    /// behaviour from either value, including through handles into executor-managed state.
    ///
    /// The default returns `false`, so every re-run output counts as changed and no dependent is
    /// ever cut off. Returning `false` is always correct; it only costs re-runs.
    fn values_equal(&self, previous: &Self::Value, next: &Self::Value) -> bool {
        let _ = (previous, next);
        false
    }

    /// Returns an advisory description of `node` for reports and Graphviz DOT output.
    ///
    /// The default returns `None`. Multi-line descriptions are rendered as separate lines.
    fn describe(&self, node: &Self::Node) -> Option<String> {
        let _ = node;
        None
    }
}
