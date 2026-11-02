// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Activation-scoped typed returns. Condition selection and restart policy stay above the VM.

use super::*;
use crate::trace::ControlEvent;

/// A denied control target or a terminal failure after a control operation was admitted.
#[derive(Clone, Debug)]
pub enum ControlError {
    /// The code instance belongs to another VM store. The pending request remains usable.
    ForeignStore,
    /// The target belongs to another execution. The pending request remains usable.
    ForeignInvocation,
    /// The exact target activation has already left. The pending request remains usable.
    ExpiredActivation,
    /// Admitted result validation or hard resource accounting terminated the invocation.
    Trapped(TrapInfo),
}
impl fmt::Display for ControlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ForeignStore => f.write_str("code instance belongs to another VM store"),
            Self::ForeignInvocation => f.write_str("control target belongs to another invocation"),
            Self::ExpiredActivation => f.write_str("control target activation has already left"),
            Self::Trapped(trap) => fmt::Display::fmt(trap, f),
        }
    }
}
impl core::error::Error for ControlError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Trapped(trap) => Some(trap),
            _ => None,
        }
    }
}

impl<H: Host> PendingHostCall<'_, '_, H> {
    /// Returns typed values from one exact live guest activation, abandoning its active callees,
    /// including independently compiled segments in this execution's suspended parent chain.
    ///
    /// The target comes from this invocation's paused-frame inspection. Recursive activations
    /// of the same function have distinct targets. Foreign/expired targets are recoverable
    /// denials: they spend no guest work and leave this request pending. Once admitted, bad
    /// result arity, kinds, rich contracts or hard limits terminate the invocation.
    ///
    /// Values use the original code/heap. Prior staging is promoted with stable handles, one
    /// hard-fuel unit is spent, and immediate payloads are charged as host ingress. No budgets
    /// reset. The target's declared function result contracts apply, independently of this host
    /// signature. A root target commits root outputs; a callback-root target restores its
    /// original host request with callback results; another target resumes its guest caller.
    ///
    /// Abandoned host instructions do not complete successfully or dispatch their hosts. Trace
    /// scopes/attempts close as `Unwound`, at the real initiating source. This primitive does not
    /// run protected cleanup; language cleanup support must be layered explicitly. Dropping an
    /// invocation remains hard cancellation and never runs guest code. Boundary recording retains
    /// admitted commands; checked replay resolves the actual active target and reissues the
    /// typed transfer. Pure foreign/expired denials do not change guest execution and are not
    /// recorded as admitted commands.
    pub fn return_from(
        mut self,
        target: &ActivationId,
        values: Vec<Value>,
    ) -> Result<(), ControlError> {
        let local = self
            .running
            .continuation
            .control
            .target_index(&self.running.continuation.ctx, target);
        let ancestor = self.running.parents.as_ref().and_then(|parents| {
            parents
                .iter()
                .enumerate()
                .rev()
                .find_map(|(segment, parent)| {
                    match parent.control.target_index(&parent.ctx, target) {
                        Err(ControlError::ForeignInvocation) => None,
                        result => Some((segment, result)),
                    }
                })
        });
        let destination = match local {
            Ok(index) => (None, index),
            Err(ControlError::ExpiredActivation) => return Err(ControlError::ExpiredActivation),
            Err(_) => match ancestor {
                Some((segment, result)) => (Some(segment), result?),
                None if self
                    .running
                    .continuation
                    .control
                    .belongs_to_execution(target) =>
                {
                    return Err(ControlError::ExpiredActivation);
                }
                None => return Err(ControlError::ForeignInvocation),
            },
        };
        self.delivering = true;
        let result = match destination {
            (None, index) => self.running.return_from(index, target.ordinal(), values),
            (Some(segment), index) => self.running.return_across(segment, index, target, values),
        };
        self.delivering = false;
        result.map_err(ControlError::Trapped)
    }
}

