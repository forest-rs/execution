# Execution examples

Start here to choose a runnable example. These unpublished workspace packages keep application,
terminal UI and profiling dependencies outside the core crates. Run the commands from the
workspace root. Binary names and arguments are unchanged by the move into this directory.

| Package | Location | Run |
| --- | --- | --- |
| `execution_tape_examples` | [`tape/`](tape/) | `cargo run -p execution_tape_examples --bin NAME` |
| `execution_graph_examples` | [`graph/`](graph/) | `cargo run -p execution_graph_examples --bin NAME` |

Tape examples previously run with `-p execution_tape_wind_tunnel` now use
`-p execution_tape_examples`. The wind tunnels retain benchmarks and standalone performance probes.

## Three places to start

```sh
# Build two blocks sharing a cell; follow a lexical return through ensure cleanup.
cargo run -p execution_tape_examples --bin smalltalk_blocks -- --explain --snapshot

# Compile an arithmetic expression with exact source ranges, then inspect its execution.
cargo run -p execution_tape_examples --bin source_frontend -- --snapshot

# Follow dynamic dependencies through an incremental native graph.
cargo run -p execution_graph_examples --bin transforms
```

## Tape: authoring, values and host integration

| Example | What it is for |
| --- | --- |
| [`bound_host`](tape/src/bin/bound_host.rs) | Bind complete host signatures before execution, reuse bindings with independent state, and preserve an owned domain error. |
| [`disasm`](tape/src/bin/disasm.rs) | Assemble a small program and print its decoded instructions and control flow. |
| [`block_parameters`](tape/src/bin/block_parameters.rs) | Compare explicit register moves with typed block arguments in equivalent loops; inspect prepared execution requirements. |
| [`nested_contracts`](tape/src/bin/nested_contracts.rs) | Construct and project nested aggregate contracts through assembly, encoding, verification and execution. Optionally write a tape for inspection. |
| [`typed_callable`](tape/src/bin/typed_callable.rs) | Invoke a first-class function through an exact indirect signature and return a nested value. Optionally save the tape. |
| [`join_contracts`](tape/src/bin/join_contracts.rs) | See which tuple guarantees survive when different member kinds meet at a typed join. Optionally save the tape for the browser. |

## Tape: source, inspection and debugging

| Example | What it is for |
| --- | --- |
| [`source_frontend`](tape/src/bin/source_frontend.rs) | Parse a small arithmetic language, emit exact source mappings, run it and explore the resulting program or recording. Can export a tape and source sidecar. |
| [`inspect_prepared`](tape/src/bin/inspect_prepared.rs) | Inspect an existing tape's prepared functions, contracts, operands and selected verifier facts without executing it. Supply a tape path and optional function ID/PC. |
| [`observatory`](tape/src/bin/observatory.rs) | Browse an existing tape and optional source sidecar, or record a read/compute/store demonstration and explore its boundary values and instruction history. |
| [`live_debugger`](tape/src/bin/live_debugger.rs) | Step a live invocation with a real host object, nested calls, object inspection and authored source connections. |

For a file-based workflow:

```sh
mkdir -p .local
cargo run -p execution_tape_examples --bin nested_contracts -- .local/nested.tape
cargo run -p execution_tape_examples --bin inspect_prepared -- .local/nested.tape 0
cargo run -p execution_tape_examples --bin observatory -- --program .local/nested.tape --snapshot
```

Pick an unused output filename; the tape generator preserves existing files.

See the [Observatory guide](../execution_tape_observatory/README.md) for navigation, live controls
and the distinction between static verifier facts and recorded observations.

## Tape: blocks, conditions and live code

