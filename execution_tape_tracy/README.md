# execution_tape_tracy

`execution_tape_tracy` translates VM call/host scopes into Tracy spans, keeping the core
crate free of profiling dependencies. It requires `std` and Rust 1.88.
Version 0.1.0 uses `execution_tape` 0.1.0 and
[`tracy-client`](https://docs.rs/tracy-client) 0.19.0. Applications start the Tracy client.
This crate was previously named `execution_tape_profiling` in the unpublished workspace;
update dependency/import paths to `execution_tape_tracy`.

## Usage

Then pass the sink into `Vm::run`:

```rust,no_run
# use execution_tape::{host::Host, trace::TraceSink, value::FuncId, verifier::VerifiedProgram, vm::{TrapInfo, Vm}};
# fn profile<H: Host>(vm: &mut Vm<H>, program: &VerifiedProgram, entry: FuncId) -> Result<(), TrapInfo> {
let mut sink = execution_tape_tracy::ProfilingTraceSink::new();
let mask = sink.mask();
vm.run(program, entry, &[], mask, Some(&mut sink))?;
# Ok(()) }
```

## Tracy example

Run the development executable from the workspace:

```bash
cargo run -p execution_tape_wind_tunnel --bin tracy_capture
```

## Labels

By default the sink uses stable id-based labels. Provide a custom resolver via
`ProfilingTraceSink::with_resolver` to supply human-readable names.

## Authored source locations

Attach the frontend's core source map before execution:

```rust,no_run
# use execution_tape::{program::Program, source::{SourceMap, SourceError}};
# fn profile<'a>(program: &'a Program, sources: &'a SourceMap) -> Result<execution_tape_tracy::ProfilingTraceSink<'a>, SourceError> {
let sink = execution_tape_tracy::ProfilingTraceSink::new().sources(program, sources)?;
# Ok(sink) }
```

Binding checks the complete program identity once. Captured guest/host boundaries resolve their
own function/PC to a supplied document label and one-based authored line. The same mapping is
queryable with `source_location()` without a live Tracy client. Snapshots are not sent as source
text or opened as files; document labels need not be paths, and a local file opened by a viewer
may differ from the supplied snapshot.

The binding applies to the exact borrowed program instance. Missing mappings, an unrepresentable
Tracy line number, or reuse with another instance fall back to `execution_tape.bytecode` and its
bytecode PC. Bind again for a different instance, even if its encoded contents match. Source
lookup happens only when a Tracy client is running; ordinary VM execution is unchanged.
Active Tracy guards close inner-to-outer when the sink is dropped, including after a host panic.
After catching a host panic, drop and recreate the sink before another run.
