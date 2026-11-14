// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Borrow-scoped handling of one suspension that no guest handler handled.

use super::*;
use crate::host::HostThrow;
use crate::program::TagId;
use crate::trace::SuspensionAnswer;
use crate::typed::VRegSlice;
use crate::value::TagRef;

/// A `suspend` yielded to the embedding, decoded once when it was yielded.
///
/// Holds no borrows, so a view keeps a copy beside its exclusive invocation borrow and borrows
/// code-owned slices on demand.
#[derive(Clone, Copy, Debug)]
pub(in crate::vm) struct PendingSuspend {
    pub(in crate::vm) location: TraceLocation,
    pub(in crate::vm) span: Option<u64>,
    /// The suspended tag, local to the program of `location`.
    pub(in crate::vm) tag: TagId,
    pub(in crate::vm) payload: VRegSlice,
    pub(in crate::vm) rets: VRegSlice,
    /// Depth of the suspending frame, the top of the waiting stack.
    pub(in crate::vm) depth: usize,
    /// An answer started but did not finish, for example because an observer panicked.
    pub(in crate::vm) abandoned: bool,
}

/// How the embedding answers a yielded suspension.
enum Answer {
    Resume(Vec<Value>),
    Throw(TagRef, Vec<Value>),
    Discard,
}

/// Exclusive view of one yielded suspension, borrowed from its invocation.
///
/// An invocation started with [`InvocationOptions::yield_suspensions`] yields a `suspend` that
/// no guest `on` clause handles: [`Invocation::resume`] reports [`RunStatus::AwaitingHandler`]
/// and [`Invocation::pending_suspension`] returns this view. The embedding is the handler of
/// last resort. It reads the tag and payload, and may inspect the suspended frames, use its
/// host, or install code. It then answers exactly once:
///
/// - [`Self::resume`] continues after the `suspend` with values for the tag's results;
/// - [`Self::resume_throw`] raises an exception at the `suspend`, where the guest's protected
///   regions can catch it;
/// - [`Self::discard`] abandons the suspension and ends the invocation with
///   [`Trap::UnhandledSuspension`], releasing its frames.
///
/// Each answer consumes the view and executes no guest instruction; the next
/// [`Invocation::resume`] continues. Dropping the view without answering leaves the suspension
/// pending. Dropping the invocation cancels it and releases its frames, as for any invocation.
/// If an observer panics while an answer is delivered, the invocation cannot continue: it
/// reports [`Trap::HostCallAbandoned`] on resumption, or cancels when dropped.
///
/// The view borrows its invocation exclusively, so a suspension can be answered only while it
/// is pending and only once: there is no handle to keep, reuse or confuse with another one.
/// Its frames stay on the invocation's stack while it waits and count against
/// [`Limits::max_call_depth`] as before. Their trace scopes closed with
/// [`TraceOutcome::Suspended`] when it was yielded and re-enter when it is resumed.
///
/// Answer values follow the ordinary host ABI: scalars, and handles that already belong to this
/// invocation (for example from its payload or entry arguments), as for
/// [`PendingHostCall::complete`].
pub struct PendingSuspension<'request, 'vm, H: Host> {
    running: Running<'request, 'vm, H>,
    pending: PendingSuspend,
}

impl<H: Host> fmt::Debug for PendingSuspension<'_, '_, H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingSuspension")
            .field("location", &self.location())
            .field("tag", &self.tag())
            .finish_non_exhaustive()
    }
}

impl<H: Host> PendingSuspension<'_, '_, H> {
    fn program(&self) -> &VerifiedProgram {
        &self
            .running
            .vm
            .units
            .unit(self.pending.location.unit)
            .program
    }

    /// The `suspend` instruction that was yielded.
    #[must_use]
    pub fn location(&self) -> TraceLocation {
        self.pending.location
    }

    /// The suspended VM tag.
    #[must_use]
    pub fn tag(&self) -> TagRef {
        self.running
            .vm
            .units
            .unit(self.pending.location.unit)
            .tag(self.pending.tag)
            .expect("verified tag")
    }

    /// The tag's ID in the program of [`Self::location`]'s unit.
    #[must_use]
    pub fn local_tag(&self) -> TagId {
        self.pending.tag
    }

    /// The suspended tag's declared name.
    #[must_use]
    pub fn tag_name(&self) -> &str {
        self.running
            .vm
            .units()
            .tag_name(self.tag())
            .expect("issued tag")
    }

    /// Storage kinds of the payload.
    #[must_use]
    pub fn payload_types(&self) -> &[ValueType] {
        self.running
            .vm
            .units()
            .tag_payload(self.tag())
            .expect("issued tag")
    }

    /// Storage kinds of the values that resume the suspension: the tag's results.
    #[must_use]
    pub fn result_types(&self) -> &[ValueType] {
        self.running
            .vm
            .units()
            .tag_results(self.tag())
            .expect("issued tag")
    }

