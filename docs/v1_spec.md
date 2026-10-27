# `execution_tape` bytecode and execution specification

This document describes wire format 0.5 and the execution contract of the Rust 0.1 API.
The filename is historical: it does not name a 1.0 format or promise durable interchange.
Decoding requires an exact wire-version match; regenerate compiled programs when upgrading
from an older format. The core remains `no_std + alloc`.

See also: [`overview.md`](overview.md).

## Fence
`execution_tape` owns:
- Program + bytecode format (portable, versioned).
- Verifier rules and diagnostics.
- Execution semantics (register VM, call frames, limits, traps).
- Tracing hooks and SpanId mapping.
- Narrow host-call ABI keyed by stable symbol + signature hash.

It does **not** own:
- Node graph authoring UI/formats.
- Domain node definitions (geometry, document layout, etc.).
- Host object semantics beyond an opaque handle contract.

## Goals
- Fast execution for mostly-static programs (hundreds of nodes/opcodes).
- Portable bytecode with frontend-supplied SpanIds.
- Verified execution with explicit instruction, depth, host-call, and data-work limits.
- Trap-first error model.
- Structural (deep) aggregates: `Tuple`, `Struct`, `Array`, immutable and acyclic.

## VM non-goals
- Multi-lane effects: calls use the single reserved r0 convention.
- In-VM recoverable errors (`Result`/`Option` values).
- Incremental scheduling and invalidation: these belong to `execution_graph` above the VM.

Function values and closures are supported. A closure pairs a function reference with a captured
aggregate environment; `call_indirect` injects that environment as the body's first argument.

## Terminology
- **Value**: runtime datum computed by the VM. Some values (notably `Bytes`/`Str`) may be stored indirectly by
  handle into a per-run arena while preserving the same observable semantics.
- **Register**: a virtual slot; bytecode operands refer to registers by index.
- **Frame**: one function activation with a `pc` and bases into typed payload banks.
- **Program Counter (pc)**: byte offset into a function’s bytecode stream.
- **SpanId**: nonzero frontend-owned identifier for tracing/source mapping. Stability is the
  frontend's responsibility; optional `source::SourceMap` metadata connects instructions to source snapshots.
- **Host symbol**: stable identifier for a host-provided function.
- **Sig hash**: hash of the host call signature (arguments and results).

## Determinism and portability
- Numeric: `f64` follows platform IEEE behavior; float nondeterminism is acceptable.
- Portable serialization: canonical little-endian, varint encoding for indices/immediates.
- No pointer serialization or serialization of external host-object state.

## Value model

### Value kinds
The VM defines a closed set of value kinds:
- `Unit`
- `Bool`
- `I64`
- `U64`
- `F64`
- `Decimal` (`mantissa: i128`, `scale: u8`)
- `Bytes` (alloc-backed)
- `Str` (alloc-backed, UTF-8)
- `Obj` (opaque handle)
- `Agg` (aggregate handle: tuple/struct/array heap node)
- `Func` (function reference)
- `Closure` (function reference + captured environment aggregate)

The VM stores payload-bearing registers in per-kind banks (`RegClass`), rather than as tagged
`Value`s. The producer declares each register's kind, and verification enforces it before preparing
typed operands. Unit/effect registers have no runtime payload bank.

### `Decimal`
- Per-value scale (`u8`) and mantissa (`i128`), including the full signed range.
- Construction preserves scale; equality distinguishes `1.0` and `1.00` (including differently
  scaled zeros). No implicit normalization, division, or rounding is performed.
- Decimal literal mantissas use signed LEB128, bounded to 19 bytes. Scale is one raw byte.
  The in-memory constant pool packs each decimal into 17 bytes in the shared blob arena;
  `Program::const_decimal` reads it without growing other constant entries.
- Arithmetic:
  - `dec_add`, `dec_sub`, `dec_mul` are defined only when the result scale is well-defined (e.g. add/sub require equal scale; mul defines scale = a.scale + b.scale).
  - Division and rounding are host-provided.

### Host objects (`Obj`)
`Obj` is an opaque value:
- `host_type: HostTypeId` (stable within the embedder)
- `handle: u64` (embedder-defined identity)

Constraints:
- No guest equality/hash/ordering operations on objects.
- No portable runtime value archive is defined. Snapshots and recordings may retain opaque
  tokens, but do not reconstruct external object state.
- Host calls may accept/return `Obj` values, and the verifier checks that host signatures match.

### Aggregates (`Agg`)
Aggregates are immutable, acyclic, and structural.

Kinds:
- `Tuple([Value])` (heterogeneous, fixed arity)
- `Struct(TypeId, [Value])` (fields in the order defined by `TypeId`)
- `Array(ElemTypeId, [Value])` (homogeneous)

Constraints:
- Construction and snapshot imports validate handles and preserve acyclicity.
- There are no guest structural equality/hash operations or aggregate serialization opcodes.
- Owned `ValueSnapshot`s retain aggregate DAGs in memory. `Program::encode` serializes executable
  metadata and constants, not live aggregate storage.

