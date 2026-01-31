---
id: et-2df6
status: open
deps: []
links: []
created: 2026-01-31T07:20:30Z
type: task
priority: 2
assignee: Bruce Mitchener
---
# Expand decimal behavior (rounding/div/conversions)

We currently support only dec_add/dec_sub/dec_mul with strict scale rules and trap on overflow/scale mismatch. Pricing work will need rounding policies, division, and explicit conversions (i64/u64 <-> Decimal, scale changes). This ticket tracks the design and staged implementation of richer decimal semantics without bloating core VM.

## Design

Open questions:
- Rounding modes: which set (bankers/half-up/ceil/floor/trunc)? Per-op vs per-context?
- Division: trap vs produce host error? result scale selection rules?
- Scale normalization: do we allow rescale ops (e.g. dec_rescale(dst, a, target_scale, rounding_mode))?
- Overflow policy: keep trapping? add checked vs wrapping variants?
- Conversions: dec_from_i64(scale), dec_to_i64(rounding), etc.
- Equality/ordering: compare decimals with differing scales? normalize?

## Acceptance Criteria

- Spec sections added for chosen decimal ops + rounding policy.
- VM implements agreed-upon minimal set (likely: dec_cmp + dec_rescale + dec_div or host-provided division).
- Verifier enforces operand/result types for new ops.
- Conformance tests cover scale mismatch, overflow, rounding edge cases.


## Notes

**2026-01-31T07:20:35Z**

Context: pricing uses i64 + decimal; v1 started with per-value scale and traps on scale mismatch for add/sub. This ticket is intentionally broader than just field rounding; it should decide core vs host responsibilities.
