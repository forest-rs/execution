---
id: et-0c7c
status: open
deps: []
links: []
created: 2026-01-31T08:36:41Z
type: task
priority: 2
assignee: Bruce Mitchener
---
# Bytes/string ops backlog

Track additional `bytes` / `str` ops beyond current v1 set.

Already implemented:
- `bytes_len`, `bytes_eq`, `bytes_concat`, `bytes_get`, `bytes_get_imm`, `bytes_slice`, `bytes_to_str`
- `str_len`, `str_eq`, `str_concat`, `str_slice`, `str_to_bytes`

Candidates:
- search: `bytes_find`, `str_find` (define return type: `u64` + sentinel? or Option-like?)
- predicates: `str_starts_with`, `str_ends_with`, `bytes_starts_with`, `bytes_ends_with`
- casing/normalization (likely host-only)
- hashing (probably host)

Open questions:
- whether to add non-trapping "checked" variants that return `Any`/`Unit`/bool instead of trapping on OOB/invalid UTF-8.
