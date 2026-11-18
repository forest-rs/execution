// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Borrow-scoped, cooperatively scheduled execution.

use super::*;
use crate::callable::{Callable, CallableError};
use crate::program::LinkId;
use alloc::borrow::Cow;
use alloc::sync::Arc;

mod host_request;
pub use host_request::PendingHostCall;
pub(super) use host_request::read_registers;
pub(super) mod callback;
mod request;
pub use request::{RequestError, RequestId};
mod suspension;
pub(super) use suspension::PendingSuspend;
pub use suspension::PendingSuspension;

/// Progress after executing a turn's scheduling quantum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RunStatus {
    /// A safe instruction boundary was reached with the turn still active.
    Suspended,
    /// An admitted host instruction is waiting for embedding-supplied completion.
    /// Query [`Turn::pending_host_call`]; resuming alone does not invoke the host.
    AwaitingHost,
    /// A guest callback returned to the host request that started it.
    ///
    /// That original request is pending again. Take the callback's results with
    /// [`PendingHostCall::take_callback_results`], then complete the request or call back again.
    /// Resuming alone does not invoke the host.
    CallbackReturned,
    /// A `suspend` that no guest `on` clause handles is waiting for the embedding to handle it.
    ///
    /// Only turns started with [`TurnOptions::yield_suspensions`] report this.
    /// Query [`Turn::pending_suspension`] for the tag and payload, then resume, raise into
    /// or discard it. Resuming the turn alone executes nothing and reports this again.
    AwaitingHandler,
    /// The root returned. Its results stay in the turn, readable through [`Turn::result`],
    /// until [`Turn::finish`] publishes them; dropping the turn discards them.
    Complete,
}

impl RunStatus {
    fn waiting(pending: host_call::PendingHost) -> Self {
        if pending.callback_returned {
            Self::CallbackReturned
        } else {
            Self::AwaitingHost
        }
    }
}

/// How one turn runs: whether it yields host calls and suspensions, its allowance, and the
/// observers it borrows.
///
/// Scheduling does not change the VM's hard resource limits. Trace scopes cover the logical
/// turn, including time spent suspended; they measure elapsed latency, not active CPU time.
#[derive(Default)]
pub struct TurnOptions<'a> {
    pub(super) yield_host_calls: bool,
    pub(super) yield_suspensions: bool,
    pub(super) mask: Option<TraceMask>,
    pub(super) trace: Option<BorrowedOrOwned<'a, dyn TraceSink + 'a>>,
    pub(super) access: Option<BorrowedOrOwned<'a, dyn AccessSink + 'a>>,
    pub(super) allowance: Option<Allowance>,
}

impl fmt::Debug for TurnOptions<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TurnOptions")
            .field("mask", &self.mask)
            .field("yield_host_calls", &self.yield_host_calls)
            .field("yield_suspensions", &self.yield_suspensions)
            .field("access_recording", &self.access.is_some())
            .field("allowance", &self.allowance)
            .finish_non_exhaustive()
    }
}

impl<'a> TurnOptions<'a> {
    /// Gives this turn `allowance` in place of the work its VM's [`Limits`] allow.
    ///
    /// Fuel, host calls and contract, aggregate and payload work restart from it; frame and
    /// saved-computation limits stay the VM's. Recordings note the limits each turn ran under,
    /// so replay gives each the same.
    #[must_use]
    pub fn allowance(mut self, allowance: Allowance) -> Self {
        self.allowance = Some(allowance);
        self
    }

    /// The limits a turn with these options runs under, given its VM's.
    pub(super) fn limits(&self, vm: &Limits) -> Limits {
        self.allowance
            .map_or_else(|| vm.clone(), |allowance| vm.with_allowance(allowance))
    }

    /// Yields admitted host calls for the embedding to complete or dispatch explicitly.
    ///
    /// This preserves the ordinary host ABI and adds no async runtime. A pending request
    /// exclusively borrows its turn; the embedding owns scheduling and external effects.
    /// Default turns continue to invoke their host synchronously.
    #[must_use]
    pub fn yield_host_calls(mut self) -> Self {
        self.yield_host_calls = true;
        self
    }

    /// Yields suspensions that no guest handler handles to the embedding, which handles them.
    ///
    /// A `suspend` whose search for an `on` clause reaches the computation's root returns
    /// [`RunStatus::AwaitingHandler`] instead of trapping with [`Trap::UnhandledSuspension`].
    /// [`Turn::pending_suspension`] then exposes its tag and payload, and resumes it with
    /// the tag's results, raises an exception at it, or discards it. A suspension inside a guest
    /// callback still traps with [`Trap::SuspensionAcrossHost`]: only one that reaches the root
    /// of the turn's own stack is yielded. Default turns keep trapping.
    #[must_use]
    pub fn yield_suspensions(mut self) -> Self {
        self.yield_suspensions = true;
        self
    }
    /// Requests the existing VM observation callbacks for the whole turn.
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

    /// Owns a trace observer for an independently stored turn.
    /// Shared `Rc<RefCell<S>>` sinks can retain captured evidence after turn retirement.
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

/// What [`Vm::start`] runs: an installed function, or a retained [`Callable`] in a unit installed
/// from its exact program ([`Callable::at`]).
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub enum Callee<'c> {
    /// The installed function, whose arguments are exactly those passed.
    Function(FuncRef),
    /// A retained function or closure, started in `unit`. A closure imports its captures into
    /// the VM, charged to the turn, and receives its environment ahead of the arguments passed.
    Callable {
        /// The retained target.
        callable: &'c Callable,
        /// The unit installed from the callable's program.
        unit: UnitId,
    },
}

impl From<FuncRef> for Callee<'_> {
    fn from(function: FuncRef) -> Self {
        Self::Function(function)
    }
}

/// The arguments [`Vm::start`] passes: borrowed for the turn, or owned.
///
/// Borrowed arguments are copied into the VM, payloads included, only if the turn saves its
/// computation. A [`Callable`] entry takes ownership, copying borrowed arguments.
#[derive(Clone, Debug)]
pub struct Arguments<'t>(pub(super) Cow<'t, [Value]>);

