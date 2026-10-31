# Conditions above the VM

The `conditions` example compiles a small expression AST into verified tape and supplies
an authored-source sidecar. It demonstrates a possible language ABI for conditions, handlers,
restarts and protected bodies. These conventions belong to this example, not to the VM.
The inspection commands need no native condition service:

```sh
cargo run -p execution_tape_wind_tunnel --bin conditions -- recovery --disassemble
cargo run -p execution_tape_wind_tunnel --bin conditions -- cleanup-signals --static --snapshot
cargo run -p execution_tape_wind_tunnel --bin conditions -- cleanup-signals --static --explore
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

## Run the language protocol

Without `--static` or `--disassemble`, the example drives the tape in a native language service:

```sh
cargo run -p execution_tape_wind_tunnel --bin conditions -- recovery
cargo run -p execution_tape_wind_tunnel --bin conditions -- cleanup --snapshot
cargo run -p execution_tape_wind_tunnel --bin conditions -- cleanup-replaces --explore
cargo test -p execution_tape_wind_tunnel --bin conditions
```

`recovery` stops at native work inside a guest handler. The printed explanation separates
logical dynamic bindings from physical guest frames, including the retained signaling calls.
The command-line driver supplies an empty reply to the demo wait, then the handler recovers
with `(42, true)`. It prints the language policy events with authored source, verifies a fresh
guest replay, and optionally opens the recorded Observatory view. The latter shows VM evidence;
the separate language-policy log is printed before entering the terminal UI.

The example runtime owns these semantics:

| Mechanism | Example behavior |
|---|---|
| Class matching | `DivisionByZero` inherits from `ArithmeticError`, `Error` and `Condition`; warnings inherit from `Condition`. |
| Handler search | Applicable bindings are tried innermost first. While a handler runs, existing bindings at and inside that binding are masked; newly established bindings remain visible. |
| Normal handler return | Under `CommonLisp`, a normal return declines. Under `Dylan`, `accepted` chooses recovery or decline. These are two policies, not complete implementations of either language. |
| Function restart | An ordinary callable result returns to its invoker. Calling it does not itself resolve the signal, and it may be called repeatedly. |
| Exiting restart | The runtime runs the restart function, processes crossed cleanup and transfers its typed results from the selected body activation. |
| Restart token | An opaque identity belongs to one runtime and one dynamic extent. A same-name binding cannot revive an expired token. |
| Protected body | Normal completion and an explicit exit both run cleanup in LIFO order. Each binding is consumed before entering cleanup. |
| Unhandled condition | The embedding receives the condition and applicable restart tokens. It can invoke a restart or explicitly supply recovery values. |

`cleanup` demonstrates an exit across two protected bodies. Inner cleanup emits `20`, awaits
external work and returns; outer cleanup emits `10`; only then does the VM transfer `(42, true)`.
During that wait, the inner protection has already expired logically, while the physical
signaling stack is still retained. Handler masks are restored to the cleanup owner's logical
environment before cleanup executes.

In `cleanup-replaces`, cleanup selects another exit. The original intent to return `42` is
replaced with `99`. There are two language exit-selection events, one abandoned cleanup and
one final VM transfer. The consumed cleanup does not start again. In `cleanup-local-exit`,
an exit confined to a nested cleanup scope leaves the original pending transfer intact.
`cleanup-signals` exercises a new signal during cleanup using the restored outer handler.

Other cases cover nested signals and new handler bindings, explicit decline and normal-return
decline, repeated restart functions, returning to an unhandled-condition debugger, expired
tokens, rebinding, absent names and condition-specific restart applicability. Invalid guest
restart requests signal an `InvalidRestart` language condition; foreign debugger tokens are
recoverable denials before guest work. Names, classes, payloads and token conventions are
deliberately small and private, not a proposed core object model.

## Ownership, resource limits and evidence

The pending structured-service request keeps its borrowed body/handler/cleanup arguments alive
while guest callbacks execute. The runtime records exact body activation identities, rather
than deriving scope identity from function IDs or PCs. Retiring a binding releases its borrowed
guest closure handles; a retired restart keeps only a token tombstone for expiry diagnostics.
An exit prunes native continuations using the VM's surviving activations. A malformed external
reply traps and retires language state. It cannot be used to bypass an unhandled condition.

All callbacks and cleanup execute in the original invocation with cumulative VM fuel, host-call,
call-depth, contract and data-work limits. Polling a native wait does not spend additional guest
work. This example also caps total scope registrations at 256 per invocation and policy events
at 4,096; a scope-budget failure is a hard host failure. The native policy operations have no
separate CPU work meter. These bounds do not make the example a hardened general language runtime.

Hard VM failure or cancellation bypasses cooperative guest cleanup. Dropping during a cleanup
wait runs no further cleanup and yields an incomplete VM recording. Applications needing
guaranteed native resource release must provide it independently of guest execution.

A finished session owns two distinct kinds of evidence after the runtime is gone:

* The bounded native policy log records selected handlers, scope retirement, exit intentions
  and cleanup decisions, with its own truncation flag and returned/trapped/cancelled status.
* The VM recording owns observed values, actual callback returns, admitted transfer commands
  and instruction history, with independent capture-completeness checks.

Checked replay reexecutes guest handlers, restarts and cleanup under the **recorded embedding
choices**. It does not rerun native handler selection, masking policy, debugger decisions or
external effects. The example's tests check those native semantics directly, then independently
check guest replay after the original runtime has been dropped. A successful replay therefore
does not certify the native policy log. Neither evidence stream is a portable session archive.

Every retained policy location, VM command, callback return, host outcome and instruction maps
to the exact authored AST snapshot. A transfer's initiating instruction and the target frame's
suspended instruction retain separate locations. Source mapping and UI rendering remain
explicit tooling work; they are not added to ordinary verification or execution.
