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

The compact-storage follow-up uses indexed outputs, fixed-size port/read arrays,
and reusable node/resource slots. On the same machine and Rust 1.95 release build,
10,000 scoped cold queries took 4.05 ms; unchanged query-all took 0.433 ms.
At 100,000 nodes, scoped cold queries took 41.1 ms and unchanged query-all took
4.84 ms. Single-key edit plus `run_all` was 0.42–1.71 µs across these runs;
clean `run_all` was 0.042 µs. These small timings are noisy.

To measure live allocated heap after the workload, hold the graph in memory:

```sh
target/release/scaling 100000 all --hold
# In another terminal, use the printed process ID:
/usr/bin/heap -q PID
# Press Enter in the probe terminal to exit.
```

With 100,000 independent nodes, the publication-contract baseline (`3ee2f86`
plus the same probe) retained 115,768,064 allocated bytes in 1,100,219 allocations.
Compact storage retained 75,565,888 bytes in 700,210 allocations, about 35% fewer
bytes. These are process-wide live heap totals, including probe/runtime state;
they are neither peak RSS nor isolated graph payload sizes. Construction took
22.0 ms and cold `run_all` took 35.9 ms in the compact-storage heap run.

The node-removal regression separately checks 1,000 create/run/remove cycles:
live nodes, resources, dependencies, and pending outputs return to zero after
each removal, while allocated slot capacities remain bounded. Public node IDs
are never reused even when physical storage is reused.
