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

## Authoring and numeric baselines

```sh
cargo bench -p execution_tape_wind_tunnel --bench authoring -- --save-baseline before
cargo bench -p execution_tape_wind_tunnel --bench numeric_baselines -- --save-baseline before
cargo run -p execution_tape_wind_tunnel --bin value_layout
```

The authoring workloads cover two frontend styles equally: an emitter allocating
typed temporaries and joins as it goes, and a module of pre-numbered closure
bodies/callers with contracts, names, and spans. Construction-only and
construction-plus-verification both include destruction. They do not time guest
execution. Use the separate `verifier` benchmark for isolated preparation costs.
Round-trip execution tests check the generated programs and callable metadata.

The numeric probe times reused-context addition on bounded `I64`, `F64`, and
fixed-scale decimal inputs. Preparation, initial capacity growth, and output
checking are outside timing. Integer/decimal outputs are checked exactly; float
outputs use a tolerance. These are primitive instruction probes, not comparisons
of interchangeable numeric semantics or measurements of application workloads.

The numeric boundary group compares 128 inline loads, pooled loads, or echo-host
calls for `I64` and Decimal, with the same reuse and output-checking rules.
`numeric_storage` prints the wire version, actual prepared-buffer/frame sizes,
constant entry/blob lengths and capacities, and used recording value bytes.
Its recording figure excludes capacities, metadata, payloads, and the shared program.

The layout tool reports sizes and alignments on its current target. Its wider
decimal models show the effect of native `i128` alignment, two-word storage, and
borrowed host views. Those models implement no arithmetic and establish no
throughput, allocation, stable ABI, or cross-target guarantee. Run timing probes
sequentially, retaining the same target/toolchain and workload when comparing a
candidate; use `--baseline before` for the later run.

#### Wider decimal representation

On macOS arm64, Rust 1.99 release builds, compared with the pre-wide core `b9a6d45`, byte-aligned signed 128-bit Decimal uses
17 bytes rather than 16. `Value` stays 32 bytes, `ValueRef` 24, `Const` 32,
`ConstEntry` 16, `RecordedValue` 24, and `disasm::ConstValue` 24. Public operand
codec `DecodedOperand` grows from 24 to 32 bytes; this is a transient decoding
representation, separate from the interpreter's prepared instruction stream.

For the 1,026-instruction addition fixtures, prepared used/capacity bytes remain
73,888/73,912 for all three numeric kinds. Scalar frame elements stay 16 bytes;
the two decimal register elements grow from 32 to 34 bytes. A 1,024-value scalar
or decimal recording retains 24,576 used value bytes in either revision, excluding
the costs listed above. The all-decimal 1,024-constant fixture additionally owns
17,408 blob bytes (also its measured capacity); constant entries remain 16,384
bytes. The scalar constant fixture acquires no blob storage. These are buffer
measurements, not allocator or RSS totals, and layouts may differ on other targets.

Sequential Criterion runs (same workloads, bounded values, reused context/output
buffers) gave these middle estimates. Preparation and destruction are outside timing:

| Work | Parent | Wider decimal |
| --- | ---: | ---: |
| 1,024 I64 additions | 4.907 µs | 4.916 µs |
| 1,024 F64 additions | 4.950 µs | 4.948 µs |
| 1,024 Decimal additions | 5.152 µs | 5.268 µs |
| 128 Decimal inline loads | 0.669 µs | 0.686 µs |
| 128 Decimal pooled loads | 0.688 µs | 0.690 µs |
| 128 Decimal echo-host calls | 3.775 µs | 3.929 µs |

These small probes retain the measured arithmetic/host costs of the wider range;
they establish neither an application speedup nor a universal overhead bound.
A version that unpacked each pooled decimal during every execution took about
0.839 µs for that load fixture. Preparation now resolves those immutable literals
once into the existing prepared literal instruction; it adds no instruction rows
or per-run pool traversal. A two-word layout also worked, but used 24 bytes per
Decimal and enlarged recording rows; the byte-aligned representation avoids both.
No unsafe code, packed references, or new production dependency is required.

For reproducibility, the parent storage probe and added boundary fixtures were
copied unchanged apart from decimal constructor syntax. An unused parent-only
`--cfg execution_decimal_parent_measurement` isolated Cargo's cached artifacts
in the shared build directory; optimization settings were identical. The storage
probe prints both wire version and Decimal size to identify the compared core.

## Register authoring helper probe

A short macOS arm64/Rust 1.99 release probe (2026-10-26) compared core main
`609483a` plus these fixtures with the same streaming fixture using declared
function bodies and assembler-owned temporary allocation:

| Steps | Explicit ledger, build | Allocated body, build | Explicit ledger, build + verify | Allocated body, build + verify |
| --- | ---: | ---: | ---: | ---: |
| 256 | 6.79 us | 7.27 us | 55.84 us | 55.88 us |
| 1,024 | 21.70 us | 21.80 us | 223.29 us | 223.63 us |

These are short construction probes including destruction, not application
execution results. The 256-step construction case remains slower. Smaller
cases and unchanged lowered controls varied across runs, so these observations
do not establish a general speedup or cost bound. An initial out-of-line
allocator was about 12% slower at 1,024 steps; making the small public allocator
inline reduced that repeat to the values above. Keep the primitive, host-heavy,
and decimal probes separate when changing runtime representations.
