// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Borrow-scoped delivery of one admitted host instruction.

use super::*;
use crate::host::HostThrow;
use crate::host::SigHash;
use crate::program::HostSigId;
use crate::value::TagRef;
use crate::vm::host_call::{HostReturnSite, PendingHost, admit_return, adopted_roots, observed};

/// The code-owned slices of a [`PendingHost`] request.
struct Site<'a> {
    symbol: &'a str,
    args: &'a [VReg],
    destinations: &'a [VReg],
    ret_types: &'a [ValueType],
}
/// Installed code with the invocation's entry unit at hand, the usual requester.
#[derive(Clone, Copy)]
struct Code<'a> {
    units: &'a UnitTable,
    entry: UnitId,
    program: &'a VerifiedProgram,
}

impl<'a> Code<'a> {
    fn new(units: &'a UnitTable, entry: UnitId, program: &'a VerifiedProgram) -> Self {
        Self {
            units,
            entry,
            program,
        }
    }
    fn of(self, unit: UnitId) -> &'a VerifiedProgram {
        if unit == self.entry {
            self.program
        } else {
            &self.units.unit(unit).program
        }
    }
    fn sources(self, args: &'a [Value], heap: &'a AggHeap) -> ValueSources<'a> {
        ValueSources::new(self.units, args, heap).with_current(self.entry, self.program.program())
    }
}

/// Code-owned parts of the request, borrowed on demand.
impl PendingHost {
    fn program<'a>(&self, code: Code<'a>) -> &'a VerifiedProgram {
        code.of(self.location.unit)
    }
    fn symbol<'a>(&self, code: Code<'a>) -> &'a str {
        self.program(code)
            .program()
            .symbol_str(self.symbol)
            .expect("verified host symbol")
    }
    fn args<'a>(&self, code: Code<'a>) -> &'a [VReg] {
        self.program(code)
            .verified(self.location.func)
            .expect("admitted function")
            .vregs(self.args)
    }
    fn types<'a>(&self, code: Code<'a>) -> (&'a [ValueType], &'a [ValueType]) {
        let p = self.program(code).program();
        let signature = p.host_sig(self.import).expect("verified host signature");
        (
            p.host_sig_args(signature).expect("verified argument types"),
            p.host_sig_rets(signature).expect("verified return types"),
        )
    }
    /// The waiting frame's register base. Frames do not move while their request waits.
    fn base<'a>(&self, ctx: &'a Registers) -> &'a RegBase {
        &ctx.frames[self.depth - 1].base
    }
    fn resolve<'a>(&self, code: Code<'a>) -> Site<'a> {
        let program = self.program(code);
        let body = program
            .verified(self.location.func)
            .expect("admitted function");
        let p = program.program();
        let signature = p.host_sig(self.import).expect("verified host signature");
        Site {
            symbol: p.symbol_str(self.symbol).expect("verified host symbol"),
            args: body.vregs(self.args),
            destinations: body.vregs(self.rets),
            ret_types: p.host_sig_rets(signature).expect("verified return types"),
        }
    }
}
/// Borrows the values of registers a waiting instruction already read successfully.
pub(in crate::vm) fn read_registers<'a>(
    ctx: &'a Registers,
    sources: ValueSources<'a>,
    base: &'a RegBase,
    regs: &'a [VReg],
) -> impl ExactSizeIterator<Item = ValueRef<'a>> {
    regs.iter().map(move |&register| {
        // All arguments were read successfully before the instruction yielded. No guest
        // operation or heap mutation can run while the pending guard borrows its invocation.
        read_value_ref_at(
            &ctx.arena,
            sources,
            &ctx.bools,
            &ctx.i64s,
            &ctx.u64s,
            &ctx.f64s,
            &ctx.w32s,
            &ctx.decimals,
            &ctx.bytes,
            &ctx.strs,
            &ctx.objs,
            &ctx.aggs,
            &ctx.funcs,
            &ctx.closures,
            *base,
            register,
        )
        .expect("admitted host argument")
    })
}