## Type model

### Program type table
The program contains a type table defining:
- Struct layouts (`TypeId` → ordered field list and field types).
- Array element types (`ElemTypeId`).

The verifier requires a complete register-kind ledger and checks every instruction's operand and
result kinds, including unreachable code. Reachable dataflow separately establishes initialization
and structural facts. Program-owned compositional contracts refine function boundaries; unknown
boundary values are checked at runtime. A storage kind such as `Agg` alone proves no nested shape.

## Effects model
Call and host-call instructions carry `eff_in` and `eff_out` operands; returns carry an effect
operand. The verifier requires all of them to name r0, declared as `Unit` and initialized at entry.
Pure instructions do not carry effect operands. Unit has no runtime payload storage.

Host calls execute synchronously in verified control-flow order. The reserved register expresses
a calling convention, not distinct SSA effect values or multiple effect lanes. Failed guest
execution does not roll back external host effects.

## Control flow and functions

### Functions
A function's position in the function table defines its `FuncId`. In memory, each entry includes:
- `arg_count` and `ret_count` (value arguments/results; effect is separate).
- `reg_count`, including r0 and unused declarations.
- A range into the shared register-kind ledger; r0 is `Unit`, followed by argument kinds.
- Ranges into packed bytecode and span-entry arenas.
- Ranges of argument/result contract IDs and optional value-name IDs.

The wire function table instead references indexed bytecode blobs and span tables, as described
below. Decoding packs them into the shared arenas.

### Calls and frames
Each frame holds the function, source PC, and bases into payload-bearing register banks.
Preparation translates virtual register IDs into class-local operands. Runtime addressing uses
the corresponding bank base and class-local index, not `base + virtual_register_id`. Unit needs
no payload address. Arguments and results are explicit register lists in bytecode.

Recursion is allowed, subject to `Limits::max_call_depth`. Function references and nominal type
IDs are program-local; aggregate handles are heap-local. Raw `Value`s carry no ownership domain.
The graph adapter's owned `TapeValue` snapshots preserve that domain and reject incompatible
program-local transfers; they do not enable cross-program guest invocation.

### Control flow invariants
- All jump targets must point to instruction boundaries.
- Structured blocks are not required, but all target pcs must be within the current function.
- Typed block parameters may represent joins directly; mutable-register control flow remains supported.
- `block` declares ordered, unique parameter registers and their `ValueType`s. Parameters may not
  include `r0`. Function-entry arguments still come from the function signature; a declaration at
  PC zero must have no parameters. A declaration begins a basic block and requires a following
  executable body ending in an explicit terminator.
- All entries into a declared block target its header, with matching argument counts. Fallthrough
  and branches into its body are rejected. Argument types are checked on reachable source edges;
  declaration, arity, split, and target structure are checked even in unreachable code.
- Edge transfers read every source before overwriting any destination. Cycles, self transfers,
  and repeated sources are valid. The two arms of a branch remain distinct even at equal targets.
- `Agg` parameters specify the value kind only. Common known incoming structure survives the join;
  conflicting nested facts widen, and opaque shapes grant no structural knowledge. Such a parameter alone cannot type a
  projection. Function/closure kinds likewise do not declare callable signatures.
- Declarations disappear during preparation. A branch plus its selected parallel transfer costs
  one fuel and emits one instruction attempt/completion pair. Source header PC, first executable
  PC, lexical instruction end, and actual successor remain distinct. Existing call trace semantics
  are unchanged; call-frame entry locations use the first executable PC and its span.

## Limits and traps

### Limits

`max_contract_work` bounds recursive aggregate obligations separately from guest instruction fuel.
It counts each aggregate/contract pair visit (including cached visits), each scalar member visit,
each callable signature position, and each child edge (including unconstrained slots and captured environments). Successful proofs are cached by aggregate
handle and contract ID until the run ends; they never survive heap compaction or the next run's
reset. Exceeding the budget traps with the function, argument/result position and configured limit.
Traversals are iterative; sharing does not cause repeated expansion of proven subgraphs.

- `fuel`: each executable instruction costs one; successful host calls may charge additional fuel.
- `max_call_depth`: caps guest frames.
- `max_host_calls`: caps executed host calls.
- `max_contract_work`: bounds boundary-contract traversal as described above.
- `max_payload_work`: bounds logical byte/string copying and comparison work (64 MiB by default).
- `max_aggregate_work`: charges one node plus immediate field count for each guest aggregate
  construction (1,000,000 units by default).

