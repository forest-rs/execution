---
id: et-71ae
status: open
deps: []
links: [et-eaf4]
created: 2026-01-31T14:47:37Z
type: task
priority: 2
assignee: Bruce Mitchener
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
