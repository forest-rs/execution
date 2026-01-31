---
id: et-fa1d
status: open
deps: []
links: []
created: 2026-01-31T08:36:35Z
type: task
priority: 2
assignee: Bruce Mitchener
---
# More int ops (unary/minmax/bit twiddling)

Add additional integer ops beyond v1 minimal arithmetic/bitwise/shifts/div/rem.

Candidates:
- unary: `i64_neg`, `i64_abs` (must define overflow: likely trap on `i64::MIN` for abs/neg?)
- min/max/clamp
- bit twiddling: `u64_clz`, `u64_ctz`, `u64_popcnt`, rotates

Open questions:
- do we want trap-on-overflow variants for unary ops, or always wrapping?
