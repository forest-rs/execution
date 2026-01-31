---
id: et-8a8d
status: closed
deps: []
links: []
created: 2026-01-31T01:32:59Z
type: task
priority: 1
assignee: Bruce Mitchener
parent: et-8de9
tags: [execution_tape, format]
---
# Define bytecode + serialization format

Define versioned program header/sections, canonical encoding, span table format, and host symbol/signature hashing.


## Notes

**2026-01-31T01:44:43Z**

Created `execution_tape` crate skeleton and initial draft container encoder/decoder (`Program` encode/decode + LEB128).

**2026-01-31T01:45:55Z**

`execution_tape` crate now builds clean: `cargo fmt`, `cargo clippy -D warnings`, `cargo test` all pass (roundtrip tests for varints and `Program` container).

**2026-01-31T01:57:13Z**

Extended `Program` container: const pool, function metadata (arg/ret/reg counts), and per-function span tables. Decoder now skips unknown section tags for forward compat; duplicate sections rejected. Tests updated; `cargo clippy -D warnings` and `cargo test` pass.

**2026-01-31T02:07:19Z**

Added Types section (`TypeTable` with struct/array element types) and split function storage into `function_table` + `bytecode_blobs` + `span_tables` (format `v0.2`, `VERSION_MINOR=2`; decoder still supports `v0.1`). Updated docs and tests; fmt/clippy/test pass.

**2026-01-31T02:10:41Z**

Closed: format/container now includes `symbols`/`const_pool`/`types`/`function_table`/`bytecode_blobs`/`span_tables`; no premature version migration logic; decode enforces required sections; negative tests added.
