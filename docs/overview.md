# `execution_tape` (v1) — design notes

See also: [`v1_spec.md`](v1_spec.md).

## Fence
`execution_tape` owns how an already-lowered program runs: IR/bytecode shape, verification, execution (including limits), tracing hooks, and a narrow host-call ABI.

It explicitly does **not** own node-graph authoring formats/UI, domain-specific node definitions, or host object semantics beyond an opaque handle contract.

## Goals
- Fast execution for mostly-static programs (hundreds of nodes/opcodes).
- Portable, versioned bytecode serialization.
- Verifiable programs (fail fast with good diagnostics).
- `no_std + alloc` core by default; optional `std` feature.
- Tracing/profiling first; interactive debugging later (including planned call-frame and host-call
  scopes for external profilers).

## Non-goals (v1)
- Incremental recompute / dirty propagation (design should not preclude it).
- Closures / captured environments (function refs only).
- Multi-lane effects (single linear effect token only).
- Recoverable in-VM errors (`Result`/`Option`): v1 is trap-first.

## Glossary
- **`Program`**: serialized artifact containing constants, types, functions, and bytecode.
- **`VerifiedProgram`**: a `Program` that has passed verifier checks and is safe to execute.
- **`Frame`**: a call instance with its own register base and program counter.
- **Register**: a virtual slot holding a `Value`.
- **`SpanId`**: stable identifier for tracing and source mapping (e.g. node GUID).
- **Host call**: effectful operation implemented by the embedder.
- Host-call arguments are passed as borrowed views (bytes/strings are exposed as `&[u8]`/`&str` to avoid cloning).
- **Effect token**: SSA-like value that enforces effect ordering.

## Execution model (v1)
- Register-based VM with function frames and recursion.
- The VM executes `VerifiedProgram` only (verification is a required pre-step).
- Sync host calls only.
- Limits: fuel (instruction budget), max host calls, max call depth; host may contribute additional cost.
- Errors are traps:
  - VM traps: invalid bytecode, out-of-bounds, type mismatch at runtime boundary, fuel exceeded, stack overflow.
  - Host traps: host call reports fatal error (no rollback semantics).

## Values and types (v1)
### Builtins
- `Bool`, `I64`, `U64`, `F64`, `Unit`
- `Decimal { mantissa: i64, scale: u8 }` (per-value scale; rounding/division TBD)
- `Bytes` and/or `Str` (alloc-backed; representation TBD)

### Host objects
- `Obj { host_type: HostTypeId, handle: u64 }`
- Opaque: no VM-level equality/hash/serialization in v1.

### Aggregates (structural, immutable, acyclic)
- `Tuple` (heterogeneous, fixed arity)
- `Struct(TypeId)` (named fields with stable ordering defined by `TypeId`)
- `Array(ElemTypeId)` (homogeneous, ordered)
- Aggregates are immutable heap values; handles refer to acyclic graphs.
- Serialization is total for aggregates composed of serializable values; encountering `Obj` during serialization is a trap in v1.

## Effects
v1 uses a single linear effect token `eff`.

Effectful operations consume `eff_in` and produce `eff_out`:
- Pure op: `r3 = add(r1, r2)`
- Effectful op: `(eff2, r_out...) = host_call(eff1, symbol, args...)`

This provides an explicit sequencing mechanism that is easy to verify and extend to effect lanes later.

## Control flow
- Loops + conditionals.
- Recursion via `call` and frames.
- IR may use `phi`; lowering removes `phi` by inserting edge copies so bytecode has no `phi`.

## Serialization (portable)
- Versioned header; canonical little-endian encoding; varint indices/immediates.
- Function table and constant pool indices are bounds-checked by the verifier.
- Span mapping is stored as a compact side table (e.g. `pc_delta -> SpanId`) to avoid per-instruction payload bloat.
- Host calls use a compact `HostSigId` in bytecode; `HostSigId` resolves via a program table to `(symbol, signature hash, arg types, ret types)`.

## Verifier (must-haves)
- Control-flow integrity: valid jump targets, well-formed block boundaries.
- Register discipline: init-before-use; bounded register count per function.
- Type discipline: builtins + aggregate handles; host call signatures match (argument and return shapes).
- Resource bounds: max call depth and optional max blocks/instructions.
- Structural constraints: aggregate graphs are acyclic (enforced by construction or validated when deserializing aggregates).

## Extension points (v2+)
- Effect lanes (multiple tokens), plus rules for merging at control-flow joins.
- Closures/captures (`Closure { func_id, env }`) and environment heap values.
- Host-type capabilities (per-type eq/hash/serialization) and/or inline extern values.
- Incremental recompute: cache pure regions; host-managed caching for effectful regions.

## Open questions
- `Bytes`/`Str` representation and constant-pool encoding.
- Aggregate allocation strategy (arena vs refcount) under `no_std + alloc`.
- Precise instruction cost model and host cost reporting contract.
- Span table format and stability guarantees relative to source graph ids.
