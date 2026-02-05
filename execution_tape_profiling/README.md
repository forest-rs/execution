# execution_tape_profiling

`execution_tape_profiling` provides profiling adapters for `execution_tape` and keeps the core
crate free of profiling dependencies. It is intended for `std` environments.
Currently the only backend is Tracy via [`tracy-client`](https://docs.rs/tracy-client).

## Usage

Then pass the sink into `Vm::run`:

```rust,ignore
let mut sink = execution_tape_profiling::ProfilingTraceSink::new();
let mask = sink.mask();
vm.run(&program, entry, &[], mask, Some(&mut sink))?;
```

## Tracy example

Run the bundled Tracy example:

```bash
cargo run -p execution_tape_profiling --example tracy_simple
```

## Labels

By default the sink uses stable id-based labels. Provide a custom resolver via
`ProfilingTraceSink::with_resolver` to supply human-readable names.
