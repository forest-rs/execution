// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Borrow-scoped, cooperatively scheduled execution.

use super::*;
use crate::callable::{Callable, CallableError};
use crate::control::arena::Resumptions;
use alloc::borrow::Cow;
use alloc::sync::Arc;

pub(super) mod composition;
pub(super) mod delimited;
mod exits;
pub(super) mod guest_call;
mod host_request;
mod tail;
pub use exits::ControlError;
pub use host_request::PendingHostCall;

use super::continuation::CodeOwner;

/// Progress after executing an invocation's scheduling quantum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunStatus {
    /// A safe instruction boundary was reached with the invocation still active.
    Suspended,
    /// An admitted host instruction is waiting for embedding-supplied completion.
    /// Query [`Invocation::pending_host_call`]; resuming alone does not invoke the host.
    /// A guest callback return restores its original request in this same state; take the
    /// callback results before choosing and completing the host outcome.
    AwaitingHost,
    /// The root returned and its results were committed.
    Complete,
}

/// Optional borrowed or owned observers of one logical invocation.
///
/// Scheduling does not change the VM's hard resource limits. Logical trace scopes retain their
/// identities across suspension. Profiling adapters measure attached-chain elapsed intervals:
/// capture parking closes physical spans, but scheduling pauses and host waiting still count.
/// Those intervals measure elapsed latency, not active CPU time.
#[derive(Default)]
pub struct InvocationOptions<'a> {
    pub(super) yield_host_calls: bool,
    pub(super) mask: Option<TraceMask>,
    pub(super) trace: Option<BorrowedOrOwned<'a, dyn TraceSink + 'a>>,
    pub(super) access: Option<BorrowedOrOwned<'a, dyn AccessSink + 'a>>,
}

impl fmt::Debug for InvocationOptions<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InvocationOptions")
            .field("mask", &self.mask)
            .field("yield_host_calls", &self.yield_host_calls)
            .field("access_recording", &self.access.is_some())
            .finish_non_exhaustive()
    }
}

impl<'a> InvocationOptions<'a> {
    /// Yields admitted host calls for the embedding to complete or dispatch explicitly.
    ///
    /// This preserves the ordinary host ABI and adds no async runtime. A pending request
    /// exclusively borrows its invocation; the embedding owns scheduling and external effects.
    /// Default invocations continue to invoke their host synchronously.
    #[must_use]
    pub fn yield_host_calls(mut self) -> Self {
        self.yield_host_calls = true;
        self
    }
    /// Requests the existing VM observation callbacks for the entire logical invocation.
    #[must_use]
    pub fn trace(mut self, mask: TraceMask, sink: &'a mut dyn TraceSink) -> Self {
        self.mask = Some(mask);
        self.trace = Some(BorrowedOrOwned::Borrowed(sink));
        self
    }

    /// Records host resource accesses across all scheduling quanta.
    #[must_use]
    pub fn access(mut self, sink: &'a mut dyn AccessSink) -> Self {
        self.access = Some(BorrowedOrOwned::Borrowed(sink));
        self
    }

    /// Owns a trace observer for an independently stored invocation.
    /// Shared `Rc<RefCell<S>>` sinks can retain captured evidence after invocation retirement.
    #[must_use]
    pub fn trace_owned(mut self, mask: TraceMask, sink: impl TraceSink + 'a) -> Self {
        self.mask = Some(mask);
        self.trace = Some(BorrowedOrOwned::Owned(Box::new(sink)));
        self
    }

    /// Owns an access observer across suspension. No graph access borrow is retained.
    /// Shared `Rc<RefCell<S>>` sinks can collect accesses for a task's next graph poll.
    #[must_use]
    pub fn access_owned(mut self, sink: impl AccessSink + 'a) -> Self {
        self.access = Some(BorrowedOrOwned::Owned(Box::new(sink)));
        self
    }
}

