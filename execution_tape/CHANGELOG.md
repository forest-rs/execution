<!-- Instructions

This changelog follows the patterns described here: <https://keepachangelog.com/en/>.

Subheadings to categorize changes are `added, changed, deprecated, removed, fixed, security`.

-->

# Changelog

The latest published Execution Tape release is [0.0.1](#001-2026-05-31) which was released on 2026-05-31.
You can find its changes [documented below](#001-2026-05-31).

## [Unreleased][]

This release has an [MSRV][] of 1.88.

### Added

- Function and closure value contracts preserve exact exposed signatures through aggregates,
  projections, joins and indirect calls. Concrete closure environments are checked against the
  selected body, including closures received through guest boundaries.


- Observatory repeats the verification outcome in the Details title and highlights its
  verdict. Explain unknown member types (`?`) and why tuple length needs no element type,
  so a valid join is distinguishable from a rejected program. ([#161][] by [@waywardmonkeys][])

- Run `join_contracts [OUTPUT.tape]` to execute both arms of a typed join and optionally save
  its program. Observatory exposes why `tuple(i64)` and `tuple(bool)` meet at `tuple(?)`, while
  preserving the outer tuple's known length. ([#161][] by [@waywardmonkeys][])

- Measure optional inspection separately with the `inspection` benchmark: selected-function
  analysis including destruction, repeated same-PC queries, and forward source scans. Share
  the verifier benchmark fixtures so the workloads stay comparable. ([#161][] by [@waywardmonkeys][])

- Observatory shows selected-instruction before/after contracts and local definition origins,
  including aliased projections and the input arms of typed joins. Views cache optional analysis
  per selected function and disclose unavailable/unreachable facts. `inspect_prepared` accepts
  an optional source PC to print register facts and incoming parameter bindings. ([#161][] by [@waywardmonkeys][])

- Inspect opt-in verifier facts through `ProgramAnalysis`, selected `FunctionAnalysis` and
  `InstructionFacts` queries. Distinguish storage kinds, definite initialization, unreachable
  code and nested contracts; follow local origins and incoming edge bindings. Contracts share
  one analysis-local table. Normal execution artifacts retain no inspection state. Use
  `FunctionDisassembly::instruction(pc)` to follow exact source locations with indexed lookup. ([#160][] by [@waywardmonkeys][])

- Run `nested_contracts [OUTPUT.tape]` to execute and optionally save a nested-result example.
  Observatory previews nested guest signatures with bounded depth/member counts and caches the
  selected summary. `inspect_prepared` prints contract definitions once in child-before-parent
  order and maps argument/result positions to their IDs. The `contracts` benchmark measures the
  runtime cost of enforcing shared-graph contracts against kind-only signatures. ([#157][] by [@waywardmonkeys][])

- Intern and inspect compositional value contracts with `ContractTableBuilder`. Definitions
  share flat immutable storage, preserve unknown tuple members, and reject missing/forward
  references. Contract IDs belong to one table; nominal schema validation belongs to its owner. ([#155][] by [@waywardmonkeys][])

- Build and inspect shared compositional value contracts with `ContractTableBuilder` and
  `ProgramBuilder::contract`. Guest arguments/results can describe nested tuples and nominal
  structs/arrays. Local construction, projections, direct calls and typed joins preserve nested facts. ([#156][] by [@waywardmonkeys][])


- Correlate VM runs, guest calls and host work with the optional `execution_tape_footfall`
  adapter. Instruction details are opt-in; bounded captures preserve loss metadata. The
  `footfall_capture` tool saves the tape, context manifest and Footfall/Perfetto exports. ([#143][] by [@waywardmonkeys][])

- Open compiled tapes with `observatory --program PATH`, using the same browser interactively
  or with `--snapshot`. Decode errors identify the file; rejected programs remain inspectable.
  `--write-demo PATH` creates a compiled sample without overwriting an existing file. ([#151][] by [@waywardmonkeys][])

- Browse compiled programs with Observatory `browse` / `render_program`. Search functions,
  inspect imports/constants/aggregate types, follow calls and control-flow edges, and navigate
  rejected-program diagnostics without execution. Existing recording views remain available. ([#151][] by [@waywardmonkeys][])

- Query `VerifyError::function` and `pc` to locate rejected instructions or function contracts
  without parsing diagnostic text. Program-wide errors have no function location. ([#150][] by [@waywardmonkeys][])

- Draw static source control flow in the Observatory with independent colored edge lanes,
  conditional-arm labels and target arrows. Count excess edges explicitly. ([#149][] by [@waywardmonkeys][])


- Add syntax/value colors to the Observatory using an indexed 256-color palette, without
  requiring 24-bit RGB support. The demo accepts `--color=always` to explicitly override `NO_COLOR`. ([#149][] by [@waywardmonkeys][])


- Inspect recorded guest callers in the Observatory stack pane. Cycle keyboard focus with
  Tab/Shift+Tab and select a frame to move source to its captured call-site instruction.
  Show the application-to-guest invocation separately from guest frames, using captured inputs.
  The read/compute/store demo now includes a guest helper and compact call-value rows. ([#148][] by [@waywardmonkeys][])


- Capture bounded guest activation ancestry in `execution_tape_recording`, with exact caller
  PCs, shared parent links and borrowed innermost-to-root `CallStack` iteration. Inspect
  `HostCall::stack` and `Recording::outcome_stack`; trap ancestry survives VM unwinding.
  Replay checks captured activation order, caller locations and boundary ancestry. ([#148][] by [@waywardmonkeys][])

- Add full-lifecycle aggregate benchmarks covering repeated runs, temporary retention, and
  returned aggregate graphs. ([#125][] and [#128][] by [@waywardmonkeys][])

- Add a runnable block-argument comparison and Criterion benchmark in `execution_tape_wind_tunnel`.
  Compare equivalent transfer-heavy loops by cold preparation, reused execution, prepared
  storage, and frame requirements, checking output equality before timing. ([#137][] by [@waywardmonkeys][])

- Borrow invocation inputs/limits, raw host outcomes/fuel and root return candidates through
  `TraceSink::values` with `TraceMask::VALUES`. Combine with `RUN` for the final outcome;
  candidates precede validation/compaction and their handles are borrowed for the callback.
  Recording ownership and replay policy remain outside the VM. ([#144][] by [@waywardmonkeys][])

- Own invocation boundary values and reachable aggregate graphs with the separate
  `execution_tape_recording` crate. Capture budgets and incomplete states are explicit;
  replay reexecutes guest code in a fresh VM with recorded host outcomes, preserving graph
  identities and typed failures without invoking the live host. ([#145][] by [@waywardmonkeys][])

- Run `record_replay` in the wind-tunnel crate to capture a shared-graph workload, drop the
  live VM/host, and check fresh replay. The `recording` benchmark measures ordinary execution,
  capture/freeze/drop and fresh replay/drop, checking results and storage counts before timing. ([#146][] by [@waywardmonkeys][])

- Declare typed control-flow joins with `Asm::block`, pass arguments with `Label::with_args`, and
  use the existing `br`/`jmp` methods. Edges transfer values simultaneously, including cycles,
  without extra guest instructions, fuel, or trace events. Inspect source arms with
  `InstrView::edge` and declaration mappings with `FunctionDisassembly::executable_pc`. ([#136][] by [@waywardmonkeys][])

- Preserve typed, owned operation errors with `HostError::custom`. Recover concrete failures
  through the standard error source chain, including after dropping the host or VM. Cloning a
  failure shares its immutable payload. ([#133][] by [@waywardmonkeys][])

- Register host callbacks with complete signatures through `HostFunctions`, validate imports with `bind`, and
  create independent host instances with `HostBindings::with_state`. Binding failures identify
  every missing/mismatched import and include expected and available complete signatures. ([#132][] by [@waywardmonkeys][])

- Expose the active program-local import through `HostContext::host_sig_id`, and embedded host
  state through `Vm::host` and `host_mut` for inspection and updates between runs. ([#131][] by [@waywardmonkeys][])

- Add owned `ValueSnapshot` retention through `AggHeap::snapshot` and `HostContext::retain`,
  with `AggHeap::import` for moving retained data into another heap. Snapshots copy only
  reachable aggregate data and preserve sharing. Host objects and function IDs keep their domains. ([#127][] by [@waywardmonkeys][])

- Inspect prepared functions through `VerifiedProgram::function` and `functions`, with borrowed
  signatures, aggregate contracts, instruction locations, register-bank requirements, and
  function-local buffer sizes. No bytecode decoding or allocation is needed. The wind-tunnel
  `inspect_prepared` tool prints a summary and optional instruction detail from an encoded file. ([#130][] by [@waywardmonkeys][])

- Explore recordings through the optional Ratatui `execution_tape_observatory` crate:
  navigate invocation/host-call/outcome boundaries, open immediate aggregate fields, inspect
  named source calls and static control-flow cues, and request checked replay. Clearly mark
  keyboard focus and distinguish sent arguments from returned values. The wind-tunnel
  `observatory` tool captures a read/compute/store workflow and supports headless text rendering. ([#147][] by [@waywardmonkeys][])

- Expose the final return/trap PC through `Recording::outcome_location`, including the VM
  location of rejected invocations. Return locations precede aggregate-handle compaction. ([#147][] by [@waywardmonkeys][])

### Changed

- Wire version 0.4 encodes indirect signatures with shared compositional contract IDs; regenerate
  encoded programs. Replace `call_sig(args, rets)` with fallible `call_sig(FunctionSig { ... })`
  for kind-only signatures or `call_sig(FunctionAbi { ... })` for refinements. The argument/result
  kind accessors return borrowed `ContractKinds` views; contract accessors expose the same IDs.
  Indirect target equivalence is exact, including nested contracts; it does not implement variance
  or subtyping. Coarse signatures do not match richer declarations merely because their ABI kinds
  agree. Known incompatible targets now fail verification; opaque mismatches trap before dispatch.
  `closure_new` now requires an environment-bearing target and validates its environment before
  producing a closure. Signature mismatches precede selected-callee dispatch; validation can still
  fail after earlier host work, whose external effects are not rolled back.
  Handle the new callable mismatch diagnostics and `ContractPath::{Member, Environment}` path steps.
  Contract-work limits additionally count callable signature positions and environment edges.


- Contract failures display readable expected/found types and indexed argument/result paths.
  Structured error fields remain unchanged; use them rather than parsing `Display` text. ([#159][] by [@waywardmonkeys][])

- Guest signatures now store authoritative compositional contract IDs in wire version 0.3;
  regenerate encoded programs. `FunctionSig` remains the kind-only authoring shorthand.
  Replace `FunctionAbi::{sig, arg_agg_shapes}` with `args`/`rets` lists of `ContractRef`, and
  replace argument-shape helpers with shared contracts. `AggShape`, its argument-only metadata,
  and `AggHeap::check_shape` are removed. Host and indirect-call signatures retain their ABI kinds. ([#156][] by [@waywardmonkeys][])

- Handle `ProgramBuilder::declare_function`'s `Result`; missing contract IDs fail before mutation.
  Function argument/result kind accessors now return borrowed `ContractKinds` views rather than
  slices; use `iter()` or query `arg_contracts`/`ret_contracts` and the program's contract table. ([#156][] by [@waywardmonkeys][])

- Add `Limits::max_contract_work` to exhaustive initializers (or use `..Default::default()`).
  Aggregate boundary validation is iterative, shares successful proofs within a run, and counts
  value visits and child edges separately from instruction fuel. Result obligations are checked
  before caller installation or root commit. `Trap::ContractMismatch` replaces
  `ArgumentShapeMismatch`; handle `ContractWorkExceeded` and the corresponding verifier contract
  diagnostics in exhaustive matches. Failures include the boundary position and nested member path. ([#156][] by [@waywardmonkeys][])


- `Recorder::mask` now includes `CALL`. Add `call_frames` to exhaustive `RecordLimits`
  initializers (or use `..Default::default()`); this budget counts all entered activations.
  Explicit masks that omit `CALL` retain value-only capture with unavailable stacks. Update
  exhaustive `CaptureResource` and `Divergence` matches for the new stack cases. ([#148][] by [@waywardmonkeys][])

- Include the actual caller instruction in `ScopeKind::CallFrame::caller`, with `None` for
  the root activation. Entry metadata is preserved on normal and trapping exits. Update
  exhaustive patterns to bind `caller` or use `..`, and supply it when constructing this variant. ([#148][] by [@waywardmonkeys][])

- Declare every function register by transferring its kind `Vec` to `Asm::set_register_types`,
  including r0 (`Unit`), arguments and temporaries. Raw `FunctionDef` inputs now own `register_types` in place of `reg_count`.
  Preserve the complete `AsmParts` when assembling raw definitions. `Asm::block` now takes only
  register IDs; parameter kinds come from the function ledger. Programs use wire version 0.2;
  regenerate serialized programs with the updated producer. Declarations do not initialize values
  or establish aggregate shapes. ([#141][] by [@waywardmonkeys][])

- Replace `VerifyError::{TypeMismatch, UnknownTypeAtUse, UnknownTypeAtSelect, UnstableRegType,
  UnstableRegTypeAt}` with `MissingRegisterTypes`, `RegisterTypeMismatch` and
  `UnknownAggregateShape`. A register mismatch's `pc: None` identifies the r0/argument prefix;
  instruction failures carry their source PC. Static kind and call/return contracts are checked
  even in unreachable code. `Program::encode` now panics on manually malformed register
  declarations; verify manually edited containers before encoding them. ([#141][] by [@waywardmonkeys][])

- Run the disassembler example with `cargo run -p execution_tape_wind_tunnel --bin disasm`.
  Examples now live in the separate tool crate, keeping the core crate focused on the library. ([#138][] by [@waywardmonkeys][])

- The draft bytecode adds `Block`, `JmpArgs`, and `BrArgs`; older readers reject these opcodes.
  Existing label-only branches remain supported. Exhaustive opcode, operand codec/schema,
  disassembly operand, and `VerifyError` matches must handle the new variants. Block failures
  report declaration or source-edge context, including arm, target, and argument details.
  A block reserved with `Asm::block` may be placed once; failed repeated placement preserves
  its existing bytes and name. ([#136][] by [@waywardmonkeys][])

- Share allocation-free register operand iteration between verification and disassembly.
  Opcode generation now supports multiple register lists, list-only outputs, and additional
  scalar operands without adding iterator constructors. ([#135][] by [@waywardmonkeys][])

- `HostError`, `Trap`, and `TrapInfo` no longer implement `PartialEq`/`Eq`, because arbitrary
  host errors need not support value equality. Match variants and compare diagnostic fields or
  downcast concrete error sources instead. Exhaustive `HostError` matches must handle `Custom`. ([#133][] by [@waywardmonkeys][])

- Exhaustive `HostError` matches must handle `ProgramMismatch`: prepared host instances reject
  calls from another program, including equal cloned programs, before invoking a callback. ([#132][] by [@waywardmonkeys][])

- VM runs now retain only new aggregates reachable from returned values; traps discard all new
  aggregates. Previously retained values stay valid until the heap is cleared. Treat host
  aggregate and closure-environment arguments as borrowed for the call; use `HostContext::retain`
  for durable host state, then `AggHeap::import` before using it in a VM. Returning a dangling
  aggregate or closure environment now traps at `ret`. ([#129][] by [@waywardmonkeys][])

- `AggHeap::tuple_new`, `struct_new`, and `array_new` now return `Result<AggHandle, AggError>`.
  Handle the result when constructing aggregates. Heap and staged constructors reject dangling
  aggregate children and closure environments before mutation, preventing forward references
  and cycles. Schema validation remains a separate operation. ([#126][] by [@waywardmonkeys][])

- Verification now validates immutable metadata, including unused constant ranges, symbol and
  field-name ranges, struct layouts, and optional names. Malformed programs fail preparation
  with `VerifyError::InvalidMetadata` instead of trapping during execution. Exhaustive error
  matches must handle the new variant; `MetadataTable` identifies the failing table. ([#124][] by [@waywardmonkeys][])

- Borrow immutable byte/string payloads from constants, entry arguments, and aggregate fields
  during execution. Move host-owned return payloads into run storage. Owned outputs and
  persistent aggregate contents retain their existing lifetime and representation. ([#120][] by [@waywardmonkeys][])
- `Vm::run_with_ctx` now takes a mutable output `Vec` and returns `Result<(), TrapInfo>`.
  Pass an empty or cleared buffer to replace results, or retain contents to append. Traps
  preserve existing contents. `run` remains the allocating convenience entry point. ([#119][] by [@waywardmonkeys][])
- Define `TraceSink::instr` as an instruction attempt; `next_pc` is its lexical end.
  Use the new `instr_end` callback for success/trap and the actual control-flow successor.
  Add a `TraceOutcome` argument to `scope_exit` implementations; scopes now close on traps
  even without run tracing, and host success includes validating returned values. ([#118][] by [@waywardmonkeys][])
- Improve crate and README documentation, keeping examples synchronized. ([#93][] by [@no-materials][])
- Reduce verifier allocations by reusing dataflow buffers and storing aggregate metadata sparsely.
  Verification only computes liveness-based diagnostics through the `_with_lints` entry points. ([#109][] by [@waywardmonkeys][])
- Decode bytecode and span tables directly into packed arenas, removing temporary per-function
  allocations and repacking copies. ([#111][] by [@waywardmonkeys][])
- Release virtual-register mappings after lowering to reduce retained execution storage. ([#112][] by [@waywardmonkeys][])

### Fixed

- Observatory distinguishes fixed register storage kinds from nested call-result contracts and
  required return contracts. Rejected contracts name the function and explain the member path,
  including named struct fields, without exposing Rust debug syntax. ([#159][] by [@waywardmonkeys][])

- Discard obsolete aggregate shape facts when calls or nested projections overwrite a register.
  Aggregate selects retain only the shape shared by both inputs, including aliased destinations. ([#134][] by [@waywardmonkeys][])

- Preserve load-time traps for malformed byte/string constant ranges, even when the loaded
  value is unused. ([#123][] by [@waywardmonkeys][])

- Validate aggregate argument contracts before entry and direct, indirect, and closure calls.
  Exhaustive matches on `Trap` must handle `ArgumentShapeMismatch`; `AggHeap::check_shape`
  exposes allocation-free validation with structured `AggShapeError` diagnostics. ([#117][] by [@waywardmonkeys][])
- Reject direct calls whose known aggregate argument shapes contradict the callee contract.
  Exhaustive matches on `VerifyError` must handle `CallArgShapeMismatch`. ([#116][] by [@waywardmonkeys][])
- Reject impossible collection counts in program sections and instruction register lists before
  reserving storage. ([#105][] by [@waywardmonkeys][])
- Apply the call-depth limit to the entry frame, including a zero-depth limit. ([#106][] by [@waywardmonkeys][])
- Trap on `f64` conversions at the exclusive `i64` and `u64` upper bounds. ([#106][] by [@waywardmonkeys][])
- Preserve known aggregate shapes across jumps, branch joins, and loop backedges. ([#107][] by [@waywardmonkeys][])
- Report incorrect entry argument counts as `Trap::ArityMismatch` rather than `Trap::InvalidPc`. ([#113][] by [@waywardmonkeys][])

## [0.0.1][] (2026-05-31)

This release has an [MSRV][] of 1.88.

This is the initial release of Execution Tape, a `no_std` bytecode container, verifier, register VM
runtime, host-call ABI, tracing API, and disassembler.

[@no-materials]: https://github.com/no-materials
[@waywardmonkeys]: https://github.com/waywardmonkeys

[#124]: https://github.com/forest-rs/execution/pull/124
[#125]: https://github.com/forest-rs/execution/pull/125
[#126]: https://github.com/forest-rs/execution/pull/126
[#127]: https://github.com/forest-rs/execution/pull/127
[#128]: https://github.com/forest-rs/execution/pull/128
[#129]: https://github.com/forest-rs/execution/pull/129
[#130]: https://github.com/forest-rs/execution/pull/130
[#131]: https://github.com/forest-rs/execution/pull/131
[#132]: https://github.com/forest-rs/execution/pull/132
[#133]: https://github.com/forest-rs/execution/pull/133
[#141]: https://github.com/forest-rs/execution/pull/141

[#134]: https://github.com/forest-rs/execution/pull/134
[#135]: https://github.com/forest-rs/execution/pull/135
[#136]: https://github.com/forest-rs/execution/pull/136
[#137]: https://github.com/forest-rs/execution/pull/137
[#138]: https://github.com/forest-rs/execution/pull/138

[#93]: https://github.com/forest-rs/execution/pull/93
[#105]: https://github.com/forest-rs/execution/pull/105
[#106]: https://github.com/forest-rs/execution/pull/106
[#107]: https://github.com/forest-rs/execution/pull/107
[#109]: https://github.com/forest-rs/execution/pull/109
[#111]: https://github.com/forest-rs/execution/pull/111
[#112]: https://github.com/forest-rs/execution/pull/112
[#113]: https://github.com/forest-rs/execution/pull/113

[#116]: https://github.com/forest-rs/execution/pull/116
[#117]: https://github.com/forest-rs/execution/pull/117
[#118]: https://github.com/forest-rs/execution/pull/118
[#119]: https://github.com/forest-rs/execution/pull/119
[#120]: https://github.com/forest-rs/execution/pull/120

[#123]: https://github.com/forest-rs/execution/pull/123

[Unreleased]: https://github.com/forest-rs/execution/compare/execution_tape-v0.0.1...HEAD
[0.0.1]: https://github.com/forest-rs/execution/releases/tag/execution_tape-v0.0.1

[#144]: https://github.com/forest-rs/execution/pull/144

[#145]: https://github.com/forest-rs/execution/pull/145

[#146]: https://github.com/forest-rs/execution/pull/146

[#147]: https://github.com/forest-rs/execution/pull/147

[#148]: https://github.com/forest-rs/execution/pull/148

[#149]: https://github.com/forest-rs/execution/pull/149

[#150]: https://github.com/forest-rs/execution/pull/150

[#161]: https://github.com/forest-rs/execution/pull/161
[#160]: https://github.com/forest-rs/execution/pull/160
[#159]: https://github.com/forest-rs/execution/pull/159
[#157]: https://github.com/forest-rs/execution/pull/157
[#156]: https://github.com/forest-rs/execution/pull/156
[#155]: https://github.com/forest-rs/execution/pull/155
[#151]: https://github.com/forest-rs/execution/pull/151

[#143]: https://github.com/forest-rs/execution/pull/143

[MSRV]: README.md#minimum-supported-rust-version-msrv
