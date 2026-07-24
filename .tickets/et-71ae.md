---
id: et-71ae
status: closed
deps: []
links: [et-eaf4]
created: 2026-01-31T14:47:37Z
type: task
priority: 2
assignee: no-materials
---
# Wind tunnel: add loop + allocation benchmarks

We have an initial Criterion-based wind-tunnel crate (`execution_tape_wind_tunnel`) with a few core scenarios.

Add additional benchmarks that better match real workloads and reduce measurement bias:
- “Many ops per run” variants (e.g. 1_000 host calls in one run; 1_000 calls in one run) to avoid over-measuring per-run setup.
- Allocation/heap pressure scenarios (bytes/str concat, aggregate construction/access) to surface allocation regressions.
- Optional parameterization for sizes (N, payload sizes) to get scaling curves.

Acceptance:
- New benches compile and run under `cargo bench -p execution_tape_wind_tunnel --bench vm`.
- Each bench has a short comment describing what it measures.


## Notes

**2026-02-01T05:45:28Z**

Bench updates likely land under et-eaf4 (PR7: Cleanup + wind-tunnel perf confirmation).

**2026-02-05T10:55:07Z**

Implemented new wind-tunnel benches (bytes/str concat chains + array_alloc_loop) and expanded loop iterations to include 1_000. Ran cargo bench -p execution_tape_wind_tunnel --bench vm successfully.
