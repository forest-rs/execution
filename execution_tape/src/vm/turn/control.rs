// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Native effects use the same saved stacks and response protocol as guest clauses.

use super::*;
use crate::control::{ContractOwner, ControlBoundary, ControlTag};
use crate::vm::budget::DataBudget;
use invocation::delimited::{Saved, attach_trace};

pub(in crate::vm) struct EffectRequest {
    pub(in crate::vm) tag: ControlTag,
    pub(in crate::vm) request: Vec<Value>,
    pub(in crate::vm) location: TraceLocation,
}
#[derive(Debug)]
pub(super) struct EffectState {
    pub(super) tag: ControlTag,
    pub(super) request: Vec<Value>,
    pub(super) location: TraceLocation,
    pub(super) instance: Instance,
    pub(super) continuation: ContinuationRef,
    pub(super) admitted: bool,
    span: Option<u64>,
}

/// An escaped typed operation and the one-use authority to supply its response.
///
/// The request registration and continuation belong to the originating VM. Dropping this
/// object releases the request registration but does not implicitly discard the saved stack;
/// resume or explicitly discard the continuation. Exporting a snapshot does not export control.
#[derive(Clone, Debug)]
pub struct Effect {
    /// Exact operation identity; matching signatures alone do not make two tags interchangeable.
    pub tag: ControlTag,
    /// Actual suspension source in `instance`.
    pub location: TraceLocation,
    /// Exact code and nominal namespace that produced the request.
    pub instance: Instance,
    /// Durable request values, resolved through the originating VM.
    pub request: RootedValues,
    /// One-use response capability. Its parameters are the operation's response contracts;
    /// its results are the suspended root's eventual result contracts.
    pub continuation: ContinuationRef,
}

/// Borrowed native effect evidence before turn publication.
pub struct PendingEffect<'a> {
    state: &'a EffectState,
    heap: &'a AggHeap,
    results: &'a mut crate::host::HostResults,
    budget: &'a mut DataBudget,
}
impl fmt::Debug for PendingEffect<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingEffect")
            .field("tag", &self.state.tag)
            .field("location", &self.state.location)
            .field("continuation", &self.state.continuation)
            .finish()
    }
}
impl<'a> PendingEffect<'a> {
    /// Reads and constructs response values in the operation's exact namespace, spending this
    /// turn's allowance. Use `values.arguments(...)` to preserve nominal provenance.
    pub fn values(&mut self) -> crate::host::ValueContext<'_, '_> {
        crate::host::ValueContext::new(
            self.state.tag.instance(),
            self.heap,
            self.results,
            self.budget,
        )
    }

    /// Exact operation requested by the guest.
    #[must_use]
    pub fn tag(&self) -> &'a ControlTag {
        &self.state.tag
    }
    /// Actual source instruction and its exact code namespace.
    #[must_use]
    pub fn location(&self) -> TraceLocation {
        self.state.location
    }
    /// Code interpreting the request's nominal values and source location.
    #[must_use]
    pub fn instance(&self) -> &'a Instance {
        &self.state.instance
    }
    /// Request values borrowed for this turn only.
    #[must_use]
    pub fn request(&self) -> &'a [Value] {
        &self.state.request
    }
    /// Aggregate storage borrowed with the request.
    #[must_use]
    pub fn aggregates(&self) -> &'a AggHeap {
        self.heap
    }
    /// Clone this capability, release the view, and use [`Turn::resume_continuation`] to respond
    /// during this turn. Finishing instead publishes the same capability and durable request.
    #[must_use]
    pub fn continuation(&self) -> &'a ContinuationRef {
        &self.state.continuation
    }
}