| Example | What it is for | Guide |
| --- | --- | --- |
| [`smalltalk_blocks`](tape/src/bin/smalltalk_blocks.rs) | Shared mutable captures, ordinary block returns, lexical `^`, nested `ensure:` cleanup and an escaped return after its home has ended. Compare `--normal-return`; inspect the source-linked recording. | [Blocks and lexical homes](tape/blocks.md) |
| [`conditions`](tape/src/bin/conditions.rs) | Language-owned dynamic handlers, restart lookup, recovery, non-local exits, cleanup suspension and replacement, and expired capabilities above the VM's control primitives. Use `--help` for the scenarios. | [Conditions above the VM](tape/conditions.md) |
| [`message_dispatch`](tape/src/bin/message_dispatch.rs) | Embedding-controlled method lookup, shared cells and compatible method publication while an existing invocation keeps its original code. Inspect the actual waiting caller. | [Messages and method replacement](tape/messages.md) |
| [`live_language`](tape/src/bin/live_language.rs) | Follow an escaped block through independent methods, a waiting handler, live publication and original cleanup to its exact lexical home. `--snapshot` and `--explore` open the source-linked, read-only task view. | [Composed language control](tape/live-language.md) |

The block and condition examples use one program and invocation. The message-dispatch example
executes independently owned methods in separate invocations. Their guides explain those control
boundaries and what checked guest replay does and does not reproduce.
`live_language` connects independent invocations with an explicit language-owned task; its guide
explains how explicit stops preserve replayable child prefixes during a lexical exit.

## Tape: recording and profiling

| Example | What it is for |
| --- | --- |
| [`record_replay`](tape/src/bin/record_replay.rs) | Capture host boundaries, drop the live VM and host, then replay the guest with owned recorded data. Optional arguments control call count, aggregate depth and payload size. Its workload is also used by the recording benchmark. |
| [`footfall_capture`](tape/src/bin/footfall_capture.rs) | Capture branches, joins, guest calls, host work and failures with Footfall. Writes the exact tape, source sidecar, manifest, capture and Perfetto export to a new output directory. |
| [`tracy_capture`](tape/src/bin/tracy_capture.rs) | Connect VM and host scopes to Tracy, including exact program/source labels. Run with a Tracy client when collecting a live profile. |

The [Footfall adapter guide](../execution_tape_footfall/README.md) and
[Tracy adapter guide](../execution_tape_tracy/README.md) describe the backend contracts.
Profiling observations and replay recordings provide different evidence.

## Graph: incremental execution and application lifetimes

| Example | What it is for |
| --- | --- |
| [`tax`](graph/src/bin/tax.rs) | A first tape-backed graph: host dependency keys, invalidation, incremental reruns and explanations of why nodes ran. `--dot` emits the graph. |
| [`transforms`](graph/src/bin/transforms.rs) | Native execution with conditional parent-transform reads and dynamic dependencies, checked against a fresh reference evaluator. |
| [`procedural`](graph/src/bin/procedural.rs) | A mixed native/tape surface workflow: selective edits, stale results after failure, retained output lifetimes, selected VM recordings and optional whole-edit Footfall capture. |
| [`live_code`](graph/src/bin/live_code.rs) | Escaping callbacks sharing a host cell, compatible code publication, paused old code and object-root inspection after the graph is dropped. |
| [`deferred_assets`](graph/src/bin/deferred_assets.rs) | Deferred native loading with graph-owned pending tickets, selective execution, input edits during I/O, stale-completion rejection and retained buffer lifetimes. |

The [graph walkthroughs](graph/README.md) explain the larger workflows and their evidence.
Graph examples run headlessly by default. Enable `--features observatory` for their optional
terminal views; `procedural` uses `--features footfall` for whole-edit capture:

```sh
cargo run -p execution_graph_examples --features observatory --bin live_code -- --snapshot
cargo run -p execution_graph_examples --features footfall --bin procedural -- --edits 8 --capture .local/whole-edit
```

## Performance tools stay in the wind tunnels

Use the [tape wind tunnel](../execution_tape_wind_tunnel/README.md) for Criterion measurements,
`numeric_storage` and `value_layout`. Use the [graph wind tunnel](../execution_graph_wind_tunnel/README.md)
for Criterion measurements, `scaling` and `churn`. Those probes answer measurement questions;
the examples here demonstrate application and language semantics.

The block-parameter and recording benchmarks compile the same workload source kept under
[`tape/src/support/`](tape/src/support/), without depending on the example package's UI/profiling
libraries. The capture tests beside the tape examples exercise the exported artifacts and
Footfall boundaries, including a separate process with no backend installed.
