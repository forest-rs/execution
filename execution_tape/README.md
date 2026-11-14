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
use execution_tape::value::{FuncRef, Value};
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
    let unit = vm.install(program)?;
    let entry = FuncRef::new(unit, entry);
    let out = vm.run(entry, &[Value::I64(41)], TraceMask::NONE, None)?;
    assert_eq!(out, vec![Value::I64(42)]);
    Ok(())
}
```

Execution always enters code installed in the VM. [`Vm::install`](vm::Vm::install) gives a
verified program a [`UnitId`](value::UnitId) in that VM, and a [`FuncRef`](value::FuncRef)
names one function of one unit. Install a program once and run it as often as needed: every
install creates a new unit, and the VM retains every installed unit until it is dropped.
[`Vm::units`](vm::Vm::units) reports what is installed.

For repeated execution, keep an [`ExecutionContext`](vm::ExecutionContext) and a result `Vec`.
Clear the result buffer between runs and call [`Vm::run_with_ctx`](vm::Vm::run_with_ctx)
to reuse both allocations. Results append in return-signature order; a trap preserves the
buffer's original contents. Aggregate results belong to the VM heap and survive context resets.

## Hosts that borrow per-call state

A host that holds a borrow, such as `&mut Context` for one request, cannot live in a VM that
outlasts the borrow. Rather than building a VM and reinstalling for every call, park the VM
around a host that owns nothing and use [`Vm::replace_host`](vm::Vm::replace_host) to lend it
the borrowing host for one call. The swap moves installed units and retained aggregates
without copying or reinstalling them, so a call costs about what it costs on a VM that owns
its host; a fresh VM and install per call costs several times more (see the `borrowed_host`
wind-tunnel bench).

```rust
use execution_tape::host::{Host, HostContext, HostError, SigHash, ValueRef};
use execution_tape::trace::TraceMask;
use execution_tape::value::{FuncRef, Value};
use execution_tape::vm::{ExecutionContext, Limits, TrapInfo, Vm};

/// State the embedding owns and lends for one call.
struct Request {
    events: Vec<i64>,
}

/// Borrows the request while a call runs.
struct Lent<'r>(&'r mut Request);

impl Host for Lent<'_> {
    fn call(
        &mut self,
        _symbol: &str,
        _sig_hash: SigHash,
        args: &[ValueRef<'_>],
        _rets: &mut [Value],
        _ctx: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        let [ValueRef::I64(event)] = args else {
            return Err(HostError::Failed);
        };
        self.0.events.push(*event);
        Ok(0)
    }
}

/// Holds the VM between calls; guest code never runs against it.
struct Parked;

