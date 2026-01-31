---
id: et-49cf
status: closed
deps: []
links: []
created: 2026-01-31T01:32:59Z
type: task
priority: 1
assignee: Bruce Mitchener
parent: et-8de9
tags: [execution_tape, docs]
---
# Write execution_tape v1 spec

Turn current design notes into a concrete spec: binary sections, opcode set, verifier rules, host ABI.


## Notes

**2026-01-31T01:37:00Z**

Draft spec written: `docs/v1_spec.md`

**2026-01-31T02:42:08Z**

Synced v1_spec.md with implemented container + verifier: PC is byte offset; sectioned container (symbols/const_pool/types/function_table/bytecode_blobs/span_tables); register convention (r0=eff, args in r1..); documented the minimal opcode encodings currently implemented by the decoder.

**2026-01-31T02:43:47Z**

Added 'Implemented verifier rules (v1, current)' subsection to spec (CFG target/boundary checks, reachability, reg bounds, init-before-use, call arity, entry init conventions).

**2026-01-31T02:44:49Z**

Spec synced to implementation (sections, PC/register conventions, opcode encoding, implemented verifier rules). Closing for now; reopen if we expand the opcode set/ABI.
