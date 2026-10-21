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
- Multi-lane effects (single linear effect token only).
- Recoverable in-VM errors (`Result`/`Option`): v1 is trap-first.

## Glossary
- **`Program`**: serialized artifact containing constants, types, functions, and bytecode.
- **`VerifiedProgram`**: a `Program` that has passed verifier checks and is safe to execute. In addition to the
  serialized `Program`, it includes verifier-produced, execution-ready function bodies (decoded/validated
  instruction stream + register layout) used by the VM.
- **`Frame`**: a call instance with its own register base and program counter.
- **Register**: a virtual slot with a statically verified `ValueType`. The VM may store registers in
  per-`RegClass` arrays rather than as a tagged union to keep the hot loop monomorphic.
- **`SpanId`**: stable identifier for tracing and source mapping (e.g. node GUID).
- **Host call**: effectful operation implemented by the embedder.
- Host-call arguments are passed as borrowed views (bytes/strings are exposed as `&[u8]`/`&str` to avoid cloning).
- Hosts also receive read-only VM context for program metadata, tuple/struct/array inspection, and optional incremental access recording.
- **Effect token**: SSA-like value that enforces effect ordering.

## Execution model (v1)
- Register-based VM with function frames and recursion.
- The VM executes `VerifiedProgram` only (verification is a required pre-step).
- Sync host calls only.
- Limits: fuel (instruction budget), max host calls, max call depth; host may contribute additional cost.
- Errors are traps:
  - VM traps: invalid bytecode, out-of-bounds, type mismatch at runtime boundary, fuel exceeded, stack overflow.
  - Host traps: host call reports fatal error (no rollback semantics).

## Typed joins

Reserve typed destinations with `Asm::block`, place their declarations with `place`, and pass
incoming registers through the existing branch methods:

```rust
let join = asm.block(&[(4, ValueType::I64), (5, ValueType::I64)]);
asm.br(1, join.with_args(&[2, 3]), join.with_args(&[3, 2]));
asm.place(join)?;
asm.ret(0, &[4, 5]);
```

Both arms are checked even when a condition is constant. Transfers read all arguments before
changing destinations, so loops, cycles, and repeated sources have ordinary simultaneous-copy
semantics. Bare labels still work. This is a typed block-parameter contract within the existing
register VM; it does not require global SSA or declare recursive aggregate/callable types.
`Agg` is a kind-only declaration: compatible incoming shape facts support projections, while
conflicting or opaque facts cannot establish a projection's result type.

Declarations take no fuel and emit no instruction events. Prepared inspection lists executable
operations with their original lexical ends; the end of an instruction preceding a declaration
can differ from the next executable PC. Source disassembly exposes ordered parameter/type
lists and borrowed per-arm `InstrView::edge` views. `FunctionDisassembly::executable_pc` maps a
header to its first executable operation without hiding the original edge target. Structured
`VerifyError::BlockDeclaration` and `BlockEdge` failures retain source locations and contract
fields. Structural checks cover all encoded edges; type/initialization checks follow the
verifier's reachable-code policy.

## Values and types (v1)
### Builtins
- `Bool`, `I64`, `U64`, `F64`, `Unit`
- `Decimal { mantissa: i64, scale: u8 }` (per-value scale; rounding/division TBD)
- `Bytes` and/or `Str` (owned public values; runtime registers use handles to owned or borrowed payloads)

Within a run, byte/string handles resolve to immutable program constants, entry arguments,
aggregate fields, or owned computed payloads. The execution context stores source indices rather
than borrowed pointers, and discards them before reuse with another program, input set, or VM.
Host arguments borrow these sources directly. Host-owned return payloads move into run storage.
Owned results copy at the return boundary; persistent aggregates own their fields independently
of the context. Clearing scratch therefore never invalidates a returned byte/string value or
an aggregate retained in the VM heap. Aggregate handles remain scoped to their VM heap.

### Host objects
- `Obj { host_type: HostTypeId, handle: u64 }`
- Opaque: no VM-level equality/hash/serialization in v1.

