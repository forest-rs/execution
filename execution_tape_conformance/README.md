# `execution_tape_conformance`

Internal conformance/regression tests for the `execution_tape` format, verifier, and VM.

`generated_guests` checks 256 deterministic expression seeds against a separate recursive
evaluator, at six inputs including integer extrema. The grammar includes wrapping integer
addition, XOR, nested conditional blocks with typed joins, aliased tuple projections, and
guest calls. Both authored programs and encoded/decoded programs execute against the model;
corrupted result-register declarations and truncated returns must be rejected. This is a bounded semantic generator,
not arbitrary-byte fuzzing or coverage of every opcode, value kind, or host boundary.

Failures report the seed, input, error and a reduced expression. The structural shrinker keeps
only smaller expressions that preserve the failure, reaching a local minimum under its own
reductions. An injected call-model defect checks the shrinker independently of VM correctness.
Reproduce one seed without editing the test:

```sh
EXECUTION_TAPE_CASE=29 cargo test -p execution_tape_conformance --test generated_guests -- --nocapture
```

