---
id: et-bfe0
status: open
deps: []
links: []
created: 2026-01-31T06:06:36Z
type: task
priority: 2
assignee: Bruce Mitchener
parent: et-8de9
tags: [execution_tape, types, format, perf]
---
# Intern struct field names (avoid per-type String)

Struct field names are currently stored as UTF-8 bytes per type (and originate as `Vec<String>` in `StructTypeDef`). Consider interning field names as symbols (or using a shared string arena) so repeated names across structs don't duplicate storage and so callers can refer to fields by stable ids.

## Design

Motivation:
- Reduce memory and serialized size when many structs share common field names (e.g. x/y/z, position, color, etc.).
- Provide a stable identifier for field names that can be used by tooling/host APIs.

Options:
1) Reuse `Program` symbol table: add a separate `field_name_symbols` table or allow struct fields to reference `Program::symbols` via `SymbolId`.
2) Add a dedicated `FieldNameId` table in `TypeTable` (interned within types section), with encode/decode changes.
3) Keep current string-bytes storage but add optional dedup during `Program::new` packing.

Notes:
- If we change the on-disk encoding, include a migration note (not needed yet since v1 hasn't shipped).
- Prefer minimal dependencies; no hash maps unless we already have interning in builder.

## Acceptance Criteria

- Decide on representation + encoding.
- Implement interning/dedup and update verifier/VM/builder.
- Add a test showing two structs sharing a field name share a single interned entry (or at least deduped bytes).


## Notes

**2026-01-31T06:09:48Z**

Broadened scope: this is a general string interning/dedup policy ticket (not only struct field names). Candidates include: host symbols, struct field names, other schema/type names, and possibly const strings. Goal is to use stable ids + shared arenas to reduce duplication and improve tooling ergonomics. We'll keep it as a single umbrella ticket for now; split into child tickets if implementation gets large.