impl<'t> From<&'t [Value]> for Arguments<'t> {
    fn from(args: &'t [Value]) -> Self {
        Self(Cow::Borrowed(args))
    }
}

impl<'t, const N: usize> From<&'t [Value; N]> for Arguments<'t> {
    fn from(args: &'t [Value; N]) -> Self {
        Self(Cow::Borrowed(args))
    }
}

impl<'t> From<&'t Vec<Value>> for Arguments<'t> {
    fn from(args: &'t Vec<Value>) -> Self {
        Self(Cow::Borrowed(args))
    }
}

impl From<Vec<Value>> for Arguments<'_> {
    fn from(args: Vec<Value>) -> Self {
        Self(Cow::Owned(args))
    }
}

/// How a [`Turn`] ended, which [`Turn::finish`] publishes.
#[derive(Debug)]
#[non_exhaustive]
pub enum TurnOutcome {
    /// The computation returned these values, whose aggregates stay in the VM's heap until
    /// collected or released like any other values.
    Returned(Vec<Value>),
    /// The computation trapped; everything it allocated was discarded.
    Trapped(TrapInfo),
    /// The computation waits in the VM under this ID, for [`Vm::resume`] to continue it.
    Saved(ComputationId),
}

/// A lease on a VM that drives one computation, from [`Vm::start`] or [`Vm::resume`], until it
/// finishes.
///
/// [`Self::drive`] runs the computation in bounded quanta; it may stop between instructions,
/// at an admitted host request or at a yielded suspension. Every entry runs an installed unit
/// and retains its program; the turn borrows its VM, its observers and any borrowed arguments.
/// Host calls dispatch synchronously by default; opting into yielded requests retains an
/// admitted host instruction until the embedding supplies its completion.
///
/// [`Self::finish`] ends the turn and publishes how it ended: the values the computation
/// returned, its trap, or, for one still running or waiting, the ID it is saved under in the VM
/// for a later [`Vm::resume`]. Each turn has its own work allowance. Dropping a turn instead
/// discards everything it allocated, even after the computation returned, cancels an unfinished
/// computation and closes trace scopes with [`TraceOutcome::Cancelled`]; forgetting it does the
/// same at the VM's next entry point. External host effects are not rolled back.
///
/// [`Self::result`] peeks at a computation that returned or trapped before finishing.
pub struct Turn<'a, H: Host> {
    pub(super) vm: &'a mut Vm<H>,
    pub(super) continuation: continuation::Continuation<'a>,
    checkpoint: Checkpoint,
    budget: Budget,
    /// The limits this turn runs under: the VM's, with its options' allowance in place of their
    /// work limits.
    limits: Limits,
    /// Whether [`Self::finish`] published the returned outputs, which dropping must keep.
    published: bool,
    /// The VM's execution state while this turn has it on loan. Its register file,
    /// control and budget are moved out into the fields above, and back before it is returned.
    exec: Option<Box<Exec>>,
}

/// Temporary authority to operate one control stack in its execution transaction.
/// This is deliberately separate from both the public owning facade and suspended state.
pub(super) struct Running<'request, 'state, H: Host> {
    pub(super) vm: &'request mut Vm<H>,
    pub(super) continuation: &'request mut continuation::Continuation<'state>,
    checkpoint: Checkpoint,
    pub(super) budget: &'request mut Budget,
}

impl<'a, H: Host> Turn<'a, H> {
    pub(super) fn running(&mut self) -> Running<'_, 'a, H> {
        Running::new(
            self.vm,
            &mut self.continuation,
            &mut self.budget,
            self.checkpoint,
        )
    }
}

impl<'request, 'state, H: Host> Running<'request, 'state, H> {
    pub(super) fn new(
        vm: &'request mut Vm<H>,
        continuation: &'request mut continuation::Continuation<'state>,
        budget: &'request mut Budget,
        checkpoint: Checkpoint,
    ) -> Self {
        budget.select(&continuation.stack.control);
        Self {
            vm,
            continuation,
            budget,
            checkpoint,
        }
    }
}

impl<H: Host> fmt::Debug for Turn<'_, H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Turn")
            .field("entry", &self.continuation.stack.entry)
            .field("frames", &self.continuation.stack.ctx.frames.len())
            .field("remaining_fuel", &self.budget.fuel)
            .field("outcome", &self.continuation.stack.outcome)
            .finish_non_exhaustive()
    }
}

impl<H: Host> Vm<H> {
    /// Starts a computation of `callee` with `args` in a new turn, without executing an
    /// instruction.
    ///
    /// The callee is an installed function or a retained [`Callable`] in a unit installed from
    /// its program ([`Callable::at`]). Arguments are borrowed or owned ([`Arguments`]). Validates
    /// entry arguments and contracts, initializes the root frame, and emits requested entry
    /// observations; [`Turn::drive`] then executes bounded scheduling quanta. A rejected entry
    /// returns its ordinary trap and leaves no new aggregates behind. A unit this VM did not
    /// install traps with [`Trap::UnknownUnit`] before any observation, and a callable whose
    /// program is not the unit's traps with [`Trap::UnitProgramMismatch`]. A closure imports its
    /// retained captures, charged to the turn's aggregate and payload work, and receives its
    /// environment ahead of `args`; dropping the turn or a trap discards the imports.
    pub fn start<'t, 'c>(
        &'t mut self,
        callee: impl Into<Callee<'c>>,
        args: impl Into<Arguments<'t>>,
        options: TurnOptions<'t>,
    ) -> Result<Turn<'t, H>, TrapInfo> {
        let Arguments(args) = args.into();
        match callee.into() {
            Callee::Function(entry) => {
                self.settle();
                let checkpoint = self.memory.checkpoint();
                self.start_with_inputs(entry, args, options, checkpoint, DataWork::default())
            }
            Callee::Callable { callable, unit } => {
                let limits = options.limits(&self.limits);
                let (checkpoint, args, work) =
                    callable.prepare_inputs(self, unit, args.into_owned(), &limits)?;
                self.start_with_inputs(
                    FuncRef::new(unit, callable.function_id()),
                    Cow::Owned(args),
                    options,
                    checkpoint,
                    work,
                )
            }
        }
    }

    pub(crate) fn start_prepaid<'a>(
        &'a mut self,
        entry: FuncRef,
        args: &'a [Value],
        options: TurnOptions<'a>,
        work: DataWork,
    ) -> Result<Turn<'a, H>, TrapInfo> {
        self.settle();
        let checkpoint = self.memory.checkpoint();
        self.start_with_inputs(entry, Cow::Borrowed(args), options, checkpoint, work)
    }

    pub(super) fn start_with_inputs<'a>(
        &'a mut self,
        entry: FuncRef,
        args: Cow<'a, [Value]>,
        options: TurnOptions<'a>,
        checkpoint: Checkpoint,
        work: DataWork,
    ) -> Result<Turn<'a, H>, TrapInfo> {
        Turn::prepare(self, entry, args, options, checkpoint, work)
    }
}