impl<H: Host> Running<'_, '_, H> {
    fn return_from(
        &mut self,
        index: usize,
        ordinal: u64,
        values: Vec<Value>,
    ) -> Result<(), TrapInfo> {
        let pending = self
            .continuation
            .control
            .pending_host
            .expect("pending host guard");
        let location = pending.location;
        let span = self.continuation.ctx.span_at(
            self.continuation.program.program(),
            location.func,
            location.pc,
        );
        let result = self
            .prepare_exit(index, ordinal, &values)
            .and_then(|()| self.apply_exit(index, ordinal, values));
        if let Err(trap) = result {
            self.continuation.control.pending_host = None;
            let error = self
                .continuation
                .ctx
                .trap(location.func, location.pc, span, trap);
            self.finish(Err(error.clone()));
            return Err(error);
        }
        Ok(())
    }

    fn prepare_exit(&mut self, index: usize, ordinal: u64, values: &[Value]) -> Result<(), Trap> {
        let pending = self
            .continuation
            .control
            .pending_host
            .expect("pending host guard");
        let mut staging = self.continuation.host_results.take().unwrap_or_default();
        let work = staging.work;

        let created = staging
            .promote(self.vm.memory.aggs_mut())
            .map_err(Trap::AggError)?;
        let program = self.continuation.program.program();
        self.continuation
            .trace
            .values(program, || ValueEvent::ReturnFrom {
                location: pending.location,
                target: ordinal,
                target_location: self
                    .continuation
                    .control
                    .paused_location(&self.continuation.ctx, index),
                unwound_callbacks: self
                    .continuation
                    .control
                    .callbacks
                    .as_ref()
                    .map_or(0, |callbacks| callbacks.count_exited(index)),
                values,
                work,
                created: &created,
                aggregates: self.vm.memory.aggs(),
            });
        if self.budget.fuel == 0 {
            return Err(Trap::FuelExceeded);
        }
        self.budget.fuel -= 1;
        check_exit_values(self.vm, self.budget, self.continuation, index, values)
    }

    fn return_across(
        &mut self,
        segment: usize,
        index: usize,
        target_id: &ActivationId,
        values: Vec<Value>,
    ) -> Result<(), TrapInfo> {
        let pending = self
            .continuation
            .control
            .pending_host
            .expect("active host request");
        let location = pending.location;
        let span = self.continuation.ctx.span_at(
            self.continuation.program.program(),
            location.func,
            location.pc,
        );
        let result = (|| {
            let mut staged = self.continuation.host_results.take().unwrap_or_default();
            let work = staged.work;
            let created = staged
                .promote(self.vm.memory.aggs_mut())
                .map_err(Trap::AggError)?;
            let source = self
                .continuation
                .control
                .activation(&self.continuation.ctx, pending.depth - 1);
            let destination = &self.parents.as_ref().expect("parent chain")[segment];
            self.continuation
                .trace
                .control(self.continuation.program.program(), || {
                    ControlEvent::ReturnAcross {
                        location,
                        source: &source,
                        target: target_id,
                        instance: destination.ctx.instance.as_ref().expect("target instance"),
                        target_location: destination
                            .control
                            .paused_location(&destination.ctx, index),
                        unwound_callbacks: destination
                            .control
                            .callbacks
                            .as_ref()
                            .map_or(0, |callbacks| callbacks.count_exited(index)),
                        values: &values,
                        work,
                        created: &created,
                        aggregates: self.vm.memory.aggs(),
                    }
                });
            if self.budget.fuel == 0 {
                return Err(Trap::FuelExceeded);
            }
            self.budget.fuel -= 1;
            let target = &self.parents.as_ref().expect("execution parent chain")[segment];
            composition::check_transfer(
                self.vm.memory.aggs(),
                &values,
                self.continuation
                    .ctx
                    .instance
                    .as_ref()
                    .expect("active instance"),
                target.ctx.instance.as_ref().expect("target instance"),
                self.budget,
                &self.vm.limits,
                target.ctx.frames[index].func,
                false,
            )
            .map_err(|trap| trap.in_contract_definition(&target.program))?;
            self.budget.select(&target.control);
            check_exit_values(self.vm, self.budget, target, index, &values)
                .map_err(|trap| trap.in_contract_definition(&target.program))
        })();
        if let Err(trap) = result {
            let error = self
                .continuation
                .ctx
                .trap(location.func, location.pc, span, trap);
            self.finish(Err(error.clone()));
            return Err(error);
        }
        // Validation precedes every destructive control change. Storage remains untouched until
        // the root transaction completes, so surviving values never need snapshots or remaps.
        self.unwind_segment(target_id.ordinal());
        while self.parents.as_ref().expect("parent chain").len() > segment + 1 {
            let parent = self
                .parents
                .as_deref_mut()
                .expect("parent chain")
                .pop()
                .expect("intermediate parent");
            *self.continuation = parent;
            self.unwind_segment(target_id.ordinal());
        }
        *self.continuation = self
            .parents
            .as_deref_mut()
            .expect("parent chain")
            .pop()
            .expect("target parent");
        self.budget.select(&self.continuation.control);
        if let Err(trap) = self.apply_exit(index, target_id.ordinal(), values) {
            let current = self
                .continuation
                .control
                .pending_host
                .map_or(location, |pending| pending.location);
            let error = self
                .continuation
                .ctx
                .trap(current.func, current.pc, None, trap);
            self.finish(Err(error.clone()));
            return Err(error);
        }
        Ok(())
    }