/// Exclusive view of one admitted host call, borrowed from its originating invocation.
///
/// Arguments, code and aggregate storage remain alive while it is pending. Inspection borrows
/// payloads; no byte/string copy is required. The embedding owns asynchronous work, scheduling,
/// external object lifetimes and effects. Use [`Self::complete_with`] for asynchronous
/// producers; this does not provide an async runtime or promise a `Send` future.
///
/// [`Self::complete`] supplies an external outcome; [`Self::dispatch`] invokes the existing host.
/// Both consume the guard and finish exactly this instruction, without executing the next one.
/// Dropping a view without either operation leaves the call pending. Dropping its invocation
/// cancels the guest and closes scopes; performed host effects are not rolled back.
///
/// If delivery or an observer panics, the invocation cannot retry that operation. Drop it, or
/// observe [`Trap::HostCallAbandoned`] on resumption; cancelled trace evidence is incomplete.
///
/// # Handle validity
///
/// Aggregate, closure and function handles read from any request of an invocation (its
/// [`arguments`](Self::arguments), [`callback_results`](Self::callback_results) and the values
/// they reach) stay valid until that invocation commits its root or rolls back. The embedding may
/// keep them as plain [`Value`]s and pass them to a later [`call_guest`](Self::call_guest) or
/// [`complete`](Self::complete) of the same invocation, including from a nested or later request.
/// Guest execution, callbacks, completions and caught exceptions only append to the heap; the
/// heap is compacted only when the root returns, and a trap or cancellation truncates it to the
/// invocation's start. Handles from [`context`](Self::context) are staged: `call_guest` promotes
/// them unchanged, while `complete` adopts only those reachable from its outcome, under new
/// handles. Retain values that must outlive the invocation through its outputs, a snapshot or
/// [`HostContext::retain`].
pub struct PendingHostCall<'request, 'vm, H: Host> {
    pub(super) running: Running<'request, 'vm, H>,
    pub(super) delivering: bool,
    pending: PendingHost,
}
impl<H: Host> fmt::Debug for PendingHostCall<'_, '_, H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingHostCall")
            .field("location", &self.location())
            .field("symbol", &self.symbol())
            .finish_non_exhaustive()
    }
}
impl<H: Host> PendingHostCall<'_, '_, H> {
    fn code(&self) -> Code<'_> {
        self.running.code()
    }
    /// Exact source instruction that admitted this request.
    #[must_use]
    pub fn location(&self) -> TraceLocation {
        self.pending.location
    }
    /// Installed unit whose program admitted this request.
    #[must_use]
    pub fn unit(&self) -> UnitId {
        self.location().unit
    }
    /// Installs `program` in this request's VM while the request waits.
    ///
    /// The waiting frames keep their units. Complete the request with, or call back into, a
    /// [`FuncRef`] of the new unit to run its code in this invocation.
    pub fn install(
        &mut self,
        program: impl Into<Arc<VerifiedProgram>>,
    ) -> Result<UnitId, InstallError> {
        install_observed(self.running.vm, self.running.continuation, program.into())
    }
    /// Inspects the units installed in this request's VM.
    #[must_use]
    pub fn units(&self) -> Units<'_> {
        self.running.vm.units()
    }
    /// Program-local declared import. Its identity belongs to [`Self::program`].
    #[must_use]
    pub fn import(&self) -> HostSigId {
        self.pending.import
    }
    /// Exact code whose import and nominal value identities this request uses.
    #[must_use]
    pub fn program(&self) -> &VerifiedProgram {
        self.pending.program(self.code())
    }
    /// Borrows the heap that gives this request's aggregate argument handles their meaning.
    #[must_use]
    pub fn aggregates(&self) -> &AggHeap {
        self.running.vm.memory.aggs()
    }
    /// Borrows host state for read-only inspection while this request waits.
    #[must_use]
    pub fn host(&self) -> &H {
        &self.running.vm.host
    }
    /// Mutates host state while this request waits, without releasing the request.
    ///
    /// This is [`Invocation::host_mut`] reached through the request, so a driver can consult or
    /// update its runtime and then complete, dispatch or call back on the same view. The borrow
    /// is exclusive: copy what you need from [`Self::arguments`] first, as handles stay valid
    /// (see [handle validity](Self#handle-validity)). Preserve the validity of object handles the
    /// guest still holds.
    pub fn host_mut(&mut self) -> &mut H {
        &mut self.running.vm.host
    }
    /// Borrows the actual caller's frames and initialized registers while this request waits.
    ///
    /// Analysis is lazy and read-only, as with [`Invocation::inspect`]. The pending call is
    /// shown before its result, preserving uninitialized destinations as unavailable.
    #[must_use]
    pub fn inspect(&self) -> InvocationInspection<'_> {
        self.running.inspect()
    }
    /// Declared host symbol, without allocating a label.
    #[must_use]
    pub fn symbol(&self) -> &str {
        self.pending.symbol(self.code())
    }
    /// Declared signature hash.
    #[must_use]
    pub fn signature_hash(&self) -> SigHash {
        self.pending.signature
    }
    /// Declared positional argument kinds.
    #[must_use]
    pub fn argument_types(&self) -> &[ValueType] {
        self.pending.types(self.code()).0
    }
    /// Declared positional result kinds.
    #[must_use]
    pub fn return_types(&self) -> &[ValueType] {
        self.pending.types(self.code()).1
    }
    /// Borrows actual arguments without copying byte/string payloads.
    ///
    /// Aggregate, closure and function handles among them stay valid for the rest of this
    /// invocation, as described under [handle validity](Self#handle-validity).
    pub fn arguments(&self) -> impl ExactSizeIterator<Item = ValueRef<'_>> {
        let inv = &self.running;
        let code = self.code();
        read_registers(
            &inv.continuation.ctx,
            code.sources(&inv.continuation.args, inv.vm.memory.aggs()),
            self.pending.base(&inv.continuation.ctx),
            self.pending.args(code),
        )
    }
    pub(crate) fn restore_host_work(&mut self, work: DataWork) -> Result<(), Trap> {
        let inv = &mut self.running;
        inv.continuation
            .host_results
            .get_or_insert_with(|| inv.budget.host_results())
            .prepaid(work)
    }
    pub(crate) fn intern_shape(
        &mut self,
        shape: crate::aggregates::AggShape,
    ) -> Result<crate::value::ShapeId, AggError> {
        self.running.vm.memory.aggs_mut().intern_shape(shape)
    }

    pub(crate) fn restore_host_node(
        &mut self,
        kind: crate::value::AggType,
        values: Vec<Value>,
    ) -> Result<AggHandle, AggError> {
        let inv = &mut self.running;
        inv.continuation
            .host_results
            .get_or_insert_with(|| inv.budget.host_results())
            .restore_node(inv.vm.memory.aggs(), kind, values)
    }

    /// Reads arguments, stages new aggregate results and records actual host resource accesses.
    ///
    /// Context readers include earlier staged results across successive views. Completing the
    /// call adopts reachable results; dropping the invocation discards them. Handles belong to
    /// this invocation's heap. Copy scalar/opaque arguments before borrowing
    /// this context mutably to record reads or writes, as in an ordinary host implementation.
    pub fn context(&mut self) -> HostContext<'_, '_> {
        let pending = self.pending;
        let inv = &mut self.running;
        HostContext::new(
            inv.vm.units.unit(pending.location.unit),
            pending.import,
            inv.vm.memory.aggs(),
            inv.continuation
                .host_results
                .get_or_insert_with(|| inv.budget.host_results()),
            inv.continuation
                .access
                .as_mut()
                .map(|sink| &mut **sink as &mut dyn AccessSink),
        )
    }
    /// Completes this instruction with return slots and extra host fuel, or a host error.
    ///
    /// This uses the ordinary host ABI: successfully adopted values move out of `values`,
    /// leaving Unit slots. Stack arrays and reusable buffers avoid a result-vector allocation.
    /// The raw outcome is observed before arity, type and payload-work validation. Aggregate,
    /// function, closure and nominal type IDs must belong to this request's original heap/code;
    /// this method does not import foreign VM handles. Validation failure terminates the run,
    /// rolls back new guest aggregates, and preserves already performed external effects.
    /// Values may be handles read from any earlier request of this invocation; see
    /// [handle validity](Self#handle-validity).
    pub fn complete(
        mut self,
        values: &mut [Value],
        outcome: Result<u64, HostError>,
    ) -> Result<(), TrapInfo> {
        self.delivering = true;
        let result = self
            .running
            .complete_pending(&self.pending, values, outcome);
        self.delivering = false;
        result
    }
    /// Completes this instruction by raising `tag` with `payload` as a guest exception.
    ///
    /// This is [`Self::complete`] with [`HostError::throw`]: protected regions around the call can
    /// catch it, and a wrong tag, arity or payload kind traps. Aggregate payload values built with
    /// [`Self::context`] are adopted like returned values.
    pub fn throw(self, tag: TagRef, payload: Vec<Value>) -> Result<(), TrapInfo> {
        self.complete(&mut [], Err(HostError::throw(tag, payload)))
    }
    /// Produces and admits an external outcome asynchronously, with cancellation tracking.
    ///
    /// The producer can borrow request inputs across `await`; no argument payload clone or
    /// async runtime dependency is required. Once the future has been polled, abandoning it
    /// marks this delivery unusable: external effects may already have occurred. The invocation
    /// then cancels on drop or reports [`Trap::HostCallAbandoned`] on resumption, preventing
    /// accidental re-dispatch. Dropping an unpolled future leaves the request pending.
    pub async fn complete_with(
        mut self,
        produce: impl AsyncFnOnce(&mut Self) -> Result<(Vec<Value>, u64), HostError>,
    ) -> Result<(), TrapInfo> {
        self.delivering = true;
        let outcome = produce(&mut self).await;
        let (mut values, outcome) = match outcome {
            Ok((values, fuel)) => (values, Ok(fuel)),
            Err(error) => (Vec::new(), Err(error)),
        };
        let result = self
            .running
            .complete_pending(&self.pending, &mut values, outcome);
        self.delivering = false;
        result
    }
    /// Invokes the original host synchronously with its normal borrowed arguments and context.
    ///
    /// This is the fallback for requests the embedding does not handle externally. The
    /// instruction was already admitted: dispatch spends no second instruction/host-call debit.
    pub fn dispatch(mut self) -> Result<(), TrapInfo> {
        self.delivering = true;
        let result = self.running.dispatch_pending(&self.pending);
        self.delivering = false;
        result
    }
}
impl<H: Host> Drop for PendingHostCall<'_, '_, H> {
    fn drop(&mut self) {
        if self.delivering && self.running.continuation.outcome.is_none() {
            let pending = self
                .running
                .continuation
                .control
                .pending_host
                .get_or_insert(self.pending);
            pending.abandoned = true;
        }
    }
}

