# Execution Tape Footfall

`FootfallSink` implements the VM's existing `TraceSink`, keeping Footfall out of the core VM.
The adapter requires only `no_std + alloc`; applications choose their capture/export backend.
The workspace uses published Footfall 0.2 with default features disabled.

`execution_tape_footfall` 0.1.0 uses `execution_tape` 0.1.0. Its MSRV is Rust 1.88.
It requires only Footfall's `alloc` support;
applications enable Footfall's runtime/export features when selecting those backends.

Install the backend, register an application context, then create
`FootfallSink::new(context.handle())`. Pass `sink.mask()` to `Vm::run` for run, call and host
scopes when Footfall's current level captures scopes. The recommendation is `TraceMask::NONE`
at `Off` or `Counters`, including before a backend is installed. Recompute `sink.mask()` before
each run if the level can change. A level change cannot enable callbacks omitted from the VM's
already-copied mask; pass an explicit mask when capture may start during execution.
Use `TraceMask::RUN` explicitly for coarse invocation timing. Add `TraceMask::INSTR` only
when investigating individual instructions. Keep the context registered until all callbacks
and active scopes finish. Reuse a sink for sequential runs;
it stays on one thread. Dropping it closes remaining scopes from inner to outer, without
creating completion events for an interrupted run.
After catching a host panic, drop and recreate the sink before another run, regardless of mask.

Context labels must identify the program and revision when combining programs. Function,
import and PC identifiers are program-local. Keep the corresponding encoded program with
the capture to resolve names, spans, typed joins and instruction operands later.

For authored source, export the frontend's `execution_tape::source::SourceMap` sidecar beside
that exact container and reference both in the context-to-program manifest. After capture,
decode the map, bind it to the saved program, then look up `TraceLocation { func, pc }` from
an observation. `text()`, `start()` and `end()` expose exact snapshot ranges and named byte
coordinates without terminal dependencies. Missing mappings remain unknown; opaque span IDs
are not authored line numbers. Source snapshots are an explicit export chosen by the application.
The adapter's event schema and per-event costs do not change when a source sidecar is exported.

The static event schema uses at most four inline fields:

| Observation | Numeric fields |
| --- | --- |
| `execution_tape.run` scope | `entry`, `arg_count` |
| `execution_tape.call` scope | `func`, `pc`, `depth` |
| `execution_tape.host` scope | `func`, `pc`, `depth`, `host_sig` |
| `execution_tape.instr` message | `func`, `pc`, `opcode`, optional `span_id` |
| `execution_tape.instr_end` message | `func`, `pc`, optional `next_func` and `next_pc` |
| `execution_tape.instr_trap` message | `func`, `pc` |
| `execution_tape.scope_end` / `scope_trap` message | `func`, `pc`, `depth`, `scope_token` |
| `execution_tape.run_end` / `run_trap` message | `scope_token`, trap `func` and `pc` |

End/trap descriptors distinguish success from failure without parsing text. Absent spans
and successors are omitted. Completion messages reference the Footfall scope token where
one exists. Trap messages carry a static outcome marker; full diagnostics and typed host errors
remain in the ordinary VM result, without copying arbitrary error payloads into profiling events.

Run the capture example:

```sh
cargo run -p execution_tape_examples --bin footfall_capture -- .local/footfall-demo
```

It captures branches, a typed join, a guest call, host work, and guest/host failures. The output
contains the exact program, a context-to-program manifest with `program.sources`, the original
Footfall capture and a Perfetto trace. The example's explicit source mappings point to the Rust
statements constructing its tape; they are not inferred from its span IDs. Host instrumentation
shares the context and capture with VM events. An independent artifact test resolves every
recorded instruction against the saved source snapshot after the live VM has exited.

Check Footfall's drop counts and event window before trusting an incomplete capture.
Instruction tracing is deliberately opt-in and can produce substantial traffic. Explicit VM
tracing while Footfall is disabled still incurs callbacks and can allocate scope bookkeeping.
These observations contain no register values or complete host inputs/results and are **not a
replay record**.

Licensed under either of Apache License, Version 2.0 or MIT license at your option.

Resumable VM invocations keep logical scopes open across scheduling quanta, so
these scopes include suspended time. Cancellation closes scopes with a
`cancelled` outcome message and does not imply a guest trap or undo host effects.
