# Conditions above the VM

The `conditions` example compiles a small expression AST into verified tape and supplies
an authored-source sidecar. It demonstrates a possible language ABI for conditions, handlers,
restarts and protected bodies. These conventions belong to this example, not to the VM.
The first inspection commands need no native condition service:

```sh
cargo run -p execution_tape_wind_tunnel --bin conditions -- recovery --disassemble
cargo run -p execution_tape_wind_tunnel --bin conditions -- cleanup-signals --snapshot
cargo run -p execution_tape_wind_tunnel --bin conditions -- cleanup-signals --explore
```

Use `--help` for the scenario names. In the static browser, select functions and instructions
to inspect contracts and the corresponding authored expression. The source document is labelled
`conditions.expr (assembled AST)`: it is a formatted version of the Rust-authored example AST,
not parsed language input or the Rust implementation of the compiler. Its ranges identify exact
instructions, including generated closure construction and function returns. Maps are bound to
the exact tape; changing a captured constant invalidates the old map.

## What the frontend emits

A handler binding is a call to the example service `condition.handle`, carrying the protected
body closure, handler closure, condition class and handler-return policy. Closures capture a
verified four-integer environment. Body and restart functions return `(i64, bool)`; handler
functions return `(accepted: bool, value: i64, flag: bool)`; cleanup functions return no values.
The acceptance flag is separate from the user's secondary result.

`recovery` explicitly signals a `DivisionByZero` condition through the ordinary calls
`compute` and `divide`; it does not execute a division instruction. Its signal site is:

```text
000016: const.i64 r8 ; 4
000019: const.i64 r9 ; 5
000022: host_call eff_out=r0, host_sig#3("condition.signal"), eff_in=r0,
          args=[r8, r9], rets=[r6, r7]
000032: ret eff=r0, rets=[r6, r7]
```

Class `4` means `DivisionByZero` and `5` is its example payload. Neither integer has intrinsic
meaning to execution_tape. The handler captures `40`, awaits native work, adds `2` and returns
`(true, 42, true)`. The language service must interpret that reply before completing the signal
with `(42, true)`. A restart binding similarly passes body and restart closures to
`condition.restart_bind`; a protected body passes body and cleanup closures to
`condition.protect`. Conditional handler bodies use ordinary typed branches and block joins.

## Which stack belongs where?

Core execution_tape maintains guest call frames, pending host requests, callback ancestry,
exact activation identities and typed result contracts. It has no condition-class hierarchy,
handler stack, restart lookup policy or automatic protected-cleanup registry.

A language runtime supplies those meanings. At a pending host request it can use `call_guest`
to run a handler, restart or cleanup closure in the same invocation. The original host request
and its callers remain suspended. A callback's own typed return goes back to that service;
it does not implicitly complete the original host request. An explicit `return_from` can
instead return values from an exact live guest activation and abandon its callees. Core checks
the target's ownership, lifetime, result contracts and resource budgets.

A condition runtime can therefore maintain its logical dynamic extents above the VM. For
an exit, it must restore the language environment and run the required cleanup callbacks
before requesting the final physical transfer. Arbitrary core traps, fuel exhaustion,
cancellation and dropping an invocation do not execute guest cleanup. A language that wants
recoverable arithmetic failure must signal it explicitly rather than relying on a core trap.

This example's ABI and compiler are private development tooling. They establish no language,
source-format or portable recording compatibility promise, add no production dependencies,
and change neither core opcodes nor the wire format.