/// One invocation that may suspend between instructions or at an admitted host request.
///
/// `Vm::start` and `Callable::start` borrow their VM. `Callable::start_owned` transfers its VM
/// into this same invocation type. Observers can be borrowed or owned. Raw entries borrow
/// inputs; every entry retains its exact code definition. These domains remain
/// intact while suspended. Host calls dispatch synchronously by default; opting into yielded
/// requests retains an admitted host instruction until the embedding supplies its completion.
/// Dropping an active invocation cancels it, discards its new aggregates, and closes trace scopes
/// with [`TraceOutcome::Cancelled`]. External host effects are not rolled back.
///
/// Completed invocations cannot execute again. Their result remains available through
/// [`Self::result`] until dropped, and owned outputs can be taken with [`Self::take_outputs`].
///
/// [`PendingHostCall::call_guest`] can enter other verified code in the same store. Suspended
/// callers share its budgets and are visible in [`Self::inspect`]. Only the root publishes
/// results or reclaims new aggregate storage. A fatal trap or hard cancellation terminates
/// every retained segment; host effects remain outside the transaction.
pub struct Invocation<'a, H: Host> {
    pub(super) vm: BorrowedOrOwned<'a, Vm<H>>,
    pub(super) continuation: continuation::Continuation<'a>,
    pub(super) parents: Vec<continuation::Continuation<'a>>,
    checkpoint: Checkpoint,
    budget: Budget,
    next_segment: u64,
    pub(super) resumptions: Resumptions<delimited::Saved<'a>>,
}

/// Temporary authority to operate one control stack in its execution transaction.
/// This is deliberately separate from both the public owning facade and suspended state.
pub(super) struct Running<'request, 'state, H: Host> {
    pub(super) vm: &'request mut Vm<H>,
    pub(super) continuation: &'request mut continuation::Continuation<'state>,
    pub(super) parents: &'request mut Vec<continuation::Continuation<'state>>,
    pub(super) checkpoint: Checkpoint,
    pub(super) budget: &'request mut Budget,
    pub(super) next_segment: &'request mut u64,
    pub(super) resumptions: &'request mut Resumptions<delimited::Saved<'state>>,
}

impl<'a, H: Host> Invocation<'a, H> {
    pub(in crate::vm) fn from_scratch(
        vm: &'a mut Vm<H>,
        program: &VerifiedProgram,
        entry: FuncId,
        args: &'a [Value],
        options: InvocationOptions<'a>,
        mut scratch: ExecutionContext,
        outputs: Vec<Value>,
    ) -> Self {
        scratch.budget.reset(&vm.limits);
        let checkpoint = vm.memory.checkpoint();
        let mut continuation = continuation::Continuation::with_control(
            CodeOwner::Definition(program.clone()),
            entry,
            Cow::Borrowed(args),
            options,
            ExecutionControl::default(),
        );
        continuation.ctx = scratch.registers;
        continuation.output_start = outputs.len();
        continuation.outputs = outputs;
        Self {
            vm: BorrowedOrOwned::Borrowed(vm),
            continuation,
            budget: scratch.budget,
            parents: Vec::new(),
            checkpoint,
            next_segment: 1,
            resumptions: Resumptions::default(),
        }
    }

    pub(in crate::vm) fn initialize_scratch(&mut self) -> Result<(), TrapInfo> {
        let result =
            self.continuation
                .initialize(&mut self.vm, &mut self.budget, DataWork::default());
        if let Err(error) = &result {
            self.running().finish(Err(error.clone()));
        }
        result
    }

    pub(in crate::vm) fn recover_scratch(&mut self) -> (ExecutionContext, Vec<Value>) {
        let root = self.parents.first_mut().unwrap_or(&mut self.continuation);
        (
            ExecutionContext {
                registers: core::mem::take(&mut root.ctx),
                budget: core::mem::take(&mut self.budget),
            },
            core::mem::take(&mut root.outputs),
        )
    }

    pub(super) fn running(&mut self) -> Running<'_, 'a, H> {
        self.budget.select(&self.continuation.control);
        Running {
            vm: &mut self.vm,
            continuation: &mut self.continuation,
            parents: &mut self.parents,
            next_segment: &mut self.next_segment,
            resumptions: &mut self.resumptions,
            budget: &mut self.budget,
            checkpoint: self.checkpoint,
        }
    }
}

