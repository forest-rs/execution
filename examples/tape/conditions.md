# Conditions above the VM

The `conditions` example compiles a small expression AST into verified tape and supplies
an authored-source sidecar. It demonstrates a possible language ABI for conditions, handlers,
restarts and protected bodies. These conventions belong to this example, not to the VM.
The inspection commands need no native condition service:

```sh
cargo run -p execution_tape_examples --bin conditions -- recovery --disassemble
cargo run -p execution_tape_examples --bin conditions -- cleanup-signals --static --snapshot
cargo run -p execution_tape_examples --bin conditions -- cleanup-signals --static --explore
```

Use `--help` for the scenario names. In the static browser, select functions and instructions
to inspect contracts and the corresponding authored expression. The source document is labelled
`conditions.expr (assembled AST)`: it is a formatted version of the Rust-authored example AST,
not parsed language input or the Rust implementation of the compiler. Its ranges identify exact
instructions, including generated closure construction and function returns. Maps are bound to
the exact tape; changing a captured constant invalidates the old map.

The frontend sets `Asm::source` as it emits each authored expression and collects
`source_mappings` before defining the body. It does not rediscover instruction boundaries through
disassembly to construct its map. The static browser and recorded explorer use that same sidecar.

## What the frontend emits

Handler, restart and protected bodies compile inline. A handler binding calls the example
service `condition.push_handler` with the handler closure, condition class and handler-return
policy; a restart binding calls `condition.push_restart` with the restart closure, name, mode and
applicable class. Each returns a scope number, and the body that follows runs inside a protected
region whose `catch_all_ref` clause calls `condition.pop` and rethrows with `throw_ref`. The
normal path pops the scope too, so the runtime's dynamic stack always matches the guest's.

A restart scope's region also catches the `condition.exit(scope, value, flag)` tag. When the
exit names this scope, the region returns `(value, flag)` as the restart expression's result;
otherwise it pops and rethrows. A protected body's `catch_all_ref` clause calls the cleanup
function and rethrows, and its normal path calls the same cleanup function before continuing.
An exception raised by cleanup, including another exit, replaces the one being unwound.

Closures capture a verified four-integer environment. Restart functions return `(i64, bool)`;
handler functions return `(accepted: bool, value: i64, flag: bool)`; cleanup functions return no
values. The acceptance flag is separate from the user's secondary result.

The frontend uses `ProgramBuilder::declare_closure` to describe the captured context and exposed
signature once. `load_captures` gives the body meaningful local names, `argument` excludes the
hidden environment, and `instantiate` emits the matching closure construction. These helpers
generate ordinary tape instructions; the language runtime still supplies handler and restart policy.

`recovery` explicitly signals a `DivisionByZero` condition through the ordinary calls
`compute` and `divide`; it does not execute a division instruction. Its signal site is an ordinary
`host_call` to `condition.signal` with class `4` (`DivisionByZero`) and payload `5`. Neither
integer has intrinsic meaning to execution_tape. The handler captures `40`, awaits native work,
adds `2` and returns `(true, 42, true)`. The language service interprets that reply and completes
the signal with `(42, true)`.

## Which stack belongs where?

Core execution_tape maintains guest call frames, pending host requests, callback ancestry,
protected regions and the unwinder. It has no condition-class hierarchy, handler stack or restart
lookup policy.

The language runtime keeps the dynamic stack of handler and restart bindings. Handlers run on top
of the signaling stack: at the pending `condition.signal` request the runtime uses `call_guest`
to run a handler closure, and a restart function runs the same way at `condition.invoke` or at an
unhandled signal. A callback's typed return goes back to the runtime; it does not complete the
original request implicitly.

An exiting restart becomes a guest exception. After the restart function returns its values, the
runtime completes the pending request with `PendingHostCall::throw`, raising `condition.exit` at
that `host_call`. The VM unwinds the abandoned callbacks, requests and frames; each crossed binding
pops itself and each crossed protected body runs its cleanup in guest code, which may itself wait
for native work, signal again or exit elsewhere. The runtime forgets callbacks and masks whose
frames the exception abandoned. Arbitrary traps, fuel exhaustion, cancellation and dropping an
invocation do not execute guest cleanup. A language that wants recoverable arithmetic failure must
signal it explicitly rather than relying on a core trap.

