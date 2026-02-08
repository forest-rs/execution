# execution_graph

Incremental execution graph built on `execution_tape`.

This crate provides a small `no_std` graph that executes verified `execution_tape` programs as
nodes and re-executes only the nodes that are affected by changes.

## Model

- **Nodes** are `(VerifiedProgram, entry FuncId)` pairs.
- **Edges** represent data dependencies; they are recorded dynamically from each node run:
  - reading an external input records `ResourceKey::Input(name)`
  - reading another node's output records `ResourceKey::TapeOutput { node, output }`
  - host ops can record additional dependencies via `execution_tape::host::AccessSink`
- **Invalidation** is done by name: calling `invalidate_input("foo")` marks the input key
  `ResourceKey::Input("foo")` dirty, which may trigger re-execution of transitive dependents.

Input names are part of the dependency key space: the string you pass to `set_input_value(node,
"foo", ..)` must match the string you pass to `invalidate_input("foo")` for incremental scheduling
to work.

Host state invalidation uses the same key space: if a host op records a
`ResourceKeyRef::HostState { op, key }` read during execution, you can invalidate that state later
via `ExecutionGraph::invalidate_tape_key(...)` (or by constructing the corresponding owned
`execution_graph::ResourceKey` and calling `ExecutionGraph::invalidate(...)`).

## Execution behavior

`run_node` drains and executes only the dirty work within the dependency closure of the target
node’s outputs, leaving unrelated dirty work dirty to be handled by a later `run_all`.

For debugging and instrumentation, `run_all_with_report` / `run_node_with_report` return a
structured report that includes one plausible “why re-ran” cause path per executed node.

## Current limitations

- Error reporting is intentionally minimal (`GraphError::Trap` is opaque); richer error surfaces
  are expected to be layered on in follow-up PRs.
