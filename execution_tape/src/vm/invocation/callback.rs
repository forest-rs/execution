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

impl Callbacks {
    pub(super) fn count_exited(&self, first: usize) -> usize {
        self.waiting
            .iter()
            .rev()
            .take_while(|waiting| waiting.host.depth >= first)
            .count()
    }
    pub(super) fn waiting_at(&self, depth: usize) -> Option<(PendingHost, bool)> {
        self.waiting
            .iter()
            .rev()
            .find(|waiting| waiting.host.depth == depth)
            .map(|waiting| (waiting.host, waiting.instruction))
    }
    pub(super) fn discard_exited(&mut self, first: usize) {
        while self
            .waiting
            .last()
            .is_some_and(|waiting| waiting.host.depth >= first)
        {
            self.waiting.pop();
        }
        self.returned = None;
    }
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
    /// retains the command and its actual return; checked replay reexecutes the guest callback.
    pub fn call_guest(mut self, callee: Value, args: Vec<Value>) -> Result<(), TrapInfo> {
        self.delivering = true;
        let result = self.running.begin_callback(callee, args);
        self.delivering = false;
        result
    }

    /// Borrows the latest successful callback results without consuming them.
    ///
    /// An empty successful callback yields `Some(&[])`; `None` means no results are available.
    /// Handles use this request's original heap/code. Inspection spends no guest work.
    #[must_use]
    pub fn callback_results(&self) -> Option<&[Value]> {
        self.running
            .continuation
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
        self.running
            .continuation
            .control
            .callbacks
            .as_mut()?
            .returned
            .take()
    }
}

impl<H: Host> Running<'_, '_, H> {
    fn begin_callback(&mut self, callee: Value, args: Vec<Value>) -> Result<(), TrapInfo> {
        let pending = self
            .continuation
            .control
            .pending_host
            .expect("pending host guard");
        let span = self.continuation.ctx.span_at(
            self.continuation.program.program(),
            pending.location.func,
            pending.location.pc,
        );
        let result = self.prepare_callback(pending, &callee, args);
        match result {
            Ok(()) => Ok(()),
            Err(trap) => {
                self.continuation.control.pending_host = None;
                let error = self.continuation.ctx.trap(
                    pending.location.func,
                    pending.location.pc,
                    span,
                    trap,
                );
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
            self.continuation.ctx.frames.len(),
            "only the active host request can enter a callback"
        );
        let mut staging = self.continuation.host_results.take().unwrap_or_default();
        let work = staging.work;

        let created = staging
            .promote(self.vm.memory.aggs_mut())
            .map_err(Trap::AggError)?;
        self.continuation
            .trace
            .values(self.continuation.program.program(), || {
                ValueEvent::CallbackEnter {
                    location: pending.location,
                    callee,
                    args: &args,
                    work,
                    created: &created,
                    aggregates: self.vm.memory.aggs(),
                }
            });
        if self.budget.fuel == 0 {
            return Err(Trap::FuelExceeded);
        }
        self.budget.fuel -= 1;
        if self.budget.frames >= self.vm.limits.max_call_depth {
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
        let program = self.continuation.program.program();
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
        self.budget.data.charge_payload_bytes(payload, 1)?;
        let prepared = self
            .continuation
            .program
            .verified(function)
            .ok_or(Trap::InvalidPc)?;
        let base = self.continuation.ctx.alloc_frame(prepared);
        for (register, value) in prepared.reg_layout.arg_regs.iter().copied().zip(args) {
            self.continuation
                .ctx
                .write_owned_value(base, register, value)?;
        }
        self.vm.check_arg_contracts(
            &mut self.continuation.ctx,
            self.budget,
            program,
            function,
            prepared,
            base,
        )?;
        if let Some(environment) = environment {
            // An opaque Agg contract must still resolve a closure's environment before entry.
            let contract = program.verified_contracts(body.arg_types)[0];
            if matches!(program.contracts.get(contract), Some(Contract::Kind(_))) {
                self.budget
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
        self.budget.frames += 1;
        self.continuation.ctx.frames.push(Frame {
            func: function,
            pc: prepared.pc_at_ix(0).unwrap_or(0),
            instr_ix: 0,
            byte_len: prepared.byte_len,
            base,
            return_to: None,
        });
        self.continuation.control.enter(pending.location);
        let callbacks = self
            .continuation
            .control
            .callbacks
            .get_or_insert_with(|| Box::new(Callbacks::default()));
        callbacks.returned = None;
        let instruction = self.continuation.trace.pending.take();
        debug_assert!(
            instruction.is_none_or(|location| location == pending.location),
            "the outstanding instruction must be this host call"
        );
        callbacks.waiting.push(WaitingHost {
            host: pending,
            instruction: instruction.is_some(),
        });
        self.continuation.control.pending_host = None;
        self.continuation.trace.call_scope_enter_if_enabled(
            program,
            self.continuation.ctx.frames.len(),
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
    ctx: &mut Registers,
    budget: &mut Budget,
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
    vm.check_result_contracts(ctx, budget, program, location.func, body, base, rets)
        .map_err(|trap| ctx.trap(location.func, location.pc, span, trap))?;
    let mut values = Vec::with_capacity(rets.len());
    for &reg in rets {
        values.push(
            ctx.materialize_vreg(
                budget,
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
    budget.frames -= 1;
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
