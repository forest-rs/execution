// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Internal dispatch interfaces for executing [`RunPlan`](crate::plan::RunPlan) values.
//!
//! This module intentionally stays internal. It provides a stable seam between planning ("what to
//! run") and execution strategy ("how to run"), so future scheduler work can swap dispatch
//! implementations without reshaping `ExecutionGraph` public APIs.

use alloc::vec::Vec;

use crate::access::NodeId;
use crate::graph::GraphError;
use crate::plan::{PlanScope, RunPlan};
use crate::report::RunReport;

/// Internal dispatcher contract.
///
/// Dispatchers execute nodes in a precomputed [`RunPlan`] and may optionally assemble traced
/// reporting if the plan carries trace payload.
pub(crate) trait Dispatcher {
    /// Executes `plan` without producing traced reporting.
    ///
    /// The dispatcher receives a node runner callback and returns the drained scheduling buffer so
    /// callers can reuse its capacity.
    fn dispatch<F>(&mut self, plan: RunPlan, run_node: F) -> Result<Vec<NodeId>, GraphError>
    where
        F: FnMut(NodeId) -> Result<(), GraphError>;

    /// Executes `plan` and returns traced reporting if available.
    ///
    /// Returns both the drained scheduling buffer (for capacity reuse) and the assembled report.
    fn dispatch_with_report<F>(
        &mut self,
        plan: RunPlan,
        run_node: F,
    ) -> Result<(Vec<NodeId>, RunReport), GraphError>
    where
        F: FnMut(NodeId) -> Result<(), GraphError>;
}

/// Serial in-thread dispatcher used by default.
///
/// Nodes are executed in the order provided by the [`RunPlan`], preserving deterministic behavior
/// and fail-fast error semantics.
#[derive(Copy, Clone, Debug, Default)]
pub(crate) struct InlineDispatcher;

impl Dispatcher for InlineDispatcher {
    fn dispatch<F>(&mut self, mut plan: RunPlan, mut run_node: F) -> Result<Vec<NodeId>, GraphError>
    where
        F: FnMut(NodeId) -> Result<(), GraphError>,
    {
        // Keep scope as part of the dispatch contract even before scope-specific strategies exist.
        match plan.scope() {
            PlanScope::All | PlanScope::WithinDependenciesOf(_) => {}
        }

        let mut to_run: Vec<NodeId> = plan.take_nodes();
        for node in to_run.drain(..) {
            run_node(node)?;
        }
        Ok(to_run)
    }

    fn dispatch_with_report<F>(
        &mut self,
        mut plan: RunPlan,
        mut run_node: F,
    ) -> Result<(Vec<NodeId>, RunReport), GraphError>
    where
        F: FnMut(NodeId) -> Result<(), GraphError>,
    {
        // Keep scope as part of the dispatch contract even before scope-specific strategies exist.
        match plan.scope() {
            PlanScope::All | PlanScope::WithinDependenciesOf(_) => {}
        }

        let mut trace = plan.take_trace();
        let mut report = RunReport::default();
        let mut to_run: Vec<NodeId> = plan.take_nodes();

        for node in to_run.drain(..) {
            run_node(node)?;
            if let Some(t) = trace.as_mut()
                && let Some(r) = t.take_report_for(node)
            {
                report.executed.push(r);
            }
        }

        Ok((to_run, report))
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::vec;

    use super::{Dispatcher, InlineDispatcher};
    use crate::access::{NodeId, ResourceKey};
    use crate::graph::GraphError;
    use crate::plan::{RunPlan, RunPlanTrace};
    use crate::report::NodeRunReport;

    #[test]
    fn inline_dispatcher_fail_fast_matches_graph_error_semantics() {
        let n_err = NodeId::new(7);
        let n_ok = NodeId::new(8);

        let plan = RunPlan::all(vec![n_err, n_ok]);
        let mut dispatcher = InlineDispatcher;
        let mut executed = vec![];

        assert_eq!(
            dispatcher.dispatch(plan, |node| {
                executed.push(node);
                if node == n_err {
                    return Err(GraphError::Trap);
                }
                Ok(())
            }),
            Err(GraphError::Trap)
        );

        assert_eq!(executed, vec![n_err]);
    }

    #[test]
    fn inline_dispatcher_with_report_keeps_execution_order() {
        let n0 = NodeId::new(0);
        let n1 = NodeId::new(1);

        let r0 = NodeRunReport {
            node: n0,
            because_of: ResourceKey::tape_output(n0, "value"),
            why_path: vec![ResourceKey::input("seed")],
        };
        let r1 = NodeRunReport {
            node: n1,
            because_of: ResourceKey::tape_output(n1, "value"),
            why_path: vec![ResourceKey::input("seed")],
        };

        let mut node_reports = vec![None; 2];
        node_reports[0] = Some(r0.clone());
        node_reports[1] = Some(r1.clone());

        let plan =
            RunPlan::all(vec![n1, n0]).with_trace(RunPlanTrace::from_node_reports(node_reports));
        let mut dispatcher = InlineDispatcher;
        let mut executed = vec![];
        let (_buf, report) = dispatcher
            .dispatch_with_report(plan, |node| {
                executed.push(node);
                Ok(())
            })
            .expect("dispatch should succeed");

        assert_eq!(executed, vec![n1, n0]);
        assert_eq!(report.executed.len(), 2);
        assert_eq!(report.executed[0], r1);
        assert_eq!(report.executed[1], r0);
    }

    #[test]
    fn inline_dispatcher_with_report_handles_short_trace_vectors() {
        let node = NodeId::new(4);

        // Empty trace payload: execution should still succeed and simply produce no traced rows.
        let trace = RunPlanTrace::from_node_reports(vec![]);

        let mut dispatcher = InlineDispatcher;
        let (_buf, out) = dispatcher
            .dispatch_with_report(RunPlan::all(vec![node]).with_trace(trace), |_n| Ok(()))
            .expect("dispatch should succeed");

        assert!(out.executed.is_empty());
    }
}
