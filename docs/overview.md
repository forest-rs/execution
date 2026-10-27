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
- Limits: instruction fuel, host calls, call depth, contract work, and logical byte/string work;
  host calls may contribute additional fuel cost.
- Errors are traps:
  - VM traps: invalid bytecode, out-of-bounds, type mismatch at runtime boundary, fuel exceeded, stack overflow.
  - Host traps: host call reports fatal error (no rollback semantics).

`Limits::max_payload_work` defaults to 64 MiB per run. Concatenation, slices, string-to-bytes,
and each owned payload materialization charge copied bytes before allocation/copying. Repeated
aggregate fields and root return slots charge each actual copy. Equal-length equality charges
one operand's byte length even on an early mismatch; unequal lengths cost zero. Bytes-to-string
charges twice the input length before copying and UTF-8 validation, so budget exhaustion can
precede an invalid-UTF-8 error. Slice bounds and character boundaries are checked first.

Successful direct host byte/string return slots charge their combined byte lengths after all
return-kind checks and before any result adoption. Raw host outcomes remain observable before
this rejection; host work and effects have already happened. Borrowed arguments, constants,
aggregate projections, and guest-call transfers cost zero. `PayloadWorkExceeded` reports the
limit, successfully used work, and the rejected request; rejection leaves used work unchanged.
Unrepresentable payload sizes/work calculations trap with `PayloadSizeOverflow`.

This deterministic work budget is separate from memory residency. Aggregate slots, frames,
storage of previously retained results, nested host aggregate payloads, host allocations/effects,
preparation, capture, and graph-adapter imports are outside it. Trusted workloads can explicitly
raise the limit or select `u64::MAX`. Usage resets on each invocation and context clearing.

`Limits::max_aggregate_work` separately bounds guest tuple, struct, and array construction,
defaulting to 1,000,000 units per run. Each constructor charges one node plus its immediate
field count before allocating field storage or copying payloads. Repeated fields each cost one;
empty nodes cost one; nested handles cost one field without traversing their contents. Payload
materialization still charges the byte/string budget separately. `AggregateWorkExceeded`
reports the configured limit, charged work and rejected request without charging that request.
`AggregateSizeOverflow` rejects an unrepresentable work calculation. Guest calls share the
counter; each new invocation resets it. Failed invocations roll back their new aggregates as usual.
Inputs and previously retained results, host allocations, graph imports, frame storage,
preparation, and capture remain outside this construction-work budget. It is not a heap limit.

## Authoring register declarations

Unit/effect registers have no runtime payload bank. Their declarations and initialization
requirements remain part of verification; host/guest boundaries still check their kinds.
Unit-only constants, moves and selects prepare as no-ops that retain source PCs, fuel charges
and trace/register observations. Unit join bindings need no parallel copies or cycle scratch.
Inspection reports their logical declaration count, while `register_bytes` counts only
payload-bearing banks. No wire-format or calling-convention change is required.

Declare the function once with `ProgramBuilder::declare_function`, then create its
body with `ProgramBuilder::function_asm`. The body owns declarations for r0 and
arguments in signature order. `Asm::alloc_register(kind)` appends each temporary's
known kind and returns its register ID. The assembler does not borrow the program
builder, so emitters can continue registering constants, imports, and contracts.
Finish with `define_function` and `build_verified`.

Emitters can replace an external kind vector and allocation callback with the
assembler's register allocator. For signatures discovered during emission, start
with `Asm::new`: its first register allocation lazily reserves r0 as `Unit` and
returns r1 for the first value register. Declare argument kinds in signature
order before temporaries, or transfer a known r0/argument prefix. Supply the
finished signature to `push_function_checked` afterward. Frontends that already number and type their
lowered registers should transfer that ledger with `set_register_types`, then
append any backend temporaries with `alloc_register`. No type inference pass is
introduced. Declarations reserve storage; they do not initialize values or prove
aggregate structure. Existing raw assembly and explicit ledgers remain supported.

## Typed joins

Each function supplies a complete register-kind ledger through allocation or
`Asm::set_register_types`, including r0 (`Unit`),
arguments and compiler temporaries. Record kinds when allocating registers. Declarations determine
execution banks; the verifier checks definitions and operands against them without reconstructing
scalar types at control-flow joins. Declarations do not initialize registers or prove aggregate shapes.