impl<H: Host> fmt::Debug for Invocation<'_, H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Invocation")
            .field("entry", &self.continuation.entry)
            .field("frames", &self.continuation.ctx.frames.len())
            .field("remaining_fuel", &self.budget.fuel)
            .field("outcome", &self.continuation.outcome)
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
        program: &VerifiedProgram,
        entry: FuncId,
        args: &'a [Value],
        options: InvocationOptions<'a>,
    ) -> Result<Invocation<'a, H>, TrapInfo> {
        self.start_prepaid(program, entry, args, options, DataWork::default())
    }

    pub(crate) fn start_prepaid<'a>(
        &'a mut self,
        program: &VerifiedProgram,
        entry: FuncId,
        args: &'a [Value],
        options: InvocationOptions<'a>,
        work: DataWork,
    ) -> Result<Invocation<'a, H>, TrapInfo> {
        let checkpoint = self.memory.checkpoint();
        self.start_with_inputs(
            CodeOwner::Definition(program.clone()),
            entry,
            Cow::Borrowed(args),
            options,
            checkpoint,
            work,
        )
    }

    fn start_with_inputs<'a>(
        &'a mut self,
        program: CodeOwner,
        entry: FuncId,
        args: Cow<'a, [Value]>,
        options: InvocationOptions<'a>,
        checkpoint: Checkpoint,
        work: DataWork,
    ) -> Result<Invocation<'a, H>, TrapInfo> {
        Invocation::prepare(
            BorrowedOrOwned::Borrowed(self),
            program,
            entry,
            args,
            options,
            checkpoint,
            work,
        )
    }
}

impl<'a, H: Host> Invocation<'a, H> {
    pub(super) fn prepare(
        vm: BorrowedOrOwned<'a, Vm<H>>,
        program: CodeOwner,
        entry: FuncId,
        args: Cow<'a, [Value]>,
        options: InvocationOptions<'a>,
        checkpoint: Checkpoint,
        work: DataWork,
    ) -> Result<Self, TrapInfo> {
        let budget = Budget::new(&vm.limits);
        let mut invocation = Invocation {
            vm,
            budget,
            checkpoint,
            continuation: continuation::Continuation::new(program, entry, args, options),
            parents: Vec::new(),
            next_segment: 1,
            resumptions: Resumptions::default(),
        };
        if let Err(error) =
            invocation
                .continuation
                .initialize(&mut invocation.vm, &mut invocation.budget, work)
        {
            invocation.running().finish(Err(error.clone()));
            return Err(error);
        }
        Ok(invocation)
    }
}

