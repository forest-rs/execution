// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! One exclusive, bounded execution lease over VM-owned control and values.

use super::*;
use alloc::borrow::Cow;
use continuation::{CodeOwner, Continuation};
use domain::{Chain, Domain};
use invocation::{Running, finish_control_trace};
mod control;
mod observation;
pub(super) use control::EffectRequest;
use control::EffectState;
pub use control::{Effect, PendingEffect};

/// Published progress after releasing a VM's execution lease.
///
/// A paused computation and its pending host admission stay in the VM. Returned values use
/// durable registrations, so subsequent collection cannot silently change their meaning.
#[derive(Debug)]
pub enum TurnOutcome {
    /// A typed effect escaped guest handlers. The request and one-use response capability
    /// belong to this VM and can outlive this turn.
    Effect(Effect),
    /// No computation is active. Saved computations may still exist.
    Idle,
    /// The VM owns an active computation stopped at an instruction boundary.
    Paused,
    /// The VM owns this exact admitted host instruction awaiting completion.
    AwaitingHost(HostRequestId),
    /// The root returned successfully and its values are registered in the VM.
    Returned(RootedValues),
}

impl TurnOutcome {
    /// Takes a successful return, preserving any unexpected boundary and its capabilities.
    /// Use an exhaustive match when the embedding handles host requests or language effects.
    pub fn into_returned(self) -> Result<RootedValues, Self> {
        match self {
            Self::Returned(values) => Ok(values),
            other => Err(other),
        }
    }
}

/// Exclusive permission to advance a VM's computations for one bounded turn.
///
/// Work limits reset when a turn opens; retained frames and continuation quotas do not.
/// Driving observes progress. Only [`Self::finish`] publishes it. Dropping without finishing
/// aborts the acquired active chain and continuations created during this turn, rolls back
/// new aggregates, and preserves untouched older saved computations. Consumed capabilities
/// remain consumed. Host effects are outside this publication boundary.
///
/// Arguments and observers can be borrowed for this lease. Finishing paused work owns its
/// inputs before releasing those borrows; already-owned payloads move without copying.
#[must_use = "finish the turn to publish progress; dropping it aborts active work"]
pub struct Turn<'a, H: Host> {
    vm: &'a mut Vm<H>,
    domain: Domain,
    effect: Option<EffectState>,
    native_results: crate::host::HostResults,
    active: Option<Chain<'a>>,
    observers: Observers<'a>,
    mask: TraceMask,
    yield_host_calls: bool,
    budget: Budget,
    checkpoint: Checkpoint,
    epoch: u64,
    published: bool,
}

impl<H: Host> fmt::Debug for Turn<'_, H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Turn")
            .field("epoch", &self.epoch)
            .field("active", &self.active.is_some())
            .field("saved_computations", &self.domain.saved.len())
            .field("remaining_fuel", &self.budget.fuel)
            .finish_non_exhaustive()
    }
}

impl<H: Host> Vm<H> {
    /// Acquires paused work, if any, without implicitly starting or replacing a computation.
    ///
    /// The lease is exclusive. Its borrowed observers expire at publication or cancellation;
    /// durable control stays in the VM. An identity exhaustion denial changes no control.
    pub fn turn<'a>(
        &'a mut self,
        mut options: TurnOptions<'a>,
    ) -> Result<Turn<'a, H>, ControlError> {
        let available = self.domain.as_ref().ok_or(ControlError::StoreLeased)?;
        let next = available
            .next_epoch
            .checked_add(1)
            .ok_or(ControlError::IdentityExhausted)?;
        let checkpoint = self.memory.checkpoint();
        let mut budget = Budget::new(&self.limits);
        budget.frames = available.frames;
        let mut domain = self.domain.take().expect("available domain");
        domain.next_epoch = next;
        let active = domain.paused.take();
        let observers = Observers::take(&mut options);
        let mask = if observers.trace.is_some() {
            options.mask.unwrap_or(TraceMask::NONE)
        } else {
            TraceMask::NONE
        };
        let mut turn = Turn {
            vm: self,
            domain,
            active,
            effect: None,
            native_results: crate::host::HostResults::default(),
            observers,
            mask,
            yield_host_calls: options.yield_host_calls,
            budget,
            checkpoint,
            epoch: next,
            published: false,
        };
        observation::join_domain(
            &mut turn.domain,
            &mut turn.observers,
            mask,
            turn.yield_host_calls,
        );
        if let Some(chain) = &mut turn.active {
            for stack in chain
                .parents
                .iter_mut()
                .chain(core::iter::once(&mut chain.continuation))
            {
                observation::join(
                    stack,
                    &mut turn.observers,
                    mask,
                    false,
                    turn.yield_host_calls,
                );
            }
        }
        Ok(turn)
    }
}

