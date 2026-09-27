// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use crate::{
    ExecutionGraph, FnExecutor, FnNode, GraphError, HostOpId, NodeId, NodeOutcome, NodeStatus,
    OutputId, OutputReadError, ReportDetailMask, ResourceKey,
};
use alloc::{rc::Rc, vec, vec::Vec};
use core::cell::Cell;

#[derive(Clone, Debug, Eq, PartialEq)]
enum Error {
    Read(OutputReadError),
    Failed,
}
impl From<OutputReadError> for Error {
    fn from(error: OutputReadError) -> Self {
        Self::Read(error)
    }
}
type Graph = ExecutionGraph<FnExecutor<i64, Error>>;
const OUT: OutputId = OutputId::new(0);
fn graph() -> Graph {
    Graph::new(FnExecutor::new().with_value_eq(|a, b| a == b))
}
fn constant(value: i64) -> FnNode<i64, Error> {
    FnNode::new(move |_, outputs, _| {
        outputs.push(value);
        Ok(())
    })
}
fn reader(producer: NodeId) -> FnNode<i64, Error> {
    FnNode::restartable(move |_, outputs, access| {
        let Some(value) = access.read_node_output(producer, OUT)? else {
            return Ok(NodeOutcome::Pending);
        };
        outputs.push(value + 1);
        Ok(NodeOutcome::Complete)
    })
}
fn reads(g: &Graph, node: NodeId) -> Vec<ResourceKey> {
    g.node_dependencies(node).unwrap().cloned().collect()
}

#[test]
fn runtime_demand_updates_unrun_producer_and_reports_publications_once() {
    let mut g = graph();
    let selected = Rc::new(Cell::new(None));
    let target = selected.clone();
    let child = g
        .add_node(
            FnNode::restartable(move |_, outputs, access| {
                let Some(value) = access.read_node_output(target.get().unwrap(), OUT)? else {
                    return Ok(NodeOutcome::Pending);
                };
                outputs.push(value + 1);
                Ok(NodeOutcome::Complete)
            }),
            vec![],
            vec!["out".into()],
        )
        .unwrap();
    let parent = g.add_node(constant(4), vec![], vec!["out".into()]).unwrap();
    selected.set(Some(parent));
    g.set_node_label(parent, "parent").unwrap();
    assert_eq!(g.node_status(child), Some(NodeStatus::NeverRun));
    let report = g.run_all_with_report(ReportDetailMask::FULL).unwrap();
    assert_eq!(
        report.executed.iter().map(|r| r.node).collect::<Vec<_>>(),
        [parent, child]
    );
    assert_eq!(report.executed[0].node_label.as_deref(), Some("parent"));
    assert_eq!(
        (report.execution_attempts, report.suspended_attempts),
        (3, 1)
    );
    assert_eq!(g.node_attempt_count(child), Some(2));
    assert_eq!(g.node_run_count(child), Some(1));
    assert_eq!(g.node_status(child), Some(NodeStatus::Current));
    assert_eq!(g.node_outputs(child).unwrap().get_by_id(OUT), Some(&5));
    assert_eq!(reads(&g, child), [ResourceKey::node_output(parent, OUT)]);
    assert_eq!(g.run_node(child).unwrap().execution_attempts, 0);
}

#[test]
fn reset_drops_parent_then_rediscovers_it_outside_the_old_scope() {
    let mut g = graph();
    let parent_value = Rc::new(Cell::new(4));
    let state = parent_value.clone();
    let parent = g
        .add_node(
            FnNode::new(move |_, outputs, access| {
                access.read_input("parent");
                outputs.push(state.get());
                Ok(())
            }),
            vec![],
            vec!["out".into()],
        )
        .unwrap();
    let reset = Rc::new(Cell::new(false));
    let state = reset.clone();
    let child = g
        .add_node(
            FnNode::restartable(move |_, outputs, access| {
                access.read_input("reset");
                let value = if state.get() {
                    0
                } else {
                    let Some(value) = access.read_node_output(parent, OUT)? else {
                        return Ok(NodeOutcome::Pending);
                    };
                    value
                };
                outputs.push(value + 2);
                Ok(NodeOutcome::Complete)
            }),
            vec![],
            vec!["out".into()],
        )
        .unwrap();
    g.run_node(child).unwrap();
    assert_eq!(g.node_outputs(child).unwrap().get("out"), Some(&6));
    assert!(reads(&g, child).contains(&ResourceKey::node_output(parent, OUT)));
    reset.set(true);
    g.invalidate_input("reset");
    g.run_node(child).unwrap();
    assert_eq!(reads(&g, child), [ResourceKey::input("reset")]);
    parent_value.set(9);
    g.invalidate_input("parent");
    assert_eq!(g.run_node(child).unwrap().executed_nodes, 0);
    assert_eq!(g.node_status(parent), Some(NodeStatus::Pending));
    assert_eq!(g.node_outputs(child).unwrap().get("out"), Some(&2));
    reset.set(false);
    g.invalidate_input("reset");
    let summary = g.run_node(child).unwrap();
    assert_eq!(
        (
            summary.executed_nodes,
            summary.execution_attempts,
            summary.suspended_attempts
        ),
        (2, 3, 1)
    );
    assert_eq!(g.node_outputs(child).unwrap().get("out"), Some(&11));
    assert!(reads(&g, child).contains(&ResourceKey::node_output(parent, OUT)));
}

