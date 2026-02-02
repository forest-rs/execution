---
id: et-9dae
status: closed
deps: []
links: []
created: 2026-02-02T01:44:37Z
type: task
priority: 2
assignee: Bruce Mitchener
tags: [bytecode, asm, verifier, cleanup]
---
# Centralize opcode bytes via Opcode enum

Replace scattered literal opcode bytes with a single enum mapping.

## Design

### Goal

Have a single authoritative mapping between op names and opcode bytes. Avoid scattered magic
numbers across `asm`, bytecode decoding, and docs.

### Proposed Approach

Introduce `execution_tape/src/opcode.rs` containing:

- `#[repr(u8)] pub(crate) enum Opcode { ... }`
- `impl Opcode { pub(crate) fn from_u8(b: u8) -> Option<Self> }`
- `impl Opcode { pub(crate) fn is_terminator(self) -> bool }` (optional convenience)
- `impl Opcode { pub(crate) fn name(self) -> &'static str }` (optional convenience)

Then update:

- `execution_tape/src/asm.rs`: emit opcodes via `Opcode::X as u8` instead of literals.
- `execution_tape/src/bytecode.rs`: decode `Opcode` first; `UnknownOpcode { opcode: u8 }` remains,
  but decoding matches on `Opcode` variants (not numeric literals).
- `docs/v1_spec.md`: add a short note that `Opcode` is the authoritative list. Keep the existing
  table for now, but treat it as derived/documentation.
- Add a small regression test asserting a few key opcode values (e.g. `Call == 0x50`, `Ret == 0x51`,
  `BoolNot == 0x30`, `BoolAnd == 0x88`).

### Non-goals

- No refactor to “struct-per-op” instruction types in this ticket.
- No format version bump.
- No semantic behavior changes besides centralizing constants.

### Risks

- Easy to miss a call site and leave a literal behind; mitigate with `rg` checks for `0x..` literals
  in `execution_tape/src/{asm,bytecode,typed,verifier}.rs`.
- Keep the enum crate-internal for now; avoid expanding public API surface until the format
  stabilizes.

### Validation

- `cargo fmt --all`
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- `cargo test --workspace --all-features`

## Acceptance Criteria

- New `Opcode` enum exists and covers all current opcodes.
- `execution_tape/src/asm.rs` and `execution_tape/src/bytecode.rs` no longer use literal opcode
  bytes for known ops.
- A regression test asserts a few key opcode values.
- `docs/v1_spec.md` references `Opcode` as authoritative (minimal doc churn).
- `cargo fmt`, `cargo clippy -D warnings`, `cargo test` pass.