impl<'vm, H: Host> Invocation<'vm, H> {
    /// Borrows one admitted host request. Only opt-in yielded invocations expose requests.
    ///
    /// The guard prevents resuming or completing another invocation by accident. Repeated
    /// queries and dropped views do not execute the host or replenish hard budgets.
    pub fn pending_host_call(&mut self) -> Option<PendingHostCall<'_, 'vm, H>> {
        self.running().pending_host_call()
    }
}

impl<'request, 'vm, H: Host> Running<'request, 'vm, H> {
    pub(in crate::vm) fn pending_host_call(self) -> Option<PendingHostCall<'request, 'vm, H>> {
        if self.continuation.outcome.is_some() {
            return None;
        }
        let pending = self
            .continuation
            .control
            .pending_host
            .filter(|pending| !pending.abandoned)?;
        Some(PendingHostCall {
            running: self,
            delivering: false,
            pending,
        })
    }

    fn code(&self) -> Code<'_> {
        Code::new(
            &self.vm.units,
            self.continuation.unit,
            &self.continuation.program,
        )
    }

    fn dispatch_pending(&mut self, request: &PendingHost) -> Result<(), TrapInfo> {
        let vm = &mut *self.vm;
        let code = Code::new(
            &vm.units,
            self.continuation.unit,
            &self.continuation.program,
        );
        let site = request.resolve(code);
        let mut inline_args = [ValueRef::Unit; 8];
        let mut arg_vec;
        let args = if site.args.len() <= inline_args.len() {
            &mut inline_args[..site.args.len()]
        } else {
            arg_vec = vec![ValueRef::Unit; site.args.len()];
            &mut arg_vec
        };
        for (slot, value) in args.iter_mut().zip(read_registers(
            &self.continuation.ctx,
            code.sources(&self.continuation.args, vm.memory.aggs()),
            request.base(&self.continuation.ctx),
            site.args,
        )) {
            *slot = value;
        }
        let mut inline_rets: [Value; 8] = core::array::from_fn(|_| Value::Unit);
        let mut ret_vec;
        let values = if site.ret_types.len() <= inline_rets.len() {
            &mut inline_rets[..site.ret_types.len()]
        } else {
            ret_vec = vec![Value::Unit; site.ret_types.len()];
            &mut ret_vec
        };
        let context = HostContext::new(
            vm.units.unit(request.location.unit),
            request.import,
            vm.memory.aggs(),
            self.continuation
                .host_results
                .get_or_insert_with(|| self.budget.host_results()),
            self.continuation
                .access
                .as_mut()
                .map(|sink| &mut **sink as &mut dyn AccessSink),
        );
        let outcome = vm
            .host
            .call(site.symbol, request.signature, args, values, context);
        self.complete_pending(request, values, outcome)
    }

    fn complete_pending(
        &mut self,
        request: &PendingHost,
        values: &mut [Value],
        mut outcome: Result<u64, HostError>,
    ) -> Result<(), TrapInfo> {
        if let Some(callbacks) = &mut self.continuation.control.callbacks {
            callbacks.returned = None;
        }
        let mut results = self
            .continuation
            .host_results
            .take()
            .unwrap_or_else(|| self.budget.host_results());
        let work = results.work;
        let created = match adopted_roots(values, &mut outcome) {
            Some(roots) => results.adopt(self.vm.memory.aggs_mut(), roots, true),
            None => results.adopt(self.vm.memory.aggs_mut(), &mut [], false),
        };
        self.budget.aggregate_work += work.aggregate;
        self.budget.payload_work += work.payload;
        let result = if self.continuation.trace.active() {
            finish_pending::<WithTracePolicy>(
                &mut self.continuation.ctx,
                self.budget,
                Code::new(
                    &self.vm.units,
                    self.continuation.unit,
                    &self.continuation.program,
                ),
                &self.continuation.args,
                self.vm.memory.aggs(),
                request,
                values,
                outcome,
                work,
                created.clone(),
                &mut self.continuation.trace,
                &mut self.continuation.register_state,
            )
        } else {
            finish_pending::<NoTracePolicy>(
                &mut self.continuation.ctx,
                self.budget,
                Code::new(
                    &self.vm.units,
                    self.continuation.unit,
                    &self.continuation.program,
                ),
                &self.continuation.args,
                self.vm.memory.aggs(),
                request,
                values,
                outcome,
                work,
                created.clone(),
                &mut self.continuation.trace,
                &mut (),
            )
        };
        let pending = self
            .continuation
            .control
            .pending_host
            .take()
            .expect("pending guard");
        let result = match result {
            Ok(Some(thrown)) => self.raise_pending(pending, thrown),
            Ok(None) => Ok(()),
            Err(error) => Err(error),
        };
        if let Err(error) = &result {
            self.finish(Err(error.clone()));
        }
        result
    }
    /// Raises a delivered [`HostThrow`] at its pending `host_call`.
    fn raise_pending(&mut self, pending: PendingHost, thrown: HostThrow) -> Result<(), TrapInfo> {
        let prepared = &self.vm.units.unit(pending.location.unit).program;
        let program = prepared.program();
        let span =
            self.continuation
                .ctx
                .span_at(program, pending.location.func, pending.location.pc);
        let continuation = &mut *self.continuation;
        if continuation.trace.active() {
            self.vm.raise_host::<WithTracePolicy>(
                &mut continuation.ctx,
                self.budget,
                prepared,
                thrown,
                &mut continuation.trace,
                &mut continuation.control,
                pending.location,
                span,
            )
        } else {
            self.vm.raise_host::<NoTracePolicy>(
                &mut continuation.ctx,
                self.budget,
                prepared,
                thrown,
                &mut continuation.trace,
                &mut continuation.control,
                pending.location,
                span,
            )
        }
    }

    pub(super) fn finish_abandoned_host(&mut self, pending: PendingHost) -> TrapInfo {
        if let Some(results) = self.continuation.host_results.take() {
            self.budget.aggregate_work += results.work.aggregate;
            self.budget.payload_work += results.work.payload;
        }
        self.continuation.control.pending_host = None;
        self.finish_abandoned(pending.location)
    }

    /// Ends the invocation after a delivery to the waiting instruction at `location` was
    /// abandoned part way, so that it can be neither retried nor continued.
    pub(super) fn finish_abandoned(&mut self, location: TraceLocation) -> TrapInfo {
        let span = self.continuation.ctx.span_at(
            self.vm.units.unit(location.unit).program.program(),
            location.func,
            location.pc,
        );
        let error =
            self.continuation
                .ctx
                .trap(location.func, location.pc, span, Trap::HostCallAbandoned);
        self.vm.memory.rollback(self.checkpoint);
        self.budget.frames -= self.continuation.ctx.held_frames();
        self.budget.contract_scratch.clear_proofs();
        self.continuation.outputs.clear();
        // Install the terminal state before invoking observers, as in normal finish.
        // Unwinding from cancellation must not expose the fallthrough continuation.
        self.continuation.outcome = Some(Err(error.clone()));
        self.continuation
            .trace
            .cancel(self.continuation.program.program(), location, span);
        callback::finish_instructions(
            &mut self.continuation.control,
            &mut self.continuation.trace,
            self.continuation.program.program(),
            TraceOutcome::Cancelled,
        );
        self.continuation
            .trace
            .run_end(self.continuation.program.program(), TraceOutcome::Cancelled);
        error
    }
}

