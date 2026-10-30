# execution_graph

<!-- We use cargo-rdme to keep this README in sync with the crate-level docs in src/lib.rs.
To update the section below, edit the doc comment in src/lib.rs, then run:
cargo rdme --workspace-project=execution_graph --heading-base-level=0
Full documentation at https://github.com/orium/cargo-rdme -->

<!-- cargo-rdme start -->

Incremental execution graph with pluggable node executors.

This crate provides a small `no_std` graph that runs nodes in dependency order and re-runs
only the nodes affected by a change. The graph owns node identity, input bindings, dependency
tracking, dirty propagation, scheduling, and reporting. What a node *is* and how it runs is
supplied by an [`Executor`]: the value type carried on edges, the node body type, and the
code that turns bound inputs into outputs.

Every node declares at least one output, which provides its execution identity. For work
without a data result, declare a completion output and return a unit value. Zero-output
definitions are rejected by [`GraphError::NoOutputs`].

Two executors ship with the crate:
- [`FnExecutor`] runs native Rust closures ([`FnNode`]).
- [`TapeExecutor`] runs verified `execution_tape` programs ([`TapeNode`]); it is behind the
  default-on `tape` feature.

An embedder that needs both kinds of node in one graph writes an executor whose node type is
an enum over the two and delegates each variant.

## Quick Start

```rust
use core::convert::Infallible;

use execution_graph::{ExecutionGraph, FnExecutor, FnNode};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut graph = ExecutionGraph::new(FnExecutor::<i64, Infallible>::new());

    let double = graph.add_node(
        FnNode::named("double", |inputs, outputs, _access| {
            outputs.push(inputs[0] * 2);
            Ok(())
        }),
        vec!["x".into()],
        vec!["doubled".into()],
    )?;
    let increment = graph.add_node(
        FnNode::named("increment", |inputs, outputs, _access| {
            outputs.push(inputs[0] + 1);
            Ok(())
        }),
        vec!["value".into()],
        vec!["result".into()],
    )?;

    graph.set_input_value(double, "x", 20)?;
    graph.connect(double, "doubled", increment, "value")?;

    let summary = graph.run_all()?;
    assert_eq!(summary.executed_nodes, 2);
    assert_eq!(graph.node_outputs(increment).unwrap().get("result"), Some(&41));

    // Nothing changed, so nothing re-runs.
    assert_eq!(graph.run_all()?.executed_nodes, 0);

    // Invalidating an input by name re-runs exactly its dependents.
    graph.set_input_value(double, "x", 21)?;
    graph.invalidate_input("x");
    assert_eq!(graph.run_all()?.executed_nodes, 2);
    assert_eq!(graph.node_outputs(increment).unwrap().get("result"), Some(&43));
    Ok(())
}
```

## Model

- **Nodes** are executor-defined bodies with named positional inputs and named outputs.
- **Edges** represent data dependencies; they are recorded dynamically from each node run:
  - reading an external input records `ResourceKey::Input(name)`
  - reading another node's output records `ResourceKey::NodeOutput { node, output }`
  - executors record additional dependencies through the [`NodeAccess`] they receive
- **Invalidation** is done by name: calling `invalidate_input("foo")` marks the input key
  `ResourceKey::Input("foo")` dirty, which may trigger re-execution of transitive dependents.

Input names are part of the dependency key space: the string you pass to `set_input_value(node,
"foo", ..)` must match the string you pass to `invalidate_input("foo")` for incremental
scheduling to work.

Executor-managed state uses the same key space: if a node records a
[`ResourceKey::HostState { op, key }`](ResourceKey::HostState) read during execution, you can
invalidate that state later via [`ExecutionGraph::invalidate`].

## Deferred work

[`NodeOutcome::Pending`] remains a discard-and-restart protocol for dynamic graph reads.
[`NodeOutcome::Deferred`] transfers an owned [`NodeTask`] whose polls continue the same
attempt. Independent nodes can run while it waits; consumers remain blocked and cannot use
an old cached output as current. Old caches stay available through `node_outputs`.

Use [`FnNode::deferred`] for a task-producing native node, or return the outcome from a custom
executor. A task owns its inputs, continuation and tentative values. Its successful result
crosses the normal arity/dependency/publication boundary. Reads observed before waiting are
retained provisionally, without replacing committed dependencies. Rebinding or changing a
read cancels obsolete work. If an upstream output still needs verification, the task waits
for that verification; an unchanged result preserves its continuation.
Supersession discovered while resolving producers ends that node's visit and requests a
fresh query. It never repeatedly restarts an effectful body inside the same query.

