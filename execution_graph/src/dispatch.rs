// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Internal dispatch interfaces for executing [`RunPlan`] values.
//!
//! This module intentionally stays internal. It provides a stable seam between planning ("what to
//! run") and execution strategy ("how to run"), so future scheduler work can swap dispatch
//! implementations without reshaping `ExecutionGraph` public APIs.

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::access::NodeId;
use crate::executor::Executor;
use crate::graph::{ExecutionGraph, GraphError, NodeStep};
use crate::plan::{PlanScope, RunPlan};
use crate::report::{NodeRunDetail, RunDetailReport, RunSummary};
use hashbrown::HashSet;

/// Internal dispatcher contract.
///
/// Dispatchers execute nodes in a precomputed [`RunPlan`] and may optionally assemble traced
/// reporting if the plan carries trace payload.
pub(crate) trait Dispatcher<X: Executor> {
    /// Executes `plan` without producing traced reporting, returning the summary counts.
    ///
    /// The drained scheduling buffer is returned to the graph's scratch workspace (for capacity
    /// reuse on the next planning pass) on every exit path, success or error.
    fn dispatch(
        &mut self,
        graph: &mut ExecutionGraph<X>,
        plan: RunPlan,
    ) -> Result<RunSummary, GraphError<X::Error>>;

    /// Executes `plan` and returns traced reporting if available.
    ///
    /// Like [`Dispatcher::dispatch`], the scheduling buffer is reclaimed on every exit path.
    fn dispatch_with_report(
        &mut self,
        graph: &mut ExecutionGraph<X>,
        plan: RunPlan,
    ) -> Result<RunDetailReport, GraphError<X::Error>>;
}

/// Serial in-thread dispatcher used by default.
///
/// Nodes are executed in the order provided by the [`RunPlan`], preserving deterministic behavior
/// and fail-fast error semantics.
#[derive(Copy, Clone, Debug, Default)]
pub(crate) struct InlineDispatcher;

impl<X: Executor> Dispatcher<X> for InlineDispatcher {
    fn dispatch(
        &mut self,
        graph: &mut ExecutionGraph<X>,
        plan: RunPlan,
    ) -> Result<RunSummary, GraphError<X::Error>> {
        run(graph, plan, false).map(|(summary, _)| summary)
    }
    fn dispatch_with_report(
        &mut self,
        graph: &mut ExecutionGraph<X>,
        plan: RunPlan,
    ) -> Result<RunDetailReport, GraphError<X::Error>> {
        run(graph, plan, true).map(|(_, report)| report)
    }
}

#[derive(Clone, Copy)]
enum Work {
    Run(NodeId),
    Resume(NodeId),
}

