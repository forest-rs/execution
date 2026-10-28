// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Borrow-scoped, cooperatively scheduled execution.

use super::*;

/// Progress after executing an invocation's scheduling quantum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunStatus {
    /// A safe instruction boundary was reached with the invocation still active.
    Suspended,
    /// The root returned and its results were committed.
    Complete,
}

/// Optional observations of one invocation, borrowed until it completes or is dropped.
///
/// Scheduling does not change the VM's hard resource limits. Trace scopes cover the logical
/// invocation, including time spent suspended; they measure elapsed latency, not active CPU time.
#[derive(Default)]
pub struct InvocationOptions<'a> {
    pub(super) mask: Option<TraceMask>,
    pub(super) trace: Option<&'a mut dyn TraceSink>,
    pub(super) access: Option<&'a mut dyn AccessSink>,
}

impl fmt::Debug for InvocationOptions<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InvocationOptions")
            .field("mask", &self.mask)
            .field("access_recording", &self.access.is_some())
            .finish_non_exhaustive()
    }
}

impl<'a> InvocationOptions<'a> {
    /// Requests the existing VM observation callbacks for the entire logical invocation.
    #[must_use]
    pub fn trace(mut self, mask: TraceMask, sink: &'a mut dyn TraceSink) -> Self {
        self.mask = Some(mask);
        self.trace = Some(sink);
        self
    }

    /// Records host resource accesses across all scheduling quanta.
    #[must_use]
    pub fn access(mut self, sink: &'a mut dyn AccessSink) -> Self {
        self.access = Some(sink);
        self
    }
}

/// One invocation that may suspend between complete guest instructions.
///
/// Borrows its VM, verified program, inputs and observers. These borrows preserve their ownership
/// domains while suspended. Host calls are synchronous and indivisible scheduling operations.
/// Dropping an active invocation cancels it, discards its new aggregates, and closes trace scopes
/// with [`TraceOutcome::Cancelled`]. External host effects are not rolled back.
///
/// Completed invocations cannot execute again. Their result remains available through
/// [`Self::result`] until dropped, and owned outputs can be taken with [`Self::take_outputs`].
pub struct Invocation<'a, H: Host> {
    pub(super) vm: &'a mut Vm<H>,
    pub(super) program: &'a VerifiedProgram,
    pub(super) entry: FuncId,
    pub(super) args: &'a [Value],
    pub(super) ctx: ExecutionContext,
    checkpoint: Checkpoint,
    trace: TraceCtx<'a>,
    access: Option<&'a mut dyn AccessSink>,
    register_state: observations::RegisterState,
    outputs: Vec<Value>,
    outcome: Option<Result<(), TrapInfo>>,
}

impl<H: Host> fmt::Debug for Invocation<'_, H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Invocation")
            .field("entry", &self.entry)
            .field("frames", &self.ctx.frames.len())
            .field("remaining_fuel", &self.ctx.fuel)
            .field("outcome", &self.outcome)
            .finish_non_exhaustive()
    }
}

impl<H: Host> Vm<H> {
    /// Starts an invocation without executing an instruction.
    ///
    /// Validates entry arguments and contracts, initializes the root frame, and emits requested
    /// entry observations. Use [`Invocation::resume`] to execute bounded scheduling quanta.
    /// A rejected entry returns its ordinary trap and leaves no new aggregates behind.
    pub fn start<'a>(
        &'a mut self,
        program: &'a VerifiedProgram,
        entry: FuncId,
        args: &'a [Value],
        options: InvocationOptions<'a>,
    ) -> Result<Invocation<'a, H>, TrapInfo> {
        let checkpoint = self.memory.checkpoint();
        let mut invocation = Invocation {
            vm: self,
            program,
            entry,
            args,
            ctx: ExecutionContext::new(),
            checkpoint,
            trace: TraceCtx::new(options.mask.unwrap_or(TraceMask::NONE), options.trace),
            access: options.access,
            register_state: observations::RegisterState::default(),
            outputs: Vec::new(),
            outcome: None,
        };
        invocation
            .trace
            .run_start(program.program(), entry, args.len());
        invocation
            .trace
            .values(program.program(), || ValueEvent::Entry {
                entry,
                args,
                aggregates: invocation.vm.memory.aggs(),
                limits: &invocation.vm.limits,
            });
        let result = if invocation.trace.active() {
            invocation.vm.initialize_run::<WithTracePolicy>(
                &mut invocation.ctx,
                program,
                entry,
                args,
                &mut invocation.trace,
            )
        } else {
            invocation.vm.initialize_run::<NoTracePolicy>(
                &mut invocation.ctx,
                program,
                entry,
                args,
                &mut invocation.trace,
            )
        };
        if let Err(error) = result {
            invocation.finish(Err(error.clone()));
            return Err(error);
        }
        Ok(invocation)
    }
}

