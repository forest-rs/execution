# `execution_tape` v1 specification (draft)

This document is the concrete v1 spec to unblock implementation. It is intentionally biased toward a small, verifiable, `no_std + alloc` core.

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
- Domain node definitions (pricing, mesh generation, layout, etc.).
- Host object semantics beyond an opaque handle contract.

## Goals
- Fast execution for mostly-static programs (hundreds of nodes/opcodes).
- Portable bytecode with stable SpanIds.
- Verifiable and bounded execution (fuel / depth / host call limits).
- Trap-first error model for v1.
- Structural (deep) aggregates: `Tuple`, `Struct`, `Array`, immutable and acyclic.

## Non-goals (v1)
- Closures/captures (first-class function refs only).
- Multi-lane effects (single linear effect token only).
- In-VM recoverable errors (`Result`/`Option` values).
- Incremental recompute (pure-region caching is a v2+ topic).

## Terminology
- **Value**: runtime datum computed by the VM. Some values (notably `Bytes`/`Str`) may be stored indirectly by
  handle into a per-run arena while preserving the same observable semantics.
- **Register**: a virtual slot; bytecode operands refer to registers by index.
- **Frame**: one function activation containing `pc`, `base`, and per-frame register count.
- **Program Counter (pc)**: byte offset into a function’s bytecode stream.
- **SpanId**: stable identifier for tracing/source mapping.
- **Host symbol**: stable identifier for a host-provided function.
- **Sig hash**: hash of the host call signature (arguments and results).

## Determinism and portability
- Numeric: `f64` follows platform IEEE behavior; float nondeterminism is acceptable.
- Portable serialization: canonical little-endian, varint encoding for indices/immediates.
- No pointer serialization. Host objects (`Obj`) are non-serializable by default.

## Value model

### Value kinds
The VM defines a closed set of value kinds. v1 requires these kinds:
- `Unit`
- `Bool`
- `I64`
- `U64`
- `F64`
- `Decimal` (`mantissa: i64`, `scale: u8`)
- `Bytes` (alloc-backed)
- `Str` (alloc-backed, UTF-8)
- `Obj` (opaque handle)
- `Agg` (aggregate handle: tuple/struct/array heap node)
- `Func` (function reference)

Implementation note: the VM may store registers in per-kind register files (`RegClass`) rather than as a tagged
union `Value` in order to avoid runtime tag checks in the hot interpreter loop. This is safe because execution
requires verification, and the verifier assigns each register a stable kind and enforces type correctness.

### `Decimal`
- Per-value scale (`u8`) and mantissa (`i64`).
- v1 arithmetic requirements:
  - `dec_add`, `dec_sub`, `dec_mul` are defined only when the result scale is well-defined (e.g. add/sub require equal scale; mul defines scale = a.scale + b.scale).
  - Division and rounding are host-provided in v1.

### Host objects (`Obj`)
`Obj` is an opaque value:
- `host_type: HostTypeId` (stable within the embedder)
- `handle: u64` (embedder-defined identity)

Constraints:
- No VM-level equality/hash/ordering in v1.
- Any attempt to serialize an `Obj` traps.
- Host calls may accept/return `Obj` values, and the verifier checks that host signatures match.

### Aggregates (`Agg`)
Aggregates are immutable, acyclic, and structural.

Kinds:
- `Tuple([Value])` (heterogeneous, fixed arity)
- `Struct(TypeId, [Value])` (fields in the order defined by `TypeId`)
- `Array(ElemTypeId, [Value])` (homogeneous)

Constraints:
- No cycles in v1. Construction must not create cycles; deserialization must reject cycles.
- Structural equality/hash are not required as v1 opcodes, but the representation must make them well-defined for v2.
- Aggregates are serializable if (and only if) all contained values are serializable.

## Type model

### Program type table
The program contains a type table defining:
- Builtin types (well-known ids).
- Struct layouts (`TypeId` → ordered field list and field types).
- Array element types (`ElemTypeId`).

The verifier uses the type table to validate aggregate ops and (optionally) to type-check non-host bytecode more strongly.

