# Tape examples

The [examples guide](../README.md) lists every runnable binary, what it demonstrates and how to
run it. This unpublished package owns the teaching examples, inspection tools and executable
profiling integrations; the core crates retain no UI or capture-backend dependency.

The larger examples have focused walkthroughs:

- [Blocks and lexical homes](blocks.md): shared captures, `^`, `ensure:` and an expired home.
- [Conditions above the VM](conditions.md): handlers, restarts and cooperative cleanup.
- [Messages and method replacement](messages.md): method lookup, live publication and a waiting caller.
- [Composed language control](live-language.md): an escaped block, independent methods, conditions,
  original cleanup and an exact lexical home within a language-owned task.

For measurement workloads, use the [tape wind tunnel](../../execution_tape_wind_tunnel/README.md).
Its recording and block-parameter benchmarks reuse the source under `src/support/`.
