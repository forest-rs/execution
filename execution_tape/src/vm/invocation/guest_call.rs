// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Guest entry retains the exact callable owner and returns through one segment boundary.

use super::*;
use crate::{program::CallSigId, trace::EntryOrigin, value::FunctionRef};

pub(in crate::vm) struct GuestTransfer {
    pub(in crate::vm) location: TraceLocation,
    pub(in crate::vm) instruction: usize,
    pub(in crate::vm) target: FunctionRef,
    pub(in crate::vm) environment: Option<AggHandle>,
    pub(in crate::vm) signature: CallSigId,
    pub(in crate::vm) args: VRegSlice,
    pub(in crate::vm) rets: VRegSlice,
}

impl<H: Host> Vm<H> {
    pub(in crate::vm) fn prepare_guest_transfer<P: TracePolicy>(
        &mut self,
        ctx: &mut Registers,
        budget: &mut Budget,
        caller: &VerifiedProgram,
        inputs: &[Value],
        transfer: GuestTransfer,
        trace: &mut TraceCtx<'_>,
        control: &mut ExecutionControl,
    ) -> Result<Progress, TrapInfo> {
        let frame = ctx.frames.last().expect("calling guest frame");
        let location = transfer.location;
        let span = caller
            .verified(frame.func)
            .and_then(|function| function.span_at_ix(transfer.instruction))
            .map(|id| id.get());
        let trap = |ctx: &Registers, error| ctx.trap(location.func, location.pc, span, error);
        if !self.owns_instance(transfer.target.instance()) {
            return Err(trap(
                ctx,
                Trap::ForeignStoreValue {
                    kind: if transfer.environment.is_some() {
                        ValueType::Closure
                    } else {
                        ValueType::Func
                    },
                },
            ));
        }
        let expected = caller.program();
        let signature = expected
            .call_sig(transfer.signature)
            .expect("verified indirect signature");
        let actual = transfer.target.instance().program().program();
        let function = &actual.functions[transfer.target.id().0 as usize];
        let actual_args = actual.verified_contracts(function.arg_types);
        let actual_args = if transfer.environment.is_some() {
            let Some((&environment, args)) = actual_args.split_first() else {
                return Err(trap(ctx, Trap::ArityMismatch));
            };
            if actual.contracts.kind(environment) != Some(ValueType::Agg) {
                return Err(trap(ctx, Trap::ArityMismatch));
            }
            args
        } else {
            actual_args
        };
        let matches = budget
            .contract_scratch
            .same_signature(
                expected,
                expected
                    .call_sig_arg_contracts(signature)
                    .expect("verified arguments"),
                expected
                    .call_sig_ret_contracts(signature)
                    .expect("verified results"),
                actual,
                actual_args,
                actual.verified_contracts(function.ret_types),
                self.limits.max_contract_work,
            )
            .map_err(|error| {
                trap(
                    ctx,
                    self.contract_trap(
                        location.func,
                        ContractPosition::Argument {
                            callee: location.func,
                            index: 0,
                        },
                        error,
                    ),
                )
            })?;
        if !matches {
            return Err(trap(
                ctx,
                Trap::CallableSignatureMismatch {
                    signature: transfer.signature,
                    function: transfer.target,
                },
            ));
        }
        if budget.frames >= self.limits.max_call_depth {
            return Err(trap(ctx, Trap::CallDepthExceeded));
        }
        let frame = ctx.frames.last().expect("calling guest frame");
        let pending = PendingGuest {
            location,
            instruction: transfer.instruction,
            depth: ctx.frames.len(),
            rets: transfer.rets,
            successor: None,
            abandoned: true,
        };
        let base = frame.base;
        let args = caller
            .verified(frame.func)
            .expect("calling definition")
            .vregs(transfer.args);
        let mut values = Vec::with_capacity(args.len());
        let sources = ValueSources::new(caller.program(), inputs, self.memory.aggs());
        for &arg in args {
            values.push(
                ctx.materialize_vreg(budget, base, arg, sources)
                    .map_err(|error| trap(ctx, error))?,
            );
        }
        if ctx.instance.is_none() {
            ctx.instance = Some(
                self.instances
                    .default_for(caller)
                    .map_err(|error| trap(ctx, error))?,
            );
        }
        control.waiting = Waiting::Guest(pending);
        // A guest call completes at dispatch. Its results become initialized when its child
        // actually returns. The qualified control event identifies the foreign successor.
        P::instr_end(trace, caller.program(), None);
        Ok(Progress::GuestCall {
            target: transfer.target,
            environment: transfer.environment,
            args: values,
        })
    }
}

