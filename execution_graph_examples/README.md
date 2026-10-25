# Execution graph examples

- `tax`: tape nodes, host-state invalidation and dependency reports.
- `transforms`: native nodes with dynamic dependencies, scoped queries and fresh-evaluation checks.
- `procedural`: mixed tape/native geometry, owned viewport/undo results and selected VM recordings.

## Procedural surface

```sh
cargo run -p execution_graph_examples --bin procedural -- --edits 1000 --explain
cargo run -p execution_graph_examples --features observatory --bin procedural -- --inspect
cargo run -p execution_graph_examples --features observatory --bin procedural -- --snapshot
cargo run -p execution_graph_examples --bin procedural -- --write-mesh surface.obj
cargo run -p execution_graph_examples --features footfall --bin procedural -- --edits 8 --capture .local/whole-edit
```

The graph compiles `segments = density * levels + quality()` and a deformation tuple into tape.
Native nodes build a grid, deform its points and compute bounds. Geometry travels through
reference-counted native values; small tape values retain their own aggregate/program domains.
The mixed executor delegates through `TapeExecutor::execute_node` without exposing VM storage.

Amplitude edits reuse grid topology. A host quality edit rebuilds it. A rejected deformation
preserves the previous viewport result as pending; recovery makes it current again. Each edit
checks bounds against an independent closed form and checks that obsolete mesh payloads drop.
All nodes are then removed while a deliberately retained undo mesh stays readable.

`--record` retains selected tape invocations (at most three). `--explain` shows their actual watched
values and authored source ranges; `--inspect` opens the same evidence with validated source
snapshots in Observatory. Replay checks the tape invocation and recorded host result after the
graph disappears. It does not replay native geometry or prove graph publication.

`--write-mesh` writes the final native surface as OBJ, refusing to overwrite an existing file.
`--density` accepts 1..=64; segment counts are limited to 512 and amplitudes must be finite and
within -100..=100. These are example-domain limits, not a general VM resource policy.

The edit-loop duration includes graph work, native deformation, independent bound checks and
lease-release checks. Selected capture timing includes sink setup, VM execution and result
retention, excluding input import and recorder finalization. Neither is an isolated VM benchmark
or an allocator/RSS measurement. Aggregate ingress still copies selected reachable graphs;
output bundles retain storage until their last owner drops. Use the wind tunnels for isolated costs.

## Whole-edit Footfall experiment

`--capture DIR` requires the optional `footfall` feature and creates a new directory,
refusing to overwrite an existing one. It supports at most 1,024 amplitude edits.
Read its captures with tools supporting Footfall 0.2.
The default build and both core crates gain no Footfall dependency.

Each edit is an existing Footfall **transaction marker** and scope. Transactions
group observations; they do not roll back graph publications. Footfall scopes
describe executor bodies and host work. Separate messages from actual graph
reports describe successful publication, scheduled-node cutoff, scheduling causes,
and failed queries. These are post-query notifications, so their timestamps do
not measure when publication occurred or its latency. A failed edit can publish
its parameter node and retain the old mesh: body completion, graph publication,
and useful output remain distinct.
Flows correlate the observed serial stages, without claiming a critical path.

The experiment tracks distinct native point and topology buffers by their `Vec`
capacities. Shared topology has one identity. Explicit cache, undo, and temporary
failure-check roots name the buffers they retain. These are semantic ownership
checkpoints, not interception of every Rust reference operation. The report shows
which buffers undo retains after node removal, then their release when undo drops.
Allocator/object overhead and VM storage are outside these byte totals.

The directory contains:

- `edits.ffcap`: streamed Footfall capture, reopened for analysis after execution.
- `timeline.pftrace`: Perfetto export of the complete capture, including earlier
  windows that may no longer fit the runtime's bounded ring.
- `report.txt`: edit outcomes, scheduling causes, explicit ownership roots, scope
  coverage, buffer lifetimes, and amplitude/topology comparison populations.
- `program.extape`, `parameters.expr`, and `manifest.json`: the exact compiled
  program, authored source, instruction byte ranges, and edit/node/attempt correlations.
- `vm-N-A.txt`: checked VM replay explanations for node N, attempt A, with watched
  values and source ranges. These are readable explanations, not portable replay
  archives. Capture mode also selects the VM invocation for the rejected edit.

There are no fabricated rendering frames or presentation measurements. Native
geometry and whole-graph effects are not replayed. The report requires complete
scope/resource evidence and preserves comparison caveats. It separates recording
mode and item count when comparing populations: recording setup is observed work,
and changing topology changes workload size. These are explanations of this run,
not statistically established speedup measurements.

Per-edit scopes exclude file checkpointing, while the printed loop duration in
capture mode includes it. Counter sampling retains Footfall's default frame-end
policy; this headless experiment records storage checkpoints as explicit messages.
