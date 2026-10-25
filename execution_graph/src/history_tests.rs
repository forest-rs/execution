// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Deterministic edit histories checked against a fresh, cache-free evaluator.
//! No dependency on the scheduler's storage, revisions, or publication implementation.
//! Scope: native scalar nodes with authored inputs and two outputs. Dynamic suspension and
//! host-write semantics have separate focused tests; this is not a general graph fuzzer.

use crate::{
    ExecutionGraph, FnExecutor, FnNode, GraphError, InputId, InputSource, NodeId, NodeStatus,
    OutputId, ResourceKey,
};
use alloc::{
    collections::{BTreeMap, BTreeSet},
    format,
    rc::Rc,
    string::String,
    vec,
    vec::Vec,
};
use core::cell::RefCell;

const SLOTS: usize = 6;
type Graph = ExecutionGraph<FnExecutor<i64, &'static str>>;

#[derive(Clone, Copy, Debug)]
enum Op {
    Add(usize, i64),
    Remove(usize),
    Literal(usize, usize, u64, i64),
    Connect(usize, usize, usize, usize),
    Clear(usize, usize),
    Invalidate(u64),
    Fail(usize, bool),
    Query(usize),
    All,
}

#[derive(Clone, Copy, Debug)]
enum Source {
    Literal(u64, i64),
    Output(NodeId, usize),
}

#[derive(Clone)]
struct Node {
    inputs: [Option<Source>; 2],
    bias: i64,
    fail: bool,
}

#[derive(Clone, Default)]
struct Model {
    nodes: BTreeMap<NodeId, Node>,
    slots: [Option<NodeId>; SLOTS],
}

impl Model {
    // Deliberately recompute every operand. No graph cache, revisions, or scheduler are consulted.
    fn evaluate(&self, node: NodeId, active: &mut BTreeSet<NodeId>) -> Result<[i64; 2], ()> {
        let body = self.nodes.get(&node).ok_or(())?;
        if !active.insert(node) {
            return Err(());
        }
        let mut inputs = [0_i64; 2];
        for (i, source) in body.inputs.iter().enumerate() {
            inputs[i] = match source.ok_or(())? {
                Source::Literal(_, value) => value,
                Source::Output(producer, output) => self.evaluate(producer, active)?[output],
            };
        }
        active.remove(&node);
        if body.fail {
            return Err(());
        }
        Ok([
            inputs[0].wrapping_add(inputs[1]).wrapping_add(body.bias),
            inputs[0].wrapping_mul(inputs[1]).wrapping_sub(body.bias),
        ])
    }

    // Only current authored connections matter. Missing producers remain broken connections.
    fn reaches(&self, node: NodeId, target: NodeId, seen: &mut BTreeSet<NodeId>) -> bool {
        if node == target {
            return true;
        }
        if !seen.insert(node) {
            return false;
        }
        self.nodes.get(&node).is_some_and(|body| {
            body.inputs.iter().flatten().any(|source| match *source {
                Source::Output(producer, _) => self.reaches(producer, target, seen),
                Source::Literal(..) => false,
            })
        })
    }

    fn inspect(&self, graph: &Graph) -> Result<(), String> {
        for (&id, body) in &self.nodes {
            for (slot, expected) in body.inputs.iter().enumerate() {
                let actual = graph
                    .node_input_source(id, InputId::new(u32::try_from(slot).unwrap()))
                    .map_err(|error| format!("source: {error:?}"))?;
                let matches = match (expected, actual) {
                    (None, None) => true,
                    (
                        Some(Source::Literal(key, value)),
                        Some(InputSource::External {
                            key: actual_key,
                            value: actual_value,
                        }),
                    ) => actual_key == &ResourceKey::InputId(*key) && actual_value == value,
                    (
                        Some(Source::Output(node, output)),
                        Some(InputSource::NodeOutput {
                            node: actual_node,
                            output: actual_output,
                        }),
                    ) => *node == actual_node && *output == actual_output.index() as usize,
                    _ => false,
                };
                if !matches {
                    return Err(format!("authored source differs at {id:?}:{slot}"));
                }
            }
            if graph.node_status(id) == Some(NodeStatus::Current) {
                let fresh = self
                    .evaluate(id, &mut BTreeSet::new())
                    .map_err(|()| format!("current {id:?} cannot be freshly evaluated"))?;
                if cached(graph, id) != fresh {
                    return Err(format!(
                        "current {id:?}: cached {:?}, fresh {fresh:?}",
                        cached(graph, id)
                    ));
                }
            }
        }
        Ok(())
    }
}

