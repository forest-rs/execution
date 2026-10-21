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

- Add owned `ValueSnapshot` retention through `AggHeap::snapshot` and `HostContext::retain`,
  with `AggHeap::import` for moving retained data into another heap. Snapshots copy only
  reachable aggregate data and preserve sharing. Host objects and function IDs keep their domains.

### Changed

- `AggHeap::tuple_new`, `struct_new`, and `array_new` now return `Result<AggHandle, AggError>`.
  Handle the result when constructing aggregates. Heap and staged constructors reject dangling
  aggregate children and closure environments before mutation, preventing forward references
  and cycles. Schema validation remains a separate operation.

- Verification now validates immutable metadata, including unused constant ranges, symbol and
  field-name ranges, struct layouts, and optional names. Malformed programs fail preparation
  with `VerifyError::InvalidMetadata` instead of trapping during execution. Exhaustive error
  matches must handle the new variant; `MetadataTable` identifies the failing table.

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

[MSRV]: README.md#minimum-supported-rust-version-msrv