impl<H: Host> Invocation<'_, H> {
    /// Admits at most `quantum` guest instructions, or returns the terminal result again.
    ///
    /// A quantum of zero observes the current suspension without executing. Hard fuel, host-call,
    /// contract, payload and aggregate-work usage accumulate across resumes; quanta reset none of
    /// these. Host extra fuel is charged against hard fuel, not against the instruction quantum.
    /// Prepared block declarations take no steps; an edge transfer belongs to its branch step.
    /// A yielded host call spends its instruction/host-call debit once and returns
    /// [`RunStatus::AwaitingHost`]. Resuming while it waits spends nothing and never dispatches
    /// the host. Completing the request finishes that instruction without admitting another.
    pub fn resume(&mut self, quantum: u64) -> Result<RunStatus, TrapInfo> {
        self.resume_mode::<true>(quantum)
    }

    pub(in crate::vm) fn resume_mode<const COOPERATIVE: bool>(
        &mut self,
        quantum: u64,
    ) -> Result<RunStatus, TrapInfo> {
        if let Some(outcome) = &self.continuation.outcome {
            return if outcome.is_ok() && !self.parents.is_empty() {
                self.running().restore_after_return()
            } else {
                outcome.clone().map(|()| RunStatus::Complete)
            };
        }
        if let Some(location) = self.continuation.control.waiting.interrupted() {
            let error = self.continuation.ctx.trap(
                location.func,
                location.pc,
                None,
                Trap::GuestTransferAbandoned,
            );
            self.running().finish(Err(error.clone()));
            return Err(error);
        }
        if let Some(pending) = self.continuation.control.waiting.host() {
            if pending.abandoned {
                return Err(self.running().finish_abandoned_host(pending));
            }
            return Ok(RunStatus::AwaitingHost);
        }
        let result = self
            .continuation
            .advance_mode::<H, COOPERATIVE>(&mut self.vm, &mut self.budget, quantum)
            .and_then(|progress| match progress {
                Progress::Suspended => Ok(RunStatus::Suspended),
                Progress::Control {
                    location,
                    instruction,
                } => self.running().dispatch_control(location, instruction),
                Progress::AwaitingHost => Ok(RunStatus::AwaitingHost),
                Progress::GuestCall {
                    target,
                    environment,
                    args,
                } => self.enter_guest(target, environment, args),
                Progress::Returned(site) if !self.parents.is_empty() => {
                    self.running().finish_segment_return(site)?;
                    self.running().restore_after_return()
                }
                Progress::Returned(site) => {
                    self.running().check_root_tail_result(site)?;
                    self.running().prepare_root_return(site.location);
                    let continuation = &mut self.continuation;
                    if continuation.trace.active() {
                        self.vm.commit_return::<WithTracePolicy>(
                            &mut continuation.ctx,
                            &mut self.budget,
                            continuation.program.program(),
                            self.checkpoint,
                            &mut continuation.outputs[continuation.output_start..],
                            &mut continuation.trace,
                            site,
                        )?;
                    } else {
                        self.vm.commit_return::<NoTracePolicy>(
                            &mut continuation.ctx,
                            &mut self.budget,
                            continuation.program.program(),
                            self.checkpoint,
                            &mut continuation.outputs[continuation.output_start..],
                            &mut continuation.trace,
                            site,
                        )?;
                    }
                    Ok(RunStatus::Complete)
                }
            });
        match &result {
            Ok(RunStatus::Suspended | RunStatus::AwaitingHost) => {}
            Ok(RunStatus::Complete) => self.running().finish(Ok(())),
            Err(error) => {
                self.running().finish(Err(error.clone()));
            }
        }
        result
    }

    /// Borrows the committed outputs or terminal trap; `None` means execution is suspended.
    #[must_use]
    pub fn result(&self) -> Option<Result<&[Value], &TrapInfo>> {
        self.continuation.outcome.as_ref().and_then(|result| {
            if result.is_ok() && !self.parents.is_empty() {
                return None;
            }
            Some(
                result
                    .as_ref()
                    .map(|()| self.continuation.outputs.as_slice()),
            )
        })
    }

    /// Takes committed owned outputs, leaving an empty result buffer. Returns `None` before a
    /// successful return. Aggregate handles remain owned by this invocation's VM heap.
    /// These are raw ABI values: function/closure/nominal IDs still need their original code
    /// owner. Retain returned callbacks with [`Self::retain_callable`] before dropping an owned
    /// invocation when its lookup handle has already been discarded.
    pub fn take_outputs(&mut self) -> Option<Vec<Value>> {
        (self.parents.is_empty() && matches!(self.continuation.outcome, Some(Ok(()))))
            .then(|| core::mem::take(&mut self.continuation.outputs))
    }

    /// Consumes a successful invocation and retains its result independently of the VM.
    ///
    /// An owned VM transfers its reachable aggregate graph without copying byte/string payloads.
    /// A borrowed VM copies reachable aggregates, leaving its existing retained handles valid.
    /// Both paths move root payloads, preserve sharing and discard unreachable snapshot nodes.
    /// Function/closure/nominal IDs still need their original program; clone [`Self::program`]
    /// before consumption to retain that exact definition. Host objects stay host-owned. Snapshot work
    /// is explicit embedding work outside guest budgets.
    ///
    /// Returns `None` after a trap or before successful completion; consuming an unfinished
    /// invocation cancels it. Inspect [`Self::result`] first to preserve a trap diagnostic.
    /// If outputs were already taken, the snapshot is empty.
    pub fn into_snapshot(mut self) -> Result<Option<crate::aggregates::ValueSnapshot>, AggError> {
        if !self.parents.is_empty() || !matches!(self.continuation.outcome, Some(Ok(()))) {
            return Ok(None);
        }
        let outputs = core::mem::take(&mut self.continuation.outputs);
        let snapshot = match &mut self.vm {
            BorrowedOrOwned::Borrowed(vm) => vm.memory.aggs().snapshot_owned(outputs)?,
            BorrowedOrOwned::Owned(vm) => {
                core::mem::take(vm.memory.aggs_mut())
                    .into_snapshot(outputs)?
                    .0
            }
        };
        Ok(Some(snapshot))
    }

    /// Retains one successful function/closure result with its exact code and captures.
    ///
    /// Every entry retains its exact verified definition, including [`Vm::start`]. The result
    /// remains in the invocation; retention explicitly copies its reachable immutable captures
    /// outside guest work budgets. The retained callable survives this invocation and VM.
    pub fn retain_callable(&self, index: usize) -> Result<Callable, CallableError> {
        let outputs = self
            .result()
            .and_then(Result::ok)
            .ok_or(CallableError::NoSuccessfulResult)?;
        let value = outputs.get(index).ok_or(CallableError::OutputIndex {
            index,
            count: outputs.len(),
        })?;
        if !matches!(value, Value::Func(_) | Value::Closure(_)) {
            return Err(CallableError::NotCallable {
                actual: value.value_type(),
            });
        }
        Callable::retain(self.aggregates(), ValueRef::from_value(value))
    }

    /// Remaining hard instruction/host fuel, including after completion or a trap.
    #[must_use]
    pub fn remaining_fuel(&self) -> u64 {
        self.budget.fuel
    }

    /// Cumulative admitted VM work, including entry checks, host staging and control operations.
    ///
    /// Snapshot before and after an operation and use [`ExecutionWork::checked_sub`] to obtain
    /// its debit. Counters remain available after successful completion or a trap. Fuel is the
    /// amount actually debited, including clipped host extra fuel. Failed charges do not count
    /// the refused work. Capture/snapshot copies and arbitrary native work are outside this meter.
    ///
    /// This read-only query does not install tracing or change any limits. Summing independent
    /// invocations accounts for task work; it does not enforce a shared allocation or work ceiling.
    #[must_use]
    pub fn work(&self) -> ExecutionWork {
        ExecutionWork {
            fuel: self.vm.limits.fuel - self.budget.fuel,
            host_calls: self.budget.host_calls,
            contract: self.budget.contract_scratch.work_used(),
            data: self.data_work(),
        }
    }

    /// Aggregate/payload work spent so far, including owned-input preparation and root returns.
    /// Counters accumulate across quanta and remain available after a return or trap. These are
    /// logical work units, not retained heap bytes. Includes staged host results before completion;
    /// arbitrary native allocations and contract work remain separate.
    #[must_use]
    pub fn data_work(&self) -> DataWork {
        self.budget.data.used
    }

    /// The active segment's exact verified definition. It changes when control enters or returns
    /// from another instance. After a trap, it identifies the segment that actually failed.
    #[must_use]
    pub fn program(&self) -> &VerifiedProgram {
        &self.continuation.program
    }

    /// The original shared program handle supplied by a callable entry, when available.
    ///
    /// All entries retain their verified definition. Use `program().clone()` to keep that
    /// definition independently of any invocation. Callable entries additionally expose their
    /// original `Arc` here; raw [`Vm::start`] entries return `None`.
    #[must_use]
    pub fn program_owner(&self) -> Option<&Arc<VerifiedProgram>> {
        match &self.continuation.program {
            CodeOwner::Definition(_) | CodeOwner::Instance(_) => None,
            CodeOwner::Original(program) | CodeOwner::RetainedInstance { program, .. } => {
                Some(program)
            }
        }
    }

    /// The active store instance, if explicitly selected or admitted by a host boundary.
    /// Scalar entries may have no instance yet; inspecting this query creates none.
    #[must_use]
    pub fn instance(&self) -> Option<&Instance> {
        self.continuation.ctx.instance.as_ref()
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
}

