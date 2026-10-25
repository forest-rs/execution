// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use crate::{
    ExecutionGraph, FnExecutor, FnNode, GraphError, InputId, InputSource, NodeStatus, OutputId,
    ResourceKey,
};
use alloc::{rc::Rc, vec, vec::Vec};
use core::cell::Cell;

type Graph = ExecutionGraph<FnExecutor<i64, &'static str>>;
fn copy() -> FnNode<i64, &'static str> {
    FnNode::new(|inputs, outputs, _| {
        outputs.push(inputs[0]);
        Ok(())
    })
}
fn constant(value: i64) -> FnNode<i64, &'static str> {
    FnNode::new(move |_, outputs, _| {
        outputs.push(value);
        Ok(())
    })
}

#[test]
fn reverse_wiring_without_intermediate_execution_preserves_committed_history() {
    let mut graph = Graph::new(FnExecutor::new());
    let a = graph
        .add_node(copy(), vec!["input".into()], vec!["out".into()])
        .unwrap();
    let b = graph
        .add_node(copy(), vec!["input".into()], vec!["out".into()])
        .unwrap();
    graph.set_input_value(a, "input", 1).unwrap();
    graph.connect(a, "out", b, "input").unwrap();
    graph.run_all().unwrap();
    let committed = graph
        .node_dependencies(b)
        .unwrap()
        .cloned()
        .collect::<Vec<_>>();
    graph.set_input_value(b, "input", 2).unwrap();
    graph.connect(b, "out", a, "input").unwrap();
    assert_eq!(graph.node_status(b), Some(NodeStatus::Pending));
    assert_eq!(graph.node_outputs(b).unwrap().get("out"), Some(&1));
    assert_eq!(
        graph
            .node_dependencies(b)
            .unwrap()
            .cloned()
            .collect::<Vec<_>>(),
        committed
    );
    assert_eq!(graph.node_run_count(b), Some(1));
    assert_eq!(graph.run_node(a).unwrap().executed_nodes, 2);
    assert_eq!(graph.node_outputs(a).unwrap().get("out"), Some(&2));
    assert_eq!(graph.node_outputs(b).unwrap().get("out"), Some(&2));
    assert_eq!(
        graph
            .node_dependencies(b)
            .unwrap()
            .cloned()
            .collect::<Vec<_>>(),
        [ResourceKey::input("input")]
    );
}

#[test]
fn failed_rewired_attempt_keeps_history_and_prospective_order_for_retry() {
    let fail = Rc::new(Cell::new(false));
    let failure = Rc::clone(&fail);
    let mut graph = Graph::new(FnExecutor::new());
    let a = graph
        .add_node(copy(), vec!["a".into()], vec!["out".into()])
        .unwrap();
    let b = graph
        .add_node(
            FnNode::new(move |inputs, outputs, _| {
                outputs.push(inputs[0]);
                if failure.get() {
                    Err("injected")
                } else {
                    Ok(())
                }
            }),
            vec!["b".into()],
            vec!["out".into()],
        )
        .unwrap();
    graph.set_input_value(a, "a", 1).unwrap();
    graph.connect(a, "out", b, "b").unwrap();
    graph.run_all().unwrap();
    let reads = graph
        .node_dependencies(b)
        .unwrap()
        .cloned()
        .collect::<Vec<_>>();
    graph.set_input_value(b, "b", 10).unwrap();
    graph.connect(b, "out", a, "a").unwrap();
    fail.set(true);
    assert!(matches!(
        graph.run_all(),
        Err(GraphError::Node {
            source: "injected",
            ..
        })
    ));
    assert_eq!(graph.node_outputs(b).unwrap().get("out"), Some(&1));
    assert_eq!(
        graph
            .node_dependencies(b)
            .unwrap()
            .cloned()
            .collect::<Vec<_>>(),
        reads
    );
    fail.set(false);
    graph.run_all().unwrap();
    assert_eq!(graph.node_outputs(a).unwrap().get("out"), Some(&10));
}

