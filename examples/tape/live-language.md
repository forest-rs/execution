# Independently compiled methods in one language task

`live_language` combines the block, condition and live-publication stories across three independently
prepared code domains. Each invocation owns its heap and retains its exact code and source snapshot.
A small language-owned task connects them through retained callables and scalar tokens.

```sh
cargo run -p execution_tape_examples --bin live_language -- --explain
cargo run -p execution_tape_examples --bin live_language -- --normal-return --explain
cargo test -p execution_tape_examples --bin live_language
```

The [frontend](src/bin/live_language/program.rs) hand-lowers the source snapshots printed by the
example. This is a finite executable composition policy, not a Smalltalk parser or a general
language runtime. Read its declarations first, then follow the semantic operations in the
[task runtime](src/bin/live_language/runtime.rs).
There is one condition and one statically selected handler in this fixture. The
[condition example](conditions.md) separately exercises dynamic matching, decline, masking,
restart selection and cleanup that signals or replaces an exit.

## Follow one message across code owners

1. `run` records its exact lexical activation and calls a block factory. The factory returns
   a closure capturing that home; its own activation ends before the block is invoked.
2. The task explicitly retains the closure with its original program and immutable environment.
   A scalar block token crosses the message boundary. No raw closure or aggregate handle is
   interpreted in another program.
3. An independently compiled `through:` method establishes the example's protection and handler
   policy, retaining its cleanup closure before starting the block in another invocation.
4. The block signals `NeedValue`. A third program runs the handler, which waits for the driver
   to choose a recovery value. The original sender, protected method and block all remain paused.
5. While the handler waits, the driver publishes a compatible replacement for `through:`.
   Future lookup selects revision 1; the pending method and retained cleanup keep revision 0.
6. The driver supplies `42`. The handler returns it, the block selects its lexical home, and
   the task consumes the protection binding before entering its original cleanup.
7. Cleanup waits for native work, then records `10`. The replacement would record `100`.
   Only after cleanup returns does the task retire the crossed invocations and request the
   checked return inside the home VM. The final answer is `42`.

At the first native wait, `--explain` prints:

```text
Language task → independent VM frames (outermost first):
  lexical home: home.st · Runtime send: (Runtime retain: block)
  protected method / revision 0: through.st · Runtime protect: block
  escaped block: home.st · Runtime signal: #NeedValue
  NeedValue handler: handler.st · Runtime chooseRecovery
```

These are real paused VM frames grouped by their independent invocations. The returned factory
is absent. Program-local function IDs intentionally overlap, so every source lookup carries its
exact code owner. The text explanation also shows the different stack at the cleanup wait and
the source of each language decision. It does not add a multi-invocation view to Observatory yet.

`--normal-return` lets the block return normally instead of selecting its lexical home. The
same original cleanup runs, then both the protected method and sender execute their following
code. All five invocations return normally and support checked guest replay.

## Control and evidence have explicit boundaries

The VM's `return_from` accepts targets from its own invocation. Passing the home activation to
the block's VM is a recoverable `ForeignInvocation` denial; matching local activation ordinals
do not establish identity. The task must perform the transfer in the home VM, after its language
cleanup has completed. This example uses a single, non-reused home token and block token within
one task; those integers are not capabilities to exchange between tasks.

In the lexical-return case, the crossed block and protected-method invocations are **cancelled**.
Core cannot describe a cross-invocation unwind, and their recordings truthfully report
`MissingCompletion` and reject replay. The handler, cleanup and home each return successfully
and are replayed independently. The home recording sees the transfer at its pending send;
the separate language event identifies the block's initiating return source.

Successful guest replay checks each complete invocation under recorded host outcomes and
control commands. It does not reexecute method lookup, verify the task's handler choice, or
reconstruct this language cleanup policy. The native task tests independently check those
decisions. This example establishes no portable task recording or combined replay guarantee.

Instruction fuel is shared by capping each scheduling quantum against the task's remaining
fuel and debiting actual VM fuel consumption, including the final typed transfer. Polling native
waits spends none. `--fuel N` changes that task budget. Each VM still has its own default host-call,
contract, aggregate and payload limits; a shared data-work/retained-memory policy remains future
work. The fixture executes no synchronous host dispatch or host extra-fuel charges.
The report sums `Invocation::work` across live and retired VMs exactly once, including entry
checks, imported closure captures and pending host staging. Fuel counts actually debited units;
the other counters retain their respective VM units. This accounting does not install shared
host-call, contract or allocation ceilings. Native capture copies are outside the VM work meter.

Cancelling or exhausting fuel retires the task without running further guest cleanup. Native
effects already performed remain real. The example's tests also execute the retained block
after its home ends: its code and captures remain valid, but its lexical return is denied.

## What this slice establishes

Retained callables, compatible code publication, exact activations and existing source mappings
can compose into this finite language task without changing core opcodes or the wire format.
The remaining seams are visible: shared resource accounting, first-class causal/unwind evidence,
multi-invocation debugging, and a deliberate ABI for general dynamic values. Those need further
design before this task policy becomes reusable infrastructure.