fn cached(graph: &Graph, node: NodeId) -> Vec<i64> {
    graph
        .node_outputs(node)
        .unwrap()
        .iter()
        .map(|(_, value)| *value)
        .collect()
}

fn reads(graph: &Graph, node: NodeId) -> Vec<ResourceKey> {
    graph.node_dependencies(node).unwrap().cloned().collect()
}

fn publication(graph: &Graph, node: NodeId) -> (Vec<i64>, Vec<ResourceKey>, Option<u64>) {
    (
        cached(graph, node),
        reads(graph, node),
        graph.node_run_count(node),
    )
}

fn replay(ops: &[Op]) -> Result<(), String> {
    replay_with_wrong_sum(ops, false)
}

fn replay_with_wrong_sum(ops: &[Op], wrong_sum: bool) -> Result<(), String> {
    let failures = Rc::new(RefCell::new(BTreeSet::new()));
    let mut graph = Graph::new(FnExecutor::new().with_value_eq(|a, b| a == b));
    let mut model = Model::default();
    for &op in ops {
        match op {
            Op::Add(slot, bias) if model.slots[slot].is_none() => {
                let failures = Rc::clone(&failures);
                let node = graph
                    .add_node(
                        FnNode::<i64, &'static str>::new(move |inputs, outputs, access| {
                            outputs.push(
                                inputs[0]
                                    .wrapping_add(inputs[1])
                                    .wrapping_add(bias)
                                    .wrapping_add(i64::from(wrong_sum)),
                            );
                            outputs.push(inputs[0].wrapping_mul(inputs[1]).wrapping_sub(bias));
                            if failures.borrow().contains(&access.node()) {
                                Err("injected")
                            } else {
                                Ok(())
                            }
                        }),
                        vec!["a".into(), "b".into()],
                        vec!["sum".into(), "product".into()],
                    )
                    .map_err(|error| format!("add: {error:?}"))?;
                model.slots[slot] = Some(node);
                model.nodes.insert(
                    node,
                    Node {
                        inputs: [None; 2],
                        bias,
                        fail: false,
                    },
                );
            }
            Op::Remove(slot) => {
                if let Some(node) = model.slots[slot].take() {
                    drop(
                        graph
                            .remove_node(node)
                            .map_err(|error| format!("remove: {error:?}"))?,
                    );
                    model.nodes.remove(&node);
                    failures.borrow_mut().remove(&node);
                    if graph.node_status(node).is_some() || graph.node_outputs(node).is_some() {
                        return Err(format!("retired {node:?} is still visible"));
                    }
                }
            }
            Op::Literal(slot, input, key, value) => {
                if let Some(node) = model.slots[slot] {
                    graph
                        .set_input_value_by_id(
                            node,
                            InputId::new(u32::try_from(input).unwrap()),
                            key,
                            value,
                        )
                        .map_err(|error| format!("literal: {error:?}"))?;
                    graph.invalidate(ResourceKey::InputId(key));
                    model.nodes.get_mut(&node).unwrap().inputs[input] =
                        Some(Source::Literal(key, value));
                }
            }
            Op::Connect(from, to, input, output) => {
                if let (Some(from), Some(to)) = (model.slots[from], model.slots[to]) {
                    let mut prospective = model.clone();
                    prospective.nodes.get_mut(&to).unwrap().inputs[input] =
                        Some(Source::Output(from, output));
                    let cyclic = prospective.reaches(from, to, &mut BTreeSet::new());
                    let before = (
                        graph.to_dot(),
                        graph.storage_stats(),
                        cached(&graph, to),
                        reads(&graph, to),
                        graph.node_status(to),
                        graph.node_run_count(to),
                    );
                    let result = graph.connect_by_id(
                        from,
                        OutputId::new(u32::try_from(output).unwrap()),
                        to,
                        InputId::new(u32::try_from(input).unwrap()),
                    );
                    if cyclic {
                        if !matches!(result, Err(GraphError::DependencyCycle { .. })) {
                            return Err(format!("cyclic connection accepted: {op:?}, {result:?}"));
                        }
                        let after = (
                            graph.to_dot(),
                            graph.storage_stats(),
                            cached(&graph, to),
                            reads(&graph, to),
                            graph.node_status(to),
                            graph.node_run_count(to),
                        );
                        if before != after {
                            return Err(format!("rejected connection mutated graph: {op:?}"));
                        }
                    } else {
                        result.map_err(|error| {
                            format!("acyclic connection rejected: {op:?}: {error:?}")
                        })?;
                        model = prospective;
                    }
                }
            }
            Op::Clear(slot, input) => {
                if let Some(node) = model.slots[slot] {
                    graph
                        .clear_input(node, InputId::new(u32::try_from(input).unwrap()))
                        .map_err(|error| format!("clear: {error:?}"))?;
                    model.nodes.get_mut(&node).unwrap().inputs[input] = None;
                }
            }
            Op::Invalidate(key) => graph.invalidate(ResourceKey::InputId(key)),
            Op::Fail(slot, fail) => {
                if let Some(node) = model.slots[slot] {
                    model.nodes.get_mut(&node).unwrap().fail = fail;
                    if fail {
                        failures.borrow_mut().insert(node);
                    } else {
                        failures.borrow_mut().remove(&node);
                    }
                    graph.invalidate(ResourceKey::node_output(node, OutputId::new(0)));
                }
            }
            Op::Query(_) | Op::All => {
                let target = match op {
                    Op::Query(slot) => model.slots[slot],
                    _ => None,
                };
                if matches!(op, Op::Query(_)) && target.is_none() {
                    continue;
                }
                let expected_success = match target {
                    Some(node) => model.evaluate(node, &mut BTreeSet::new()).is_ok(),
                    None => model
                        .nodes
                        .keys()
                        .all(|&node| model.evaluate(node, &mut BTreeSet::new()).is_ok()),
                };
                let previous: BTreeMap<_, _> = model
                    .nodes
                    .keys()
                    .map(|&node| (node, publication(&graph, node)))
                    .collect();
                let result = match target {
                    Some(node) => graph.run_node(node),
                    None => graph.run_all(),
                };
                if result.is_ok() != expected_success {
                    return Err(format!(
                        "query success differs: {op:?}, expected {expected_success}, got {result:?}"
                    ));
                }
                let failed_node = match result {
                    Err(GraphError::Node { node, .. } | GraphError::MissingInput { node, .. }) => {
                        Some(node)
                    }
                    Err(GraphError::MissingUpstreamOutput { reader, .. }) => Some(reader),
                    _ => None,
                };
                if let Some(node) = failed_node
                    && previous[&node] != publication(&graph, node)
                {
                    return Err(format!("failed {node:?} changed its publication"));
                }
                if let Some(node) = target {
                    if expected_success && graph.node_status(node) != Some(NodeStatus::Current) {
                        return Err(format!("successful query left {node:?} pending"));
                    }
                } else if expected_success
                    && model
                        .nodes
                        .keys()
                        .any(|&node| graph.node_status(node) != Some(NodeStatus::Current))
                {
                    return Err("successful run_all left pending nodes".into());
                }
            }
            Op::Add(..) => {}
        }
        model.inspect(&graph)?;
    }
    Ok(())
}