Reserve destinations with `Asm::block`, place their declarations with `place`, and pass incoming
registers through the existing branch methods. Parameter kinds come from the function ledger:

```rust
asm.set_register_types(vec![ValueType::Unit, ValueType::Bool, ValueType::I64,
    ValueType::I64, ValueType::I64, ValueType::I64]);
let join = asm.block(&[4, 5]);
asm.br(1, join.with_args(&[2, 3]), join.with_args(&[3, 2]));
asm.place(join)?;
asm.ret(0, &[4, 5]);
```

Kinds, register bounds, and call/return arity are checked even in unreachable instructions.
Initialization and flow-sensitive structural contracts apply to reachable instructions. Both branch arms are
checked even when a condition is constant. Transfers read all arguments before
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

Run `cargo run -p execution_tape_wind_tunnel --bin block_parameters` to execute equivalent
loops using explicit moves and block arguments, with prepared/frame statistics. Run
`cargo bench -p execution_tape_wind_tunnel --bench block_parameters` for cold owned preparation
(fixture cloning excluded) and reused execution. Each case checks results before timing;
reported buffer bytes are layout measurements, not allocator traffic or RSS.

## Values and types (v1)
### Builtins
- `Bool`, `I64`, `U64`, `F64`, `Unit`
- `Decimal::new(mantissa: i128, scale: u8)` (per-value scale; rounding/division are host policies)
- `Bytes` and/or `Str` (owned public values; runtime registers use handles to owned or borrowed payloads)

Within a run, byte/string handles resolve to immutable program constants, entry arguments,
aggregate fields, or owned computed payloads. The execution context stores source indices rather
than borrowed pointers, and discards them before reuse with another program, input set, or VM.
Host arguments borrow these sources directly. Host-owned return payloads move into run storage.
Owned results copy at the return boundary; persistent aggregates own their fields independently
of the context. Clearing scratch therefore never invalidates a returned byte/string value or
an aggregate retained in the VM heap. Aggregate handles remain scoped to their VM heap.

### Compositional guest values

Guest signatures reference a shared program-owned contract DAG. `ValueType` still describes
storage/host ABI kinds, and the register ledger remains kind-only. A nested tuple and a flat tuple
both occupy an aggregate bank slot, while their contracts distinguish contents across function
boundaries. `FunctionSig` is the kind-only shorthand; `FunctionAbi` supplies `ContractRef` lists.
Use `ProgramBuilder::contract` to intern children before parents, then use their IDs in signatures.

Local construction infers nested facts without annotations. Projections preserve selected child
facts, including aliased destinations. Typed edges transfer those facts simultaneously; joins and
selects preserve common structure. Direct calls propagate declared result contracts. The verifier
rejects known contradictions, and the interpreter checks unresolved boundary data before callee
entry or result installation. Opaque host results cannot bypass a declared result contract.

Checks borrow immutable graphs, traverse iteratively and cache successful `(handle, contract)`
proofs for one run. `Limits::max_contract_work` bounds pair/value visits and member edges separately
from instruction fuel. Errors report the argument/result position and nested member path. Signature
inspection borrows contract IDs and projected kind views, without expanding a graph into a tree.
Nominal IDs remain significant and unknown tuple slots remain explicit. Function and closure
contracts expose exact argument/result signatures, preserved through projections and indirect calls.
Closures hide their environment layout while concrete bodies enforce it. Indirect target matching
uses full contract identity; opaque callable kinds establish no signature. Recursive type equations
and variance/subtyping are separate capabilities.

## Host objects
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
- Typed block parameters express joins directly; edge transfers are simultaneous preparation details.

## Serialization (portable)
- Versioned header; canonical little-endian encoding; varint indices/immediates.
- Function table and constant pool indices are bounds-checked by the verifier.
- Span mapping is stored as a compact side table (e.g. `pc_delta -> SpanId`) to avoid per-instruction payload bloat.
- Host calls use a compact `HostSigId` in bytecode; `HostSigId` resolves via a program table to `(symbol, signature hash, arg types, ret types)`.

