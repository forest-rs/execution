# `execution_tape`

<!-- We use cargo-rdme to keep this README in sync with the crate-level docs in src/lib.rs.
To update the section below, edit the doc comment in src/lib.rs, then run:
cargo rdme --workspace-project=execution_tape --heading-base-level=0
Full documentation at https://github.com/orium/cargo-rdme -->

<!-- cargo-rdme start -->

Portable, verifiable bytecode container and typed register VM.

`execution_tape` is the low-level execution layer for already-lowered programs. It owns the
portable program format, verifier, register VM, host-call ABI, aggregate values, tracing hooks,
and disassembly tools. It does not own language semantics, graph authoring, or host object
lifetimes.

The crate is `no_std + alloc` by default. The `std` feature is currently reserved for
integrations that need standard-library support.

## Quick Start

Build, verify, and run a one-function program:

```rust
extern crate alloc;

use alloc::vec;

use execution_tape::asm::{FunctionSig, ProgramBuilder};
use execution_tape::host::{Host, HostContext, HostError, SigHash, ValueRef};
use execution_tape::program::ValueType;
use execution_tape::trace::TraceMask;
use execution_tape::value::Value;
use execution_tape::vm::{Limits, Vm};

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
    let mut builder = ProgramBuilder::new();
    builder.set_program_name("add_one");
    let entry = builder.declare_function(FunctionSig {
        arg_types: vec![ValueType::I64],
        ret_types: vec![ValueType::I64],
    })?;
    let mut asm = builder.function_asm(entry)?;
    let one = asm.alloc_register(ValueType::I64);
    let result = asm.alloc_register(ValueType::I64);
    asm.const_i64(one, 1).i64_add(result, 1, one).ret(0, &[result]);
    builder.define_function(entry, asm)?;
    builder.set_function_input_name(entry, 0, "x")?;
    builder.set_function_output_name(entry, 0, "y")?;

    let program = builder.build_verified()?;
    let mut vm = Vm::new(NoHost, Limits::default());
    let out = vm.run(&program, entry, &[Value::I64(41)], TraceMask::NONE, None)?;
    assert_eq!(out, vec![Value::I64(42)]);
    Ok(())
}
```

For repeated execution, keep an [`ExecutionContext`](vm::ExecutionContext) and a result `Vec`.
Clear the result buffer between runs and call [`Vm::run_with_ctx`](vm::Vm::run_with_ctx)
to reuse both allocations. Results append in return-signature order; a trap preserves the
buffer's original contents. Aggregate results belong to the VM heap and survive context resets.

## Nested Guest Contracts

`ValueType` remains the storage/ABI kind. Intern shared compositional contracts when guest
arguments/results need richer structure, then use the ordinary function builder and VM:

```rust
use execution_tape::asm::{FunctionAbi, ProgramBuilder};
use execution_tape::contract::Contract;
use execution_tape::program::ValueType;
let mut builder = ProgramBuilder::new();
let integer = builder.contract(Contract::Kind(ValueType::I64)).unwrap();
let inner = builder.contract(Contract::Tuple(&[Some(integer)])).unwrap();
let outer = builder.contract(Contract::Tuple(&[Some(inner)])).unwrap();
let signature = FunctionAbi { args: vec![], rets: vec![outer.into()] };
```

Constructors and projections preserve nested facts; direct calls propagate enforced result
contracts. Joins retain common structure. Opaque boundary values are checked before callee entry
or result installation, using a separate bounded contract-work budget. Contract inspection
borrows definitions without expanding shared graphs. `Contract::Function` and `Contract::Closure`
describe exact exposed signatures. Their argument/result contract IDs also author indirect calls
through `ProgramBuilder::call_sig(FunctionAbi { ... })`. `Kind(Func)` and `Kind(Closure)` remain
opaque; exact callable matching implements no variance or subtyping. Closure environment layouts
stay hidden from their exposed signature and are checked against the concrete body before use.

Run `cargo run -p execution_tape_wind_tunnel --bin nested_contracts` for a complete example.
Run `cargo run -p execution_tape_wind_tunnel --bin typed_callable` for an indirect nested result.

## Source connections

[`source::SourceMap`] owns optional frontend snapshots and exact instruction-to-source ranges.
Bind a map to the exact program, then use [`source::BoundSources::lookup`] to borrow selected
text and named line/byte coordinates without a terminal dependency. Structured bounded
context and a standalone sidecar codec are available in the same `no_std + alloc` module.
Source snapshots are frontend-owned metadata: ordinary verification/execution never retains
a map or performs source lookup. Observatory consumes this API for its terminal panels.

