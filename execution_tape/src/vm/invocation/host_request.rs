// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Borrow-scoped delivery of one admitted host instruction.

use super::*;
use crate::host::HostThrow;
use crate::host::SigHash;
use crate::program::HostSigId;
use crate::value::TagRef;
use crate::vm::host_call::{HostReturnSite, PendingHost, admit_return, adopted_roots, observed};

struct Site<'a> {
    pending: PendingHost,
    base: RegBase,
    span: Option<u64>,
    import: HostSigId,
    symbol: &'a str,
    signature: SigHash,
    args: &'a [VReg],
    arg_types: &'a [ValueType],
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

/// The admitted host instruction, read from the program of the unit that admitted it.
fn site<'a>(code: Code<'a>, ctx: &Registers, pending: PendingHost) -> Site<'a> {
    let program = code.of(pending.location.unit);
    let body = program
        .verified(pending.location.func)
        .expect("admitted function");
    let (
        ExecInstr::HostCall {
            host_sig,
            args,
            rets,
        },
        _,
        _,
    ) = body
        .fetch_at_ix(pending.instruction)
        .expect("admitted instruction")
    else {
        unreachable!("pending host instruction")
    };
    let p = program.program();
    let signature = p.host_sig(*host_sig).expect("verified host signature");
    Site {
        pending,
        base: ctx.frames[pending.depth - 1].base,
        span: body.span_at_ix(pending.instruction).map(|span| span.get()),
        import: *host_sig,
        symbol: p
            .symbol_str(signature.symbol)
            .expect("verified host symbol"),
        signature: signature.sig_hash,
        args: body.vregs(*args),
        arg_types: p.host_sig_args(signature).expect("verified argument types"),
        destinations: body.vregs(*rets),
        ret_types: p.host_sig_rets(signature).expect("verified return types"),
    }
}
fn arguments<'a>(
    ctx: &'a Registers,
    sources: ValueSources<'a>,
    base: RegBase,
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
            &ctx.decimals,
            &ctx.bytes,
            &ctx.strs,
            &ctx.objs,
            &ctx.aggs,
            &ctx.funcs,
            &ctx.closures,
            base,
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
pub struct PendingHostCall<'request, 'vm, H: Host> {
    pub(super) running: Running<'request, 'vm, H>,
    pub(super) delivering: bool,
    original: PendingHost,
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
    fn site(&self) -> Site<'_> {
        site(
            Code::new(
                &self.running.vm.units,
                self.running.continuation.unit,
                &self.running.continuation.program,
            ),
            &self.running.continuation.ctx,
            self.running
                .continuation
                .control
                .pending_host
                .expect("pending guard"),
        )
    }
    /// Exact source instruction that admitted this request.
    #[must_use]
    pub fn location(&self) -> TraceLocation {
        self.site().pending.location
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
        self.running.vm.install(program)
    }
    /// Program-local declared import. Its identity belongs to [`Self::program`].
    #[must_use]
    pub fn import(&self) -> HostSigId {
        self.site().import
    }
    /// Exact code whose import and nominal value identities this request uses.
    #[must_use]
    pub fn program(&self) -> &VerifiedProgram {
        &self.running.vm.units.unit(self.unit()).program
    }
    /// Borrows the heap that gives this request's aggregate argument handles their meaning.
    #[must_use]
    pub fn aggregates(&self) -> &AggHeap {
        self.running.vm.memory.aggs()
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
        self.site().symbol
    }
    /// Declared signature hash.
    #[must_use]
    pub fn signature_hash(&self) -> SigHash {
        self.site().signature
    }
    /// Declared positional argument kinds.
    #[must_use]
    pub fn argument_types(&self) -> &[ValueType] {
        self.site().arg_types
    }
    /// Declared positional result kinds.
    #[must_use]
    pub fn return_types(&self) -> &[ValueType] {
        self.site().ret_types
    }
    /// Borrows actual arguments without copying byte/string payloads.
    pub fn arguments(&self) -> impl ExactSizeIterator<Item = ValueRef<'_>> {
        let inv = &self.running;
        let site = self.site();
        arguments(
            &inv.continuation.ctx,
            Code::new(
                &inv.vm.units,
                inv.continuation.unit,
                &inv.continuation.program,
            )
            .sources(&inv.continuation.args, inv.vm.memory.aggs()),
            site.base,
            site.args,
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
        let import = self.import();
        let inv = &mut self.running;
        let unit = inv.vm.units.unit(
            inv.continuation
                .control
                .pending_host
                .expect("pending guard")
                .location
                .unit,
        );
        HostContext::new(
            unit,
            import,
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
    pub fn complete(
        mut self,
        values: &mut [Value],
        outcome: Result<u64, HostError>,
    ) -> Result<(), TrapInfo> {
        self.delivering = true;
        let result = self.running.complete_pending(values, outcome);
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
        let result = self.running.complete_pending(&mut values, outcome);
        self.delivering = false;
        result
    }
    /// Invokes the original host synchronously with its normal borrowed arguments and context.
    ///
    /// This is the fallback for requests the embedding does not handle externally. The
    /// instruction was already admitted: dispatch spends no second instruction/host-call debit.
    pub fn dispatch(mut self) -> Result<(), TrapInfo> {
        self.delivering = true;
        let result = self.running.dispatch_pending();
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
                .get_or_insert(self.original);
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
        self.continuation
            .control
            .pending_host
            .filter(|pending| !pending.abandoned)?;
        Some(PendingHostCall {
            original: self
                .continuation
                .control
                .pending_host
                .expect("pending guard"),
            running: self,
            delivering: false,
        })
    }

    fn dispatch_pending(&mut self) -> Result<(), TrapInfo> {
        let pending = self
            .continuation
            .control
            .pending_host
            .expect("pending guard");
        let vm = &mut *self.vm;
        let code = Code::new(
            &vm.units,
            self.continuation.unit,
            &self.continuation.program,
        );
        let site = site(code, &self.continuation.ctx, pending);
        let mut inline_args = [ValueRef::Unit; 8];
        let mut arg_vec;
        let args = if site.args.len() <= inline_args.len() {
            &mut inline_args[..site.args.len()]
        } else {
            arg_vec = vec![ValueRef::Unit; site.args.len()];
            &mut arg_vec
        };
        for (slot, value) in args.iter_mut().zip(arguments(
            &self.continuation.ctx,
            code.sources(&self.continuation.args, vm.memory.aggs()),
            site.base,
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
            vm.units.unit(site.pending.location.unit),
            site.import,
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
            .call(site.symbol, site.signature, args, values, context);
        self.complete_pending(values, outcome)
    }

    fn complete_pending(
        &mut self,
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
                self.continuation
                    .control
                    .pending_host
                    .expect("pending guard"),
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
                self.continuation
                    .control
                    .pending_host
                    .expect("pending guard"),
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
        let span = self.continuation.ctx.span_at(
            self.vm.units.unit(pending.location.unit).program.program(),
            pending.location.func,
            pending.location.pc,
        );
        let error = self.continuation.ctx.trap(
            pending.location.func,
            pending.location.pc,
            span,
            Trap::HostCallAbandoned,
        );
        self.vm.memory.rollback(self.checkpoint);
        self.budget.frames -= self.continuation.ctx.frames.len();
        self.budget.contract_scratch.clear_proofs();
        self.continuation.outputs.clear();
        self.continuation.control.pending_host = None;
        // Install the terminal state before invoking observers, as in normal finish.
        // Unwinding from cancellation must not expose the fallthrough continuation.
        self.continuation.outcome = Some(Err(error.clone()));
        self.continuation
            .trace
            .cancel(self.continuation.program.program(), pending.location, span);
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
    pending: PendingHost,
    values: &mut [Value],
    outcome: Result<u64, HostError>,
    work: DataWork,
    created: Result<core::ops::Range<u32>, AggError>,
    trace: &mut TraceCtx<'_>,
    register_state: &mut P::RegisterState,
) -> Result<Option<HostThrow>, TrapInfo> {
    let site = site(code, ctx, pending);
    let program = code.of(pending.location.unit).program();
    if P::values_enabled(trace) {
        let args: Vec<_> =
            arguments(ctx, code.sources(root_args, heap), site.base, site.args).collect();
        P::values(trace, program, || ValueEvent::HostCall {
            location: site.pending.location,
            host_sig: site.import,
            args: &args,
            outcome: observed(code.units, site.pending.location.unit, values, &outcome),
            aggregates: heap,
            work,
            created: created.as_ref().ok().unwrap_or(&(0..0)),
        });
    }
    created.map_err(|error| {
        ctx.trap(
            site.pending.location.func,
            site.pending.location.pc,
            site.span,
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
            location: site.pending.location,
            span: site.span,
            base: site.base,
            symbol: site.symbol,
            signature: site.signature,
            expected: site.ret_types,
            destinations: site.destinations,
        },
        values,
        outcome,
    )?;
    let signature = program.host_sig(site.import).expect("verified import");
    P::host_scope_exit(
        trace,
        program,
        site.import,
        signature.symbol,
        signature.sig_hash,
        ctx.frames.len(),
        site.pending.location.func,
        site.pending.location.pc,
        site.span,
    );
    P::registers(
        register_state,
        trace,
        program,
        ctx,
        site.pending.location,
        site.base,
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
