---
id: et-bc6c
status: open
deps: []
links: []
created: 2026-01-31T08:36:29Z
type: task
priority: 2
assignee: Bruce Mitchener
---
# More float ops (no-std friendly)

Add additional `f64` ops beyond v1 minimal arithmetic/comparisons.

Candidates:
- unary: `f64_neg`, `f64_abs`
- min/max (decide NaN behavior: IEEE vs Rust `min/max` vs `total_cmp`-style)
- rounding: `f64_floor`, `f64_ceil`, `f64_round`, `f64_trunc` (note: `no_std` may require `libm` or host)
- remainder: `f64_rem`
- bit ops: `f64_to_bits` / `f64_from_bits` (if useful for plumbing)

Constraints:
- avoid adding deps unless necessary; prefer host calls for heavy math.
- specify NaN semantics explicitly.
