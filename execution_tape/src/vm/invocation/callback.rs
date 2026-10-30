// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Same-invocation guest reentry. Native stacks and scheduling stay with the embedding.

use super::*;
use crate::vm::host_call::PendingHost;

#[derive(Default)]
pub(in crate::vm) struct Callbacks {
    waiting: Vec<WaitingHost>,
    pub(in crate::vm) returned: Option<Vec<Value>>,
}
struct WaitingHost {
    host: PendingHost,
    instruction: bool,
}

impl<H: Host> PendingHostCall<'_, '_, H> {
    /// Starts a same-program guest function or closure while retaining this host request.
    ///
    /// Consumes the view, not the host operation. Drive the same invocation with `resume`:
    /// the callback may yield further host requests, whose guards support the same operation.
    /// When the callback returns, `resume` reports `AwaitingHost` for this original request.
    /// Take its callback results, then complete the host operation explicitly. Repeated calls
    /// discard any untaken previous callback results.
    ///
    /// The target and all raw argument handles belong to this invocation's code/heap domain;
    /// this does not import a foreign callable. Closure arguments exclude the hidden environment.
    /// Arguments move into the invocation. Their immediate byte/string lengths are charged as
    /// host ingress; result materialization uses the ordinary payload-work budget. Each callback
    /// consumes one hard-fuel unit and shares call-depth, host-call and contract limits with its
    /// suspended callers. There is no budget reset or native Rust stack capture.
    ///
    /// Previously staged host aggregates are promoted, preserving handles, before guest reentry.
    /// Even unused staged nodes then live until root commit or rollback; their already admitted
    /// work remains charged. Later host construction uses the updated shared heap/budgets.
    /// Invalid targets, arguments or exhausted limits terminate the whole invocation, as guest
    /// calls do. Dropping it cancels all waiting operations without executing guest code.
    ///
    /// Callback entry/return and actual guest ancestry are observable. The boundary recorder
    /// currently marks callback captures explicitly incomplete; checked replay refuses them.
    pub fn call_guest(mut self, callee: Value, args: Vec<Value>) -> Result<(), TrapInfo> {
        self.delivering = true;
        let result = self.invocation.begin_callback(callee, args);
        self.delivering = false;
        result
    }

    /// Borrows the latest successful callback results without consuming them.
    ///
    /// An empty successful callback yields `Some(&[])`; `None` means no results are available.
    /// Handles use this request's original heap/code. Inspection spends no guest work.
    #[must_use]
    pub fn callback_results(&self) -> Option<&[Value]> {
        self.invocation
            .control
            .callbacks
            .as_ref()?
            .returned
            .as_deref()
    }

    /// Takes the latest successful callback results belonging to this pending host operation.
    ///
    /// `None` means no callback has returned or the slots were already taken. The host operation
    /// is still pending; result handles retain this invocation's original heap/code domains.
    /// Taking values does not retain them independently. Completing the host operation discards
    /// untaken slots. Use the request context's `retain` for an independent aggregate snapshot.
    pub fn take_callback_results(&mut self) -> Option<Vec<Value>> {
        self.invocation.control.callbacks.as_mut()?.returned.take()
    }
}

