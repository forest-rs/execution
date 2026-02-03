---
id: et-2ef3
status: closed
deps: [et-8a8d]
links: []
created: 2026-01-31T01:32:59Z
type: task
priority: 1
assignee: Bruce Mitchener
parent: et-8de9
tags: [execution_tape, verifier]
---
# Implement verifier

Verifier for control-flow integrity, register init-before-use, type discipline, bounds checks, and resource limits.


## Notes

**2026-01-31T02:21:20Z**

Added initial verifier module: execution_tape/src/verifier.rs with VerifyConfig/VerifyError and container-level checks (bytecode/span ranges, span delta sanity, reg_count limit) + tests. fmt/clippy/test pass.

**2026-01-31T02:33:54Z**

Added draft bytecode decoder (internal) and verifier bytecode checks: CFG jump-target validation (byte-offset PC), reachability, and must-init init-before-use analysis for a minimal opcode subset. Added tests for invalid jump, uninitialized read, and call arity mismatch. fmt/clippy/test pass.

**2026-01-31T04:48:02Z**

**Overview**
- Goal: reject malformed/unsafe bytecode *before* execution (CFG integrity, init-before-use, typed discipline, bounds checks).
- Non-goals (for the initial slices): full dataflow optimizations, sophisticated type unions, effect rollback/cancellation.

**Concepts + glossary**
- CFG integrity: `br`/`jmp` targets must be in-bounds and land on instruction boundaries.
- Must-init: a register is “initialized” only if written on all paths before a read.
- Type lattice: concrete `ValueType` plus `Any` as merge-top.
- `HostSigId`: host calls validated against program-carried signature tables.

**Why this shape**
- CFG target validation prevents jumping into the middle of an instruction (which would make decoding/exec ambiguous).
- Must-init analysis catches a large class of runtime failures early and enables predictable execution semantics.
- Table-driven typing (function/host sig tables + `TypeTable`) keeps core opcodes minimal and lets the embedder evolve types without hardcoding everything into the verifier.

**Usage example (what it enables)**
- A builder can emit recursive or branching bytecode freely; verifier ensures:
  - every register read is definitely initialized,
  - every `call` matches the callee’s signature,
  - every `host_call` matches the referenced `HostSigId`.

**Extension points**
- Add per-opcode typing rules for numeric/aggregate ops as they land.
- Add limits (max bytecode size, max blocks, max host sigs) to make verification cost predictable.

**Gotchas / risks**
- `Any` can propagate widely through merges; design ops so they fail fast when they need concrete types.
- Keep verifier linear-ish in program size; avoid per-instr allocations where possible.

**2026-01-31T05:07:12Z**

Added verifier rules for i64_add and tuple/struct/array aggregate ops; added negative tests for type/arity mismatches.

**2026-01-31T05:56:11Z**

Added typed aggregate metadata to type analysis so tuple_get/struct_get/array_get can yield concrete ValueTypes; added verifier errors for aggregate kind mismatch and immediate projection OOB.

**2026-01-31T06:51:00Z**

Fixed a verifier false-negative on loops: type analysis now uses an explicit 'unknown/top' state during fixpoint iteration, so loop-invariant register types initialized in a prologue are preserved across back-edges (conformance test vm_loop_sum_0_to_n_minus_1).

**2026-02-03T08:13:50Z**

Reviewed verifier implementation. Control-flow integrity (boundary computation + CFG + jump validation + terminators), init-before-use (must-init + read/write validation), type discipline (type lattice + transfer/validate + host sig validation), bounds checks (regs/consts/host sigs/type ids/aggregate indices/spans), and resource limit (VerifyConfig::max_regs_per_function) are all implemented in execution_tape/src/verifier.rs. Ticket scope appears complete; closing.
