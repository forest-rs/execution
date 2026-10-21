<!-- Instructions

This changelog follows the patterns described here: <https://keepachangelog.com/en/>.

Subheadings to categorize changes are `added, changed, deprecated, removed, fixed, security`.

-->

# Changelog

The latest published Execution Graph release is [0.0.1](#001-2026-05-31) which was released on 2026-05-31.
You can find its changes [documented below](#001-2026-05-31).

## [Unreleased][]

This release has an [MSRV][] of 1.88.

### Added

- Add composable `ReportDetailMask` methods and bitwise operators, with `Default` selecting no
  optional details. ([#94][] by [@waywardmonkeys][])
- Add advisory node labels through `set_node_label`, `clear_node_label`, and `node_label`, with
  labels available in reports and Graphviz DOT output. ([#95][] by [@waywardmonkeys][])
- Add `NodeRunDetail::why_path_traced` to distinguish traced cause paths from fallback paths. ([#96][] by [@waywardmonkeys][])
- Add the `Executor` trait and typed `NodeAccess` for node execution and dependency recording.
  `TapeExecutor` and `TapeNode` adapt verified tape programs through the default-on `tape` feature. ([#97][] by [@waywardmonkeys][])
- Add `FnExecutor` and `FnNode` for native Rust closures. Custom executors can mix native and tape
  nodes in one graph. ([#97][] by [@waywardmonkeys][])
- Add executor accessors and `node_description`; `Executor::describe` supplies descriptions for
  Graphviz DOT output. ([#97][] by [@waywardmonkeys][])
- Add opt-in early cutoff through `Executor::values_equal`, `FnExecutor::with_value_eq`, and
  `TapeExecutor::set_early_cutoff`, with cutoff counts and details in reports. Tape cutoff compares
  plain values, using float bit patterns; handles always count as changed. ([#98][] by [@waywardmonkeys][])
- Add `GraphError::DependencyCycle` for rejected cyclic connections. ([#100][] by [@waywardmonkeys][])
- Add positional `InputId` and `OutputId`, indexed connections and output lookup, numeric external
  input keys, and name/ID lookup for construction and diagnostics. ([#103][] by [@waywardmonkeys][])
- Add `remove_node`, returning the retired body and outputs while dirtying consumers and reclaiming
  storage. Public node identities are never reused. ([#103][] by [@waywardmonkeys][])
- Add deduplicated batch invalidation through `invalidate_many` and live/reusable storage counts
  through `storage_stats`. ([#103][] by [@waywardmonkeys][])
- Add dynamic output reads and explicit `NodeOutcome::Pending` restarts, with `FnNode::restartable`.
  Pending attempts publish no outputs; missing resources, writes before suspension, and runtime
  dependency cycles have explicit errors. ([#104][] by [@waywardmonkeys][])
- Add committed dependency and cache-status queries, plus executor-attempt and suspension counts
  in summaries and partial reports. ([#104][] by [@waywardmonkeys][])

### Changed

- `TapeError` no longer implements `PartialEq`/`Eq`, following typed host failures in Execution
  Tape. Match variants and compare fields or inspect the standard error source chain instead. ([#133][] by [@waywardmonkeys][])

- Reuse the graph output buffer directly in `TapeExecutor`, avoiding a temporary result vector. ([#119][] by [@waywardmonkeys][])
- Improve crate and README documentation, keeping examples synchronized. ([#93][] by [@no-materials][])
- Mark public report output structs as `non_exhaustive`. ([#94][] by [@waywardmonkeys][])
- Include node labels in `ReportDetailMask::FULL`. ([#95][] by [@waywardmonkeys][])
- Avoid traced planning when reports request no optional details, while still reporting executed
  nodes in order. ([#96][] by [@waywardmonkeys][])
- Parameterize `ExecutionGraph` by an executor, `GraphError` by the executor's error type, and
  `NodeOutputs` by its value type. `ExecutionGraph::new` accepts an executor; tape configuration
  and `set_strict_deps` move to `TapeExecutor`. ([#97][] by [@waywardmonkeys][])
- Track observed resource revisions so already-observed writes do not cause reruns and early
  cutoff works across scoped queries. ([#102][] by [@waywardmonkeys][])
- Replace the `invalidation` dependency with an internal store that visits pending dependencies
  without scanning unrelated dirty roots. ([#102][] by [@waywardmonkeys][])
- Store output values in declaration order and dependency lists in reusable shared ranges.
  Reclaim unused host keys and avoid workspace-sized cleanup for small queries. ([#103][] by [@waywardmonkeys][])

### Removed

- Remove tape-specific `GraphError` variants `BadEntryFunc`, `BadInputArity`, `StrictDepsViolation`,
  and `Trap`; these are now `TapeError` variants wrapped by `GraphError::InvalidNode` or
  `GraphError::Node`. ([#97][] by [@waywardmonkeys][])

### Fixed

- Stop a node's own host writes from continually scheduling it again, while still invalidating
  other readers of the written keys. ([#90][] by [@no-materials][])
- Ignore host writes to graph-owned inputs, including when checking strict dependency reporting. ([#90][] by [@no-materials][])
- Preserve failed and unrun dirty work after dispatch errors so subsequent runs can retry it,
  and reclaim the schedule buffer on error as well as success. ([#91][] by [@no-materials][])
- Fix the README Quick Start's `Result` handling so the example compiles. ([#93][] by [@no-materials][])
- Honor report detail bits independently: requesting `WHY_PATH` no longer implicitly fills
  `because_of`. ([#94][] by [@waywardmonkeys][])
- Preserve partial execution reports in `GraphError::RunReportFailed` when a node fails, accessible
  through `GraphError::partial_report`. ([#96][] by [@waywardmonkeys][])
- Reject duplicate output names with `GraphError::DuplicateOutput` instead of silently aliasing
  them. ([#97][] by [@waywardmonkeys][])
- Preserve pending work outside a scoped run's dependency closure, including work sharing an
  invalidated input or recomputed output. Retain its cause information across scoped runs. ([#99][] and [#102][] by [@waywardmonkeys][])
- Validate dependencies before publishing outputs. Rejected wiring and failed node attempts leave
  committed outputs and dependencies intact; earlier successful publications survive later failure.
  Host mutations are outside graph rollback. ([#100][] by [@waywardmonkeys][])
- Rebinding an input to another resource updates its dependency edge and resets recorded reads,
  so invalidation reaches the node before its next execution. ([#103][] by [@waywardmonkeys][])
- Invalidate every output of a directly forced producer so readers of sibling outputs cannot return
  stale cached values before the producer runs. ([#104][] by [@waywardmonkeys][])

## [0.0.1][] (2026-05-31)

This release has an [MSRV][] of 1.88.

This is the initial release of Execution Graph, a `no_std` incremental execution graph for
dirty-tracked re-execution of verified `execution_tape` programs.

[@no-materials]: https://github.com/no-materials
[@waywardmonkeys]: https://github.com/waywardmonkeys

[#90]: https://github.com/forest-rs/execution/pull/90
[#91]: https://github.com/forest-rs/execution/pull/91
[#93]: https://github.com/forest-rs/execution/pull/93
[#94]: https://github.com/forest-rs/execution/pull/94
[#95]: https://github.com/forest-rs/execution/pull/95
[#96]: https://github.com/forest-rs/execution/pull/96
[#97]: https://github.com/forest-rs/execution/pull/97
[#98]: https://github.com/forest-rs/execution/pull/98
[#99]: https://github.com/forest-rs/execution/pull/99
[#100]: https://github.com/forest-rs/execution/pull/100
[#102]: https://github.com/forest-rs/execution/pull/102
[#103]: https://github.com/forest-rs/execution/pull/103
[#104]: https://github.com/forest-rs/execution/pull/104

[#119]: https://github.com/forest-rs/execution/pull/119

[#133]: https://github.com/forest-rs/execution/pull/133

[Unreleased]: https://github.com/forest-rs/execution/compare/execution_graph-v0.0.1...HEAD
[0.0.1]: https://github.com/forest-rs/execution/releases/tag/execution_graph-v0.0.1

[MSRV]: README.md#minimum-supported-rust-version-msrv
