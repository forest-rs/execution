# Graph wind tunnel

Run the Criterion workloads with `cargo bench -p execution_graph_wind_tunnel`.

The standalone native scaling probe isolates node bookkeeping and pending writes:

```sh
cargo +1.95 build --release -p execution_graph_wind_tunnel --bin scaling
target/release/scaling 10000 scoped
/usr/bin/time -l target/release/scaling 100000
```

`scoped` creates independent nodes that read one host key and write another key with
no readers, then queries them individually. The default mode runs the cold graph
with `run_all`. Each subsequent timing is the median of seven repetitions, with
execution counts checked. Values are `u64`; names, dependencies, and bookkeeping
belong to the graph. Process peak RSS includes the executable, allocator overhead,
and transient construction/scheduling storage, and is not retained bytes per node.

Measured on macOS arm64 with Rust 1.95 release builds, 2026-09-27. Baseline is
`e4f51a4` plus this probe; candidate is the revision-based scheduler. These are
microbenchmark observations, not layerstack end-to-end results.

| Workload | Baseline | Revision scheduler |
|---|---:|---:|
| 1,000 nodes, scoped cold queries | 9.96 ms | 0.72 ms |
| 1,000 nodes, unchanged query-all | 6.35 ms | 0.074 ms |
| 10,000 nodes, scoped cold queries | 645.8 ms | 6.24 ms |
| 10,000 nodes, unchanged query-all | 553.1 ms | 0.454 ms |
| 100,000 nodes, scoped cold queries | not run | 46.4 ms |
| 100,000 nodes, unchanged scoped query-all | not run | 3.60 ms |
| 100,000 nodes, one key + `run_all` (default mode) | 17.2 µs | 0.25 µs |
| 100,000 nodes, clean `run_all` (default mode) | 2.04 µs | 0.042 µs |
| 100,000 nodes, creation (default mode) | 13.1 ms | 25.6 ms |
| 100,000 nodes, cold `run_all` (default mode) | 44.9 ms | 37.9 ms |
| 100,000 nodes, peak process RSS (default mode) | 114.9 MB | 138.3 MB |

The scoped scanning and fixed drain costs disappear. Creation and peak memory
regress: dirty consumers now carry shared cause links and an ordered pending set.
Compact ports and reclaimable storage are the next measured change. Invalidation
also performs propagation eagerly; compare full edit-plus-query workloads, not
query time alone, when evaluating fanout-heavy consumers.
