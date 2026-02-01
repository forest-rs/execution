---
id: et-ff91
status: closed
deps: [et-24fc]
links: []
created: 2026-02-01T17:23:57Z
type: task
priority: 2
assignee: Bruce Mitchener
parent: et-7452
tags: [verifier, cleanup]
---
# PR9: Remove ValueType::Any; verifier uses RegType::{Uninit,Concrete,Ambiguous}

## Design

Replace internal verifier type-propagation sentinel uses of `ValueType::Any` with an explicit internal enum (`RegType::{Uninit, Concrete(ValueType), Ambiguous}`), and remove `ValueType::Any` from the public format entirely.

## Acceptance Criteria

- `ValueType::Any` is removed from `ValueType` and from the on-disk encoding (tag `11` is no longer accepted by the decoder).
- Verifier internals use `RegType::{Uninit, Concrete(ValueType), Ambiguous}` (no `ValueType::Any` sentinel).
- `cargo fmt`, `cargo clippy -D warnings`, `cargo test` pass.

## Notes

**2026-02-01T17:30:03Z**

Replaced internal verifier type sentinel (ValueType::Any) with internal RegType::{Concrete,Ambiguous}. No behavior change expected; all tests/clippy pass.

**2026-02-01T17:46:35Z**

Amended to remove `ValueType::Any` from the public format (decoder no longer accepts tag `11`) and to use `RegType::Uninit` explicitly for definite-uninit state.
