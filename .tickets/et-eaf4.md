---
id: et-eaf4
status: open
deps: [et-4d1d]
links: [et-71ae]
created: 2026-02-01T05:44:32Z
type: task
priority: 2
assignee: Bruce Mitchener
parent: et-7452
tags: [perf, wind-tunnel, cleanup]
---
# PR7: Cleanup + wind-tunnel perf confirmation

Tighten the verified path, remove leftover dynamic-check scaffolding, and validate speedups in execution_tape_wind_tunnel.

## Design

- Update wind-tunnel benches to cover reg-classed interpreter and bytes/str handle behavior.
- Remove dead code paths left from `Value`-tag register model.
- Keep host return validation as trust boundary.

Branch naming: `et-tagless/pr7-cleanup-wind-tunnel`.

## Acceptance Criteria

- Wind-tunnel benches run and show no regressions on core numeric loops.
- `cargo clippy` stays clean; no new deps.
