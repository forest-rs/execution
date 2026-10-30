// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Borrow-scoped delivery of one admitted host instruction.

use super::*;
use crate::host::SigHash;
use crate::program::HostSigId;
use crate::vm::host_call::{HostReturnSite, PendingHost, admit_return};

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
fn site<'a>(
    program: &'a VerifiedProgram,
    ctx: &ExecutionContext,
    pending: PendingHost,
) -> Site<'a> {
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
    ctx: &'a ExecutionContext,
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
    pub(super) invocation: &'request mut Invocation<'vm, H>,
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
            &self.invocation.program,
            &self.invocation.ctx,
            self.invocation.control.pending_host.expect("pending guard"),
        )
    }
    /// Exact source instruction that admitted this request.
    #[must_use]
    pub fn location(&self) -> TraceLocation {
        self.site().pending.location
    }
    /// Program-local declared import. Its identity belongs to [`Self::program`].
    #[must_use]
    pub fn import(&self) -> HostSigId {
        self.site().import
    }
    /// Exact code whose import and nominal value identities this request uses.
    #[must_use]
    pub fn program(&self) -> &VerifiedProgram {
        &self.invocation.program
    }
    /// Borrows the heap that gives this request's aggregate argument handles their meaning.
    #[must_use]
    pub fn aggregates(&self) -> &AggHeap {
        self.invocation.vm.memory.aggs()
    }
    /// Borrows the actual caller's frames and initialized registers while this request waits.
    ///
    /// Analysis is lazy and read-only, as with [`Invocation::inspect`]. The pending call is
    /// shown before its result, preserving uninitialized destinations as unavailable.
    #[must_use]
    pub fn inspect(&self) -> InvocationInspection<'_> {
        self.invocation.inspect()
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
        let inv = &self.invocation;
        let site = self.site();
        arguments(
            &inv.ctx,
            ValueSources::new(inv.program.program(), &inv.args, inv.vm.memory.aggs()),
            site.base,
            site.args,
        )
    }
    pub(crate) fn restore_host_work(&mut self, work: DataWork) -> Result<(), Trap> {
        let inv = &mut self.invocation;
        inv.host_results
            .get_or_insert_with(|| inv.ctx.host_results())
            .prepaid(work)
    }
    pub(crate) fn restore_host_node(
        &mut self,
        kind: crate::value::AggType,
        values: Vec<Value>,
    ) -> Result<AggHandle, AggError> {
        let inv = &mut self.invocation;
        inv.host_results
            .get_or_insert_with(|| inv.ctx.host_results())
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
        let inv = &mut self.invocation;
        HostContext::new(
            inv.program.program(),
            import,
            inv.vm.memory.aggs(),
            inv.host_results
                .get_or_insert_with(|| inv.ctx.host_results()),
            inv.access
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
        let result = self.invocation.complete_pending(values, outcome);
        self.delivering = false;
        result
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
        let result = self.invocation.complete_pending(&mut values, outcome);
        self.delivering = false;
        result
    }
    /// Invokes the original host synchronously with its normal borrowed arguments and context.
    ///
    /// This is the fallback for requests the embedding does not handle externally. The
    /// instruction was already admitted: dispatch spends no second instruction/host-call debit.
    pub fn dispatch(mut self) -> Result<(), TrapInfo> {
        self.delivering = true;
        let result = self.invocation.dispatch_pending();
        self.delivering = false;
        result
    }
}
impl<H: Host> Drop for PendingHostCall<'_, '_, H> {
    fn drop(&mut self) {
        if self.delivering && self.invocation.outcome.is_none() {
            let pending = self
                .invocation
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
        if self.outcome.is_some() {
            return None;
        }
        self.control
            .pending_host
            .filter(|pending| !pending.abandoned)?;
        Some(PendingHostCall {
            original: self.control.pending_host.expect("pending guard"),
            invocation: self,
            delivering: false,
        })
    }

    fn dispatch_pending(&mut self) -> Result<(), TrapInfo> {
        let pending = self.control.pending_host.expect("pending guard");
        let site = site(&self.program, &self.ctx, pending);
        let vm = &mut *self.vm;
        let mut inline_args = [ValueRef::Unit; 8];
        let mut arg_vec;
        let args = if site.args.len() <= inline_args.len() {
            &mut inline_args[..site.args.len()]
        } else {
            arg_vec = vec![ValueRef::Unit; site.args.len()];
            &mut arg_vec
        };
        for (slot, value) in args.iter_mut().zip(arguments(
            &self.ctx,
            ValueSources::new(self.program.program(), &self.args, vm.memory.aggs()),
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
            self.program.program(),
            site.import,
            vm.memory.aggs(),
            self.host_results
                .get_or_insert_with(|| self.ctx.host_results()),
            self.access
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
        outcome: Result<u64, HostError>,
    ) -> Result<(), TrapInfo> {
        if let Some(callbacks) = &mut self.control.callbacks {
            callbacks.returned = None;
        }
        let mut results = self
            .host_results
            .take()
            .unwrap_or_else(|| self.ctx.host_results());
        let work = results.work;
        let created = results.adopt(self.vm.memory.aggs_mut(), values, outcome.is_ok());
        self.ctx.aggregate_work += work.aggregate;
        self.ctx.payload_work += work.payload;
        let result = if self.trace.active() {
            finish_pending::<WithTracePolicy>(
                &mut self.ctx,
                &self.program,
                &self.args,
                self.vm.memory.aggs(),
                self.control.pending_host.expect("pending guard"),
                values,
                outcome,
                work,
                created.clone(),
                &mut self.trace,
                &mut self.register_state,
            )
        } else {
            finish_pending::<NoTracePolicy>(
                &mut self.ctx,
                &self.program,
                &self.args,
                self.vm.memory.aggs(),
                self.control.pending_host.expect("pending guard"),
                values,
                outcome,
                work,
                created.clone(),
                &mut self.trace,
                &mut (),
            )
        };
        self.control.pending_host = None;
        if let Err(error) = &result {
            self.finish(Err(error.clone()));
        }
        result
    }
    pub(super) fn finish_abandoned_host(&mut self, pending: PendingHost) -> TrapInfo {
        if let Some(results) = self.host_results.take() {
            self.ctx.aggregate_work += results.work.aggregate;
            self.ctx.payload_work += results.work.payload;
        }
        let span = self.ctx.span_at(
            self.program.program(),
            pending.location.func,
            pending.location.pc,
        );
        let error = self.ctx.trap(
            pending.location.func,
            pending.location.pc,
            span,
            Trap::HostCallAbandoned,
        );
        self.vm.memory.rollback(self.checkpoint);
        self.ctx.contract_scratch.clear_proofs();
        self.outputs.clear();
        self.control.pending_host = None;
        // Install the terminal state before invoking observers, as in normal finish.
        // Unwinding from cancellation must not expose the fallthrough continuation.
        self.outcome = Some(Err(error.clone()));
        self.trace
            .cancel(self.program.program(), pending.location, span);
        callback::finish_instructions(
            &mut self.control,
            &mut self.trace,
            self.program.program(),
            TraceOutcome::Cancelled,
        );
        self.trace
            .run_end(self.program.program(), TraceOutcome::Cancelled);
        error
    }
}

fn finish_pending<P: TracePolicy>(
    ctx: &mut ExecutionContext,
    prepared: &VerifiedProgram,
    root_args: &[Value],
    heap: &AggHeap,
    pending: PendingHost,
    values: &mut [Value],
    outcome: Result<u64, HostError>,
    work: DataWork,
    created: Result<core::ops::Range<u32>, AggError>,
    trace: &mut TraceCtx<'_>,
    register_state: &mut P::RegisterState,
) -> Result<(), TrapInfo> {
    let site = site(prepared, ctx, pending);
    let program = prepared.program();
    if P::values_enabled(trace) {
        let args: Vec<_> = arguments(
            ctx,
            ValueSources::new(program, root_args, heap),
            site.base,
            site.args,
        )
        .collect();
        P::values(trace, program, || ValueEvent::HostCall {
            location: site.pending.location,
            host_sig: site.import,
            args: &args,
            outcome: match &outcome {
                Ok(fuel) => HostCallOutcome::Returned {
                    values,
                    extra_fuel: *fuel,
                },
                Err(error) => HostCallOutcome::Failed(error),
            },
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
    admit_return(
        ctx,
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
        site.pending.location.func,
        site.pending.location.pc,
        site.base,
        ValueSources::new(program, root_args, heap),
        crate::trace::RegisterPhase::Outputs,
    );
    P::instr_end(
        trace,
        program,
        ctx.frames.last().map(|frame| TraceLocation {
            func: frame.func,
            pc: frame.pc,
        }),
    );
    Ok(())
}