impl<'a, H: Host> Turn<'a, H> {
    fn prepare(
        vm: &'a mut Vm<H>,
        entry: FuncRef,
        args: Cow<'a, [Value]>,
        options: TurnOptions<'a>,
        checkpoint: Checkpoint,
        work: DataWork,
    ) -> Result<Self, TrapInfo> {
        let program = Arc::clone(vm.entry_program(entry.unit)?);
        let limits = options.limits(&vm.limits);
        let mut exec = vm.lend(checkpoint);
        let mut budget = core::mem::take(&mut exec.budget);
        budget.reset(&limits);
        budget.limit_frames(vm.saved.resident_frames());
        let stack = vm.next_stack();
        let continuation =
            continuation::Continuation::new(&mut exec, stack, program, entry, args, options);
        let mut turn = Turn {
            vm,
            budget,
            checkpoint,
            limits,
            published: false,
            continuation,
            exec: Some(exec),
        };
        if let Err(error) =
            turn.continuation
                .initialize(turn.vm, &mut turn.budget, &turn.limits, work)
        {
            turn.running().finish(Err(error.clone()));
            return Err(error);
        }
        Ok(turn)
    }
}

impl<H: Host> Turn<'_, H> {
    /// Admits at most `quantum` guest instructions, or returns the terminal result again.
    ///
    /// A quantum of zero observes the current suspension without executing. Fuel, host-call,
    /// contract, payload and aggregate-work usage accumulate across quanta within the turn;
    /// quanta reset none of these. Host extra fuel is charged against fuel, not against the
    /// instruction quantum. Prepared block declarations take no steps; an edge transfer belongs
    /// to its branch step. A yielded host call spends its instruction/host-call debit once and
    /// returns [`RunStatus::AwaitingHost`]; a guest callback returning to its request reports
    /// [`RunStatus::CallbackReturned`]. Driving while a request waits spends nothing, reports the
    /// same status again and never dispatches the host. Completing the request finishes that
    /// instruction without admitting another.
    pub fn drive(&mut self, quantum: u64) -> Result<RunStatus, TrapInfo> {
        if let Some(outcome) = &self.continuation.stack.outcome {
            return outcome.clone().map(|()| RunStatus::Complete);
        }
        if let Some(location) = self.continuation.stack.control.delivering.take() {
            // A synchronous host call was interrupted before its results were admitted.
            return Err(self.running().finish_abandoned(location));
        }
        if let Some(pending) = self.continuation.stack.control.pending_host {
            if pending.abandoned {
                return Err(self.running().finish_abandoned_host(pending));
            }
            return Ok(RunStatus::waiting(pending));
        }
        if let Some(pending) = self.continuation.stack.control.pending_suspension {
            if pending.abandoned {
                self.continuation.stack.control.pending_suspension = None;
                return Err(self.running().finish_abandoned(pending.location));
            }
            return Ok(RunStatus::AwaitingHandler);
        }
        let result = self
            .continuation
            .advance(self.vm, &mut self.budget, quantum)
            .and_then(|progress| match progress {
                Progress::Suspended => Ok(RunStatus::Suspended),
                Progress::AwaitingHandler => Ok(RunStatus::AwaitingHandler),
                Progress::AwaitingHost => Ok(RunStatus::waiting(
                    self.continuation
                        .stack
                        .control
                        .pending_host
                        .expect("a waiting host request"),
                )),
                Progress::Returned(site) => {
                    let continuation = &mut self.continuation;
                    if continuation.trace.active() {
                        self.vm.commit_return::<WithTracePolicy>(
                            &mut continuation.stack.ctx,
                            &mut self.budget,
                            continuation.stack.program.program(),
                            self.checkpoint,
                            &mut continuation.stack.outputs,
                            &mut continuation.trace,
                            site,
                        )?;
                    } else {
                        self.vm.commit_return::<NoTracePolicy>(
                            &mut continuation.stack.ctx,
                            &mut self.budget,
                            continuation.stack.program.program(),
                            self.checkpoint,
                            &mut continuation.stack.outputs,
                            &mut continuation.trace,
                            site,
                        )?;
                    }
                    Ok(RunStatus::Complete)
                }
            });
        match &result {
            Ok(
                RunStatus::Suspended
                | RunStatus::AwaitingHost
                | RunStatus::CallbackReturned
                | RunStatus::AwaitingHandler,
            ) => {}
            Ok(RunStatus::Complete) => self.running().finish(Ok(())),
            Err(error) => self.running().finish(Err(error.clone())),
        }
        result
    }

    /// Peeks at the returned values or terminal trap; `None` means execution is still running or
    /// waiting.
    ///
    /// Returned values, and the aggregates they reach, last only as long as the turn unless
    /// [`Self::finish`] publishes them: dropping or forgetting the turn discards them, and later
    /// allocations reuse their handles. Copying handles out of this view keeps nothing alive.
    #[must_use]
    pub fn result(&self) -> Option<Result<&[Value], &TrapInfo>> {
        self.continuation.stack.outcome.as_ref().map(|result| {
            result
                .as_ref()
                .map(|()| self.continuation.stack.outputs.as_slice())
        })
    }

    /// Takes committed owned outputs, leaving an empty result buffer. Returns `None` before a
    /// successful return. Aggregate handles remain owned by this turn's VM heap.
    /// These are raw ABI values: function and closure references name units of this VM.
    /// Retain returned callbacks with [`Self::retain_callable`] before dropping an owned
    /// turn to keep them independently of the VM.
    pub fn take_outputs(&mut self) -> Option<Vec<Value>> {
        let returned = matches!(self.continuation.stack.outcome, Some(Ok(())));
        if returned && !self.published {
            self.published = true;
            self.vm.close_lease();
        }
        returned.then(|| core::mem::take(&mut self.continuation.stack.outputs))
    }

    /// Temporary forward to [`Self::drive`], removed later in this change.
    #[doc(hidden)]
    pub fn resume(&mut self, quantum: u64) -> Result<RunStatus, TrapInfo> {
        self.drive(quantum)
    }

    /// Consumes a successful turn and retains its result independently of the VM.
    ///
    /// Copies the reachable aggregates, leaving the VM's existing retained handles valid; root
    /// payloads move, sharing is preserved and unreachable nodes are left behind. To move a VM's
    /// whole graph without copying instead, take the outputs and use [`Vm::into_snapshot`].
    /// Function/closure/nominal IDs still need their original program; clone [`Self::program`]
    /// before consumption to retain that exact owner. Host objects stay host-owned. Snapshot work
    /// is explicit embedding work outside guest budgets.
    ///
    /// Returns `None` after a trap or before successful completion; consuming an unfinished
    /// turn cancels it. Inspect [`Self::result`] first to preserve a trap diagnostic.
    /// If outputs were already taken, the snapshot is empty.
    pub fn into_snapshot(mut self) -> Result<Option<crate::aggregates::ValueSnapshot>, AggError> {
        let Some(outputs) = self.take_outputs() else {
            return Ok(None);
        };
        Ok(Some(self.vm.memory.aggs().snapshot_owned(outputs)?))
    }

    /// Retains one successful function/closure result with its exact code and captures.
    ///
    /// The result remains in the turn; retention explicitly copies its reachable immutable
    /// captures outside guest work budgets. The retained callable survives this turn and
    /// VM.
    pub fn retain_callable(&self, index: usize) -> Result<Callable, CallableError> {
        let outputs = self
            .result()
            .and_then(Result::ok)
            .ok_or(CallableError::NoSuccessfulResult)?;
        let value = outputs.get(index).ok_or(CallableError::OutputIndex {
            index,
            count: outputs.len(),
        })?;
        self.vm.retained_callable(value)
    }
}