### Aggregates (structural, immutable, acyclic)
- `Tuple` (heterogeneous, fixed arity)
- `Struct(TypeId)` (named fields with stable ordering defined by `TypeId`)
- `Array(ElemTypeId)` (homogeneous, ordered)
- Aggregates are immutable heap values; handles refer to acyclic graphs.
- Serialization is total for aggregates composed of serializable values; encountering `Obj` during serialization is a trap in v1.

### Functions and closures
- `Func`: a first-class reference to a program function.
- `Closure { func_id, env }`: a `Func` paired with a captured environment aggregate. When called via
  `call_indirect`, the VM injects `env` as the callee's first (`Agg`) argument; the call signature covers
  only the caller-visible arguments (see [v1 spec](v1_spec.md)).

## Effects
v1 uses a single linear effect token `eff`.

Effectful operations consume `eff_in` and produce `eff_out`:
- Pure op: `r3 = add(r1, r2)`
- Effectful op: `(eff2, r_out...) = host_call(eff1, symbol, args...)`

This provides an explicit sequencing mechanism that is easy to verify and extend to effect lanes later.

## Prepared host bindings

`HostFunctions<State>` registers callbacks with complete signatures. `bind(&VerifiedProgram)`
validates every declared import, including unused imports, before any operation runs. A binding
failure reports all missing symbols and signature mismatches in import-ID order, with expected
and available signatures. No mutable host state is consumed on failure.

`HostBindings::with_state` creates per-VM host instances that borrow one immutable callback
table. Each instance owns independent state, available through `vm.host().state()` and
`vm.host_mut().state_mut()` between runs. The definition registry can be dropped after binding;
changing registrations does not alter existing bindings.

Bindings belong to the original verified program allocation. Calls from another program,
including an equal clone, fail before a callback runs. This adapter suits repeated execution of
one program; custom `Host` implementations can also serve multiple programs. Callbacks still use
the ordinary borrowed argument ABI, access recording, runtime return checks, and fuel charging.

Host callbacks can return `HostError::custom(error)` for detailed operation failures. The payload
must implement `core::error::Error + Send + Sync + 'static` and owns its data independently of
the host. VM and graph results preserve its concrete type through the standard error source
chain. Downcast a source to recover domain fields; no host-side last-error slot is needed.
Cloned failures share the immutable payload. Errors containing arbitrary host payloads do not
implement value equality; match variants and compare their diagnostic fields instead.

Run the maintained example with:

```sh
cargo run -p execution_tape_wind_tunnel --bin bound_host
```

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

## Prepared-program inspection

`VerifiedProgram::function(FuncId)` borrows one prepared function; `functions()` visits all
functions in ID order. These views expose signatures, aggregate argument contracts, instruction
locations/spans, register-bank requirements, and buffer sizes without decoding bytecode or
allocating inspection records. Unknown function IDs return `None`.

`FunctionStats` distinguishes used prepared-buffer bytes from reserved capacity. Its register
bytes cover bank elements for one frame, with bytes/strings represented by handles. Counts use
the current build's layout and exclude payloads, aggregate data, frame bookkeeping, source
program storage, shared signature caches, and allocator overhead. They are not total heap or RSS
measurements. Instructions include unreachable code; `next_pc` is the lexical end, not a branch's
runtime successor.

The wind-tunnel tool prints tab-separated per-function statistics. Supply an optional function ID
for its signature, aggregate contracts, and prepared instruction locations:

```sh
cargo run -p execution_tape_wind_tunnel --bin inspect_prepared -- program.et 0
```

## Extension points (v2+)
- Effect lanes (multiple tokens), plus rules for merging at control-flow joins.
- Host-type capabilities (per-type eq/hash/serialization) and/or inline extern values.
- Incremental recompute: cache pure regions; host-managed caching for effectful regions.

## Open questions
- `Bytes`/`Str` representation and constant-pool encoding.
- Aggregate allocation strategy (arena vs refcount) under `no_std + alloc`.
- Precise instruction cost model and host cost reporting contract.
- Span table format and stability guarantees relative to source graph ids.
