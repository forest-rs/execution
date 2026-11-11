// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Raising typed exceptions.
//!
//! `throw` materializes its payload, checks the tag's payload contracts and then unwinds to the
//! innermost catching clause of a protected region. A `*_ref` clause also binds a shared
//! reference to the exception, which `throw_ref` propagates again with its original tag, payload
//! and throw site. An exception no clause catches leaves the invocation as [`Trap::Uncaught`];
//! its payload is detached into an owned [`ValueSnapshot`] before the failed invocation rolls
//! back its aggregates, so the embedding can inspect it.

use super::*;
use crate::aggregates::ValueSnapshot;
use crate::host::HostThrow;
use crate::program::TagId;
use crate::typed::ExecCatch;

/// A guest exception that no protected region caught before it left the invocation.
///
/// The payload is an independent snapshot: aggregate handles refer to
/// [`ValueSnapshot::aggregates`], not to the VM heap, which discards the failed invocation's
/// allocations. Host objects remain host-owned handles; function IDs still refer to the
/// throwing program.
#[derive(Clone, Debug)]
pub struct UncaughtException {
    tag: TagId,
    name: Box<str>,
    payload: ValueSnapshot,
    origin: TraceLocation,
}

impl UncaughtException {
    /// Program-local identity of the thrown tag.
    #[must_use]
    pub fn local_tag(&self) -> TagId {
        self.tag
    }

    /// The thrown tag's declared name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Payload values in tag order, with their reachable aggregate graph.
    #[must_use]
    pub fn payload(&self) -> &ValueSnapshot {
        &self.payload
    }

    /// The original `throw`. A rethrown exception keeps it; the trap reports the last raise.
    #[must_use]
    pub fn origin(&self) -> TraceLocation {
        self.origin
    }
}

impl fmt::Display for UncaughtException {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "uncaught exception tag#{} (\"{}\") with {} payload values",
            self.tag.0,
            self.name,
            self.payload.values().len()
        )
    }
}

/// A raised exception: its tag, checked payload and original throw site.
///
/// Exception reference registers share one record; rethrowing never copies or rechecks it.
#[derive(Debug)]
pub(crate) struct Exception {
    tag: TagId,
    payload: Vec<Value>,
    origin: TraceLocation,
    /// Invocation-local raise ordinal, linking rethrows to the original in evidence.
    ordinal: usize,
}

