<!-- Instructions

This changelog follows the patterns described here: <https://keepachangelog.com/en/>.

Subheadings to categorize changes are `added, changed, deprecated, removed, fixed, security`.

-->

# Changelog

The latest published Execution Tape release is [0.0.1](#001-2026-05-31) which was released on 2026-05-31.
You can find its changes [documented below](#001-2026-05-31).

## [Unreleased][]

This release has an [MSRV][] of 1.88.

### Changed

- Improve crate and README documentation, keeping examples synchronized. ([#93][] by [@no-materials][])
- Reduce verifier allocations by reusing dataflow buffers and storing aggregate metadata sparsely.
  Verification only computes liveness-based diagnostics through the `_with_lints` entry points. ([#109][] by [@waywardmonkeys][])
- Decode bytecode and span tables directly into packed arenas, removing temporary per-function
  allocations and repacking copies. ([#111][] by [@waywardmonkeys][])
- Release virtual-register mappings after lowering to reduce retained execution storage. ([#112][] by [@waywardmonkeys][])

### Fixed

- Reject direct calls whose known aggregate argument shapes contradict the callee contract.
  Exhaustive matches on `VerifyError` must handle `CallArgShapeMismatch`.

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

[Unreleased]: https://github.com/forest-rs/execution/compare/execution_tape-v0.0.1...HEAD
[0.0.1]: https://github.com/forest-rs/execution/releases/tag/execution_tape-v0.0.1

[MSRV]: README.md#minimum-supported-rust-version-msrv
