# Execution Tape Recording

Owned in-memory VM boundary recordings and checked replay, kept outside the core VM.
This crate needs only `no_std + alloc` and Execution Tape.

Share a prepared program in `Arc`, construct `Recorder::new(program.clone(), limits)`, and
pass `recorder.mask()` and `Some(&mut recorder)` to ordinary VM execution. Consume it with
`finish()` and inspect the resulting recording's `status()`. `replay()` executes the guest
again in a fresh VM, checks host arguments and the final values/trap, and returns the actual
replay result. It invokes no live host or resource-access sink.

Recorded values expose function/import identities, host arguments and raw outcomes, exact
float bits, decimal scales, and aggregate graphs. Nodes have recording-local IDs: shared
references stay shared and distinct equal nodes stay distinct. `aggregate(id)` borrows one
node's immediate contents. Equal string/byte payloads share owned contents. The recorder
retains only observed graph nodes and shares the immutable prepared program.

The recommended mask also captures guest activations through `CALL`, including exact caller
instruction locations. Each host observation and final outcome borrows an innermost-to-root
`CallStack`; suspended callers point at the instruction that entered their child. A parent-linked
arena shares ancestry across observations without copying a stack per call. It retains every
entered activation, including ones with no host calls, up to `RecordLimits::call_frames`.
`RecordStats::call_frames` counts this storage. Trap ancestry is pinned before VM unwinding.
Missing call callbacks, rejected entry and failed capture leave stack data explicitly unavailable.
Replay checks captured activation order, caller locations and boundary ancestry as well as values.

Replay restores observed initial nodes before execution. Guest-created nodes are matched
through boundary values and allocation ordinals relative to the initial heap. VM heaps append
immutable nodes until root commit; this lets replay resolve a host return even when that node
was never an entry argument. Final heap compaction does not change recording identities.

Capture limits bound value/node/host-call/activation counts and copied payload bytes. They do not measure
vector capacity, allocator overhead, the shared program or opaque custom-error payload sizes.
Exceeding a budget stops value/graph retention while live execution continues. The final VM
trap is still retained when its payload budget permits. Missing entry/completion,
invalid aggregate references, program mismatch and recorder reuse mark a capture incomplete.
The original invocation can still return or trap normally. Partial captures remain inspectable;
replay rejects them before running guest code.

These recordings are in-memory artifacts. Custom errors retain their typed owned payloads;
object/function handles retain their original domains. External object state, host effects and
resource accesses are not reconstructed. No portable archive, cross-platform float guarantee,
or per-instruction register history is claimed.

Run `cargo run -p execution_tape_wind_tunnel --bin record_replay` from the workspace for an
executable capture/drop/replay example. Optional arguments select host-call count, shared graph
depth and byte-payload size. `cargo bench -p execution_tape_wind_tunnel --bench recording`
measures ordinary execution, capture/freeze/drop and fresh replay/drop separately.

Licensed under either of Apache License, Version 2.0 or MIT license at your option.
