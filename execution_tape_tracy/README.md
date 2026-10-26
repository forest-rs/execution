# execution_tape_tracy

`execution_tape_tracy` translates VM call/host scopes into Tracy spans, keeping the core
crate free of profiling dependencies. It requires `std` and Rust 1.88.
The first release is being prepared as 0.1.0 alongside `execution_tape` 0.1.0, using
[`tracy-client`](https://docs.rs/tracy-client) 0.19.0. Applications start the Tracy client.
This crate was previously named `execution_tape_profiling` in the unpublished workspace;
update dependency/import paths to `execution_tape_tracy`.

## Usage

Then pass the sink into `Vm::run`:

```rust,ignore
let mut sink = execution_tape_tracy::ProfilingTraceSink::new();
let mask = sink.mask();
vm.run(&program, entry, &[], mask, Some(&mut sink))?;
```

## Tracy example

Run the development executable from the workspace:

```bash
cargo run -p execution_tape_wind_tunnel --bin tracy_capture
```

## Labels

By default the sink uses stable id-based labels. Provide a custom resolver via
`ProfilingTraceSink::with_resolver` to supply human-readable names.
