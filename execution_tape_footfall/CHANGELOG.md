<!-- Instructions

This changelog follows <https://keepachangelog.com/en/>.
Categories: added, changed, deprecated, removed, fixed, security.

-->

# Changelog

Changes for the initial Execution Tape Footfall [0.1.0](#010-2026-10-27) release, dated 2026-10-27, are documented below.

## [Unreleased][]

This release has an [MSRV][] of 1.88.

### Added

- Report cancelled resumable invocations distinctly and close logical scopes across quanta.
  Elapsed scope time includes suspension; already performed host effects remain.

## [0.1.0][] (2026-10-27)

This release has an [MSRV][] of 1.88.

### Added

- Resolve captured function/PC locations offline through core source maps. The capture example
  exports an exact program/source pair and references its source sidecar in the manifest;
  event emission performs no source lookup or source-text copying. ([#199][] by [@waywardmonkeys][])

- Provide `FootfallSink` for VM run, guest-call, host-call and optional instruction observations.
  The adapter uses `no_std + alloc`; applications own runtime/export backends and program contexts.
  Capture-level admission and scope-token correlation preserve explicit incomplete-evidence semantics. ([#199][] by [@waywardmonkeys][])

[Unreleased]: https://github.com/forest-rs/execution/compare/execution_tape_footfall-v0.1.0...HEAD
[0.1.0]: https://github.com/forest-rs/execution/releases/tag/execution_tape_footfall-v0.1.0
[MSRV]: README.md
[#199]: https://github.com/forest-rs/execution/pull/199
[@waywardmonkeys]: https://github.com/waywardmonkeys
