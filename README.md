# `forest-rs/execution`

Workspace repository for the `execution_tape` bytecode container + VM and related crates.

Crates:
- `execution_tape/`: core `no_std + alloc` crate (format, verifier, VM, tracing hooks)
- `execution_tape_conformance/`: conformance/regression tests
- `execution_tape_wind_tunnel/`: benchmarks (Criterion)

Docs:
- `docs/overview.md`
- `docs/v1_spec.md`

Tickets:
- Tickets live in `.tickets/` at the repo root.
- Use `tk` from this repo (or from within a crate directory): `tk list`, `tk create ...`, etc.

Suggested checks:
```sh
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo bench -p execution_tape_wind_tunnel
```