## Resource policy

Unit/effect registers carry verifier facts without runtime payload storage. Their source
declarations and initialization/effect checks still apply. Unit-only moves, constants and
selects keep their instruction/fuel and observation boundaries; host/guest arguments and
results still require the declared kinds. Inspection counts Unit declarations separately
from the bytes occupied by payload-bearing register banks.

[`vm::Limits`] separately bounds instruction fuel, call depth, host-call count, contract work,
and logical byte/string work. `max_payload_work` defaults to 64 MiB per run: copying operations
and owned result/aggregate-field materialization charge bytes before copying, while comparisons
and UTF-8 conversion charge their documented input work. Direct host-owned bytes/strings are
charged before adoption, after the host has already executed. Borrowed inputs and guest-call
transfers cost zero. The counter resets every run; scalar instructions have no payload debit.

This is not a heap or process-memory limit. Storage of retained results, aggregate/frame storage,
arbitrary host effects/allocations, preparation, recording, and graph imports need their own
policies. Guest and `HostContext` tuple/struct/array construction share a separate
[`vm::Limits::max_aggregate_work`] budget (1,000,000 by default): one node plus its immediate
field count, charged before field allocation or payload copying. Empty nodes cost one;
nested handles do not traverse their contents. Staged host fields charge immediate payload
bytes before VM adoption, including work later discarded. The counter is shared across calls and
resets each run. Exhaustion reports [`vm::Trap::AggregateWorkExceeded`].
See [`vm::Limits::max_payload_work`] for exact byte/string charging rules and
[`vm::Trap::PayloadWorkExceeded`] for recoverable limit diagnostics.

## Inspecting Verifier Facts

`ProgramAnalysis::new(&program, &config)` verifies a borrowed source once. When a prepared
program already exists, `ProgramAnalysis::from(&prepared)` reuses its proof. Request one
function, then query source PCs without retaining register snapshots at every instruction:

```rust
use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
use execution_tape::program::ValueType;
use execution_tape::verifier::{ProgramAnalysis, ValueFact, VerifyConfig};
let mut body = Asm::new();
body.set_register_types(vec![ValueType::Unit, ValueType::I64, ValueType::Agg]);
body.const_i64(1, 42).tuple_new(2, &[1]);
let pc = body.pc();
body.tuple_get(1, 2, 0).ret(0, &[1]);
let mut builder = ProgramBuilder::new();
let entry = builder.push_function_checked(body, FunctionSig {
    arg_types: vec![], ret_types: vec![ValueType::I64],
}).unwrap();
let program = builder.build_checked().unwrap();
let proof = ProgramAnalysis::new(&program, &VerifyConfig::default()).unwrap();
let mut function = proof.function(entry).unwrap();
let facts = function.instruction(pc).unwrap();
let aggregate = facts.register(2).unwrap();
assert_eq!(aggregate.storage_kind, ValueType::Agg);
assert!(matches!(aggregate.before, ValueFact::Contract(_)));
```

Facts distinguish unreachable code, uninitialized registers, opaque kinds, and nested
contracts. IDs belong to the analysis's shared contract table. Origins identify arguments,
local defining instructions or incoming block facts. Incoming edges expose simultaneous
parameter bindings before their meet; external invocation seeds are separate from CFG edges.
“After” means local source dataflow: calls assume successful callee return, while branches
precede their outgoing edge transfers. These facts are not runtime observations.
Ordinary verification, preparation and execution retain no inspection data.

## Core Pieces

- `asm`: ergonomic builders for functions, call signatures, constants, host signatures, and
  bytecode emission.
- `contract`: shared compositional guest value contracts, authoring and borrowed kind views.
- `program`: serialized program model, type tables, constants, host signatures, and names.
- `verifier`: validation and lowering into an execution-ready `VerifiedProgram`.
- `vm`: bounded interpreter for verified programs.
- `host`: host-call trait, borrowed argument views, aggregate readers, and access recording
  hooks.
- `trace`: profiling events and opt-in borrowed invocation, host outcome and return-candidate
  values for recording and diagnostics.
- `recording`: owned bounded observations, instruction histories and checked replay.
- `disasm`: human-readable disassembly for verified programs.

## Resumable execution

`Vm::start` validates an invocation and returns a handle that borrows its VM, program, inputs,
and optional observers. `Invocation::resume` executes a scheduling quantum at complete
instruction boundaries. Hard fuel and data-work budgets accumulate across resumes; a quantum
is separate from these limits. Host calls run synchronously by default.

