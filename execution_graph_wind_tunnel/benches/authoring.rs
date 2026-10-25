// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use execution_graph::{ExecutionGraph, FnExecutor, FnNode, InputId, NodeId, OutputId};
use std::hint::black_box;

type Graph = ExecutionGraph<FnExecutor<u64, ()>>;

fn copy() -> FnNode<u64, ()> {
    FnNode::new(|inputs, outputs, _| {
        outputs.push(inputs[0]);
        Ok(())
    })
}

fn prepare(fan_in: usize, published: bool) -> (Graph, Vec<NodeId>, NodeId) {
    let mut graph = Graph::new(FnExecutor::new());
    let mut leaves = Vec::with_capacity(fan_in);
    for arm in 0..fan_in {
        let mut previous = None;
        for _ in 0..32 {
            let node = graph
                .add_node(copy(), vec!["input".into()], vec!["out".into()])
                .unwrap();
            if let Some(previous) = previous {
                graph
                    .connect_by_id(previous, OutputId::new(0), node, InputId::new(0))
                    .unwrap();
            } else {
                graph
                    .set_input_value_by_id(node, InputId::new(0), arm as u64, arm as u64)
                    .unwrap();
            }
            previous = Some(node);
        }
        leaves.push(previous.unwrap());
    }
    let sink = graph
        .add_node(
            FnNode::new(|inputs, outputs, _| {
                outputs.push(inputs.iter().sum());
                Ok(())
            }),
            (0..fan_in).map(|slot| slot.to_string().into()).collect(),
            vec!["out".into()],
        )
        .unwrap();
    if published {
        for slot in 0..fan_in {
            graph
                .set_input_value_by_id(sink, InputId::new(slot as u32), 10_000 + slot as u64, 0)
                .unwrap();
        }
        graph.run_all().unwrap();
    }
    (graph, leaves, sink)
}

fn authoring(c: &mut Criterion) {
    for (name, published) in [("cold_wide_join", false), ("published_wide_join", true)] {
        let mut group = c.benchmark_group(name);
        for fan_in in [16, 64] {
            group.bench_with_input(
                BenchmarkId::from_parameter(fan_in),
                &fan_in,
                |b, &fan_in| {
                    // Time only connections into one join. Preparation, execution (when requested),
                    // and destruction are outside the measured interval. Each arm has 32 nodes.
                    b.iter_batched_ref(
                        || prepare(fan_in, published),
                        |(graph, leaves, sink)| {
                            for (slot, &leaf) in leaves.iter().enumerate() {
                                graph
                                    .connect_by_id(
                                        leaf,
                                        OutputId::new(0),
                                        *sink,
                                        InputId::new(slot as u32),
                                    )
                                    .unwrap();
                            }
                            black_box(graph);
                        },
                        BatchSize::SmallInput,
                    );
                },
            );
        }
        group.finish();
    }
}

criterion_group!(benches, authoring);
criterion_main!(benches);