impl<'a, H: Host> Turn<'a, H> {
    /// Inspects a typed operation that escaped all guest-installed dynamic handlers.
    #[must_use]
    pub fn pending_effect(&mut self) -> Option<PendingEffect<'_>> {
        Some(PendingEffect {
            state: self.effect.as_ref().filter(|effect| effect.admitted)?,
            heap: self.vm.memory.aggs(),
            results: &mut self.native_results,
            budget: &mut self.budget.data,
        })
    }

    pub(super) fn effect_interrupted(&self) -> TrapInfo {
        let effect = self.effect.as_ref().expect("interrupted effect");
        TrapInfo {
            func: effect.location.func,
            pc: effect.location.pc,
            span_id: effect.span,
            trap: Trap::GuestTransferAbandoned,
        }
    }

    pub(super) fn capture_effect(&mut self) -> Result<(), TrapInfo> {
        let chain = self.active.as_ref().expect("suspending root");
        let stack = &chain.continuation;
        let location = stack
            .native_effect
            .as_ref()
            .expect("escaped operation")
            .location;
        let span = stack
            .ctx
            .span_at(stack.program.program(), location.func, location.pc);
        let trap = |error| {
            stack.ctx.trap(
                location.func,
                location.pc,
                stack
                    .ctx
                    .span_at(stack.program.program(), location.func, location.pc),
                error,
            )
        };
        if self.domain.saved.len() >= self.vm.limits.max_continuations {
            return Err(trap(Trap::ContinuationLimitExceeded));
        }
        let copied = chain
            .parents
            .iter()
            .chain(core::iter::once(stack))
            .try_fold(0_usize, |total, stack| {
                total
                    .checked_add(borrowed_payload(&stack.args)?)
                    .ok_or(Trap::PayloadSizeOverflow)
            })
            .map_err(trap)?;
        self.budget
            .data
            .charge_payload_bytes(copied, 1)
            .map_err(trap)?;
        let root = chain.parents.first().unwrap_or(stack);
        let results = ContractOwner {
            program: (*root.program).clone(),
            range: root.program.program().functions[root.entry.0 as usize].ret_types,
        };
        let instance = stack
            .ctx
            .instance
            .as_ref()
            .expect("suspender instance")
            .clone();
        let mut chain = self.active.take().expect("installed chain");
        let request = chain
            .continuation
            .native_effect
            .take()
            .expect("escaped operation");
        let declaration = request
            .tag
            .instance()
            .program()
            .program()
            .call_sig(request.tag.signature_id())
            .expect("verified operation");
        let parameters = ContractOwner {
            program: request.tag.instance().program().clone(),
            range: declaration.rets,
        };
        let mut stacks = chain.parents;
        stacks.push(chain.continuation);
        stacks[0].parent = None;
        let saved = Saved::Suspended(stacks.into_iter().map(Continuation::into_owned).collect());
        let key = match self.domain.saved.insert(
            &instance.0.store,
            saved,
            self.vm.limits.max_continuations,
            self.epoch,
        ) {
            Ok(key) => key,
            Err((_, Saved::Suspended(mut stacks))) => {
                let continuation = stacks.pop().expect("suspending leaf");
                let error = continuation.ctx.trap(
                    location.func,
                    location.pc,
                    None,
                    Trap::ContinuationLimitExceeded,
                );
                self.active = Some(Chain {
                    continuation,
                    parents: stacks,
                });
                return Err(error);
            }
            Err(_) => unreachable!("inserted suspended chain"),
        };
        let continuation = ContinuationRef::new(key, parameters, results);
        self.effect = Some(EffectState {
            tag: request.tag,
            request: request.request,
            location,
            instance,
            continuation,
            admitted: false,
            span,
        });
        // Authority and storage are installed before an observer can interrupt parking.
        let Saved::Suspended(stacks) = self
            .domain
            .saved
            .get_mut(
                self.effect
                    .as_ref()
                    .expect("installed effect")
                    .continuation
                    .id(),
            )
            .expect("installed suffix")
        else {
            unreachable!()
        };
        for stack in stacks.iter_mut().rev() {
            stack
                .trace
                .observe(&mut self.observers)
                .stack_activity(stack.program.program(), crate::trace::StackActivity::Parked);
        }
        self.effect.as_mut().expect("installed effect").admitted = true;
        Ok(())
    }

    /// Consumes one suspension and supplies its typed response in this VM.
    ///
    /// The VM must be idle or waiting for this same native effect. Stale or foreign capabilities
    /// are denied without spending work or changing valid control. Once admitted, authority is
    /// consumed before input work or observers run; failure never restores the predecessor.
    /// Work limits belong to this turn, while every saved frame remains charged to residency.
    pub fn resume_continuation(
        &mut self,
        reference: &ContinuationRef,
        arguments: impl Into<Arguments<'a>>,
    ) -> Result<(), ControlError> {
        if self.active.is_some()
            || self
                .effect
                .as_ref()
                .is_some_and(|effect| effect.continuation.id() != reference.id())
        {
            return Err(ControlError::Busy);
        }
        let arguments = arguments.into();
        arguments
            .validate(self.vm)
            .map_err(ControlError::Retention)?;
        self.domain
            .saved
            .get(reference.id())
            .map_err(|error| ControlError::Resumption(error.into()))?;
        let saved = self
            .domain
            .saved
            .take(reference.id())
            .expect("exclusive checked authority");
        self.effect = None;
        let (chain, environment, fresh) = match saved {
            Saved::Suspended(mut stacks) => {
                let continuation = stacks.pop().expect("saved leaf");
                (
                    Chain {
                        continuation,
                        parents: stacks,
                    },
                    None,
                    false,
                )
            }
            Saved::New {
                callable,
                mut control,
            } => {
                let (function, environment) = match callable {
                    Value::Func(function) => (function, None),
                    Value::Closure(closure) => (closure.func, Some(closure.env)),
                    _ => unreachable!("admitted callable"),
                };
                control.yield_host_calls = self.yield_host_calls;
                let stack = Continuation::new(
                    CodeOwner::Instance(function.instance().clone()),
                    function.id(),
                    Cow::Owned(Vec::new()),
                    self.mask,
                    control,
                );
                (
                    Chain {
                        continuation: stack,
                        parents: Vec::new(),
                    },
                    environment,
                    true,
                )
            }
        };
        self.active = Some(chain);
        let result = self.admit_response(reference, arguments, environment, fresh);
        if let Err(error) = result {
            if let Some(mut running) = self.running() {
                running.finish(Err(error.clone()));
            }
            return Err(ControlError::Trapped(error));
        }
        Ok(())
    }

    fn admit_response(
        &mut self,
        reference: &ContinuationRef,
        arguments: Arguments<'a>,
        environment: Option<AggHandle>,
        fresh: bool,
    ) -> Result<(), TrapInfo> {
        let chain = self.active.as_mut().expect("installed response");
        let stack = &mut chain.continuation;
        let location = stack.control.waiting.site().map_or(
            TraceLocation {
                func: stack.entry,
                pc: 0,
            },
            |(location, _)| location,
        );
        stack.control.waiting.poison();
        self.budget.select(&stack.control);
        let outcome = (|| -> Result<(), Trap> {
            if self.budget.fuel == 0 {
                return Err(Trap::FuelExceeded);
            }
            self.budget.fuel -= 1;
            self.native_results
                .promote(self.vm.memory.aggs_mut())
                .map_err(Trap::AggError)?;
            let (arguments, source) =
                arguments
                    .materialize(self.vm, &mut self.budget)
                    .map_err(|error| match error {
                        ControlError::Input(trap) => trap,
                        _ => Trap::ForeignStoreValue {
                            kind: ValueType::Agg,
                        },
                    })?;
            let owner = reference.parameter_owner();
            let contracts = owner.program.program().verified_contracts(owner.range);
            if contracts.len() != arguments.len() {
                return Err(Trap::ControlArityMismatch {
                    boundary: ControlBoundary::Resume,
                    expected: contracts.len(),
                    actual: arguments.len(),
                });
            }
            for (index, (&contract, value)) in contracts.iter().zip(arguments.iter()).enumerate() {
                self.budget
                    .contract_scratch
                    .check_value(
                        owner.program.program(),
                        self.vm.memory.aggs(),
                        value,
                        contract,
                        self.vm.limits.max_contract_work,
                    )
                    .map_err(|error| {
                        self.vm
                            .contract_trap(
                                location.func,
                                ContractPosition::Control {
                                    boundary: ControlBoundary::Resume,
                                    index: Some(
                                        u32::try_from(index).expect("verified signature arity"),
                                    ),
                                },
                                error,
                            )
                            .in_contract_definition(&owner.program)
                    })?;
            }
            let target = stack.program.instance().expect("qualified saved code");
            invocation::composition::check_transfer(
                self.vm.memory.aggs(),
                &arguments,
                source.as_ref().unwrap_or(target),
                target,
                &mut self.budget,
                &self.vm.limits,
                stack.entry,
                true,
            )?;
            let bytes = arguments.iter().try_fold(0_usize, |total, value| {
                let bytes = match value {
                    Value::Str(text) => text.len(),
                    Value::Bytes(bytes) => bytes.len(),
                    _ => 0,
                };
                total.checked_add(bytes).ok_or(Trap::PayloadSizeOverflow)
            })?;
            self.budget.data.charge_payload_bytes(bytes, 1)?;
            if fresh {
                let mut arguments = arguments.into_owned();
                if let Some(environment) = environment {
                    arguments.insert(0, Value::Agg(environment));
                }
                stack.args = Cow::Owned(arguments);
            } else {
                for parent in &mut chain.parents {
                    attach_trace(parent, &mut self.observers);
                }
                attach_trace(stack, &mut self.observers);
                Running::<H>::complete_control(
                    stack,
                    arguments.into_owned(),
                    self.vm.memory.aggs(),
                    &mut self.observers,
                )?;
            }
            Ok(())
        })();
        if let Err(trap) = outcome {
            let stack = &self
                .active
                .as_ref()
                .expect("admitted response")
                .continuation;
            return Err(stack.ctx.trap(
                location.func,
                location.pc,
                stack
                    .ctx
                    .span_at(stack.program.program(), location.func, location.pc),
                trap,
            ));
        }
        if fresh {
            self.active
                .as_mut()
                .expect("new computation")
                .continuation
                .initialize(
                    self.vm,
                    &mut self.budget,
                    DataWork::default(),
                    &mut self.observers,
                )?;
        }
        Ok(())
    }

    /// Consumes a saved capability and releases its frames without executing guest cleanup.
    /// Denied stale or foreign capabilities leave all other state untouched.
    pub fn discard_continuation(
        &mut self,
        reference: &ContinuationRef,
    ) -> Result<(), ControlError> {
        let saved = self
            .domain
            .saved
            .take(reference.id())
            .map_err(|error| ControlError::Resumption(error.into()))?;
        self.budget.frames -= saved.frames();
        if self
            .effect
            .as_ref()
            .is_some_and(|effect| effect.continuation.id() == reference.id())
        {
            self.effect = None;
        }
        saved.cancel(&mut self.observers);
        Ok(())
    }
}
