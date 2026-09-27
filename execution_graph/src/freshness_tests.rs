// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

extern crate std;

use crate::{ExecutionGraph, FnExecutor, FnNode, HostOpId, ResourceKey};
use alloc::{rc::Rc, vec};
use core::cell::Cell;

#[test]
fn a_late_reader_observes_a_write_without_inheriting_old_dirty_work() {
    let mut graph = ExecutionGraph::new(FnExecutor::<u64, ()>::new());
    let op = HostOpId::new(1);
    let writer = graph
        .add_node(
            FnNode::new(move |_, outputs, access| {
                access.write_host_state(op, 0);
                outputs.push(7);
                Ok(())
            }),
            vec![],
            vec!["out".into()],
        )
        .unwrap();
    graph.run_node(writer).unwrap();
    let reader = graph
        .add_node(
            FnNode::new(move |_, outputs, access| {
                access.read_host_state(op, 0);
                outputs.push(7);
                Ok(())
            }),
            vec![],
            vec!["out".into()],
        )
        .unwrap();
    graph.run_node(reader).unwrap();
    assert_eq!(graph.run_node(reader).unwrap().executed_nodes, 0);
    assert_eq!(graph.run_all().unwrap().executed_nodes, 0);
    graph.invalidate(ResourceKey::host_state(op, 0));
    assert_eq!(graph.run_node(reader).unwrap().executed_nodes, 1);
}

#[test]
fn writes_from_failed_attempts_still_invalidate_prior_readers() {
    let shared = Rc::new(Cell::new(0));
    let mut graph = ExecutionGraph::new(FnExecutor::<i64, ()>::new());
    let op = HostOpId::new(1);
    let read = shared.clone();
    let reader = graph
        .add_node(
            FnNode::new(move |_, outputs, access| {
                access.read_host_state(op, 0);
                outputs.push(read.get());
                Ok(())
            }),
            vec![],
            vec!["out".into()],
        )
        .unwrap();
    graph.run_node(reader).unwrap();
    let writer = graph
        .add_node(
            FnNode::new(move |_, _, access| {
                shared.set(9);
                access.write_host_state(op, 0);
                Err(())
            }),
            vec![],
            vec!["out".into()],
        )
        .unwrap();
    assert!(graph.run_node(writer).is_err());
    assert_eq!(graph.run_node(reader).unwrap().executed_nodes, 1);
    assert_eq!(graph.node_outputs(reader).unwrap().get("out"), Some(&9));
    assert_eq!(graph.run_node(reader).unwrap().executed_nodes, 0);
}