impl<H: Host> Vm<H> {
    /// Raises `tag` at the throwing instruction, then propagates it.
    ///
    /// Payload materialization, observation and contract checks happen at the throw site, so a
    /// rejected payload reports its own trap there rather than an exception.
    #[allow(
        clippy::too_many_arguments,
        reason = "mirrors the interpreter state borrowed by other instruction helpers"
    )]
    pub(super) fn raise<P: TracePolicy>(
        &self,
        ctx: &mut Registers,
        budget: &mut Budget,
        prepared: &VerifiedProgram,
        tag: TagId,
        payload: &[VReg],
        base: RegBase,
        sources: ValueSources<'_>,
        trace: &mut TraceCtx<'_>,
        control: &mut ExecutionControl,
        location: TraceLocation,
        span_id: Option<u64>,
    ) -> Result<(), TrapInfo> {
        let fail = |ctx: &Registers, trap| ctx.trap(location.func, location.pc, span_id, trap);
        let mut values = Vec::with_capacity(payload.len());
        for &register in payload {
            match ctx.materialize_vreg(budget, base, register, sources) {
                Ok(value) => values.push(value),
                Err(trap) => return Err(fail(ctx, trap)),
            }
        }
        self.raise_values::<P>(
            ctx, budget, prepared, tag, values, trace, control, location, span_id,
        )
    }

    /// Raises a host's [`HostThrow`] at its `host_call`.
    ///
    /// The tag must belong to the calling program and the payload must match its declared
    /// kinds; otherwise the call traps, as an invalid host return does. Payload bytes are charged
    /// as host ingress before the exception is observed and checked like a guest `throw`.
    #[allow(
        clippy::too_many_arguments,
        reason = "mirrors the interpreter state borrowed by other instruction helpers"
    )]
    pub(super) fn raise_host<P: TracePolicy>(
        &self,
        ctx: &mut Registers,
        budget: &mut Budget,
        prepared: &VerifiedProgram,
        thrown: HostThrow,
        trace: &mut TraceCtx<'_>,
        control: &mut ExecutionControl,
        location: TraceLocation,
        span_id: Option<u64>,
    ) -> Result<(), TrapInfo> {
        let program = prepared.program();
        let fail = |ctx: &Registers, trap| ctx.trap(location.func, location.pc, span_id, trap);
        let Some(entry) = program.tag(thrown.tag) else {
            return Err(fail(ctx, Trap::TagOutOfBounds { tag: thrown.tag }));
        };
        let kinds = program.tag_payload(entry).expect("verified tag payload");
        if kinds.len() != thrown.payload.len() {
            return Err(fail(ctx, Trap::ArityMismatch));
        }
        let mut bytes = 0_usize;
        for (value, &kind) in thrown.payload.iter().zip(kinds.iter()) {
            value.check_type(kind).map_err(|trap| fail(ctx, trap))?;
            bytes = bytes.saturating_add(match value {
                Value::Bytes(bytes) => bytes.len(),
                Value::Str(text) => text.len(),
                _ => 0,
            });
        }
        budget
            .charge_payload_bytes(bytes, 1)
            .map_err(|trap| fail(ctx, trap))?;
        self.raise_values::<P>(
            ctx,
            budget,
            prepared,
            thrown.tag,
            thrown.payload,
            trace,
            control,
            location,
            span_id,
        )
    }

    /// Observes and checks a materialized payload, then propagates the new exception.
    #[allow(
        clippy::too_many_arguments,
        reason = "mirrors the interpreter state borrowed by other instruction helpers"
    )]
    fn raise_values<P: TracePolicy>(
        &self,
        ctx: &mut Registers,
        budget: &mut Budget,
        prepared: &VerifiedProgram,
        tag: TagId,
        values: Vec<Value>,
        trace: &mut TraceCtx<'_>,
        control: &mut ExecutionControl,
        location: TraceLocation,
        span_id: Option<u64>,
    ) -> Result<(), TrapInfo> {
        let program = prepared.program();
        let fail = |ctx: &Registers, trap| ctx.trap(location.func, location.pc, span_id, trap);
        P::values(trace, program, || ValueEvent::Thrown {
            location,
            tag,
            values: &values,
            aggregates: self.memory.aggs(),
        });
        let entry = program.tag(tag).expect("verified tag");
        let contracts = program
            .tag_payload_contracts(entry)
            .expect("verified tag payload");
        for (index, (value, &contract)) in values.iter().zip(contracts).enumerate() {
            if matches!(program.contracts.get(contract), Some(Contract::Kind(_))) {
                continue;
            }
            if let Err(error) = budget.contract_scratch.check_value(
                program,
                self.memory.aggs(),
                value,
                contract,
                self.limits.max_contract_work,
            ) {
                let position = ContractPosition::Payload {
                    tag,
                    index: u32::try_from(index).expect("verified payload arity"),
                };
                return Err(fail(
                    ctx,
                    self.contract_trap(location.func, position, error),
                ));
            }
        }
        let exception = Arc::new(Exception {
            tag,
            payload: values,
            origin: location,
            ordinal: ctx.raised,
        });
        ctx.raised += 1;
        self.propagate::<P>(
            ctx, budget, prepared, exception, trace, control, location, span_id,
        )
    }

    /// Raises a caught exception again at `location`, without rechecking its payload.
    #[allow(
        clippy::too_many_arguments,
        reason = "mirrors the interpreter state borrowed by other instruction helpers"
    )]
    pub(super) fn rethrow<P: TracePolicy>(
        &self,
        ctx: &mut Registers,
        budget: &mut Budget,
        prepared: &VerifiedProgram,
        exception: Arc<Exception>,
        trace: &mut TraceCtx<'_>,
        control: &mut ExecutionControl,
        location: TraceLocation,
        span_id: Option<u64>,
    ) -> Result<(), TrapInfo> {
        P::values(trace, prepared.program(), || ValueEvent::Rethrown {
            location,
            tag: exception.tag,
            exception: exception.ordinal,
            origin: exception.origin,
        });
        self.propagate::<P>(
            ctx, budget, prepared, exception, trace, control, location, span_id,
        )
    }

    /// Unwinds `exception`, raised at `location`, to the innermost catching clause.
    ///
    /// The search starts at the raising activation and continues outward. A catch abandons the
    /// activations and pending host requests it passes through, and execution continues at the
    /// handler: `Ok(())`. With no catching clause, the returned trap ends the invocation with a
    /// detached payload snapshot.
    #[allow(
        clippy::too_many_arguments,
        reason = "mirrors the interpreter state borrowed by other instruction helpers"
    )]
    fn propagate<P: TracePolicy>(
        &self,
        ctx: &mut Registers,
        budget: &mut Budget,
        prepared: &VerifiedProgram,
        exception: Arc<Exception>,
        trace: &mut TraceCtx<'_>,
        control: &mut ExecutionControl,
        location: TraceLocation,
        span_id: Option<u64>,
    ) -> Result<(), TrapInfo> {
        let program = prepared.program();
        let tag = exception.tag;
        if let Some((frame, catch)) = find_catch(ctx, prepared, tag) {
            return self.catch::<P>(
                ctx, budget, prepared, exception, trace, control, location, span_id, frame, catch,
            );
        }
        let fail = |ctx: &Registers, trap| ctx.trap(location.func, location.pc, span_id, trap);
        let payload = match self.memory.aggs().snapshot(&exception.payload) {
            Ok(payload) => payload,
            Err(error) => return Err(fail(ctx, Trap::AggError(error))),
        };
        let name = program.tag_name(tag).unwrap_or_default().into();
        Err(fail(
            ctx,
            Trap::Uncaught(Box::new(UncaughtException {
                tag,
                name,
                payload,
                origin: exception.origin,
            })),
        ))
    }

    /// Unwinds to `frame` and enters `catch`'s handler with the payload.
    #[allow(
        clippy::too_many_arguments,
        reason = "continues the raise with the same interpreter state"
    )]
    fn catch<P: TracePolicy>(
        &self,
        ctx: &mut Registers,
        budget: &mut Budget,
        prepared: &VerifiedProgram,
        exception: Arc<Exception>,
        trace: &mut TraceCtx<'_>,
        control: &mut ExecutionControl,
        location: TraceLocation,
        span_id: Option<u64>,
        frame: usize,
        catch: ExecCatch,
    ) -> Result<(), TrapInfo> {
        let program = prepared.program();
        let tag = exception.tag;
        let catcher = &ctx.frames[frame];
        let body = prepared
            .verified(catcher.func)
            .expect("active verified function");
        let target_ix = catch.handler.target_ix as usize;
        let handler = TraceLocation {
            func: catcher.func,
            pc: body.pc_at_ix(target_ix).expect("verified handler entry"),
        };
        let depth = frame + 1;
        let unwound_callbacks = control
            .callbacks
            .as_ref()
            .map_or(0, |callbacks| callbacks.count_exited(depth));
        P::values(trace, program, || ValueEvent::Caught {
            location,
            tag,
            handler,
            depth,
            region: catch.region,
            clause: catch.clause,
            unwound_callbacks,
        });
        let outcome = TraceOutcome::Caught { depth };
        // Close a raising host call's scope and the raising instruction first, then every
        // abandoned activation and pending host request, innermost first, while their frames
        // and banks are still intact.
        trace.exit_host(program, location, span_id, outcome);
        trace.instr_end(program, Some(handler), outcome);
        for exited in (depth + 1..=ctx.frames.len()).rev() {
            trace.exit_frame(program, exited, location, span_id, outcome);
            let waiting = control
                .callbacks
                .as_ref()
                .and_then(|callbacks| callbacks.waiting_at(exited - 1));
            if let Some((host, instruction)) = waiting {
                trace.pending = instruction.then_some(host.location);
                let span = ctx.span_at(program, host.location.func, host.location.pc);
                trace.exit_host(program, host.location, span, outcome);
                trace.instr_end(program, None, outcome);
            }
        }
        if let Some(first) = ctx.frames.get(depth) {
            let base = first.base;
            budget.frames -= ctx.frames.len() - depth;
            ctx.frames.truncate(depth);
            ctx.truncate_to(base);
        }
        control.truncate(depth);
        if let Some(callbacks) = &mut control.callbacks {
            callbacks.discard_exited(depth);
        }
        let catcher = ctx.frames.last_mut().expect("catching activation");
        catcher.instr_ix = target_ix;
        catcher.pc = handler.pc;
        let base = catcher.base;
        let params = body.vregs(catch.handler.params);
        let (payload_params, reference) = if catch.handler.binds_exception {
            let (last, payload) = params.split_last().expect("verified exception parameter");
            (payload, Some(*last))
        } else {
            (params, None)
        };
        for (&param, value) in payload_params.iter().zip(&exception.payload) {
            ctx.write_owned_value(base, param, value.clone())
                .map_err(|trap| ctx.trap(location.func, location.pc, span_id, trap))?;
        }
        if let Some(VReg::Exn(reference)) = reference {
            ctx.write_exn(base, reference, exception);
        }
        Ok(())
    }
}

// Scope exits for activations and host requests that a catch abandons.
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

/// Finds the innermost activation whose protected regions catch `tag` at its raising instruction.
///
/// The leaf raised with `throw`; every other activation is suspended at the call or host call
/// that entered the activation above it. Each frame has already advanced past that instruction.
fn find_catch(
    ctx: &Registers,
    prepared: &VerifiedProgram,
    tag: TagId,
) -> Option<(usize, ExecCatch)> {
    ctx.frames
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, frame)| {
            let protection = prepared.verified(frame.func)?.protection.as_deref()?;
            let raising = frame.instr_ix.checked_sub(1)?;
            Some((index, protection.catch(raising, tag)?))
        })
}

// The pinned sizes are the 64-bit layout; 32-bit targets such as wasm32 lay `Value` out
// smaller, so pinning them there would test pointer width rather than `Exn`.
#[cfg(all(test, target_pointer_width = "64"))]
mod tests {
    use super::*;

    #[test]
    fn exception_references_add_no_boundary_value_representation() {
        // `Exn` lives only in guest registers; boundary values and kinds keep their size.
        assert_eq!(size_of::<Value>(), 32);
        assert_eq!(size_of::<ValueType>(), 16);
    }
}
