# Tape wind tunnel

The `vm` Criterion benchmark separates preparation, scalar arithmetic, calls,
aggregate operations, and byte/string boundaries. The `recording` benchmark
measures capture/replay work separately. Run them with `cargo bench -p
execution_tape_wind_tunnel`; use `--bench vm -- --list` to inspect individual cases.

## Resumable execution and optional inspection

`stepping` separates ordinary execution, reusable contexts, scheduling quanta, frame metadata,
verifier-backed register queries and conservative host-object traversal. Its scalar fixture
executes 1,000 additions with an opaque receiver argument. Programs are prepared outside timing;
execution cases include return/destruction and reuse the VM. Scheduled runs own fresh scratch
and activation identities. Inspection starts at a fixed suspension; cached queries reuse one
inspection view. Root traversal includes an existing 1,024-field aggregate without growing it
between samples. No UI drawing, recording or application object-graph traversal is timed.

On macOS arm64, Rust 1.99 release builds, 2026-10-28, a repeat with 30 samples, 1 s warmup and
3 s measurement gave these point estimates:

| Case | Time |
|---|---:|
| Ordinary `run`, 1,000 additions | 4.750 µs |
| Ordinary reused context, 1,000 additions | 4.751 µs |
| Scheduled, quantum 1 | 11.504 µs |
| Scheduled, quantum 64 | 4.475 µs |
| Scheduled, one uninterrupted quantum | 4.239 µs |
| One frame's metadata | 5.34 ns |
| Fresh register query, including analysis and drop | 31.47 µs |
| Cached register query | 61.48 ns |
| Cached inventory, 1,026 object references | 1.056 µs |

This small scalar fixture does not establish that scheduling is generally faster than ordinary
execution. Different interpreter specializations and code layout can affect dispatch. Single
steps pay a resume call per instruction; optional analysis and object inventory remain explicit
inspection costs. Metadata alone does not build register facts. A short initial run was noisy,
especially for scheduled execution; the repeat above is the more stable observation.

The ordinary `vm` suite was also compared with main `48284bd`, using the same release toolchain
and fixtures. The initial main baseline used 10 samples with 1 s warmup/measurement; the initial
candidate used 30 samples with 1 s warmup and 3 s measurement. Both sides were then rebuilt and
repeated with the longer candidate settings:

| Ordinary workload | Main initial | Candidate initial | Main repeat | Candidate repeat |
|---|---:|---:|---:|---:|
| 10 additions | 161.2 ns | 159.4 ns | 176.9 ns | 163.5 ns |
| 1,000 additions | 6.159 µs | 4.940 µs | 5.960 µs | 5.281 µs |
| 1,000 guest-call iterations | 86.12 µs | 76.78 µs | 89.66 µs | 77.27 µs |
| 10,000 branch-loop iterations | 440.4 µs | 464.7 µs | 495.6 µs | 460.1 µs |
| 1,000 host-call iterations | 74.12 µs | 75.98 µs | 82.32 µs | 75.63 µs |

These are unpaired local observations, not a general speedup or zero-overhead claim. The initial
roughly 6% branch-loop and 3% host-loop regressions reversed in the repeat; baseline variation is
large enough that neither sign establishes a robust workload-wide conclusion. Ordinary execution
has no scheduling test or activation bookkeeping in its per-instruction specialization, and
prepared programs retain no debugger indexes; refactoring still changes generated code.
Forcing interpreter inlining traded better branch timing for worse host/call timing and was
reverted. These probes exclude preparation, retained-memory/RSS and real host work.

```sh
cargo bench -p execution_tape_wind_tunnel --bench stepping -- \
  --warm-up-time 1 --measurement-time 3 --sample-size 30 --noplot
```

Use the ordinary `vm` cases with `--save-baseline <name>` on main, then `--baseline <name>` on
the candidate. Keep benchmark output in a locally ignored directory; do not compare a candidate
only against an older optimization branch.

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

## Unit registers without payload storage

