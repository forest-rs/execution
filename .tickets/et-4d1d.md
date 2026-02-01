---
id: et-4d1d
status: open
deps: [et-fe3c, et-766c]
links: []
created: 2026-02-01T05:44:28Z
type: task
priority: 1
assignee: Bruce Mitchener
parent: et-7452
tags: [vm, verifier, perf]
---
# PR6: Split frames by register class; verifier lowers regs; tagless hot loop

Split frame/register storage by class and lower verified instructions to class-local indices so the interpreter hot loop does monomorphic loads/stores with no Value tag checks.

## Design

- Define `RegClass` enum and per-frame storage (`i64` / `u64` / `f64` / `bool` / `decimal` / `agg` / `obj` / `func` / `bytes_handle` / `str_handle` / `effect`).
- Verifier lowers virtual reg indices → `(class, idx)` and rewrites verified instructions accordingly.
- Interpreter uses class-local arrays; remove match-on-`Value` in the hot loop.

Branch naming: `et-tagless/pr6-regclass-frames`.

## Acceptance Criteria

- Verified interpreter loop does not pattern-match on `Value` tags for register reads/writes.
- All typed ops become monomorphic loads/stores.
- Tests updated accordingly.
