# Guest-driven messages and method replacement

`message_dispatch` demonstrates embedding-controlled method lookup and live replacement.
A guest sends `counter.increment()` twice. It is explicitly configured to yield each
`Object.send` request to the embedding, which selects an independently owned callable from a
method table and answers immediately. There is no I/O wait, artificial delay or async executor.
Ordinary method dispatch does not require yielding; this example uses the handoff to keep lookup
and the nested debugger under embedding control. Once the first method has entered
and loaded `+1`, lookup publishes a compatible `+10` replacement. The first invocation keeps
its original code and returns `1`; the second lookup returns `11` using the same host cell.

```sh
cargo run -p execution_tape_examples --bin message_dispatch
cargo run -p execution_tape_examples --bin message_dispatch -- --snapshot --request 1
cargo run -p execution_tape_examples --bin message_dispatch -- --interactive --color=always
cargo run -p execution_tape_examples --bin message_dispatch -- --capture /tmp/message-story
cargo run -p execution_tape_examples --bin observatory -- \
  --program /tmp/message-story/caller.tape --sources /tmp/message-story/caller.sources --snapshot
```

The live method view links to the actual pending sender, its initialized registers and its
own source map. Select the external caller in the stack to inspect that code; stepping still
controls the method. `c` runs to completion; `q` leaves the view and cancels unfinished work.
Already performed host effects and the method publication survive cancellation. `--request 1`
opens the replacement invocation, with the first reply visible in its caller.

With `--request 1`, the initial method is `+10` and the shared cell contains `1`. The gold
**CALLER WAITING** line describes `main`, which awaits its second reply; the `PAUSED` status
describes the method being controlled. `s` executes the cell addition, producing `11`; another
`s` returns it. `q` after return leaves the debugger and supplies that reply to the caller,
which finishes with `(1, 11)`. Selecting the caller only changes what is inspected.

**Fuel remaining** is the controlled invocation's execution budget. Each guest instruction
costs one unit, and hosts can report extra fuel. Waiting or browsing consumes none; resuming
does not refill the budget. Exhaustion traps rather than producing another debugger pause.

The headless result is JSON. `--capture` requires a new directory and writes a result survey,
exact tapes and source sidecars, caller/method snapshots, and boundary-recording views. Each
complete recording is checked by replay without running method lookup or touching live host
state. These separate recordings describe separate invocations; their text views are not a
portable replay archive. The sources are direct assembly fixtures, not a new language compiler.

Use `--selector missing` to inspect an owned domain error at the guest send, or `--fuel 4` to
observe a method exhausting fuel after a cell write. The caller's remaining fuel bounds each
callback, and successful callback work is charged as host extra fuel. This is one serial
embedding policy, not a general scheduler or a shared resource quota across arbitrary tasks.
The asynchronous producer API is exercised separately by conformance tests and the
`host_requests` benchmark; it is not needed by this immediate-dispatch example.