    fn unwind_segment(&mut self, target: u64) {
        let outcome = TraceOutcome::Unwound { target };
        finish_control_trace(self.continuation, outcome);
        self.budget.frames -= self.continuation.ctx.frames.len();
        self.continuation.ctx.clear();
    }

    fn apply_exit(
        &mut self,
        index: usize,
        ordinal: u64,
        mut values: Vec<Value>,
    ) -> Result<(), Trap> {
        let location = self
            .continuation
            .control
            .pending_host
            .expect("pending host guard")
            .location;
        if index == 0 {
            self.check_return_values(&values)?;
        }
        let target = &self.continuation.ctx.frames[index];
        let returns_to_guest = target.return_to;
        let program = self.continuation.program.program();
        if index == 0 {
            self.continuation
                .trace
                .values(program, || ValueEvent::ReturnCandidate {
                    location,
                    values: &values,
                    aggregates: self.vm.memory.aggs(),
                });
        }
        let outcome = TraceOutcome::Unwound { target: ordinal };
        let target_base = target.base;
        let span = self
            .continuation
            .ctx
            .span_at(program, location.func, location.pc);
        // Emit exits while all actual frames/banks remain intact. If a sink panics, the guard
        // poisons the original host request and inspection must still see waiting callers,
        // never the uninitialized result banks of a prematurely popped child.
        self.close_exited_host(location, outcome);
        let mut restored = None;
        for depth in (index + 1..=self.continuation.ctx.frames.len()).rev() {
            self.continuation.trace.exit_frame(
                self.continuation.program.program(),
                depth,
                location,
                span,
                outcome,
            );
            let waiting = self
                .continuation
                .control
                .callbacks
                .as_ref()
                .and_then(|callbacks| callbacks.waiting_at(depth - 1));
            if let Some((host, instruction)) = waiting {
                self.continuation.trace.pending = instruction.then_some(host.location);
                if depth - 1 == index {
                    restored = Some(host);
                } else {
                    self.close_exited_host(host.location, outcome);
                }
            }
        }
        self.budget.frames -= self.continuation.ctx.frames.len() - index;
        self.continuation.ctx.frames.truncate(index);
        self.continuation.ctx.truncate_to(target_base);
        self.continuation.control.pending_host = restored;
        if let Some(callbacks) = &mut self.continuation.control.callbacks {
            callbacks.discard_exited(index);
            if restored.is_some() {
                callbacks.returned = Some(values);
                return Ok(());
            }
        }
        if index == 0 {
            self.budget.contract_scratch.clear_proofs();
            if !self.has_parents() {
                self.vm
                    .memory
                    .commit(self.checkpoint, &mut values)
                    .map_err(Trap::AggError)?;
            }
            self.continuation.outputs = values;
            self.continuation.returned_at = Some(location);
            self.finish(Ok(()));
        } else {
            let caller = self.continuation.ctx.frames.last().expect("target caller");
            let body = self
                .continuation
                .program
                .verified(caller.func)
                .expect("verified caller");
            let destinations =
                body.vregs(returns_to_guest.expect("noncallback continuation").dst_rets);
            if destinations.len() != values.len() {
                return Err(Trap::InvalidPc);
            }
            let base = caller.base;
            for (&destination, value) in destinations.iter().zip(values) {
                self.continuation
                    .ctx
                    .write_owned_value(base, destination, value)?;
            }
        }
        Ok(())
    }