The `unit_registers` fixture compares main `84cbb9b` with removal of the Unit bank on
macOS arm64, Rust 1.99 release builds, 2026-10-27. Each run executes 16 move/select,
simultaneous join, guest-call and host-call groups, returning two Unit values. Most of
the wide fixture's declarations are unused: preparation must still validate/count them,
while execution should not allocate a payload for them. Preparation and result checking
are outside timing. Fresh-VM timing includes destruction; reused-context timing clears
the output vector while retaining its capacity.

Criterion uses 40 samples, 0.3 s warmup and 1 s measurement. Point estimates:

| Workload | Main | Without Unit bank |
|---|---:|---:|
| 8 Unit declarations, fresh VM | 2.061 µs | 1.763 µs |
| 8 Unit declarations, reused context | 1.892 µs | 1.674 µs |
| 65,535 Unit declarations, fresh VM | 3.439 µs | 1.753 µs |
| 65,535 Unit declarations, reused context | 3.151 µs | 1.665 µs |
| 1,024 integer additions (control) | 4.874 µs | 4.802 µs |

The integer-add control detects no statistically significant change. The Unit workloads
are targeted microbenchmarks, not a claim about application speedups. `ExecutionContext`
shrinks from 464 to 440 bytes on this target. Reported frame register elements drop from
37 / 262,145 bytes to one byte in both fixtures (the Boolean condition). This removes the
Unit bank and its allocation, not payload-bearing registers, result buffers, or frame vectors.
Unit block cycles also stop creating prepared copy operands and scratch slots. Logical
declaration counts remain visible; source instructions still consume fuel and emit events.

```sh
cargo bench -p execution_tape_wind_tunnel --bench unit_registers -- \
  --save-baseline before --warm-up-time 0.3 --measurement-time 1 --sample-size 40 --noplot
# Repeat on the comparison revision with --baseline before.
```

Earlier numeric-storage measurements below describe their stated revisions, which still
had the Unit bank. Current numeric fixtures use four fewer frame-register bytes per effect
declaration; their decimal representation and wire encoding are unchanged by this removal.

## Aggregate construction work

The `aggregate_work` benchmark constructs 128 temporary tuples, structs, or arrays per run,
with either zero or 1,024 repeated integer fields per node. It returns the final length and
checks both that result and the absence of retained aggregates before timing. Preparation is
outside timing; the reused-context run includes construction, field materialization, and
temporary cleanup. The output vector is cleared each iteration, retaining its capacity.

A short macOS arm64 release probe compares `10c1748` with aggregate work accounting, using
30 samples, 0.2 s warmup and 0.5 s measurement on 2026-10-26. Both use default limits; the
wide fixture consumes 131,200 nodes/fields, below the new 1,000,000-unit default.

| 128 constructions | Before | With budget |
|---|---:|---:|
| Empty tuple | 1.325 µs | 1.264 µs |
| Empty struct | 1.278 µs | 1.281 µs |
| Empty array | 1.273 µs | 1.266 µs |
| Tuple, 1,024 fields | 790.5 µs | 780.0 µs |
| Struct, 1,024 fields | 777.3 µs | 775.3 µs |
| Array, 1,024 fields | 777.5 µs | 778.6 µs |

An existing 1,024-integer-add control measured 5.194 µs before and 4.806 µs after. There is
no aggregate debit on the scalar instruction path, but these short, unpaired runs include
code-layout and machine-state effects. The empty-tuple baseline also has a broad interval.
These observations support neither a general speedup nor a zero-overhead claim. They do
not measure real application workloads, preparation, host allocation, or retained heap/RSS.

```sh
cargo bench -p execution_tape_wind_tunnel --bench aggregate_work -- \
  --save-baseline before --warm-up-time 0.2 --measurement-time 0.5 --sample-size 30 --noplot
# Repeat on the comparison revision with --baseline before instead of --save-baseline before.
```

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
73,888/73,912 for all three numeric kinds. Scalar frame elements stay 20 bytes;
the decimal fixture grows from 36 to 38 bytes. These totals include the four-byte
effect-register bank entry, which earlier inspection counts omitted. A 1,024-value scalar
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
