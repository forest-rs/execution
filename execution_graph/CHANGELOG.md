<!-- Instructions

This changelog follows the patterns described here: <https://keepachangelog.com/en/>.

Subheadings to categorize changes are `added, changed, deprecated, removed, fixed, security`.

-->

# Changelog

The latest published Execution Graph release is [0.0.1](#001-2026-05-31) which was released on 2026-05-31.
You can find its changes [documented below](#001-2026-05-31).

## [Unreleased]

### Added

- Added advisory graph node labels via `ExecutionGraph::set_node_label`,
  `ExecutionGraph::clear_node_label`, and `ExecutionGraph::node_label`; labels can be included in
  execution reports with `ReportDetailMask::NODE_LABEL` and are rendered in Graphviz DOT output.

### Changed

- `ReportDetailMask::FULL` now includes `ReportDetailMask::NODE_LABEL`.

## [0.0.1][] (2026-05-31)

This release has an [MSRV][] of 1.88.

This is the initial release of Execution Graph, a `no_std` incremental execution graph for
dirty-tracked re-execution of verified `execution_tape` programs.

[Unreleased]: https://github.com/forest-rs/execution/compare/execution_graph-v0.0.1...HEAD
[0.0.1]: https://github.com/forest-rs/execution/releases/tag/execution_graph-v0.0.1

[MSRV]: README.md#minimum-supported-rust-version-msrv
