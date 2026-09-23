<!-- Instructions

This changelog follows the patterns described here: <https://keepachangelog.com/en/>.

Subheadings to categorize changes are `added, changed, deprecated, removed, fixed, security`.

-->

# Changelog

The latest published Execution Graph release is [0.0.1](#001-2026-05-31) which was released on 2026-05-31.
You can find its changes [documented below](#001-2026-05-31).

## [Unreleased]

### Added

- Added the `Executor` trait: the graph no longer knows what a node body is. An executor supplies
  the value type carried on edges, the node body type, and the code that runs a node; the graph
  keeps dependency tracking, dirty propagation, scheduling, and reporting.
- Added `NodeAccess`, the per-run dependency recorder handed to an executor, with typed
  `read_input`, `read_host_state`, `read_opaque_host`, `write_host_state`, and
  `write_opaque_host` methods. Graph inputs cannot be written from a node by construction.
- Added `TapeExecutor`, `TapeNode`, and `TapeError` in the new `tape` module, which runs verified
  `execution_tape` programs as nodes. `ExecutionGraph<TapeExecutor<H>>::add_tape_node` derives
  input arity and output names from the program as `add_node` used to.
- Added the default-on `tape` cargo feature; `execution_tape` is now an optional dependency and
  the graph builds without it.
- Added `ExecutionGraph::executor`, `ExecutionGraph::executor_mut`, and
  `ExecutionGraph::node_description`; `Executor::describe` text is rendered in Graphviz DOT.
- Added `GraphError::DuplicateOutput`; `add_node` rejects repeated output names instead of
  silently aliasing them.
- Added `FnExecutor` and `FnNode`, a closure-backed executor for native Rust node bodies, so a
  graph of ordinary Rust operations needs no tape programs. A single graph can mix closure and
  tape nodes through an executor whose node type is an enum over both.
- Added early cutoff: a re-run output that the executor reports as unchanged through the new
  `Executor::values_equal` hook stops propagation, so scheduled dependents whose reads all turn
  out unchanged are skipped. It is opt-in: `values_equal` defaults to "changed",
  `FnExecutor::with_value_eq` enables it with a comparison, and `TapeExecutor::set_early_cutoff`
  enables it for plain tape values (floats by bit pattern; handles always count as changed).
  `RunSummary::cut_off_nodes` counts skipped nodes and `RunDetailReport::cut_off` lists them.
  Outputs marked dirty directly always run; if `run_node` retains pending work outside its
  closure as direct marks, cutoff is conservative for that deferred work.
- Added advisory graph node labels via `ExecutionGraph::set_node_label`,
  `ExecutionGraph::clear_node_label`, and `ExecutionGraph::node_label`; labels can be included in
  execution reports with `ReportDetailMask::NODE_LABEL` and are rendered in Graphviz DOT output.

### Changed

- `set_input_value` that binds a slot to a different key (for example replacing a `connect`ed
  output with an external value) now rewires like `connect`: it schedules the node and resets its
  recorded reads.
- `ExecutionGraph` is generic over an `Executor` instead of an `execution_tape` host:
  `ExecutionGraph::new(executor)` replaces `new(host, limits)`, and `add_node(body, input_names,
  output_names)` takes an executor node body plus explicit output names. Tape nodes use
  `add_tape_node(program, entry, input_names)`.
- `GraphError<E>` is parameterized by the executor's error type. Executor failures surface as
  `GraphError::Node { node, source }` and rejected node definitions as
  `GraphError::InvalidNode(source)`. `NodeOutputs<V>` is parameterized by the value type.
- `set_strict_deps` moved from `ExecutionGraph` to `TapeExecutor`; `invalidate_tape_key` is only
  available on graphs whose executor is a `TapeExecutor`.
- `ReportDetailMask::FULL` now includes `ReportDetailMask::NODE_LABEL`.
- `set_input_value` that rebinds a slot to a different key resets the node's recorded reads and
  adds a dependency edge to the new input, as `connect` does, so `invalidate_input` with the new
  name reaches a node that has not read it yet.

### Removed

- Removed `GraphError::BadEntryFunc`, `GraphError::BadInputArity`,
  `GraphError::StrictDepsViolation`, and `GraphError::Trap`; they are now `TapeError` variants
  wrapped in `GraphError::InvalidNode` or `GraphError::Node`.

### Fixed

- `run_node` and `run_node_with_report` no longer drop pending work outside the target's closure
  when the scoped drain takes a shared dirty root: nodes that read the same invalidated input, or
  another reader of an output the run recomputes, now run on the next `run_all`. Reports explain
  that deferred work from its original root after a traced scoped run; after an untraced one the
  path starts at the drained key and `why_path_traced` is `Some(false)`. Chained scoped runs keep
  the original root. A node the scoped run schedules is not run again for an output that is
  still dirty outside the closure, whether a scoped run kept that mark, an earlier scoped run
  kept it, or it was invalidated directly: the output's readers are marked instead, so they see
  the new value even under early cutoff. This also fixes a stale reader when two outputs of one
  node were invalidated directly and `run_node` targeted a reader of only one of them. Deferred
  work stays dirty when the drained key it depends on turns out
  unchanged, so early cutoff is conservative for it.
  The retention lives in one internal function that mirrors `invalidation`'s proposed
  `DrainBuilder::retain_out_of_scope` (forest-rs/invalidation#5); it switches to that option
  once an `invalidation` release with it publishes.

## [0.0.1][] (2026-05-31)

This release has an [MSRV][] of 1.88.

This is the initial release of Execution Graph, a `no_std` incremental execution graph for
dirty-tracked re-execution of verified `execution_tape` programs.

[Unreleased]: https://github.com/forest-rs/execution/compare/execution_graph-v0.0.1...HEAD
[0.0.1]: https://github.com/forest-rs/execution/releases/tag/execution_graph-v0.0.1

[MSRV]: README.md#minimum-supported-rust-version-msrv