```rust
use std::{cell::Cell, convert::Infallible, rc::Rc, task::{Context, Poll}};
use execution_graph::{ExecutionGraph, FnExecutor, FnNode, NodeAccess, NodeTask, NodeStatus};

#[derive(Debug)]
struct Reply(Rc<Cell<Option<i64>>>);
impl NodeTask<i64, Infallible> for Reply {
    fn poll(&mut self, _: &mut Context<'_>, out: &mut Vec<i64>, _: &mut NodeAccess<'_, i64>)
        -> Poll<Result<(), Infallible>>
    {
        match self.0.take() {
            Some(value) => { out.push(value); Poll::Ready(Ok(())) }
            None => Poll::Pending,
        }
    }
}
let reply = Rc::new(Cell::new(None));
let mut graph = ExecutionGraph::new(FnExecutor::<i64, Infallible>::new());
let waiting = graph.add_node(FnNode::deferred({
    let reply = Rc::clone(&reply);
    move |_, _| Ok(Box::new(Reply(Rc::clone(&reply))))
}), vec![], vec!["result".into()]).unwrap();
let ready = graph.add_node(FnNode::new(|_, out, _| { out.push(42); Ok(()) }),
    vec![], vec!["result".into()]).unwrap();
let summary = graph.run_all().unwrap();
assert_eq!(summary.waiting_nodes, 1);
assert_eq!(graph.node_status(ready), Some(NodeStatus::Current));
assert_eq!(graph.node_status(waiting), Some(NodeStatus::Waiting));
reply.set(Some(7)); // This small example drives readiness manually.
graph.run_node(waiting).unwrap();
assert_eq!(graph.node_outputs(waiting).unwrap().get("result"), Some(&7));
```

No async runtime or worker thread is created by the graph. Manually query again, or install
an embedding waker with [`ExecutionGraph::set_task_waker`]. A task registers that waker with
its external source. A wake requests another query; it does not reenter or mutate the graph.
Ordinary pending tasks are polled at most once per query; requested graph producers may
permit additional polls when they become current. A completed or cancelled attempt is never
polled again. Unwinding from a poll poisons its continuation; the next query cancels it and
reports [`GraphError::TaskAbandoned`].

[`ExecutionGraph::task_info`] inspects an active attempt without polling it.
[`ExecutionGraph::task_dependencies`] borrows its provisional read prefix, separately from
the last successful publication's `node_dependencies`.
[`ExecutionGraph::cancel_task`] drops it and retains the previous published cache. A later
demand can start a fresh attempt. Removing the node or dropping the graph also drops owned
work. This retires the continuation and prevents late publication; whether external work
stops is the embedding's cancellation policy. External effects cannot be rolled back.

Task-event collection is opt-in through [`ExecutionGraph::set_collect_task_events`].
[`ExecutionGraph::take_task_events`] includes start, wait, publication and cancellation,
correlated by graph-local [`NodeAttempt`]. It records transitions, not CPU time or a replay
of native effects. Reports separately list waiting and blocked nodes and count task polls;
a body becoming ready does not count as publication until validation succeeds.

## Early cutoff

A node re-runs when something it read changed, but its new output may equal the old one (a
parameter nudged within a clamp, say). With early cutoff, such an unchanged output stops
propagation: a scheduled node whose reads all turn out unchanged is skipped instead of re-run.
[`RunSummary::cut_off_nodes`] counts these nodes and [`RunDetailReport::cut_off`] lists them
with the same cause detail as executed nodes.

Whether an output changed is the executor's call, through [`Executor::values_equal`]. The
default says "changed" for every output, so cutoff is opt-in: enable it with
[`FnExecutor::with_value_eq`] or [`TapeExecutor::set_early_cutoff`]. Cutoff decisions are
per output and across runs:

- graph inputs and executor state that were invalidated count as changed;
- a node output marked dirty directly forces its producer to run; its readers execute only
  when an output revision changes or another read changed;
- host writes advance resource revisions immediately, including writes from a failed attempt;
- pending readers outside a scoped run retain their causes and compare revisions when queried,
  so an unchanged output can cut off work across separate scoped runs;
- a node that has never run, or was rewired by `connect` or by `set_input_value` binding a
  different key since its last run, always runs; both rewirings also schedule the node.

A cut-off node keeps the outputs, run count, and last access log of its last real run.

Graph construction is checked at the public API boundary: `add_node`, `set_input_value`, and
`connect` return [`GraphError`] values for duplicate output names, input arity mismatches,
unknown input names, and unknown output names.