#[test]
fn failed_requested_producer_preserves_reader_cache_reads_and_log() {
    let mut g = graph();
    g.set_collect_access_log(true);
    let good = g.add_node(constant(1), vec![], vec!["out".into()]).unwrap();
    let failing = Rc::new(Cell::new(true));
    let fail = failing.clone();
    let other = g
        .add_node(
            FnNode::new(move |_, outputs, _| {
                if fail.get() {
                    return Err(Error::Failed);
                }
                outputs.push(9);
                Ok(())
            }),
            vec![],
            vec!["out".into()],
        )
        .unwrap();
    let target = Rc::new(Cell::new(good));
    let selected = target.clone();
    let child = g
        .add_node(
            FnNode::restartable(move |_, outputs, access| {
                access.read_input("selected");
                let Some(value) = access.read_node_output(selected.get(), OUT)? else {
                    return Ok(NodeOutcome::Pending);
                };
                outputs.push(value);
                Ok(NodeOutcome::Complete)
            }),
            vec![],
            vec!["out".into()],
        )
        .unwrap();
    g.run_node(child).unwrap();
    let old_reads = reads(&g, child);
    let old_log = g.node_last_access(child).cloned();
    target.set(other);
    g.invalidate_input("selected");
    let error = g
        .run_node_with_report(child, ReportDetailMask::FULL)
        .unwrap_err();
    let report = error.partial_report().unwrap();
    assert_eq!(
        (
            report.executed.len(),
            report.execution_attempts,
            report.suspended_attempts
        ),
        (0, 2, 1)
    );
    assert_eq!(g.node_outputs(child).unwrap().get("out"), Some(&1));
    assert_eq!(reads(&g, child), old_reads);
    assert_eq!(
        g.node_last_access(child).unwrap().as_slice(),
        old_log.as_ref().unwrap().as_slice()
    );
    assert_eq!(g.node_status(child), Some(NodeStatus::Pending));
    failing.set(false);
    let summary = g.run_node(child).unwrap();
    assert_eq!((summary.executed_nodes, summary.execution_attempts), (2, 3));
    assert_eq!(g.node_outputs(child).unwrap().get("out"), Some(&9));
    assert!(!reads(&g, child).contains(&ResourceKey::node_output(good, OUT)));
}

#[test]
fn pending_is_explicit_and_host_writes_are_never_silently_replayed() {
    let mut g = graph();
    let producer = g.add_node(constant(7), vec![], vec!["out".into()]).unwrap();
    let ordinary = g
        .add_node(
            FnNode::new(move |_, outputs, access| {
                let _ = access.read_node_output(producer, OUT)?;
                outputs.push(0);
                Ok(())
            }),
            vec![],
            vec!["out".into()],
        )
        .unwrap();
    assert_eq!(
        g.run_node(ordinary),
        Err(GraphError::UnresolvedOutputReads {
            node: ordinary,
            producers: vec![producer]
        })
    );
    assert_eq!(g.node_attempt_count(ordinary), Some(1));
    assert_eq!(g.node_attempt_count(producer), Some(0));
    let writer = g
        .add_node(
            FnNode::restartable(move |_, _, access| {
                access.write_host_state(HostOpId::new(5), 1);
                assert!(access.read_node_output(producer, OUT)?.is_none());
                Ok(NodeOutcome::Pending)
            }),
            vec![],
            vec!["out".into()],
        )
        .unwrap();
    assert_eq!(
        g.run_node(writer),
        Err(GraphError::PendingAfterWrite { node: writer })
    );
    assert_eq!(g.node_attempt_count(writer), Some(1));
    let empty = g
        .add_node(
            FnNode::restartable(|_, _, _| Ok(NodeOutcome::Pending)),
            vec![],
            vec!["out".into()],
        )
        .unwrap();
    assert_eq!(
        g.run_node(empty),
        Err(GraphError::PendingWithoutReads { node: empty })
    );
}