    fn close_exited_host(&mut self, location: TraceLocation, outcome: TraceOutcome<'_>) {
        self.continuation.trace.exit_host(
            self.continuation.program.program(),
            location,
            self.continuation.ctx.span_at(
                self.continuation.program.program(),
                location.func,
                location.pc,
            ),
            outcome,
        );
        self.continuation
            .trace
            .instr_end(self.continuation.program.program(), None, outcome);
    }
}

impl TraceCtx<'_> {
    fn exit_host(
        &mut self,
        program: &Program,
        location: TraceLocation,
        span: Option<u64>,
        outcome: TraceOutcome<'_>,
    ) {
        if matches!(self.scopes.last(), Some((ScopeKind::HostCall { .. }, _))) {
            let (kind, depth) = self.scopes.pop().expect("entered host scope");
            if let Some(mut sink) = self.sink.as_mut().map(Observer::borrow_mut) {
                sink.scope_exit(
                    program,
                    kind,
                    depth,
                    location.func,
                    location.pc,
                    span,
                    outcome,
                );
            }
        }
    }
    fn exit_frame(
        &mut self,
        program: &Program,
        depth: usize,
        location: TraceLocation,
        span: Option<u64>,
        outcome: TraceOutcome<'_>,
    ) {
        if matches!(self.scopes.last(), Some((ScopeKind::CallFrame { .. }, _))) {
            let (kind, entered_depth) = self.scopes.pop().expect("entered guest scope");
            debug_assert_eq!(
                depth, entered_depth,
                "exit scopes follow actual guest activations"
            );
            if let Some(mut sink) = self.sink.as_mut().map(Observer::borrow_mut) {
                sink.scope_exit(
                    program,
                    kind,
                    depth,
                    location.func,
                    location.pc,
                    span,
                    outcome,
                );
            }
        }
    }
}

fn check_exit_values<H: Host>(
    vm: &Vm<H>,
    budget: &mut Budget,
    continuation: &continuation::Continuation<'_>,
    index: usize,
    values: &[Value],
) -> Result<(), Trap> {
    let program = continuation.program.program();
    let function = continuation.ctx.frames[index].func;
    let body = &program.functions[function.0 as usize];
    let kinds = body.ret_types(program).map_err(|_| Trap::InvalidPc)?;
    if kinds.len() != values.len() {
        return Err(Trap::ArityMismatch);
    }
    for (value, kind) in values.iter().zip(kinds) {
        value.check_type(*kind)?;
    }
    let bytes = values.iter().try_fold(0_usize, |sum, value| {
        let bytes = match value {
            Value::Bytes(bytes) => bytes.len(),
            Value::Str(text) => text.len(),
            _ => 0,
        };
        sum.checked_add(bytes).ok_or(Trap::PayloadSizeOverflow)
    })?;
    budget.data.charge_payload_bytes(bytes, 1)?;
    let prepared = continuation
        .program
        .verified(function)
        .expect("active verified function");
    for check in &prepared.contract_checks[prepared.arg_check_count as usize..] {
        budget
            .contract_scratch
            .check_value(
                program,
                vm.memory.aggs(),
                &values[check.index as usize],
                check.contract,
                vm.limits.max_contract_work,
            )
            .map_err(|error| {
                vm.contract_trap(
                    function,
                    ContractPosition::Result { index: check.index },
                    error,
                )
            })?;
    }
    Ok(())
}