// Chunk deletion followed by singleton deletion produces a 1-minimal history: no remaining
// operation can be removed while preserving a failure. Preconditions lost by deletion become
// no-ops (e.g. connecting a missing logical slot), so reduced histories still replay safely.
fn shrink(mut ops: Vec<Op>, mut fails: impl FnMut(&[Op]) -> bool) -> Vec<Op> {
    let mut chunk = ops.len().div_ceil(2);
    while chunk > 0 {
        let mut start = 0;
        while start < ops.len() {
            let end = (start + chunk).min(ops.len());
            let candidate: Vec<_> = ops[..start].iter().chain(&ops[end..]).copied().collect();
            if fails(&candidate) {
                ops = candidate;
                start = 0;
            } else {
                start += chunk;
            }
        }
        chunk /= 2;
    }
    ops
}

fn history(mut state: u64, steps: usize) -> Vec<Op> {
    let mut ops = Vec::new();
    for slot in 0..SLOTS {
        ops.extend([
            Op::Add(slot, slot as i64),
            Op::Literal(slot, 0, (slot * 2) as u64, 2),
            Op::Literal(slot, 1, (slot * 2 + 1) as u64, 3),
        ]);
    }
    ops.push(Op::All);
    for step in 0..steps {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let slot = usize::try_from((state >> 16) % SLOTS as u64).unwrap();
        let other = usize::try_from((state >> 32) % SLOTS as u64).unwrap();
        let input = usize::try_from((state >> 40) & 1).unwrap();
        let key = (state >> 48) % 16;
        let value = i64::from(i8::from_le_bytes([state.to_le_bytes()[0]]));
        ops.push(match (state >> 56) % 16 {
            0 | 1 => Op::Add(slot, value),
            2 => Op::Remove(slot),
            3..=6 => Op::Literal(slot, input, key, value),
            7..=10 => Op::Connect(
                other,
                slot,
                input,
                usize::try_from((state >> 41) & 1).unwrap(),
            ),
            11 => Op::Clear(slot, input),
            12 => Op::Invalidate(key),
            13 => Op::Fail(slot, state & 1 != 0),
            14 => Op::Query(slot),
            _ => Op::All,
        });
        if step % 64 == 63 {
            // Recover periodically so histories exercise successful publications and cutoffs
            // throughout, rather than spending their tail querying only broken graphs.
            for slot in 0..SLOTS {
                ops.extend([
                    Op::Add(slot, value),
                    Op::Literal(slot, 0, (slot * 2) as u64, value),
                    Op::Literal(slot, 1, (slot * 2 + 1) as u64, 3),
                    Op::Fail(slot, false),
                    Op::Query(slot),
                ]);
            }
            ops.push(Op::All);
        }
    }
    ops
}

