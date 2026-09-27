// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use crate::{ExecutionGraph, FnExecutor, FnNode, GraphError, InputId, OutputId, ResourceKey};
use alloc::vec;

type Graph = ExecutionGraph<FnExecutor<i64, ()>>;
fn copy() -> FnNode<i64, ()> {
    FnNode::new(|inputs, outputs, _| {
        outputs.extend_from_slice(inputs);
        Ok(())
    })
}

#[test]
fn numeric_slots_and_external_keys_have_distinct_identity() {
    let mut g = Graph::new(FnExecutor::new());
    let a = g
        .add_node(copy(), vec!["x".into()], vec!["out".into()])
        .unwrap();
    let b = g
        .add_node(copy(), vec!["x".into()], vec!["out".into()])
        .unwrap();
    g.set_input_value_by_id(a, InputId::new(0), 10, 1).unwrap();
    g.set_input_value_by_id(b, InputId::new(0), 20, 2).unwrap();
    g.run_all().unwrap();
    g.set_input_value_by_id(a, InputId::new(0), 10, 3).unwrap();
    g.invalidate_many([
        ResourceKey::InputId(10),
        ResourceKey::InputId(10),
        ResourceKey::input("x"),
    ]);
    assert_eq!(g.run_all().unwrap().executed_nodes, 1);
    assert_eq!(
        g.node_outputs(a).unwrap().get_by_id(OutputId::new(0)),
        Some(&3)
    );
    assert_eq!(
        g.node_outputs(b).unwrap().get_by_id(OutputId::new(0)),
        Some(&2)
    );
    assert!(matches!(
        g.set_input_value_by_id(a, InputId::new(1), 10, 9),
        Err(GraphError::UnknownInputId { .. })
    ));
}

#[test]
fn removal_while_dirty_retires_identity_and_keeps_consumers_recoverable() {
    let mut g = Graph::new(FnExecutor::new());
    let a = g
        .add_node(copy(), vec!["x".into()], vec!["out".into()])
        .unwrap();
    let b = g
        .add_node(copy(), vec!["v".into()], vec!["out".into()])
        .unwrap();
    g.set_input_value(a, "x", 1).unwrap();
    g.connect_by_id(a, OutputId::new(0), b, InputId::new(0))
        .unwrap();
    g.run_all().unwrap();
    g.invalidate_input("x");
    let removed = g.remove_node(a).unwrap();
    assert_eq!(removed.outputs.get("out"), Some(&1));
    let replacement = g
        .add_node(copy(), vec!["x".into()], vec!["out".into()])
        .unwrap();
    assert_ne!(replacement, a);
    g.set_input_value(replacement, "x", 9).unwrap();
    assert!(
        matches!(g.run_node(b), Err(GraphError::MissingUpstreamOutput { reader, node, output }) if reader == b && node == a && output == OutputId::new(0))
    );
    assert_eq!(g.node_outputs(b).unwrap().get("out"), Some(&1));
    assert!(g.node_outputs(a).is_none());
    assert!(g.remove_node(a).is_err());
    g.connect(replacement, "out", b, "v").unwrap();
    assert_eq!(g.run_node(b).unwrap().executed_nodes, 2);
    assert_eq!(g.node_outputs(b).unwrap().get("out"), Some(&9));
    assert_eq!(g.run_all().unwrap().executed_nodes, 0);
}

#[test]
fn churn_reuses_storage_without_reusing_public_node_ids() {
    let mut g = Graph::new(FnExecutor::new());
    let mut previous = None;
    for key in 0..1000 {
        let node = g
            .add_node(copy(), vec!["x".into()], vec!["out".into()])
            .unwrap();
        assert_ne!(Some(node), previous);
        g.set_input_value_by_id(node, InputId::new(0), key, 1)
            .unwrap();
        g.run_node(node).unwrap();
        g.remove_node(node).unwrap();
        let stats = g.storage_stats();
        assert_eq!(
            (
                stats.nodes,
                stats.resources,
                stats.dependencies,
                stats.pending_outputs
            ),
            (0, 0, 0, 0)
        );
        assert!(
            stats.node_capacity <= 4
                && stats.resource_capacity <= 4
                && stats.dependency_capacity <= 4,
            "{stats:?}"
        );
        previous = Some(node);
    }
}