impl<H: Host> Invocation<'_, H> {
    fn begin_callback(&mut self, callee: Value, args: Vec<Value>) -> Result<(), TrapInfo> {
        let pending = self.control.pending_host.expect("pending host guard");
        let span = self.ctx.span_at(
            self.program.program(),
            pending.location.func,
            pending.location.pc,
        );
        let result = self.prepare_callback(pending, &callee, args);
        match result {
            Ok(()) => Ok(()),
            Err(trap) => {
                self.control.pending_host = None;
                let error = self
                    .ctx
                    .trap(pending.location.func, pending.location.pc, span, trap);
                self.finish(Err(error.clone()));
                Err(error)
            }
        }
    }

    fn prepare_callback(
        &mut self,
        pending: PendingHost,
        callee: &Value,
        mut args: Vec<Value>,
    ) -> Result<(), Trap> {
        debug_assert_eq!(
            pending.depth,
            self.ctx.frames.len(),
            "only the active host request can enter a callback"
        );
        let mut staging = self
            .host_results
            .take()
            .unwrap_or_else(|| self.ctx.host_results());
        let work = staging.work;
        self.ctx.aggregate_work += work.aggregate;
        self.ctx.payload_work += work.payload;
        let created = staging
            .promote(self.vm.memory.aggs_mut())
            .map_err(Trap::AggError)?;
        self.trace
            .values(self.program.program(), || ValueEvent::CallbackEnter {
                location: pending.location,
                callee,
                args: &args,
                work,
                created: &created,
                aggregates: self.vm.memory.aggs(),
            });
        if self.ctx.fuel == 0 {
            return Err(Trap::FuelExceeded);
        }
        self.ctx.fuel -= 1;
        if self.ctx.frames.len() >= self.vm.limits.max_call_depth {
            return Err(Trap::CallDepthExceeded);
        }
        let (function, environment) = match callee {
            Value::Func(function) => (*function, None),
            Value::Closure(closure) => (closure.func, Some(closure.env)),
            value => {
                return Err(Trap::TypeMismatch {
                    expected: ValueType::Func,
                    actual: value.value_type(),
                });
            }
        };
        let program = self.program.program();
        let body = program
            .functions
            .get(function.0 as usize)
            .ok_or(Trap::InvalidPc)?;
        if let Some(environment) = environment {
            if body.arg_types(program).map_err(|_| Trap::InvalidPc)?.get(0) != Some(&ValueType::Agg)
            {
                return Err(Trap::ArityMismatch);
            }
            args.insert(0, Value::Agg(environment));
        }
        validate_entry_args(program, body, &args)?;
        let payload = args.iter().try_fold(0_usize, |sum, value| {
            let bytes = match value {
                Value::Bytes(bytes) => bytes.len(),
                Value::Str(text) => text.len(),
                _ => 0,
            };
            sum.checked_add(bytes).ok_or(Trap::PayloadSizeOverflow)
        })?;
        self.ctx.charge_payload_bytes(payload, 1)?;
        let prepared = self.program.verified(function).ok_or(Trap::InvalidPc)?;
        let base = self.ctx.alloc_frame(prepared);
        for (register, value) in prepared.reg_layout.arg_regs.iter().copied().zip(args) {
            self.ctx.write_owned_value(base, register, value)?;
        }
        self.vm
            .check_arg_contracts(&mut self.ctx, program, function, prepared, base)?;
        if let Some(environment) = environment {
            // An opaque Agg contract must still resolve a closure's environment before entry.
            let contract = program.verified_contracts(body.arg_types)[0];
            if matches!(program.contracts.get(contract), Some(Contract::Kind(_))) {
                self.ctx
                    .contract_scratch
                    .check_value(
                        program,
                        self.vm.memory.aggs(),
                        &Value::Agg(environment),
                        contract,
                        self.vm.limits.max_contract_work,
                    )
                    .map_err(|error| {
                        self.vm.contract_trap(
                            function,
                            ContractPosition::Argument {
                                callee: function,
                                index: 0,
                            },
                            error,
                        )
                    })?;
            }
        }
        self.ctx.frames.push(Frame {
            func: function,
            pc: prepared.pc_at_ix(0).unwrap_or(0),
            instr_ix: 0,
            byte_len: prepared.byte_len,
            base,
            return_to: None,
        });
        self.control.enter(pending.location);
        let callbacks = self
            .control
            .callbacks
            .get_or_insert_with(|| Box::new(Callbacks::default()));
        callbacks.returned = None;
        let instruction = self.trace.pending.take();
        debug_assert!(
            instruction.is_none_or(|location| location == pending.location),
            "the outstanding instruction must be this host call"
        );
        callbacks.waiting.push(WaitingHost {
            host: pending,
            instruction: instruction.is_some(),
        });
        self.control.pending_host = None;
        self.trace.call_scope_enter_if_enabled(
            program,
            self.ctx.frames.len(),
            function,
            prepared.pc_at_ix(0).unwrap_or(0),
            prepared.span_at_ix(0).map(|span| span.get()),
            Some(pending.location),
        );
        Ok(())
    }
}

#[inline]
pub(in crate::vm) fn returns_to_host(control: &ExecutionControl, depth: usize) -> bool {
    control
        .callbacks
        .as_ref()
        .and_then(|callbacks| callbacks.waiting.last())
        .is_some_and(|waiting| waiting.host.depth + 1 == depth)
}

pub(in crate::vm) fn return_to_host<H: Host, P: TracePolicy>(
    vm: &mut Vm<H>,
    ctx: &mut ExecutionContext,
    prepared: &VerifiedProgram,
    root_args: &[Value],
    control: &mut ExecutionControl,
    trace: &mut TraceCtx<'_>,
    body: &ExecFunc,
    rets: &[VReg],
    location: TraceLocation,
    span: Option<u64>,
    base: RegBase,
) -> Result<(), TrapInfo> {
    let program = prepared.program();
    vm.check_result_contracts(ctx, program, location.func, body, base, rets)
        .map_err(|trap| ctx.trap(location.func, location.pc, span, trap))?;
    let mut values = Vec::with_capacity(rets.len());
    for &reg in rets {
        values.push(
            ctx.materialize_vreg(
                base,
                reg,
                ValueSources::new(program, root_args, vm.memory.aggs()),
            )
            .map_err(|trap| ctx.trap(location.func, location.pc, span, trap))?,
        );
    }
    let callbacks = control.callbacks.as_mut().expect("callback root");
    let waiting = callbacks.waiting.last().expect("waiting host");
    P::values(trace, program, || ValueEvent::CallbackReturn {
        location,
        caller: waiting.host.location,
        values: &values,
        aggregates: vm.memory.aggs(),
    });
    let waiting = callbacks.waiting.pop().expect("waiting host");
    callbacks.returned = Some(values);
    let depth = ctx.frames.len();
    ctx.frames.pop().expect("callback root frame");
    ctx.truncate_to(base);
    control.leave();
    control.pending_host = Some(waiting.host);
    P::call_scope_exit(trace, program, depth, location.func, location.pc, span);
    // The guest returns to embedding code. Its parent host instruction is still pending;
    // pointing at that instruction would falsely imply that it executes a second time.
    P::instr_end(trace, program, None);
    trace.pending = waiting.instruction.then_some(waiting.host.location);
    Ok(())
}

pub(in crate::vm) fn finish_instructions(
    control: &mut ExecutionControl,
    trace: &mut TraceCtx<'_>,
    program: &Program,
    outcome: TraceOutcome<'_>,
) {
    if let Some(callbacks) = &mut control.callbacks {
        // Parent host attempts remained open while callbacks ran. Close each explicitly even
        // on trap/cancellation; a child returning never fabricates a successful host outcome.
        while let Some(waiting) = callbacks.waiting.pop() {
            trace.pending = waiting.instruction.then_some(waiting.host.location);
            trace.instr_end(program, None, outcome);
        }
        callbacks.returned = None;
    }
}