    /// Borrows the payload in tag order, without copying byte/string contents.
    ///
    /// Aggregate, closure and function handles among them belong to this invocation and stay
    /// valid until it commits its root or rolls back, so they may be passed back in an answer.
    pub fn payload(&self) -> impl ExactSizeIterator<Item = ValueRef<'_>> {
        let continuation = &*self.running.continuation;
        let body = self
            .program()
            .verified(self.pending.location.func)
            .expect("suspended function");
        read_registers(
            &continuation.ctx,
            ValueSources::new(
                &self.running.vm.units,
                &continuation.args,
                self.running.vm.memory.aggs(),
            ),
            &continuation.ctx.frames[self.pending.depth - 1].base,
            body.vregs(self.pending.payload),
        )
    }

    /// Borrows the heap that gives the payload's aggregate handles their meaning.
    #[must_use]
    pub fn aggregates(&self) -> &AggHeap {
        self.running.vm.memory.aggs()
    }

    /// Inspects the units installed in this invocation's VM.
    #[must_use]
    pub fn units(&self) -> Units<'_> {
        self.running.vm.units()
    }

    /// Installs `program` in this invocation's VM while the suspension waits.
    ///
    /// The suspended frames keep their units. Answer with a [`FuncRef`] of the new unit, or
    /// pass one later, to run its code in this invocation.
    pub fn install(
        &mut self,
        program: impl Into<Arc<VerifiedProgram>>,
    ) -> Result<UnitId, InstallError> {
        install_observed(self.running.vm, self.running.continuation, program.into())
    }

    /// Borrows host state for read-only inspection while the suspension waits.
    #[must_use]
    pub fn host(&self) -> &H {
        &self.running.vm.host
    }

    /// Mutates host state while the suspension waits, without releasing it.
    ///
    /// Preserve the validity of object handles the guest still holds.
    pub fn host_mut(&mut self) -> &mut H {
        &mut self.running.vm.host
    }

    /// Borrows the suspended frames and their initialized registers.
    ///
    /// The suspending frame waits at its `suspend`, like a frame waiting at a host call; its
    /// results are not yet initialized. Activation identities are those the frames had before
    /// the suspension and keep after it is resumed.
    #[must_use]
    pub fn inspect(&self) -> InvocationInspection<'_> {
        self.running.inspect()
    }

    /// Resumes the suspension: the `suspend`'s results receive `values`.
    ///
    /// The values must match the tag's [`result kinds`](Self::result_types) and its refined
    /// result contracts in the suspending program; their byte and string contents are charged
    /// as payload work. The raw values are observed first. A mismatch ends the invocation with a
    /// trap at the `suspend` and rolls back its aggregates, as an invalid host return does: a
    /// wrong count with [`Trap::ArityMismatch`], a wrong kind with [`Trap::TypeMismatch`], and a
    /// violated contract with [`Trap::ContractMismatch`].
    /// Executes no guest instruction; [`Invocation::resume`] continues after the `suspend`.
    pub fn resume(mut self, values: Vec<Value>) -> Result<(), TrapInfo> {
        self.running.answer(self.pending, Answer::Resume(values))
    }

    /// Resumes the suspension by raising `tag` with `payload` at the `suspend`.
    ///
    /// Protected regions around the `suspend` can catch the exception; one nothing catches ends
    /// the invocation with [`Trap::Uncaught`]. The tag, payload kinds and refined payload
    /// contracts are checked as for [`PendingHostCall::throw`], through the suspending unit's
    /// binding of the tag or else the unit that declared it.
    pub fn resume_throw(mut self, tag: TagRef, payload: Vec<Value>) -> Result<(), TrapInfo> {
        self.running
            .answer(self.pending, Answer::Throw(tag, payload))
    }

    /// Abandons the suspension without resuming it.
    ///
    /// The invocation ends with [`Trap::UnhandledSuspension`] at the `suspend`, the outcome it
    /// would have had without [`InvocationOptions::yield_suspensions`]: its frames are released
    /// and its new aggregates rolled back. No guest code runs, so the suspended activations'
    /// protected regions do not; use [`Self::resume_throw`] to unwind through them instead.
    /// Returns that trap, which the invocation also reports from now on.
    pub fn discard(mut self) -> TrapInfo {
        self.running
            .answer(self.pending, Answer::Discard)
            .expect_err("a discarded suspension ends the invocation")
    }
}

impl<'vm, H: Host> Invocation<'vm, H> {
    /// Borrows the suspension this invocation yielded, if one is waiting.
    ///
    /// Only invocations started with [`InvocationOptions::yield_suspensions`] yield suspensions.
    /// Repeated queries and dropped views neither execute guest code nor answer it.
    pub fn pending_suspension(&mut self) -> Option<PendingSuspension<'_, 'vm, H>> {
        if self.continuation.outcome.is_some() {
            return None;
        }
        let pending = self
            .continuation
            .control
            .pending_suspension
            .filter(|pending| !pending.abandoned)?;
        Some(PendingSuspension {
            running: self.running(),
            pending,
        })
    }
}

