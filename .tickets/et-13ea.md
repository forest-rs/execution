---
id: et-13ea
status: closed
deps: [et-766c]
links: []
created: 2026-02-01T13:43:59Z
type: task
priority: 2
assignee: Bruce Mitchener
parent: et-7452
tags: [vm, regfile, tagless, verifier]
---
# PR6: Split register file by RegClass (tagless VM)

Goal: move VM execution to a monomorphic, per-`RegClass` register file so the hot interpreter loop
does not consult runtime tags.

## Design

- Verifier produces a per-function `RegLayout`:
  - each virtual register `rN` maps to exactly one `RegClass` and a class-local index
  - registers must have a stable class across all writes/paths (reject merges that produce `Any`)
- Verifier produces a typed instruction stream (`VerifiedInstr`) with typed register operands
  (e.g. `I64Add { dst: I64Reg, a: I64Reg, b: I64Reg }`), so the VM does not need per-op mapping.
- VM register storage is split by class (SoA):
  - `Vec<i64>`, `Vec<u64>`, `Vec<f64>`, `Vec<Decimal>`, `Vec<BytesHandle>`, `Vec<StrHandle>`, etc.
  - `Unit` registers use `u32` storage (always `0`) for now.
- Remove `RegValue` entirely once the tagless execution path lands.

Branch naming: `et-tagless/pr6-split-regfile`.

## Acceptance Criteria

- Hot interpreter loop does not pattern-match on a `Value`/`RegValue` tag to load/store operands.
- Verification rejects programs where a register does not have a stable `RegClass`.
- `cargo fmt --all`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`,
  `cargo test --workspace --all-features` all pass.

## Blockers

- et-766c [done] PR5: Per-run arenas + bytes/str handles in registers