impl<H: Host> Running<'_, '_, H> {
    /// Retire all execution-local control state before root publication can compact storage.
    fn prepare_root_return(&mut self, location: TraceLocation) {
        self.continuation.control.waiting = Waiting::Transition(location);
        self.cancel_saved();
        self.budget.contract_scratch.clear_proofs();
    }

    fn finish(&mut self, result: Result<(), TrapInfo>) {
        self.finish_outcome(result, false);
    }

    fn finish_outcome(&mut self, result: Result<(), TrapInfo>, abandoned: bool) {
        if self.continuation.outcome.is_some() {
            return;
        }
        self.continuation.outcome = Some(result);
        let result = self
            .continuation
            .outcome
            .clone()
            .expect("terminal outcome installed");
        if let Err(error) = &result {
            let saved = self.retire_saved();
            self.budget.frames -= self.continuation.ctx.frames.len();
            self.budget.contract_scratch.clear_proofs();
            {
                for parent in self.parents.iter_mut().rev() {
                    if parent.outcome.is_none() {
                        self.budget.frames -= parent.ctx.frames.len();
                        parent.outcome = Some(Err(error.clone()));
                    }
                }
            }
            self.vm.memory.rollback(self.checkpoint);
            self.continuation
                .outputs
                .truncate(self.continuation.output_start);
            // Essential retirement and rollback precede arbitrary cancellation observers.
            // A panic cannot leave a terminal invocation retaining uncommitted storage.
            for stack in saved {
                stack.cancel();
            }
        }
        let outcome = match &result {
            Ok(()) => TraceOutcome::Ok,
            Err(_) if abandoned => TraceOutcome::Cancelled,
            Err(error) => TraceOutcome::Trap(error),
        };
        finish_control_trace(self.continuation, outcome);
        if result.is_err() {
            for parent in self.parents.iter_mut().rev() {
                finish_control_trace(parent, outcome);
            }
        }
    }

