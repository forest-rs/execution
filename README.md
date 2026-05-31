# `forest-rs/execution`

Workspace repository for `execution_tape`, a small verifiable bytecode container and VM, and
`execution_graph`, an incremental execution graph built on top of it.

Crates:
- `execution_tape/`: publishable `no_std + alloc` crate for the bytecode format, verifier, VM,
  tracing hooks, host ABI, and disassembler.
- `execution_graph/`: publishable `no_std + alloc` crate for dirty-tracked incremental execution
  of verified tape programs.
- `execution_graph_examples/`: runnable graph examples, including the `tax` demo.
- `execution_tape_conformance/`: conformance/regression tests for the tape format, verifier, and VM.
- `execution_tape_profiling/`: optional profiling adapters; kept separate from the core crate.
- `execution_tape_wind_tunnel/` and `execution_graph_wind_tunnel/`: Criterion benchmarks.

Docs:
- `docs/overview.md`: design notes for the tape VM and host boundary.
- `docs/v1_spec.md`: current v1 bytecode/container draft.
- `execution_tape/README.md`: crate overview and quick start.
- `execution_graph/README.md`: graph model, dependency keys, and demo.

The workspace MSRV is Rust 1.88.

Tickets:
- Tickets live in `.tickets/` at the repo root.
- Use `tk` from this repo (or from within a crate directory): `tk list`, `tk create ...`, etc.

Suggested checks:
```sh
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo test --doc --workspace --all-features
cargo package -p execution_tape
cargo package -p execution_graph
cargo bench -p execution_tape_wind_tunnel
cargo bench -p execution_graph_wind_tunnel
```