```rust
use execution_tape::{host::Host, verifier::VerifiedProgram, value::{FuncId, Value},
    vm::{InvocationOptions, RunStatus, TrapInfo, Vm}};
fn scheduled<H: Host>(vm: &mut Vm<H>, program: &VerifiedProgram, entry: FuncId,
    args: &[Value]) -> Result<Vec<Value>, TrapInfo> {
    let mut invocation = vm.start(program, entry, args, InvocationOptions::default())?;
    while invocation.resume(1024)? == RunStatus::Suspended {
        // Return control to the application's scheduler between quanta.
    }
    Ok(invocation.take_outputs().expect("successful root return"))
}
```

Dropping an unfinished invocation cancels it and rolls back its new aggregates. Host effects
remain performed. Trace scopes close with `TraceOutcome::Cancelled`; a cancelled recording is
incomplete and cannot be replayed as a finished invocation. Logical profiling scopes include
suspension time. Ordinary `run` and `run_with_ctx` remain run-to-completion conveniences and
use the unbounded dispatch specialization without scheduling checks.

A stored task can instead transfer its VM into [`callable::Callable::start_owned`].
This returns the same `Invocation` type with independently owned VM, code and inputs;
it needs no self-references or pinned VM allocation. Borrowed entry points still support
reusable VM storage. Observations can also be owned, including shared collectors:

```rust
use std::{cell::RefCell, rc::Rc};
use execution_tape::{callable::Callable, host::Host,
    recording::{Recorder, RecordLimits}, trace::TraceSink, value::Value,
    vm::{Invocation, InvocationOptions, TrapInfo, Vm}};
fn stored<H: Host + 'static>(target: &Callable, vm: Vm<H>, args: Vec<Value>)
    -> Result<(Invocation<'static, H>, Rc<RefCell<Recorder>>), TrapInfo>
{
    let capture = Rc::new(RefCell::new(Recorder::new(
        target.program().clone(), RecordLimits::default())));
    let mask = capture.mask();
    let invocation = target.start_owned(vm, args, InvocationOptions::default()
        .yield_host_calls().trace_owned(mask, Rc::clone(&capture)))?;
    Ok((invocation, capture))
}
```

Shared trace/access collectors forward normal callbacks and filtering. Borrow them only
between execution calls, releasing all borrows before resuming or retiring the invocation.
Dropping an owned invocation cancels unfinished work and releases
its VM and observers; external collector handles can retain the incomplete evidence.
Raw aggregate/callable result IDs still need that VM/code owner: explicitly snapshot or
retain values before retirement. Owned execution does not make raw handles portable or
automatically create a scheduler, worker thread or external effect rollback.

## Structured host results

Hosts can use `HostContext::tuple_new`, `struct_new` and `array_new` to construct immutable
results while their input payloads borrow the original heap. The context's readers and
`retain` understand both domains; `aggregates()` exposes only the original base heap.

```rust
use execution_tape::{host::{HostContext, HostError}, value::Value};
fn loaded(mut context: HostContext<'_, '_>, bytes: Vec<u8>) -> Result<Value, HostError> {
    let size = bytes.len() as u64;
    let root = context.tuple_new(vec![Value::U64(size), Value::Bytes(bytes)])?;
    Ok(Value::Agg(root))
}
```

Successful host completion adopts only staged nodes reachable from raw return slots.
Return validation still runs afterward; a raw result is not proof of successful guest
installation. Errors and cancellation discard staging. Native effects remain performed.
Handles and nominal IDs belong to this invocation and program; these constructors do not
make foreign handles portable. Use `retain` for results needed outside the host call.
After successful execution, `Invocation::into_snapshot` consumes the invocation and moves
an owned VM's reachable results out without copying payloads. A borrowed VM copies its
reachable graph and keeps its existing raw handles valid. Clone `Invocation::program_owner`
before consumption to retain the exact code domain for nominal/callable snapshot IDs, even
after the original lookup handle has been dropped.
Recordings preserve adopted node identities and all successful construction debits, so
replay restores rich results without repeating native work or replenishing guest budgets.

## Yielded host requests

Opt into `InvocationOptions::yield_host_calls()` to receive `RunStatus::AwaitingHost` after
a host instruction is admitted. `pending_host_call()` exclusively borrows that invocation:
query its import, source location, borrowed arguments and aggregate context, then `dispatch`
through the existing host or `complete` with return slots and a fuel/error outcome. Stack
return arrays and reusable buffers work with the same ABI as `Host::call`.

