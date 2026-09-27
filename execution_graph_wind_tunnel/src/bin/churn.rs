// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Native create/rewire/remove probe with a bounded live working set.
use execution_graph::{ExecutionGraph, Executor, HostOpId, NodeAccess, ResourceKey};
use std::{convert::Infallible, hint::black_box, time::Instant};

#[derive(Debug)]
struct Native {
    width: u64,
}
impl Executor for Native {
    type Value = u64;
    type Node = u64;
    type Error = Infallible;
    fn execute(
        &mut self,
        node: &mut u64,
        _: &[u64],
        outputs: &mut Vec<u64>,
        access: &mut NodeAccess<'_, u64>,
    ) -> Result<execution_graph::NodeOutcome, Infallible> {
        for key in 0..=self.width {
            access.read_host_state(HostOpId::new(0), *node * 64 + key);
        }
        outputs.push(self.width);
        Ok(execution_graph::NodeOutcome::Complete)
    }
}
fn main() {
    let count: u64 = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "1000".into())
        .parse()
        .unwrap();
    let mut graph = ExecutionGraph::new(Native { width: 1 });
    let mut samples = Vec::new();
    let mut steady_storage = None;
    for cycle in 0..21 {
        let start = Instant::now();
        let nodes: Vec<_> = (0..count)
            .map(|i| graph.add_node(i, vec![], vec!["out".into()]).unwrap())
            .collect();
        for width in [1, 16, 3, 32, 0] {
            graph.executor_mut().width = width;
            graph.invalidate_many(
                (0..count).map(|i| ResourceKey::host_state(HostOpId::new(0), i * 64)),
            );
            assert_eq!(graph.run_all().unwrap().executed_nodes, count as usize);
            for &node in &nodes {
                assert_eq!(graph.node_outputs(node).unwrap().get("out"), Some(&width));
            }
        }
        for node in nodes {
            black_box(graph.remove_node(node).unwrap());
        }
        let stats = graph.storage_stats();
        assert_eq!(
            (
                stats.nodes,
                stats.resources,
                stats.dependencies,
                stats.pending_outputs
            ),
            (0, 0, 0, 0)
        );
        if cycle == 1 {
            steady_storage = Some(stats);
        }
        if cycle > 1 {
            assert_eq!(
                Some(stats),
                steady_storage,
                "storage must plateau after warmup"
            );
        }
        if cycle > 0 {
            samples.push(start.elapsed().as_nanos());
        }
    }
    samples.sort_unstable();
    println!(
        "nodes_per_cycle={count} rewrites_per_cycle=5 measured_cycles=20 median_cycle_ns={} storage={:?}",
        samples[10],
        graph.storage_stats()
    );
}
