---
id: et-db69
status: open
deps: []
links: []
created: 2026-01-31T06:12:52Z
type: task
priority: 2
assignee: Bruce Mitchener
parent: et-8de9
tags: [execution_tape, format, debug, tooling]
---
# Function naming strategy (debug vs stable ids)

Decide whether and how functions should be named (debug labels vs stable identifiers), and where that metadata should live (sidecar vs optional container section).

## Design

Open questions:
- Are function names primarily for debugging/tracing/UI, or are they stable identifiers that embedder code will rely on?
- Should names be unique, or can multiple FuncIds share a name (overloads/versions)?
- Are names required at runtime (VM errors, host introspection), or tooling-only?
- If serialized: should we add an optional container section (e.g. function_names: FuncId -> SymbolId/ByteRange) or keep it external?
- How does this interact with SpanId stability (SpanId may already point back to a graph node id)?

Recommendation (tentative): start tooling-only (sidecar or graph-level metadata) and add an optional container section later if it proves valuable.

## Acceptance Criteria

- Written decision: where names live + intended semantics.
- If in-container: specify encoding + update spec + add decode/encode + tests.
- If external: specify expected mapping shape and stability requirements.

