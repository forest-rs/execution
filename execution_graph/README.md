# execution_graph

Incremental execution graph built on `execution_tape`.

This crate provides a small `no_std` graph that executes verified `execution_tape` programs as
nodes and re-executes only the nodes that are affected by changes.

## Quick Start

Use `execution_tape` to build verified programs, then wire them as graph nodes:

```rust
use std::sync::Arc;

use execution_graph::ExecutionGraph;
use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
use execution_tape::host::{Host, HostContext, HostError, SigHash, ValueRef};
use execution_tape::program::ValueType;
use execution_tape::value::Value;
use execution_tape::vm::Limits;

struct NoHost;

impl Host for NoHost {
    fn call(
        &mut self,
        _symbol: &str,
        _sig_hash: SigHash,
        _args: &[ValueRef<'_>],
        _rets: &mut [Value],
        _ctx: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        Err(HostError::UnknownSymbol)
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut asm = Asm::new();
    asm.const_i64(2, 1);
    asm.i64_add(3, 1, 2);
    asm.ret(0, &[3]);

    let mut builder = ProgramBuilder::new();
    let entry = builder.push_function_checked(
        asm,
        FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::I64],
        },
    )?;
    builder.set_function_output_name(entry, 0, "y")?;
    let program = Arc::new(builder.build_verified()?);

    let mut graph = ExecutionGraph::new(NoHost, Limits::default());
    let node = graph.add_node(program, entry, vec!["x".into()]);
    graph.set_input_value(node, "x", Value::I64(41));

    let summary = graph.run_all()?;
    assert_eq!(summary.executed_nodes, 1);
    assert_eq!(graph.node_outputs(node).unwrap().get("y"), Some(&Value::I64(42)));
    Ok(())
}
```

## Model

- **Nodes** are `(VerifiedProgram, entry FuncId)` pairs.
- **Edges** represent data dependencies; they are recorded dynamically from each node run:
  - reading an external input records `ResourceKey::Input(name)`
  - reading another node's output records `ResourceKey::NodeOutput { node, output }`
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

Graph construction is checked at the public API boundary: `add_node`, `set_input_value`, and
`connect` return `GraphError` values for unknown entry functions, input arity mismatches, unknown
input names, and unknown output names.

## Execution behavior

`run_node` drains and executes only the dirty work within the dependency closure of the target
node’s outputs, leaving unrelated dirty work dirty to be handled by a later `run_all`.

For low overhead telemetry, `run_all` / `run_node` return only an executed-node summary.

For debugging and instrumentation:
- `run_all_with_report` / `run_node_with_report` accept a `ReportDetailMask` so you can choose
  cheaper detail levels (for example, node + immediate cause key without path tracing).
- Use `ReportDetailMask::FULL` when you want full per-node cause paths.

## Demo

Run the demo with:

```sh
cargo run -p execution_graph_examples --bin tax
```

Emit Graphviz DOT for the same graph:

```sh
cargo run -p execution_graph_examples --bin tax -- --dot
```

## Current limitations

- `execution_graph` intentionally stays close to the VM: traps expose `execution_tape::vm::TrapInfo`
  rather than source-language diagnostics.
- VM traps are still collapsed to `GraphError::Trap` at the graph boundary. Missing inputs,
  missing upstream outputs, bad output arity, and strict-deps failures are reported with context.
- Graph nodes are currently `execution_tape` entrypoints only; custom dispatch can be layered later
  without changing the resource-key model.

## Minimum supported Rust Version (MSRV)

This crate has been verified to compile with **Rust 1.88** and later.
