// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Terminal transfer inherits a result recipient; retired frames never forward results.

use super::*;
use crate::{control::ControlBoundary, trace::ControlEvent, typed::ControlInstruction};

struct Replacement<'a> {
    stacks: Vec<continuation::Continuation<'a>>,
    response: Option<Vec<Value>>,
    value: Value,
    structural_results: bool,
    argument_start: usize,
}

impl<'a, H: Host> Running<'_, 'a, H> {
    pub(super) fn dispatch_tail(
        &mut self,
        location: TraceLocation,
        operation: &ControlInstruction,
    ) -> Result<RunStatus, TrapInfo> {
        let replacement = self
            .prepare_tail(location, operation)
            .map_err(|error| self.control_trap(location, error))?;
        self.replace_activation(location, replacement)?;
        Ok(RunStatus::Suspended)
    }

    fn check_tail_environment(
        &mut self,
        target: &crate::value::FunctionRef,
        environment: Option<AggHandle>,
        boundary: ControlBoundary,
    ) -> Result<(), Trap> {
        let Some(environment) = environment else {
            return Ok(());
        };
        let code = target.instance().program();
        let body = &code.program().functions[target.id().0 as usize];
        let input = crate::program::ByteRange {
            offset: body.arg_types.offset,
            len: 1,
        };
        let value = [Value::Agg(environment)];
        self.check_control_values((code, input), &value, boundary)?;
        composition::check_transfer(
            self.vm.memory.aggs(),
            &value,
            target.instance(),
            target.instance(),
            self.budget,
            &self.vm.limits,
            target.id(),
            true,
        )
    }

    fn prepare_tail(
        &mut self,
        location: TraceLocation,
        operation: &ControlInstruction,
    ) -> Result<Replacement<'a>, Trap> {
        let code = (*self.continuation.program).clone();
        let frame = self
            .continuation
            .ctx
            .frames
            .last()
            .expect("tail source frame");
        let base = frame.base;
        let source = self
            .continuation
            .ctx
            .instance
            .as_ref()
            .expect("control namespace")
            .clone();
        match operation {
            ControlInstruction::TailCall {
                signature,
                callee,
                args,
            } => {
                let value = Vm::<H>::contract_value(&self.continuation.ctx, base, *callee);
                let (target, environment) = self.control_callable(&value)?;
                self.check_tail_environment(&target, environment, ControlBoundary::TailCall)?;
                let program = target.instance().program();
                let body = &program.program().functions[target.id().0 as usize];
                let mut parameters = body.arg_types;
                if environment.is_some() {
                    parameters.offset += 1;
                    parameters.len -= 1;
                }
                self.check_signature(
                    &code,
                    *signature,
                    (program, parameters),
                    (program, body.ret_types),
                    ControlBoundary::TailCall,
                )?;
                let args = self.control_values(*args)?;
                self.check_control_values((program, parameters), &args, ControlBoundary::TailCall)?;
                composition::check_transfer(
                    self.vm.memory.aggs(),
                    &args,
                    &source,
                    target.instance(),
                    self.budget,
                    &self.vm.limits,
                    target.id(),
                    true,
                )?;
                let structural_results = !code.shares_definition_with(program);
                let control = self.new_control();
                let child = self.segment(&target, environment, args, control, location);
                Ok(Replacement {
                    stacks: vec![child],
                    response: None,
                    value,
                    structural_results,
                    argument_start: usize::from(environment.is_some()),
                })
            }
            ControlInstruction::TailResume {
                signature,
                continuation,
                args,
            } => {
                let reference = self.continuation.ctx.read_continuation(base, *continuation);
                self.check_signature(
                    &code,
                    *signature,
                    (
                        &reference.parameter_owner().program,
                        reference.parameter_owner().range,
                    ),
                    (
                        &reference.result_owner().program,
                        reference.result_owner().range,
                    ),
                    ControlBoundary::TailResume,
                )?;
                let args = self.control_values(*args)?;
                self.check_control_values(
                    (
                        &reference.parameter_owner().program,
                        reference.parameter_owner().range,
                    ),
                    &args,
                    ControlBoundary::TailResume,
                )?;
                let saved = self
                    .resumptions
                    .get(reference.id())
                    .map_err(|error| Trap::Continuation(error.into()))?;
                let structural_results = match saved {
                    delimited::Saved::New { callable, .. } => {
                        let definition = match callable {
                            Value::Func(function)
                            | Value::Closure(Closure { func: function, .. }) => {
                                function.instance().program()
                            }
                            _ => unreachable!("saved qualified callable"),
                        };
                        !code.shares_definition_with(definition)
                    }
                    delimited::Saved::Suspended(stacks) => {
                        !code.shares_definition_with(&stacks.first().expect("saved root").program)
                    }
                };
                let new_target = match saved {
                    delimited::Saved::New { callable, .. } => {
                        let (target, environment) = self.control_callable(callable)?;
                        self.check_tail_environment(
                            &target,
                            environment,
                            ControlBoundary::TailResume,
                        )?;
                        composition::check_transfer(
                            self.vm.memory.aggs(),
                            &args,
                            &source,
                            target.instance(),
                            self.budget,
                            &self.vm.limits,
                            target.id(),
                            true,
                        )?;
                        Some((target, environment))
                    }
                    delimited::Saved::Suspended(stacks) => {
                        let leaf = stacks.last().expect("saved leaf");
                        composition::check_transfer(
                            self.vm.memory.aggs(),
                            &args,
                            &source,
                            leaf.ctx.instance.as_ref().expect("saved namespace"),
                            self.budget,
                            &self.vm.limits,
                            location.func,
                            true,
                        )?;
                        None
                    }
                };
                let argument_start = usize::from(
                    new_target
                        .as_ref()
                        .is_some_and(|(_, environment)| environment.is_some()),
                );
                // All protocol, ownership and authority checks precede this one-use consumption.
                let saved = self
                    .resumptions
                    .take(reference.id())
                    .expect("validated tail authority");
                let (stacks, response) = match saved {
                    delimited::Saved::New { control, .. } => {
                        let (target, environment) = new_target.expect("validated callable");
                        (
                            vec![self.segment(&target, environment, args, control, location)],
                            None,
                        )
                    }
                    delimited::Saved::Suspended(stacks) => (stacks, Some(args)),
                };
                Ok(Replacement {
                    stacks,
                    response,
                    value: Value::Continuation(reference),
                    structural_results,
                    argument_start,
                })
            }
            _ => unreachable!("terminal transfer opcode"),
        }
    }

    fn replace_activation(
        &mut self,
        location: TraceLocation,
        mut replacement: Replacement<'a>,
    ) -> Result<(), TrapInfo> {
        let source = self.continuation.control.activation(
            &self.continuation.ctx,
            self.continuation.ctx.frames.len() - 1,
        );
        let target = replacement
            .stacks
            .first()
            .expect("replacement root")
            .control
            .root_activation();
        let span = self.continuation.ctx.span_at(
            self.continuation.program.program(),
            location.func,
            location.pc,
        );
        let top = self.continuation.ctx.frames.pop().expect("retired frame");
        self.budget.frames -= 1;
        let root = replacement.stacks.first_mut().expect("replacement root");
        let retiring_root = self.continuation.ctx.frames.is_empty();
        if retiring_root {
            root.parent = self.continuation.parent.take();
            root.delimiter = self.continuation.delimiter.take();
            root.structural_results |= self.continuation.structural_results;
            root.outputs = core::mem::take(&mut self.continuation.outputs);
            root.output_start = self.continuation.output_start;
        } else {
            let parent = self
                .continuation
                .ctx
                .frames
                .last()
                .expect("surviving caller");
            let parent_id = self.continuation.control.activation(
                &self.continuation.ctx,
                self.continuation.ctx.frames.len() - 1,
            );
            let caller = TraceLocation {
                func: parent.func,
                pc: self
                    .continuation
                    .program
                    .verified(parent.func)
                    .expect("caller definition")
                    .pc_at_ix(parent.instr_ix - 1)
                    .expect("ordinary call site"),
            };
            root.parent = Some((parent_id, caller));
            self.continuation.control.waiting = Waiting::Guest(PendingGuest {
                location: caller,
                instruction: parent.instr_ix - 1,
                depth: self.continuation.ctx.frames.len(),
                rets: top.return_to.expect("ordinary result recipient").dst_rets,
                successor: None,
                abandoned: false,
            });
            self.continuation.ctx.truncate_to(top.base);
        }
        root.structural_results |= replacement.structural_results;
        let recipient = root.parent.as_ref().map(|(id, _)| id.clone());
        let active = replacement.stacks.pop().expect("replacement leaf");
        let replacement_parent_count = replacement.stacks.len();
        let mut retired = core::mem::replace(self.continuation, active);
        // Install every surviving owner and poison the replacement before arbitrary observers.
        self.continuation.control.waiting = if replacement.response.is_some() {
            let mut waiting = self.continuation.control.waiting;
            waiting.poison();
            waiting
        } else {
            Waiting::Transition(TraceLocation {
                func: self.continuation.entry,
                pc: 0,
            })
        };
        if !retiring_root {
            let parent_index = self.parents.len();
            self.parents.push(retired);
            self.parents.extend(replacement.stacks);
            let parent = &mut self.parents[parent_index];
            Self::observe_tail(
                parent,
                location,
                span,
                &source,
                &target,
                recipient.as_ref(),
                &replacement.value,
                replacement
                    .response
                    .as_deref()
                    .unwrap_or(&self.continuation.args[replacement.argument_start..]),
                self.vm.memory.aggs(),
                false,
            );
        } else {
            self.parents.extend(replacement.stacks);
            Self::observe_tail(
                &mut retired,
                location,
                span,
                &source,
                &target,
                recipient.as_ref(),
                &replacement.value,
                replacement
                    .response
                    .as_deref()
                    .unwrap_or(&self.continuation.args[replacement.argument_start..]),
                self.vm.memory.aggs(),
                true,
            );
        }
        if let Some(response) = replacement.response {
            let first = self.parents.len() - replacement_parent_count;
            for stack in &mut self.parents[first..] {
                delimited::attach_trace(stack);
            }
            delimited::attach_trace(self.continuation);
            let pending = self
                .continuation
                .control
                .waiting
                .control()
                .expect("saved suspension");
            Self::complete_control(self.continuation, response, self.vm.memory.aggs())
                .map_err(|error| self.control_trap(pending.location, error))?;
            self.budget.select(&self.continuation.control);
        } else {
            self.continuation
                .initialize(self.vm, self.budget, DataWork::default())?;
        }
        Ok(())
    }

    fn observe_tail(
        stack: &mut continuation::Continuation<'_>,
        location: TraceLocation,
        span: Option<u64>,
        source: &ActivationId,
        target: &ActivationId,
        recipient: Option<&ActivationId>,
        value: &Value,
        args: &[Value],
        heap: &AggHeap,
        root: bool,
    ) {
        stack
            .trace
            .control(stack.program.program(), || ControlEvent::TailTransfer {
                location,
                source,
                target,
                recipient,
                value,
                args,
                aggregates: heap,
            });
        let outcome = TraceOutcome::TailTransferred { target };
        stack
            .trace
            .instr_end(stack.program.program(), None, outcome);
        stack.trace.exit_frame(
            stack.program.program(),
            stack.ctx.frames.len() + 1,
            location,
            span,
            outcome,
        );
        if root {
            stack.trace.run_end(stack.program.program(), outcome);
        }
    }

    pub(super) fn check_root_tail_result(&mut self, site: ReturnSite) -> Result<(), TrapInfo> {
        if !self.continuation.structural_results {
            return Ok(());
        }
        let source = self
            .continuation
            .ctx
            .instance
            .as_ref()
            .expect("tail result namespace");
        composition::check_transfer_policy(
            self.vm.memory.aggs(),
            &self.continuation.outputs[self.continuation.output_start..],
            source,
            source,
            self.budget,
            &self.vm.limits,
            site.location.func,
            false,
            true,
        )
        .map_err(|error| self.control_trap(site.location, error))
    }
}

#[cfg(test)]
mod tests;