impl<H: Host> Vm<H> {
    /// Copies `values` and the aggregates they reach out of this VM into a snapshot.
    ///
    /// The VM's aggregates and their handles stay as they are; shared nodes stay shared in the
    /// copy, and unreachable ones are left behind. Function, closure and nominal IDs still need
    /// their original program, and host objects stay the host's. Snapshot work is the
    /// embedding's, outside any turn's budget. To move a retiring VM's graph without copying,
    /// use [`Self::into_snapshot`].
    ///
    /// Like every VM entry point, this first discards what a forgotten turn allocated, so values
    /// a turn returned but never published through [`Turn::finish`] name nothing to copy.
    pub fn snapshot(
        &mut self,
        values: &[Value],
    ) -> Result<crate::aggregates::ValueSnapshot, AggError> {
        self.settle();
        self.memory.aggs().snapshot_owned(values.to_vec())
    }

    /// Retains a function or closure `value` of this VM with its exact code and captures.
    ///
    /// Copies the closure's reachable immutable captures outside any turn's budget. The
    /// retained callable outlives this VM and can start in any VM with its program installed.
    /// Like [`Self::snapshot`], this first discards what a forgotten turn allocated.
    pub fn retain_callable(&mut self, value: &Value) -> Result<Callable, CallableError> {
        self.settle();
        self.retained_callable(value)
    }

    /// Retains `value` as [`Self::retain_callable`] does, without settling first.
    fn retained_callable(&self, value: &Value) -> Result<Callable, CallableError> {
        let unit = match value {
            Value::Func(function) => function.unit,
            Value::Closure(closure) => closure.func.unit,
            _ => {
                return Err(CallableError::NotCallable {
                    actual: value.value_type(),
                });
            }
        };
        // The value may be code of any unit, not only an entry unit.
        let program = self
            .units
            .get(unit)
            .ok_or(CallableError::MissingCodeOwner)?;
        Callable::retain(
            Arc::clone(&program.program),
            self.aggregates(),
            ValueRef::from_value(value),
        )
    }
}

