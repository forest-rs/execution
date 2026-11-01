# Conditions across independently compiled code

```sh
cargo run -p execution_tape_examples --bin composed_conditions -- resume --explain
cargo run -p execution_tape_examples --bin composed_conditions -- recover
cargo run -p execution_tape_examples --bin composed_conditions -- escape --snapshot
```

The example compiles seven small definitions independently. Each uses local function `f0`.
One VM owns their host state and aggregate heap, and one invocation owns the real suspended
parent chain and cumulative resource budget. The language runtime supplies method lookup,
dynamic handler and restart scopes, publication policy, and the choice to run cleanup.

`resume` computes `84 / 2 = 42` through this control path:

```text
main: first send
  divide/v1: signal ZeroDivisor(84, 0)
    zero_divisor_handler: invoke use_divisor
      use_divisor: return 2
    handler returns 2
  division resumes and returns 42
main: second send
  divide/v2: return 99
main returns 42 + 99 = 141
```

The runtime publishes `divide/v2` while `divide/v1` is suspended. The original activation keeps
its exact code; the next lookup selects the replacement. The condition is one shared tuple
through the method, handler and restart. Child returns preserve its heap identity, and the
root commits storage once after all active children have returned.

`recover` chooses a handler that directly returns a resumption divisor. It also computes `141`,
with no restart invocation. `escape` selects a different language policy at the restart boundary:

```text
main
  divide/v1: signal ZeroDivisor
    zero_divisor_handler: pending restart request
      ensure_cleanup
        native_cleanup: one host effect
      cleanup returns
    return_from(main, 42)
main returns 42
```

Cleanup is an actual guest function in the same execution. It can suspend at its native boundary;
the handler and signalling method remain inspectable while it waits. Once cleanup returns, the
runtime asks the VM to return from the exact lexical home. The VM validates that home's result
contract and retires the crossed activations. The abandoned second send never executes.

The runtime's handler and restart tables use opaque activation identities. Scope availability is
checked against the VM's live frames. A returning child cannot donate its identity to a later
child, even when both display local activation `0`. Stack labels include the segment ordinal
to keep those display identities distinct.

## Read the execution

`--explain` walks the VM's actual stack at each pending boundary and resolves each frame through
its own exact source map. `--snapshot` opens a static Observatory view at the handler's pending
restart request. Selectable parent frames belong to the same invocation, so the browser uses
their own code, registers, source and breakpoint domains. `--disassemble` prints every compiled
definition, including the division's typed control-flow join.

The authored snippets and exact byte ranges are supplied by this example's tiny frontend in
[`program.rs`](src/bin/composed_conditions/program.rs). The policy and ordinary native host live
in [`runtime.rs`](src/bin/composed_conditions/runtime.rs). Source snapshots are embedded strings;
viewing them opens no source file and does not infer guest locations from the Rust compiler code.

## The core boundary

`Vm::instantiate` gives code a store namespace. At a pending request,
`call_instance` enters that instance with moved arguments and retains the real parent in the
invocation. A successful child restores the original pending request and its callback results;
the runtime decides how to complete that host operation. `return_from` also accepts a live
activation in an independently compiled suspended parent.

All segments share instruction fuel, live call depth, host admission, contract work and aggregate
and payload work. Transfer-provenance checks are iterative and use the contract-work budget.
Structural values and host objects can cross code boundaries in this store. Raw callable IDs
remain local to their instance, and nominal aggregate schemas require their original definition.
Nested identities are checked before transfer; an equal numeric ID cannot change meaning.

Fatal traps and dropping the invocation terminate every segment and discard its new aggregates.
Performed host effects remain. Hard termination executes no guest cleanup:

```sh
cargo run -p execution_tape_examples --bin composed_conditions -- escape --fuel 5
```

Protected cleanup is an explicit language action, so a language can define how cleanup failures,
replacement exits, cancellation or unavailable fuel affect its own condition protocol.
