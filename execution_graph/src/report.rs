// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Structured execution reporting.
//!
//! This module provides small, allocation-based report types intended for debugging and
//! instrumentation. Formatting and UI are left to embedders.

use alloc::vec::Vec;

use crate::{NodeId, ResourceKey};

/// Cheap run summary for incremental execution.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RunSummary {
    /// Number of nodes executed during the run.
    pub executed_nodes: usize,
}

/// Bitmask that controls which optional fields are populated in [`NodeRunDetail`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReportDetailMask(u8);

impl ReportDetailMask {
    /// No optional fields.
    pub const NONE: Self = Self(0);
    /// Include the immediate dirty key that scheduled the node.
    pub const BECAUSE_OF: Self = Self(1 << 0);
    /// Include one plausible cause path from dirty root to output key.
    pub const WHY_PATH: Self = Self(1 << 1);
    /// Include all optional fields.
    pub const FULL: Self = Self(Self::BECAUSE_OF.0 | Self::WHY_PATH.0);

    /// Returns `true` if this mask contains every bit in `other`.
    #[must_use]
    #[inline]
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }
}

/// Per-node detail record with optional payloads controlled by [`ReportDetailMask`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRunDetail {
    /// The node that executed.
    pub node: NodeId,
    /// The (graph-local) key whose dirtiness caused this node to be scheduled.
    pub because_of: Option<ResourceKey>,
    /// One plausible cause path from a dirty root to the output key for this node.
    ///
    /// The vector is ordered from root to leaf (inclusive).
    pub why_path: Option<Vec<ResourceKey>>,
}

/// Detail report for a graph run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunDetailReport {
    /// Per-node detail records in execution order.
    pub executed: Vec<NodeRunDetail>,
}