impl<H: Host> Turn<'_, H> {
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
    /// turns accounts for task work; it does not enforce a shared allocation or work ceiling.
    #[must_use]
    pub fn work(&self) -> ExecutionWork {
        ExecutionWork {
            fuel: self.limits.fuel - self.budget.fuel,
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
        DataWork {
            aggregate: self.budget.aggregate_work
                + self
                    .continuation
                    .stack
                    .host_results
                    .as_ref()
                    .map_or(0, |r| r.work.aggregate),
            payload: self.budget.payload_work
                + self
                    .continuation
                    .stack
                    .host_results
                    .as_ref()
                    .map_or(0, |r| r.work.payload),
        }
    }

    /// The entry unit's program, retained by this turn.
    ///
    /// Clone this handle before consuming the turn when a retained aggregate snapshot
    /// contains function, closure or nominal type IDs.
    #[must_use]
    pub fn program(&self) -> &Arc<VerifiedProgram> {
        &self.continuation.stack.program
    }

    /// The installed unit this turn entered.
    #[must_use]
    pub fn unit(&self) -> UnitId {
        self.continuation.stack.unit
    }

    /// Installs `program` in this turn's VM between scheduling quanta.
    ///
    /// This is [`Vm::install`] for a VM the turn borrows or owns. Suspended frames keep
    /// the units they run; guest code reaches the new unit only through a [`FuncRef`] it is
    /// given, for example by a host call or as a callback.
    pub fn install(
        &mut self,
        program: impl Into<Arc<VerifiedProgram>>,
    ) -> Result<UnitId, InstallError> {
        install_observed(self.vm, &mut self.continuation, program.into())
    }

    /// Inspects the units installed in this turn's VM.
    #[must_use]
    pub fn units(&self) -> Units<'_> {
        self.vm.units()
    }

    /// Borrows aggregate storage. Values observed while suspended belong to this heap.
    #[must_use]
    pub fn aggregates(&self) -> &AggHeap {
        self.vm.memory.aggs()
    }

    /// Reclaims every aggregate this turn and the VM's registered groups do not reach,
    /// while the turn waits between quanta, at a host request or at a suspension.
    ///
    /// This is [`Vm::collect`], saved computations included, with the turn's state as
    /// further roots: its arguments, the reference registers of its active frames and of the
    /// frames its unresumed continuations hold, the payloads of exceptions those registers hold, pending callback results and,
    /// once it has returned, its outputs. A register is a root only where verification
    /// establishes that it holds a value: a register not yet written retains nothing, while a
    /// written one keeps its value alive until overwritten, even if never read again. The
    /// turn's string and byte descriptors are compacted the same way, dropping owned
    /// payloads no live register names. Aggregates a waiting host built and promoted by calling
    /// back stay alive until its request completes.
    ///
    /// The turn continues as before. Every aggregate handle copied out of it earlier,
    /// such as a request's [`arguments`](PendingHostCall::arguments), callback results already
    /// taken, or a previous resolution of a registered group, names nothing afterwards and
    /// fails with [`AggError::BadHandle`]: read them again. Borrowed entry arguments are left untouched; the turn keeps its own relocated copy.
    ///
    /// Collection is refused, moving and reclaiming nothing, while a host request holds
    /// aggregates its host constructed in staging ([`CollectError::HostStaging`]), or holds
    /// ones it built and promoted by calling back and the collection would move anything
    /// ([`CollectError::HostHandles`]),
    /// after an interrupted delivery ([`CollectError::Abandoned`]), or when planning finds a
    /// handle or register it cannot account for. Observers of a turn that has not
    /// finished see [`ValueEvent::Collected`]; a recording cannot replay across it and reports
    /// [`crate::recording::CaptureFault::Relocated`].
    pub fn collect(&mut self) -> Result<CollectionReport, CollectError> {
        let vm = &mut *self.vm;
        let running = &mut self.continuation;
        let mut handles = Vec::new();
        let stack_plan = running.stack.plan_collection(
            &running.args,
            &vm.units,
            vm.memory.aggs(),
            &mut handles,
        )?;
        let saved = vm.plan_saved(&mut handles)?;
        let roots =
            stack_plan.roots() + saved.iter().map(collection::StackPlan::roots).sum::<u64>();
        let plan = vm.plan_collection(&mut handles, roots)?;
        running.stack.check_host_handles(&plan)?;
        vm.check_saved(&plan)?;
        // Planning succeeded: nothing below can fail.
        let mut report = vm.apply_collection(&plan, Some(self.checkpoint));
        let compaction = plan.compaction();
        self.checkpoint = self.checkpoint.relocated(compaction);
        vm.relocate_lease(self.checkpoint);
        running
            .stack
            .relocate(&mut running.args, &stack_plan, compaction);
        stack_plan.report(&mut report);
        vm.relocate_saved(&saved, compaction, &mut report);
        // Contract proofs are keyed by handles that may now name other aggregates.
        self.budget.contract_scratch.clear_proofs();
        if running.stack.outcome.is_none() {
            // A finished turn's observers have seen its end, and a finished recording
            // already holds its values by content.
            let program = Arc::clone(&running.stack.program);
            let aggregates = vm.memory.aggs();
            running
                .trace
                .values(program.program(), || ValueEvent::Collected {
                    report: &report,
                    aggregates,
                });
        }
        Ok(report)
    }

    /// Borrows the current values of a group registered with this turn's VM.
    ///
    /// This is [`Vm::resolve`] while the turn borrows or owns the VM; resolve again after
    /// [`Self::collect`].
    pub fn resolve(&self, root: &RootedValues) -> Result<&[Value], RootError> {
        self.vm.resolve(root)
    }

    /// Reports the VM's heap size and collection history; see [`Vm::heap_stats`].
    #[must_use]
    pub fn heap_stats(&self) -> HeapStats {
        self.vm.heap_stats()
    }

    /// Borrows host state for read-only inspection between scheduling quanta.
    #[must_use]
    pub fn host(&self) -> &H {
        &self.vm.host
    }
}

