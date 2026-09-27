// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Conditional world-transform reads with graph-owned values and a fresh reference evaluator.

use execution_graph::{
    ExecutionGraph, FnExecutor, FnNode, HostOpId, NodeId, NodeOutcome, NodeStatus, OutputId,
    OutputReadError, ResourceKey,
};
use std::{cell::RefCell, rc::Rc};

const SCENE: HostOpId = HostOpId::new(1);
const WORLD: OutputId = OutputId::new(0);

/// Affine 2D transform: [a, b, c, d, tx, ty], with column vectors.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Transform([f64; 6]);
impl Transform {
    fn translation(x: f64, y: f64) -> Self {
        Self([1.0, 0.0, 0.0, 1.0, x, y])
    }
    fn compose(self, rhs: Self) -> Self {
        let [a, b, c, d, x, y] = self.0;
        let [e, f, g, h, u, v] = rhs.0;
        Self([
            a * e + c * f,
            b * e + d * f,
            a * g + c * h,
            b * g + d * h,
            a * u + c * v + x,
            b * u + d * v + y,
        ])
    }
}
#[derive(Clone, Debug)]
struct Prim {
    local: Transform,
    reset: bool,
    parent: Option<usize>,
}
#[derive(Debug)]
struct Scene {
    prims: Vec<Prim>,
    nodes: Vec<NodeId>,
}
type Graph = ExecutionGraph<FnExecutor<Transform, OutputReadError>>;

fn world_node(scene: Rc<RefCell<Scene>>, prim: usize) -> FnNode<Transform, OutputReadError> {
    FnNode::restartable(move |_: &[Transform], outputs, access| {
        access.read_host_state(SCENE, prim as u64);
        let (local, reset, parent) = {
            let scene = scene.borrow();
            let data = &scene.prims[prim];
            (
                data.local,
                data.reset,
                data.parent.map(|parent| scene.nodes[parent]),
            )
        };
        let world = if !reset && let Some(parent) = parent {
            let Some(parent_world) = access.read_node_output(parent, WORLD)? else {
                return Ok(NodeOutcome::Pending);
            };
            parent_world.compose(local)
        } else {
            local
        };
        outputs.push(world);
        Ok(NodeOutcome::Complete)
    })
}

fn fresh_world(scene: &Scene, prim: usize) -> Transform {
    let data = &scene.prims[prim];
    if !data.reset
        && let Some(parent) = data.parent
    {
        fresh_world(scene, parent).compose(data.local)
    } else {
        data.local
    }
}
fn verify(graph: &mut Graph, scene: &Scene, prim: usize) {
    let node = scene.nodes[prim];
    graph.run_node(node).unwrap();
    let cached = graph.node_outputs(node).unwrap().get_by_id(WORLD).unwrap();
    assert_eq!(
        cached.0.map(f64::to_bits),
        fresh_world(scene, prim).0.map(f64::to_bits),
        "incremental world must match a fresh evaluation bitwise"
    );
    assert_eq!(
        graph.node_status(node),
        Some(NodeStatus::Current),
        "query must establish freshness"
    );
}

fn main() {
    let scene = Rc::new(RefCell::new(Scene {
        prims: vec![
            Prim {
                local: Transform::translation(10.0, 0.0),
                reset: false,
                parent: None,
            },
            Prim {
                local: Transform::translation(0.0, 3.0),
                reset: false,
                parent: Some(0),
            },
            Prim {
                local: Transform::translation(2.0, 1.0),
                reset: false,
                parent: Some(1),
            },
        ],
        nodes: Vec::new(),
    }));
    let mut graph = Graph::new(FnExecutor::new().with_value_eq(|a, b| a == b));
    for prim in 0..3 {
        let node = graph
            .add_node(
                world_node(scene.clone(), prim),
                vec![],
                vec!["world".into()],
            )
            .unwrap();
        graph
            .set_node_label(node, format!("world[{prim}]"))
            .unwrap();
        scene.borrow_mut().nodes.push(node);
    }
    // One query discovers and computes every required ancestor; there is no host query loop.
    verify(&mut graph, &scene.borrow(), 2);
    let [root, middle, leaf] = scene.borrow().nodes[..] else {
        unreachable!()
    };
    scene.borrow_mut().prims[1].reset = true;
    graph.invalidate(ResourceKey::host_state(SCENE, 1));
    verify(&mut graph, &scene.borrow(), 2);
    assert!(
        !graph
            .node_dependencies(middle)
            .unwrap()
            .any(|key| *key == ResourceKey::node_output(root, WORLD)),
        "reset must remove the parent dependency"
    );
    // A root edit no longer reaches the reset subtree.
    scene.borrow_mut().prims[0].local = Transform::translation(20.0, 0.0);
    graph.invalidate(ResourceKey::host_state(SCENE, 0));
    assert_eq!(
        graph.run_node(leaf).unwrap().executed_nodes,
        0,
        "root edit must stop at reset"
    );
    assert_eq!(
        graph.node_status(root),
        Some(NodeStatus::Pending),
        "unqueried root must remain pending"
    );
    // Clearing reset discovers the pending parent outside the old dependency scope.
    scene.borrow_mut().prims[1].reset = false;
    graph.invalidate(ResourceKey::host_state(SCENE, 1));
    verify(&mut graph, &scene.borrow(), 2);
    println!(
        "world={:?}; conditional dependencies and fresh-reference checks passed",
        graph.node_outputs(leaf).unwrap().get_by_id(WORLD)
    );
}