impl<'a, H: Host> Turn<'a, H> {
    /// Starts a qualified function or closure only when no computation is active.
    ///
    /// Foreign-store and non-callable denials preserve existing state. Raw arguments use the
    /// callee's nominal interpretation; registered arguments retain their original owner and
    /// are checked when crossing definitions. Closure arguments exclude the hidden environment.
    pub fn start<'callee>(
        &mut self,
        callee: impl Into<Callee<'callee>>,
        arguments: impl Into<Arguments<'a>>,
    ) -> Result<(), ControlError> {
        if self.active.is_some() || self.effect.is_some() {
            return Err(ControlError::Busy);
        }
        let arguments = arguments.into();
        arguments
            .validate(self.vm)
            .map_err(ControlError::Retention)?;
        let before = self.budget.data.used;
        let (function, environment) = callee.into().prepare(self.vm, &mut self.budget)?;
        let (mut arguments, source) = arguments.materialize(self.vm, &mut self.budget)?;
        if let Some(source) = source {
            invocation::composition::check_transfer(
                self.vm.memory.aggs(),
                &arguments,
                &source,
                function.instance(),
                &mut self.budget,
                &self.vm.limits,
                function.id(),
                true,
            )
            .map_err(ControlError::Input)?;
        }
        if let Some(environment) = environment {
            self.budget
                .data
                .charge_payload_bytes(
                    borrowed_payload(&arguments).map_err(ControlError::Input)?,
                    1,
                )
                .map_err(ControlError::Input)?;
            arguments.to_mut().insert(0, Value::Agg(environment));
        }
        let mut control = ExecutionControl::start(self.yield_host_calls);
        control.in_store(function.instance());
        control.segment = self.domain.next_segment;
        self.domain.next_segment = self
            .domain
            .next_segment
            .checked_add(1)
            .ok_or(ControlError::IdentityExhausted)?;
        let work = DataWork {
            aggregate: self.budget.data.used.aggregate - before.aggregate,
            payload: self.budget.data.used.payload - before.payload,
        };
        let stack = Continuation::new(
            CodeOwner::Instance(function.instance().clone()),
            function.id(),
            arguments,
            self.mask,
            control,
        );
        self.active = Some(Chain {
            continuation: stack,
            parents: Vec::new(),
        });
        let result = self
            .active
            .as_mut()
            .expect("installed entry")
            .continuation
            .initialize_prepaid(self.vm, &mut self.budget, work, &mut self.observers);
        if let Err(error) = result {
            self.running()
                .expect("installed entry")
                .finish(Err(error.clone()));
            return Err(ControlError::Trapped(error));
        }
        Ok(())
    }

    fn running(&mut self) -> Option<Running<'_, 'a, H>> {
        let chain = self.active.as_mut()?;
        self.budget.select(&chain.continuation.control);
        Some(Running {
            vm: self.vm,
            continuation: &mut chain.continuation,
            parents: &mut chain.parents,
            observers: &mut self.observers,
            checkpoint: self.checkpoint,
            epoch: self.epoch,
            defer_publication: true,
            budget: &mut self.budget,
            next_segment: &mut self.domain.next_segment,
            resumptions: &mut self.domain.saved,
        })
    }

    /// Admits at most `quantum` instructions. A successful return is still private until finish.
    /// Calling with zero observes the current stop reason without executing guest instructions.
    pub fn drive(&mut self, quantum: u64) -> Result<RunStatus, TrapInfo> {
        self.drive_mode::<true>(quantum)
    }

    pub(in crate::vm) fn yields_host_calls(&self) -> bool {
        self.yield_host_calls
    }

    pub(in crate::vm) fn drive_mode<const COOPERATIVE: bool>(
        &mut self,
        quantum: u64,
    ) -> Result<RunStatus, TrapInfo> {
        if let Some(effect) = &self.effect {
            if !effect.admitted {
                return Err(self.effect_interrupted());
            }
            return Ok(RunStatus::AwaitingEffect);
        }
        let Some(mut running) = self.running() else {
            return Ok(RunStatus::Complete);
        };
        let status = running.drive::<COOPERATIVE>(quantum)?;
        if status == RunStatus::AwaitingEffect
            && let Err(error) = self.capture_effect()
        {
            if let Some(mut running) = self.running() {
                running.finish(Err(error.clone()));
            }
            return Err(error);
        }
        Ok(status)
    }

    /// Borrows the exact host admission identified by an earlier published outcome.
    /// A delayed or foreign response is denied before mutation, staging, or guest work.
    pub fn pending_host_call(
        &mut self,
        expected: &HostRequestId,
    ) -> Result<PendingHostCall<'_, 'a, H>, ControlError> {
        let chain = self.active.as_ref().ok_or(ControlError::NoHostRequest)?;
        let pending = chain
            .continuation
            .control
            .waiting
            .host()
            .filter(|pending| !pending.abandoned)
            .ok_or(ControlError::NoHostRequest)?;
        if chain.continuation.control.host_request_id(pending) != *expected {
            return Err(ControlError::HostRequestMismatch);
        }
        self.running()
            .expect("checked active chain")
            .pending_host_call()
            .ok_or(ControlError::NoHostRequest)
    }

    /// Current host admission without borrowing its arguments or allocating a response buffer.
    #[must_use]
    pub fn host_request_id(&self) -> Option<HostRequestId> {
        let stack = &self.active.as_ref()?.continuation;
        let pending = stack
            .control
            .waiting
            .host()
            .filter(|pending| !pending.abandoned)?;
        Some(stack.control.host_request_id(pending))
    }

    /// Inspects active and saved computations with the same vocabulary as [`Vm::inspect`].
    /// Register analysis is built only when queried; parked continuations remain visible even
    /// when a native effect has no active chain.
    #[must_use]
    pub fn inspect(&self) -> ExecutionInspection<'_> {
        inspection::execution_inspection(
            self.active
                .as_ref()
                .map(|chain| (&chain.continuation, chain.parents.as_slice())),
            &self.domain.saved,
            self.vm.memory.aggs(),
            &self.vm.roots,
            self.effect.as_ref().map(|effect| effect.request.as_slice()),
        )
    }

    /// Borrows host state between operations. Guest storage cannot be moved through this view.
    #[must_use]
    pub fn host(&self) -> &H {
        &self.vm.host
    }

    /// Mutates host state between operations; external effects do not roll back with the turn.
    pub fn host_mut(&mut self) -> &mut H {
        &mut self.vm.host
    }

    /// Cumulative admitted work in this turn, including all resumed and newly entered stacks.
    #[must_use]
    pub fn work(&self) -> ExecutionWork {
        ExecutionWork {
            fuel: self.vm.limits.fuel - self.budget.fuel,
            host_calls: self.budget.host_calls,
            contract: self.budget.contract_scratch.work_used(),
            data: self.budget.data.used,
        }
    }

    /// Publishes progress and releases the VM. Publication failure aborts this turn.
    /// In particular, a root-group quota denial is a fatal resource limit: completed private
    /// work is discarded and cannot be retried. External host effects remain performed.
    ///
    /// All retained control and registered values participate in suffix reclamation. Existing
    /// heap addresses outside the suffix stay stable; full moving collection remains explicit.
    pub fn finish(mut self) -> Result<TurnOutcome, ControlError> {
        if let Some(chain) = &self.active {
            if let Some(Err(error)) = &chain.continuation.outcome {
                return Err(ControlError::Trapped(error.clone()));
            }

            for stack in chain
                .parents
                .iter()
                .chain(core::iter::once(&chain.continuation))
            {
                let interrupted = if let Some(pending) = stack
                    .control
                    .waiting
                    .host()
                    .filter(|pending| pending.abandoned)
                {
                    Some((pending.location, Trap::HostCallAbandoned))
                } else {
                    stack
                        .control
                        .waiting
                        .interrupted()
                        .map(|location| (location, Trap::GuestTransferAbandoned))
                };
                if let Some((location, trap)) = interrupted {
                    let error = stack.ctx.trap(
                        location.func,
                        location.pc,
                        stack
                            .ctx
                            .span_at(stack.program.program(), location.func, location.pc),
                        trap,
                    );
                    return Err(ControlError::Trapped(error));
                }
            }
        }
        if self.effect.as_ref().is_some_and(|effect| !effect.admitted) {
            return Err(ControlError::Trapped(self.effect_interrupted()));
        }
        if let Some(effect) = &self.effect {
            self.vm
                .validate_retention(&effect.instance, &effect.request)
                .map_err(ControlError::Retention)?;
        }
        self.native_results
            .promote(self.vm.memory.aggs_mut())
            .map_err(|error| ControlError::Input(Trap::AggError(error)))?;
        let returned = self
            .active
            .as_ref()
            .and_then(|chain| chain.continuation.pending_return);
        if returned.is_some() {
            let chain = self.active.as_mut().expect("returned chain");
            let owner = chain
                .continuation
                .ctx
                .instance
                .as_ref()
                .expect("qualified entry");
            self.vm
                .validate_retention(owner, &chain.continuation.outputs)
                .map_err(ControlError::Retention)?;
            self.budget.frames -= chain.continuation.ctx.frames.len();
            let owner = chain.continuation.ctx.instance.take();
            chain.continuation.ctx.clear();
            chain.continuation.ctx.instance = owner;
            chain.continuation.args = Cow::Owned(Vec::new());
        }
        // No staging delta with pre-compaction base indices may cross publication.
        if let Some(chain) = &mut self.active {
            for stack in chain
                .parents
                .iter_mut()
                .chain(core::iter::once(&mut chain.continuation))
            {
                if let Some(results) = &mut stack.host_results {
                    results
                        .promote(self.vm.memory.aggs_mut())
                        .map_err(|error| ControlError::Input(Trap::AggError(error)))?;
                }
            }
        }
        let copied = self
            .active
            .as_ref()
            .map_or(Ok(0_usize), |chain| {
                chain
                    .parents
                    .iter()
                    .chain(core::iter::once(&chain.continuation))
                    .try_fold(0_usize, |total, stack| {
                        total
                            .checked_add(borrowed_payload(&stack.args)?)
                            .ok_or(Trap::PayloadSizeOverflow)
                    })
            })
            .map_err(ControlError::Input)?;
        self.budget
            .data
            .charge_payload_bytes(copied, 1)
            .map_err(ControlError::Input)?;
        self.domain.paused = self.active.take().map(Chain::into_owned);
        self.budget.contract_scratch.clear_proofs();
        let mut handles = Vec::new();
        self.vm.roots.handles(&mut handles);
        if let Some(effect) = &self.effect {
            handles.extend(
                effect
                    .request
                    .iter()
                    .filter_map(crate::aggregates::aggregate_handle)
                    .map(Value::Agg),
            );
        }
        let plan = self
            .domain
            .plan_collection(self.vm.memory.aggs(), &mut handles)
            .map_err(|error| ControlError::Input(Trap::AggError(error)))?;
        let remap = self
            .vm
            .memory
            .aggs_mut()
            .compact_since(self.checkpoint.index(), &mut handles, true)
            .map_err(|error| ControlError::Input(Trap::AggError(error)))?
            .expect("requested remap");
        self.vm
            .roots
            .remap(&remap)
            .expect("planned registered roots");
        self.domain.apply_collection(plan, &remap);
        if let Some(effect) = &mut self.effect {
            remap
                .remap_values_in_place(&mut effect.request)
                .expect("planned effect request");
        }
        self.domain.frames = self.budget.frames;
        let mut completed = None;
        let outcome = if let Some(effect) = self.effect.take() {
            let request = self
                .vm
                .retain_validated(effect.instance.clone(), effect.request);
            TurnOutcome::Effect(Effect {
                tag: effect.tag,
                location: effect.location,
                instance: effect.instance,
                request,
                continuation: effect.continuation,
            })
        } else if returned.is_some() {
            let chain = self.domain.paused.take().expect("returned chain");
            let mut stack = chain.continuation;
            let instance = stack.ctx.instance.take().expect("qualified result owner");
            let roots = self
                .vm
                .retain_validated(instance, core::mem::take(&mut stack.outputs));
            stack.outcome = Some(Ok(()));
            completed = Some(stack);
            TurnOutcome::Returned(roots)
        } else if let Some(chain) = &self.domain.paused {
            match chain.continuation.control.waiting.host() {
                Some(pending) => {
                    TurnOutcome::AwaitingHost(chain.continuation.control.host_request_id(pending))
                }
                None => TurnOutcome::Paused,
            }
        } else {
            TurnOutcome::Idle
        };
        // Publication is irrevocable before arbitrary observers can unwind.
        self.vm.domain = Some(core::mem::take(&mut self.domain));
        self.published = true;
        if let (Some(mut stack), Some(site)) = (completed, returned) {
            stack
                .trace
                .observe(&mut self.observers)
                .call_scope_exit_if_enabled(
                    stack.program.program(),
                    1,
                    site.location.func,
                    site.location.pc,
                    site.span,
                );
            stack.trace.observe(&mut self.observers).instr_end(
                stack.program.program(),
                None,
                TraceOutcome::Ok,
            );
            finish_control_trace(&mut stack, TraceOutcome::Ok, &mut self.observers);
        }
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests;

impl<H: Host> Drop for Turn<'_, H> {
    fn drop(&mut self) {
        if self.published {
            observation::release_domain(
                self.vm.domain.as_mut().expect("published domain"),
                &mut self.observers,
            );
            return;
        }
        let active = self.active.take().or_else(|| self.domain.paused.take());
        let retired: Vec<_> = self.domain.saved.retire_epoch(self.epoch).collect();
        self.vm.memory.rollback(self.checkpoint);
        self.domain.frames = self
            .domain
            .saved
            .iter()
            .map(|(_, saved)| saved.frames())
            .sum();
        self.vm.domain = Some(core::mem::take(&mut self.domain));
        self.published = true;
        observation::release_domain(
            self.vm.domain.as_mut().expect("restored domain"),
            &mut self.observers,
        );
        // Retire authority and roll back storage before cancellation can call consumer code.
        if let Some(mut chain) = active {
            for stack in
                core::iter::once(&mut chain.continuation).chain(chain.parents.iter_mut().rev())
            {
                if stack.outcome.is_none() {
                    finish_control_trace(stack, TraceOutcome::Cancelled, &mut self.observers);
                }
            }
        }
        for saved in retired {
            saved.cancel(&mut self.observers);
        }
    }
}

#[expect(
    clippy::ptr_arg,
    reason = "ownership status determines whether publication copies; a slice loses it"
)]
fn borrowed_payload(values: &Cow<'_, [Value]>) -> Result<usize, Trap> {
    if matches!(values, Cow::Owned(_)) {
        return Ok(0);
    }
    values.iter().try_fold(0_usize, |total, value| {
        let bytes = match value {
            Value::Str(text) => text.len(),
            Value::Bytes(bytes) => bytes.len(),
            _ => 0,
        };
        total.checked_add(bytes).ok_or(Trap::PayloadSizeOverflow)
    })
}