impl<H: Host> Running<'_, '_, H> {
    /// Delivers the embedding's answer to `pending`, then finishes the invocation on a trap.
    ///
    /// The suspension stays recorded as abandoned until delivery returns, so an observer that
    /// panics part way leaves the invocation terminal instead of resumable after a `suspend`
    /// whose results were never written.
    fn answer(&mut self, pending: PendingSuspend, answer: Answer) -> Result<(), TrapInfo> {
        self.continuation.control.pending_suspension = Some(PendingSuspend {
            abandoned: true,
            ..pending
        });
        let result = if self.continuation.trace.active() {
            self.deliver::<WithTracePolicy>(pending, answer)
        } else {
            self.deliver::<NoTracePolicy>(pending, answer)
        };
        self.continuation.control.pending_suspension = None;
        if let Err(error) = &result {
            self.finish(Err(error.clone()));
        }
        result
    }

    fn deliver<P: TracePolicy>(
        &mut self,
        pending: PendingSuspend,
        answer: Answer,
    ) -> Result<(), TrapInfo> {
        let vm = &*self.vm;
        let location = pending.location;
        let unit = vm.units.unit(location.unit);
        let tag = unit.tag(pending.tag).expect("verified tag");
        let continuation = &mut *self.continuation;
        let program = unit.program.program();
        P::values(&mut continuation.trace, program, || {
            ValueEvent::SuspensionAnswered {
                location,
                tag,
                answer: match &answer {
                    Answer::Resume(values) => SuspensionAnswer::Resumed { values },
                    Answer::Throw(tag, payload) => SuspensionAnswer::Threw {
                        tag: *tag,
                        local: unit.local_tag(*tag),
                        payload,
                    },
                    Answer::Discard => SuspensionAnswer::Discarded,
                },
                aggregates: vm.memory.aggs(),
            }
        });
        let ctx = &mut continuation.ctx;
        let fail = |ctx: &Registers, trap| ctx.trap(location.func, location.pc, pending.span, trap);
        match answer {
            Answer::Discard => Err(fail(ctx, Trap::UnhandledSuspension { tag })),
            Answer::Resume(values) => {
                vm.reenter_scopes(ctx, &mut continuation.trace, 0, None);
                vm.admit_resumption(ctx, self.budget, pending, values)
                    .map_err(|trap| fail(ctx, trap))
            }
            Answer::Throw(tag, payload) => {
                vm.reenter_scopes(ctx, &mut continuation.trace, 0, None);
                vm.raise_host::<P>(
                    ctx,
                    self.budget,
                    &unit.program,
                    HostThrow { tag, payload },
                    &mut continuation.trace,
                    &mut continuation.control,
                    location,
                    pending.span,
                )
            }
        }
    }
}

impl<H: Host> Vm<H> {
    /// Writes the values resuming `pending` to its `suspend`'s results, checking them first
    /// for count (`ArityMismatch`), kind and payload work, then against the tag's refined result
    /// contracts.
    fn admit_resumption(
        &self,
        ctx: &mut Registers,
        budget: &mut Budget,
        pending: PendingSuspend,
        mut values: Vec<Value>,
    ) -> Result<(), Trap> {
        let location = pending.location;
        let unit = self.units.unit(location.unit);
        let tag = self.units.tag(unit.tag(pending.tag).expect("verified tag"));
        let kinds = tag.expect("issued tag").result_kinds();
        if values.len() != kinds.len() {
            return Err(Trap::ArityMismatch);
        }
        let mut bytes = 0_usize;
        for (value, &kind) in values.iter().zip(kinds) {
            value.check_type(kind)?;
            bytes = bytes
                .checked_add(match value {
                    Value::Bytes(bytes) => bytes.len(),
                    Value::Str(text) => text.len(),
                    _ => 0,
                })
                .ok_or(Trap::PayloadSizeOverflow)?;
        }
        budget.charge_payload_bytes(bytes, 1)?;
        let body = unit
            .program
            .verified(location.func)
            .expect("suspended function");
        let base = ctx.frames[pending.depth - 1].base;
        let destinations = body.vregs(pending.rets);
        for (&destination, value) in destinations.iter().zip(&mut values) {
            ctx.write_owned_value(base, destination, core::mem::replace(value, Value::Unit))?;
        }
        let program = unit.program.program();
        let entry = program.tag(pending.tag).expect("verified tag");
        let contracts = program
            .tag_result_contracts(entry)
            .expect("verified tag results");
        for (index, (&register, &contract)) in destinations.iter().zip(contracts).enumerate() {
            if matches!(program.contracts.get(contract), Some(Contract::Kind(_))) {
                continue;
            }
            let value = Self::contract_value(ctx, base, register);
            budget
                .contract_scratch
                .check_value(
                    &self.units,
                    location.unit,
                    self.memory.aggs(),
                    &value,
                    contract,
                    self.limits.max_contract_work,
                )
                .map_err(|error| {
                    let position = ContractPosition::Resumption {
                        tag: pending.tag,
                        index: u32::try_from(index).expect("verified result arity"),
                    };
                    self.contract_trap(FuncRef::new(location.unit, location.func), position, error)
                })?;
        }
        Ok(())
    }
}
