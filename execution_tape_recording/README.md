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
is implied.

Run `cargo run -p execution_tape_wind_tunnel --bin record_replay` from the workspace for an
executable capture/drop/replay example. Optional arguments select host-call count, shared graph
depth and byte-payload size. `cargo bench -p execution_tape_wind_tunnel --bench recording`
measures ordinary execution, capture/freeze/drop and fresh replay/drop separately.

Licensed under either of Apache License, Version 2.0 or MIT license at your option.

## Optional execution explanations

Configure `Recorder::new(...).with_instructions(InstructionCapture { max_steps, watches })`
before running, and pass its recommended mask. Default recorders retain no instruction history
and request no register inspection. `instructions()` iterates actual attempts in execution order;
`instruction(index)` directly accesses one attempt, its actual successor and guest ancestry.
Typed block headers have no execution event. Calls complete at callee entry; returns resume the
suspended caller. Fuel exhaustion before fetching an instruction creates no attempted step.

`instruction_status()` is independent of boundary `status()`: a truncated instruction prefix
can coexist with complete boundary data and successful replay. Replay checks retained instruction
locations, opcodes, successors, outcomes and watched values as well as ordinary boundary evidence.
A missing callback or completion reports incomplete history; it never invents successful execution.

Each `RegisterWatch` selects one source location and virtual registers. Inputs are borrowed
before execution; successful local writes are borrowed afterward. Call dispatch and guest return
have no output-register event. Unwatched values and non-operands remain unknown. Before/after
aggregate observations preserve their separate identities, even when a destination aliases an
input. The VM builds optional source-to-bank maps only for requested functions, for that run;
no observation maps are retained in ordinary prepared programs. Watched values use the existing
value/aggregate/payload budgets; exhausting one still makes boundary capture incomplete.

Trace forwarding wrappers must forward `TraceSink::run_mask` as well as instruction/value
callbacks. The VM reports the actual nonempty subscription before event delivery, independently
of `RUN`; `NONE` still emits no callbacks. History uses that evidence to distinguish zero attempts
from disabled callbacks. A recommended mask alone does not establish which events were enabled.
