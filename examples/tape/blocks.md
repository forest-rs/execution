# Blocks, lexical homes and shared captures

The `smalltalk_blocks` example follows one short [Smalltalk-style story](src/bin/smalltalk_blocks/story.st)
through compiled tape, live control decisions, source-linked recording and checked guest replay.
It is hand-lowered from that source fixture; there is no Smalltalk parser or complete image runtime.

```sh
cargo run -p execution_tape_examples --bin smalltalk_blocks
cargo run -p execution_tape_examples --bin smalltalk_blocks -- --normal-return
cargo run -p execution_tape_examples --bin smalltalk_blocks -- --explain --snapshot
cargo run -p execution_tape_examples --bin smalltalk_blocks -- --explore
cargo run -p execution_tape_examples --bin smalltalk_blocks -- --disassemble
cargo test -p execution_tape_examples --bin smalltalk_blocks
```

## Follow the story

The factory returns two distinct blocks that capture the same mutable cell:

```smalltalk
makeIncrement: counter
    ^ [ :amount | counter value: (counter value + amount) ]
```

Their factory activations have already returned when `run` sends `value:` to the blocks.
The first produces `41`, the second `42`. The captured environments are immutable; the cell
identity inside each environment refers to shared language-owned state. Copying a scalar into
both environments would give different semantics.

The next method creates a block with a lexical return and two protected cleanup scopes:

```smalltalk
answerUsing: counter
    | answer |
    answer := [ [ ^ counter value ]
        ensure: [ Demo wait. counter cleaned: 2 ] ]
        ensure: [ counter cleaned: 1 ].
    Demo afterEnsure.
    ^ answer
```

The inner `^` returns `42` from **the method that created the block**. It does not return from
its immediate block caller. The language runtime raises a `Block.nonLocal` exception addressed
to that home; unwinding runs the inner cleanup block, which waits for native work, then the
outer cleanup block. The observed order is `[2, 1]`, once each. The method's home region then
answers `42`, and `Demo afterEnsure` is skipped.

`--normal-return` removes that first block's `^`. Both cleanup blocks still execute in order,
but the block returns to its caller and the method reaches `Demo afterEnsure`. Both modes
produce the same numeric answer; the observed control path distinguishes them.

Finally, `makeEscaped:` returns a block that contains `^ counter value`. The block and cell
remain valid, but its lexical home method has ended. The attempted return produces an owned
`BlockCannotReturn` diagnostic with separate locations for the return block and its expired
home activation. The driver explicitly supplies `-1` as a fallback, allowing this example to
finish and replay. That fallback is an embedding choice, not a promise about Pharo's exception
resumability. A caller can instead leave the computation suspended or cancel it.

Typical final output includes:

```text
shared counter: first block → 41, second block → 42
answerUsing: → 42; cleanup order: [2, 1]
code after ensure: skipped by lexical return
escaped block after explicit recovery: -1
checked guest replay under recorded controls: [41, 42, 42, -1]
```

## Read the compiler from the declaration outward

One closure declaration establishes its captured values and caller-visible signature:

```rust
let increment = program.declare_closure(
    &[ValueType::Obj(CELL).into()],
    FunctionSig {
        arg_types: vec![ValueType::I64],
        ret_types: vec![ValueType::I64],
    },
)?;
let mut body = program.function_asm(increment.function())?;
let [counter] = increment.load_captures(&mut body)?;
let amount = increment.argument(0)?;
let [previous] = program.emit_call(&mut body, read, &[counter])?;
```

The declaration derives the exact environment contract, hidden body argument and exposed call
signature. Body emission remains detached from the program builder. `emit_call` derives result
register kinds and supplies the call convention. Ordinary arithmetic and typed control flow use
`Asm` directly; there is no parallel family of arithmetic APIs.

The fixture uses typed integers, opaque cells, numbered lexical homes and aggregate method-result boxes.
The result representation matters for lexical returns: `makeEscaped:` normally answers a block,
but its inner `^ counter value` would answer an integer to the same method while that home was
active. A one-member aggregate permits both values in that method's declared return kind;
its member contract is deliberately unconstrained. `answerUsing:` has the more precise integer
member contract. Because an unconstrained member does not prove that a value is callable, the private
`Object.requireBlock` primitive checks and unboxes the returned payload before typed invocation.
These are private fixture conventions, not a general Smalltalk object ABI. The root returns one
four-member tuple for the final report.

## Keep guest control and language policy distinct

`ensure:` compiles inline. Its protected code runs inside a region whose `catch_all_ref` clause
calls the cleanup block and rethrows; normal completion calls the same cleanup block. Each method
with a lexical home runs inside a region that catches `Block.nonLocal(home, boxed)` and answers
the boxed value when the home matches; otherwise it rethrows.

`runtime.rs` owns only the lexical-home table. `Block.home` records the exact `ActivationId` of
the creating method, so recursion or a later activation of the same function cannot revive an
expired home. `Block.return` checks that activation is still live: if so, it boxes the value and
completes the pending request with `PendingHostCall::throw`; if not, it reports
`BlockCannotReturn`. Home numbers are never reused. The example caps registered homes at 64 per
invocation. Cells belong to the invocation's host; no shared-state or garbage-collection policy is
installed in core. During a cleanup wait, the returning block's frame has already been unwound.

Every block and cleanup shares the invocation's existing hard budgets. Repeated polls at a native
wait or an expired return do not spend guest fuel or data work. Hard VM failure and cancellation
execute no guest cleanup. Native resources needing unconditional release must have their own
lifetime mechanism.

All code in this example belongs to **one program and invocation**. Independently published
method revisions running in different invocations still need an explicit control-composition
architecture. This fixture does not establish cross-program non-local returns, reflective context
mutation, a full Smalltalk exception protocol or image persistence. The broader condition example
separately exercises cleanup that signals or replaces a selected exit.

## Inspect what the evidence actually says

`--explain` prints language decisions at their **pending service sites** and shows the guest
frames during cleanup. The policy log is native evidence, not a new guest instruction stream; the
raised return, its catch and the cleanup instructions are VM evidence in the recording.

The authored source is a snapshot of `story.st`, labelled `story.st (assembled fixture)` in the
viewer. `Asm::source` records exact locations during emission, including generated capture loads,
closure construction and returns. Source attachment checks the exact encoded program identity;
switching between lexical and normal returns rejects a stale map.

Checked replay runs the guest blocks and cleanup again, under the recorded host outcomes,
including the raised return. It does not rerun the native lexical-home table, cell mutations or
debugger fallback. Tests check those live semantics separately, then replay after the original
runtime is gone. Cancellation leaves an explicitly incomplete recording that replay rejects.
The session is an in-memory development artifact, not a portable image or recording archive.