v1 note: the runtime may still be dynamically typed for primitives, but aggregate ops must be type-correct.

## Effects model
v1 uses a single linear effect token `eff` to sequence side effects.

Semantics:
- Pure ops do not mention `eff`.
- `host_call` consumes an `eff_in` register and produces an `eff_out` register.
- Programs must thread `eff` through all effectful operations; the verifier enforces this by requiring `host_call` to take/return `eff` and by checking init-before-use.

`eff` is represented at runtime as `Unit`; it exists for verification/ordering only.

## Control flow and functions

### Functions
Programs contain a function table; each function entry includes:
- `FuncId`
- `arg_count` (value args; effect is threaded separately)
- `ret_count` (value returns)
- `reg_count` (total registers used by the function)
- `bytecode_index` (index into the bytecode blob list)
- `span_table_index` (index into the span table list)

### Calls and frames
The VM maintains a call stack of frames:
- `func_id`
- `pc` (bytecode index within the function)
- `base` (start index into the global register file)

Registers are frame-local via `base + reg_index`.

Recursion is allowed. Limits enforce max depth.

### Control flow invariants
- All jump targets must point to instruction boundaries.
- Structured blocks are not required, but all target pcs must be within the current function.
- IR may use `phi`, but lowering must remove `phi` by inserting edge copies; bytecode has no `phi`.

## Limits and traps

### Limits (v1)
- `fuel`: decremented by a per-instruction cost (default 1 unless overridden by opcode).
- `max_call_depth`: hard stack limit.
- `max_host_calls`: caps `host_call` count.
- optional `max_regs_total`: caps `base + reg_count` growth to avoid memory blowups.

### Traps (v1)
Traps abort execution and return an error containing:
- trap code (enum)
- current `FuncId`
- current `pc`
- best-effort `SpanId` resolved from span table (if available)
- optional host symbol (for host-call failures)

Trap codes (minimum):
- `InvalidProgram` (failed verification; not a runtime trap)
- `TypeMismatch`
- `UninitializedRegister`
- `OutOfBounds`
- `InvalidJumpTarget`
- `FuelExceeded`
- `CallDepthExceeded`
- `HostCallLimitExceeded`
- `HostCallFailed`
- `SerializationEncounteredObj`

## Bytecode encoding

### Canonical integer encoding
- All multibyte integers are little-endian.
- Indices and small immediates use unsigned LEB128 (ULEB128).
- Signed immediates (if present) use signed LEB128 (SLEB128).
- Decoders must accept non-canonical (non-minimal) LEB128 encodings, as long as they decode to the same integer value.
  - Rationale: some producers (including the provided bytecode builder) reserve fixed-width windows
    for later patching (e.g. branch/jump targets) to avoid shifting the byte stream.

### Instruction encoding (v1)
Each instruction is:
- `opcode: u8`
- `operands: varint-encoded fields` (register indices, immediates, ids)

Register operands are ULEB128 indices (`Reg`).

### Program counter (PC)
For control-flow instructions, `pc_*` operands are **byte offsets** into the function bytecode stream, and must
point at an instruction boundary (or the end of the stream where allowed).

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
- `span_id: ULEB128` (or fixed-width if needed)

Meaning: starting at `pc += pc_delta`, current span becomes `span_id` until the next entry.

SpanId stability is the compiler’s responsibility (typically derived from stable graph node GUIDs).

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

Unknown section tags are skipped by decoders (forward-compat). Known sections are rejected if duplicated.

## Tracing (hooks)

The VM can optionally emit tracing events to a sink.

v1 is intended to support:
- run start/end events
- instruction step events
- scope events for profiling:
  - call frames (function enter/exit)
  - host calls (time spent in embedder code)

The tracing API is designed so that profiler integrations (e.g. using the ecosystem `profiling`
crate) can be built as adapters without adding any dependency to the `execution_tape` core crate.

### Section tags (v1)
- `1 = symbols`
- `2 = const_pool`
- `3 = types`
- `4 = function_table`
- `5 = bytecode_blobs`
- `6 = span_tables`
- `7 = function_sigs`
- `8 = host_sigs`