    pub(super) fn cancel(&mut self) {
        if self.continuation.outcome.is_none()
            || self.parents.iter().any(|parent| parent.outcome.is_none())
        {
            self.vm.memory.rollback(self.checkpoint);
        }
        self.cancel_saved();
        cancel_continuation(self.vm, self.continuation, self.budget, self.checkpoint);
        for parent in self.parents.iter_mut().rev() {
            cancel_continuation(self.vm, parent, self.budget, self.checkpoint);
        }
    }
}

fn cancel_continuation<H: Host>(
    vm: &mut Vm<H>,
    continuation: &mut continuation::Continuation<'_>,
    budget: &mut Budget,
    checkpoint: Checkpoint,
) {
    if continuation.outcome.is_some() {
        return;
    }
    let location = continuation
        .trace
        .pending
        .or_else(|| {
            continuation
                .control
                .waiting
                .host()
                .map(|pending| pending.location)
        })
        .or_else(|| {
            continuation.ctx.frames.last().map(|frame| {
                let pc = if matches!(
                    continuation.trace.scopes.last(),
                    Some((ScopeKind::HostCall { .. }, _))
                ) {
                    continuation
                        .program
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
            func: continuation.entry,
            pc: 0,
        });
    let span = continuation
        .ctx
        .span_at(continuation.program.program(), location.func, location.pc);
    vm.memory.rollback(checkpoint);
    budget.frames -= continuation.ctx.frames.len();
    continuation.ctx.clear();
    continuation
        .trace
        .cancel(continuation.program.program(), location, span);
    continuation
        .trace
        .run_end(continuation.program.program(), TraceOutcome::Cancelled);
}

fn finish_control_trace(
    continuation: &mut continuation::Continuation<'_>,
    outcome: TraceOutcome<'_>,
) {
    if !matches!(outcome, TraceOutcome::Ok) {
        let location = continuation
            .control
            .waiting
            .host()
            .map(|pending| pending.location)
            .or(continuation.trace.pending)
            .or_else(|| {
                continuation.ctx.frames.last().map(|frame| TraceLocation {
                    func: frame.func,
                    pc: frame.pc,
                })
            })
            .unwrap_or(TraceLocation {
                func: continuation.entry,
                pc: 0,
            });
        let span =
            continuation
                .ctx
                .span_at(continuation.program.program(), location.func, location.pc);
        continuation
            .trace
            .unwind(continuation.program.program(), location, span, outcome);
    }
    continuation
        .trace
        .finish(continuation.program.program(), outcome);

    continuation
        .trace
        .run_end(continuation.program.program(), outcome);
}

impl<H: Host> Drop for Invocation<'_, H> {
    fn drop(&mut self) {
        self.running().cancel();
    }
}

impl Callable {
    /// Starts this exact retained target in a VM, without executing a guest instruction.
    ///
    /// Takes ownership of the supplied arguments, excluding the hidden environment, without
    /// cloning their byte/string payloads. Raw argument handles must belong to the destination
    /// VM and this program's identity domain. A closure imports its retained graph and prepends
    /// its hidden environment. Capture copy work is admitted before import and debited against
    /// the invocation's normal
    /// aggregate/payload limits. Preparation rejection emits no run observations or host effects.
    ///
    /// Dropping, trapping, or rejecting entry rolls back imported captures as well as guest
    /// allocations. Success retains only captures reachable from returned values. Recordings
    /// include the hidden environment and the entry-work debit, so checked replay uses the
    /// ordinary body signature with the same budgets. Host object validity remains the host's
    /// responsibility; use `visit_objects` to enumerate retained capture references. The returned
    /// invocation owns its code and inputs, so the lookup/callable handle can be dropped at once.
    pub fn start<'a, H: Host>(
        &self,
        vm: &'a mut Vm<H>,
        args: Vec<Value>,
        options: InvocationOptions<'a>,
    ) -> Result<Invocation<'a, H>, TrapInfo> {
        let (checkpoint, args, work, owner) = self.prepare_inputs(vm, args)?;
        vm.start_with_inputs(
            owner,
            self.function_id(),
            Cow::Owned(args),
            options,
            checkpoint,
            work,
        )
    }

    /// Starts an invocation that owns its VM, exact code and inputs.
    ///
    /// Transfer this invocation into a task or scheduler without borrowing VM storage or using
    /// self-references. It uses the same interpreter, limits, tracing and cancellation as `start`.
    /// Supply owned observers in `InvocationOptions` when the invocation must be `'static`.
    /// A host can still borrow external data when that data outlives the returned invocation.
    /// Dropping the invocation releases its VM and cancels unfinished guest work; external host
    /// effects remain real. Raw result handles still belong to that VM: explicitly snapshot or
    /// retain values before dropping it. Borrowing `start` continues to support reusable VMs.
    pub fn start_owned<'a, H: Host + 'a>(
        &self,
        mut vm: Vm<H>,
        args: Vec<Value>,
        options: InvocationOptions<'a>,
    ) -> Result<Invocation<'a, H>, TrapInfo> {
        let (checkpoint, args, work, owner) = self.prepare_inputs(&mut vm, args)?;
        Invocation::prepare(
            BorrowedOrOwned::Owned(Box::new(vm)),
            owner,
            self.function_id(),
            Cow::Owned(args),
            options,
            checkpoint,
            work,
        )
    }

    fn prepare_inputs<H: Host>(
        &self,
        vm: &mut Vm<H>,
        mut args: Vec<Value>,
    ) -> Result<(Checkpoint, Vec<Value>, DataWork, CodeOwner), TrapInfo> {
        let entry = self.function_id();
        let Some(capture) = self.capture() else {
            let checkpoint = vm.memory.checkpoint();
            return Ok((
                checkpoint,
                args,
                DataWork::default(),
                CodeOwner::Original(Arc::clone(self.program())),
            ));
        };
        let trap = |trap| TrapInfo {
            func: entry,
            pc: 0,
            span_id: None,
            trap,
        };
        let signature = self.signature();
        if args.len() != signature.arg_contracts().len() {
            return Err(trap(Trap::ArityMismatch));
        }
        for (value, kind) in args.iter().zip(signature.arg_types()) {
            value.check_type(*kind).map_err(trap)?;
        }
        let work = DataWork {
            aggregate: capture.aggregate_work,
            payload: capture.payload_work,
        };
        work.check(&vm.limits).map_err(trap)?;
        let checkpoint = vm.memory.checkpoint();
        let imported = vm.import_capture(&capture.snapshot).map_err(trap)?;
        let [Value::Closure(closure)] = imported.as_slice() else {
            unreachable!("retained closure root");
        };
        args.insert(0, Value::Agg(closure.env));
        let owner = CodeOwner::RetainedInstance {
            program: Arc::clone(self.program()),
            instance: closure.func.instance().clone(),
        };
        Ok((checkpoint, args, work, owner))
    }
}
