---
id: et-e5a1
status: open
deps: []
links: [et-991c]
created: 2026-01-31T14:47:47Z
type: task
priority: 2
assignee: Bruce Mitchener
---
# Perf: cache decoded bytecode for verified runs

Verified execution currently still pays bytecode decoding costs and various per-run setup costs.

Investigate caching decoded instruction streams (and any other reusable per-function metadata) so that repeated `run_verified*` executions avoid re-decoding.

Notes:
- Keep core design simple: avoid complicated global caches; prefer storing decoded streams in `VerifiedProgram` or in a VM-local cache keyed by `(program identity, func id)`.
- Preserve `no_std + alloc` core; avoid new deps.
- Add/extend wind-tunnel benchmarks to validate wins for call-heavy and loop-heavy workloads.

Acceptance:
- Benchmark(s) demonstrate improvement (ns/op) for at least one workload.
- No regressions in conformance tests.


## Notes

**2026-02-01T05:45:28Z**

Superseded by et-991c (VerifiedProgram caches decoded/validated instruction stream).