The [overview](overview.md#execution-model) specifies data-work charging and exclusions. These
are work limits, not heap/RSS limits. Preparation, total frame storage, host allocations, graph
imports, and previously retained results are not bounded by these data-work counters.
`VerifyConfig::max_regs_per_function` separately caps declarations (65,536 by default); there
are no configurable preparation-byte, instruction-count, or constant-size budgets.

### Traps
`TrapInfo` reports a `Trap`, function ID, source PC and best-effort SpanId. Host failures also carry
the symbol, signature hash and owned `HostError`; custom host errors remain available through
the standard error source chain.

Runtime failures include exhausted fuel/depth/host/data-work limits, arithmetic and conversion
errors, invalid indexing/UTF-8, guest contract mismatches, and rejected host returns. `TrapCode`
represents an explicit guest trap. See `vm::Trap` for the complete variant set.

Malformed containers and invalid bytecode fail decoding or verification before execution; they
are not runtime `InvalidProgram` traps. A failed invocation discards newly allocated aggregates
and preserves the caller's previous output buffer. It cannot undo host effects.

## Bytecode encoding

### Canonical integer encoding
- All multibyte integers are little-endian.
- Indices and small immediates use unsigned LEB128 (ULEB128).
- Signed immediates (if present) use signed LEB128 (SLEB128).
- Decoders must accept non-canonical (non-minimal) LEB128 encodings, as long as they decode to the same integer value.
  - Rationale: some producers (including the provided bytecode builder) reserve fixed-width windows
    for later patching (e.g. branch/jump targets) to avoid shifting the byte stream.

### Instruction encoding
Each instruction is:
- `opcode: u8`
- `operands: varint-encoded fields` (register indices, immediates, ids)

Register operands are ULEB128 indices (`Reg`).

### Program counter (PC)
For control-flow instructions, `pc_*` operands are **byte offsets** into the function bytecode stream, and must
point at a valid instruction boundary within that function.

#### Note: labels and patching
Most producers will want to write `br`/`jmp` targets using symbolic labels. There are two common ways to lower
labels to concrete `pc_*` byte offsets:
- **Two-pass encode**: compute the final byte offsets for all instructions (including varint lengths), then
  encode the instruction stream once with final `pc_*` operands.
- **Single-pass encode with fixups**: emit placeholder bytes for `pc_*` operands, remember “fixups”, and patch
  the placeholder bytes after all labels are placed.

`execution_tape` intentionally allows non-canonical LEB128 encodings so that fixup-based assemblers can reserve a
fixed-width window (e.g. 5 bytes for a `u32` PC) and patch in-place without shifting later bytes.

### Span table encoding (compact)
Per function, store a list of entries:
- `pc_delta: ULEB128`
- `span_id: ULEB128` (nonzero)

Meaning: starting at `pc += pc_delta`, current span becomes `span_id` until the next entry.

SpanId meaning and stability are the frontend's responsibility. A SpanId alone is not a source
range. Optional `source::SourceMap` metadata binds exact instruction locations to validated UTF-8
ranges in frontend snapshots; its program identity must match before lookup. These maps are
separate from the executable container and add no source storage to ordinary verification/runs.

## Container format (sections)
The serialized program is:
- Header:
  - `magic[8] = "EXTAPE\\0\\0"`
  - `version_major: u16le`
  - `version_minor: u16le`
- A sequence of sections:
  - `tag: u8`
  - `len: ULEB128`
  - `payload[len]`

The header must match version 0.5 exactly. Within that version, unknown section tags are skipped;
this does not imply compatibility with future wire versions. Known sections are rejected if duplicated.

## Tracing (hooks)

The VM can optionally emit tracing events to a sink.

The tracing API supports:
- run start/end events
- instruction step events
- scope events for profiling:
  - call frames (function enter/exit)
  - host calls (time spent in embedder code)

`TraceMask` selects callbacks. `execution_tape_footfall` and `execution_tape_tracy` implement
profiler adapters without adding their backend dependencies to the core. Borrowed value events
also support owned in-memory capture and checked replay through `execution_tape::recording`.
Callbacks and recording are opt-in; source lookup is separate optional metadata.

### Section tags
- `1 = symbols`
- `2 = const_pool`
- `3 = types`
- `4 = function_table`
- `5 = bytecode_blobs`
- `6 = span_tables`
- `7 = function_sigs`
- `8 = host_sigs`
- `9 = names` (optional)
- `10 = call_sigs` (optional)
- `11` is retired
- `12 = contracts`

Sections `1..=8` and `12` are required; `9 = names` and `10 = call_sigs` are optional (each omitted
entirely when empty). Unknown section tags are skipped within the accepted wire version.

## Symbols
The symbol table stores UTF-8 strings used by the program, including host-call targets and optional debug metadata:
- `count: ULEB128`
- repeated `count` times:
  - `len: ULEB128`
  - `utf8_bytes[len]`

Symbol id `0` is reserved and is encoded as an empty string (`len = 0`). All referenced symbol ids must be non-zero.

## Constant pool
The program constant pool stores immutable literals referenced by index:
- `I64/U64/F64/Bool/Unit/Decimal`
- `Bytes` (length + bytes)
- `Str` (length + UTF-8 bytes)

The constant pool contains no aggregate literals. Aggregate construction uses runtime instructions.

## Types
The type table defines struct layouts and array element types.

### ValueType encoding
Value types are encoded as:
- `tag: u8`
- additional payload depending on `tag`

Tags:
- `0 Unit`
- `1 Bool`
- `2 I64`
- `3 U64`
- `4 F64`
- `5 Decimal`
- `6 Bytes`
- `7 Str`
- `8 Obj(host_type_id: u64le)`
- `9 Agg`
- `10 Func`
- `11 Closure`

### Struct types
Field names are interned once per Types payload; each struct field references that table by id.

- `field_name_count: ULEB128`
- repeated `field_name_count` times:
  - `name_len: ULEB128`
  - `name_utf8[name_len]`
- `struct_count: ULEB128`
- repeated `struct_count` times:
  - `field_count: ULEB128`
  - repeated `field_count` times:
    - `field_name_id: ULEB128` (index into the field-name table above)
    - `field_type: ValueType`

### Array element types
- `elem_count: ULEB128`
  - repeated `elem_count` times:
  - `elem_type: ValueType`

## Bytecode blobs
Stores per-function bytecode streams:
- `count: ULEB128`
- repeated `count` times:
  - `len: ULEB128`
  - `bytecode[len]`

## Span tables
Stores per-function span tables:
- `count: ULEB128`
- repeated `count` times:
  - `entry_count: ULEB128`
  - repeated `entry_count` times:
    - `pc_delta: ULEB128`
    - `span_id: ULEB128`

## Function table
Stores per-function metadata and references into blob sections:
- `count: ULEB128`
- repeated `count` times:
  - `arg_count: ULEB128` (value args; effect is not counted)
  - `ret_count: ULEB128`
  - `reg_count: ULEB128`
  - `register_types[reg_count]: ValueType...` (ABI/storage kind encoding)
  - `bytecode_index: ULEB128`
  - `span_table_index: ULEB128`

The current wire version is 0.5. Older versions are rejected; rebuild serialized programs
from their producer. Each declaration is authoritative for the register's entire function lifetime.
Every instruction definition and operand must match, including inactive instructions and unused
results. A declaration does not initialize a register or supply a projection's aggregate shape proof.
Scalar kinds need no dataflow inference; must-initialization and aggregate proofs remain separate.

## Function signatures (typed)
Typed signatures are mandatory and stored separately from `function_table`:
- `count: ULEB128` (must match `function_table.count`)
- repeated `count` times:
  - `arg_count: ULEB128`
  - `arg_contract_ids[arg_count]: ULEB128...`
  - `has_arg_names: u8` (`0` or `1`)
  - if `has_arg_names == 1`:
    - `arg_name_symbol_ids[arg_count]: ULEB128...` (`0` means “no name”; otherwise a non-zero `symbol_id`)
  - `ret_count: ULEB128`
  - `ret_contract_ids[ret_count]: ULEB128...`
  - `has_ret_names: u8` (`0` or `1`)
  - if `has_ret_names == 1`:
    - `ret_name_symbol_ids[ret_count]: ULEB128...` (`0` means “no name”; otherwise a non-zero `symbol_id`)

The `arg_count`/`ret_count` must match the corresponding `function_table` entry.

Each ID references the program's mandatory compositional contract table. ABI kinds are projected
from those definitions; they are not a second serialized guest signature. Register declarations
remain kind-only. Tuple/struct/array contracts all project to `Agg`.

## Compositional contracts

Section tag `12` is mandatory, including for an empty table. Tag `11` is retired. The payload is a
ULEB128 definition count followed by that many nodes:

- tag `0`: one `ValueType` kind, with no structural refinement;
- tag `1`: tuple length (ULEB128), then one `u8` presence flag per slot; flag `1` is followed by a
  child contract ID (ULEB128), and flag `0` leaves the slot unconstrained;
- tag `2`: nominal struct `TypeId` (ULEB128), field count (ULEB128), then field contract IDs;
- tag `3`: nominal `ElemTypeId` (ULEB128), then one element contract ID;
- tag `4`: function argument count/IDs, then result count/IDs (all ULEB128);
- tag `5`: closure user argument count/IDs, then result count/IDs (all ULEB128).

Callable contracts describe exact exposed signatures. A closure's captured environment is hidden
from this signature and checked against its concrete body's first argument. `Kind(Func)` and
`Kind(Closure)` remain opaque; they prove no invocation signature.

Child IDs must refer to earlier definitions. Forward/cyclic or missing references are rejected.
Equivalent duplicate definitions are normalized on decode, including their signature references.
Struct refinements must preserve schema field count and each field's kind; array refinements must
preserve the nominal element kind. These rules apply even to unused definitions. Distinct nominal
IDs remain distinct when their layouts match. Recursive functions can use finite contract DAGs;
recursive type equations are not supported.

An unconstrained tuple slot is distinct from a slot with `Kind(Agg)`. Neither proves a nested shape.
Known contradictions at reachable calls/returns fail verification. Unknown facts remain runtime
obligations. Every actual argument and successful result satisfies its declared contract: checks
precede callee entry, caller result copies and root commit. Raw root return candidates remain
observable before checking, so a rejected result can be recorded and replayed.

Local construction and projections carry nested facts. Joins/selects retain their common structural
information, widening incompatible nested facts without inventing a kind. Direct calls introduce
their declared result contracts. Indirect calls propagate their signature's result contracts after
exact target-signature matching. Callable joins retain identical exposed signatures; differing
signatures widen to an opaque callable kind. This slice implements no function variance or subtyping.

## Host signatures
Host call targets are keyed by a stable `(symbol_id, sig_hash)` pair, but bytecode references them by index:
- `count: ULEB128`
- repeated `count` times:
  - `symbol_id: ULEB128`
  - `sig_hash_u64le: u64le`
  - `arg_count: ULEB128`
  - `arg_types[arg_count]: ValueType...`
  - `ret_count: ULEB128`
  - `ret_types[ret_count]: ValueType...`

The verifier recomputes `sig_hash` from the canonical encoding of `(arg_types, ret_types)` and requires it to match.

## Call signatures (optional)
Indirect calls (`call_indirect`) reference a program-owned call signature by index. Unlike host
signatures, a call signature carries no `symbol_id` or `sig_hash` — it only constrains the argument and
result contracts checked against the callee value at the call site. A call signature lists the
*caller-visible* arguments: for a `Closure` callee the captured environment is injected by the VM as the
callee's first (`Agg`) argument and is **not** part of the call signature. The section is optional and
omitted entirely when there are no call signatures:
- `count: ULEB128`
- repeated `count` times:
  - `arg_count: ULEB128`
  - `arg_contract_ids[arg_count]: ULEB128...`
  - `ret_count: ULEB128`
  - `ret_contract_ids[ret_count]: ULEB128...`

Every ID references the shared contract table, with the same duplicate normalization as guest
function signatures. Exact prepared signature keys provide collision-safe dispatch; an ABI-kind
hash alone does not prove contract equality. Known contradictions fail verification. Otherwise,
matching occurs before callee entry, and the callee enforces the same result obligations before
caller installation.

## Names (optional)
This section provides optional debug metadata for tools (disassembly, profiling, diagnostics). It is not required for execution.

Payload:
- `has_program_name: u8` (`0` or `1`)
- if `has_program_name == 1`:
  - `program_name_symbol_id: ULEB128`
- `function_name_count: ULEB128`
- repeated `function_name_count` times:
  - `func_id: ULEB128`
  - `name_symbol_id: ULEB128`
- `label_name_count: ULEB128`
- repeated `label_name_count` times:
  - `func_id: ULEB128`
  - `pc: ULEB128` (byte offset within the function)
  - `name_symbol_id: ULEB128`

## Opcode set
This is the minimal set to support loops + recursion + host calls + aggregates.

### Conventions
- `rX` denotes a register index.
- All destination registers are written exactly once per dynamic execution of the instruction.
- Unless specified, each instruction costs 1 fuel.
- **Register convention**:
  - `r0` is the effect token register.
  - Value arguments (for a function with `arg_count = N`) are in `r1..=rN`.
  - Other registers are temporaries and/or outputs.

### Core
- `nop`
- `mov r_dst, r_src`
- `trap trap_code`
- `block params...` (declaration, no runtime step; kinds come from function register declarations)

### Constants
- `const_unit r_dst`
- `const_bool r_dst, imm_bool`
- `const_i64 r_dst, imm_sleb`
- `const_u64 r_dst, imm_uleb`
- `const_f64 r_dst, imm_f64_le`
- `const_decimal r_dst, mantissa_sleb, scale_u8`
- `const_pool r_dst, const_index` (typed by verifier/const tag)
- `const_func r_dst, func_id` (materializes a first-class `Func` reference)

### Numeric (subset; expand later)
- `i64_add r_dst, r_a, r_b`
- `i64_sub r_dst, r_a, r_b`
- `i64_mul r_dst, r_a, r_b`
- `f64_add r_dst, r_a, r_b`
- `f64_sub r_dst, r_a, r_b`
- `f64_mul r_dst, r_a, r_b`
- `f64_div r_dst, r_a, r_b`
- `f64_neg r_dst, r_a`
- `f64_abs r_dst, r_a`
- `f64_min r_dst, r_a, r_b` (NaN-propagating)
- `f64_max r_dst, r_a, r_b` (NaN-propagating)
- `f64_min_num r_dst, r_a, r_b` (number-favoring)
- `f64_max_num r_dst, r_a, r_b` (number-favoring)
- `f64_rem r_dst, r_a, r_b`
- `f64_to_bits r_dst, r_a` -> `U64`
- `f64_from_bits r_dst, r_a` -> `F64`
- `dec_add r_dst, r_a, r_b`
- `dec_sub r_dst, r_a, r_b`
- `dec_mul r_dst, r_a, r_b`

#### Floating-point semantics
- `f64_min`/`f64_max` propagate NaN: if either input is NaN, the result is NaN.
- `f64_min_num`/`f64_max_num` are number-favoring: if exactly one input is NaN, the other is returned; if both are NaN, the result is NaN.
- For `f64_min`/`f64_max`/`f64_min_num`/`f64_max_num`, when both inputs are zero with different signs, `min` returns `-0.0` and `max` returns `+0.0`.
- `f64_rem` uses Rust's `%` semantics (remainder with a truncated quotient).
- `f64_to_bits`/`f64_from_bits` preserve IEEE 754 bit patterns (including NaN payloads).
- Rounding and transcendental functions are host-provided (e.g. `f64_round_even`, `f64_round_away`, `f64_floor`, `f64_ceil`, `f64_trunc`, `f64_sqrt`, `f64_log`, `f64_exp`).

### Comparisons + branching
- `i64_eq r_dst, r_a, r_b` -> `Bool`
- `i64_lt r_dst, r_a, r_b` -> `Bool`
- `bool_not r_dst, r_a`
- `bool_and r_dst, r_a, r_b`
- `bool_or r_dst, r_a, r_b`
- `bool_xor r_dst, r_a, r_b`
- `br r_cond, pc_true, pc_false`
- `jmp pc_target`
- `jmp.args pc_target, args...`
- `br.args r_cond, pc_true, pc_false, true_arg_count, args...`
  (`args` stores true arguments followed by false arguments; the split must be within the list)

### Calls
- `call r_eff_out, func_id, r_eff_in, args... -> rets...`
  - The callee function signature defines arg/ret counts.
  - Call threads `eff` explicitly (even for pure callees, `eff` is forwarded unchanged).
  - Fuel cost: one instruction; no additional per-argument/result charge.
- `ret r_eff_in, rets...`
  - Returns values to caller-provided return registers.
- `call_indirect r_eff_out, call_sig_id, r_callee, r_eff_in, args... -> rets...`
  - Calls a `Func` or `Closure` value held in `r_callee`, checked against `call_sigs[call_sig_id]`.
  - **Closure ABI:** for a `Closure` callee the VM injects the captured environment as the callee's first
    argument (which must be `Agg`), so the callee's declared signature is `[Agg, <call_sig args>...]`; for
    a `Func` callee nothing is injected and `call_sig` matches the callee 1:1. Return types match 1:1.
  - Threads `eff` like `call`; traps if the callee's signature does not match.
- `closure_new r_dst, r_func, r_env`
  - Builds a `Closure` pairing the `Func` in `r_func` with a captured environment aggregate in `r_env`.
    The target must accept an aggregate environment first; construction checks that concrete
    contract before producing the closure. Dispatch also checks externally supplied environments,
    including that an unrefined environment handle resolves. `r_env` is injected as the first argument.

### Host calls
- `host_call r_eff_out, host_sig_id, r_eff_in, args... -> rets...`
  - Increments host-call count; traps if limit exceeded.
  - Host returns:
    - either success with return values and optional additional fuel cost
    - or failure, which becomes `HostCallFailed { symbol, sig_hash, error }`

## Opcode encodings
The schema in `execution_tape/opcodes.json` defines instruction names, bytes, operands and
encodings. `execution_tape/src/opcode.rs` exposes the generated `Opcode` mapping. For register
operands (`reg` and `reg_list`), the JSON spec also includes an `access` field (`read` or `write`) used by analysis tooling (e.g. reads/writes sets).

All register indices and small integers are ULEB128 unless noted.

- `0x00 nop`
- `0x01 mov dst, src`
- `0x02 trap code`
- `0x03 block params` (counted ULEB32 register IDs; kinds come from the function register ledger)

- `0x10 const_unit dst`
- `0x11 const_bool dst, imm_u8` (`0` or `1`)
- `0x12 const_i64 dst, imm_sleb`
- `0x13 const_u64 dst, imm_uleb`
- `0x14 const_f64 dst, bits_u64le`
- `0x15 const_decimal dst, mantissa_sleb, scale_u8`
- `0x16 const_pool dst, idx`
- `0x17 dec_add dst, a, b`
- `0x18 dec_sub dst, a, b`
- `0x19 dec_mul dst, a, b`
- `0x1A f64_add dst, a, b`
- `0x1B f64_sub dst, a, b`
- `0x1C f64_mul dst, a, b`
- `0x82 f64_div dst, a, b`

- `0x20 i64_add dst, a, b`
- `0x21 i64_sub dst, a, b`
- `0x22 i64_mul dst, a, b`
- `0x23 u64_add dst, a, b`
- `0x24 u64_sub dst, a, b`
- `0x25 u64_mul dst, a, b`
- `0x26 u64_and dst, a, b`
- `0x27 u64_or dst, a, b`

- `0x28 i64_eq dst, a, b`
- `0x29 i64_lt dst, a, b`
- `0x2A u64_eq dst, a, b`
- `0x2B u64_lt dst, a, b`
- `0x2C u64_xor dst, a, b`
- `0x2D u64_shl dst, a, b` (shift amount masked with `& 63`)
- `0x2E u64_shr dst, a, b` (shift amount masked with `& 63`)
- `0x2F u64_gt dst, a, b`

- `0x30 bool_not dst, a`
- `0x83 f64_eq dst, a, b` (IEEE: false if NaN)
- `0x84 f64_lt dst, a, b` (IEEE: false if NaN)
- `0x85 f64_gt dst, a, b` (IEEE: false if NaN)
- `0x86 f64_le dst, a, b` (IEEE: false if NaN)
- `0x87 f64_ge dst, a, b` (IEEE: false if NaN)
- `0x88 bool_and dst, a, b`
- `0x89 bool_or dst, a, b`
- `0x8A bool_xor dst, a, b`
- `0x8B f64_neg dst, a`
- `0x8C f64_abs dst, a`
- `0x8D f64_min dst, a, b` (NaN-propagating)
- `0x8E f64_max dst, a, b` (NaN-propagating)
- `0x8F f64_min_num dst, a, b` (number-favoring)
- `0x90 f64_max_num dst, a, b` (number-favoring)
- `0x91 f64_rem dst, a, b`
- `0x92 f64_to_bits dst, a` (`f64` -> `u64`)
- `0x93 f64_from_bits dst, a` (`u64` -> `f64`)
- `0x31 u64_le dst, a, b`
- `0x32 u64_ge dst, a, b`
- `0x33 i64_and dst, a, b`

- `0x34 u64_to_i64 dst, a` (traps on overflow)
- `0x35 i64_to_u64 dst, a` (traps on negative)
- `0x36 i64_or dst, a, b`
- `0x37 i64_xor dst, a, b`

- `0x38 select dst, cond, a, b`
- `0x39 i64_gt dst, a, b`
- `0x3A i64_le dst, a, b`
- `0x3B i64_ge dst, a, b`
- `0x3C i64_shl dst, a, b` (shift amount masked with `& 63`)
- `0x3D i64_shr dst, a, b` (shift amount masked with `& 63`)

- `0x40 br cond, pc_true, pc_false` (PCs are byte offsets)
- `0x41 jmp pc_target`
- `0x42 jmp.args pc_target, args` (`args`: ULEB32 count followed by register ULEB32s)
- `0x43 br.args cond, pc_true, pc_false, true_arg_count, args` (same counted register list;
  split and PCs are ULEB32s)

- `0x50 call eff_out, func_id, eff_in, argc, args..., retc, rets...`
- `0x51 ret eff_in, retc, rets...`
- `0x52 host_call eff_out, host_sig_id, eff_in, argc, args..., retc, rets...`
- `0x53 const_func dst, func_id`
- `0x54 call_indirect eff_out, call_sig_id, callee, eff_in, argc, args..., retc, rets...`
- `0x55 closure_new dst, func, env`

### Aggregates
Construction (all allocate):
- `0x60 tuple_new dst, arity, values...`
- `0x62 struct_new dst, type_id, field_count, values...`
- `0x64 array_new dst, elem_type_id, len, values...` (len must match provided values)

Projection:
- `0x61 tuple_get dst, tuple, index`
- `0x67 tuple_len dst, tuple`
- `0x63 struct_get dst, st, field_index`
- `0x68 struct_field_count dst, st`
- `0x65 array_len dst, arr`
- `0x66 array_get dst, arr, index_reg` (traps on OOB)
- `0x69 array_get_imm dst, arr, index` (traps on OOB)
- `0x6A bytes_len dst, bytes`
- `0x6B str_len dst, s` (length in UTF-8 bytes, not codepoints)

- `0x6C i64_div dst, a, b` (traps on divide-by-zero and `i64::MIN / -1`)
- `0x6D i64_rem dst, a, b` (traps on divide-by-zero and `i64::MIN % -1`)
- `0x6E u64_div dst, a, b` (traps on divide-by-zero)
- `0x6F u64_rem dst, a, b` (traps on divide-by-zero)
- `0x70 i64_to_f64 dst, a`
- `0x71 u64_to_f64 dst, a`
- `0x72 f64_to_i64 dst, a` (truncates; traps on NaN/inf/out-of-range)
- `0x73 f64_to_u64 dst, a` (truncates; traps on NaN/inf/out-of-range/negative)
- `0x74 dec_to_i64 dst, a` (traps if `a.scale != 0` or mantissa is outside `i64`)
- `0x75 dec_to_u64 dst, a` (traps if `a.scale != 0` or mantissa is outside `u64`)
- `0x76 i64_to_dec dst, a, scale_u8` (traps on overflow)
- `0x77 u64_to_dec dst, a, scale_u8` (traps on overflow)
- `0x78 bytes_eq dst, a, b`
- `0x79 str_eq dst, a, b`
- `0x7A bytes_concat dst, a, b`
- `0x7B str_concat dst, a, b`
- `0x7C bytes_get dst, bytes, index_reg` (traps on OOB)
- `0x7D bytes_get_imm dst, bytes, index` (traps on OOB)
- `0x7E bytes_slice dst, bytes, start, end` (traps on invalid range)
- `0x7F str_slice dst, s, start, end` (byte indices; traps on invalid range or non-boundary)
- `0x80 str_to_bytes dst, s`
- `0x81 bytes_to_str dst, bytes` (traps on invalid UTF-8)

There are no aggregate mutation operations.

## Host ABI

### Host symbol + signature identity
Host-call targets are identified by:
- `symbol`: stable UTF-8 string (namespace-qualified)
- `sig_hash`: stable hash of argument and result types

The program stores:
- `symbol_id -> symbol` in the `symbols` section
- `host_sig_id -> (symbol_id, sig_hash, arg_types, ret_types)` in the `host_sigs` section

### Signature hashing
`SigHash` is FNV-1a 64-bit over this canonical byte sequence:
- Prefix bytes `execution_tape:v1\0`, including the terminating zero byte. The `v1` here is the
  hash-domain marker, independent of the program wire version.
- Argument count as `u32le`, followed by each argument's one-byte ValueType tag.
- Result count as `u32le`, followed by each result's one-byte ValueType tag.
- Each `Obj` tag is immediately followed by its `HostTypeId` as `u64le`.

Use offset basis `0xcbf29ce484222325` and prime `0x100000001b3`; for each byte, XOR then multiply
with wrapping 64-bit arithmetic. Effect operands are not signature arguments and add no bytes.
`host::sig_hash` and `sig_hash_slices` implement this encoding.

### Host call interface
The embedder provides a `Host` that can:
- resolve symbols to callable handles (or reject)
- execute a call with:
  - `symbol` + `sig_hash`
  - argument `ValueRef`s (borrowed views; bytes/strings are exposed as `&[u8]`/`&str` to avoid cloning)
  - read-only VM context, including program metadata and tuple/struct/array reads for `Agg` arguments
  - optional access sink for incremental-execution dependency recording
  - ability to charge extra fuel

The embedder owns:
- `Obj` handle creation/lifetimes
- any external resource access
- any in-host caching

Re-entrancy: host calls are non-reentrant (no calling back into the same VM while executing a host call).

## Verification (normative)
Verification must run before execution for serialized programs.

Required checks:
- **Section integrity**: all offsets/lengths in bounds; no overlaps that violate decoding.
- **Function table integrity**: bytecode ranges in bounds; `reg_count` sane; arg/ret counts sane.
- **CFG integrity**: `jmp`/`br` targets in bounds and on instruction boundaries.
- **Register discipline**:
  - init-before-use on reachable paths
  - writes do not exceed `reg_count`
  - reads do not exceed `reg_count`
- **Type discipline**:
  - each opcode’s operands and results match their declared kinds (e.g. `i64_add` requires `I64`)
  - aggregate ops match aggregate kind and type ids
  - `call` matches callee signature counts and types
  - `ret` matches function return types
  - `host_call` matches the `host_sigs[host_sig_id]` types
- **Declaration limit**: `reg_count` is within `VerifyConfig::max_regs_per_function`.
- **Effect threading**:
  - call/host-call `eff_in` and `eff_out` must be r0 (`Unit`, initialized at entry)
  - `ret` must use r0 as its effect operand

### Additional verifier details
These checks apply alongside the structural and contract rules described above.

- **Required sections present**: sections `1..=8` and `12` must exist exactly once; `names` and `call_sigs` are optional.
- **Bytecode decode**: the function bytecode must decode cleanly as a sequence of opcodes.
- **CFG targets**:
  - all `br/jmp` targets must be within `[0, bytecode_len)`
  - all `br/jmp` targets must be an instruction boundary (byte offset that starts an instruction)
- **Reachability**: init-before-use is checked only on reachable blocks.
- **Register bounds**: any register operand must be `< reg_count`.
- **Register convention**:
  - `r0` (effect token) is considered initialized at entry.
  - value arguments are considered initialized at entry in `r1..=r_arg_count`.
- **Init-before-use**: any read of a register not definitely initialized on all paths to that point is rejected.
- **No fallthrough between blocks**: every reachable basic block must end in a terminator (`ret`/`trap`/`br`/`jmp`).
- **Call arity**: `call` must pass exactly `callee.arg_count` value args and list exactly `callee.ret_count` return regs.
- **Return arity**: `ret` must return exactly `func.ret_count` values.
- **HostSig table**:
  - `host_sigs[i].symbol_id` must be in-bounds
  - `host_sigs[i].sig_hash` must match the canonical hash of its `(arg_types, ret_types)`
- **Host call**: `host_call` must reference an in-bounds `host_sig_id` and its args must match that signature's types.

## Implementation authority

[`execution_tape/opcodes.json`](../execution_tape/opcodes.json) defines opcode bytes, operands,
encodings and operand roles. `execution_tape_codegen` generates the opcode/codec/disassembly
machinery from it. Calls and returns use explicit register lists; aggregates use an immutable
arena-backed DAG. Verification validates declared host signatures without resolving host symbols;
`HostFunctions::bind` can resolve all imports before execution, or a custom `Host` can handle them
at runtime. Future capabilities are distinguished from current behavior in the
[overview](overview.md#future-possibilities).