impl<H: Host> Running<'_, '_, H> {
    fn finish(&mut self, result: Result<(), TrapInfo>) {
        self.continuation.stack.outcome = Some(result);
        let result = self
            .continuation
            .stack
            .outcome
            .as_ref()
            .expect("terminal outcome installed");
        let outcome = match result {
            Ok(()) => TraceOutcome::Ok,
            Err(error) => {
                self.budget.frames -= self.continuation.stack.ctx.held_frames();
                self.budget.contract_scratch.clear_proofs();
                self.vm.memory.rollback(self.checkpoint);
                self.continuation.stack.outputs.clear();
                TraceOutcome::Trap(error)
            }
        };
        // A trap is final: dropping this turn now rolls nothing more back, so neither may settling
        // it if it is forgotten. A return publishes only through `Turn::finish`, so its lease
        // stays open and dropping or forgetting the turn discards what it allocated. Close the
        // lease before observers run.
        if self
            .continuation
            .stack
            .outcome
            .as_ref()
            .is_some_and(Result::is_err)
        {
            self.vm.close_lease();
        }
        self.continuation
            .trace
            .finish(self.continuation.stack.program.program(), outcome);
        callback::finish_instructions(
            &mut self.continuation.stack.control,
            &mut self.continuation.trace,
            self.continuation.stack.program.program(),
            outcome,
        );
        self.continuation
            .trace
            .run_end(self.continuation.stack.program.program(), outcome);
    }
    pub(super) fn cancel(&mut self) {
        if self.continuation.stack.outcome.is_some() {
            return;
        }
        let location = self
            .continuation
            .trace
            .pending
            .or_else(|| {
                self.continuation
                    .stack
                    .control
                    .pending_host
                    .map(|pending| pending.location)
            })
            .or_else(|| {
                self.continuation
                    .stack
                    .control
                    .pending_suspension
                    .map(|pending| pending.location)
            })
            .or_else(|| {
                self.continuation.stack.ctx.frames.last().map(|frame| {
                    let pc = if matches!(
                        self.continuation.trace.scopes.last(),
                        Some((ScopeKind::HostCall { .. }, _))
                    ) {
                        self.vm
                            .units
                            .unit(frame.unit)
                            .program
                            .verified(frame.func)
                            .and_then(|body| body.pc_at_ix(frame.instr_ix.saturating_sub(1)))
                            .unwrap_or(frame.pc)
                    } else {
                        frame.pc
                    };
                    TraceLocation {
                        unit: frame.unit,
                        func: frame.func,
                        pc: Pc(pc),
                    }
                })
            })
            .unwrap_or(TraceLocation {
                unit: self.continuation.stack.unit,
                func: self.continuation.stack.entry,
                pc: Pc(0),
            });
        let span = self.continuation.stack.ctx.span_at(
            self.vm.units.unit(location.unit).program.program(),
            location.func,
            location.pc.0,
        );
        self.vm.memory.rollback(self.checkpoint);
        self.vm.close_lease();
        self.budget.frames -= self.continuation.stack.ctx.held_frames();
        self.continuation.stack.ctx.clear();
        self.continuation
            .trace
            .cancel(self.continuation.stack.program.program(), location, span);
        callback::finish_instructions(
            &mut self.continuation.stack.control,
            &mut self.continuation.trace,
            self.continuation.stack.program.program(),
            TraceOutcome::Cancelled,
        );
        self.continuation.trace.run_end(
            self.continuation.stack.program.program(),
            TraceOutcome::Cancelled,
        );
    }
}

impl<H: Host> Drop for Turn<'_, H> {
    fn drop(&mut self) {
        // A saved or finished turn already returned the execution state.
        if self.exec.is_none() {
            return;
        }
        if matches!(self.continuation.stack.outcome, Some(Ok(()))) {
            // Returned but not published: discard what it allocated, outputs included, as
            // settling a forgotten turn would.
            if !self.published {
                self.vm.memory.rollback(self.checkpoint);
                self.vm.close_lease();
            }
        } else {
            self.running().cancel();
        }
        self.release();
    }
}

impl<H: Host> Turn<'_, H> {
    /// Returns the execution state to the VM, ending the turn's use of it.
    fn release(&mut self) {
        if let Some(mut exec) = self.exec.take() {
            self.continuation.give_back(&mut exec);
            exec.budget = core::mem::take(&mut self.budget);
            self.vm.restore(exec);
        }
    }

    /// Ends this turn and publishes how it ended: the values the computation returned, its trap,
    /// or the ID it waits under, saved in the VM.
    ///
    /// This is the only way to keep what a computation returned: dropping a turn, even one whose
    /// computation returned, discards what it allocated. Returned values' aggregates stay in the
    /// VM's heap, valid until a collection moves them; root them with [`Vm::root`], copy them
    /// out with [`Vm::snapshot`], or retain a returned function with [`Vm::retain_callable`].
    ///
    /// A computation still running or waiting is saved: it stays exactly where it stopped,
    /// between quanta, at a host request with its callbacks, or at a yielded suspension, and
    /// keeps every aggregate it allocated. [`Vm::resume`] continues it in a later turn, with a
    /// fresh allowance; until then the VM can run, start, collect and save other computations,
    /// and [`Vm::collect`] keeps what it reaches. Borrowed arguments are copied into the VM,
    /// payloads included, outside guest budgets. A computation saved at a host request whose
    /// host built aggregates and then called back pins them: only the host holds their handles,
    /// so the VM keeps them and refuses every collection that would move aggregates
    /// ([`CollectError::SavedHostHandles`]) until the computation is resumed and its request
    /// completes. While computations are saved, [`AggHeap::clear`] and [`AggHeap::take_nodes`]
    /// panic: the saved handles would name other aggregates. Observers belong to the turn, not
    /// the computation: a trace observer receives [`TraceSink::release`] and no further events,
    /// with everything it saw open left open, and both observers are dropped.
    ///
    /// The turn comes back unchanged, and nothing is saved, when the VM already keeps
    /// [`Limits::max_saved_computations`] computations, when the computation waits at a host
    /// request whose host has aggregates in staging (only the host's handles reach them, and
    /// they would alias aggregates later runs allocate: complete the request first, or call back
    /// to promote them), or when a delivery to it was interrupted (drive it to see its trap).
    /// While the turn is held, its VM cannot resume or discard another computation, so a full
    /// store leaves two choices: drive this computation to the end, or drop the turn. Free a
    /// slot before opening a turn that will need to save.
    #[expect(
        clippy::result_large_err,
        reason = "a refused finish hands back the turn it was given, which is moved in anyway"
    )]
    pub fn finish(mut self) -> Result<TurnOutcome, Self> {
        match &self.continuation.stack.outcome {
            Some(Ok(())) => {
                let values = core::mem::take(&mut self.continuation.stack.outputs);
                if !self.published {
                    self.published = true;
                    self.vm.close_lease();
                }
                self.release();
                Ok(TurnOutcome::Returned(values))
            }
            Some(Err(trap)) => {
                let trap = trap.clone();
                self.release();
                Ok(TurnOutcome::Trapped(trap))
            }
            None => self.save().map(TurnOutcome::Saved),
        }
    }
}