## Ports and node lifetime

[`InputId`] and [`OutputId`] identify positional ports within a node. Resolve names once with
[`ExecutionGraph::input_id`] and [`ExecutionGraph::output_id`], then use
[`ExecutionGraph::connect_by_id`] and [`NodeOutputs::get_by_id`] for indexed access.
[`ExecutionGraph::input_name`] and [`ExecutionGraph::output_name`] resolve IDs for diagnostics.
External input keys are separate from input slots: [`ResourceKey::InputId`] identifies a
resource shared by any readers, while `InputId(0)` means the first slot of a particular node.
[`ExecutionGraph::set_input_value_by_id`] binds these explicitly.
[`ExecutionGraph::node_input_source`] borrows the current authored value/key or producer port;
an unbound slot returns `None`. [`ExecutionGraph::clear_input`] disconnects one positional slot
while preserving cached outputs and committed reads. Execution reports a missing input until
it is rebound. Inspect [`ExecutionGraph::node_dependencies`] for the last successful read set,
and [`ExecutionGraph::node_status`] for whether those cached outputs are current.

[`ExecutionGraph::remove_node`] retires a node identity permanently and returns ownership of
its body and cached outputs. Its consumers become dirty; stale connections report the reader,
removed producer, and requested output until reconnected. Physical node and resource slots
are reused without reusing public identities. [`ExecutionGraph::storage_stats`] reports live
counts and reusable capacities for checking create/remove workloads.

[`ExecutionGraph::invalidate_many`] accepts and deduplicates a batch of resource keys.
Invalidating an unknown resource has no effect: future readers execute before they can cache
a value, and there is no existing consumer to invalidate.

## Dynamic output reads

[`FnNode::restartable`] and custom [`Executor`] implementations can discover graph-output
dependencies while running. [`NodeAccess::read_node_output`] returns a cloned current value,
or `Ok(None)` when the producer needs execution or verification. Return [`NodeOutcome::Pending`]
to discard the tentative outputs/reads, update those producers, and restart the computation.
Missing or retired identities produce [`OutputReadError`], rather than a cached substitute.

```rust
use execution_graph::{ExecutionGraph, FnExecutor, FnNode, NodeOutcome, OutputId, OutputReadError};

let mut graph = ExecutionGraph::new(FnExecutor::<i64, OutputReadError>::new());
let parent = graph.add_node(FnNode::new(|_, outputs, _| {
    outputs.push(10); Ok(())
}), vec![], vec!["out".into()])?;
let child = graph.add_node(FnNode::restartable(move |_, outputs, access| {
    let Some(value) = access.read_node_output(parent, OutputId::new(0))? else {
        return Ok(NodeOutcome::Pending);
    };
    outputs.push(value + 1);
    Ok(NodeOutcome::Complete)
}), vec![], vec!["out".into()])?;
let summary = graph.run_node(child)?;
assert_eq!(summary.executed_nodes, 2);
assert_eq!(summary.execution_attempts, 3); // child yields, parent publishes, child publishes
assert_eq!(graph.node_outputs(child).unwrap().get("out"), Some(&11));
```

Restarting is explicit: ordinary [`FnNode::new`] and [`FnNode::named`] closures never request
it. A completed attempt with unresolved reads is rejected. A pending attempt must be safe to
repeat and must not record host writes; arbitrary executor or external effects cannot be
detected or rolled back. A producer may perform effects when it completes, just as in a
statically wired graph. Execution errors remain failures, even after a pending read.

Successful execution replaces the committed read set, so a conditional branch can remove a
dependency and discover it again later. With unchanged wiring, scheduling conservatively visits
previously committed dependencies before the reader; new runtime requests extend that scope.
After rewiring, scheduling and cycle checks use the current authored inputs until publication.
For example, replacing `A → B` with a literal input on B allows connecting `B → A` immediately,
without executing either node during the edit. The old outputs and committed reads remain
available; historical reads still propagate invalidation until a successful publication replaces
them. Dynamic reads are rediscovered at runtime and can still report a cycle or failed attempt.
Native readers can
request tape outputs in a mixed executor, and tape input bindings can consume native outputs.
The tape adapter itself completes whole VM calls and does not suspend host calls.

[`ExecutionGraph::node_dependencies`] exposes committed reads without enabling access logs.
[`ExecutionGraph::node_status`] distinguishes a current cache from pending or never-published
results; [`ExecutionGraph::node_outputs`] alone is a cached-value accessor. Status reflects
changes reported to the graph, not unreported host mutation. Run summaries and partial reports
count executor attempts and suspensions separately from successful publications.

