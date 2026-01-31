---
id: et-cb4b
status: open
deps: []
links: []
created: 2026-01-31T13:32:57Z
type: feature
priority: 2
assignee: Bruce Mitchener
---
# Add profiling adapter via scope trace events

Now that `TraceEvent::{ScopeEnter, ScopeExit}` and `ScopeKind::{CallFrame, HostCall}` exist, add a small `std`-only adapter crate (e.g. `execution_tape_profiling`) that implements `TraceSink` and emits scopes via the ecosystem `profiling` crate.

Goals:
- No new deps in `execution_tape` (core stays `no_std + alloc`).
- Adapter should be zero-cost when not used.
- Scope labeling: start with stable ids (`FuncId`, `HostSigId`/`SymbolId`/`SigHash`), with an optional embedder hook to map ids to human names.

Open questions:
- Do we want to add optional function names to the `Program`/container format, or keep naming entirely embedder-provided?
- Should host scopes use the resolved symbol string (requires `&Program`) or stick to ids only?