impl<'a, H: Host> Turn<'a, H> {
    /// Marks this unrun turn's computation as created by [`Vm::spawn`].
    pub(super) fn mark_spawned(&mut self) {
        self.continuation.stack.spawned = true;
    }

    /// Saves this turn's computation in its VM; [`Self::finish`] describes when and how.
    #[doc(hidden)]
    #[expect(
        clippy::result_large_err,
        reason = "a refused save hands back the turn it was given, which is moved in anyway"
    )]
    pub fn save(mut self) -> Result<ComputationId, Self> {
        let stack = &self.continuation.stack;
        let staging = stack
            .host_results
            .as_ref()
            .is_some_and(crate::host::HostResults::is_staging);
        let abandoned = stack.control.delivering.is_some()
            || stack
                .control
                .pending_host
                .is_some_and(|host| host.abandoned)
            || stack
                .control
                .pending_suspension
                .is_some_and(|suspension| suspension.abandoned);
        let full = self.vm.saved.len() >= self.vm.limits.max_saved_computations;
        if stack.outcome.is_some() || full || staging || abandoned {
            return Err(self);
        }
        let program = Arc::clone(&self.continuation.stack.program);
        let work = self.work();
        let turn = self.continuation.stack.turns;
        let stack = StackId::new(self.vm.number, &self.continuation.stack.control);
        let aggregates = self.vm.memory.aggs();
        self.continuation
            .trace
            .values(program.program(), || ValueEvent::Saved {
                stack,
                work,
                turn,
                aggregates,
            });
        self.continuation.trace.release(program.program());
        let mut stack = self.continuation.stack.take();
        // Host results not in staging carry only this turn's work usage; the resuming
        // turn starts its own.
        stack.host_results = None;
        let args = Cow::Owned(core::mem::take(&mut self.continuation.args).into_owned());
        let mut exec = self
            .exec
            .take()
            .expect("a live turn holds the execution state");
        exec.budget = core::mem::take(&mut self.budget);
        // The saved stack keeps everything it allocated: nothing is left to discard.
        self.vm.close_lease();
        self.vm.restore(exec);
        let id = self.vm.saved.insert(saved::Saved { stack, args });
        self.vm.memory.aggs_mut().set_pinned(true);
        Ok(id)
    }
}

impl<H: Host> Vm<H> {
    /// Continues a saved computation in a new turn, with a fresh work allowance.
    ///
    /// The computation picks up exactly where its last turn finished: [`Turn::drive`] runs it
    /// on, and a request or suspension it was waiting at is still pending. `options` sets how
    /// this turn handles host calls and suspensions it reaches, its allowance and its observers:
    /// a trace observer receives [`TraceSink::attach`] with what is already open, in place of a
    /// run start, and then the computation's events as usual. Fuel, host calls and aggregate,
    /// payload and contract work restart from the turn's allowance; frames the computation holds
    /// still count against [`Limits::max_call_depth`], and with every other saved computation's
    /// against [`Limits::max_frames`]. A computation [`Self::spawn`] created enters on its first
    /// resume instead, as one [`Self::start`] begins: observers receive its run start and entry,
    /// and this is its first turn.
    ///
    /// The ID names nothing afterwards: finish the turn to save the computation again, or drop
    /// it to cancel the computation, which discards what this turn allocated. Refuses, leaving
    /// the computation saved, an ID this VM holds no computation for, another VM's ID, and
    /// options with a trace observer while a guest callback runs on top of a waiting host
    /// request.
    ///
    /// The computation leaves the VM's saved computations before the trace observer's
    /// [`TraceSink::attach`] runs, and the ID names nothing from then on: if the observer
    /// panics, dropping the turn cancels the computation.
    ///
    /// # Panics
    ///
    /// If the aggregate heap was replaced through [`Self::aggregates_mut`] while computations
    /// were saved: their handles would name other aggregates.
    pub fn resume<'a>(
        &'a mut self,
        id: ComputationId,
        options: TurnOptions<'a>,
    ) -> Result<Turn<'a, H>, ComputationError> {
        let saved = self.saved.get(id)?;
        let calling_back = saved
            .stack
            .control
            .callbacks
            .as_ref()
            .is_some_and(|callbacks| callbacks.running());
        // Access observers pair no events, so only a trace observer needs the handoff.
        if calling_back && options.trace.is_some() {
            return Err(ComputationError::Observed);
        }
        self.check_pinned();
        self.settle();
        let saved = self.saved.take(id).expect("checked above");
        let pinned = self.saved.len() != 0;
        self.memory.aggs_mut().set_pinned(pinned);
        let saved::Saved { mut stack, args } = *saved;
        let spawned = core::mem::take(&mut stack.spawned);
        if spawned {
            // Entered again below, as the first turn, under this one's observers.
            let owner = stack.control.owner();
            stack
                .control
                .start(owner, options.yield_host_calls, options.yield_suspensions);
        } else {
            stack.turns += 1;
            stack
                .control
                .set_yields(options.yield_host_calls, options.yield_suspensions);
        }
        let limits = options.limits(&self.limits);
        let checkpoint = self.memory.checkpoint();
        let mut exec = self.lend(checkpoint);
        let mut budget = core::mem::take(&mut exec.budget);
        budget.reset(&limits);
        budget.limit_frames(self.saved.resident_frames());
        budget.frames = if spawned { 0 } else { stack.ctx.held_frames() };
        let continuation = continuation::Continuation {
            stack,
            args,
            trace: TraceCtx::with_observer(options.mask.unwrap_or(TraceMask::NONE), options.trace),
            access: options.access,
            register_state: observations::RegisterState::default(),
        };
        let mut turn = Turn {
            vm: self,
            continuation,
            checkpoint,
            budget,
            limits,
            published: false,
            exec: Some(exec),
        };
        if spawned {
            // The same entry `spawn` checked; a refusal finishes this turn with its trap.
            if let Err(error) = turn.continuation.initialize(
                turn.vm,
                &mut turn.budget,
                &turn.limits,
                DataWork::default(),
            ) {
                turn.running().finish(Err(error));
            }
            return Ok(turn);
        }
        // The turn is complete before its observer runs: if the observer panics, dropping
        // it cancels the computation like any other.
        let continuation = &mut turn.continuation;
        let stack = &continuation.stack;
        continuation.trace.attach(
            &turn.vm.units,
            FuncRef::new(stack.unit, stack.entry),
            &stack.ctx,
            &stack.control,
        );
        let program = Arc::clone(&stack.program);
        let turns = stack.turns;
        let vm = &*turn.vm;
        let limits = &turn.limits;
        let yields_suspensions = stack.control.yield_suspensions;
        let stack = StackId::new(vm.number, &stack.control);
        continuation
            .trace
            .values(program.program(), || ValueEvent::Resumed {
                stack,
                turn: turns,
                limits,
                resident_frames: vm.saved.resident_frames(),
                yields_suspensions,
                aggregates: vm.memory.aggs(),
                units: vm.units(),
            });
        Ok(turn)
    }
}

