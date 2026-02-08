// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Structured execution reporting.
//!
//! This module provides small, allocation-based report types intended for debugging and
//! instrumentation. Formatting and UI are left to embedders.

use alloc::vec::Vec;

use crate::{NodeId, ResourceKey};

/// Report for a single node execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRunReport {
    /// The node that executed.
    pub node: NodeId,
    /// The (graph-local) key whose dirtiness caused this node to be scheduled.
    ///
    /// Currently this is always a [`ResourceKey::TapeOutput`] for `node`.
    pub because_of: ResourceKey,
    /// One plausible cause path from a dirty root to [`NodeRunReport::because_of`].
    ///
    /// The vector is ordered from root to leaf (inclusive).
    pub why_path: Vec<ResourceKey>,
}

/// Report for a graph run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunReport {
    /// Nodes executed in the order they were run.
    pub executed: Vec<NodeRunReport>,
}