#[test]
fn remaining_slot_dependency_still_rejects_a_real_declared_cycle() {
    let mut graph = Graph::new(FnExecutor::new());
    let a = graph
        .add_node(copy(), vec!["a".into()], vec!["out".into()])
        .unwrap();
    let b = graph
        .add_node(
            FnNode::new(|inputs, outputs, _| {
                outputs.push(inputs.iter().sum());
                Ok(())
            }),
            vec!["left".into(), "right".into()],
            vec!["out".into()],
        )
        .unwrap();
    graph.set_input_value(a, "a", 1).unwrap();
    graph.connect(a, "out", b, "left").unwrap();
    graph.connect(a, "out", b, "right").unwrap();
    graph.run_all().unwrap();
    graph.set_input_value(b, "left", 2).unwrap();
    let before = graph.to_dot();
    assert!(matches!(
        graph.connect(b, "out", a, "a"),
        Err(GraphError::DependencyCycle { .. })
    ));
    assert_eq!(graph.to_dot(), before);
    graph.run_all().unwrap();
    assert_eq!(graph.node_outputs(b).unwrap().get("out"), Some(&3));
}

#[test]
fn broken_binding_does_not_schedule_a_recycled_resource_during_another_edit() {
    let mut graph = Graph::new(FnExecutor::new());
    let source = graph
        .add_node(constant(1), vec![], vec!["out".into()])
        .unwrap();
    let removed = graph
        .add_node(constant(10), vec![], vec!["out".into()])
        .unwrap();
    let reader = graph
        .add_node(
            FnNode::new(|inputs, outputs, _| {
                outputs.push(inputs.iter().sum());
                Ok(())
            }),
            vec!["left".into(), "right".into()],
            vec!["out".into()],
        )
        .unwrap();
    graph.connect(source, "out", reader, "left").unwrap();
    graph.set_input_value(reader, "right", 0).unwrap();
    graph.run_all().unwrap();
    graph.connect(removed, "out", reader, "left").unwrap();
    drop(graph.remove_node(removed).unwrap());
    assert!(matches!(
        graph.node_input_source(reader, InputId::new(0)).unwrap(),
        Some(InputSource::NodeOutput { node, .. }) if node == removed
    ));
    let recycled = graph
        .add_node(constant(99), vec![], vec!["out".into()])
        .unwrap();
    graph
        .set_input_value_by_id(reader, InputId::new(1), 77, 2)
        .unwrap();
    assert!(
        matches!(graph.run_node(reader), Err(GraphError::MissingUpstreamOutput { node, .. }) if node == removed)
    );
    assert_eq!(graph.node_run_count(recycled), Some(0));
    assert_eq!(graph.node_run_count(source), Some(1));
}

#[test]
fn obsolete_prospective_keys_are_reclaimed_during_long_edit_batches() {
    let mut graph = Graph::new(FnExecutor::new());
    let node = graph
        .add_node(copy(), vec!["input".into()], vec!["out".into()])
        .unwrap();
    graph
        .set_input_value_by_id(node, InputId::new(0), 0, 0)
        .unwrap();
    graph.run_all().unwrap();
    for key in 1..1000 {
        graph
            .set_input_value_by_id(node, InputId::new(0), key, key as i64)
            .unwrap();
        let storage = graph.storage_stats();
        assert!(storage.resources <= 3, "{storage:?}");
        assert!(storage.dependencies <= 2, "{storage:?}");
    }
    assert_eq!(graph.node_outputs(node).unwrap().get("out"), Some(&0));
    graph.run_all().unwrap();
    assert_eq!(graph.node_outputs(node).unwrap().get("out"), Some(&999));
    assert_eq!(graph.storage_stats().resources, 2);
}

#[test]
fn rewired_dynamic_reads_are_resolved_at_runtime_instead_of_blocking_authored_edits() {
    use crate::NodeOutcome;
    let dynamic = Rc::new(Cell::new(true));
    let mut graph = Graph::new(FnExecutor::new());
    let a = graph
        .add_node(copy(), vec!["a".into()], vec!["out".into()])
        .unwrap();
    let read = Rc::clone(&dynamic);
    let b = graph
        .add_node(
            FnNode::restartable(move |inputs, outputs, access| {
                if read.get()
                    && access
                        .read_node_output(a, OutputId::new(0))
                        .unwrap()
                        .is_none()
                {
                    return Ok(NodeOutcome::Pending);
                }
                outputs.push(inputs[0]);
                Ok(NodeOutcome::Complete)
            }),
            vec!["b".into()],
            vec!["out".into()],
        )
        .unwrap();
    graph.set_input_value(a, "a", 1).unwrap();
    graph.connect(a, "out", b, "b").unwrap();
    graph.run_all().unwrap();
    graph.set_input_value(b, "b", 10).unwrap();
    graph.connect(b, "out", a, "a").unwrap();
    assert!(matches!(
        graph.run_node(a),
        Err(GraphError::DynamicDependencyCycle { .. })
    ));
    assert_eq!(graph.node_outputs(b).unwrap().get("out"), Some(&1));
    dynamic.set(false);
    graph.run_node(a).unwrap();
    assert_eq!(graph.node_outputs(a).unwrap().get("out"), Some(&10));
}