impl Callable {
    /// This callable started in `unit`, a unit installed from its exact program, for
    /// [`Vm::start`].
    #[must_use]
    pub fn at(&self, unit: UnitId) -> Callee<'_> {
        Callee::Callable {
            callable: self,
            unit,
        }
    }

    /// Starts this exact retained target in a VM, without executing a guest instruction.
    ///
    /// `unit` must be a unit of `vm` installed from this callable's exact program (the same
    /// [`Arc`]); otherwise entry traps with [`Trap::UnitProgramMismatch`] before any work.
    /// Takes ownership of the supplied arguments, excluding the hidden environment, without
    /// cloning their byte/string payloads. Raw argument handles must belong to the destination
    /// VM and this program's identity domain. A closure imports its retained graph and prepends
    /// its hidden environment. Capture copy work is admitted before import and debited against
    /// the turn's normal
    /// aggregate/payload limits. Preparation rejection emits no run observations or host effects.
    ///
    /// Dropping, trapping, or rejecting entry rolls back imported captures as well as guest
    /// allocations. Success retains only captures reachable from returned values. Recordings
    /// include the hidden environment and the entry-work debit, so checked replay uses the
    /// ordinary body signature with the same budgets. Host object validity remains the host's
    /// responsibility; use `visit_objects` to enumerate retained capture references. The returned
    /// turn owns its code and inputs, so the lookup/callable handle can be dropped at once.
    pub fn start<'a, H: Host>(
        &self,
        vm: &'a mut Vm<H>,
        unit: UnitId,
        args: Vec<Value>,
        options: TurnOptions<'a>,
    ) -> Result<Turn<'a, H>, TrapInfo> {
        let limits = options.limits(&vm.limits);
        let (checkpoint, args, work) = self.prepare_inputs(vm, unit, args, &limits)?;
        vm.start_with_inputs(
            FuncRef::new(unit, self.function_id()),
            Cow::Owned(args),
            options,
            checkpoint,
            work,
        )
    }

    fn prepare_inputs<H: Host>(
        &self,
        vm: &mut Vm<H>,
        unit: UnitId,
        mut args: Vec<Value>,
        limits: &Limits,
    ) -> Result<(Checkpoint, Vec<Value>, DataWork), TrapInfo> {
        let entry = self.function_id();
        vm.settle();
        if !Arc::ptr_eq(vm.entry_program(unit)?, self.program()) {
            return Err(TrapInfo {
                unit,
                func: entry,
                pc: Pc(0),
                span_id: None,
                trap: Trap::UnitProgramMismatch { unit },
            });
        }
        let Some(capture) = self.capture() else {
            let checkpoint = vm.memory.checkpoint();
            return Ok((checkpoint, args, DataWork::default()));
        };
        let trap = |trap| TrapInfo {
            unit,
            func: entry,
            pc: Pc(0),
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
        work.check(limits).map_err(trap)?;
        let checkpoint = vm.memory.checkpoint();
        let first = vm.memory.aggs().handle_end();
        let imported = vm
            .memory
            .aggs_mut()
            .import(&capture.snapshot)
            .map_err(|error| trap(Trap::AggError(error)))?;
        // Captured callables name this program's unit in the destination VM.
        vm.memory.aggs_mut().retarget_units_since(first, unit);
        let [Value::Closure(closure)] = imported.as_slice() else {
            unreachable!("retained closure root");
        };
        args.insert(0, Value::Agg(closure.env));
        Ok((checkpoint, args, work))
    }
}

/// Repoints a link while a turn is suspended and reports the change to its trace.
///
/// A relink to the current target changes nothing and reports nothing.
pub(super) fn relink_observed<H: Host>(
    vm: &mut Vm<H>,
    continuation: &mut continuation::Continuation<'_>,
    unit: UnitId,
    link: LinkId,
    target: FuncRef,
) -> Result<FuncRef, RelinkError> {
    let previous = vm.relink(unit, link, target)?;
    if previous != target {
        let units = vm.units();
        let program = units.program(unit).expect("relinked unit");
        continuation
            .trace
            .values(program.program(), || ValueEvent::Relinked {
                unit,
                link,
                target,
                units,
            });
    }
    Ok(previous)
}

/// Installs `program` while a turn is suspended and reports it to the turn's trace.
pub(super) fn install_observed<H: Host>(
    vm: &mut Vm<H>,
    continuation: &mut continuation::Continuation<'_>,
    program: Arc<VerifiedProgram>,
) -> Result<UnitId, InstallError> {
    let unit = vm.install(program)?;
    let units = vm.units();
    let installed = units.program(unit).expect("installed unit");
    continuation
        .trace
        .values(installed.program(), || ValueEvent::Installed {
            unit,
            units,
        });
    Ok(unit)
}