For asynchronous producers, `complete_with` keeps borrowed inputs alive across `await` and
tracks cancellation. This example delegates a one-string request to an embedding service:

```rust
extern crate alloc;
use alloc::{string::String, vec};
use execution_tape::{host::{Host, HostError, ValueRef}, value::Value,
    vm::{Invocation, TrapInfo}};
async fn answer<H: Host>(invocation: &mut Invocation<'_, H>,
    service: impl AsyncFnOnce(&str) -> Result<String, HostError>) -> Result<(), TrapInfo> {
    invocation.pending_host_call().expect("AwaitingHost")
        .complete_with(async move |request| {
            let Some(ValueRef::Str(text)) = request.arguments().next() else {
                return Err(HostError::SignatureMismatch);
            };
            let reply = service(text).await?;
            Ok((vec![Value::Str(reply)], 0))
        }).await
}
```

The embedding owns the async executor, message lookup, effects and external object retention.
Waiting spends no additional fuel or host-call count; completion validates the raw outcome
before continuing and does not execute the next guest instruction. Match all three run
statuses in a yielding driver. A polled producer abandoned before completion cannot be
redispatched: resumption reports `HostCallAbandoned` and recording remains incomplete.
Ordinary runs and `Host` implementations keep their existing signatures and behavior.

## Owned executable callables

[`callable::Callable`] retains an exact prepared program and an optional independent closure
environment. Callable entry retains its code and arguments independently of the lookup
handle; `start_owned` additionally owns the VM. Retain a returned closure directly from the
successful invocation. That returned callable survives its creator's frames and VM:

```rust
use std::sync::Arc;
use execution_tape::{callable::Callable, host::Host, value::{FuncId, Value},
    verifier::VerifiedProgram, vm::{InvocationOptions, RunStatus, Vm}};
fn make_callback<H: Host>(vm: &mut Vm<H>, program: Arc<VerifiedProgram>, factory: FuncId,
    args: Vec<Value>) -> Result<Callable, Box<dyn std::error::Error>> {
    let factory = Callable::function(program, factory)?;
    let mut invocation = factory.start(vm, args, InvocationOptions::default())?;
    while invocation.resume(1024)? == RunStatus::Suspended {}
    Ok(invocation.retain_callable(0)?)
}
```

Closure calls import captures transactionally, prepend the hidden environment, and check the
concrete body's contracts. Capture copies are admitted against aggregate/payload work limits
before allocation. The debit remains in effect across quanta
and checked replay. Cancellation/traps discard imports; success retains only returned roots.
`Callable::visit_objects` exposes captured object tokens for the host's own collector.
Captured environments remain immutable; shared mutable variables can refer to host-owned cells.
A callable pins code, without replacing running code or translating IDs across programs.

[`callable::CodeSlot`] publishes replaceable bindings with exact exposed-signature checks.
`current()` returns an owned `CodeVersion` snapshot. A call started from that snapshot owns
its code, so replacement does not borrow or patch an active frame. Unchanged identities are
explicit no-ops; rejection retains both code owners and a structured signature difference.
Signature comparison is iterative/memoized and preserves program-local nominal identities.
The embedding owns class/selector lookup, synchronization, cache invalidation and host objects.

## Paused frames and host-object roots

`Invocation::inspect` borrows live state at a safe instruction boundary. Frames have
invocation-scoped `ActivationId`s that remain stable across resumes and reject foreign or
retired identities. Waiting callers point at their pending call instruction; their results
are not reported before the callee returns. Register queries retain declared kinds and
return explicit unavailability when initialization is not established. Source maps resolve
the same exact frame locations used by recording and disassembly.

Register mapping and verifier facts are constructed only when requested for a function.
`visit_objects` inventories conservative references in initialized registers, arguments,
outputs and all VM aggregate fields, including retained results and temporary nodes. The
embedding collector must add its globals, snapshots and other external roots and trace its
own object graph. The VM does not choose collection, identity or relocation policy. Inspection
borrows prevent resumption or host mutation while those views are being read.

## Design Docs

The repository-level design notes live outside the packaged crate:

- <https://github.com/forest-rs/execution/blob/main/docs/overview.md>
- <https://github.com/forest-rs/execution/blob/main/docs/v1_spec.md>

## Examples

Print disassembly for a small branching program:

```sh
cargo run -p execution_tape_wind_tunnel --bin disasm
```

<!-- cargo-rdme end -->

## Minimum supported Rust Version (MSRV)

This crate has been verified to compile with **Rust 1.88** and later.
