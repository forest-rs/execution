---
id: et-aa08
status: open
deps: []
links: []
created: 2026-01-31T08:36:22Z
type: task
priority: 2
assignee: Bruce Mitchener
---
# Decimal ops backlog (beyond v1 minimal)

Track remaining Decimal-related ops and semantics beyond current minimal set.

Potential ops:
- `dec_div` (rounding mode? scale rules? trap vs saturate?)
- comparisons: `dec_eq`, `dec_lt`, `dec_gt`, `dec_le`, `dec_ge`
- unary: `dec_neg`, `dec_abs`
- min/max/clamp
- rescale/quantize: `dec_rescale` / `dec_round` / `dec_trunc` (needs rounding policy)
- conversions: `i64_to_dec_round`, `u64_to_dec_round`, `dec_to_i64_round`, etc.

Open questions:
- how scale is chosen/propagated for div
- how rounding modes are represented (immediate? value? global policy?)
- overflow behavior vs trap (v1 generally traps)

Related: `et-2df6` (expand decimal behavior).
