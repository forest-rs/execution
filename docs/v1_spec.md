# `execution_tape` bytecode and execution specification

This document describes wire format 0.6 and the execution contract of the Rust 0.1 API.
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

### Installed units
A VM executes programs it has installed. Installing a verified program creates a *unit*: the
program plus VM-local metadata (its struct/array shapes, canonical contracts and signatures). Units
are never removed, and every install of a program is a distinct unit. A function value is a
`FuncRef { unit, func }`: `func` is local to the unit's program, and the unit's program runs it,
whichever unit called it. Each activation runs the unit it entered; installing new code changes no
running or suspended activation. Independently compiled units call each other through function
values, which a host may supply (for example from a language-owned method table).

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
- Integer and `Decimal` arithmetic is exact or wraps, traps and rounds as each instruction
  specifies, identically everywhere.
- Floating point follows the contract below.
- Portable serialization: canonical little-endian, varint encoding for indices/immediates.
- No pointer serialization or serialization of external host-object state.

### Floating-point determinism
This contract is normative. The per-instruction notes later in this document describe which
result each instruction produces; this section says how far that result can be relied on.

- **Bit-exact everywhere.** Every `f32` and `f64` instruction produces the same bits on every
  supported platform: Linux, macOS, Windows and WebAssembly, on x86-64, AArch64 and other
  targets. This covers `add`, `sub`, `mul`, `div` and `sqrt` (correctly rounded: to nearest,
  ties to even), `rem` (the exact remainder of truncating division), `floor`, `ceil`, `trunc`,
  `nearest` and `round_half_away` (exact), `min`, `max`, `min_num`, `max_num` and `clamp`,
  `neg`, `abs` and `copysign`, the comparisons, every conversion between `f32`, `f64` and the
  integer types (a float result rounds once, to nearest with ties to even; an integer result
  follows [Float-to-integer conversions](#float-to-integer-conversions)), and the math
  intrinsics `pow`, `exp`, `ln`, `sin`, `cos` and `atan2` in both widths, whose results are
  defined by a pinned `libm` rather than by correct rounding (see
  [Math intrinsics](#math-intrinsics)). Subnormal inputs and results are honored, never flushed
  to zero. No instruction converts between `Decimal` and a float.
- **No contraction.** Each instruction rounds its own result in its own width. A multiply
  followed by an add is never fused into a multiply-add, and no sequence of instructions is
  evaluated at another precision.
- **NaN results.** Whether an arithmetic result is NaN is guaranteed; its sign and payload
  bits are unspecified. Arithmetic here is every instruction above except `neg`, `abs`,
  `copysign`, `f32.to_bits`/`f32.from_bits` and `f64_to_bits`/`f64_from_bits`, which only move
  or change the sign bit. Those, constants, `mov`, `select`, calls and returns, host-call
  arguments and results, aggregates, `Value`/`ValueRef`/`RecordedValue` round trips, recording
  and replay, and the wire encoding all preserve every bit, signaling NaNs included. A program
  that needs particular NaN bits writes them with `from_bits`.
- **Scope.** The contract assumes IEEE 754 binary arithmetic in the default floating-point
  environment: round to nearest and no flush-to-zero or denormals-are-zero mode. A host that
  changes that environment while the VM runs is outside it, as are targets whose Rust float
  arithmetic is not IEEE 754 (x87-only `i586`). The `f32` and `f64` math intrinsics are inside
  the contract; every other transcendental function (`tan`, `exp2`, `log10`, ...) is a host
  call, which the contract does not cover.
- **Implementations.** The interpreter uses native IEEE `add`, `sub`, `mul` and `div` and
  Rust's `as` conversions, whose rounding and saturation Rust defines exactly. It computes
  `rem`, `sqrt`, rounding, `min`/`max` and `clamp` in the core itself, without `std` or `libm`,
  and the math intrinsics with the pinned `libm`'s pure-Rust routines, so no platform math
  library is involved. `execution_tape::math` exposes each of these as a plain function. Any
  other execution engine, such as a future compiled (for example Cranelift) backend, must honor
  this same contract: it may use native instructions wherever their results agree, must compute
  the math intrinsics with the same functions, and need not canonicalize NaNs. The interpreter
  currently quiets a NaN input of the rounding operations, `sqrt`, `rem` and width conversions
  and keeps its sign and payload, and `clamp` returns a NaN `a` unchanged; that is not part of
  the contract.
- **Enforcement.** `execution_tape_conformance/tests/float_determinism.rs` pins the edge cases
  (ties, signed zeros, subnormals, saturation, float-to-integer range edges, `rem` signs, NaN
  propagation and preserved NaN bits) to literal bit patterns and runs on every CI platform,
  including `wasm32-wasip1`.

## Value model

### Value kinds
The VM defines a closed set of value kinds:
- `Unit`
- `Bool`
- `I64`
- `U64`
- `F64`
- `I32`
- `U32`
- `F32`
- `Decimal` (`mantissa: i128`, `scale: u8`)
- `Bytes` (alloc-backed)
- `Str` (alloc-backed, UTF-8)
- `Obj` (opaque handle)
- `Agg` (aggregate handle: tuple/struct/array heap node)
- `Func` (function reference)
- `Closure` (function reference + captured environment aggregate)
- `Exn` (a caught exception: tag, payload and original throw site)

`Exn` exists only in guest registers and guest-to-guest signatures. It has no `Value`
representation; host signatures, invocation entry signatures (including guest callbacks), tag
payloads, struct fields, array elements and tuples cannot contain it. Entering a function whose
signature contains `Exn` from outside the guest traps with `ExceptionBoundary`, and observers see
`Exn` registers as unavailable.

The VM stores payload-bearing registers in per-kind banks (`RegClass`), rather than as tagged
`Value`s. The producer declares each register's kind, and verification enforces it before preparing
typed operands. Unit/effect registers have no runtime payload bank. `I32`, `U32` and `F32`
registers share one bank of 32-bit slots; the verified kind decides how a slot's bits are read.

The 32-bit kinds are distinct from their 64-bit counterparts: no instruction mixes widths or
signedness, and the VM never promotes implicitly. Producers convert explicitly, so a language's
promotion rules live in its compiler.

### `Decimal`
- Per-value scale (`u8`) and mantissa (`i128`), including the full signed range.
- Construction preserves scale; equality distinguishes `1.0` and `1.00` (including differently
  scaled zeros). No implicit normalization or rounding is performed; `dec_cmp` compares numeric
  values instead.
- Decimal literal mantissas use signed LEB128, bounded to 19 bytes. Scale is one raw byte.
  The in-memory constant pool packs each decimal into 17 bytes in the shared blob arena;
  `Program::const_decimal` reads it without growing other constant entries.
- Arithmetic:
  - `dec_add`/`dec_sub` are exact at scale `max(a.scale, b.scale)`: the operand with the smaller
    scale is multiplied by a power of ten first. `dec_mul` is exact at scale `a.scale + b.scale`.
  - `dec_rescale` and `dec_div` produce a caller-chosen scale and are the only operations that
    round, by an explicit rounding mode (see "Decimal semantics").
  - Every operation traps with `DecimalOverflow` when its exact (or correctly rounded) result
    mantissa is outside `i128` or its result scale exceeds 255. Other rounding policies, and
    operations such as square roots, remain host-provided.

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

Struct and array identity at runtime is structural. Installing a program in a VM interns each
`TypeId` (field names and kinds) and `ElemTypeId` (element kind) as a shape of that VM's heap, and
heap nodes record the shape. Equal layouts from independently compiled programs are therefore the
same runtime type, and a program-local ID never names another program's layout.

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

Recursion is allowed, subject to `Limits::max_call_depth`. Function references name their unit and
nominal type IDs are resolved through the installed unit; unit IDs and aggregate handles are local
to one VM. The graph adapter's owned `TapeValue` snapshots retain their program and name its unit
in whichever executor imports them.

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

- `fuel`: each executable instruction costs one; `dec_div` charges its long-division steps and
  successful host calls may charge additional fuel.
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

`Uncaught` reports a guest exception raised by `throw` that left the invocation. It is a guest
outcome rather than a VM failure: it carries the tag, its name, and an owned `ValueSnapshot` of
the payload taken before rollback. See [Exceptions](#exceptions).

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
- `opcode`: one byte, or a prefix byte followed by one byte
- `operands: varint-encoded fields` (register indices, immediates, ids)

Register operands are ULEB128 indices (`Reg`).

#### Opcode prefixes
Lead bytes `0xF0..=0xFF` are reserved for prefixes and never name a single-byte opcode. An
assigned prefix is always followed by exactly one more byte, which selects an opcode within that
prefix's space, so each prefix opens 256 opcodes. Both bytes are fixed-width: there is no varint
form and no second spelling of an opcode, so an opcode's bytes identify it exactly and
`codec::canonicalize` never rewrites them. Currently assigned:

- `0xFC` (`OpcodePrefix::Extended`): general extension space. Assigned so far: `u64` bit
  operations from `0x00`, stack switching from `0x05`, `f64` exact operations (`round_half_away`,
  `clamp`) from `0x0B`, and the `f64` math intrinsics in the block `0x10..=0x1F`, in the same
  order as the `f32` ones at `0xFD 0xA0` (`0xFC 0x10` is `f64.pow` as `0xFD 0xA0` is
  `f32.pow`). New instruction families take the next free byte outside that block.
- `0xFD` (`OpcodePrefix::Numeric32`): 32-bit numerics and conversions, in aligned blocks
  (`i32` from `0x00`, `u32` from `0x40`, `f32` from `0x80` with its math intrinsics from `0xA0`,
  conversions from `0xC0`).

Every opcode has a 16-bit *code*: its byte for single-byte opcodes, and `prefix << 8 | byte` for
prefixed ones (`0xFC 0x05` is code `0xFC05`). Traces, recordings and diagnostics report opcodes by
code (`Opcode::code`, `Opcode::from_code`).

Decoding rejects:
- a lead byte that is neither a single-byte opcode nor an assigned prefix (including the
  unassigned bytes of the reserved range), reported as its one-byte code;
- an assigned prefix followed by a byte that names no opcode, reported as the two-byte code;
- a prefix with no following byte, as truncated bytecode.

The verifier reports the first two as `VerifyError::UnknownOpcode { func, pc, opcode }` and the
last as `BytecodeDecode`.

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

The header must match version 0.6 exactly. Within that version, unknown section tags are skipped;
this does not imply compatibility with future wire versions. Known sections are rejected if duplicated.

## Tracing (hooks)

The VM can optionally emit tracing events to a sink.

The tracing API supports:
- run start/end events
- instruction step events
- scope events for profiling:
  - call frames (function enter/exit)
  - host calls (time spent in embedder code)

Instruction events carry the opcode's code (see [Opcode prefixes](#opcode-prefixes)), so
prefixed instructions are distinguishable from their prefix.

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
- `13 = tags` (optional)
- `14 = regions` (optional)
- `15 = tag_imports` (optional)
- `16 = tag_results` (optional)

Sections `1..=8` and `12` are required; `9 = names`, `10 = call_sigs`, `13 = tags`,
`14 = regions`, `15 = tag_imports` and `16 = tag_results` are optional (each omitted entirely when empty). Unknown section tags are skipped within the accepted wire version.

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
- `12 Exn`
- `13 I32`
- `14 U32`
- `15 F32`
- `16 Cont`

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

The current wire version is 0.6. Older versions are rejected; rebuild serialized programs
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
preserve the nominal element kind. These rules apply even to unused definitions. Verification
keeps distinct IDs distinct even when their layouts match; this is conservative, because runtime
checks compare interned shapes, under which equal layouts are the same type. Recursive functions can use finite contract DAGs;
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

## Tags (optional)
Exception tags are nominal, program-local identities referenced by `throw`. A `TagId` is the index
into this table. Two tags with equal payload contracts are still different tags. The section is
omitted when the program declares no tags:
- `count: ULEB128`
- repeated `count` times:
  - `name_symbol_id: ULEB128` (non-zero; names are unique within the program)
  - `payload_count: ULEB128`
  - `payload_contract_ids[payload_count]: ULEB128...`

Payload contracts use the shared contract table, like call signatures. Decoding packs them into
`Program::contract_ids` after call signatures.

Installing a program in a VM binds each tag to a VM tag (`TagRef`). A tag the program declares
becomes a new VM tag for every install, so two installs of one program throw distinct exceptions.
A tag listed in `tag_imports` instead binds to the VM tag defined under its name
(`Vm::define_tag`), whose payload contracts must equal the declared ones after canonicalisation;
otherwise the install fails with `UnresolvedTag` or `TagPayloadMismatch` and creates no unit
or tag.

## Tag results (optional)
Declares the values that resume a suspension to a tag, for `suspend` and the continuation it
creates. A tag without an entry resumes with no values; `throw` ignores results. The section is
omitted when no tag has results:
- `count: ULEB128`
- repeated `count` times:
  - `tag_id: ULEB128` (strictly increasing; each names an entry of `tags`)
  - `result_count: ULEB128`
  - `result_contract_ids[result_count]: ULEB128...`

Result contracts are packed into `Program::contract_ids` right after their tag's payload. An
imported tag's results must equal the defined VM tag's results, like its payload. Neither payloads
nor results may contain `Exn` or `Cont`.

## Tag imports (optional)
Marks tags of the `tags` section as imported rather than defined. The section is omitted when
the program defines all its tags:
- `count: ULEB128`
- `tag_ids[count]: ULEB128...` (strictly increasing; each names an entry of `tags`)

An imported tag keeps its `tags` entry: its name is the import key, and its payload contracts
are both the import's expected payload and the contracts `throw` checks in this program.

## Regions (optional)
Protected regions attach catch clauses to instructions. Each function owns a region tree and a
table of runs that assigns instruction ranges to their innermost region. Runs need not be
contiguous per region, so frontends may lay out protected code freely. Functions without
protection are omitted:
- `count: ULEB128` (functions with protection, in strictly ascending `func_id` order)
- repeated `count` times:
  - `func_id: ULEB128`
  - `region_count: ULEB128`
  - repeated `region_count` times:
    - `parent_plus_one: ULEB128` (`0` for a root region; otherwise an earlier region + 1)
    - `clause_count: ULEB128`
    - repeated `clause_count` times:
      - `kind: u8` (`0 = catch tag`, `1 = catch_all`, `2 = catch_ref tag`, `3 = catch_all_ref`,
        `4 = on tag`)
      - `tag_id: ULEB128` (only for `catch tag`, `catch_ref tag` and `on tag`)
      - `target_pc: ULEB128` (handler block header)
  - `run_count: ULEB128`
  - repeated `run_count` times:
    - `start_pc: ULEB128`, `end_pc: ULEB128`, `region: ULEB128`

Runs are sorted, disjoint, non-empty and start and end at instruction boundaries (or the
bytecode end). A `catch tag` target declares one block parameter per payload value, with matching
kinds; a `catch_all` target declares none. The `_ref` clauses append one final `Exn` parameter
that receives the exception itself. An `on tag` clause handles suspensions instead of exceptions
(see [Stack switching](#stack-switching)); its target declares the payload parameters followed by
one `Cont` parameter receiving the suspended continuation.

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
- Unless specified, each instruction costs 1 fuel. `dec_div` costs more when its quotient needs
  long division beyond 128 bits (see decimal semantics).
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
- `i64_add_ovf`/`i64_sub_ovf`/`i64_mul_ovf r_dst, r_overflow, r_a, r_b` -> `I64`, `Bool`
- `u64_add_ovf`/`u64_sub_ovf`/`u64_mul_ovf r_dst, r_overflow, r_a, r_b` -> `U64`, `Bool`
- `u64_clz`/`u64_ctz`/`u64_popcnt r_dst, r_a` -> `U64` (leading zeros, trailing zeros and set
  bits; `clz` and `ctz` of zero are 64)
- `u64_rotl`/`u64_rotr r_dst, r_a, r_b` (rotation amount masked with `& 63`)
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
- `f64_floor r_dst, r_a` (round toward negative infinity)
- `f64_ceil r_dst, r_a` (round toward positive infinity)
- `f64_trunc r_dst, r_a` (round toward zero)
- `f64_nearest r_dst, r_a` (round to nearest integer, ties to even)
- `f64_sqrt r_dst, r_a`
- `f64_copysign r_dst, r_a, r_b` (magnitude of `a`, sign of `b`)
- `f64_to_i64_sat r_dst, r_a` -> `I64` (saturating; never traps)
- `f64_to_u64_sat r_dst, r_a` -> `U64` (saturating; never traps)
- `dec_add r_dst, r_a, r_b`
- `dec_sub r_dst, r_a, r_b`
- `dec_mul r_dst, r_a, r_b`
- `dec_div r_dst, r_a, r_b, r_scale, rounding_u8`
- `dec_rescale r_dst, r_a, r_scale, rounding_u8`
- `dec_scale r_dst, r_a` -> `U64`

### 32-bit numerics
`i32`, `u32` and `f32` mirror the 64-bit families: wrapping `add`/`sub`/`mul`, `_ovf` forms
returning the wrapped value and a `bool` flag, `div`/`rem` that trap with `DivByZero` (and
`IntDivOverflow` for `i32::MIN / -1` and `i32::MIN % -1`, as for `i64`), bitwise `and`/`or`/`xor`,
shifts masked with `& 31` (`i32.shr` arithmetic, `u32.shr` logical), comparisons to `bool`, and
`u32.clz`/`ctz`/`popcnt`/`rotl`/`rotr` (`clz`/`ctz` of zero are 32). `f32` has the same
operations as `f64` with the same NaN, signed-zero and rounding rules: `add`/`sub`/`mul`/`div`
are IEEE single precision; `rem` is the exact remainder; `floor`, `ceil`, `trunc`, `nearest`,
`round_half_away` and `sqrt` are computed exactly or correctly rounded through the `f64` routines
(exact rounding of an `f32` is representable as `f32`, and `f64` carries more than `2 * 24 + 2`
significand bits, so rounding the correctly rounded `f64` square root once to `f32` is correctly
rounded). No instruction mixes
widths or signedness; conversions are explicit:

- `i32.to_i64`, `u32.to_u64`, `u32.to_i64`, `i32.to_f64`, `u32.to_f64`, `f32.to_f64`: exact.
- `i64.to_i32`, `u64.to_u32`, `i32.to_u32`, `u32.to_i32`: trap with `IntCastOverflow` when the
  value does not fit.
- `i64.to_i32_wrap`, `u64.to_u32_wrap`: keep the low 32 bits. `i32.to_u32_wrap`,
  `u32.to_i32_wrap`, `i64.to_u64_wrap`, `u64.to_i64_wrap`: reinterpret the bits.
- `f64.to_f32`, `i32.to_f32`, `u32.to_f32`, `i64.to_f32`, `u64.to_f32`: round once to nearest,
  ties to even (`f64.to_f32` overflows to infinity).
- `f32.to_i32`, `f32.to_u32`, `f64.to_i32`, `f64.to_u32` and their `_sat` forms: truncate,
  then trap or saturate; see [Float-to-integer conversions](#float-to-integer-conversions).
- `f32.to_bits`/`f32.from_bits`: raw IEEE bits to and from `u32`.

`f32.round_half_away` and `f64.round_half_away` round like `nearest` but break ties away from zero
(see floating-point semantics). `f32.clamp` and the `f32` transcendental functions are described
under [Math intrinsics](#math-intrinsics).

Width conversions of a NaN produce a NaN; `f32.to_bits`, `f32.from_bits`, `neg`, `abs`,
`copysign` and `const.f32` never alter NaN bits (see
[Floating-point determinism](#floating-point-determinism)).
Decimal has no 32-bit conversions; go through `i64`/`u64`.

#### Integer overflow semantics
- `i64_add`/`i64_sub`/`i64_mul` (and the `u64` forms) wrap modulo 2^64 without reporting overflow.
- The `_ovf` forms write the same wrapped result to `r_dst` and, to `r_overflow`, whether the exact
  result was outside the operand type's range: signed overflow for `i64` and carry/borrow/overflow
  for `u64`. They match Cranelift `sadd_overflow`/`ssub_overflow`/`smul_overflow` and
  `uadd_overflow`/`usub_overflow`/`umul_overflow`, and never trap. A frontend with unbounded
  integers uses them as its fast path, branching on the flag to promote or signal.

#### Decimal semantics
- `r_scale` is a `U64` register holding the result scale; a value above 255 traps with
  `DecimalOverflow`.
- `rounding_u8` is an immediate rounding mode; the verifier rejects other values:
  - `0` half-even: to nearest, exact ties to the even neighbor (`2.5 -> 2`, `-3.5 -> -4`);
  - `1` half-away-from-zero: to nearest, exact ties away from zero (`2.5 -> 3`, `-2.5 -> -3`);
  - `2` toward-zero: truncate (`2.7 -> 2`, `-2.7 -> -2`);
  - `3` floor: toward negative infinity (`-2.1 -> -3`);
  - `4` ceiling: toward positive infinity (`2.1 -> 3`).
- `dec_div` computes the exact quotient `a / b` and rounds it once, by the mode, to `r_scale`
  fractional digits. The operand scales do not constrain the result scale (`1.5 / 0.25` at scale
  0 is `6`). A zero divisor traps with `DivByZero`; a rounded result outside `i128` traps with
  `DecimalOverflow`.
- `dec_div` fuel follows its work. The numerator is `|a.mantissa| × 10^e` for
  `e = r_scale + b.scale - a.scale` when `e >= 0`, and the divisor `|b.mantissa| × 10^-e`
  otherwise. When both fit in 128 bits, or the numerator is smaller than the divisor, the division
  costs the ordinary one fuel. Otherwise it runs binary long division with one step per numerator
  bit and costs one fuel plus that bit length (129 to 256 more). The extra fuel is charged after
  `DivByZero` and numerator-overflow checks and before dividing; when too little remains, the
  instruction traps with `FuelExceeded` and debits no extra fuel.
- `dec_rescale` returns `a` at `r_scale`: exactly when the scale does not decrease (trapping with
  `DecimalOverflow` if the mantissa overflows), otherwise rounded once by the mode.
- `dec_cmp` returns `-1`, `0` or `1` comparing numeric values, ignoring scale (`1.0` and `1.00`
  compare equal, as do zeros of any scale). `dec_scale` returns `a.scale`.

#### Floating-point semantics
- `f64_min`/`f64_max` propagate NaN: if either input is NaN, the result is NaN.
- `f64_min_num`/`f64_max_num` are number-favoring: if exactly one input is NaN, the other is returned; if both are NaN, the result is NaN.
- For `f64_min`/`f64_max`/`f64_min_num`/`f64_max_num`, when both inputs are zero with different signs, `min` returns `-0.0` and `max` returns `+0.0`.
- `f64_rem` is the exact remainder of truncating division (IEEE 754 `fmod`, Rust's `%`): the
  result has the sign of `a` and a magnitude below `|b|`. `rem(a, ±inf)` is `a` for finite `a`;
  `rem(±inf, b)` and `rem(a, ±0)` are NaN.
- `f64_to_bits`/`f64_from_bits` preserve IEEE 754 bit patterns (including NaN payloads).
- `f64_floor`/`f64_ceil`/`f64_trunc`/`f64_nearest` round to an integral value toward negative
  infinity, toward positive infinity, toward zero, and to the nearest integer with ties to even,
  matching IEEE 754 `roundToIntegral*` and WebAssembly `f64.floor`/`f64.ceil`/`f64.trunc`/`f64.nearest`.
  They are exact. A zero result keeps the input's sign (`f64_ceil(-0.5) = -0.0`,
  `f64_nearest(-0.5) = -0.0`); infinities and values with magnitude at least 2^52 are returned unchanged.
- `f64_sqrt` is the IEEE 754 correctly rounded square root (round to nearest, ties to even), matching
  WebAssembly `f64.sqrt`: `f64_sqrt(-0.0) = -0.0`, `f64_sqrt(+inf) = +inf`, and the square root of any
  other negative value (including `-inf`) is NaN.
- For the five operations above, a NaN input produces a NaN. Results are bit-for-bit identical on
  every platform (see [Floating-point determinism](#floating-point-determinism)).
- `f64_copysign` returns the bits of `a` with the sign bit of `b` (WebAssembly `f64.copysign`). It
  inspects no other bits: NaN payloads (including signaling NaNs) pass through unchanged, and a NaN
  `b` contributes only its sign.
- `f64_ne` is `!f64_eq`: it is true when the inputs are unordered (either is NaN), matching
  WebAssembly `f64.ne`.
- `f64.round_half_away` (and `f32.round_half_away`) rounds to the nearest integer with ties away
  from zero: IEEE 754 `roundToIntegralTiesToAway`, C's `round`/`roundf`. It differs from
  `f64_nearest` only on exact ties (`2.5 -> 3.0`, `-0.5 -> -1.0`). It is exact, and follows the
  rules above for `f64_nearest`: a zero result keeps the input's sign
  (`round_half_away(-0.4) = -0.0`), infinities and values with magnitude at least 2^52 (2^23 for
  `f32`) are returned unchanged, and a NaN input produces a NaN. The
  common emulation `trunc(x + copysign(0.5, x))` is not equivalent: it rounds `0.49999997f32` to
  `1.0` and odd integers just above 2^23 (2^52 for `f64`) to their even neighbors.
- `f64.clamp` and the `f64` transcendental functions `pow`, `exp`, `ln`, `sin`, `cos` and
  `atan2` are specified, with their `f32` forms, under [Math intrinsics](#math-intrinsics); other
  transcendental functions are host-provided.

#### Float-to-integer conversions
Every float-to-integer conversion (`f64_to_i64`, `f64_to_u64`, `f32.to_i32`, `f32.to_u32`,
`f64.to_i32`, `f64.to_u32` and their `_sat` forms) first truncates its input toward zero and only
then fits the truncated value to the target type; no range check looks at the input before
truncation. So `-0.5` and `-0.9999999` truncate to `-0.0` and convert to `0` for unsigned targets
just as for signed ones, and `-1.0` is the first negative value outside an unsigned range.

- The trapping forms match WebAssembly `i32.trunc_f32_s`/`_u`, `i32.trunc_f64_s`/`_u` and
  `i64.trunc_f64_s`/`_u`: NaN and infinities trap with `FloatToIntInvalid`, and a finite input
  whose truncation is outside the target range traps with `IntCastOverflow`. The accepted inputs
  are exactly those with `min - 1 < x < max + 1`: `-1 < x < 2^32` for `u32`, `-1 < x < 2^64` for
  `u64`, `-2^31 - 1 < x < 2^31` for `i32` and `-2^63 - 1 < x < 2^63` for `i64`. In particular
  2^32 and 2^64 themselves trap, while the largest float below each converts.
- The `_sat` forms match WebAssembly `trunc_sat` and never trap: a truncation below the target
  range (including `-inf`) produces the minimum (`i32::MIN`, `i64::MIN` or `0`), one above it
  (including `+inf`) produces the maximum, and NaN produces `0`. On every input the trapping form
  accepts, both forms give the same result.

#### Math intrinsics
The math intrinsics are `pow` (`a` raised to `b`), `exp`, `ln` (natural logarithm), `sin`,
`cos` (radians) and `atan2` (`y` first, result in `-pi..=pi`), in both widths: `f32.pow` through
`f32.atan2` and `f64.pow` through `f64.atan2`. Their results are **defined as those of the Rust
`libm` crate at exactly version 0.2.16** (`libm::powf`, `expf`, `logf`, `sinf`, `cosf`,
`atan2f` for `f32`; `libm::pow`, `exp`, `log`, `sin`, `cos`, `atan2` for `f64`), built without
default features. They are not correctly rounded (`f64.exp(1.0)` is one ulp above the correctly
rounded e); they are reproducible, and they match native Rust code that calls `libm` 0.2.16 bit
for bit.

- **Determinism.** These instructions are inside the
  [Floating-point determinism](#floating-point-determinism) contract: a result that is not NaN is
  bit-for-bit identical on every platform. That holds because `libm` computes them in plain Rust
  `f32`/`f64` arithmetic, which the contract's own assumptions make exact everywhere, and Rust
  never contracts it into fused multiply-add.
  `libm`'s optional `arch` and `unstable-intrinsics` features, which another crate in a build may
  enable through Cargo feature unification, replace only exactly specified IEEE 754 operations
  (`sqrt`, `fma`, `rint`, `floor`, `ceil`, `trunc`, `fabs`) with hardware instructions that
  produce the same bits, so they do not change any non-NaN result. 32-bit x86 without SSE2 is
  excluded, as it is for arithmetic.
- **NaN.** As for all arithmetic under the contract, only NaN-ness is guaranteed. The sign and
  payload come from the hardware's NaN propagation inside `libm` and do differ between platforms.
- **Implementation.** `execution_tape::math` publishes the functions the VM runs for these
  instructions (`math::pow_f32`, `math::exp_f32`, `math::ln_f32`, `math::sin_f32`,
  `math::cos_f32`, `math::atan2_f32` and their `_f64` forms), along with the exact `f32`/`f64`
  operations, so native
  code can compute the same bits as a tape program. They never depend on the `std` feature, so a
  `std` build does not route them to the platform C library.
- **Versioning.** The `libm` dependency is pinned with `=0.2.16`. Moving to another `libm` release,
  or to another implementation such as correctly rounded functions, changes results; it is made
  only deliberately, with a changelog entry and an update to this section and the contract
  naming the new definition. Cargo builds one copy of each semver-compatible `libm` (every
  0.2.x) per dependency graph, so the pin holds the whole build at 0.2.16: a dependent asking
  for `libm = "0.2"` resolves to 0.2.16, and one requiring a later 0.2 release (or another exact
  one) fails to resolve rather than getting a second copy. An embedding therefore moves to a new
  `libm` only with an `execution_tape` release that moves this definition, and native code in
  the same build that calls `libm` computes the same bits as the intrinsics.

`f32.clamp dst, a, lo, hi` and `f64.clamp dst, a, lo, hi` trap with `ClampBoundsInvalid` unless
`lo <= hi`, so they trap when `lo > hi` or either bound is NaN (where Rust's `f32::clamp` and
`f64::clamp` panic). Otherwise they return `lo` if `a < lo`, `hi` if `a > hi`, and `a`
otherwise, bit for bit what Rust's `clamp` returns (so `clamp(-0.0, 0.0, 1.0)` is `-0.0`, and a
NaN `a` gives a NaN). They are exact and do not use `libm`.

### Comparisons + branching
- `i64_eq r_dst, r_a, r_b` -> `Bool`
- `i64_lt r_dst, r_a, r_b` -> `Bool`
- `f64_eq`/`f64_ne`/`f64_lt`/`f64_gt`/`f64_le`/`f64_ge r_dst, r_a, r_b` -> `Bool`
- `dec_cmp r_dst, r_a, r_b` -> `I64` (`-1`, `0` or `1`, ignoring scale)
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
  - The callee may belong to any installed unit and runs that unit's program. Signatures match
    when their canonical contracts are identical: contracts are interned per VM at install, with
    struct/array types replaced by their heap shapes, so the check is one comparison. A unit the
    VM did not install traps with `UnknownUnit`.
- `closure_new r_dst, r_func, r_env`
  - Builds a `Closure` pairing the `Func` in `r_func` with a captured environment aggregate in `r_env`.
    The target must accept an aggregate environment first; construction checks that concrete
    contract before producing the closure. Dispatch also checks externally supplied environments,
    including that an unrefined environment handle resolves. `r_env` is injected as the first argument.

### Exceptions
- `throw r_eff_in, tag_id, payload...`
  - Terminator. Raises `tags[tag_id]` with `payload` in tag order.
  - The payload is materialized at the throw site and observed as `ValueEvent::Thrown`, then
    checked against the tag's payload contracts. A refined contract that the verifier could not
    prove traps there as `ContractMismatch` at position `Payload { tag, index }`.
  - The exception then searches for a catching clause. Each activation, innermost first, is
    considered at its raising instruction: the `throw` itself, or the `call`, `call.indirect` or
    `host_call` that entered the activation above it. Within an activation, the clauses of the
    instruction's innermost region are tried in order, then its parent's, and so on. A
    `catch tag` clause catches only that tag; `catch_all` catches every exception.
  - Exceptions carry a VM tag. Each activation compares the VM tags its own unit binds to its
    clauses' `tag_id`s, so units importing one tag catch each other's throws of it. A tag an
    activation's program neither declares nor imports reaches only its `catch_all`/`catch_all_ref`
    clauses; a `catch tag` clause never matches another install's declared tag, even one with
    the same name.
  - A catch abandons every activation above the catching one, including guest callbacks entered
    from a pending host request and that request itself, releasing their frames. The payload is
    written to the handler block's parameters and execution continues at the handler.
    `ValueEvent::Caught` reports the clause before unwinding; the throw, the abandoned
    activations and the abandoned host requests close with `TraceOutcome::Caught { depth }`,
    where `depth` is the catching activation's stack depth.
  - An exception that no clause catches leaves the invocation as `Trap::Uncaught`, located at
    the `throw`, and call scopes close with that trap outcome. Its payload is detached into a
    `ValueSnapshot` before rollback discards the invocation's aggregates. Host objects stay
    host-owned handles; function references keep their units.
  - Traps (fuel, limits, arithmetic, contract failures) are not exceptions and cannot be caught.
- `throw_ref r_eff_in, r_exn`
  - Terminator. Raises the exception referenced by the `Exn` register again, with its original
    tag, payload and throw site, and propagates it exactly like `throw`. The payload is neither
    copied nor checked again; `ValueEvent::Rethrown` names the original exception. Uncaught, it
    traps at the `throw_ref` with `UncaughtException::origin` still naming the original `throw`.
  - Handlers implement cleanup by catching with `catch_all_ref`, running cleanup code and then
    rethrowing. An exception raised by the cleanup code replaces the original.
  - Verification adds an exceptional edge from each raising instruction to every clause it can
    reach. The edge leaves before the raising instruction completes: registers written earlier in
    its basic block are initialized at the handler, but its results are not. Refinement facts of
    registers rewritten anywhere in that block are widened to their kind; handler parameters
    receive the tag's payload contracts.

### Stack switching
Continuations follow WebAssembly stack switching (`cont.new`, `resume`, `suspend`,
`resume_throw`, `cont.bind`), adapted to registers, VM tags and protected regions.

- A continuation is suspended or unstarted guest control: one or more activations whose bottom
  activation, once resumed, returns to its resumer. `Cont` registers hold references to them.
  Like `Exn`, `Cont` stays in guest registers and guest-to-guest signatures: it cannot appear in
  host signatures, entry signatures, tag payloads or results, or aggregates
  (`ContinuationReference`), and entering a `Cont` signature from outside traps with
  `ContinuationBoundary`.
- A continuation is typed by a call signature: its arguments are the values that resume it and
  its results the values it finally returns. Instructions name the signature they expect; the
  VM compares canonical contracts, so continuations cross units like indirect calls do. A
  different type traps with `ContinuationSignatureMismatch`.
- Continuations are one-shot. `resume`, `resume_throw`, `cont.bind` and `cont.drop` consume
  theirs; every copy of a consumed reference traps with `ContinuationConsumed`, including after
  its slot holds a new continuation. A continuation lives until it is consumed or its invocation
  ends. Its activations count against `max_call_depth` meanwhile,
  including the entry activation `cont.new` allocates.
- `cont.new r_dst, call_sig_id, r_func`: creates an unstarted continuation of the `Func` or
  `Closure` in `r_func`, checked against `call_sigs[call_sig_id]` as `call_indirect` checks its
  callee; a closure's environment is bound as the first argument.
- `cont.bind r_dst, from_sig_id, to_sig_id, r_cont, args...`: consumes a continuation of type
  `from_sig` and binds `args` to its leading inputs, producing one of type `to_sig`, whose
  arguments must be the remaining arguments and whose results must be identical
  (`ContBindSignature`).
- `cont.drop r_cont`: consumes a continuation of any type without resuming it and releases its
  activations, which no longer count against `max_call_depth`. They never run again, so their
  protected regions do not either; `resume_throw` unwinds through them instead. Continuations
  that the dropped activations' registers refer to are not released with them, since `Cont`
  references can be copied; drop each one that is abandoned. A consumed or dropped
  continuation traps with `ContinuationConsumed`.
- `resume r_eff_out, call_sig_id, r_cont, r_eff_in, args... -> rets...`: consumes a continuation
  of type `call_sig` and runs it on top of the resuming activation with `args`. When its bottom
  activation returns, `rets` receive the results. An exception escaping it propagates from the
  `resume` like one escaping a call.
- `suspend r_eff_out, tag_id, r_eff_in, payload... -> rets...`: searches the `resume`s that run
  the current activation, innermost first. At each, the `on` clauses of the regions protecting
  it are tried in order, then their parents'; catch clauses are skipped. The first `on` clause
  for the tag (comparing VM tags, as catch clauses do) receives the payload and a new
  continuation of the activations above that `resume`, typed by the tag's results and the
  `resume`'s results. Resuming it continues after the `suspend`, whose `rets` receive the resume
  values. No matching clause traps with `UnhandledSuspension`, unless the invocation yields
  suspensions (`InvocationOptions::yield_suspensions`): then the suspension waits for the
  embedding, which resumes it with values for the tag's results (checked for arity, kind,
  payload work and the suspending unit's result contracts, as `ContractPosition::Resumption`),
  raises an exception at the `suspend`, or discards it, which traps with
  `UnhandledSuspension` there. A search that reaches a guest callback's root traps with
  `SuspensionAcrossHost`, since a pending host request cannot be suspended. Payload contracts
  are checked at the `suspend`, before it is handled, yielded or trapped as unhandled; resume
  values against the resume signature's contracts.
- `resume_throw r_eff_out, call_sig_id, r_cont, r_eff_in, tag_id, payload... -> rets...`:
  consumes and reinstates a continuation like `resume`, then raises `tag` with `payload` at its
  suspension point, where its own regions may catch it. The raise is located at that `suspend`
  (its `Thrown` and `Caught` observations and its origin), while the tag and payload contracts
  are the `resume_throw`'s. An unstarted continuation is discarded instead and the exception
  raised at the `resume_throw`.
- Each instruction costs one fuel. Switching is deterministic guest control: recordings need no
  new evidence and replay re-executes it. A suspension yielded to the embedding is the
  exception: recordings retain its payload and the embedding's answer, and replay answers it
  again. Activations whose continuation suspends close their call-frame scopes with
  `TraceOutcome::Suspended`, including when it is yielded; resuming enters new scopes for them
  at their new depths. The interpreter moves a suspended continuation's frames and registers off
  its one contiguous stack and back on resume, and leaves a yielded suspension's frames in place;
  a native tier may instead give each continuation, and the invocation's own root, its own
  machine stack.

### Host calls
- `host_call r_eff_out, host_sig_id, r_eff_in, args... -> rets...`
  - Increments host-call count; traps if limit exceeded.
  - Host returns:
    - either success with return values and optional additional fuel cost
    - or failure, which becomes `HostCallFailed { symbol, sig_hash, error }`
    - or `HostError::throw(tag, payload)` (`PendingHostCall::throw` for yielded requests), which
      raises a guest exception at the `host_call` exactly as `throw` would there. Payload
      aggregates built with the call's context are adopted like results. The tag is a VM tag:
      one defined with `Vm::define_tag`, or a program tag resolved with `HostContext::tag`. A tag
      the VM never issued traps with `UnknownTag`; a wrong payload arity or kind traps like an
      invalid return. Refined payload contracts are checked through the calling unit's binding of
      the tag, or else the unit that declared it.

## Opcode encodings
The schema in `execution_tape/opcodes.json` defines instruction names, bytes, operands and
encodings. `execution_tape/src/opcode.rs` exposes the generated `Opcode` mapping. For register
operands (`reg` and `reg_list`), the JSON spec also includes an `access` field (`read` or `write`) used by analysis tooling (e.g. reads/writes sets).

### Allocating opcodes
New instruction families are allocated under a prefix, not in the remaining single-byte gaps:
give the opcode entry `"prefix": "0xFC"` and the next free `"byte"` in that space (32-bit
numerics go in their family's block under `0xFD`), then run
`cargo run -p execution_tape_codegen`. Free single bytes are kept for instructions frequent
enough that the saved byte matters. Existing opcodes are never renumbered. When `0xFC` fills, the
next prefix is declared in the schema's `prefixes` list from the reserved range.

All register indices and small integers are ULEB128 unless noted.

- `0x00 nop`
- `0x01 mov dst, src`
- `0x02 trap code`
- `0x03 block params` (counted ULEB32 register IDs; kinds come from the function register ledger)
- `0x04 throw eff_in, tag_id, payload_count, payload...`
- `0x05 throw_ref eff_in, exn`

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
- `0x94 f64_floor dst, a`
- `0x95 f64_ceil dst, a`
- `0x96 f64_trunc dst, a`
- `0x97 f64_nearest dst, a` (ties to even)
- `0x98 f64_sqrt dst, a` (correctly rounded)
- `0x99 f64_copysign dst, a, b`
- `0x9A f64_to_i64_sat dst, a` (`f64` -> `i64`, saturating; NaN -> 0)
- `0x9B f64_to_u64_sat dst, a` (`f64` -> `u64`, saturating; NaN -> 0)
- `0x9C f64_ne dst, a, b` (IEEE: true if NaN)
- `0x9D dec_cmp dst, a, b` (`decimal` -> `i64`: -1, 0 or 1, ignoring scale)
- `0x9E dec_scale dst, a` (`decimal` -> `u64`)
- `0x9F dec_rescale dst, a, scale, rounding_u8` (`scale` is a `u64` register)
- `0xA0 dec_div dst, a, b, scale, rounding_u8` (`scale` is a `u64` register; traps on zero divisor)
- `0xA1 i64_add_ovf dst, overflow, a, b` (`dst` wrapped `i64`, `overflow` `bool`)
- `0xA2 i64_sub_ovf dst, overflow, a, b`
- `0xA3 i64_mul_ovf dst, overflow, a, b`
- `0xA4 u64_add_ovf dst, overflow, a, b` (`dst` wrapped `u64`, `overflow` `bool`: carry)
- `0xA5 u64_sub_ovf dst, overflow, a, b` (`overflow`: borrow)
- `0xA6 u64_mul_ovf dst, overflow, a, b`

Under the `0xFC` prefix (each opcode is the two bytes shown):
- `0xFC 0x00 u64.clz dst, a`
- `0xFC 0x01 u64.ctz dst, a`
- `0xFC 0x02 u64.popcnt dst, a`
- `0xFC 0x03 u64.rotl dst, a, b` (rotation amount masked with `& 63`)
- `0xFC 0x04 u64.rotr dst, a, b` (rotation amount masked with `& 63`)
- `0xFC 0x05 cont.new dst, call_sig_id, func`
- `0xFC 0x06 resume eff_out, call_sig_id, cont, eff_in, argc, args..., retc, rets...`
- `0xFC 0x07 suspend eff_out, tag_id, eff_in, argc, payload..., retc, rets...`
- `0xFC 0x08 resume_throw eff_out, call_sig_id, cont, eff_in, tag_id, argc, payload..., retc, rets...`
- `0xFC 0x09 cont.bind dst, from_sig_id, to_sig_id, cont, argc, args...`
- `0xFC 0x0A cont.drop cont`
- `0xFC 0x0B f64.round_half_away dst, a` (ties away from zero)
- `0xFC 0x0C f64.clamp dst, a, lo, hi` (traps with `ClampBoundsInvalid` unless `lo <= hi`)

`f64` math intrinsics (`0xFC 0x10..=0x1F`, ordered like the `f32` block at `0xFD 0xA0`; see
[Math intrinsics](#math-intrinsics)):
- `0xFC 0x10 f64.pow dst, a, b`
- `0xFC 0x11 f64.exp dst, a`
- `0xFC 0x12 f64.ln dst, a`
- `0xFC 0x13 f64.sin dst, a`
- `0xFC 0x14 f64.cos dst, a`
- `0xFC 0x15 f64.atan2 dst, y, x`

Under the `0xFD` prefix, the 32-bit numerics in aligned family blocks (operands in encoding
order; `const.i32` takes an SLEB128 `i32`, `const.u32` a ULEB128 `u32` and `const.f32` its bits
as `u32le`):

`i32` (`0x00..=0x3F`):
- `0xFD 0x00 const.i32 dst, imm`
- `0xFD 0x01 i32.add dst, a, b`
- `0xFD 0x02 i32.sub dst, a, b`
- `0xFD 0x03 i32.mul dst, a, b`
- `0xFD 0x04 i32.div dst, a, b`
- `0xFD 0x05 i32.rem dst, a, b`
- `0xFD 0x06 i32.add_ovf dst, overflow, a, b`
- `0xFD 0x07 i32.sub_ovf dst, overflow, a, b`
- `0xFD 0x08 i32.mul_ovf dst, overflow, a, b`
- `0xFD 0x09 i32.and dst, a, b`
- `0xFD 0x0A i32.or dst, a, b`
- `0xFD 0x0B i32.xor dst, a, b`
- `0xFD 0x0C i32.shl dst, a, b`
- `0xFD 0x0D i32.shr dst, a, b`
- `0xFD 0x0E i32.eq dst, a, b`
- `0xFD 0x0F i32.lt dst, a, b`
- `0xFD 0x10 i32.gt dst, a, b`
- `0xFD 0x11 i32.le dst, a, b`
- `0xFD 0x12 i32.ge dst, a, b`

`u32` (`0x40..=0x7F`):
- `0xFD 0x40 const.u32 dst, imm`
- `0xFD 0x41 u32.add dst, a, b`
- `0xFD 0x42 u32.sub dst, a, b`
- `0xFD 0x43 u32.mul dst, a, b`
- `0xFD 0x44 u32.div dst, a, b`
- `0xFD 0x45 u32.rem dst, a, b`
- `0xFD 0x46 u32.add_ovf dst, overflow, a, b`
- `0xFD 0x47 u32.sub_ovf dst, overflow, a, b`
- `0xFD 0x48 u32.mul_ovf dst, overflow, a, b`
- `0xFD 0x49 u32.and dst, a, b`
- `0xFD 0x4A u32.or dst, a, b`
- `0xFD 0x4B u32.xor dst, a, b`
- `0xFD 0x4C u32.shl dst, a, b`
- `0xFD 0x4D u32.shr dst, a, b`
- `0xFD 0x4E u32.clz dst, a`
- `0xFD 0x4F u32.ctz dst, a`
- `0xFD 0x50 u32.popcnt dst, a`
- `0xFD 0x51 u32.rotl dst, a, b`
- `0xFD 0x52 u32.rotr dst, a, b`
- `0xFD 0x53 u32.eq dst, a, b`
- `0xFD 0x54 u32.lt dst, a, b`
- `0xFD 0x55 u32.gt dst, a, b`
- `0xFD 0x56 u32.le dst, a, b`
- `0xFD 0x57 u32.ge dst, a, b`

`f32` (`0x80..=0xBF`):
- `0xFD 0x80 const.f32 dst, bits`
- `0xFD 0x81 f32.add dst, a, b`
- `0xFD 0x82 f32.sub dst, a, b`
- `0xFD 0x83 f32.mul dst, a, b`
- `0xFD 0x84 f32.div dst, a, b`
- `0xFD 0x85 f32.rem dst, a, b`
- `0xFD 0x86 f32.neg dst, a`
- `0xFD 0x87 f32.abs dst, a`
- `0xFD 0x88 f32.min dst, a, b`
- `0xFD 0x89 f32.max dst, a, b`
- `0xFD 0x8A f32.min_num dst, a, b`
- `0xFD 0x8B f32.max_num dst, a, b`
- `0xFD 0x8C f32.copysign dst, a, b`
- `0xFD 0x8D f32.floor dst, a`
- `0xFD 0x8E f32.ceil dst, a`
- `0xFD 0x8F f32.trunc dst, a`
- `0xFD 0x90 f32.nearest dst, a`
- `0xFD 0x91 f32.sqrt dst, a`
- `0xFD 0x92 f32.eq dst, a, b`
- `0xFD 0x93 f32.ne dst, a, b`
- `0xFD 0x94 f32.lt dst, a, b`
- `0xFD 0x95 f32.gt dst, a, b`
- `0xFD 0x96 f32.le dst, a, b`
- `0xFD 0x97 f32.ge dst, a, b`
- `0xFD 0x98 f32.round_half_away dst, a` (ties away from zero)
- `0xFD 0x99 f32.clamp dst, a, lo, hi` (traps with `ClampBoundsInvalid` unless `lo <= hi`)

`f32` math intrinsics (`0xA0..=0xBF`; see [Math intrinsics](#math-intrinsics)):
- `0xFD 0xA0 f32.pow dst, a, b`
- `0xFD 0xA1 f32.exp dst, a`
- `0xFD 0xA2 f32.ln dst, a`
- `0xFD 0xA3 f32.sin dst, a`
- `0xFD 0xA4 f32.cos dst, a`
- `0xFD 0xA5 f32.atan2 dst, y, x`

Conversions (`0xC0..=0xFF`):
- `0xFD 0xC0 i32.to_i64 dst, a`
- `0xFD 0xC1 i64.to_i32 dst, a`
- `0xFD 0xC2 i64.to_i32_wrap dst, a`
- `0xFD 0xC3 u32.to_u64 dst, a`
- `0xFD 0xC4 u64.to_u32 dst, a`
- `0xFD 0xC5 u64.to_u32_wrap dst, a`
- `0xFD 0xC6 u32.to_i64 dst, a`
- `0xFD 0xC7 i32.to_u32 dst, a`
- `0xFD 0xC8 u32.to_i32 dst, a`
- `0xFD 0xC9 i32.to_u32_wrap dst, a`
- `0xFD 0xCA u32.to_i32_wrap dst, a`
- `0xFD 0xCB i64.to_u64_wrap dst, a`
- `0xFD 0xCC u64.to_i64_wrap dst, a`
- `0xFD 0xCD f32.to_f64 dst, a`
- `0xFD 0xCE f64.to_f32 dst, a`
- `0xFD 0xCF f32.to_bits dst, a`
- `0xFD 0xD0 f32.from_bits dst, a`
- `0xFD 0xD1 f32.to_i32 dst, a`
- `0xFD 0xD2 f32.to_u32 dst, a`
- `0xFD 0xD3 f32.to_i32_sat dst, a`
- `0xFD 0xD4 f32.to_u32_sat dst, a`
- `0xFD 0xD5 f64.to_i32 dst, a`
- `0xFD 0xD6 f64.to_u32 dst, a`
- `0xFD 0xD7 f64.to_i32_sat dst, a`
- `0xFD 0xD8 f64.to_u32_sat dst, a`
- `0xFD 0xD9 i32.to_f32 dst, a`
- `0xFD 0xDA u32.to_f32 dst, a`
- `0xFD 0xDB i64.to_f32 dst, a`
- `0xFD 0xDC u64.to_f32 dst, a`
- `0xFD 0xDD i32.to_f64 dst, a`
- `0xFD 0xDE u32.to_f64 dst, a`
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
- `0x72 f64_to_i64 dst, a` (truncates, then traps on NaN/inf/out-of-range; see
  [Float-to-integer conversions](#float-to-integer-conversions))
- `0x73 f64_to_u64 dst, a` (truncates, then traps on NaN/inf/out-of-range; `-0.5` converts to `0`)
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
  - `throw` matches the `tags[tag_id]` payload count and kinds, and statically known payload facts
    do not contradict the tag's payload contracts
- **Declaration limit**: `reg_count` is within `VerifyConfig::max_regs_per_function`.
- **Effect threading**:
  - call/host-call `eff_in` and `eff_out` must be r0 (`Unit`, initialized at entry)
  - `ret` and `throw` must use r0 as their effect operand

### Additional verifier details
These checks apply alongside the structural and contract rules described above.

- **Required sections present**: sections `1..=8` and `12` must exist exactly once; `names` and `call_sigs` are optional.
- **Bytecode decode**: the function bytecode must decode cleanly as a sequence of opcodes. An
  unassigned opcode is `UnknownOpcode` with its code and pc; other malformed bytes are
  `BytecodeDecode`.
- **CFG targets**:
  - all `br/jmp` targets must be within `[0, bytecode_len)`
  - all `br/jmp` targets must be an instruction boundary (byte offset that starts an instruction)
- **Reachability**: init-before-use is checked only on reachable blocks.
- **Register bounds**: any register operand must be `< reg_count`.
- **Register convention**:
  - `r0` (effect token) is considered initialized at entry.
  - value arguments are considered initialized at entry in `r1..=r_arg_count`.
- **Init-before-use**: any read of a register not definitely initialized on all paths to that point is rejected.
- **No fallthrough between blocks**: every reachable basic block must end in a terminator (`ret`/`trap`/`throw`/`br`/`jmp`).
- **Call arity**: `call` must pass exactly `callee.arg_count` value args and list exactly `callee.ret_count` return regs.
- **Return arity**: `ret` must return exactly `func.ret_count` values.
- **HostSig table**:
  - `host_sigs[i].symbol_id` must be in-bounds
  - `host_sigs[i].sig_hash` must match the canonical hash of its `(arg_types, ret_types)`
- **Host call**: `host_call` must reference an in-bounds `host_sig_id` and its args must match that signature's types.
- **Tag table**: every tag's name symbol and payload contracts must be in bounds, and names must be
  unique, including tags that no instruction throws.
- **Throw**: `throw` must reference an in-bounds `tag_id`. Diagnostics name the tag
  (`ThrowArityMismatch`, `ThrowPayloadMismatch`).
- **Stack switching**: call signatures and tags must be in bounds; `resume` arguments and
  results match its signature, `suspend` and `resume_throw` payloads match their tag, `suspend`
  results match the tag's results, `cont.bind` signatures differ by exactly the bound arguments,
  and continuation operands are `Cont` registers. Effect operands are `r0`.
- **Regions**: parents precede their children; clause tags are in bounds and targets are block
  headers whose parameters receive the payload (`InvalidRegion` names the region and clause);
  runs are sorted, disjoint and on instruction boundaries (`InvalidRegionRun`). Initialization
  and refinement facts flow along exceptional edges as described under [Exceptions](#exceptions).

## Implementation authority

[`execution_tape/opcodes.json`](../execution_tape/opcodes.json) defines opcode bytes, operands,
encodings and operand roles. `execution_tape_codegen` generates the opcode/codec/disassembly
machinery from it. Calls and returns use explicit register lists; aggregates use an immutable
arena-backed DAG. Verification validates declared host signatures without resolving host symbols;
`HostFunctions::bind` can resolve all imports before execution, or a custom `Host` can handle them
at runtime. Future capabilities are distinguished from current behavior in the
[overview](overview.md#future-possibilities).
