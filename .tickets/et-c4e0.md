---
id: et-c4e0
status: open
deps: []
links: []
created: 2026-02-04T07:13:39Z
type: task
priority: 1
assignee: Bruce Mitchener
tags: [jit, cranelift, vm, perf, aarch64]
---
# CLIF JIT (aarch64): run VerifiedProgram via Cranelift

## Design

Prototype a Cranelift-based JIT backend for `execution_tape` to evaluate performance and
architecture implications. This is explicitly a “prove it out” effort: correctness first,
then measure.

### Fence

This JIT owns native-code generation and execution for **verified** bytecode; it explicitly does
not own:
- the bytecode format
- the verifier rules
- the portable interpreter

### Constraints / decisions

- Target architecture: **aarch64** (primary dev machine).
- No tracing support in the first slice (“no-trace JIT first”).
- JIT lives in a **new workspace crate** to keep `execution_tape` core `no_std + alloc` friendly.
- Large deps are acceptable in that new crate (`cranelift-*`).

### Proposed crate shape

- New crate: `execution_tape_jit_clif` (std-only).
- Public entrypoint: `JitEngine::compile(&VerifiedProgram) -> Result<JittedProgram, JitError>`.
- Execution: `JittedProgram::run(entry, args, host, limits) -> Result<RunResult, Trap>`.
- The interpreter remains the reference behavior for traps and edge cases.

### Codegen model (first slice)

- Inputs: `VerifiedProgram` only (trust boundary is verification).
- Per-function compilation:
  - Build CFG blocks from verifier metadata (or from verified decoded stream).
  - Translate bytecode ops to CLIF IR.
  - Registers map to per-regclass locals/SSA values (tagless).
  - Branch targets become CLIF block jumps.
- Host calls:
  - Initially via a Rust/ABI trampoline that the JIT calls with `(symbol, sig_hash, args, rets, host_ctx)`.
  - Keep bytes/str as slices/handles consistent with current Host ABI.

### Limits (v1)

- Fuel/loop limit: either omit initially (for the first perf smoke test) or add a cheap periodic
  check at block headers. Prefer correctness parity with interpreter once basic codegen works.
- Call depth: reuse existing VM limits or implement a counter in the trampoline.

### Rollout plan

1) Create `execution_tape_jit_clif` crate + minimal `JitEngine` skeleton; no integration yet.
2) Compile/run tiny subset: `const_*`, integer arithmetic (`i64_add` etc), `br/jmp`, `ret`.
3) Add host_call trampoline and run a small conformance subset under JIT.
4) Add fuel checking parity.
5) Benchmark against interpreter (wind-tunnel) and decide whether to expand opcode coverage.

## Acceptance

- New crate `execution_tape_jit_clif` exists in the workspace and builds on **aarch64**.
- JIT compilation accepts `VerifiedProgram` and rejects unverified programs by API shape.
- JIT can execute at least one end-to-end verified program (const + arithmetic + branch + ret)
  with matching result vs interpreter.
- Host calls work for at least one trivial host function via a trampoline (no tracing).
- A minimal benchmark compares interpreter vs JIT for at least one loop-heavy workload.
- Workspace stays green: `cargo fmt`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`,
  `cargo test --workspace --all-features`.