#[test]
fn cycle_through_a_bound_producer_identifies_the_request_chain() {
    let mut g = graph();
    let selected = Rc::new(Cell::new(None));
    let target = selected.clone();
    let a = g
        .add_node(
            FnNode::restartable(move |_, outputs, access| {
                let Some(value) = access.read_node_output(target.get().unwrap(), OUT)? else {
                    return Ok(NodeOutcome::Pending);
                };
                outputs.push(value);
                Ok(NodeOutcome::Complete)
            }),
            vec![],
            vec!["out".into()],
        )
        .unwrap();
    let b = g
        .add_node(
            FnNode::new(|inputs, outputs, _| {
                outputs.push(inputs[0]);
                Ok(())
            }),
            vec!["in".into()],
            vec!["out".into()],
        )
        .unwrap();
    selected.set(Some(b));
    g.connect(a, "out", b, "in").unwrap();
    assert_eq!(
        g.run_node(a),
        Err(GraphError::DynamicDependencyCycle {
            path: vec![a, b, a]
        })
    );
    assert_eq!(g.node_run_count(a), Some(0));
    assert_eq!(g.node_run_count(b), Some(0));
}

#[test]
fn multiple_requests_resolve_before_one_restart_and_missing_ids_are_typed() {
    let mut g = graph();
    let a = g.add_node(constant(2), vec![], vec!["out".into()]).unwrap();
    let b = g.add_node(constant(3), vec![], vec!["out".into()]).unwrap();
    let child = g
        .add_node(
            FnNode::restartable(move |_, outputs, access| {
                let av = access.read_node_output(a, OUT)?;
                let bv = access.read_node_output(b, OUT)?;
                let (Some(av), Some(bv)) = (av, bv) else {
                    return Ok(NodeOutcome::Pending);
                };
                outputs.push(av + bv);
                Ok(NodeOutcome::Complete)
            }),
            vec![],
            vec!["out".into()],
        )
        .unwrap();
    let summary = g.run_node(child).unwrap();
    assert_eq!((summary.executed_nodes, summary.suspended_attempts), (3, 1));
    g.remove_node(a).unwrap();
    assert_eq!(
        g.run_node(child),
        Err(GraphError::Node {
            node: child,
            source: Error::Read(OutputReadError::MissingNode { node: a })
        })
    );
    let invalid = g
        .add_node(
            FnNode::restartable(move |_, _, access| {
                access.read_node_output(b, OutputId::new(1))?;
                Ok(NodeOutcome::Complete)
            }),
            vec![],
            vec!["out".into()],
        )
        .unwrap();
    assert_eq!(
        g.run_node(invalid),
        Err(GraphError::Node {
            node: invalid,
            source: Error::Read(OutputReadError::MissingOutput {
                node: b,
                output: OutputId::new(1)
            })
        })
    );
}

#[test]
fn deep_first_use_requests_do_not_recurse_on_the_native_stack() {
    let mut g = graph();
    let mut tip = g.add_node(constant(1), vec![], vec!["out".into()]).unwrap();
    for _ in 0..2000 {
        tip = g.add_node(reader(tip), vec![], vec!["out".into()]).unwrap();
    }
    let summary = g.run_node(tip).unwrap();
    assert_eq!(
        (summary.executed_nodes, summary.suspended_attempts),
        (2001, 2000)
    );
    assert_eq!(g.node_outputs(tip).unwrap().get("out"), Some(&2001));
}

#[test]
fn forcing_one_output_requires_readers_of_other_outputs_to_verify() {
    let mut g = graph();
    let value = Rc::new(Cell::new(1));
    let v = value.clone();
    let producer = g
        .add_node(
            FnNode::new(move |_, outputs, _| {
                outputs.extend([v.get(), -v.get()]);
                Ok(())
            }),
            vec![],
            vec!["positive".into(), "negative".into()],
        )
        .unwrap();
    let child = g
        .add_node(reader(producer), vec![], vec!["out".into()])
        .unwrap();
    g.run_node(child).unwrap();
    value.set(2);
    g.invalidate(ResourceKey::node_output(producer, OutputId::new(1)));
    g.run_node(child).unwrap();
    assert_eq!(g.node_outputs(child).unwrap().get("out"), Some(&3));
}