fn run<X: Executor>(
    graph: &mut ExecutionGraph<X>,
    mut plan: RunPlan,
    reporting: bool,
) -> Result<(RunSummary, RunDetailReport), GraphError<X::Error>> {
    match plan.scope() {
        PlanScope::All | PlanScope::WithinDependenciesOf(_) => {}
    }
    let detail = plan.detail();
    let mut trace = plan.take_trace();
    let mut summary = RunSummary::default();
    let mut report = RunDetailReport::default();
    let to_run = plan.take_nodes();
    if to_run.is_empty() {
        graph.reclaim_schedule_buffer(to_run);
        return Ok((summary, report));
    }
    let mut expanded = false;
    // Keep the ordinary schedule in its reusable buffer. Extra stack space is needed only
    // after a dynamic read asks for producers not already current.
    let mut next = 0;
    let mut work = Vec::new();
    let mut active = Vec::new();
    let mut active_set = HashSet::new();
    let result = (|| {
        loop {
            let frame = if let Some(frame) = work.pop() {
                frame
            } else if let Some(&node) = to_run.get(next) {
                next += 1;
                Work::Run(node)
            } else {
                break;
            };
            let node = match frame {
                Work::Run(node) => {
                    if active_set.contains(&node) {
                        let mut path = active.clone();
                        path.push(node);
                        return Err(GraphError::DynamicDependencyCycle { path });
                    }
                    // A runtime request may already have completed a node in the original plan.
                    if expanded && graph.node_status(node) == Some(crate::NodeStatus::Current) {
                        continue;
                    }
                    node
                }
                Work::Resume(node) => {
                    let completed = active.pop();
                    debug_assert_eq!(
                        completed,
                        Some(node),
                        "nested requests finish before their readers"
                    );
                    active_set.remove(&node);
                    node
                }
            };
            let before = graph.execution_attempts();
            let step = graph.execute_scheduled_node(node);
            summary.execution_attempts +=
                usize::try_from(graph.execution_attempts() - before).unwrap();
            match step {
                Err(error) => {
                    graph.remark_scheduled_dirty(&[node]);
                    return Err(error);
                }
                Ok(NodeStep::Pending(producers)) => {
                    expanded = true;
                    summary.suspended_attempts += 1;
                    active.push(node);
                    active_set.insert(node);
                    work.push(Work::Resume(node));
                    // Append in reverse so requests and their dependency plans run in order.
                    for producer in producers.into_iter().rev() {
                        let mut required =
                            graph.plan_within_dependencies_of_report(producer, detail)?;
                        if let Some(extra) = required.take_trace() {
                            trace.get_or_insert_with(Default::default).extend(extra);
                        }
                        let required_nodes = required.take_nodes();
                        if let Some(&repeated) = required_nodes
                            .iter()
                            .find(|node| active_set.contains(*node))
                        {
                            let mut path = active.clone();
                            path.push(producer);
                            if repeated != producer {
                                path.push(repeated);
                            }
                            graph.reclaim_schedule_buffer(required_nodes);
                            return Err(GraphError::DynamicDependencyCycle { path });
                        }
                        for &required_node in required_nodes.iter().rev() {
                            work.push(Work::Run(required_node));
                        }
                        graph.reclaim_schedule_buffer(required_nodes);
                    }
                    continue;
                }
                Ok(NodeStep::Executed) => summary.executed_nodes += 1,
                Ok(NodeStep::CutOff) => summary.cut_off_nodes += 1,
            }
            if reporting {
                let records = if matches!(step, Ok(NodeStep::Executed)) {
                    &mut report.executed
                } else {
                    &mut report.cut_off
                };
                if let Some(trace) = trace.as_mut() {
                    if let Some(row) = trace.take_report_for(node) {
                        records.push(row);
                    }
                } else {
                    records.push(NodeRunDetail {
                        node,
                        node_label: None,
                        because_of: None,
                        why_path: None,
                        why_path_traced: None,
                    });
                }
            }
        }
        Ok(())
    })();
    graph.reclaim_schedule_buffer(to_run);
    report.execution_attempts = summary.execution_attempts;
    report.suspended_attempts = summary.suspended_attempts;
    match result {
        Ok(()) => Ok((summary, report)),
        Err(source) if reporting => Err(GraphError::RunReportFailed {
            source: Box::new(source),
            partial_report: report,
        }),
        Err(source) => Err(source),
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::vec;

    use super::{Dispatcher, InlineDispatcher};
    use crate::access::ResourceKey;
    use crate::graph::{ExecutionGraph, GraphError};
    use crate::native::{FnExecutor, FnNode};
    use crate::plan::{RunPlan, RunPlanTrace};
    use crate::report::NodeRunDetail;

    type Graph = ExecutionGraph<FnExecutor<i64, ()>>;

    fn identity_node() -> FnNode<i64, ()> {
        FnNode::new(|inputs, outputs, _access| {
            outputs.push(inputs[0]);
            Ok(())
        })
    }

    fn const_node(value: i64) -> FnNode<i64, ()> {
        FnNode::new(move |_inputs, outputs, _access| {
            outputs.push(value);
            Ok(())
        })
    }

    #[test]
    fn inline_dispatcher_fail_fast_matches_graph_error_semantics() {
        let mut g = Graph::new(FnExecutor::new());
        let n_err = g
            .add_node(identity_node(), vec!["in".into()], vec!["value".into()])
            .unwrap();
        let n_ok = g
            .add_node(const_node(7), vec![], vec!["value".into()])
            .unwrap();
        let plan = RunPlan::all(vec![n_err, n_ok]);
        let mut dispatcher = InlineDispatcher;

        assert_eq!(
            dispatcher.dispatch(&mut g, plan),
            Err(GraphError::MissingInput {
                node: n_err,
                name: "in".into()
            })
        );

        assert_eq!(g.node_run_count(n_err), Some(0));
        assert_eq!(g.node_run_count(n_ok), Some(0));
    }

    #[test]
    fn inline_dispatcher_with_report_keeps_execution_order() {
        let mut g = Graph::new(FnExecutor::new());
        let n0 = g
            .add_node(const_node(11), vec![], vec!["value".into()])
            .unwrap();
        let n1 = g
            .add_node(const_node(11), vec![], vec!["value".into()])
            .unwrap();

        let r0 = NodeRunDetail {
            node: n0,
            node_label: Some("first".into()),
            because_of: Some(ResourceKey::node_output(n0, crate::OutputId::new(0))),
            why_path: Some(vec![ResourceKey::input("seed")]),
            why_path_traced: Some(true),
        };
        let r1 = NodeRunDetail {
            node: n1,
            node_label: Some("second".into()),
            because_of: Some(ResourceKey::node_output(n1, crate::OutputId::new(0))),
            why_path: Some(vec![ResourceKey::input("seed")]),
            why_path_traced: Some(true),
        };

        let mut node_reports = vec![None; 2];
        node_reports[0] = Some(r0.clone());
        node_reports[1] = Some(r1.clone());

        let plan =
            RunPlan::all(vec![n1, n0]).with_trace(RunPlanTrace::from_node_reports(node_reports));
        let mut dispatcher = InlineDispatcher;
        let report = dispatcher
            .dispatch_with_report(&mut g, plan)
            .expect("dispatch should succeed");

        assert_eq!(report.executed.len(), 2);
        assert_eq!(report.executed[0], r1);
        assert_eq!(report.executed[1], r0);
    }

    #[test]
    fn inline_dispatcher_with_report_returns_partial_report_on_error() {
        let mut g = Graph::new(FnExecutor::new());
        let n_ok = g
            .add_node(const_node(11), vec![], vec!["value".into()])
            .unwrap();
        let n_err = g
            .add_node(identity_node(), vec!["in".into()], vec!["value".into()])
            .unwrap();

        let r_ok = NodeRunDetail {
            node: n_ok,
            node_label: Some("ok".into()),
            because_of: Some(ResourceKey::node_output(n_ok, crate::OutputId::new(0))),
            why_path: Some(vec![ResourceKey::input("seed")]),
            why_path_traced: Some(true),
        };
        let r_err = NodeRunDetail {
            node: n_err,
            node_label: Some("err".into()),
            because_of: Some(ResourceKey::node_output(n_err, crate::OutputId::new(0))),
            why_path: Some(vec![ResourceKey::input("seed")]),
            why_path_traced: Some(true),
        };

        let mut node_reports = vec![None; 2];
        node_reports[0] = Some(r_ok.clone());
        node_reports[1] = Some(r_err);

        let plan = RunPlan::all(vec![n_ok, n_err])
            .with_trace(RunPlanTrace::from_node_reports(node_reports));
        let mut dispatcher = InlineDispatcher;
        let err = dispatcher
            .dispatch_with_report(&mut g, plan)
            .expect_err("second node should fail");

        let GraphError::RunReportFailed {
            source,
            partial_report,
        } = err
        else {
            panic!("expected partial report error");
        };

        assert_eq!(
            *source,
            GraphError::MissingInput {
                node: n_err,
                name: "in".into()
            }
        );
        assert_eq!(partial_report.executed, vec![r_ok]);
        assert_eq!(g.node_run_count(n_ok), Some(1));
        assert_eq!(g.node_run_count(n_err), Some(0));
    }

    #[test]
    fn inline_dispatcher_with_report_synthesizes_minimal_rows_without_trace() {
        let mut g = Graph::new(FnExecutor::new());
        let n0 = g
            .add_node(const_node(5), vec![], vec!["value".into()])
            .unwrap();
        let n1 = g
            .add_node(const_node(5), vec![], vec!["value".into()])
            .unwrap();

        let mut dispatcher = InlineDispatcher;
        let out = dispatcher
            .dispatch_with_report(&mut g, RunPlan::all(vec![n1, n0]))
            .expect("dispatch should succeed");

        assert_eq!(
            out.executed,
            vec![
                NodeRunDetail {
                    node: n1,
                    node_label: None,
                    because_of: None,
                    why_path: None,
                    why_path_traced: None,
                },
                NodeRunDetail {
                    node: n0,
                    node_label: None,
                    because_of: None,
                    why_path: None,
                    why_path_traced: None,
                },
            ]
        );
    }

    #[test]
    fn inline_dispatcher_with_report_handles_short_trace_vectors() {
        let mut g = Graph::new(FnExecutor::new());
        let node = g
            .add_node(const_node(5), vec![], vec!["value".into()])
            .unwrap();

        // Empty trace payload: execution should still succeed and simply produce no traced rows.
        let trace = RunPlanTrace::from_node_reports(vec![]);

        let mut dispatcher = InlineDispatcher;
        let out = dispatcher
            .dispatch_with_report(&mut g, RunPlan::all(vec![node]).with_trace(trace))
            .expect("dispatch should succeed");

        assert_eq!(g.node_run_count(node), Some(1));
        assert!(out.executed.is_empty());
    }
}
