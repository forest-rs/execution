// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Standalone native graph scaling probe. Run in release mode; use `/usr/bin/time -l` for
//! process peak RSS separately from timings. No allocator instrumentation or host allocations.

use std::convert::Infallible;
use std::hint::black_box;
use std::time::Instant;

use execution_graph::{ExecutionGraph, Executor, HostOpId, NodeAccess, ResourceKey};

#[derive(Debug)]
struct Native {
    writes: bool,
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
        access: &mut NodeAccess<'_>,
    ) -> Result<(), Infallible> {
        access.read_host_state(HostOpId::new(0), *node);
        if self.writes {
            access.write_host_state(HostOpId::new(1), *node);
        }
        outputs.push(*node);
        Ok(())
    }
}

fn measured(mut f: impl FnMut()) -> u128 {
    let mut samples = Vec::new();
    for _ in 0..7 {
        let start = Instant::now();
        f();
        samples.push(start.elapsed().as_nanos());
    }
    samples.sort_unstable();
    samples[3]
}

fn main() {
    let mut args = std::env::args().skip(1);
    let count: u32 = args
        .next()
        .unwrap_or_else(|| "10000".into())
        .parse()
        .unwrap();
    let scoped = args.next().is_some_and(|s| s == "scoped");
    let hold = args.any(|arg| arg == "--hold");
    assert!(count > 0);
    let mut graph = ExecutionGraph::new(Native { writes: scoped });
    let start = Instant::now();
    let nodes: Vec<_> = (0..count)
        .map(|id| {
            graph
                .add_node(u64::from(id), vec![], vec!["out".into()])
                .unwrap()
        })
        .collect();
    println!(
        "nodes={count} mode={} create_ns={}",
        if scoped { "scoped" } else { "all" },
        start.elapsed().as_nanos()
    );
    let start = Instant::now();
    if scoped {
        for &node in &nodes {
            assert_eq!(graph.run_node(node).unwrap().executed_nodes, 1);
        }
    } else {
        assert_eq!(graph.run_all().unwrap().executed_nodes, count as usize);
    }
    println!("cold_ns={}", start.elapsed().as_nanos());
    let unchanged = measured(|| {
        for &node in &nodes {
            assert_eq!(graph.run_node(node).unwrap().executed_nodes, 0);
        }
    });
    println!("unchanged_query_all_ns={unchanged}");
    let last = *nodes.last().unwrap();
    println!(
        "one_key_run_all_ns={}",
        measured(|| {
            graph.invalidate(ResourceKey::host_state(
                HostOpId::new(0),
                u64::from(count - 1),
            ));
            assert_eq!(graph.run_all().unwrap().executed_nodes, 1);
            black_box(graph.node_outputs(last));
        })
    );
    println!(
        "clean_run_all_ns={}",
        measured(|| {
            assert_eq!(graph.run_all().unwrap().executed_nodes, 0);
        })
    );
    if hold {
        println!("hold_pid={}", std::process::id());
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).unwrap();
    }
    black_box(graph);
}
