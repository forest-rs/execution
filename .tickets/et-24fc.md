---
id: et-24fc
status: closed
deps: [et-eaf4]
links: []
created: 2026-02-01T16:08:08Z
type: task
priority: 2
assignee: Bruce Mitchener
parent: et-7452
tags: [perf, vm, dispatch]
---
# PR8: Frame instruction index (no per-step pc lookup)

## Design

Store an instruction index in each call frame and advance it linearly in the hot loop. Only map byte-offset PCs to indices on control-flow (br/jmp) to avoid binary-search per instruction.

## Acceptance Criteria

- VM does not call `VerifiedFunction::fetch_at_pc` in the hot loop.
- Control-flow still uses byte-offset PCs externally (for tracing/traps).
- Wind-tunnel benches show improved small straight-line programs (e.g. `i64_add_chain/10`).
- `cargo fmt`, `cargo clippy -D warnings`, `cargo test` pass.

## Notes

**2026-02-01T16:26:56Z**

Implemented frame instruction index (no per-step pc->instr search). VM advances instr_ix linearly and only binary-searches on br/jmp. Wind-tunnel: vs PR7, i64_add_chain/10 ~275.8ns -> ~102.7ns (~-62.8%); call_loop/1000 ~205.4µs -> ~107.3µs (~-47.7%). Raw: /tmp/bench-pr7.txt, /tmp/bench-pr8.txt.