#[test]
fn generated_edit_histories_match_fresh_evaluation() {
    for seed in 0..64 {
        let ops = history(seed, 512);
        if let Err(error) = replay(&ops) {
            let minimal = shrink(ops, |candidate| replay(candidate).is_err());
            panic!(
                "seed {seed}: {error}\nreduced failure: {:?}\nhistory: {minimal:#?}",
                replay(&minimal)
            );
        }
    }
}

#[test]
fn oracle_detects_and_shrinks_a_planted_wrong_sum() {
    let ops = history(17, 64);
    assert!(replay(&ops).is_ok());
    let fails = |ops: &[Op]| replay_with_wrong_sum(ops, true).is_err();
    assert!(fails(&ops));
    let minimal = shrink(ops.clone(), fails);
    assert!(minimal.len() < ops.len());
    assert!(fails(&minimal));
    for deleted in 0..minimal.len() {
        let mut candidate = minimal.clone();
        candidate.remove(deleted);
        assert!(!fails(&candidate), "history was not 1-minimal");
    }
}

#[test]
fn oracle_detects_a_current_cache_with_omitted_invalidation() {
    let mut graph = Graph::new(FnExecutor::new());
    let node = graph
        .add_node(
            FnNode::<i64, &'static str>::new(|inputs, outputs, _| {
                outputs.extend([inputs[0] + inputs[1], inputs[0] * inputs[1]]);
                Ok(())
            }),
            vec!["a".into(), "b".into()],
            vec!["sum".into(), "product".into()],
        )
        .unwrap();
    graph
        .set_input_value_by_id(node, InputId::new(0), 0, 2)
        .unwrap();
    graph
        .set_input_value_by_id(node, InputId::new(1), 1, 3)
        .unwrap();
    graph.run_all().unwrap();
    let mut model = Model::default();
    model.nodes.insert(
        node,
        Node {
            inputs: [Some(Source::Literal(0, 2)), Some(Source::Literal(1, 3))],
            bias: 0,
            fail: false,
        },
    );
    model.inspect(&graph).unwrap();
    // Same-key updates deliberately require explicit invalidation. Omit it to plant stale data.
    graph
        .set_input_value_by_id(node, InputId::new(0), 0, 7)
        .unwrap();
    model.nodes.get_mut(&node).unwrap().inputs[0] = Some(Source::Literal(0, 7));
    assert_eq!(graph.node_status(node), Some(NodeStatus::Current));
    assert!(model.inspect(&graph).unwrap_err().contains("cached"));
    graph.invalidate(ResourceKey::InputId(0));
    graph.run_all().unwrap();
    model.inspect(&graph).unwrap();
}