## Verifier (must-haves)
- Control-flow integrity: valid jump targets, well-formed block boundaries.
- Register discipline: init-before-use; bounded register count per function.
- Type discipline: builtins + aggregate handles; host call signatures match ABI kinds; guest boundaries enforce compositional value contracts.
- Resource bounds: max call depth and optional max blocks/instructions.
- Structural constraints: aggregate graphs are acyclic (enforced by construction or validated when deserializing aggregates).

## Prepared-program inspection

`VerifiedProgram::function(FuncId)` borrows one prepared function; `functions()` visits all
functions in ID order. These views expose signatures, compositional argument/result contracts, instruction
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

## Source connections

Frontend snapshots and instruction-to-source mappings live in `execution_tape::source`,
independently of the terminal explorer. `SourceMap::new` validates exact instruction PCs,
UTF-8 byte ranges and budgets; binding checks the full canonical program identity. A bound
lookup returns a validated borrowed `SourceRange` with exact `text()`, `document()`, `range()`,
and named `SourcePosition { line, byte_column }` endpoints. These queries allocate no preview
strings. `context()` explicitly requests bounded sanitized structured lines; Observatory adds
terminal-cell layout. Source maps and their sidecar encoding remain optional frontend metadata,
with no source lookup or storage added to ordinary verification/execution.

## Optional instruction facts

`ProgramAnalysis::new(&program, &config)` verifies a borrowed source once; `ProgramAnalysis::from(&prepared)`
reuses a prepared program's proof. Select a function with `function(FuncId)`, then query an exact
source boundary with `instruction(pc)`. `register(reg)` returns the fixed storage kind plus
before/after guarantees and local definition origins. `FunctionDisassembly::instruction(pc)`
provides an indexed source lookup for following those defining locations.

`ValueFact` distinguishes unreachable, not definitely initialized, and an initialized contract.
An initialized `Kind(Agg)` means its nested structure is unknown. Contract IDs belong to this
function analysis's table and remain stable across its queries; they are not source contract IDs.
Incoming edges keep same-target arms separate and expose predecessor values and simultaneous
parameter bindings. Header parameters are already admitted before the declaration itself.
Function entry additionally meets invocation argument/effect seeds, which are not CFG edges.

After facts describe local source transfer: a call assumes successful callee return, while a
branch precedes the successor edge bindings. They do not imply trace dispatch timing or observed
runtime values. Rejected programs yield no inferred facts. The optional artifact retains block
checkpoints and compact aggregate write effects; its cursor replays only the selected block prefix.
Each register's outgoing fact comes directly from its write effect. There are no full per-instruction
register-state snapshots, no `Program` clones, and no inspection storage in normal VM artifacts.

## Borrowed value observations

`TraceMask::VALUES` enables `TraceSink::values` on the ordinary execution path. `ValueEvent::Entry`
borrows arguments, limits and the initial aggregate heap before invocation validation. `HostCall`
borrows host arguments and the raw returned values/fuel or typed error before ABI validation and
fuel debit. A call denied by VM limits has no host outcome. `ReturnCandidate` borrows only newly
appended top-level outputs before aggregate validation and compaction; it includes the root return
location and does not report nested function returns.

Combine `VALUES` with `RUN` for the final outcome. A candidate can still fail, and pre-commit
aggregate handles can change at commit. Values and their heap are borrowed for the callback;
the sink chooses what to retain. Snapshotting an invalid graph returns an error independently
of the eventual VM trap. Missing completion, including panic unwinding, is incomplete evidence.
These are observation primitives: recording storage, program identity, sharing across events,
opaque host values/errors and replay policy belong outside the VM.

## Extension points (v2+)
- Effect lanes (multiple tokens), plus rules for merging at control-flow joins.
- Host-type capabilities (per-type eq/hash/serialization) and/or inline extern values.
- Incremental recompute: cache pure regions; host-managed caching for effectful regions.

## Open questions
- `Bytes`/`Str` representation and constant-pool encoding.
- Aggregate allocation strategy (arena vs refcount) under `no_std + alloc`.
- Precise instruction cost model and host cost reporting contract.
- Span table format and stability guarantees relative to source graph ids.
