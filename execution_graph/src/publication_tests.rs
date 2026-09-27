// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

extern crate std;

use super::*;
use crate::{FnExecutor, FnNode};
use alloc::{rc::Rc, vec};
use core::cell::Cell;

type Graph = ExecutionGraph<FnExecutor<i64, &'static str>>;

fn copy() -> FnNode<i64, &'static str> {
    FnNode::new(|inputs, outputs, _| {
        outputs.extend_from_slice(inputs);
        Ok(())
    })
}

#[test]
fn rejected_connection_preserves_bindings_outputs_and_dirty_state() {
    let mut graph = Graph::new(FnExecutor::new());
    let a = graph
        .add_node(copy(), vec!["x".into()], vec!["out".into()])
        .unwrap();
    let b = graph
        .add_node(copy(), vec!["x".into()], vec!["out".into()])
        .unwrap();
    graph.set_input_value(a, "x", 7).unwrap();
    graph.connect(a, "out", b, "x").unwrap();
    graph.run_all().unwrap();

    assert!(graph.connect(b, "out", a, "x").is_err());
    assert_eq!(graph.run_all().unwrap().executed_nodes, 0);
    graph.set_input_value(a, "x", 8).unwrap();
    graph.invalidate_input("x");
    assert_eq!(graph.run_all().unwrap().executed_nodes, 2);
    assert_eq!(graph.node_outputs(b).unwrap().get("out"), Some(&8));
}

#[test]
fn self_connection_is_rejected_before_the_first_run() {
    let mut graph = Graph::new(FnExecutor::new());
    let a = graph
        .add_node(copy(), vec!["x".into()], vec!["out".into()])
        .unwrap();
    graph.set_input_value(a, "x", 4).unwrap();
    assert!(graph.connect(a, "out", a, "x").is_err());
    graph.run_all().unwrap();
    assert_eq!(graph.node_outputs(a).unwrap().get("out"), Some(&4));
}

#[test]
fn rejected_connection_rolls_back_edges_on_earlier_outputs() {
    let mut graph = Graph::new(FnExecutor::new());
    let node = graph
        .add_node(
            FnNode::new(|inputs, outputs, _| {
                outputs.extend_from_slice(&[inputs[0], inputs[0]]);
                Ok(())
            }),
            vec!["x".into()],
            vec!["a".into(), "b".into()],
        )
        .unwrap();
    graph.set_input_value(node, "x", 7).unwrap();
    assert!(matches!(
        graph.connect(node, "b", node, "x"),
        Err(GraphError::DependencyCycle { .. })
    ));
    graph.run_all().unwrap();
    assert_eq!(graph.node_outputs(node).unwrap().get("a"), Some(&7));
    assert_eq!(graph.node_outputs(node).unwrap().get("b"), Some(&7));
}

#[test]
fn failed_attempt_keeps_committed_outputs_reads_and_log() {
    let phase = Rc::new(Cell::new(0));
    let captured = phase.clone();
    let mut graph = Graph::new(FnExecutor::new());
    graph.set_collect_access_log(true);
    let node = graph
        .add_node(
            FnNode::new(move |_, outputs, access| {
                access.read_input(if captured.get() == 0 { "old" } else { "new" });
                outputs.push(captured.get());
                match captured.get() {
                    1 => Err("failed"),
                    2 => {
                        outputs.push(99);
                        Ok(())
                    }
                    _ => Ok(()),
                }
            }),
            vec![],
            vec!["out".into()],
        )
        .unwrap();
    graph.run_all().unwrap();
    let old_log = graph.node_last_access(node).unwrap().as_slice().to_vec();
    let old_reads = graph.nodes[0].last_read_ids.clone();
    for next in [1, 2] {
        phase.set(next);
        graph.invalidate_input("old");
        assert!(graph.run_all().is_err());
        assert_eq!(graph.node_outputs(node).unwrap().get("out"), Some(&0));
        assert_eq!(graph.nodes[0].last_read_ids, old_reads);
        assert_eq!(graph.node_last_access(node).unwrap().as_slice(), old_log);
        assert_eq!(graph.node_run_count(node), Some(1));
    }
    phase.set(3);
    assert_eq!(graph.run_all().unwrap().executed_nodes, 1);
    graph.invalidate_input("old");
    assert_eq!(graph.run_all().unwrap().executed_nodes, 0);
    graph.invalidate_input("new");
    assert_eq!(graph.run_all().unwrap().executed_nodes, 1);
}

#[test]
fn later_failure_keeps_successful_upstream_publication() {
    let fail = Rc::new(Cell::new(false));
    let captured = fail.clone();
    let mut graph = Graph::new(FnExecutor::new());
    let a = graph
        .add_node(copy(), vec!["x".into()], vec!["out".into()])
        .unwrap();
    let b = graph
        .add_node(
            FnNode::new(move |inputs, outputs, _| {
                outputs.push(inputs[0] * 2);
                if captured.get() {
                    Err("failed")
                } else {
                    Ok(())
                }
            }),
            vec!["x".into()],
            vec!["out".into()],
        )
        .unwrap();
    graph.set_input_value(a, "x", 1).unwrap();
    graph.connect(a, "out", b, "x").unwrap();
    graph.run_all().unwrap();
    graph.set_input_value(a, "x", 2).unwrap();
    graph.invalidate_input("x");
    fail.set(true);
    let err = graph
        .run_all_with_report(ReportDetailMask::FULL)
        .unwrap_err();
    assert_eq!(err.partial_report().unwrap().executed[0].node, a);
    assert_eq!(graph.node_outputs(a).unwrap().get("out"), Some(&2));
    assert_eq!(graph.node_outputs(b).unwrap().get("out"), Some(&2));
    fail.set(false);
    assert_eq!(graph.run_all().unwrap().executed_nodes, 1);
    assert_eq!(graph.node_outputs(b).unwrap().get("out"), Some(&4));
    assert_eq!(graph.node_run_count(a), Some(2));
}