This example's ABI and compiler are private development tooling. They establish no language,
source-format or portable recording compatibility promise and add no production dependencies.

## Run the language protocol

Without `--static` or `--disassemble`, the example drives the tape in a native language service:

```sh
cargo run -p execution_tape_examples --bin conditions -- recovery
cargo run -p execution_tape_examples --bin conditions -- cleanup --snapshot
cargo run -p execution_tape_examples --bin conditions -- cleanup-replaces --explore
cargo test -p execution_tape_examples --bin conditions
```

`recovery` stops at native work inside a guest handler. The printed explanation separates
the runtime's dynamic bindings from the guest frames, including the signaling calls beneath the
handler.
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
| Exiting restart | The runtime runs the restart function, then raises `condition.exit` at the pending request; the restart scope's region receives its typed results. |
| Restart token | An opaque identity belongs to one runtime and one dynamic extent. A same-name binding cannot revive an expired token. |
| Protected body | Normal completion and every exception run cleanup in LIFO order, in guest code. Crossed bindings pop before outer cleanup runs. |
| Unhandled condition | The embedding receives the condition and applicable restart tokens. It can invoke a restart or explicitly supply recovery values. |

`cleanup` demonstrates an exit across two protected bodies. Inner cleanup emits `20`, awaits
external work and rethrows; outer cleanup emits `10`; only then does the restart scope receive
`(42, true)`. During that wait the signaling frames and the abandoned handler callback are
already gone, so the handler binding is no longer masked when cleanup runs.

In `cleanup-replaces`, cleanup selects another exit. The original intent to return `42` is
replaced with `99`: there are two exit selections and two raised exits, and the replaced cleanup
does not run again. In `cleanup-local-exit`, an exit confined to a nested cleanup scope is caught
there, after which the original exit continues. `cleanup-signals` exercises a new signal during
cleanup using the outer handler.

Other cases cover nested signals and new handler bindings, explicit decline and normal-return
decline, repeated restart functions, returning to an unhandled-condition debugger, expired
tokens, rebinding, absent names and condition-specific restart applicability. Invalid guest
restart requests signal an `InvalidRestart` language condition; foreign debugger tokens are
recoverable denials before guest work. Names, classes, payloads and token conventions are
deliberately small and private, not a proposed core object model.

## Ownership, resource limits and evidence

Scope numbers come from the runtime and are returned to the guest, so identity never depends on
function IDs or PCs. Retiring a binding releases its guest closure handles; a retired restart
keeps only a token tombstone for expiry diagnostics. The runtime forgets callbacks and handler
masks whose frames are no longer live, using the VM's own activation identities. A malformed
external reply traps and retires language state. It cannot be used to bypass an unhandled
condition.

All callbacks and cleanup execute in the original invocation with cumulative VM fuel, host-call,
call-depth, contract and data-work limits. Polling a native wait does not spend additional guest
work. This example also caps total scope registrations at 256 per invocation and policy events
at 4,096; a scope-budget failure is a hard host failure. The native policy operations have no
separate CPU work meter. These bounds do not make the example a hardened general language runtime.

Hard VM failure or cancellation bypasses cooperative guest cleanup. Dropping during a cleanup
wait runs no further cleanup and yields an incomplete VM recording. Applications needing
guaranteed native resource release must provide it independently of guest execution.

A finished session owns two distinct kinds of evidence after the runtime is gone:

* The bounded native policy log records selected handlers, scope entry and retirement and exit
  intentions, with its own truncation flag and returned/trapped/cancelled status.
* The VM recording owns observed values, callback commands and returns, raised and caught
  exceptions and instruction history, with independent capture-completeness checks.

Checked replay reexecutes guest handlers, restarts and cleanup under the **recorded embedding
choices**. It does not rerun native handler selection, masking policy, debugger decisions or
external effects. The example's tests check those native semantics directly, then independently
check guest replay after the original runtime has been dropped. A successful replay therefore
does not certify the native policy log. Neither evidence stream is a portable session archive.

Every retained policy location, VM command, callback return, host outcome and instruction maps
to the exact authored AST snapshot. Source mapping and UI rendering remain explicit tooling work;
they are not added to ordinary verification or execution.
