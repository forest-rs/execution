// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Indirect guest calls use the value's code owner, without asking the host to interpret an ID.

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
        self.running()
            .enter_segment(&target, environment, args, EntryOrigin::GuestCall)?;
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
