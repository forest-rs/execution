---
id: et-6da3
status: open
deps: [et-6e83]
links: []
created: 2026-02-01T05:44:17Z
type: task
priority: 2
assignee: Bruce Mitchener
parent: et-7452
tags: [host, abi]
---
# PR4: Host ABI uses slice-based value refs

Change Host::call to accept args as slice-based references (bytes as &[u8], str as &str) while keeping returns owned initially.

## Design

- Introduce `AbiValueRef<'a>` (or similar) for host argument passing.
- `Host::call` takes args: `&[AbiValueRef]` (bytes as `&[u8]`, strings as `&str`).
- Returns remain owned `Value`s initially; VM interns bytes/str into per-run arenas.
- Keep `no_std + alloc`; avoid new deps.

Branch naming: `et-tagless/pr4-host-slice-abi`.

## Acceptance Criteria

- No host call requires owned `Vec<u8>` / `String` for arguments.
- Existing host tests updated.