impl<'state, H: Host> Invocation<'state, H> {
    pub(in crate::vm) fn enter_guest(
        &mut self,
        target: FunctionRef,
        environment: Option<AggHandle>,
        args: Vec<Value>,
    ) -> Result<RunStatus, TrapInfo> {
        let callee = match environment {
            Some(env) => Value::Closure(Closure { func: target, env }),
            None => Value::Func(target),
        };
        self.running()
            .enter_segment(callee, args, EntryOrigin::GuestCall)?;
        let parent = self.parents.last_mut().expect("guest caller retained");
        parent
            .control
            .waiting
            .guest_mut()
            .expect("pending guest call")
            .abandoned = false;
        Ok(RunStatus::Suspended)
    }
}

impl<H: Host> PendingHostCall<'_, '_, H> {
    /// Starts a guest function or closure in its exact instance while retaining this host request.
    ///
    /// Accepts a qualified function, closure, or dynamic `Value`. For an explicit instance,
    /// pass `instance.function(entry)?` directly. All calls keep this execution's yielding policy,
    /// observer and resource-access sink; those are selected once at root entry.
    ///
    /// Consumes the view, not the host operation. Drive the same invocation with `resume`:
    /// the callback may yield further host requests, whose guards support the same operation.
    /// When the callback returns, `resume` reports `AwaitingHost` for this original request.
    /// Take its callback results, then complete the host operation explicitly. Repeated calls
    /// discard any untaken previous callback results.
    ///
    /// The target must belong to this VM store; it may retain another verified definition. Raw
    /// aggregate handles use this VM heap. Closure arguments exclude the hidden environment.
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
    pub fn call_guest(
        mut self,
        callee: impl Into<Value>,
        args: Vec<Value>,
    ) -> Result<(), ControlError> {
        let callee = callee.into();
        let instance = match &callee {
            Value::Func(function) => Some(function.instance()),
            Value::Closure(closure) => Some(closure.func.instance()),
            _ => None,
        };
        if instance.is_some_and(|instance| !self.running.vm.owns_instance(instance)) {
            return Err(ControlError::ForeignStore);
        }
        self.delivering = true;
        let result = self
            .running
            .enter_segment(callee, args, EntryOrigin::HostRequest);
        self.delivering = false;
        result.map_err(ControlError::Trapped)
    }

    /// Borrows the latest successful callback results without consuming them.
    ///
    /// An empty successful callback yields `Some(&[])`; `None` means no results are available.
    /// Aggregate handles use this VM heap; callable targets retain their own code.
    /// Inspection spends no guest work.
    #[must_use]
    pub fn callback_results(&self) -> Option<&[Value]> {
        self.running
            .continuation
            .control
            .callback_results
            .as_deref()
    }

    /// Takes the latest successful callback results belonging to this pending host operation.
    ///
    /// `None` means no callback has returned or the slots were already taken. The host operation
    /// is still pending; aggregate handles use the execution heap and callable targets retain
    /// their own code.
    /// Taking values does not retain them independently. Completing the host operation discards
    /// untaken slots. Use the request context's `retain` for an independent aggregate snapshot.
    pub fn take_callback_results(&mut self) -> Option<Vec<Value>> {
        self.running.continuation.control.callback_results.take()
    }
}
