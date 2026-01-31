---
id: et-8ab0
status: closed
deps: [et-8a8d]
links: []
created: 2026-01-31T02:03:20Z
type: task
priority: 3
assignee: Bruce Mitchener
parent: et-8de9
tags: [execution_tape, format]
---
# Split large const payloads into blob storage

Move `Bytes`/`Str` payloads out of the `Const` enum into a dedicated blob/string table (indices into separate storage) to keep `Const` compact and allow de-dup.


## Notes

**2026-01-31T02:16:52Z**

Related: `Program` now packs per-function bytecode + span entries into arenas via `ByteRange` (`Program::new`), but `Const::Bytes`/`Str` still store owned buffers; see `et-090a` for broader arena packing.

**2026-01-31T05:46:06Z**

Picking this up now: pack `Const` `Bytes`/`Str` payloads into a program-owned blob arena to keep the const pool compact in memory; keep on-disk encoding unchanged.

**2026-01-31T05:56:02Z**

Implemented const blob arena: `Program` now packs `Bytes`/`Str` const payloads into `Program::const_blob_data` and stores `ByteRange` in `ConstEntry`. On-disk `const_pool` encoding unchanged; VM/verifier updated accordingly.