#[test]
fn clear_and_inspect_wiring_without_reactivating_historical_prerequisites() {
    let mut graph = Graph::new(FnExecutor::new());
    let a = graph
        .add_node(copy(), vec!["a".into()], vec!["out".into()])
        .unwrap();
    let b = graph
        .add_node(copy(), vec!["b".into()], vec!["out".into()])
        .unwrap();
    graph.set_input_value(a, "a", 1).unwrap();
    graph.connect(a, "out", b, "b").unwrap();
    graph.run_all().unwrap();
    assert!(
        matches!(graph.node_input_source(b, InputId::new(0)).unwrap(), Some(InputSource::NodeOutput { node, output }) if node == a && output == OutputId::new(0))
    );
    graph.clear_input(b, InputId::new(0)).unwrap();
    assert!(
        graph
            .node_input_source(b, InputId::new(0))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        graph
            .node_dependencies(b)
            .unwrap()
            .cloned()
            .collect::<Vec<_>>(),
        [ResourceKey::node_output(a, OutputId::new(0))]
    );
    graph.invalidate_input("a");
    assert!(matches!(graph.run_node(b), Err(GraphError::MissingInput { node, .. }) if node == b));
    assert_eq!(graph.node_run_count(a), Some(1));
    assert_eq!(graph.node_outputs(b).unwrap().get("out"), Some(&1));
    graph.set_input_value(b, "b", 2).unwrap();
    assert!(
        matches!(graph.node_input_source(b, InputId::new(0)).unwrap(), Some(InputSource::External { value, key }) if *value == 2 && key == &ResourceKey::input("b"))
    );
    graph.run_node(b).unwrap();
    assert_eq!(graph.node_outputs(b).unwrap().get("out"), Some(&2));
}

#[test]
fn clear_validates_identity_and_targets_one_duplicate_named_slot() {
    let mut graph = Graph::new(FnExecutor::new());
    let node = graph
        .add_node(
            FnNode::new(|inputs, outputs, _| {
                outputs.push(inputs.iter().sum());
                Ok(())
            }),
            vec!["same".into(), "same".into()],
            vec!["out".into()],
        )
        .unwrap();
    let before = graph.storage_stats();
    graph.clear_input(node, InputId::new(0)).unwrap();
    assert_eq!(graph.storage_stats(), before);
    assert!(matches!(
        graph.clear_input(node, InputId::new(2)),
        Err(GraphError::UnknownInputId { .. })
    ));
    assert!(matches!(
        graph.node_input_source(node, InputId::new(2)),
        Err(GraphError::UnknownInputId { .. })
    ));
    graph.set_input_value(node, "same", 3).unwrap();
    graph.run_all().unwrap();
    graph.clear_input(node, InputId::new(0)).unwrap();
    assert!(
        graph
            .node_input_source(node, InputId::new(0))
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        graph.node_input_source(node, InputId::new(1)).unwrap(),
        Some(InputSource::External { value: &3, .. })
    ));
    graph
        .set_input_value_by_id(node, InputId::new(0), 77, 5)
        .unwrap();
    graph.run_all().unwrap();
    assert_eq!(graph.node_outputs(node).unwrap().get("out"), Some(&8));
    drop(graph.remove_node(node).unwrap());
    assert!(matches!(
        graph.clear_input(node, InputId::new(0)),
        Err(GraphError::BadNodeId)
    ));
    assert!(matches!(
        graph.node_input_source(node, InputId::new(0)),
        Err(GraphError::BadNodeId)
    ));
}
