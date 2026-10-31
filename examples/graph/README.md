# Execution graph examples

See the [examples guide](../README.md) for the purpose of each graph and tape binary.
The walkthroughs below cover the larger application workflows.

- `tax`: tape nodes, host-state invalidation and dependency reports.
- `transforms`: native nodes with dynamic dependencies, scoped queries and fresh-evaluation checks.
- `procedural`: mixed tape/native geometry, owned viewport/undo results and selected VM recordings.
- `live_code`: graph-exported closures, shared mutable cells, versioned lookup and paused old code.
- `deferred_assets`: real pending I/O, owned guest tasks, native results, cancellation and source-linked explanations.

## Deferred assets and owned results

```sh
cargo run -p execution_graph_examples --bin deferred_assets
cargo run -p execution_graph_examples --bin deferred_assets -- --json
cargo run -p execution_graph_examples --features observatory --bin deferred_assets -- --snapshot
cargo run -p execution_graph_examples --features observatory --bin deferred_assets -- --observatory
cargo run -p execution_graph_examples --features observatory --bin deferred_assets -- --snapshot --capture .local/deferred-assets
```

The guest compiles `points = asset.load_points(asset); return points` to a host call and return.
Its owned invocation yields at the call. An application-owned worker reads a real loopback socket;
the graph task keeps the guest and request across queries. Independent gain edits publish while
the guest waits, without restarting the request or consuming more guest fuel. The script supplies
responses and joins workers outside graph execution, so it needs no runtime or timed sleeps.

The edit history queues an obsolete response, rebinds the input before the guest admits it, and
checks that the queued point buffer drops. A replacement publishes a 32 by 32 point grid. A
malformed replacement preserves that cache as pending, with the typed decoder failure available
through the graph/VM error chain. Recovery publishes a new cloud. Native bounds match the earlier
bounds, so the caption node is cut off; geometry identity still changes.

Coordinates stay in a packed native `Vec<i64>`. The host constructs a small, contract-checked guest
record containing an opaque object token, vertex count, first-point preview and source name.
`Invocation::into_snapshot` moves the reachable guest result out of its VM; the graph value owns
both its retained `TapeValue` metadata and the native buffer. Removing the node and dropping the
returned `RemovedNode` releases the newest buffer. Explicit viewport and undo roots keep the older
one valid until those roots drop. Recordings keep metadata and tokens, never native coordinates.

The explanation joins graph attempts, pending/committed reads, publication/cutoff reports, host
requests, guest outcomes, exact authored source, native bounds source, work debits and explicit
ownership checkpoints. Graph publication and recorded guest completion are separate evidence.
Checked replay reproduces successful guest metadata and the failed guest trap; the cancelled
capture remains incomplete and refuses replay. It reconstructs neither native geometry nor I/O
or whole-graph execution.

`--snapshot` shows the actual guest awaiting its first response, then finishes the script.
`--observatory` opens the completed recovery recording after execution; it is an immutable guest
view, not a live asset editor. Both attach the exact `points.scene` source map. Capture mode creates
a new directory, refusing an existing one, with `report.txt`, `report.json`, `asset.tape`,
`asset.sources`, the authored `points.scene`, a native source snapshot `native.rs`, and optional
`pending-0.txt`. Recordings remain in memory; these files are not a portable recording archive.
For example, inspect the exact compiled input separately:

```sh
cargo run -p execution_tape_examples --bin observatory -- --program .local/deferred-assets/asset.tape --sources .local/deferred-assets/asset.sources
```

This finite, four-request service demonstrates an embedding policy. Admission synchronously opens
a local socket pair and starts a worker; response reading/decoding happen on that worker. Responses
are limited to 1 MiB with nonempty XYZ triples; a 10-second read timeout is a failure bound, not a
simulated delay. Tests require permission to open loopback sockets. A production service would
also manage worker slots, scheduling, shutdown, object namespaces and domain-specific limits.
The example's JSON schema and formatted resource keys are explanatory example data, not a stable
library interchange format.

Query wall time includes scheduling and body work. Poll wall time includes guest resumption and
native result admission; task lifetime also includes waiting and caller scheduling. These metrics
overlap and are not CPU or isolated I/O latency measurements. Buffer counts track coordinate
`Vec` capacities, excluding VM metadata, allocator overhead and RSS.

## Shared cells and live code

```sh
cargo run -p execution_graph_examples --bin live_code
cargo run -p execution_graph_examples --features observatory --bin live_code -- --snapshot
cargo run -p execution_graph_examples --features observatory --bin live_code -- --interactive
cargo run -p execution_graph_examples --features observatory --bin live_code -- --snapshot --capture .local/live-counter
```

The graph builds two closures capturing the same host-owned `CounterCell`, then exports them
with `TapeValue::retain_callable` and drops the graph. One old `+1` closure pauses before its
host call. `CodeSlot` publishes a compatible `+10` factory; a new lookup creates a callback
that returns **10**. Resuming the paused old callback returns **11**; the second old closure
returns **12**. Code and immutable environments are retained independently; the mutable cell
is shared through its opaque host object token. No active frame or escaped closure is patched.

The live view opens on that paused old call **after** the new callback ran. The program header
and authored source show increment `1`, while the object inspector shows the current cell value
`10`. Use `Backspace` to return to registers, `Tab` to select code, and `s` to execute the real
host call. `--snapshot` renders this boundary and then finishes. `--interactive` leaves control
with you: quitting cancels unfinished work; host effects that already happened remain real.

The example visits callable and paused-invocation object references before collecting an
unreachable second cell. This is a tiny embedding-owned heap, not a general guest collector;
real languages must also account for heap edges, globals and external roots. Three independent
recordings replay the completed callbacks without mutating the live cell. A cancelled callback
has no fabricated result or checked replay. Stale source maps are explicitly rejected.

Capture mode creates a new directory containing `story.json`, both exact programs, their source
sidecars, and the pre-resumption `paused.txt` view. It refuses an existing directory. These are
inspectable inputs and a result survey, not a portable heap or recording archive. For example:

```sh
cargo run -p execution_tape_examples --bin observatory -- --program .local/live-counter/old.tape --sources .local/live-counter/old.sources
```

The embedding owns selector/class lookup, publication synchronization, cache invalidation and
object retention. Guest calls still stay inside one prepared program; this example does not
provide cross-program guest calls, non-local returns, a language compiler or register editing.

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
