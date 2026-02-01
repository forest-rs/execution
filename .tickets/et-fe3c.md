---
id: et-fe3c
status: open
deps: [et-991c]
links: []
created: 2026-02-01T05:44:10Z
type: task
priority: 1
assignee: Bruce Mitchener
parent: et-7452
tags: [verifier, types]
---
# PR3: Verifier enforces stable register types; forbid Any

Strengthen verification: each virtual register must have a single concrete ValueType across all reachable paths; ValueType::Any is rejected for verified programs.

## Design

- Add verifier rule: per-reg type is stable (no join to `Any`).
- Forbid `ValueType::Any` in function signatures and in any inferred reg types for verified programs.
- Produce a per-function reg → `RegClass` mapping for later lowering.

Branch naming: `et-tagless/pr3-stable-reg-types`.

## Acceptance Criteria

- `VerifiedProgram` construction fails on `Any` or unstable reg typing.
- New targeted tests for rejection cases.
