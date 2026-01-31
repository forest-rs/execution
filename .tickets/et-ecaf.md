---
id: et-ecaf
status: closed
deps: [et-2ef3]
links: [et-3510]
created: 2026-01-31T05:45:59Z
type: task
priority: 2
assignee: Bruce Mitchener
parent: et-8de9
tags: [execution_tape, verifier, types, aggregates]
---
# Typed aggregates in verifier (v1)

Track aggregate shapes/types in the verifier so `tuple_get`/`struct_get`/`array_get` can produce concrete `ValueType`s (instead of always `Any`) when the aggregate's layout is known.

## Design

Current state: `ValueType::Agg` is coarse; verifier marks projection ops as returning `Any`. This is safe but loses useful typing for downstream ops and host calls.

Plan: extend verifier's per-register type state with optional aggregate metadata (tuple element types; struct `TypeId`; array `ElemTypeId`). Construction ops (`tuple_new`/`struct_new`/`array_new`) write `Agg` + metadata. Merge/join combines metadata conservatively: keep only if equal, otherwise drop to unknown. Projection ops use metadata (and program type table) to set `dst` `ValueType` to the known element/field type; if metadata unknown, yield `Any` (or possibly reject later).

No container format changes; verifier-only.

## Acceptance Criteria

- Verifier can type `tuple_get`/`struct_get`/`array_get` results as concrete `ValueType` when possible.
- Additional bounds checks: `tuple_get` index < arity, `struct_get` field_index < field_count.
- Tests cover typing behavior and bounds rejection.
- `cargo fmt`/`cargo clippy -D warnings`/`cargo test` pass.


## Notes

**2026-01-31T05:56:06Z**

Implemented typed aggregate tracking in verifier: track tuple element types, struct `TypeId`, array `ElemTypeId`; `tuple_get`/`struct_get`/`array_get` now produce concrete `ValueType` when metadata known; added bounds + kind mismatch checks and tests. fmt/clippy/test clean.
