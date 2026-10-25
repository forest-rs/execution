# Tape wind tunnel

The `vm` Criterion benchmark separates preparation, scalar arithmetic, calls,
aggregate operations, and byte/string boundaries. The `recording` benchmark
measures capture/replay work separately. Run them with `cargo bench -p
execution_tape_wind_tunnel`; use `--bench vm -- --list` to inspect individual cases.

## Payload-work accounting probe

The initial budget probe compares main `f9aaa54` with logical payload-work
accounting on macOS arm64, Rust 1.99 release builds, 2026-10-25. Both revisions use
Criterion 0.8.2 and the same fixtures. Programs and reusable contexts are prepared
outside the timed loop; each iteration runs the VM and destroys its returned
values. Payload fixtures contain 65,536 bytes. Limits are explicitly wide open;
the budget checks still execute at payload work sites.

A short first comparison used 20 samples, 0.2 s warmup, and 0.5 s measurement.
Scalar repeats used 30 samples, 0.3 s warmup, and 1 s measurement. Reported times
are Criterion point estimates, with the repeat shown separately:

| Workload | Main | Budget | Budget scalar repeat |
|---|---:|---:|---:|
| 10 integer additions | 191.6 ns | 183.7 ns | 186.5 ns |
| 1,000 integer additions | 5.234 µs | 5.682 µs | 5.275 µs |
| Borrowed bytes input → length | 56.3 ns | 55.6 ns | — |
| Bytes input → owned root result | 923 ns | 857 ns | — |
| Host bytes → owned root result | 1.727 µs | 1.621 µs | — |
| Host string → owned root result | 2.234 µs | 1.808 µs | — |

The 1,000-add probe initially regressed about 7%; its repeat was much closer to
baseline. The implementation adds no payload check to scalar instruction dispatch,
but code layout and per-run context changes can still affect those timings. These
short, unpaired runs support neither a general speedup nor a zero-overhead claim.
The boundary probes also do not measure every charged opcode, host application
work, retained heap, or process RSS.

Reproduce the selected cases on each revision, saving the first as a Criterion
baseline and comparing the second with `--baseline <name>`:

```sh
cargo bench -p execution_tape_wind_tunnel --bench vm -- \
  '^(i64_add_chain/(10|1000)|payload/(bytes|str)_(input|host)_(len|return)/65536)$' \
  --warm-up-time 0.2 --measurement-time 0.5 --sample-size 20 --noplot
```

Correctness tests separately exercise concatenation growth, exact/one-short
budgets, comparisons, slices, conversions, aggregate/root copies, borrowed
projections, host admission, reset/rollback, and checked replay of budget traps.
The work budget is not an allocation or resident-memory budget.
