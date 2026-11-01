// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Activation-scoped typed returns. Condition selection and restart policy stay above the VM.

use super::*;

/// A denied control target or a terminal failure after a control operation was admitted.
#[derive(Clone, Debug)]
pub enum ControlError {
    /// The target belongs to another invocation. The pending request remains usable.
    ForeignInvocation,
    /// The exact target activation has already left. The pending request remains usable.
    ExpiredActivation,
    /// Admitted result validation or hard resource accounting terminated the invocation.
    Trapped(TrapInfo),
}
impl fmt::Display for ControlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
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
    /// Returns typed values from one exact live guest activation, abandoning its active callees.
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
        let index = self.running.continuation.control.target_index(target)?;
        self.delivering = true;
        let result = self.running.return_from(index, target.ordinal(), values);
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
        let mut staging = self
            .continuation
            .host_results
            .take()
            .unwrap_or_else(|| self.budget.host_results());
        let work = staging.work;
        self.budget.aggregate_work += work.aggregate;
        self.budget.payload_work += work.payload;
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
        let function = self.continuation.ctx.frames[index].func;
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
        self.budget.charge_payload_bytes(bytes, 1)?;
        let prepared = self
            .continuation
            .program
            .verified(function)
            .expect("active verified function");
        for check in &prepared.contract_checks[prepared.arg_check_count as usize..] {
            self.budget
                .contract_scratch
                .check_value(
                    program,
                    self.vm.memory.aggs(),
                    &values[check.index as usize],
                    check.contract,
                    self.vm.limits.max_contract_work,
                )
                .map_err(|error| {
                    self.vm.contract_trap(
                        function,
                        ContractPosition::Result { index: check.index },
                        error,
                    )
                })?;
        }
        Ok(())
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
        self.continuation.control.truncate(index);
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
            self.vm
                .memory
                .commit(self.checkpoint, &mut values)
                .map_err(Trap::AggError)?;
            self.continuation.outputs = values;
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
            if let Some(sink) = self.sink.as_mut() {
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
            if let Some(sink) = self.sink.as_mut() {
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