All of the above sections are required in v1.

## Symbols
The symbol table stores host-call targets:
- `count: ULEB128`
- repeated `count` times:
  - `len: ULEB128`
  - `utf8_bytes[len]`

## Constant pool
The program constant pool stores immutable literals referenced by index:
- `I64/U64/F64/Bool/Unit/Decimal`
- `Bytes` (length + bytes)
- `Str` (length + UTF-8 bytes)

Aggregates are not required to be in the constant pool in v1 (they may be constructed at runtime), but may be added later.

## Types
The type table defines struct layouts and array element types.

### ValueType encoding (v1)
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

### Struct types
- `struct_count: ULEB128`
- repeated `struct_count` times:
  - `field_count: ULEB128`
  - repeated `field_count` times:
    - `name_len: ULEB128`
    - `name_utf8[name_len]`
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
  - `bytecode_index: ULEB128`
  - `span_table_index: ULEB128`

## Function signatures (typed)
Typed signatures are mandatory in v1 and stored separately from `function_table`:
- `count: ULEB128` (must match `function_table.count`)
- repeated `count` times:
  - `arg_count: ULEB128`
  - `arg_types[arg_count]: ValueType...`
  - `ret_count: ULEB128`
  - `ret_types[ret_count]: ValueType...`

The `arg_count`/`ret_count` must match the corresponding `function_table` entry.

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

## Opcode set (v1)
This is the minimal set to support loops + recursion + host calls + aggregates.

### Conventions
- `rX` denotes a register index.
- All destination registers are written exactly once per dynamic execution of the instruction.
- Unless specified, each instruction costs 1 fuel.
- **Register convention (v1)**:
  - `r0` is the effect token register.
  - Value arguments (for a function with `arg_count = N`) are in `r1..=rN`.
  - Other registers are temporaries and/or outputs.

### Core
- `nop`
- `mov r_dst, r_src`
- `trap trap_code`

### Constants
- `const_unit r_dst`
- `const_bool r_dst, imm_bool`
- `const_i64 r_dst, imm_sleb`
- `const_u64 r_dst, imm_uleb`
- `const_f64 r_dst, imm_f64_le`
- `const_decimal r_dst, mantissa_sleb, scale_u8`
- `const_pool r_dst, const_index` (typed by verifier/const tag)

### Numeric (subset; expand later)
- `i64_add r_dst, r_a, r_b`
- `i64_sub r_dst, r_a, r_b`
- `i64_mul r_dst, r_a, r_b`
- `f64_add r_dst, r_a, r_b`
- `f64_sub r_dst, r_a, r_b`
- `f64_mul r_dst, r_a, r_b`
- `dec_add r_dst, r_a, r_b`
- `dec_sub r_dst, r_a, r_b`
- `dec_mul r_dst, r_a, r_b`

### Comparisons + branching
- `i64_eq r_dst, r_a, r_b` -> `Bool`
- `i64_lt r_dst, r_a, r_b` -> `Bool`
- `bool_not r_dst, r_a`
- `bool_and r_dst, r_a, r_b`
- `bool_or r_dst, r_a, r_b`
- `bool_xor r_dst, r_a, r_b`
- `br r_cond, pc_true, pc_false`
- `jmp pc_target`

### Calls
- `call r_eff_out, func_id, r_eff_in, args... -> rets...`
  - The callee function signature defines arg/ret counts.
  - Call threads `eff` explicitly (even for pure callees, `eff` is forwarded unchanged).
  - Fuel cost: base 1 + (optional) per-arg/ret cost (tbd).
- `ret r_eff_out, r_eff_in, rets...`
  - Returns values to caller-provided return registers.

### Host calls
- `host_call r_eff_out, host_sig_id, r_eff_in, args... -> rets...`
  - Increments host-call count; traps if limit exceeded.
  - Host returns:
    - either success with return values and optional additional fuel cost
    - or failure, which becomes `HostCallFailed` trap