fn finish_pending<P: TracePolicy>(
    ctx: &mut Registers,
    budget: &mut Budget,
    code: Code<'_>,
    root_args: &[Value],
    heap: &AggHeap,
    request: &PendingHost,
    values: &mut [Value],
    outcome: Result<u64, HostError>,
    work: DataWork,
    created: Result<core::ops::Range<u32>, AggError>,
    trace: &mut TraceCtx<'_>,
    register_state: &mut P::RegisterState,
) -> Result<Option<HostThrow>, TrapInfo> {
    let site = request.resolve(code);
    let location = request.location;
    let program = request.program(code).program();
    if P::values_enabled(trace) {
        let args: Vec<_> = read_registers(
            ctx,
            code.sources(root_args, heap),
            request.base(ctx),
            site.args,
        )
        .collect();
        P::values(trace, program, || ValueEvent::HostCall {
            location,
            host_sig: request.import,
            args: &args,
            outcome: observed(code.units, location.unit, values, &outcome),
            aggregates: heap,
            work,
            created: created.as_ref().ok().unwrap_or(&(0..0)),
        });
    }
    created.map_err(|error| {
        ctx.trap(
            location.func,
            location.pc,
            request.span,
            Trap::AggError(error),
        )
    })?;
    if let Err(HostError::Throw(thrown)) = outcome {
        return Ok(Some(*thrown));
    }
    admit_return(
        ctx,
        budget,
        HostReturnSite {
            location,
            span: request.span,
            base: *request.base(ctx),
            symbol: site.symbol,
            signature: request.signature,
            expected: site.ret_types,
            destinations: site.destinations,
        },
        values,
        outcome,
    )?;
    let signature = program.host_sig(request.import).expect("verified import");
    P::host_scope_exit(
        trace,
        program,
        request.import,
        signature.symbol,
        signature.sig_hash,
        ctx.frames.len(),
        location,
        request.span,
    );
    P::registers(
        register_state,
        trace,
        program,
        ctx,
        location,
        *request.base(ctx),
        code.sources(root_args, heap),
        crate::trace::RegisterPhase::Outputs,
    );
    P::instr_end(
        trace,
        program,
        ctx.frames.last().map(|frame| TraceLocation {
            unit: frame.unit,
            func: frame.func,
            pc: frame.pc,
        }),
    );
    Ok(None)
}