impl<H: Host> Invocation<'_, H> {
    /// Executes at most `quantum` guest instructions, or returns the terminal result again.
    ///
    /// A quantum of zero observes the current suspension without executing. Hard fuel, host-call,
    /// contract, payload and aggregate-work usage accumulate across resumes; quanta reset none of
    /// these. Host extra fuel is charged against hard fuel, not against the instruction quantum.
    /// Prepared block declarations take no steps; an edge transfer belongs to its branch step.
    pub fn resume(&mut self, quantum: u64) -> Result<RunStatus, TrapInfo> {
        if let Some(outcome) = &self.outcome {
            return outcome.clone().map(|()| RunStatus::Complete);
        }
        let mut remaining = quantum;
        let access = self
            .access
            .as_mut()
            .map(|sink| &mut **sink as &mut dyn AccessSink);
        let result = if self.trace.active() {
            self.vm.execute_loop::<WithTracePolicy, true>(
                &mut self.ctx,
                self.program,
                self.entry,
                self.args,
                &mut self.outputs,
                self.checkpoint,
                &mut self.trace,
                access,
                &mut self.register_state,
                &mut remaining,
            )
        } else {
            self.vm.execute_loop::<NoTracePolicy, true>(
                &mut self.ctx,
                self.program,
                self.entry,
                self.args,
                &mut self.outputs,
                self.checkpoint,
                &mut self.trace,
                access,
                &mut (),
                &mut remaining,
            )
        };
        match &result {
            Ok(RunStatus::Suspended) => {}
            Ok(RunStatus::Complete) => self.finish(Ok(())),
            Err(error) => self.finish(Err(error.clone())),
        }
        result
    }

    /// Borrows the committed outputs or terminal trap; `None` means execution is suspended.
    #[must_use]
    pub fn result(&self) -> Option<Result<&[Value], &TrapInfo>> {
        self.outcome
            .as_ref()
            .map(|result| result.as_ref().map(|()| self.outputs.as_slice()))
    }

    /// Takes committed owned outputs, leaving an empty result buffer. Returns `None` before a
    /// successful return. Aggregate handles remain owned by the borrowed VM's heap.
    pub fn take_outputs(&mut self) -> Option<Vec<Value>> {
        matches!(self.outcome, Some(Ok(()))).then(|| core::mem::take(&mut self.outputs))
    }

    /// Remaining hard instruction/host fuel, including after completion or a trap.
    #[must_use]
    pub fn remaining_fuel(&self) -> u64 {
        self.ctx.fuel
    }

    /// The immutable verified program whose local identities this invocation uses.
    #[must_use]
    pub fn program(&self) -> &VerifiedProgram {
        self.program
    }

    /// Borrows aggregate storage. Values observed while suspended belong to this heap.
    #[must_use]
    pub fn aggregates(&self) -> &AggHeap {
        self.vm.memory.aggs()
    }

    /// Borrows host state for read-only inspection between scheduling quanta.
    #[must_use]
    pub fn host(&self) -> &H {
        &self.vm.host
    }

    fn finish(&mut self, result: Result<(), TrapInfo>) {
        self.outcome = Some(result);
        let result = self.outcome.as_ref().expect("terminal outcome installed");
        let outcome = match result {
            Ok(()) => TraceOutcome::Ok,
            Err(error) => {
                self.ctx.contract_scratch.clear_proofs();
                self.vm.memory.rollback(self.checkpoint);
                self.outputs.clear();
                TraceOutcome::Trap(error)
            }
        };
        self.trace.finish(self.program.program(), outcome);
        self.trace.run_end(self.program.program(), outcome);
    }
}

impl<H: Host> Drop for Invocation<'_, H> {
    fn drop(&mut self) {
        if self.outcome.is_some() {
            return;
        }
        let location = self
            .trace
            .pending
            .or_else(|| {
                self.ctx.frames.last().map(|frame| {
                    let pc = if matches!(
                        self.trace.scopes.last(),
                        Some((ScopeKind::HostCall { .. }, _))
                    ) {
                        self.program
                            .verified(frame.func)
                            .and_then(|body| body.pc_at_ix(frame.instr_ix.saturating_sub(1)))
                            .unwrap_or(frame.pc)
                    } else {
                        frame.pc
                    };
                    TraceLocation {
                        func: frame.func,
                        pc,
                    }
                })
            })
            .unwrap_or(TraceLocation {
                func: self.entry,
                pc: 0,
            });
        let span = self
            .ctx
            .span_at(self.program.program(), location.func, location.pc);
        self.vm.memory.rollback(self.checkpoint);
        self.ctx.clear();
        self.trace.cancel(self.program.program(), location, span);
        self.trace
            .run_end(self.program.program(), TraceOutcome::Cancelled);
    }
}