## Draft encoding for minimal implemented opcodes
This section documents the encoding currently implemented by the verifier decoder (subject to change).

The single authoritative mapping between instruction names and their opcode bytes lives in
`execution_tape/src/opcode.rs` as `Opcode`.

All register indices and small integers are ULEB128 unless noted.

- `0x00 nop`
- `0x01 mov dst, src`
- `0x02 trap code`

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

- `0x50 call eff_out, func_id, eff_in, argc, args..., retc, rets...`
- `0x51 ret eff_in, retc, rets...`
- `0x52 host_call eff_out, host_sig_id, eff_in, argc, args..., retc, rets...`

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
- `0x74 dec_to_i64 dst, a` (traps if `a.scale != 0`)
- `0x75 dec_to_u64 dst, a` (traps if `a.scale != 0` or `a.mantissa < 0`)
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

v1 note: no mutation ops.

## Host ABI

### Host symbol + signature identity
Host-call targets are identified by:
- `symbol`: stable UTF-8 string (namespace-qualified)
- `sig_hash`: stable hash of argument and result types

The program stores:
- `symbol_id -> symbol` in the `symbols` section
- `host_sig_id -> (symbol_id, sig_hash, arg_types, ret_types)` in the `host_sigs` section

### Signature hashing (draft)
Define `sig_hash = hash64("execution_tape:v1" ++ encode(sig))` where `encode(sig)` is a canonical byte encoding of:
- arg count and tags (including aggregate type ids where relevant)
- ret count and tags
- effect token presence (always present in v1 host calls)

Hash function should be stable and `no_std` friendly (e.g. a fixed, specified hash like xxhash64 or a small bespoke hash).

### Host call interface (conceptual)
The embedder provides a `Host` that can:
- resolve symbols to callable handles (or reject)
- execute a call with:
  - `symbol` + `sig_hash`
  - argument `Value`s (passed as borrowed views; bytes/strings are exposed as `&[u8]`/`&str` to avoid cloning)
  - access to tracing sink
  - ability to charge extra fuel

The embedder owns:
- `Obj` handle creation/lifetimes
- any external resource access
- any in-host caching

Re-entrancy: v1 host calls must be non-reentrant (no calling back into the same VM while executing a host call).

## Verification (normative)
Verification must run before execution for serialized programs.

Required checks:
- **Section integrity**: all offsets/lengths in bounds; no overlaps that violate decoding.
- **Function table integrity**: bytecode ranges in bounds; `reg_count` sane; arg/ret counts sane.
- **CFG integrity**: `jmp`/`br` targets in bounds and on instruction boundaries.
- **Register discipline**:
  - init-before-use for all regs
  - writes do not exceed `reg_count`
  - reads do not exceed `reg_count`
- **Type discipline** (minimum):
  - each opcode’s operands have required runtime tags (e.g. `i64_add` requires `I64`)
  - aggregate ops match aggregate kind and type ids
  - `call` matches callee signature counts and types
  - `ret` matches function return types
  - `host_call` matches the `host_sigs[host_sig_id]` types
- **Limits sanity**: `reg_count`, instruction count, and constant sizes within configured verifier maxima.
- **Effect threading**:
  - `host_call` and `call` must consume initialized `eff_in`
  - `ret` must return an initialized `eff` register

### Implemented verifier rules (v1, current)
This subsection is a concrete subset that is implemented today and should stay stable unless the bytecode
encoding or register conventions change.

- **Required sections present**: all v1 sections must exist exactly once (including `function_sigs` and `host_sigs`).
- **Bytecode decode**: the function bytecode must decode cleanly as a sequence of opcodes.
- **CFG targets**:
  - all `br/jmp` targets must be within `[0, bytecode_len]`
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

## Open items to resolve during implementation
- Finalize opcode numbers and exact operand encodings.
- Decide whether `call`/`ret` use explicit arg/ret register lists vs implicit fixed conventions.
- Choose aggregate heap strategy (`Rc`-like vs arena) under `no_std + alloc`.
- Decide whether verifier requires host symbol resolution at verify time or allows unresolved symbols until runtime.
