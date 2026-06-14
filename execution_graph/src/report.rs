// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Structured execution reporting.
//!
//! This module provides small, allocation-based report types intended for debugging and
//! instrumentation. Formatting and UI are left to embedders.

use alloc::vec::Vec;
use core::ops::{BitAnd, BitAndAssign, BitOr, BitOrAssign};

use crate::{NodeId, ResourceKey};

/// Cheap run summary for incremental execution.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
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
    ///
    /// This does not imply [`ReportDetailMask::BECAUSE_OF`]. Use
    /// [`ReportDetailMask::FULL`] or combine masks when both fields are needed.
    pub const WHY_PATH: Self = Self(1 << 1);
    /// Include all optional fields.
    pub const FULL: Self = Self(Self::BECAUSE_OF.0 | Self::WHY_PATH.0);

    /// Returns a mask with the bits from `self` and `other`.
    #[must_use]
    #[inline]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Returns a mask with only the bits common to `self` and `other`.
    #[must_use]
    #[inline]
    pub const fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    /// Returns `true` if this mask contains every bit in `other`.
    #[must_use]
    #[inline]
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    /// Returns `true` if this mask does not request any detail fields.
    #[must_use]
    #[inline]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl Default for ReportDetailMask {
    #[inline]
    fn default() -> Self {
        Self::NONE
    }
}

impl BitOr for ReportDetailMask {
    type Output = Self;

    #[inline]
    fn bitor(self, rhs: Self) -> Self::Output {
        self.union(rhs)
    }
}

impl BitOrAssign for ReportDetailMask {
    #[inline]
    fn bitor_assign(&mut self, rhs: Self) {
        *self = self.union(rhs);
    }
}

impl BitAnd for ReportDetailMask {
    type Output = Self;

    #[inline]
    fn bitand(self, rhs: Self) -> Self::Output {
        self.intersection(rhs)
    }
}

impl BitAndAssign for ReportDetailMask {
    #[inline]
    fn bitand_assign(&mut self, rhs: Self) {
        *self = self.intersection(rhs);
    }
}

/// Per-node detail record with optional payloads controlled by [`ReportDetailMask`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
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
#[non_exhaustive]
pub struct RunDetailReport {
    /// Per-node detail records in execution order.
    pub executed: Vec<NodeRunDetail>,
}

#[cfg(test)]
mod tests {
    use super::ReportDetailMask;

    #[test]
    fn report_detail_mask_composes_with_methods_and_operators() {
        const FULL_FROM_UNION: ReportDetailMask =
            ReportDetailMask::BECAUSE_OF.union(ReportDetailMask::WHY_PATH);

        assert_eq!(ReportDetailMask::default(), ReportDetailMask::NONE);
        assert!(ReportDetailMask::NONE.is_empty());
        assert_eq!(FULL_FROM_UNION, ReportDetailMask::FULL);
        assert_eq!(
            ReportDetailMask::BECAUSE_OF | ReportDetailMask::WHY_PATH,
            ReportDetailMask::FULL
        );
        assert_eq!(
            ReportDetailMask::FULL & ReportDetailMask::BECAUSE_OF,
            ReportDetailMask::BECAUSE_OF
        );

        let mut mask = ReportDetailMask::BECAUSE_OF;
        mask |= ReportDetailMask::WHY_PATH;
        assert_eq!(mask, ReportDetailMask::FULL);
        mask &= ReportDetailMask::WHY_PATH;
        assert_eq!(mask, ReportDetailMask::WHY_PATH);
    }
}
