// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Internal run-planning model for `execution_graph`.
//!
//! A [`RunPlan`] captures *what* should execute, while execution methods in `graph.rs` decide
//! *how* to execute it. This separation is intended to support future dispatcher/scheduler work
//! without changing public APIs.

use alloc::vec::Vec;

use crate::access::NodeId;
use crate::report::NodeRunDetail;

/// Scope of scheduled work represented by a [`RunPlan`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum PlanScope {
    /// Plan produced from draining all affected dirty work.
    All,
    /// Plan produced from draining only within the dependency closure of `node`.
    WithinDependenciesOf(NodeId),
}

/// Optional traced payload attached to a [`RunPlan`].
///
/// The payload is indexed by node id (`NodeId::as_u64() as usize`) and stores one plausible
/// rerun cause report for nodes that were scheduled.
///
/// TODO(dispatcher): Revisit this dense representation. It currently scales with total graph node
/// count for O(1) `take_report_for` lookups. A sparse map keyed by `NodeId` would scale with
/// scheduled nodes instead, but lookup/remove would become O(log n) and per-entry overhead would
/// increase.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RunPlanTrace {
    node_reports: Vec<Option<NodeRunDetail>>,
}

impl RunPlanTrace {
    /// Creates traced payload from per-node optional reports.
    #[must_use]
    #[inline]
    pub(crate) fn from_node_reports(node_reports: Vec<Option<NodeRunDetail>>) -> Self {
        Self { node_reports }
    }

    /// Removes and returns the report for `node`, if present.
    #[inline]
    pub(crate) fn take_report_for(&mut self, node: NodeId) -> Option<NodeRunDetail> {
        let index = usize::try_from(node.as_u64()).ok()?;
        self.node_reports.get_mut(index)?.take()
    }
}

/// Internal planning artifact for a single graph run.
///
/// The plan records a deterministic node schedule plus optional traced cause metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RunPlan {
    scope: PlanScope,
    nodes: Vec<NodeId>,
    trace: Option<RunPlanTrace>,
}

impl RunPlan {
    /// Creates a plan for "all dirty work" scope.
    #[must_use]
    #[inline]
    pub(crate) fn all(nodes: Vec<NodeId>) -> Self {
        Self {
            scope: PlanScope::All,
            nodes,
            trace: None,
        }
    }

    /// Creates a plan for work within the dependency closure of `node`.
    #[must_use]
    #[inline]
    pub(crate) fn within_dependencies_of(node: NodeId, nodes: Vec<NodeId>) -> Self {
        Self {
            scope: PlanScope::WithinDependenciesOf(node),
            nodes,
            trace: None,
        }
    }

    /// Attaches traced payload to the plan.
    #[must_use]
    #[inline]
    pub(crate) fn with_trace(mut self, trace: RunPlanTrace) -> Self {
        self.trace = Some(trace);
        self
    }

    /// Returns the planning scope.
    #[must_use]
    #[inline]
    pub(crate) fn scope(&self) -> PlanScope {
        self.scope
    }

    /// Returns the number of scheduled nodes.
    #[must_use]
    #[inline]
    pub(crate) fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Drains scheduled nodes from the plan.
    #[must_use]
    #[inline]
    pub(crate) fn take_nodes(&mut self) -> Vec<NodeId> {
        core::mem::take(&mut self.nodes)
    }

    /// Drains traced payload from the plan.
    #[must_use]
    #[inline]
    pub(crate) fn take_trace(&mut self) -> Option<RunPlanTrace> {
        self.trace.take()
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::RunPlanTrace;
    use crate::access::{NodeId, ResourceKey};
    use crate::report::NodeRunDetail;

    #[test]
    fn run_plan_trace_take_report_for_is_one_shot_and_bounds_safe() {
        let node = NodeId::new(3);
        let report = NodeRunDetail {
            node,
            because_of: Some(ResourceKey::node_output(node, "value")),
            why_path: Some(alloc::vec![ResourceKey::input("in")]),
        };

        let mut trace =
            RunPlanTrace::from_node_reports(alloc::vec![None, None, None, Some(report.clone())]);

        assert_eq!(trace.take_report_for(node), Some(report));
        assert_eq!(trace.take_report_for(node), None);
        assert_eq!(trace.take_report_for(NodeId::new(99)), None);
        assert_eq!(trace.take_report_for(NodeId::new(u64::MAX)), None);
    }
}