Run `cargo run -p execution_graph_examples --bin transforms` for conditional parent-world
reads, reset toggles, dependency inspection, and bitwise checks against a fresh evaluator.

## Tape programs

With the `tape` feature, [`TapeExecutor`] runs verified `execution_tape` programs as nodes.
Host calls record dependency keys through `execution_tape::host::AccessSink`, and those keys
are translated into graph [`ResourceKey`]s.

Tape edges carry [`TapeValue`], which owns its immutable result storage. Clone a cached
output to retain it across edits or removal; it remains readable after the executor drops.
Use [`TapeValue::value`] and [`TapeValue::aggregates`] for borrowed inspection. Native nodes
can adopt portable tuple snapshots through [`TapeValue::from_snapshot`] without a program.
Function, closure and nominal aggregate IDs require their exact original prepared program,
including IDs nested inside tuples. Foreign and unowned IDs fail before VM or host execution.
Use [`TapeValue::retain_callable`] to keep a published function/closure independently executable
after its graph and executor disappear. It retains the exact code and copies reachable
immutable captures; host object lifetimes remain the embedding's responsibility.

Edge clones share storage. At invocation ingress, the adapter copies only selected reachable
input graphs into VM storage, preserving shared node identity across publications. Returned
aggregate storage moves into an owned bundle without cloning payloads. One retained aggregate
output keeps its publication bundle alive; unrelated scalar payload outputs have separate
owners. Host-object tokens still require their host's own retention policy. Temporary payloads
are cleared after attempts; reusable workspace capacity is distinct from published storage.

[`TapeExecutor::with_observer`] installs a [`TapeObserver`] that selects individual invocations
for any VM [`TraceSink`](execution_tape::trace::TraceSink), including recording or profiling.
[`TapeInvocation`] correlates each selected body with its graph node and attempt. Observers
receive adapter completion before graph publication validation, so a successful recording
does not prove that the graph committed the output. Cache hits produce no VM invocation.

```rust
use std::sync::Arc;

use execution_graph::{ExecutionGraph, TapeExecutor, TapeValue};
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

let mut asm = Asm::new();
asm.set_register_types(vec![ValueType::Unit, ValueType::I64, ValueType::I64, ValueType::I64]);
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

let mut graph = ExecutionGraph::new(TapeExecutor::new(NoHost, Limits::default()));
let node = graph.add_tape_node(program, entry, vec!["x".into()])?;
graph.set_input_value(node, "x", Value::I64(41).into())?;

let summary = graph.run_all()?;
assert_eq!(summary.executed_nodes, 1);
assert_eq!(graph.node_outputs(node).unwrap().get("y").map(TapeValue::value), Some(&Value::I64(42)));
```

[`TapeExecutor::set_strict_deps`] enables a debugging mode that rejects host calls which
record no access keys, so missing dependency reporting fails loudly instead of silently
reusing stale results.

## Execution behavior

Each successful node publishes its outputs and recorded dependencies together, after output
arity and dependency validation. A failed node keeps its previous outputs, dependencies, and
access log and remains dirty. Earlier successful nodes remain committed if a later node fails:
a graph run is not a transaction. Executor and host mutations are never rolled back by the
graph; output handles must remain valid for as long as the graph owns them.

Cyclic connections return [`GraphError::DependencyCycle`] without changing the graph's wiring.

`run_node` drains and executes only the dirty work within the dependency closure of the target
node's outputs, leaving unrelated dirty work dirty to be handled by a later `run_all`.

For low overhead telemetry, `run_all` / `run_node` return only an executed-node summary.

For debugging and instrumentation:
- `run_all_with_report` / `run_node_with_report` accept a `ReportDetailMask` so you can choose
  cheaper detail levels (for example, node + immediate cause key without path tracing).
- Use `set_node_label` to attach advisory debug names that can appear in reports and DOT.
- Use `ReportDetailMask::FULL` when you want labels plus full per-node cause paths.
- If a report-producing run fails after some nodes complete, `GraphError::RunReportFailed`
  carries the partial report rows collected before the error.

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

- The built-in tape adapter still executes synchronously. A deferred custom executor can
  own native work; this task boundary alone does not turn borrowed VM invocations into
  independently owned continuations or choose an external scheduling policy.
- The tape executor collapses VM traps to [`TapeError::Trap`] at the graph boundary rather
  than source-language diagnostics.

<!-- cargo-rdme end -->

## Minimum supported Rust Version (MSRV)

This crate has been verified to compile with **Rust 1.88** and later.
