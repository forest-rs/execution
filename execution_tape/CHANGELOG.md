<!-- Instructions

This changelog follows the patterns described here: <https://keepachangelog.com/en/>.

Subheadings to categorize changes are `added, changed, deprecated, removed, fixed, security`.

-->

# Changelog

Changes for Execution Tape [0.1.0](#010-2026-10-27), dated 2026-10-27, are documented below.

## [Unreleased][]

This release has an [MSRV][] of 1.88.

### Added

- Add deterministic `f32` math intrinsics under the `0xFD` prefix, in an `f32` sub-block from
  `0xA0`: `f32.pow` (`0xA0`), `f32.exp`, `f32.ln`, `f32.sin`, `f32.cos` and `f32.atan2`
  (`0xA5`). Their results are defined as those of the `libm` crate at exactly version 0.2.16,
  which is now a dependency (pinned with `=`, default features off): bit-identical on every
  platform for non-NaN results, NaN-ness only for NaN results, and not correctly rounded.
  Upgrading `libm` changes results and will be called out here. Also add `f32.clamp` (`0xFD
  0x99`), which traps with the new `Trap::ClampBoundsInvalid` unless `lo <= hi` and otherwise
  matches `f32::clamp` bit for bit, and `f32.round_half_away` (`0xFD 0x98`) and
  `f64.round_half_away` (`0xFC 0x0B`), exact rounding with ties away from zero (C's
  `roundf`/`round`), beside the existing ties-to-even `nearest`. `Asm` gains `f32_pow`,
  `f32_exp`, `f32_ln`, `f32_sin`, `f32_cos`, `f32_atan2`, `f32_clamp`, `f32_round_half_away`
  and `f64_round_half_away`; `OperandRole` gains `Lo` and `Hi`.

- Add `execution_tape::math`, the floating-point operations the VM computes as plain functions
  (`math::pow_f32`, `math::round_half_even_f32`, `math::min_num_f64`, ...), so native code can
  compute exactly the bits a tape program does. Each function documents its `libm` counterpart.

- Add `Vm::replace_host`, which moves a VM's installed units, retained aggregates and limits to
  a VM around another host and returns the previous one. A host that borrows per-call state can
  now run against units installed once, by swapping it in for a call and a parked host back
  out, instead of building a VM and reinstalling for every call. The crate documentation shows
  the pattern, and the `borrowed_host` wind-tunnel bench compares it with a fresh VM per call.

- Add `VerifyError::display_in(&program)`, which renders a verifier diagnostic with what the
  rejected program's debug metadata knows about its location: the function name, the nearest
  label and offset, the span, the instruction, and the input name of an argument register. Add
  `VerifyError::register`, beside `function` and `pc`, for the register a diagnostic names.

- Declare a host operation once with `host_binding::HostImport`, a `const`-constructible symbol
  and complete signature. `ProgramBuilder::host_import` adds it to a program's import table,
  `HostFunctions::implement` registers its callback, `HostCallSite::calls` matches its call sites
  in a link check, and it converts into a `host_catalog::HostSpec`.

- Pass constants directly as call arguments with `ProgramBuilder::emit_call_with`. Arguments are
  a tuple, such as `(handle, "name", 0.5_f32)`, or a slice of the new `CallArg`; a `u32` is a
  register and integer constants are spelled `CallArg::U32(1)`. Each constant is loaded into a
  fresh register just before the call, strings and bytes are interned once in the constant pool,
  and the bytecode is identical to the hand-written loads. `ProgramBuilder::emit_const_str` loads
  one interned string into a fresh register. `CallBuildError` gains `ConstantKind`.

- Expose the arguments of host calls that are known before a program runs.
  `VerifiedProgram::host_call_sites` lists every reachable `host_call` with its import, symbol,
  function, pc and span, and each argument as `Some(ValueRef)` when every path to the call loads
  the same constant into it (through constants, `mov` and block arguments). Nothing is guessed:
  disagreeing joins, other writes, function arguments and call results are `None`.
  `HostFunctions::bind_checked` binds and passes each call site to an embedder check; rejected
  calls are reported as `CallError`s with their location. Migration: `BindingError` gains a
  `calls` field (empty from `bind`).

- Add 32-bit numerics under the `0xFD` prefix (`OpcodePrefix::Numeric32`), in aligned blocks:
  `i32` from `0x00`, `u32` from `0x40`, `f32` from `0x80` and conversions from `0xC0`. Each
  family mirrors its 64-bit counterpart (constants, wrapping and `_ovf` arithmetic, trapping
  `div`/`rem`, bitwise operations, shifts masked with `& 31`, comparisons; `u32` bit counting
  and rotation; the full `f32` set of `f64` operations). Conversions cover sign and zero
  extension, trapping and wrapping narrowing, `f32`/`f64` promotion and demotion, trapping and
  saturating float-to-integer, integer-to-float rounding once, `f32` bit casts, and bit
  reinterpretation between signed and unsigned of equal width, including the previously missing
  `i64.to_u64_wrap`/`u64.to_i64_wrap`. `f32` rounding and square root reuse the exact `no_std`
  `f64` routines and stay correctly rounded. Each opcode has an `Asm` method named after its
  mnemonic (`i32_add`, `f64_to_f32`, ...), plus `const_i32`, `const_u32`, `const_f32` and
  `const_f32_bits`. Operand metadata gains `OperandKind::ImmI32`, `OperandEncoding::I32Sleb`
  and `OperandEncoding::U32Le`, the codec gains `Operand::ImmI32` and `DecodedOperand::ImmI32`,
  and disassembly gains `ConstValue::{I32, U32, F32Bits}`. Migration: handle the new `Opcode`,
  `OperandKind`, `OperandEncoding`, `Operand`, `DecodedOperand` and `ConstValue` variants in
  exhaustive matches.

- Add 32-bit value types: `ValueType::{I32, U32, F32}`, `Value::{I32, U32, F32}`,
  `ValueRef::{I32, U32, F32}` and `RecordedValue::{I32, U32, F32}` (`F32` records its bits). They
  have wire type tags 13, 14 and 15 and host signature tags of the same numbers, so existing
  signature hashes are unchanged. Registers of all three share one bank of 32-bit slots, so a
  frame grows by one bank rather than three. `mov`, `select`, calls, returns, host calls,
  aggregates, recording, replay and observation carry them like the 64-bit kinds. Migration:
  handle the new `ValueType`, `Value`, `ValueRef` and `RecordedValue` variants in exhaustive
  matches.

- Reserve a prefixed opcode space so new instruction families have room beyond the single-byte
  opcodes. Lead bytes `0xF0..=0xFF` are prefixes; an assigned prefix is followed by exactly one
  opcode byte. `0xFC` (`OpcodePrefix::Extended`) is the first, and new families allocate the next
  free byte there in `opcodes.json`. Single-byte opcodes keep their bytes and decode as before.
  `Opcode` gains `code`, `from_code`, `prefix`, `from_prefixed`, `encoded_len` and `encode`;
  `Opcode::byte` is now the opcode's final byte. The verifier reports an unassigned opcode as
  `VerifyError::UnknownOpcode` with its function, pc and code instead of `BytecodeDecode`.
  Migration: handle `VerifyError::UnknownOpcode` in exhaustive matches; write opcodes with
  `Opcode::encode` rather than `as u8`.

- Count and rotate `u64` bits: `u64.clz` (`0xFC 0x00`), `u64.ctz` (`0xFC 0x01`), `u64.popcnt`
  (`0xFC 0x02`), `u64.rotl` (`0xFC 0x03`) and `u64.rotr` (`0xFC 0x04`), emitted with
  `Asm::{u64_clz, u64_ctz, u64_popcnt, u64_rotl, u64_rotr}`. They are the first opcodes in the
  extended space and cover WebAssembly's 64-bit bit-counting and rotation operations. `clz` and
  `ctz` of zero are 64, and rotation amounts are masked with `& 63`. Migration: handle the new
  `Opcode` variants in exhaustive matches.

- Switch stacks with one-shot continuations, shaped after WebAssembly stack switching.
  `cont.new` (`0xFC 0x05`) makes an unstarted continuation of a `Func` or `Closure`, `resume`
  (`0xFC 0x06`) runs one on top of the resuming activation, and `suspend` (`0xFC 0x07`) captures
  the activations up to the nearest handler of a tag. Continuations live in the new
  `ValueType::Cont` register kind, are typed by call signatures (checked at runtime against
  canonical contracts, across units), and trap with `Trap::ContinuationConsumed` when reused.
  Handlers are `on` clauses of protected regions (`CatchClause::On`, `Catch::On`): a suspension
  consults the clauses protecting each `resume` outward, receives the payload and the suspended
  continuation, and traps with `Trap::UnhandledSuspension` when none matches or
  `Trap::SuspensionAcrossHost` when it would capture a pending host request. Suspended
  activations close their profiling scopes with `TraceOutcome::Suspended`
  (`ActivationOutcome::Suspended` in recordings) and re-enter them when resumed; their frames
  count against `Limits::max_call_depth` until resumed or the invocation ends.
  Defining a tag whose payload or results hold `Cont` fails with
  `TagError::ContinuationPayload`, and checked replay compares the new traps.
  Migration: handle `ValueType::Cont`, `TraceOutcome::Suspended`, `ActivationOutcome::Suspended`,
  `CatchClause::On`, `Catch::On`, and the new `Trap`, `VerifyError`, `RegionError` and
  `TagError` variants in exhaustive matches.

- Continue a continuation by raising an exception at its suspension point with `resume_throw`
  (`0xFC 0x08`), for cleanup and cancellation of generators and coroutines. Its own protected
  regions may catch the exception; an unstarted continuation is discarded and the exception
  raised at the `resume_throw`. Exceptions escaping any continuation reach the catch clauses of
  the `resume` that ran it. Migration: handle the new `Opcode` variant in exhaustive matches.

- Bind the leading inputs of a continuation with `cont.bind` (`0xFC 0x09`), retyping it from one
  call signature to another with exactly those arguments removed (`VerifyError::ContBindSignature`).
  Values bound to a suspended continuation satisfy the tag's result contracts, statically where
  known and otherwise when bound.
  Migration: handle the new `Opcode` and `VerifyError` variants in exhaustive matches.

- Release an abandoned continuation with `cont.drop` (`0xFC 0x0A`), emitted with
  `Asm::cont_drop`. It consumes a continuation of any type without resuming it, frees its frames
  from `Limits::max_call_depth` and its slot for reuse, and traps with
  `Trap::ContinuationConsumed` when the continuation was already consumed or dropped; stale
  references never name a later continuation in the reused slot. Its activations run no code,
  so a producer that needs cleanup is unwound with `resume_throw` instead, and continuations
  held in its registers are not released with it. Recordings and replay
  cover it as ordinary guest control. Migration: handle the new `Opcode` variant in exhaustive
  matches.

- `InvocationInspection::visit_objects` reports the host objects in initialized registers of
  frames held by unresumed continuations, as `ObjectRootOrigin::Continuation`, so a collector
  running mid-invocation no longer frees objects a continuation can still use. Frames of
  unstarted continuations report only their bound arguments. Exception reference registers, in
  active or held frames, now report the objects in their exception's payload. Inspecting a
  function that declares a `Cont` register no longer panics. Migration: handle
  `ObjectRootOrigin::Continuation` in exhaustive matches.

- Recordings and checked replay cover stack switching without new evidence: suspension and
  resumption are deterministic guest control, re-executed on replay, including in invocations
  paused at any instruction boundary inside a continuation. Migration: no caller changes are
  required.

- Let suspensions that no guest handler handles reach the embedding. An invocation started with
  `InvocationOptions::yield_suspensions()` reports `RunStatus::AwaitingHandler` instead of
  trapping with `Trap::UnhandledSuspension`, and `Invocation::pending_suspension` borrows a
  `PendingSuspension` with the tag, payload and result kinds, inspection of the suspended frames,
  host access and installs. The embedding answers once: `resume` with values for the tag's
  results (checked for count, kind and payload work like a host return, a wrong count trapping
  `Trap::ArityMismatch`, and against the suspending unit's result contracts as
  `ContractPosition::Resumption`), `resume_throw` to raise an exception at the `suspend`, or
  `discard` to end the invocation with `UnhandledSuspension`. Suspensions inside guest callbacks
  still trap with `SuspensionAcrossHost`, and default invocations and `Vm::run` keep trapping.
  A suspension's payload contracts are now checked before it traps as unhandled, so the trap
  does not depend on whether the invocation yields suspensions.
  The suspended activations close their scopes as `Suspended` and re-enter when answered. Trace
  sinks observe `ValueEvent::SuspensionYielded` and `ValueEvent::SuspensionAnswered` (with
  `trace::SuspensionAnswer`), recordings retain `Recording::suspensions` (`YieldedSuspension`,
  `RecordedAnswer`, bounded by `RecordLimits::suspensions`), and replay checks each payload and
  answers again. Migration: handle `RunStatus::AwaitingHandler`, the new `ValueEvent`,
  `ContractPosition`, `CaptureResource`, `ReplayBoundary` and `Divergence` variants in
  exhaustive matches, and initialize `RecordLimits::suspensions` in struct literals.

- Declare the values that resume a suspension to a tag with
  `ProgramBuilder::declare_tag_with_results` and `import_tag_with_results`, or
  `Vm::define_tag_with_results` for embedding-defined tags, so the store tags that units share
  for exceptions can also type suspensions. They are encoded in the optional `tag_results`
  section (16), read with `Program::tag_results` and `Units::tag_results`, and must match on
  import like payloads. `throw` ignores them. Migration: code constructing `TagEntry` directly
  must initialize `results`, and `RecordedTag` gains `results`.

- List every entered guest activation with `Recording::activations` and `Recording::activation`,
  without instruction history. Each `Activation` has its entry ordinal (the live
  `ActivationId::ordinal`), function and unit, caller, parent, depth and entry sequence, and an
  `ActivationExit` with its location, `ActivationOutcome` (returned, caught, trapped or
  cancelled) and exit sequence. Sequences place activations among host calls, control commands
  and callback returns without renumbering them. Migration: no caller changes are required.

- Reach the host from a waiting request with `PendingHostCall::host` and `host_mut`. A driver
  can consult or update its runtime and then complete, dispatch or call back on the same view,
  instead of copying the request out, dropping it, using `Invocation::host_mut` and querying the
  request again. Migration: no caller changes are required.

- Document the lifetime of handles read from yielded requests. Aggregate, closure and function
  handles read from any request of an invocation (arguments, callback results and what they
  reach) stay valid until the invocation commits its root or rolls back, and may be passed to
  later `PendingHostCall::call_guest` and `complete` calls. A conformance test pins this.
  Migration: no caller changes are required.

- Complete WebAssembly scalar `f64` parity with `f64.floor` (`0x94`), `f64.ceil` (`0x95`),
  `f64.trunc` (`0x96`), `f64.nearest` (`0x97`, ties to even), `f64.sqrt` (`0x98`, correctly
  rounded), `f64.copysign` (`0x99`), the saturating conversions `f64.to_i64_sat` (`0x9A`) and
  `f64.to_u64_sat` (`0x9B`), and `f64.ne` (`0x9C`), emitted with the matching `Asm` methods.
  They are exact and implemented in the `no_std` core without `libm`, so results are
  bit-identical across platforms; NaN handling follows WebAssembly. The trapping
  `f64.to_i64`/`f64.to_u64` conversions are unchanged. Migration: handle the new `Opcode`
  variants in exhaustive matches.

- Compare, inspect, rescale and divide decimals in guest code: `dec.cmp` (`0x9D`, numeric
  `-1`/`0`/`1` ignoring scale, so `1.0` equals `1.00`), `dec.scale` (`0x9E`), `dec.rescale`
  (`0x9F`) and `dec.div` (`0xA0`), emitted with `Asm::{dec_cmp, dec_scale, dec_rescale,
  dec_div}`. Rescale and division take the target scale from a `u64` register and a
  `DecimalRounding` immediate (half-even, half-away-from-zero, toward-zero, floor, ceiling); they
  round once from the exact result and trap with `DecimalOverflow` when it does not fit (or the
  scale exceeds 255), and division traps with `DivByZero` on a zero divisor. Division costs one
  fuel, plus one per long-division bit step when its scaled numerator exceeds 128 bits (up to 256
  more), charged before it divides. The verifier rejects
  unknown modes with `VerifyError::InvalidDecimalRounding`, and disassembly shows them as
  `InputIndex::Rounding`. Migration: handle the new `Opcode`, `OperandRole::Rounding`,
  `InputIndex::Rounding` and `VerifyError::InvalidDecimalRounding` variants in exhaustive matches.

- Detect integer overflow without trapping: `i64.add_ovf` (`0xA1`), `i64.sub_ovf` (`0xA2`),
  `i64.mul_ovf` (`0xA3`) and the unsigned `u64.add_ovf` (`0xA4`), `u64.sub_ovf` (`0xA5`),
  `u64.mul_ovf` (`0xA6`), emitted with `Asm::{i64_add_ovf, i64_sub_ovf, i64_mul_ovf, u64_add_ovf,
  u64_sub_ovf, u64_mul_ovf}`. Each writes the wrapped result and a `bool` overflow flag, matching
  Cranelift's `sadd_overflow`/`uadd_overflow` family. Disassembly lists both destinations.
  Migration: handle the new `Opcode` variants in exhaustive matches.

- Raise guest exceptions from hosts. A host returns `HostError::throw(tag, payload)` with a VM
  `TagRef`, and a yielded request can complete with `PendingHostCall::throw`; the exception is
  raised at the `host_call` and is caught by protected regions like a guest `throw`.
  `HostContext::tag` resolves the calling program's `TagId` to its `TagRef`. Tags the VM never
  issued trap with `Trap::UnknownTag`. Observers see `HostCallOutcome::Threw`, and recordings keep
  the payload as `HostOutcome::Threw` so replay delivers the same exception.
  Migration: handle `HostError::Throw`, `HostCallOutcome::Threw`, `HostOutcome::Threw` and
  `Trap::UnknownTag` in exhaustive matches.

- Rethrow caught exceptions by reference. The new `ValueType::Exn` register kind holds a caught
  exception; `Catch::TagRef` and `Catch::AllRef` (`CatchClause::TagRef`/`AllRef`) bind it as the
  handler's final parameter, and `Asm::throw_ref` propagates it again with its original tag,
  payload and throw site (`UncaughtException::origin`). Handlers implement cleanup by catching
  with `catch_all_ref`, running cleanup and rethrowing; an exception raised by the cleanup
  replaces the original. `Exn` stays inside guest registers and guest-to-guest signatures: the
  verifier rejects it in host signatures, tag payloads, struct fields, array elements and tuples
  (`VerifyError::ExceptionReference`), and entering an `Exn` signature from outside the guest
  traps with `Trap::ExceptionBoundary`. `ValueEvent::Rethrown` links a rethrow to its original
  exception, and recordings store each payload once; replay checks the link at
  `ReplayBoundary::Rethrown`. `Value` is unchanged.
  Migration: handle `ValueType::Exn`, `CatchClause::TagRef`/`AllRef`, `Catch::TagRef`/`AllRef`,
  `ValueEvent::Rethrown`, `ReplayBoundary::Rethrown`, `disasm::Operands::ThrowRef`,
  `Trap::ExceptionBoundary`, `VerifyError::ExceptionReference` and
  `RegionError::ClauseException` in exhaustive matches.

- Catch exceptions in protected regions. `Asm::begin_region`, `end_region` and `resume_region`
  attribute emitted instructions to a region tree with ordered `Catch::Tag` and `Catch::All`
  clauses targeting typed blocks; a tag clause's block receives the payload as parameters. The
  optional `regions` section stores each function's `RegionEntry` tree, `CatchClause`s and
  `RegionRun`s. The innermost catching clause, searched from the throwing activation outward,
  receives the exception; abandoned activations, guest callbacks and their pending host requests
  are released. Traps, limits and payload contract failures are never caught; uncaught exceptions
  still end as `Trap::Uncaught`. The verifier follows exceptional edges for initialization and
  refinement facts and reports `InvalidRegion` and `InvalidRegionRun`, naming the region and
  clause. `ValueEvent::Caught` reports the clause and handler, abandoned work closes with
  `TraceOutcome::Caught`, recordings retain every raised exception and replay checks each catch.
  Disassembly lists regions and their clauses.
  Migration: handle `TraceOutcome::Caught`, `ValueEvent::Caught`, `InstructionOutcome::Caught`,
  `ReplayBoundary::Caught` and the new `VerifyError` variants in exhaustive matches;
  `ReplayBoundary::Thrown` now carries the exception's index. Code constructing `Function` or
  `Program` directly must initialize `Function::regions`, `Function::region_runs`,
  `Program::regions`, `Program::catch_clauses` and `Program::region_runs`, and `AsmParts` gains
  `regions` and `region_runs`.

- Raise typed exceptions with the `throw` terminator (`Asm::throw`, or the checked
  `ProgramBuilder::emit_throw`). The verifier names the tag in `TagOutOfBounds`,
  `ThrowArityMismatch` and `ThrowPayloadMismatch`; refined payload contracts are proven statically
  where possible and otherwise checked at the throw as `ContractPosition::Payload`.
  No protected regions exist yet: every exception leaves the invocation as `Trap::Uncaught`,
  located at the `throw`, whose `UncaughtException` keeps the tag, its name and a `ValueSnapshot`
  of the payload taken before rollback. `ValueEvent::Thrown` observes the payload at the throw
  site; recording captures it and replay checks it at `ReplayBoundary::Thrown`.
  Migration: handle `Trap::Uncaught`, `ValueEvent::Thrown`, `ContractPosition::Payload`,
  `disasm::Operands::Throw`, `disasm::InputIndex::Tag`, `ReplayBoundary::Thrown` and the new
  `VerifyError` variants in exhaustive matches.

- Declare nominal exception tags with `ProgramBuilder::declare_tag`. Tags live in an optional
  `tags` program section, are identified by `TagId` and carry payload contracts; equal payloads
  never make two tags the same exception. Names are unique, appear in disassembly, and the
  verifier rejects duplicates with `DuplicateTagName` and malformed entries under
  `MetadataTable::Tags`. Migration: handle `BuildError::Tag` and `VerifyError::DuplicateTagName`
  in exhaustive matches. Code constructing `Program` directly must initialize `Program::tags`.

- Inspect cumulative invocation work with `Invocation::work`: debited fuel, admitted host calls,
  contract checks and aggregate/payload work, including pending host staging. `ExecutionWork`
  provides checked snapshot differences and sums across code domains. This is accounting, not
  a shared heap or allocation limit. Migration: no caller changes are required.

- Attribute emitted instructions directly with `Asm::source` and collect exact sidecar mappings
  through `source_mappings`. `clear_source` preserves explicit gaps. Source attribution is optional
  authoring state; bytecode, ordinary verification and execution retain no source recorder.
  Migration: no caller changes are required; existing explicit `SourceMapping` construction remains supported.

- Declare closure capture layouts with `ProgramBuilder::declare_closure`. One declaration derives
  the exact tuple environment, hidden body argument and caller-visible indirect signature;
  `ClosureLayout` loads captures and constructs matching closure values with declared register kinds.
  Layouts are authoring metadata and are not retained by prepared execution.
  Migration: handle `BuildError::Closure` in exhaustive matches. Existing raw closure assembly remains supported.

- Author calls through `ProgramBuilder::emit_call`, deriving result register kinds from one
  declared direct, host or indirect signature. `ProgramBuilder::argument` resolves declared
  argument registers. Rejected authoring requests leave instruction bytes and registers unchanged;
  ordinary verification remains authoritative for initialization and rich contracts.
  Contract and call-authoring failures convert into the ordinary `BuildError` error chain.
  Migration: handle `BuildError::Call` in exhaustive matches; existing assembler calls remain supported.

- Capture embedding guest-callback commands and actual guest returns with their source,
  ancestry, promoted host staging and shared observation order. Checked replay reexecutes
  callbacks and verifies results, including nested callbacks and rejected admissions.
  Optional instruction histories retain parent-attempt links across overlapping callbacks.

- Reenter same-program guest functions/closures through `PendingHostCall::call_guest` while
  retaining the original host operation, guest frames, heap and hard budgets. Nested callbacks
  can wait for more host work; `take_callback_results` retrieves their results before explicit
  host completion. Reentry promotes previous host staging with unchanged handles. Cancellation
  closes every pending level, and invalid callback boundaries terminate the invocation. ([#228][])

- Consume successful invocations with `into_snapshot`: owned VMs move their reachable result
  graph without payload copies; borrowed VMs preserve existing handles and copy that graph.
  `Invocation::program_owner` exposes the exact retained code domain for scoped result IDs,
  including when the original callable lookup has already been dropped.

- Construct immutable host results through `HostContext::{tuple_new,struct_new,array_new}`,
  sharing the guest's aggregate/payload work budgets while keeping borrowed inputs immutable.
  Context readers and explicit retention include staged results. Completion adopts reachable
  nodes; failures/cancellation discard staging. Recording/replay preserves constructed identities
  and spent work, including discarded allocations, without re-executing native effects.

- Store independently owned executions with `Callable::start_owned`, transferring the VM,
  exact code and inputs into the existing invocation type. Borrowed execution still reuses VM
  storage. Owned trace/access options and shared `Rc<RefCell<S>>` observers preserve captures
  and resource observations after task retirement. Limits, checked replay and cancellation
  use the same execution path; raw result handles still require explicit retention.
- Yield admitted host requests with `InvocationOptions::yield_host_calls` and
  `Invocation::pending_host_call`. Borrowed requests support external completion, asynchronous
  producers with cancellation tracking, and dispatch through the original host. Return
  validation, work/fuel budgets, source inspection and checked replay share normal semantics. ([#216][] by [@waywardmonkeys][])

- Publish compatible callable updates through `CodeSlot` while retained `CodeVersion`s and
  active invocations keep their original code. No-ops and rejected changes are explicit;
  diagnostics retain both code owners and exact signature paths. Nominal IDs from different
  preparations remain distinct. Class/selector lookup and cache invalidation stay with hosts. ([#210][] by [@waywardmonkeys][])

- Retain executable functions and escaped closures with `callable::Callable`, including their
  exact code owner and independent captures. Start through the callable and use
  `Invocation::retain_callable` to retain returned callbacks without assigning a program domain
  manually. Capture imports are transactional and share the guest's data-work budgets. ([#209][] by [@waywardmonkeys][])

- Inspect paused frames and initialized register values with invocation-scoped activation
  identities. Waiting callers expose their pending source call and exclude not-yet-returned
  results. Enumerate conservative opaque-object roots for an embedding collector; analysis
  and source-register maps remain outside ordinary preparation/execution artifacts. ([#206][] by [@waywardmonkeys][])

- Add borrow-scoped resumable execution with `Vm::start`, `InvocationOptions`,
  `Invocation::resume`, and `RunStatus`. Scheduling quanta preserve cumulative hard
  budgets and ordinary host/value/recording semantics. Dropping an unfinished
  invocation rolls back new aggregates while preserving already performed host effects. ([#205][] by [@waywardmonkeys][])

- Install verified programs in a VM with `Vm::install`, which returns the new unit's `UnitId`.
  A `FuncRef { unit, func }` names one function of one installed unit, and `Vm::units` reports
  the installed units. Every install creates a distinct unit; the VM keeps each one until it is
  dropped, and running or suspended frames keep the unit they entered. A `FuncRef` naming a unit
  the VM did not install traps with `Trap::UnknownUnit`, and starting a `Callable` in a unit
  installed from a different program traps with `Trap::UnitProgramMismatch`. `Invocation::unit`
  reports the entry unit.

- Share exception tags between units. A `TagRef` names a tag of one VM. Installing a program binds
  each tag it declares to a new `TagRef`, so two installs of one program throw distinct
  exceptions, and each tag it imports with `ProgramBuilder::import_tag` to the tag defined under
  that name with `Vm::define_tag`. Imports live in the optional `tag_imports` section (`15`) and
  `Program::tag_imports`, and resolve at install by name and canonical payload contracts;
  `InstallError::UnresolvedTag` and `InstallError::TagPayloadMismatch` reject the install without
  creating a unit or tag. Exceptions carry their `TagRef`, so a `catch tag` clause catches every unit's
  throw of a tag its program imports, and a tag it neither declares nor imports reaches only
  `catch_all` clauses. `Units::{tag, tag_name, tag_payload, defined_tag}` inspect tags.
  `ValueEvent::{Thrown, Rethrown, Caught}` and `HostCallOutcome::Threw` report the `TagRef` and
  the program-local `TagId`, if any, as `local`. Recordings keep the `TagRef`, which replay
  reproduces; `Recording::tag_name` names it.
  Migration: `UncaughtException::tag` now returns the `TagRef`, `UncaughtException::local_tag`
  returns `Option<TagId>`, and `UncaughtException::unit` is removed (use `origin().unit`). Hosts
  raising a program's tag resolve it with `HostContext::tag` or `Units::tag`. Handle `local` in
  exhaustive event patterns, and initialize `Program::tag_imports` when constructing a `Program`.

- Struct and array types are structural shapes interned in the VM heap. `AggHeap::intern_shape`,
  `AggHeap::shape`, `AggHeap::import_shape`, `AggHeap::shape_count` and `AggHeap::take_nodes`
  manage them, `AggShape` describes one, and `Recording::shape` resolves recorded shapes.
  Installing a program interns its `TypeId`s and `ElemTypeId`s, so equal layouts from independently
  compiled programs are one type and colliding local IDs no longer alias. `Units::metadata_bytes`
  reports per-unit metadata. Static verification diagnostics use the new program-local
  `AggDescriptor` through `ContractMismatch::Schema`.

- Call across installed units. Function values and closures hold a `FuncRef`, so independently
  compiled units call each other with `call.indirect`, and a pending host request can answer with
  or call back into any unit. Each activation keeps the unit it entered, so installing
  replacement code leaves suspended frames running their code; `Invocation::install` and
  `PendingHostCall::install` install while an invocation is suspended, and
  `PendingHostCall::unit` and `Invocation::units` report units. Indirect calls compare exact
  contract signatures interned per VM, and callable contracts compare canonical contracts across
  units. Source maps and recording watches use the new program-local `CodeLocation`.
  `CallableError::MixedUnits` rejects a closure capturing another unit's functions.

### Removed

- `Invocation::program_owner`. Every invocation now retains its entry unit's program; use
  `Invocation::program`, which returns that `Arc<VerifiedProgram>`.

- `PendingHostCall::return_from` and `ControlError`, along with their evidence:
  `ValueEvent::ReturnFrom`, `ControlAction::ReturnFrom`, `Divergence::ControlTarget`,
  `TraceOutcome::Unwound` and `InstructionOutcome::Unwound`. These unreleased typed exits ran
  no protected cleanup and duplicated the exception path. Migration: raise an exception instead.
  A guest uses `throw`; a host completes the pending request with `PendingHostCall::throw` (or
  returns `HostError::throw`), and the target activation catches the tag in a protected region
  and returns its payload. Cleanup on the way runs as guest code in `catch_all_ref` handlers.
  Abandoned work now closes with `TraceOutcome::Caught`.

### Changed

- `f64_to_u64` now truncates before range-checking, like every other float-to-integer
  conversion and WebAssembly `i64.trunc_f64_u`: an input between `-1.0` and `-0.0`, such as
  `-0.5`, converts to `0` instead of trapping with `IntCastOverflow`. `f32.to_u32` and
  `f64.to_u32` already behaved this way. The specification now states the rule once, under
  "Float-to-integer conversions", and the float determinism suite pins every conversion's edges.

- `RecordedValue`'s `Debug` shows a float's value beside its bits, as `F32(0.1 [0x3dcccccd])`,
  instead of the bits alone.

- `VerifyError` messages now share one location prefix, `function #F, pc P: `, which leaves the
  pc out when there is none instead of printing `pc=Some(0)` or `pc=None`. Matching on message
  text needs updating; the variants are unchanged.

- The specification now states float determinism normatively (`docs/v1_spec.md`, "Floating-point
  determinism"), replacing the note that float nondeterminism was acceptable: every `f32` and
  `f64` instruction is bit-exact on every platform, nothing is contracted into a fused
  multiply-add, and arithmetic guarantees only whether a result is NaN, while moves, bit casts,
  sign operations, values and recordings keep every bit. `f64_rem` and `f32.rem` are now
  computed exactly in the core instead of through the platform's `fmod`, so their results no
  longer depend on the C library or `compiler-builtins`. A conformance test pins the edge cases
  to literal bits on every CI platform.

- Public enums that grow as the VM gains features are now `#[non_exhaustive]`, so a new variant
  is no longer a breaking change for matches outside this crate. Closed sets an exhaustive
  match should check stay exhaustive: opcodes and operand metadata, value types and values,
  wire-tagged clauses, constants and contracts, and small fixed choices. Marked: `vm::{Trap,
  RunStatus, ObjectRootOrigin, StoreEvent}`; `trace::{TraceOutcome, HostCallOutcome,
  SuspensionAnswer, ValueEvent}`; `host::HostError`, `host_binding::RegistrationError`,
  `host_catalog::HostCatalogError`; `recording::{ActivationOutcome, InstructionOutcome,
  ControlAction, CaptureResource, CaptureFault, StoreRecord, HostOutcome, RecordedAnswer,
  Divergence, ReplayBoundary, ReplayError}`; `verifier::{VerifyError, VerifyLint,
  ExceptionPlace, RegionError, RegionRunError, BlockDeclarationError, BlockEdgeError,
  ValueFact}`; `contract::{ContractPosition, ContractMismatch, ContractBuildError,
  ContractDefinitionError}`; `asm::{AsmError, BuildError, CallBuildError, ClosureBuildError,
  TagBuildError}`; `callable::{CallableError, CodeUpdateError, SignatureDifference}`;
  `codec::{EncodeError, DecodeError}`, `format::DecodeError`, `disasm::DisasmError`,
  `source::SourceError` and `aggregates::AggError`. Migration: add a wildcard arm to matches on
  these enums, and an `else` to `let` patterns on `ControlAction`.

- Opcodes are reported by 16-bit code (`Opcode::code`) rather than lead byte, so a prefixed
  instruction is not mistaken for its prefix. `TraceSink::instr` and `InstructionStep::opcode`
  carry a `u16` code; `codec::DecodeError::UnknownOpcode`, `DisasmError::UnknownOpcode`,
  `VerifyError::InternalOpcodeSchemaMismatch` and `VerifyError::UnsupportedInstruction` carry a
  `u16` code, and `DisasmError::UnknownOpcode` adds its `pc`. Codes of single-byte opcodes equal
  their old bytes. Migration: change `opcode: u8` to `opcode: u16` in trace sinks and decode
  recorded steps with `Opcode::from_code` instead of `Opcode::from_byte`.

- A guest callback returning to the request that started it now reports
  `RunStatus::CallbackReturned` instead of `AwaitingHost`, so drivers no longer infer the return
  from `take_callback_results` being `Some`. The status describes the request: `resume` keeps
  reporting it, even after the results are taken, until the request completes or calls back
  again, and fresh requests still report `AwaitingHost`. Migration: handle
  `RunStatus::CallbackReturned` in exhaustive matches; drivers that start callbacks take the
  results there, and drivers that never call `call_guest` can treat it as unreachable.

- Execution enters an installed unit instead of a borrowed program. `Vm::run`, `Vm::run_with_ctx`
  and `Vm::start` take an entry `FuncRef` in place of the `&VerifiedProgram` and `FuncId` pair,
  and `Callable::start`/`start_owned` take the `UnitId` that runs the callable's program.
  `Invocation::program` returns `&Arc<VerifiedProgram>`, and `Invocation::retain_callable` no
  longer fails with `CallableError::MissingCodeOwner` for raw entries. Migration: install each
  program once, then run its functions by reference, for example
  `let unit = vm.install(program)?; vm.run(FuncRef::new(unit, entry), args, mask, trace)`. For a
  callable, install `Arc::clone(callable.program())` and pass that unit. Install outside hot
  loops: installing is not a cache, and each call adds a unit the VM keeps.

- Function values are unit-qualified. `Value::Func`, `ValueRef::Func` and `Closure::func` hold a
  `FuncRef` instead of a `FuncId`; `TraceLocation` and `TrapInfo` gain a `unit`; and
  `Trap::CallableSignatureMismatch`, `Trap::ContractMismatch`, `Trap::ContractWorkExceeded` and
  `ContractMismatch::Callable` name a `FuncRef`. `ValueEvent::Entry::entry` is a `FuncRef`. Trap
  display includes the unit (`trap at u0:f1 pc=4`). `SourceMapping::location`,
  `SourceError` locations and `RegisterWatch::location` are `CodeLocation`s, and
  `BoundSources::lookup`/`context` accept anything convertible to one, including a
  `TraceLocation`. Migration: build function values with `FuncRef::new(unit, func)` and read
  `function.func` where a program-local ID is needed; add `unit` to `TraceLocation` literals;
  use `CodeLocation { func, pc }` for source mappings and watches.

- Record and replay invocations that span units. At entry a recording snapshots the VM's
  installs and tag definitions (`ValueEvent::Entry::units`, `Units::log`, `StoreEvent`), and
  installs while the invocation is suspended arrive as `ValueEvent::Installed`. Replay repeats
  them in order in a fresh VM, including installs during the run at their recorded point, and
  checks that every unit and tag keeps its identity; a mismatch is `Divergence::Unit` or
  `Divergence::Tag` at `ReplayBoundary::Store`. Locations, activations (`Activation::func`),
  function values (`RecordedValue::Func` and `Closure` hold a `FuncRef`), the entry
  (`recording::Invocation::entry`), host-raised tags (`HostOutcome::Threw`) and exceptions keep
  the VM's `UnitId`s and `TagRef`s.
  `Recording::{unit, units, store, tag_name}` inspect the recorded VM, `Units::tags` and
  `PendingHostCall::units` inspect a live one, and `CaptureFault::ForeignUnit` now means a unit
  whose install the recording never observed. Register watches apply to units running the
  recorder's program. Migration: read `entry.func` and `func.func` for program-local IDs, resolve
  a location's program with `Recording::unit`, handle `ValueEvent::Installed` and `units` in
  exhaustive matches, and compare host-raised tags as `TagRef`s.

- The observatory follows execution into other units. Recording views resolve functions,
  disassembly and facts through each location's unit, and the live explorer shows the code of the
  unit a frame runs and keys breakpoints by unit. Attached source maps describe the entry unit's
  program; other units' locations say so instead of resolving through the wrong program.

- Serve several programs from one bound host. `HostBindings::bind` validates and adds another
  program's imports, so units installed from any bound program call the same `BoundHost`, and
  `HostBindings::program_count` reports them. `HostBindings::import_count` now counts the slots
  of every bound program.

- Trace hooks name the unit they observe. `TraceSink::instr`, `scope_enter` and `scope_exit`
  take a `TraceLocation` in place of the `func` and `pc` pair, `TraceSink::run_start` takes the
  entry `FuncRef`, and `ScopeKind::CallFrame::func` is a `FuncRef`; each hook's `program` is the
  program of that location's unit. Migration: replace `func: FuncId, pc: u32` hook parameters with
  `location: TraceLocation` and read `location.func`/`location.pc`, read `entry.func` and
  `func.func` where a program-local ID is needed, and keep per-program state keyed by unit.

- `AggType::Struct` and `AggType::Array` carry a heap `ShapeId` instead of a program `TypeId` or
  `ElemTypeId`, and `AggHeap::struct_new`, `AggHeap::array_new` and their `AggOverlay` forms take a
  `ShapeId`, rejecting unknown shapes with `AggError::UnknownShape` and wrong struct field counts
  with `AggError::BadArity`. Snapshots carry their shapes and `AggHeap::import` translates them.
  Recorded aggregates name recording-local shapes. Runtime contract checks compare shapes, so a
  struct with an equal layout satisfies a struct contract; verification still distinguishes
  program-local IDs. `HostContext::struct_new`/`array_new` still take the calling program's IDs.
  Migration: obtain a shape with `vm.aggregates_mut().intern_shape(AggShape::Struct(def))` (or
  `AggShape::Array(kind)`) before building aggregates directly, match `AggType` by `shape`, resolve
  shapes with `AggHeap::shape` or `Recording::shape`, and handle `AggError::UnknownShape` and
  `ContractMismatch::Schema` in exhaustive matches. Release heap nodes with `AggHeap::clear` or
  `take_nodes`; replacing a VM's heap discards the shapes its units use.

- The tape wire version is now 0.6. Published 0.1.0 reads exactly 0.5, and new opcodes and
  program sections are about to land, so the version is advanced ahead of them. The wire version
  advances once per release cycle: later format changes before the next release stay at 0.6.
  Migration: regenerate serialized programs from their producer; 0.6 readers reject 0.5 headers
  with `DecodeError::UnsupportedVersion`, and 0.1.0 readers reject 0.6 headers.

- `dec.add` and `dec.sub` accept operands with different scales: the result is exact at the
  larger scale (`1.5 + 0.25 = 1.75` at scale 2) instead of trapping with
  `DecimalScaleMismatch`. Results for equal scales are unchanged, and overflow still traps.
  Migration: programs that relied on the trap must compare scales with `dec.scale` explicitly.

- Add `RecordLimits::control_commands`, `RecordStats::control_commands`, `HostCall::sequence`,
  `InstructionStep::parent_attempt`, and structured control-command/return inspection.
  Migration: initialize the new fields in complete struct literals; handle control replay
  boundaries/divergences in exhaustive matches. Remove the temporary `UnsupportedCallback`
  capture fault: complete callback recordings now support checked replay. Instruction histories
  are ordered by admission; an outstanding host attempt can complete after its callback steps.
  No version, dependency or tape wire-format change.

- Add `ValueEvent::{CallbackEnter,CallbackReturn}` and `ObjectRootOrigin::CallbackResult`.
  Migration: handle the new enum variants in exhaustive matches. Instruction observers must
  support outstanding parent host attempts across nested guest callback attempts; callback
  return has no next guest instruction. Existing execution/recordings, crate versions and tape
  wire format are unchanged.

- Add `work` and borrowed `created` metadata to `ValueEvent::HostCall`, and `work`/`constructed`
  to recorded host observations. Migration: include the new fields in explicit event patterns
  and construction, or use `..` for unrelated inspection. Hosts without structured construction
  report zero work and an empty range. Crate versions and tape wire format are unchanged.

- Add `RunStatus::AwaitingHost` and `Trap::HostCallAbandoned`. Migration: handle the new variants
  in exhaustive matches; yielding drivers must complete or dispatch a pending request before
  resuming. Abandoning a started async producer prevents redispatch and cancels its evidence.
  Default synchronous hosts, ordinary run APIs, crate versions and the wire format are unchanged. ([#216][] by [@waywardmonkeys][])

- Record prepaid input work in `ValueEvent::Entry::work` and `recording::Invocation::work` so
  checked replay preserves owned-callable copy debits. Migration: add `work` to explicit entry
  event construction/destructuring, or use `..` when inspecting unrelated fields. Raw borrowed
  entries use `DataWork::default()`. The tape wire format and crate versions are unchanged. ([#209][] by [@waywardmonkeys][])

- Add `TraceOutcome::Cancelled` for abandoned invocations and close their outstanding
  scopes. Migration: handle the new variant in exhaustive trace-outcome matches.
  Recordings of cancelled runs report `MissingCompletion` and reject checked replay.
  Existing `run`/`run_with_ctx` signatures, normal outcomes, and wire version are unchanged. ([#205][] by [@waywardmonkeys][])

## [0.1.0][] (2026-10-27)

This release has an [MSRV][] of 1.88.

### Added

- Exercise generated typed guest programs against an independent expression evaluator in the
  conformance suite, covering nested joins, integer extrema, aggregate aliases, guest calls,
  wire roundtrips and malformed variants. Failures report deterministic seeds and reduced
  expressions; the test-only structural shrinker is checked with an injected model defect. ([#196][] by [@waywardmonkeys][])

- Bound guest tuple/struct/array construction with `Limits::max_aggregate_work`, charging one
  node plus each immediate field before allocation or payload copying. The default is 1,000,000
  units per run, shared across guest calls. `AggregateWorkExceeded` reports limit/used/requested;
  checked replay preserves this failure. This is a work budget, not a heap-residency limit.
  Migration: add `max_aggregate_work` to exhaustive `Limits` literals (or use `..Limits::default()`),
  handle the new `AggregateWorkExceeded`/`AggregateSizeOverflow` traps in exhaustive matches,
  and explicitly raise the budget for trusted workloads that construct more nodes/fields. ([#195][] by [@waywardmonkeys][])

- Seed function bodies from their declared argument contracts with `ProgramBuilder::function_asm`
  and append typed temporaries with `Asm::alloc_register`. Emitters can remove their external
  register-kind vector/allocation callback; pre-numbered frontends can transfer their existing
  ledger and append backend temporaries. Declarations remain separate from initialization. ([#190][] by [@waywardmonkeys][])

- `ExecutionContext::clear` drops transient run payloads and proof state between invocations,
  retaining reusable capacities and leaving returned values and VM aggregates valid. ([#174][] by [@waywardmonkeys][])

- Consume aggregate storage with `AggHeap::into_snapshot`, moving reachable values into owned
  storage without copying their payloads. Handles are remapped in place; unreachable nodes are
  discarded. The returned `AggRemap` transfers per-node metadata to the new handle space.
  Existing borrowed `snapshot` and host retention still copy their reachable values. ([#173][] by [@waywardmonkeys][])

- Borrow selected instruction inputs and successful local writes through `TraceMask::REGISTERS`,
  `TraceSink::wants_registers` and `registers`. Events preserve source register IDs, avoid
  copying payloads, and never expose call results at dispatch or unwritten storage. ([#165][] by [@waywardmonkeys][])

- Report the actual active event subscription through `TraceSink::run_mask` before callbacks.
  Trace-forwarding wrappers must forward it when wrapping a recorder; `TraceMask::NONE` still
  emits no callbacks. Instruction history distinguishes disabled capture from zero attempts. ([#165][] by [@waywardmonkeys][])

- Retain optional bounded instruction history with `Recorder::with_instructions`, actual
  successors, guest ancestry and explicit register watches. Replay checks retained path/value
  evidence; instruction truncation remains separate from boundary completeness. The `observations`
  benchmark separates live execution, boundary recording, history and watched operand costs. ([#166][] by [@waywardmonkeys][])

- Navigate retained instruction attempts with Observatory's `i` key. Show actual successors,
  selected observed registers, guest ancestry and separate static verifier guarantees; keep
  instruction-history completeness distinct from boundary capture. ([#166][] by [@waywardmonkeys][])

- Attach explicit frontend UTF-8 source snapshots/ranges through `SourceMap`. Full encoded
  program identity rejects stale maps; bounded context escapes terminal controls and reports
  missing mappings. Source IDs never imply filesystem access or interpret opaque span IDs. ([#167][] by [@waywardmonkeys][])

- Attach frontend snapshots through `ProgramBrowser` and Observatory's `--sources` option.
  The `source_frontend` tool demonstrates parsed emission and explicit `.tape`/`.sources` exports. ([#167][] by [@waywardmonkeys][])

- Attach source snapshots to recorded execution through `RecordingExplorer`, using the same
  validated bundle as `ProgramBrowser`. Context follows attempts and suspended callers.
  `source_frontend --record` shows watched values with source; `--explain` prints replay-checked evidence. ([#167][] by [@waywardmonkeys][])

- Function and closure value contracts preserve exact exposed signatures through aggregates,
  projections, joins and indirect calls. Concrete closure environments are checked against the
  selected body, including closures received through guest boundaries. ([#163][] by [@waywardmonkeys][])


- Observatory repeats the verification outcome in the Details title and highlights its
  verdict. Explain unknown member types (`?`) and why tuple length needs no element type,
  so a valid join is distinguishable from a rejected program. ([#161][] by [@waywardmonkeys][])

- Run `join_contracts [OUTPUT.tape]` to execute both arms of a typed join and optionally save
  its program. Observatory exposes why `tuple(i64)` and `tuple(bool)` meet at `tuple(?)`, while
  preserving the outer tuple's known length. ([#161][] by [@waywardmonkeys][])

- Measure optional inspection separately with the `inspection` benchmark: selected-function
  analysis including destruction, repeated same-PC queries, and forward source scans. Share
  the verifier benchmark fixtures so the workloads stay comparable. ([#161][] by [@waywardmonkeys][])

- Observatory shows selected-instruction before/after contracts and local definition origins,
  including aliased projections and the input arms of typed joins. Views cache optional analysis
  per selected function and disclose unavailable/unreachable facts. `inspect_prepared` accepts
  an optional source PC to print register facts and incoming parameter bindings. ([#161][] by [@waywardmonkeys][])

- Inspect opt-in verifier facts through `ProgramAnalysis`, selected `FunctionAnalysis` and
  `InstructionFacts` queries. Distinguish storage kinds, definite initialization, unreachable
  code and nested contracts; follow local origins and incoming edge bindings. Contracts share
  one analysis-local table. Normal execution artifacts retain no inspection state. Use
  `FunctionDisassembly::instruction(pc)` to follow exact source locations with indexed lookup. ([#160][] by [@waywardmonkeys][])

- Run `nested_contracts [OUTPUT.tape]` to execute and optionally save a nested-result example.
  Observatory previews nested guest signatures with bounded depth/member counts and caches the
  selected summary. `inspect_prepared` prints contract definitions once in child-before-parent
  order and maps argument/result positions to their IDs. The `contracts` benchmark measures the
  runtime cost of enforcing shared-graph contracts against kind-only signatures. ([#157][] by [@waywardmonkeys][])

- Intern and inspect compositional value contracts with `ContractTableBuilder`. Definitions
  share flat immutable storage, preserve unknown tuple members, and reject missing/forward
  references. Contract IDs belong to one table; nominal schema validation belongs to its owner. ([#155][] by [@waywardmonkeys][])

- Build and inspect shared compositional value contracts with `ContractTableBuilder` and
  `ProgramBuilder::contract`. Guest arguments/results can describe nested tuples and nominal
  structs/arrays. Local construction, projections, direct calls and typed joins preserve nested facts. ([#156][] by [@waywardmonkeys][])


- Correlate VM runs, guest calls and host work with the optional `execution_tape_footfall`
  adapter. Instruction details are opt-in; bounded captures preserve loss metadata. The
  `footfall_capture` tool saves the tape, context manifest and Footfall/Perfetto exports. ([#143][] by [@waywardmonkeys][])

- Open compiled tapes with `observatory --program PATH`, using the same browser interactively
  or with `--snapshot`. Decode errors identify the file; rejected programs remain inspectable.
  `--write-demo PATH` creates a compiled sample without overwriting an existing file. ([#151][] by [@waywardmonkeys][])

- Browse compiled programs with Observatory `browse` / `render_program`. Search functions,
  inspect imports/constants/aggregate types, follow calls and control-flow edges, and navigate
  rejected-program diagnostics without execution. Existing recording views remain available. ([#151][] by [@waywardmonkeys][])

- Query `VerifyError::function` and `pc` to locate rejected instructions or function contracts
  without parsing diagnostic text. Program-wide errors have no function location. ([#150][] by [@waywardmonkeys][])

- Draw static source control flow in the Observatory with independent colored edge lanes,
  conditional-arm labels and target arrows. Count excess edges explicitly. ([#149][] by [@waywardmonkeys][])


- Add syntax/value colors to the Observatory using an indexed 256-color palette, without
  requiring 24-bit RGB support. The demo accepts `--color=always` to explicitly override `NO_COLOR`. ([#149][] by [@waywardmonkeys][])


- Inspect recorded guest callers in the Observatory stack pane. Cycle keyboard focus with
  Tab/Shift+Tab and select a frame to move source to its captured call-site instruction.
  Show the application-to-guest invocation separately from guest frames, using captured inputs.
  The read/compute/store demo now includes a guest helper and compact call-value rows. ([#148][] by [@waywardmonkeys][])


- Capture bounded guest activation ancestry in `execution_tape::recording`, with exact caller
  PCs, shared parent links and borrowed innermost-to-root `CallStack` iteration. Inspect
  `HostCall::stack` and `Recording::outcome_stack`; trap ancestry survives VM unwinding.
  Replay checks captured activation order, caller locations and boundary ancestry. ([#148][] by [@waywardmonkeys][])

- Add full-lifecycle aggregate benchmarks covering repeated runs, temporary retention, and
  returned aggregate graphs. ([#125][] and [#128][] by [@waywardmonkeys][])

- Add a runnable block-argument comparison and Criterion benchmark in `execution_tape_wind_tunnel`.
  Compare equivalent transfer-heavy loops by cold preparation, reused execution, prepared
  storage, and frame requirements, checking output equality before timing. ([#137][] by [@waywardmonkeys][])

- Borrow invocation inputs/limits, raw host outcomes/fuel and root return candidates through
  `TraceSink::values` with `TraceMask::VALUES`. Combine with `RUN` for the final outcome;
  candidates precede validation/compaction and their handles are borrowed for the callback.
  Recording ownership and replay policy remain outside the VM. ([#144][] by [@waywardmonkeys][])

- Own invocation boundary values and reachable aggregate graphs through
  `execution_tape::recording`. Capture budgets and incomplete states are explicit;
  replay reexecutes guest code in a fresh VM with recorded host outcomes, preserving graph
  identities and typed failures without invoking the live host. ([#145][] by [@waywardmonkeys][])

- Run `record_replay` in the wind-tunnel crate to capture a shared-graph workload, drop the
  live VM/host, and check fresh replay. The `recording` benchmark measures ordinary execution,
  capture/freeze/drop and fresh replay/drop, checking results and storage counts before timing. ([#146][] by [@waywardmonkeys][])

- Declare typed control-flow joins with `Asm::block`, pass arguments with `Label::with_args`, and
  use the existing `br`/`jmp` methods. Edges transfer values simultaneously, including cycles,
  without extra guest instructions, fuel, or trace events. Inspect source arms with
  `InstrView::edge` and declaration mappings with `FunctionDisassembly::executable_pc`. ([#136][] by [@waywardmonkeys][])

- Preserve typed, owned operation errors with `HostError::custom`. Recover concrete failures
  through the standard error source chain, including after dropping the host or VM. Cloning a
  failure shares its immutable payload. ([#133][] by [@waywardmonkeys][])

- Register host callbacks with complete signatures through `HostFunctions`, validate imports with `bind`, and
  create independent host instances with `HostBindings::with_state`. Binding failures identify
  every missing/mismatched import and include expected and available complete signatures. ([#132][] by [@waywardmonkeys][])

- Expose the active program-local import through `HostContext::host_sig_id`, and embedded host
  state through `Vm::host` and `host_mut` for inspection and updates between runs. ([#131][] by [@waywardmonkeys][])

- Add owned `ValueSnapshot` retention through `AggHeap::snapshot` and `HostContext::retain`,
  with `AggHeap::import` for moving retained data into another heap. Snapshots copy only
  reachable aggregate data and preserve sharing. Host objects and function IDs keep their domains. ([#127][] by [@waywardmonkeys][])

- Inspect prepared functions through `VerifiedProgram::function` and `functions`, with borrowed
  signatures, aggregate contracts, instruction locations, register-bank requirements, and
  function-local buffer sizes. No bytecode decoding or allocation is needed. The wind-tunnel
  `inspect_prepared` tool prints a summary and optional instruction detail from an encoded file. ([#130][] by [@waywardmonkeys][])

- Explore recordings through the optional Ratatui `execution_tape_observatory` crate:
  navigate invocation/host-call/outcome boundaries, open immediate aggregate fields, inspect
  named source calls and static control-flow cues, and request checked replay. Clearly mark
  keyboard focus and distinguish sent arguments from returned values. The wind-tunnel
  `observatory` tool captures a read/compute/store workflow and supports headless text rendering. ([#147][] by [@waywardmonkeys][])

- Expose the final return/trap PC through `Recording::outcome_location`, including the VM
  location of rejected invocations. Return locations precede aggregate-handle compaction. ([#147][] by [@waywardmonkeys][])

### Changed

- Align the design and bytecode documentation with typed execution, current limits, ownership,
  recording, and authored source connections. ([#202][] by [@waywardmonkeys][])

- Move frontend source maps and structured source queries from Observatory into
  `execution_tape::source` (`no_std + alloc`, no UI dependency). Observatory retains terminal
  rendering and accepts the same core map. Migration: import source types from the core module;
  use `SourceRange::document()` / `range()` / `text()` and named `SourcePosition { line,
  byte_column }` coordinates. `context()` remains structured; terminal `context_lines()` is now
  private to Observatory. Existing source sidecar encoding and tape wire bytes are unchanged. ([#200][] by [@waywardmonkeys][])

- Prepare Rust package version 0.1.0 and matching internal dependency requirements.
  The current tape wire version remains 0.5; the package-version update does not change
  encoding or require another tape regeneration. Changes since published 0.0.1 are listed here. ([#199][] by [@waywardmonkeys][])

- Prepare the Footfall and Tracy adapters for independent publication alongside the core.
  Rename the unpublished `execution_tape_profiling` crate to `execution_tape_tracy`; update
  dependency/import paths accordingly. Observatory and development tools remain unpublished. ([#199][] by [@waywardmonkeys][])

- Remove the zero-filled runtime Unit/effect register bank and prepared Unit payload operations.
  Verification, boundary kind checks, source PCs/fuel and observations are preserved. Unit block
  transfers require no copy scratch. `FunctionStats::register_bytes` now excludes this removed
  storage; logical Unit declaration counts remain inspectable. No tape regeneration is needed. ([#194][] and [#198][] by [@waywardmonkeys][])

- Widen exact decimal mantissas from `i64` to the full signed `i128` range. Construct with
  `Decimal::new(mantissa, scale)` and read with `mantissa()` / `scale()` instead of public fields.
  `Asm::const_decimal` now accepts `i128`; widen existing `i64` variables with `i128::from`.
  `Const::Decimal` and `disasm::ConstValue::Decimal` hold a `Decimal`; packed `ConstEntry::Decimal`
  holds a range read with `Program::const_decimal`. Operand codecs use one `Operand::Decimal`
  instead of separate mantissa/scale operands. Host `ValueRef::Decimal` borrows `&Decimal`;
  copy `*value` or use `to_value()` for an owned result. Scale-sensitive equality and equal-scale
  addition/subtraction are preserved; rounding and division remain host policies. ([#192][] by [@waywardmonkeys][])

- Decimal casts check both signed and unsigned 64-bit bounds. Integer-to-decimal conversion accepts
  the full `u64` range where the scaled mantissa fits, and zero at every `u8` scale. ([#192][] by [@waywardmonkeys][])

- Wire version 0.5 encodes decimal literal mantissas as signed 128-bit LEB128 (at most 19 bytes).
  Earlier versions are rejected; regenerate serialized programs and source sidecars together. ([#192][] by [@waywardmonkeys][])


- Bound logical byte/string work per invocation with `Limits::max_payload_work` (64 MiB by
  default). Copying, comparisons, conversions, direct host payload admission, and owned result
  materialization charge their documented work before performing it. Add the field to explicit
  `Limits` literals (or use `..Limits::default()`), raise it for trusted large workloads, and
  handle `Trap::PayloadWorkExceeded { limit, used, requested }` and `PayloadSizeOverflow` in
  exhaustive trap matches. This is a work budget, not a heap or process-memory limit.
  ([#185][] by [@waywardmonkeys][])

- Check kind-only value contracts directly without traversal frames or successful-proof entries.
  Preserve the same work charge, type/handle errors and opaque callable semantics. ([#168][] by [@waywardmonkeys][])
- Refresh the workspace lockfile with Rust 1.88-compatible dependencies. Developer benchmarks
  use Criterion 0.8.2 and the Tracy adapter uses `tracy-client` 0.19; benchmarks use the standard
  library's `black_box`. ([#179][] by [@waywardmonkeys][])

- Recording is now the regular `execution_tape::recording` module. Remove the separate
  `execution_tape_recording` dependency and update imports to `execution_tape::recording`.
  No feature flag or extra dependency is required; capture remains opt-in at runtime. ([#178][] by [@waywardmonkeys][])

- Wire version 0.4 encodes indirect signatures with shared compositional contract IDs; regenerate
  encoded programs. Replace `call_sig(args, rets)` with fallible `call_sig(FunctionSig { ... })`
  for kind-only signatures or `call_sig(FunctionAbi { ... })` for refinements. The argument/result
  kind accessors return borrowed `ContractKinds` views; contract accessors expose the same IDs.
  Indirect target equivalence is exact, including nested contracts; it does not implement variance
  or subtyping. Coarse signatures do not match richer declarations merely because their ABI kinds
  agree. Known incompatible targets now fail verification; opaque mismatches trap before dispatch.
  `closure_new` now requires an environment-bearing target and validates its environment before
  producing a closure. Signature mismatches precede selected-callee dispatch; validation can still
  fail after earlier host work, whose external effects are not rolled back.
  Handle the new callable mismatch diagnostics and `ContractPath::{Member, Environment}` path steps.
  Contract-work limits additionally count callable signature positions and environment edges. ([#163][] by [@waywardmonkeys][])


- Contract failures display readable expected/found types and indexed argument/result paths.
  Structured error fields remain unchanged; use them rather than parsing `Display` text. ([#159][] by [@waywardmonkeys][])

- Guest signatures now store authoritative compositional contract IDs in wire version 0.3;
  regenerate encoded programs. `FunctionSig` remains the kind-only authoring shorthand.
  Replace `FunctionAbi::{sig, arg_agg_shapes}` with `args`/`rets` lists of `ContractRef`, and
  replace argument-shape helpers with shared contracts. `AggShape`, its argument-only metadata,
  and `AggHeap::check_shape` are removed. Host and indirect-call signatures retain their ABI kinds. ([#156][] by [@waywardmonkeys][])

- Handle `ProgramBuilder::declare_function`'s `Result`; missing contract IDs fail before mutation.
  Function argument/result kind accessors now return borrowed `ContractKinds` views rather than
  slices; use `iter()` or query `arg_contracts`/`ret_contracts` and the program's contract table. ([#156][] by [@waywardmonkeys][])

- Add `Limits::max_contract_work` to exhaustive initializers (or use `..Default::default()`).
  Aggregate boundary validation is iterative, shares successful proofs within a run, and counts
  value visits and child edges separately from instruction fuel. Result obligations are checked
  before caller installation or root commit. `Trap::ContractMismatch` replaces
  `ArgumentShapeMismatch`; handle `ContractWorkExceeded` and the corresponding verifier contract
  diagnostics in exhaustive matches. Failures include the boundary position and nested member path. ([#156][] by [@waywardmonkeys][])


- `Recorder::mask` now includes `CALL`. Add `call_frames` to exhaustive `RecordLimits`
  initializers (or use `..Default::default()`); this budget counts all entered activations.
  Explicit masks that omit `CALL` retain value-only capture with unavailable stacks. Update
  exhaustive `CaptureResource` and `Divergence` matches for the new stack cases. ([#148][] by [@waywardmonkeys][])

- Include the actual caller instruction in `ScopeKind::CallFrame::caller`, with `None` for
  the root activation. Entry metadata is preserved on normal and trapping exits. Update
  exhaustive patterns to bind `caller` or use `..`, and supply it when constructing this variant. ([#148][] by [@waywardmonkeys][])

- Declare every function register by transferring its kind `Vec` to `Asm::set_register_types`,
  including r0 (`Unit`), arguments and temporaries. Raw `FunctionDef` inputs now own `register_types` in place of `reg_count`.
  Preserve the complete `AsmParts` when assembling raw definitions. `Asm::block` now takes only
  register IDs; parameter kinds come from the function ledger. Programs use wire version 0.2;
  regenerate serialized programs with the updated producer. Declarations do not initialize values
  or establish aggregate shapes. ([#141][] by [@waywardmonkeys][])

- Replace `VerifyError::{TypeMismatch, UnknownTypeAtUse, UnknownTypeAtSelect, UnstableRegType,
  UnstableRegTypeAt}` with `MissingRegisterTypes`, `RegisterTypeMismatch` and
  `UnknownAggregateShape`. A register mismatch's `pc: None` identifies the r0/argument prefix;
  instruction failures carry their source PC. Static kind and call/return contracts are checked
  even in unreachable code. `Program::encode` now panics on manually malformed register
  declarations; verify manually edited containers before encoding them. ([#141][] by [@waywardmonkeys][])

- Run the disassembler example with `cargo run -p execution_tape_wind_tunnel --bin disasm`.
  Examples now live in the separate tool crate, keeping the core crate focused on the library. ([#138][] by [@waywardmonkeys][])

- The draft bytecode adds `Block`, `JmpArgs`, and `BrArgs`; older readers reject these opcodes.
  Existing label-only branches remain supported. Exhaustive opcode, operand codec/schema,
  disassembly operand, and `VerifyError` matches must handle the new variants. Block failures
  report declaration or source-edge context, including arm, target, and argument details.
  A block reserved with `Asm::block` may be placed once; failed repeated placement preserves
  its existing bytes and name. ([#136][] by [@waywardmonkeys][])

- Share allocation-free register operand iteration between verification and disassembly.
  Opcode generation now supports multiple register lists, list-only outputs, and additional
  scalar operands without adding iterator constructors. ([#135][] by [@waywardmonkeys][])

- `HostError`, `Trap`, and `TrapInfo` no longer implement `PartialEq`/`Eq`, because arbitrary
  host errors need not support value equality. Match variants and compare diagnostic fields or
  downcast concrete error sources instead. Exhaustive `HostError` matches must handle `Custom`. ([#133][] by [@waywardmonkeys][])

- Exhaustive `HostError` matches must handle `ProgramMismatch`: prepared host instances reject
  calls from another program, including equal cloned programs, before invoking a callback. ([#132][] by [@waywardmonkeys][])

- VM runs now retain only new aggregates reachable from returned values; traps discard all new
  aggregates. Previously retained values stay valid until the heap is cleared. Treat host
  aggregate and closure-environment arguments as borrowed for the call; use `HostContext::retain`
  for durable host state, then `AggHeap::import` before using it in a VM. Returning a dangling
  aggregate or closure environment now traps at `ret`. ([#129][] by [@waywardmonkeys][])

- `AggHeap::tuple_new`, `struct_new`, and `array_new` now return `Result<AggHandle, AggError>`.
  Handle the result when constructing aggregates. Heap and staged constructors reject dangling
  aggregate children and closure environments before mutation, preventing forward references
  and cycles. Schema validation remains a separate operation. ([#126][] by [@waywardmonkeys][])

- Verification now validates immutable metadata, including unused constant ranges, symbol and
  field-name ranges, struct layouts, and optional names. Malformed programs fail preparation
  with `VerifyError::InvalidMetadata` instead of trapping during execution. Exhaustive error
  matches must handle the new variant; `MetadataTable` identifies the failing table. ([#124][] by [@waywardmonkeys][])

- Borrow immutable byte/string payloads from constants, entry arguments, and aggregate fields
  during execution. Move host-owned return payloads into run storage. Owned outputs and
  persistent aggregate contents retain their existing lifetime and representation. ([#120][] by [@waywardmonkeys][])
- `Vm::run_with_ctx` now takes a mutable output `Vec` and returns `Result<(), TrapInfo>`.
  Pass an empty or cleared buffer to replace results, or retain contents to append. Traps
  preserve existing contents. `run` remains the allocating convenience entry point. ([#119][] by [@waywardmonkeys][])
- Define `TraceSink::instr` as an instruction attempt; `next_pc` is its lexical end.
  Use the new `instr_end` callback for success/trap and the actual control-flow successor.
  Add a `TraceOutcome` argument to `scope_exit` implementations; scopes now close on traps
  even without run tracing, and host success includes validating returned values. ([#118][] by [@waywardmonkeys][])
- Improve crate and README documentation, keeping examples synchronized. ([#93][] by [@no-materials][])
- Reduce verifier allocations by reusing dataflow buffers and storing aggregate metadata sparsely.
  Verification only computes liveness-based diagnostics through the `_with_lints` entry points. ([#109][] by [@waywardmonkeys][])
- Decode bytecode and span tables directly into packed arenas, removing temporary per-function
  allocations and repacking copies. ([#111][] by [@waywardmonkeys][])
- Release virtual-register mappings after lowering to reduce retained execution storage. ([#112][] by [@waywardmonkeys][])

### Fixed

- Reject raw empty function bodies with `MissingTerminator` during verification and preparation,
  matching the executable-body requirement instead of deferring failure to VM instruction fetch. ([#164][] by [@waywardmonkeys][])

- Observatory distinguishes fixed register storage kinds from nested call-result contracts and
  required return contracts. Rejected contracts name the function and explain the member path,
  including named struct fields, without exposing Rust debug syntax. ([#159][] by [@waywardmonkeys][])
- Omit Footfall scope-token fields when a VM scope began while capture was disabled.
  Enabling capture mid-run can report its outcome without inventing an observed scope. ([#154][] by [@waywardmonkeys][])

- Use published Footfall 0.2. Scopes admitted before a capture-level reduction still close,
  keeping VM scope observations balanced across level changes. ([#154][] by [@waywardmonkeys][])

- Skip default Footfall VM callbacks when the current capture level disables scopes. Recompute
  `FootfallSink::mask()` before each run when changing levels; explicit masks remain supported,
  and scopes opened before a mid-run level drop still close normally. ([#154][] by [@waywardmonkeys][])

- Discard obsolete aggregate shape facts when calls or nested projections overwrite a register.
  Aggregate selects retain only the shape shared by both inputs, including aliased destinations. ([#134][] by [@waywardmonkeys][])

- Preserve load-time traps for malformed byte/string constant ranges, even when the loaded
  value is unused. ([#123][] by [@waywardmonkeys][])

- Validate aggregate argument contracts before entry and direct, indirect, and closure calls.
  Exhaustive matches on `Trap` must handle `ArgumentShapeMismatch`; `AggHeap::check_shape`
  exposes allocation-free validation with structured `AggShapeError` diagnostics. ([#117][] by [@waywardmonkeys][])
- Reject direct calls whose known aggregate argument shapes contradict the callee contract.
  Exhaustive matches on `VerifyError` must handle `CallArgShapeMismatch`. ([#116][] by [@waywardmonkeys][])
- Reject impossible collection counts in program sections and instruction register lists before
  reserving storage. ([#105][] by [@waywardmonkeys][])
- Apply the call-depth limit to the entry frame, including a zero-depth limit. ([#106][] by [@waywardmonkeys][])
- Trap on `f64` conversions at the exclusive `i64` and `u64` upper bounds. ([#106][] by [@waywardmonkeys][])
- Preserve known aggregate shapes across jumps, branch joins, and loop backedges. ([#107][] by [@waywardmonkeys][])
- Report incorrect entry argument counts as `Trap::ArityMismatch` rather than `Trap::InvalidPc`. ([#113][] by [@waywardmonkeys][])

## [0.0.1][] (2026-05-31)

This release has an [MSRV][] of 1.88.

This is the initial release of Execution Tape, a `no_std` bytecode container, verifier, register VM
runtime, host-call ABI, tracing API, and disassembler.

[@no-materials]: https://github.com/no-materials
[@waywardmonkeys]: https://github.com/waywardmonkeys

[#124]: https://github.com/forest-rs/execution/pull/124
[#125]: https://github.com/forest-rs/execution/pull/125
[#126]: https://github.com/forest-rs/execution/pull/126
[#127]: https://github.com/forest-rs/execution/pull/127
[#128]: https://github.com/forest-rs/execution/pull/128
[#129]: https://github.com/forest-rs/execution/pull/129
[#130]: https://github.com/forest-rs/execution/pull/130
[#131]: https://github.com/forest-rs/execution/pull/131
[#132]: https://github.com/forest-rs/execution/pull/132
[#133]: https://github.com/forest-rs/execution/pull/133
[#141]: https://github.com/forest-rs/execution/pull/141

[#134]: https://github.com/forest-rs/execution/pull/134
[#135]: https://github.com/forest-rs/execution/pull/135
[#136]: https://github.com/forest-rs/execution/pull/136
[#137]: https://github.com/forest-rs/execution/pull/137
[#138]: https://github.com/forest-rs/execution/pull/138

[#93]: https://github.com/forest-rs/execution/pull/93
[#105]: https://github.com/forest-rs/execution/pull/105
[#106]: https://github.com/forest-rs/execution/pull/106
[#107]: https://github.com/forest-rs/execution/pull/107
[#109]: https://github.com/forest-rs/execution/pull/109
[#111]: https://github.com/forest-rs/execution/pull/111
[#112]: https://github.com/forest-rs/execution/pull/112
[#113]: https://github.com/forest-rs/execution/pull/113

[#116]: https://github.com/forest-rs/execution/pull/116
[#117]: https://github.com/forest-rs/execution/pull/117
[#118]: https://github.com/forest-rs/execution/pull/118
[#119]: https://github.com/forest-rs/execution/pull/119
[#120]: https://github.com/forest-rs/execution/pull/120

[#123]: https://github.com/forest-rs/execution/pull/123

[Unreleased]: https://github.com/forest-rs/execution/compare/execution_tape-v0.1.0...HEAD
[0.1.0]: https://github.com/forest-rs/execution/compare/execution_tape-v0.0.1...execution_tape-v0.1.0
[0.0.1]: https://github.com/forest-rs/execution/releases/tag/execution_tape-v0.0.1

[#144]: https://github.com/forest-rs/execution/pull/144

[#145]: https://github.com/forest-rs/execution/pull/145

[#146]: https://github.com/forest-rs/execution/pull/146

[#147]: https://github.com/forest-rs/execution/pull/147

[#148]: https://github.com/forest-rs/execution/pull/148

[#149]: https://github.com/forest-rs/execution/pull/149

[#150]: https://github.com/forest-rs/execution/pull/150

[#163]: https://github.com/forest-rs/execution/pull/163
[#161]: https://github.com/forest-rs/execution/pull/161
[#160]: https://github.com/forest-rs/execution/pull/160
[#159]: https://github.com/forest-rs/execution/pull/159
[#157]: https://github.com/forest-rs/execution/pull/157
[#156]: https://github.com/forest-rs/execution/pull/156
[#155]: https://github.com/forest-rs/execution/pull/155
[#151]: https://github.com/forest-rs/execution/pull/151

[#143]: https://github.com/forest-rs/execution/pull/143

[#154]: https://github.com/forest-rs/execution/pull/154

[MSRV]: README.md#minimum-supported-rust-version-msrv

[#164]: https://github.com/forest-rs/execution/pull/164

[#165]: https://github.com/forest-rs/execution/pull/165

[#166]: https://github.com/forest-rs/execution/pull/166

[#167]: https://github.com/forest-rs/execution/pull/167
[#168]: https://github.com/forest-rs/execution/pull/168

[#173]: https://github.com/forest-rs/execution/pull/173

[#174]: https://github.com/forest-rs/execution/pull/174

[#178]: https://github.com/forest-rs/execution/pull/178

[#179]: https://github.com/forest-rs/execution/pull/179

[#185]: https://github.com/forest-rs/execution/pull/185

[#190]: https://github.com/forest-rs/execution/pull/190

[#192]: https://github.com/forest-rs/execution/pull/192

[#198]: https://github.com/forest-rs/execution/pull/198

[#200]: https://github.com/forest-rs/execution/pull/200

[#199]: https://github.com/forest-rs/execution/pull/199

[#194]: https://github.com/forest-rs/execution/pull/194
[#195]: https://github.com/forest-rs/execution/pull/195
[#196]: https://github.com/forest-rs/execution/pull/196
[#202]: https://github.com/forest-rs/execution/pull/202
[#205]: https://github.com/forest-rs/execution/pull/205
[#206]: https://github.com/forest-rs/execution/pull/206

[#209]: https://github.com/forest-rs/execution/pull/209
[#210]: https://github.com/forest-rs/execution/pull/210
[#216]: https://github.com/forest-rs/execution/pull/216

[#228]: https://github.com/forest-rs/execution/pull/228
