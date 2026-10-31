// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Borrow-scoped, cooperatively scheduled execution.

use super::*;
use crate::callable::{Callable, CallableError};
use alloc::borrow::Cow;
use alloc::sync::Arc;

mod exits;
mod host_request;
pub use exits::ControlError;
pub use host_request::PendingHostCall;
pub(super) mod callback;

pub(super) enum ProgramRef<'a> {
    Borrowed(&'a VerifiedProgram),
    Owned(Arc<VerifiedProgram>),
}
impl core::ops::Deref for ProgramRef<'_> {
    type Target = VerifiedProgram;
    fn deref(&self) -> &Self::Target {
        match self {
            Self::Borrowed(program) => program,
            Self::Owned(program) => program,
        }
    }
}

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
/// Scheduling does not change the VM's hard resource limits. Trace scopes cover the logical
/// invocation, including time spent suspended; they measure elapsed latency, not active CPU time.
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
/// code/inputs; callable entries retain their exact code and inputs. These domains remain
/// intact while suspended. Host calls dispatch synchronously by default; opting into yielded
/// requests retains an admitted host instruction until the embedding supplies its completion.
/// Dropping an active invocation cancels it, discards its new aggregates, and closes trace scopes
/// with [`TraceOutcome::Cancelled`]. External host effects are not rolled back.
///
/// Completed invocations cannot execute again. Their result remains available through
/// [`Self::result`] until dropped, and owned outputs can be taken with [`Self::take_outputs`].
pub struct Invocation<'a, H: Host> {
    pub(super) vm: BorrowedOrOwned<'a, Vm<H>>,
    pub(super) program: ProgramRef<'a>,
    pub(super) entry: FuncId,
    pub(super) args: Cow<'a, [Value]>,
    pub(super) ctx: ExecutionContext,
    pub(super) control: ExecutionControl,
    checkpoint: Checkpoint,
    trace: TraceCtx<'a>,
    access: Option<BorrowedOrOwned<'a, dyn AccessSink + 'a>>,
    host_results: Option<crate::host::HostResults>,
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
        self.start_prepaid(program, entry, args, options, DataWork::default())
    }

    pub(crate) fn start_prepaid<'a>(
        &'a mut self,
        program: &'a VerifiedProgram,
        entry: FuncId,
        args: &'a [Value],
        options: InvocationOptions<'a>,
        work: DataWork,
    ) -> Result<Invocation<'a, H>, TrapInfo> {
        let checkpoint = self.memory.checkpoint();
        self.start_with_inputs(
            ProgramRef::Borrowed(program),
            entry,
            Cow::Borrowed(args),
            options,
            checkpoint,
            work,
        )
    }

    fn start_with_inputs<'a>(
        &'a mut self,
        program: ProgramRef<'a>,
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
    fn prepare(
        vm: BorrowedOrOwned<'a, Vm<H>>,
        program: ProgramRef<'a>,
        entry: FuncId,
        args: Cow<'a, [Value]>,
        options: InvocationOptions<'a>,
        checkpoint: Checkpoint,
        work: DataWork,
    ) -> Result<Self, TrapInfo> {
        let yield_host_calls = options.yield_host_calls;
        let mut invocation = Invocation {
            vm,
            program,
            entry,
            args,
            ctx: ExecutionContext::new(),
            control: ExecutionControl::default(),
            checkpoint,
            trace: TraceCtx::with_observer(options.mask.unwrap_or(TraceMask::NONE), options.trace),
            access: options.access,
            host_results: None,
            register_state: observations::RegisterState::default(),
            outputs: Vec::new(),
            outcome: None,
        };
        invocation
            .trace
            .run_start(invocation.program.program(), entry, invocation.args.len());
        invocation
            .trace
            .values(invocation.program.program(), || ValueEvent::Entry {
                entry,
                args: &invocation.args,
                aggregates: invocation.vm.memory.aggs(),
                limits: &invocation.vm.limits,
                work,
            });
        let result = work
            .check(&invocation.vm.limits)
            .map_err(|trap| invocation.ctx.trap(entry, 0, None, trap))
            .and_then(|()| {
                if invocation.trace.active() {
                    invocation.vm.initialize_run::<WithTracePolicy>(
                        &mut invocation.ctx,
                        &invocation.program,
                        entry,
                        &invocation.args,
                        &mut invocation.trace,
                    )
                } else {
                    invocation.vm.initialize_run::<NoTracePolicy>(
                        &mut invocation.ctx,
                        &invocation.program,
                        entry,
                        &invocation.args,
                        &mut invocation.trace,
                    )
                }
            });
        if let Err(error) = result {
            invocation.finish(Err(error.clone()));
            return Err(error);
        }
        invocation.ctx.aggregate_work = work.aggregate;
        invocation.ctx.payload_work = work.payload;
        invocation.control = ExecutionControl::start(yield_host_calls);
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
        if let Some(outcome) = &self.outcome {
            return outcome.clone().map(|()| RunStatus::Complete);
        }
        if let Some(pending) = &self.control.pending_host {
            if pending.abandoned {
                return Err(self.finish_abandoned_host(*pending));
            }
            return Ok(RunStatus::AwaitingHost);
        }
        self.control.remaining = quantum;
        let access = self
            .access
            .as_mut()
            .map(|sink| &mut **sink as &mut dyn AccessSink);
        let result = if self.trace.active() {
            self.vm.execute_loop::<WithTracePolicy, true>(
                &mut self.ctx,
                &self.program,
                self.entry,
                &self.args,
                &mut self.outputs,
                self.checkpoint,
                &mut self.trace,
                access,
                &mut self.register_state,
                &mut self.control,
            )
        } else {
            self.vm.execute_loop::<NoTracePolicy, true>(
                &mut self.ctx,
                &self.program,
                self.entry,
                &self.args,
                &mut self.outputs,
                self.checkpoint,
                &mut self.trace,
                access,
                &mut (),
                &mut self.control,
            )
        };
        match &result {
            Ok(RunStatus::Suspended | RunStatus::AwaitingHost) => {}
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
    /// successful return. Aggregate handles remain owned by this invocation's VM heap.
    /// These are raw ABI values: function/closure/nominal IDs still need their original code
    /// owner. Retain returned callbacks with [`Self::retain_callable`] before dropping an owned
    /// invocation when its lookup handle has already been discarded.
    pub fn take_outputs(&mut self) -> Option<Vec<Value>> {
        matches!(self.outcome, Some(Ok(()))).then(|| core::mem::take(&mut self.outputs))
    }

    /// Consumes a successful invocation and retains its result independently of the VM.
    ///
    /// An owned VM transfers its reachable aggregate graph without copying byte/string payloads.
    /// A borrowed VM copies reachable aggregates, leaving its existing retained handles valid.
    /// Both paths move root payloads, preserve sharing and discard unreachable snapshot nodes.
    /// Function/closure/nominal IDs still need their original program; clone [`Self::program_owner`]
    /// before consumption to retain that exact owner. Host objects stay host-owned. Snapshot work
    /// is explicit embedding work outside guest budgets.
    ///
    /// Returns `None` after a trap or before successful completion; consuming an unfinished
    /// invocation cancels it. Inspect [`Self::result`] first to preserve a trap diagnostic.
    /// If outputs were already taken, the snapshot is empty.
    pub fn into_snapshot(mut self) -> Result<Option<crate::aggregates::ValueSnapshot>, AggError> {
        if !matches!(self.outcome, Some(Ok(()))) {
            return Ok(None);
        }
        let outputs = core::mem::take(&mut self.outputs);
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
    /// Start through [`Callable::start`] to supply the owning program handle. A raw `Vm::start`
    /// only borrows its program and returns [`CallableError::MissingCodeOwner`]. The result
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
        let ProgramRef::Owned(program) = &self.program else {
            return Err(CallableError::MissingCodeOwner);
        };
        Callable::retain(
            Arc::clone(program),
            self.aggregates(),
            ValueRef::from_value(value),
        )
    }

    /// Remaining hard instruction/host fuel, including after completion or a trap.
    #[must_use]
    pub fn remaining_fuel(&self) -> u64 {
        self.ctx.fuel
    }

    /// Aggregate/payload work spent so far, including owned-input preparation and root returns.
    /// Counters accumulate across quanta and remain available after a return or trap. These are
    /// logical work units, not retained heap bytes. Includes staged host results before completion;
    /// arbitrary native allocations and contract work remain separate.
    #[must_use]
    pub fn data_work(&self) -> DataWork {
        DataWork {
            aggregate: self.ctx.aggregate_work
                + self.host_results.as_ref().map_or(0, |r| r.work.aggregate),
            payload: self.ctx.payload_work
                + self.host_results.as_ref().map_or(0, |r| r.work.payload),
        }
    }

    /// The immutable verified program whose local identities this invocation uses.
    #[must_use]
    pub fn program(&self) -> &VerifiedProgram {
        &self.program
    }

    /// Exact retained code owner for callable entries, including independently owned executions.
    ///
    /// Clone this handle before consuming the invocation when a retained aggregate snapshot
    /// contains function, closure or nominal type IDs. It remains available after the original
    /// callable lookup is dropped. Raw `Vm::start` entries only borrow code and return `None`;
    /// their embedding remains responsible for that program's lifetime and identity.
    #[must_use]
    pub fn program_owner(&self) -> Option<&Arc<VerifiedProgram>> {
        match &self.program {
            ProgramRef::Borrowed(_) => None,
            ProgramRef::Owned(program) => Some(program),
        }
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
        callback::finish_instructions(
            &mut self.control,
            &mut self.trace,
            self.program.program(),
            outcome,
        );
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
            .or_else(|| self.control.pending_host.map(|pending| pending.location))
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
        callback::finish_instructions(
            &mut self.control,
            &mut self.trace,
            self.program.program(),
            TraceOutcome::Cancelled,
        );
        self.trace
            .run_end(self.program.program(), TraceOutcome::Cancelled);
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
        let (checkpoint, args, work) = self.prepare_inputs(vm, args)?;
        vm.start_with_inputs(
            ProgramRef::Owned(Arc::clone(self.program())),
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
        let (checkpoint, args, work) = self.prepare_inputs(&mut vm, args)?;
        Invocation::prepare(
            BorrowedOrOwned::Owned(Box::new(vm)),
            ProgramRef::Owned(Arc::clone(self.program())),
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
    ) -> Result<(Checkpoint, Vec<Value>, DataWork), TrapInfo> {
        let entry = self.function_id();
        let Some(capture) = self.capture() else {
            let checkpoint = vm.memory.checkpoint();
            return Ok((checkpoint, args, DataWork::default()));
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
        let imported = vm
            .memory
            .aggs_mut()
            .import(&capture.snapshot)
            .map_err(|error| trap(Trap::AggError(error)))?;
        let [Value::Closure(closure)] = imported.as_slice() else {
            unreachable!("retained closure root");
        };
        args.insert(0, Value::Agg(closure.env));
        Ok((checkpoint, args, work))
    }
}