impl Host for Parked {
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

/// A program installed once and run for many requests.
struct Module {
    vm: Option<Vm<Parked>>,
    entry: FuncRef,
    scratch: ExecutionContext,
    out: Vec<Value>,
}

impl Module {
    fn run(&mut self, request: &mut Request, args: &[Value]) -> Result<(), TrapInfo> {
        // A panic mid-run leaves `vm` empty; rebuild the module after one.
        let parked = self.vm.take().expect("the module is not running");
        let (mut vm, idle) = parked.replace_host(Lent(request));
        self.out.clear();
        let result = vm.run_with_ctx(
            &mut self.scratch,
            self.entry,
            args,
            &mut self.out,
            TraceMask::NONE,
            None,
            None,
        );
        self.vm = Some(vm.replace_host(idle).0);
        result
    }
}

let mut vm = Vm::new(Parked, Limits::default());
let unit = vm.install(program)?;
let mut module = Module {
    vm: Some(vm),
    entry: FuncRef::new(unit, entry),
    scratch: ExecutionContext::new(),
    out: Vec::new(),
};
for event in [3, 5] {
    let mut request = Request { events: Vec::new() };
    module.run(&mut request, &[Value::I64(event)])?;
    assert_eq!(request.events, [event]);
}
```

The VM holds no reference to a host between runs, and an invocation borrows the VM, so the
host can only change between runs. Object handles belong to the hosts: an
[`Obj`](value::Obj) that outlives a call, inside a retained aggregate or a value the
embedding kept, keeps its bits under the next host. Do not let handles that mean something
for one call only outlive it. Recordings treat handles as opaque and replay without a host.

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

Run `cargo run -p execution_tape_examples --bin nested_contracts` for a complete example.
Run `cargo run -p execution_tape_examples --bin typed_callable` for an indirect nested result.

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
Ordinary verification, preparation and execution retain none of these facts.

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
- `math`: the floating-point operations the VM computes, as plain functions native code can
  call to get the same bits, including the `f32` transcendentals pinned to `libm`.

## The pinned `libm`

The math intrinsics are defined as the results of `libm` 0.2.16, so this crate depends on
`libm = "=0.2.16"`. Cargo builds one copy of each semver-compatible `libm` (every 0.2.x) per
dependency graph, so the pin applies to the whole build: a crate that asks for
`libm = "0.2"` gets 0.2.16, and one that requires a later 0.2 release fails to resolve
(Cargo reports the conflict with `=0.2.16`) instead of building a second copy. Moving to a
newer `libm` waits for an `execution_tape` release that moves the pin, which changes intrinsic
results and is called out in the changelog. The shared copy is also why native code calling
`libm` directly gets the same bits as a tape program.

## Resumable execution

`Vm::start` validates an invocation and returns a handle that borrows its VM, inputs and
optional observers, and retains its entry unit's program. `Invocation::resume` executes a scheduling quantum at complete
instruction boundaries. Hard fuel and data-work budgets accumulate across resumes; a quantum
is separate from these limits. Host calls run synchronously by default.

```rust
use execution_tape::{host::Host, value::{FuncRef, Value},
    vm::{InvocationOptions, RunStatus, TrapInfo, Vm}};
fn scheduled<H: Host>(vm: &mut Vm<H>, entry: FuncRef, args: &[Value])
    -> Result<Vec<Value>, TrapInfo> {
    let mut invocation = vm.start(entry, args, InvocationOptions::default())?;
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
fn stored<H: Host + 'static>(target: &Callable, mut vm: Vm<H>, args: Vec<Value>)
    -> Result<(Invocation<'static, H>, Rc<RefCell<Recorder>>), Box<dyn std::error::Error>>
{
    let unit = vm.install(target.program().clone())?;
    let capture = Rc::new(RefCell::new(Recorder::new(
        target.program().clone(), RecordLimits::default())));
    let mask = capture.mask();
    let invocation = target.start_owned(vm, unit, args, InvocationOptions::default()
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
A yielded request that reenters guest code first promotes all previous staging with unchanged
handles; unused promoted nodes remain until root commit/rollback, as described below.
Handles and nominal IDs belong to this invocation and program; these constructors do not
make foreign handles portable. Use `retain` for results needed outside the host call.
After successful execution, `Invocation::into_snapshot` consumes the invocation and moves
an owned VM's reachable results out without copying payloads. A borrowed VM copies its
reachable graph and keeps its existing raw handles valid. Clone `Invocation::program`
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
before continuing and does not execute the next guest instruction. Match every run status in
a yielding driver. A polled producer abandoned before completion cannot be
redispatched: resumption reports `HostCallAbandoned` and recording remains incomplete.
Ordinary runs and `Host` implementations keep their existing signatures and behavior.

### Guest callbacks from a pending host request

`PendingHostCall::call_guest` enters a function or closure in this invocation's original
program/heap while retaining the admitted host instruction and its callers. Drive the same
invocation with `resume`: the callback can yield further host requests, which can reenter
guest code in turn. When it returns, `resume` reports `RunStatus::CallbackReturned`: the
original request is pending again, with result slots available through
`callback_results`/`take_callback_results`. The embedding still chooses and supplies the
final host outcome. Callback and host result signatures are independent.

```rust
use execution_tape::{host::Host, value::Value, vm::{Invocation, TrapInfo}};
fn start_handler<H: Host>(run: &mut Invocation<'_, H>, handler: Value,
    args: Vec<Value>) -> Result<(), TrapInfo> {
    run.pending_host_call().expect("AwaitingHost").call_guest(handler, args)
}
// Resume and handle nested host requests normally. Once `resume` reports `CallbackReturned`:
fn accept_handler<H: Host>(run: &mut Invocation<'_, H>) -> Result<(), TrapInfo> {
    let mut request = run.pending_host_call().expect("original request restored");
    let mut results = request.take_callback_results().expect("handler returned");
    request.complete(&mut results, Ok(0)) // this example uses matching host/handler results
}
```

Active frame identities and source callers survive reentry. Closures use their concrete
body/environment contracts. All nested work shares the original hard budgets; each callback
spends one fuel unit, and moved immediate payload arguments are charged as host ingress.
Previously staged host aggregates are promoted with unchanged handles before reentry, so
callbacks and later host construction cannot reuse their staged identities. Even unused
promoted nodes remain until root commit/rollback; their existing work stays charged.
Aggregate, closure and function handles read from any request of an invocation (arguments,
callback results and the values they reach) stay valid until the root commits or the
invocation rolls back, so an embedding can keep them and pass them to later `call_guest` or
`complete` calls of the same invocation.

Invalid callback admission or return traps the whole invocation. Dropping it cancels every
pending level without running guest code. Guest reentry does not capture a native Rust
stack or import another program's function, closure or nominal IDs. Synchronous `Host::call`
retains its existing interface; reentry uses the explicit yielded-request protocol.

`ValueEvent::CallbackEnter`/`CallbackReturn` and ordinary call scopes describe actual guest
activity. Callback return is distinct from host completion and root publication. Recording
retains embedding callback commands, promoted staging and actual guest returns in observation
order. Checked replay runs those guest bodies again and verifies their results. Optional
instruction histories link callback attempts to the host instruction that remains pending.

### Exceptions and nonlocal exits

Nonlocal control is a typed exception. `throw` raises a program tag with its payload, and a
host raises one at its `host_call` with `HostError::throw` (or `PendingHostCall::throw` for a
yielded request). Protected regions attach ordered `catch`/`catch_all` clauses to
instructions; the innermost catching clause receives the payload in a typed block, and every
activation, guest callback and pending host request in between is abandoned. `catch_ref` and
`catch_all_ref` also bind the exception as an `Exn` register, which `throw_ref` rethrows with
its original tag, payload and throw site, so cleanup is ordinary guest code that rethrows.
Traps, limits and cancellation are never caught and run no guest code. An exception nothing
catches ends the invocation as `Trap::Uncaught` with a detached payload snapshot. Language
condition classes, handler selection and restart policy remain above the core VM.

### Stack switching

One-shot continuations follow WebAssembly stack switching. `cont.new` wraps a function or
closure as an unstarted continuation in a `Cont` register, and `resume` runs it on top of the
resuming activation. `suspend` raises a tag to the nearest `on` clause protecting a `resume`
that runs it; the handler receives the payload and the suspended activations as a new
continuation, which a later `resume` continues with the tag's results. `resume_throw` raises
an exception at the suspension point instead, and `cont.bind` binds leading inputs.
Continuations are typed by call signatures, checked across units, consumed by use, and live
until resumed, released with `cont.drop`, or the invocation ends; their frames count against
the call-depth limit meanwhile.
Generators, coroutines and resumable conditions are guest code on these primitives.

A suspension that no `on` clause handles traps with `UnhandledSuspension`, unless the
invocation opts into `InvocationOptions::yield_suspensions()`. Then the embedding is the
handler of last resort: `resume` reports `RunStatus::AwaitingHandler`, and
`pending_suspension()` borrows the tag and payload. The embedding answers once, by resuming
with the tag's results, raising an exception at the `suspend`, or discarding the suspension,
which ends the invocation with `UnhandledSuspension`. A REPL can answer a condition with a
restart this way, as often as the guest signals one:

```rust
use execution_tape::{host::Host, value::Value, vm::{Invocation, RunStatus, TrapInfo}};
fn drive<H: Host>(run: &mut Invocation<'_, H>,
    choose: impl Fn(&str) -> Value) -> Result<(), TrapInfo> {
    loop {
        match run.resume(u64::MAX)? {
            RunStatus::Complete => return Ok(()),
            RunStatus::AwaitingHandler => {
                let condition = run.pending_suspension().expect("waiting suspension");
                let restart = choose(condition.tag_name());
                condition.resume(vec![restart])?;
            }
            _ => {}
        }
    }
}
```

While it waits, the suspended frames stay inspectable and the embedding can install code or
use its host. A suspension inside a guest callback still traps with `SuspensionAcrossHost`.
Recordings retain each yielded suspension and its answer, and replay answers it again.

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
    let unit = vm.install(Arc::clone(&program))?;
    let factory = Callable::function(program, factory)?;
    let mut invocation = factory.start(vm, unit, args, InvocationOptions::default())?;
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

## Units and live replacement

Independently compiled programs call each other in one invocation through function values.
A [`FuncRef`](value::FuncRef) names a unit and a function of its program; `call.indirect` runs
the target's own unit, checking its exact contract signature, which every unit of a VM interns
the same way. Struct and array types are interned as heap shapes, so values keep their meaning
in every unit. Each activation keeps the unit it entered: installing replacement code changes
no running or suspended frame, and new calls reach new code only through the function values a
language hands out, for example from a host-owned method table:

```rust
use std::sync::Arc;
use execution_tape::{host::Host, value::{FuncId, FuncRef, Value}, verifier::VerifiedProgram,
    vm::Invocation};
fn answer_with_edit<H: Host>(run: &mut Invocation<'_, H>, edited: Arc<VerifiedProgram>,
    method: FuncId) -> Result<(), Box<dyn std::error::Error>> {
    let mut request = run.pending_host_call().expect("a yielded method lookup");
    // Frames already running the old method keep it; this lookup answers with the new one.
    let unit = request.install(edited)?;
    request.complete(&mut [Value::Func(FuncRef::new(unit, method))], Ok(0))?;
    Ok(())
}
```

Exceptions carry a VM `TagRef`. Each install of a program defines fresh tags for the tags it
declares, while tags it imports (`ProgramBuilder::import_tag`) bind at install to the tag the
embedding defined under that name with `Vm::define_tag`, provided the payloads agree. A
`catch tag` clause therefore catches every unit's throw of a tag its program imports, and a
tag the program cannot name reaches only `catch_all`.

A recording follows execution into every unit. It snapshots the VM's installs and tag
definitions at entry and records installs made while the invocation is suspended; replay
repeats them in a fresh VM and checks that each unit and tag keeps its `UnitId` or `TagRef`.

## Paused frames and host-object roots

`Invocation::inspect` borrows live state at a safe instruction boundary. Frames have
invocation-scoped `ActivationId`s that remain stable across resumes and reject foreign or
retired identities. Waiting callers point at their pending call instruction; their results
are not reported before the callee returns. Register queries retain declared kinds and
return explicit unavailability when initialization is not established. Source maps resolve
the same exact frame locations used by recording and disassembly.

Verification keeps, per function, which registers are definitely initialized at each block
entry and call-like instruction, so inspection never reanalyzes a function. A view maps a
function's registers to their banks only when one of its frames is queried.
`visit_objects` inventories conservative references in initialized registers, arguments,
pending callback results, outputs and all VM aggregate fields, including retained results and temporary nodes.
Registers include those of frames held by unresumed continuations, which `frames` does not list, and
an exception reference contributes its payload's objects. The
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
cargo run -p execution_tape_examples --bin disasm
```

<!-- cargo-rdme end -->

## Minimum supported Rust Version (MSRV)

This crate has been verified to compile with **Rust 1.88** and later.
