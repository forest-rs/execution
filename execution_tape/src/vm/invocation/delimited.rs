// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Delimited control moves owned guest stack suffixes; the invocation retains storage authority.

use super::*;
use crate::control::{ContractList, ContractOwner, ControlBoundary, Handler, SignatureMismatch};
use crate::program::{ByteRange, CallSigId};
use crate::typed::ControlInstruction;
use crate::value::FunctionRef;

pub(in crate::vm) struct Delimiter {
    handlers: HandlerTable,
    results: ContractOwner,
}
enum HandlerTable {
    /// Bind dynamic tag registers in the frozen resumer to immutable prepared clause targets.
    Guest,
    Native(Vec<Handler>),
}
pub(in crate::vm) enum Saved<'a> {
    New {
        callable: Value,
        control: ExecutionControl,
    },
    Suspended(Vec<continuation::Continuation<'a>>),
}
impl Saved<'_> {
    fn frames(&self) -> usize {
        match self {
            Self::New { .. } => 0,
            Self::Suspended(stacks) => stacks.iter().map(|stack| stack.ctx.frames.len()).sum(),
        }
    }
    pub(super) fn cancel(self) {
        if let Self::Suspended(mut stacks) = self {
            for stack in stacks.iter_mut().rev() {
                finish_control_trace(stack, TraceOutcome::Cancelled);
            }
        }
    }
}

impl<'request, 'a, H: Host> Running<'request, 'a, H> {
    fn control_trap(&self, location: TraceLocation, trap: Trap) -> TrapInfo {
        let span = self.continuation.ctx.span_at(
            self.continuation.program.program(),
            location.func,
            location.pc,
        );
        self.continuation
            .ctx
            .trap(location.func, location.pc, span, trap)
    }

    pub(super) fn dispatch_control(
        &mut self,
        location: TraceLocation,
        instruction: usize,
    ) -> Result<RunStatus, TrapInfo> {
        self.continuation.control.enable_identity();
        if self.continuation.ctx.instance.is_none() {
            self.continuation.ctx.instance = Some(
                self.vm
                    .instances
                    .default_for(&self.continuation.program)
                    .map_err(|error| self.control_trap(location, error))?,
            );
        }
        let code = (*self.continuation.program).clone();
        let body = code.verified(location.func).expect("running definition");
        let (ExecInstr::Control(operation), _, _) =
            body.fetch_at_ix(instruction).expect("running instruction")
        else {
            unreachable!("control dispatch")
        };
        let base = self
            .continuation
            .ctx
            .frames
            .last()
            .expect("running frame")
            .base;
        let mut initialize = false;
        let result = (|| -> Result<(), Trap> {
            match operation.as_ref() {
                ControlInstruction::Tag {
                    dst,
                    declaration,
                    fresh,
                } => {
                    let instance = self
                        .continuation
                        .ctx
                        .instance
                        .as_ref()
                        .expect("resolved namespace");
                    let tag = instance
                        .control_tag(*declaration)
                        .expect("verified declaration");
                    self.continuation.ctx.write_tag(
                        base,
                        *dst,
                        if *fresh { tag.fresh() } else { tag },
                    );
                }
                ControlInstruction::New {
                    dst,
                    signature,
                    callee,
                } => {
                    let callee = Vm::<H>::contract_value(&self.continuation.ctx, base, *callee);
                    let (target, environment) = self.control_callable(&callee)?;
                    let program = target.instance().program();
                    let entry = &program.program().functions[target.id().0 as usize];
                    let mut parameters = entry.arg_types;
                    if environment.is_some() {
                        if parameters.len == 0 {
                            return Err(Trap::ArityMismatch);
                        }
                        parameters.offset += 1;
                        parameters.len -= 1;
                    }
                    self.check_signature(
                        &code,
                        *signature,
                        (program, parameters),
                        (program, entry.ret_types),
                        ControlBoundary::Create,
                    )?;
                    let parameters = ContractOwner {
                        program: program.clone(),
                        range: parameters,
                    };
                    let results = ContractOwner {
                        program: program.clone(),
                        range: entry.ret_types,
                    };
                    let control = self.new_control();
                    let reference = self.save(
                        Saved::New {
                            callable: callee,
                            control,
                        },
                        parameters,
                        results,
                    )?;
                    self.continuation
                        .ctx
                        .write_continuation(base, *dst, reference);
                }
                ControlInstruction::Resume {
                    signature,
                    continuation,
                    args,
                    returned,
                    handlers,
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
                        ControlBoundary::Resume,
                    )?;
                    self.resumptions
                        .get(reference.id())
                        .map_err(|error| Trap::Continuation(error.into()))?;
                    let arguments = self.control_values(*args)?;
                    self.check_control_values(
                        (
                            &reference.parameter_owner().program,
                            reference.parameter_owner().range,
                        ),
                        &arguments,
                        ControlBoundary::Resume,
                    )?;
                    if let Saved::Suspended(stack) = self
                        .resumptions
                        .get(reference.id())
                        .expect("checked authority")
                    {
                        composition::check_transfer(
                            self.vm.memory.aggs(),
                            &arguments,
                            self.continuation
                                .ctx
                                .instance
                                .as_ref()
                                .expect("running namespace"),
                            stack
                                .last()
                                .expect("captured leaf")
                                .ctx
                                .instance
                                .as_ref()
                                .expect("captured namespace"),
                            self.budget,
                            &self.vm.limits,
                            location.func,
                            true,
                        )?;
                    }
                    let entry = code
                        .program()
                        .call_sig(*signature)
                        .expect("verified signature");
                    for arm in handlers {
                        let tag = self.continuation.ctx.read_tag(base, arm.tag);
                        if !self.vm.owns_instance(tag.instance()) {
                            return Err(Trap::ForeignStoreValue {
                                kind: ValueType::Tag,
                            });
                        }
                        let declaration = tag
                            .instance()
                            .program()
                            .program()
                            .call_sig(tag.signature_id())
                            .expect("operation declaration");
                        self.check_signature(
                            &code,
                            arm.signature,
                            (tag.instance().program(), declaration.args),
                            (tag.instance().program(), declaration.rets),
                            ControlBoundary::Handler,
                        )?;
                    }
                    let delimiter = Delimiter {
                        handlers: HandlerTable::Guest,
                        results: ContractOwner {
                            program: code.clone(),
                            range: entry.rets,
                        },
                    };
                    // All recoverable checks precede consuming authority. No observer runs between
                    // invalidating the capability and installing the stack in its execution owner.
                    let saved = self
                        .resumptions
                        .take(reference.id())
                        .map_err(|error| Trap::Continuation(error.into()))?;
                    self.continuation.control.waiting = Waiting::Control(PendingGuest {
                        location,
                        instruction,
                        depth: self.continuation.ctx.frames.len(),
                        rets: returned.parameters,
                        successor: Some((returned.pc, returned.instruction)),
                        abandoned: false,
                    });
                    initialize = self.attach_saved(saved, arguments, delimiter, location)?;
                    return Ok(());
                }
                ControlInstruction::Suspend {
                    signature,
                    tag,
                    args,
                    rets,
                } => {
                    let tag = self.continuation.ctx.read_tag(base, *tag);
                    if !self.vm.owns_instance(tag.instance()) {
                        return Err(Trap::ForeignStoreValue {
                            kind: ValueType::Tag,
                        });
                    }
                    let owner = tag.instance().program();
                    let signature_entry = owner
                        .program()
                        .call_sig(tag.signature_id())
                        .expect("verified protocol");
                    self.check_signature(
                        &code,
                        *signature,
                        (owner, signature_entry.args),
                        (owner, signature_entry.rets),
                        ControlBoundary::Suspend,
                    )?;
                    let request = self.control_values(*args)?;
                    self.check_control_values(
                        (owner, signature_entry.args),
                        &request,
                        ControlBoundary::Suspend,
                    )?;
                    initialize = self.capture(
                        tag,
                        request,
                        PendingGuest {
                            location,
                            instruction,
                            depth: self.continuation.ctx.frames.len(),
                            rets: *rets,
                            successor: None,
                            abandoned: false,
                        },
                    )?;
                    return Ok(());
                }
                ControlInstruction::Drop { continuation } => {
                    let reference = self.continuation.ctx.read_continuation(base, *continuation);
                    let saved = self
                        .resumptions
                        .take(reference.id())
                        .map_err(|error| Trap::Continuation(error.into()))?;
                    self.cancel_stack(saved);
                }
                ControlInstruction::IsHandled { dst, tag } => {
                    let tag = self.continuation.ctx.read_tag(base, *tag);
                    let handled = self.handler_index(&tag).is_some();
                    self.continuation.ctx.write_bool(base, *dst, handled);
                }
            }
            let stack = &mut *self.continuation;
            observations::emit(
                &mut stack.register_state,
                &mut stack.trace,
                stack.program.program(),
                &stack.ctx,
                location.func,
                location.pc,
                base,
                ValueSources::new(stack.program.program(), &stack.args, self.vm.memory.aggs()),
                crate::trace::RegisterPhase::Outputs,
            );
            let frame = stack
                .ctx
                .frames
                .last()
                .expect("completed control instruction");
            let next = TraceLocation {
                func: frame.func,
                pc: frame.pc,
            };
            stack
                .trace
                .instr_end(stack.program.program(), Some(next), TraceOutcome::Ok);
            Ok(())
        })();
        result.map_err(|trap| self.control_trap(location, trap))?;
        if initialize {
            self.continuation
                .initialize(self.vm, self.budget, DataWork::default())?;
        }
        Ok(RunStatus::Suspended)
    }

    fn save(
        &mut self,
        saved: Saved<'a>,
        parameters: ContractOwner,
        results: ContractOwner,
    ) -> Result<ContinuationRef, Trap> {
        match self.resumptions.insert(
            self.continuation.control.execution_owner(),
            saved,
            self.vm.limits.max_continuations,
        ) {
            Ok(key) => Ok(ContinuationRef::new(key, parameters, results)),
            Err((_, saved)) => {
                self.cancel_stack(saved);
                Err(Trap::ContinuationLimitExceeded)
            }
        }
    }

    fn control_callable(&self, value: &Value) -> Result<(FunctionRef, Option<AggHandle>), Trap> {
        let (target, environment) = match value {
            Value::Func(target) => (target.clone(), None),
            Value::Closure(closure) => (closure.func.clone(), Some(closure.env)),
            _ => {
                return Err(Trap::TypeMismatch {
                    expected: ValueType::Func,
                    actual: value.value_type(),
                });
            }
        };
        if let Some(environment) = environment {
            let program = target.instance().program().program();
            let body = &program.functions[target.id().0 as usize];
            if program
                .verified_contracts(body.arg_types)
                .first()
                .and_then(|&id| program.contracts.kind(id))
                != Some(ValueType::Agg)
            {
                return Err(Trap::ArityMismatch);
            }
            self.vm
                .memory
                .aggs()
                .fields(environment)
                .map_err(Trap::AggError)?;
        }
        if !self.vm.owns_instance(target.instance()) {
            return Err(Trap::ForeignStoreValue {
                kind: value.value_type(),
            });
        }
        Ok((target, environment))
    }

    fn check_signature(
        &mut self,
        expected: &VerifiedProgram,
        signature: CallSigId,
        parameters: (&VerifiedProgram, ByteRange),
        results: (&VerifiedProgram, ByteRange),
        boundary: ControlBoundary,
    ) -> Result<(), Trap> {
        let entry = expected
            .program()
            .call_sig(signature)
            .expect("verified signature");
        self.check_protocol(
            boundary,
            (expected, entry.args),
            (expected, entry.rets),
            parameters,
            results,
        )
    }

    fn check_protocol(
        &mut self,
        boundary: ControlBoundary,
        expected_inputs: (&VerifiedProgram, ByteRange),
        expected_outputs: (&VerifiedProgram, ByteRange),
        actual_inputs: (&VerifiedProgram, ByteRange),
        actual_outputs: (&VerifiedProgram, ByteRange),
    ) -> Result<(), Trap> {
        fn view((program, range): (&VerifiedProgram, ByteRange)) -> ContractList<'_> {
            ContractList::new(
                program.program(),
                program.program().verified_contracts(range),
            )
        }
        let same = self
            .budget
            .contract_scratch
            .same_protocol(
                (view(expected_inputs), view(expected_outputs)),
                (view(actual_inputs), view(actual_outputs)),
                self.vm.limits.max_contract_work,
            )
            .map_err(|error| self.contract_error(error, boundary, None))?;
        if same {
            return Ok(());
        }
        let retain = |(program, range): (&VerifiedProgram, ByteRange)| ContractOwner {
            program: program.clone(),
            range,
        };
        Err(Trap::ControlSignatureMismatch(Box::new(
            SignatureMismatch {
                boundary,
                expected_inputs: retain(expected_inputs),
                expected_outputs: retain(expected_outputs),
                actual_inputs: retain(actual_inputs),
                actual_outputs: retain(actual_outputs),
            },
        )))
    }

    fn contract_error(
        &self,
        error: CheckError,
        boundary: ControlBoundary,
        index: Option<u32>,
    ) -> Trap {
        self.vm.contract_trap(
            self.continuation.ctx.cur_func(),
            ContractPosition::Control { boundary, index },
            error,
        )
    }

    fn check_control_values(
        &mut self,
        owner: (&VerifiedProgram, ByteRange),
        values: &[Value],
        boundary: ControlBoundary,
    ) -> Result<(), Trap> {
        let contracts = owner.0.program().verified_contracts(owner.1);
        if contracts.len() != values.len() {
            return Err(Trap::ControlArityMismatch {
                boundary,
                expected: contracts.len(),
                actual: values.len(),
            });
        }
        for (index, (&contract, value)) in contracts.iter().zip(values).enumerate() {
            self.budget
                .contract_scratch
                .check_value(
                    owner.0.program(),
                    self.vm.memory.aggs(),
                    value,
                    contract,
                    self.vm.limits.max_contract_work,
                )
                .map_err(|mut error| {
                    if let CheckError::Mismatch(violation) = &mut error
                        && violation.contract_program.is_none()
                        && !self.continuation.program.shares_definition_with(owner.0)
                    {
                        violation.contract_program = Some(owner.0.clone());
                    }
                    self.contract_error(
                        error,
                        boundary,
                        Some(u32::try_from(index).expect("verified signature arity")),
                    )
                })?;
        }
        Ok(())
    }

    fn control_values(&mut self, registers: VRegSlice) -> Result<Vec<Value>, Trap> {
        let stack = &mut self.continuation;
        let frame = stack.ctx.frames.last().expect("running frame");
        let base = frame.base;
        let function = stack
            .program
            .verified(frame.func)
            .expect("running definition");
        let sources =
            ValueSources::new(stack.program.program(), &stack.args, self.vm.memory.aggs());
        function
            .vregs(registers)
            .iter()
            .map(|&reg| stack.ctx.materialize_vreg(self.budget, base, reg, sources))
            .collect()
    }

    fn check_handlers(&mut self, delimiter: &Delimiter) -> Result<(), Trap> {
        let HandlerTable::Native(handlers) = &delimiter.handlers else {
            unreachable!("guest clauses checked against prepared protocols")
        };
        for arm in handlers {
            let Handler::Suspend { tag, handler } = arm;
            if !self.vm.owns_instance(tag.instance()) {
                return Err(Trap::ForeignStoreValue {
                    kind: ValueType::Tag,
                });
            }
            let (target, environment) = self.control_callable(handler)?;
            let program = target.instance().program().program();
            let body = &program.functions[target.id().0 as usize];
            let inputs = program.verified_contracts(body.arg_types);
            let inputs = if environment.is_some() {
                inputs.get(1..).ok_or(Trap::ArityMismatch)?
            } else {
                inputs
            };
            let Some((&continuation, _request)) = inputs.split_last() else {
                return Err(Trap::ArityMismatch);
            };
            if program.contracts.kind(continuation) != Some(ValueType::Continuation) {
                return Err(Trap::TypeMismatch {
                    expected: ValueType::Continuation,
                    actual: program
                        .contracts
                        .kind(continuation)
                        .expect("verified contract"),
                });
            }
            let target_program = target.instance().program();
            let declaration = tag
                .instance()
                .program()
                .program()
                .call_sig(tag.signature_id())
                .expect("verified protocol");
            let hidden = u32::from(environment.is_some());
            let request_range = ByteRange {
                offset: body.arg_types.offset + hidden,
                len: body.arg_types.len - hidden - 1,
            };
            self.check_protocol(
                ControlBoundary::Handler,
                (tag.instance().program(), declaration.args),
                (&delimiter.results.program, delimiter.results.range),
                (target_program, request_range),
                (target_program, body.ret_types),
            )?;
            if let Some(Contract::Continuation {
                parameters,
                results,
            }) = program.contracts.get(continuation)
                && !self
                    .budget
                    .contract_scratch
                    .same_protocol(
                        (
                            ContractList::new(program, parameters),
                            ContractList::new(program, results),
                        ),
                        (tag.response(), delimiter.results.view()),
                        self.vm.limits.max_contract_work,
                    )
                    .map_err(|error| self.contract_error(error, ControlBoundary::Handler, None))?
            {
                return Err(self.contract_error(
                    CheckError::Mismatch(ContractViolation {
                        contract_program: Some(target_program.clone()),
                        contract: continuation,
                        path: Vec::new(),
                        mismatch: crate::contract::ContractMismatch::Signature {
                            kind: ValueType::Continuation,
                        },
                    }),
                    ControlBoundary::Handler,
                    Some(u32::try_from(inputs.len() - 1).expect("verified argument count")),
                ));
            }
        }
        Ok(())
    }

    fn child(
        &mut self,
        callable: Value,
        args: Vec<Value>,
        control: ExecutionControl,
        location: TraceLocation,
    ) -> Result<continuation::Continuation<'a>, Trap> {
        let (target, environment) = self.control_callable(&callable)?;
        let child = self.segment(&target, environment, args, control, location);
        self.admit_segment(&child, environment)?;
        Ok(child)
    }

    fn attach_saved(
        &mut self,
        saved: Saved<'a>,
        args: Vec<Value>,
        delimiter: Delimiter,
        location: TraceLocation,
    ) -> Result<bool, Trap> {
        match saved {
            Saved::New { callable, control } => {
                let mut child = self.child(callable, args, control, location)?;
                child.delimiter = Some(delimiter);
                let parent = core::mem::replace(self.continuation, child);
                self.parents.push(parent);
                return Ok(true);
            }
            Saved::Suspended(mut stack) => {
                stack.first_mut().expect("captured root").delimiter = Some(delimiter);
                let active = stack.pop().expect("captured stack");
                let parent = core::mem::replace(self.continuation, active);
                self.parents.push(parent);
                self.parents.extend(stack);
                Self::complete_control(self.continuation, args, self.vm.memory.aggs())?;
                self.budget.select(&self.continuation.control);
            }
        }
        Ok(false)
    }

    fn handler_index(&self, tag: &ControlTag) -> Option<(usize, usize)> {
        (0..=self.parents.len()).rev().find_map(|index| {
            let stack: &continuation::Continuation<'_> = if index == self.parents.len() {
                self.continuation
            } else {
                &self.parents[index]
            };
            let arm = match &stack.delimiter.as_ref()?.handlers {
                HandlerTable::Native(handlers) => handlers.iter().position(
                    |arm| matches!(arm, Handler::Suspend { tag: expected, .. } if expected == tag),
                ),
                HandlerTable::Guest => {
                    let resumer = self.parents.get(index.checked_sub(1)?)?;
                    let site = resumer
                        .control
                        .waiting
                        .control()
                        .expect("waiting guest resume");
                    let base = resumer.ctx.frames[site.depth - 1].base;
                    Self::guest_clauses(resumer)
                        .iter()
                        .position(|arm| resumer.ctx.read_tag(base, arm.tag) == *tag)
                }
            }?;
            Some((index, arm))
        })
    }

    fn guest_clauses<'s>(
        resumer: &'s continuation::Continuation<'_>,
    ) -> &'s [crate::typed::ControlHandler] {
        let site = resumer
            .control
            .waiting
            .control()
            .expect("waiting guest resume");
        let (ExecInstr::Control(operation), _, _) = resumer
            .program
            .verified(site.location.func)
            .expect("resumer definition")
            .fetch_at_ix(site.instruction)
            .expect("resumer instruction")
        else {
            unreachable!()
        };
        let ControlInstruction::Resume { handlers, .. } = operation.as_ref() else {
            unreachable!()
        };
        handlers
    }

    fn capture(
        &mut self,
        tag: ControlTag,
        request: Vec<Value>,
        pending: PendingGuest,
    ) -> Result<bool, Trap> {
        let (index, arm) = self
            .handler_index(&tag)
            .ok_or_else(|| Trap::UnhandledControlTag { tag: tag.clone() })?;
        if self.resumptions.len() >= self.vm.limits.max_continuations {
            return Err(Trap::ContinuationLimitExceeded);
        }
        let root = if index == self.parents.len() {
            &mut self.continuation
        } else {
            &mut self.parents[index]
        };
        let mut delimiter = root.delimiter.take().expect("matching delimiter");
        let results = delimiter.results.clone();
        let declaration = tag
            .instance()
            .program()
            .program()
            .call_sig(tag.signature_id())
            .expect("verified protocol");
        let parameters = ContractOwner {
            program: tag.instance().program().clone(),
            range: declaration.rets,
        };
        self.continuation.control.waiting = Waiting::Control(pending);
        let callable = match &mut delimiter.handlers {
            HandlerTable::Guest => {
                let resumer = &self.parents[index - 1];
                let target = Self::guest_clauses(resumer)[arm].target;
                composition::check_transfer(
                    self.vm.memory.aggs(),
                    &request,
                    self.continuation
                        .ctx
                        .instance
                        .as_ref()
                        .expect("suspender namespace"),
                    resumer.ctx.instance.as_ref().expect("resumer namespace"),
                    self.budget,
                    &self.vm.limits,
                    pending.location.func,
                    true,
                )?;
                let mut captured = self.parents.split_off(index);
                let resumer = self.parents.pop().expect("matching resumer");
                captured.push(core::mem::replace(self.continuation, resumer));
                let reference = self.save(Saved::Suspended(captured), parameters, results)?;
                let mut values = request;
                values.push(Value::Continuation(reference));
                let site = self
                    .continuation
                    .control
                    .waiting
                    .control_mut()
                    .expect("waiting guest resume");
                site.rets = target.parameters;
                site.successor = Some((target.pc, target.instruction));
                Self::complete_control(self.continuation, values, self.vm.memory.aggs())?;
                self.budget.select(&self.continuation.control);
                return Ok(false);
            }
            HandlerTable::Native(handlers) => {
                let Handler::Suspend { handler, .. } = handlers.swap_remove(arm);
                handlers.clear();
                handler
            }
        };
        let control = self.new_control();
        // Validate the actual request across the code boundary before moving its source
        // stack. The generated continuation is execution-local metadata, appended below.
        let mut handler = self.child(callable, request, control, pending.location)?;
        let resumer = &self.parents[index - 1];
        let (location, depth) = resumer
            .control
            .waiting
            .site()
            .expect("resume delimiter retains its resumer");
        handler.parent = Some((
            resumer.control.activation(&resumer.ctx, depth - 1),
            location,
        ));
        handler.delimiter = Some(delimiter);
        let leaf = core::mem::replace(self.continuation, handler);
        let mut captured = self.parents.split_off(index);
        captured.push(leaf);
        let reference = self.save(Saved::Suspended(captured), parameters, results)?;
        self.continuation
            .args
            .to_mut()
            .push(Value::Continuation(reference));
        Ok(true)
    }

    fn complete_control(
        stack: &mut continuation::Continuation<'a>,
        values: Vec<Value>,
        heap: &AggHeap,
    ) -> Result<(), Trap> {
        let pending = stack
            .control
            .waiting
            .control_mut()
            .expect("waiting control operation");
        // An observer can panic. Keep the owned stack installed and poison the transfer
        // until every result observation has completed; it must never be retried.
        pending.abandoned = true;
        let pending = *pending;
        let frame = &stack.ctx.frames[pending.depth - 1];
        let destinations = stack
            .program
            .verified(frame.func)
            .expect("waiting definition")
            .vregs(pending.rets);
        if destinations.len() != values.len() {
            return Err(Trap::ArityMismatch);
        }
        let base = frame.base;
        for (&destination, value) in destinations.iter().zip(values) {
            stack.ctx.write_owned_value(base, destination, value)?;
        }
        if let Some((_, instruction)) = pending.successor {
            let frame = &mut stack.ctx.frames[pending.depth - 1];
            frame.instr_ix = instruction as usize;
            frame.pc = stack
                .program
                .verified(frame.func)
                .expect("resumer definition")
                .pc_at_ix(instruction as usize)
                .expect("typed target");
        }
        if let Some((target, _)) = pending.successor {
            observations::emit_edge(
                &mut stack.register_state,
                &mut stack.trace,
                stack.program.program(),
                &stack.ctx,
                pending.location,
                base,
                ValueSources::new(stack.program.program(), &stack.args, heap),
                target,
            );
        } else {
            observations::emit(
                &mut stack.register_state,
                &mut stack.trace,
                stack.program.program(),
                &stack.ctx,
                pending.location.func,
                pending.location.pc,
                base,
                ValueSources::new(stack.program.program(), &stack.args, heap),
                crate::trace::RegisterPhase::Outputs,
            );
        }
        let frame = &stack.ctx.frames[pending.depth - 1];
        let next = TraceLocation {
            func: frame.func,
            pc: frame.pc,
        };
        stack
            .trace
            .instr_end(stack.program.program(), Some(next), TraceOutcome::Ok);
        stack.control.waiting = Waiting::Ready;
        Ok(())
    }

    fn cancel_stack(&mut self, saved: Saved<'a>) {
        cancel_stack(saved, self.budget);
    }
}

fn cancel_stack(saved: Saved<'_>, budget: &mut Budget) {
    budget.frames -= saved.frames();
    saved.cancel();
}

impl<'state, H: Host> Running<'_, 'state, H> {
    pub(super) fn retire_saved(&mut self) -> impl Iterator<Item = Saved<'state>> + use<'state, H> {
        self.budget.frames -= self
            .resumptions
            .iter()
            .map(|(_, saved)| saved.frames())
            .sum::<usize>();
        self.resumptions.close()
    }

    pub(super) fn cancel_saved(&mut self) {
        // Invalidate all aliases and retire every frame charge before observing any sibling.
        for stack in self.retire_saved() {
            stack.cancel();
        }
    }
}

impl<H: Host> Running<'_, '_, H> {
    pub(super) fn restore_after_return(&mut self) -> Result<RunStatus, TrapInfo> {
        if self.continuation.delimiter.is_none() {
            return self.restore_parent();
        }
        let location = self.continuation.returned_at.expect("actual return");
        let parent = self.parents.pop().expect("resumer");
        let values = core::mem::take(&mut self.continuation.outputs);
        // Reinstall the transaction owner before notifying arbitrary observers.
        *self.continuation = parent;
        self.budget.select(&self.continuation.control);
        // The returning root's ordinary boundary has already checked final contracts.
        let status = if self.continuation.control.waiting.control().is_some() {
            if let Err(trap) =
                Self::complete_control(self.continuation, values, self.vm.memory.aggs())
            {
                return Err(self.control_trap(location, trap));
            }
            RunStatus::Suspended
        } else {
            self.continuation.control.callback_results = Some(values);
            RunStatus::AwaitingHost
        };
        Ok(status)
    }
}

impl<H: Host> PendingHostCall<'_, '_, H> {
    /// Creates a one-use computation without running it or completing this host request.
    ///
    /// The qualified function or closure must belong to this store. Its original exposed
    /// arguments become the initial resume inputs; later suspensions carry their own response
    /// contracts. Saved storage and hard budgets belong to this execution. The capability may
    /// be cloned, but resuming or discarding any alias consumes every alias.
    ///
    /// Consumes this view while leaving the host operation pending. Query
    /// [`Invocation::pending_host_call`] again for the next command. A fatal admission cannot
    /// leave a reusable view into terminated execution.
    ///
    /// Foreign-store denial preserves the pending request and spends no guest work. An admitted
    /// operation promotes staging, spends one hard-fuel unit, and may terminate on a hard limit.
    pub fn create_continuation(
        mut self,
        callee: impl Into<Value>,
    ) -> Result<ContinuationRef, ControlError> {
        let callee = callee.into();
        if let Value::Func(function) = &callee
            && !self.running.vm.owns_instance(function.instance())
        {
            return Err(ControlError::ForeignStore);
        }
        if let Value::Closure(closure) = &callee
            && !self.running.vm.owns_instance(closure.func.instance())
        {
            return Err(ControlError::ForeignStore);
        }
        self.delivering = true;
        let location = self.location();
        let result = (|| {
            // Staged environments become ordinary execution storage before validation.
            self.running.native_control_work()?;
            let (target, environment) = self.running.control_callable(&callee)?;
            let program = target.instance().program();
            let body = &program.program().functions[target.id().0 as usize];
            let mut parameters = body.arg_types;
            if environment.is_some() {
                if parameters.len == 0 {
                    return Err(Trap::ArityMismatch);
                }
                parameters.offset += 1;
                parameters.len -= 1;
            }
            let parameters = ContractOwner {
                program: program.clone(),
                range: parameters,
            };
            let results = ContractOwner {
                program: program.clone(),
                range: body.ret_types,
            };
            let control = self.running.new_control();
            self.running.save(
                Saved::New {
                    callable: callee,
                    control,
                },
                parameters,
                results,
            )
        })();
        self.delivering = false;
        result.map_err(|trap| self.running.fail_native_control(location, trap))
    }

    /// Consumes a continuation under these handlers, leaving this host operation pending.
    ///
    /// Drive the same invocation with `resume`. On normal completion it reports `AwaitingHost`;
    /// retrieve the computed values with [`Self::take_callback_results`] before completing the
    /// original host request. Handler clauses execute as guest code outside the captured stack.
    /// No native Rust frame is captured. Wrong-execution and stale-authority denials preserve
    /// this request and valid capabilities elsewhere without spending guest work.
    pub fn resume_continuation(
        mut self,
        continuation: &ContinuationRef,
        args: Vec<Value>,
        handlers: &[Handler],
    ) -> Result<(), ControlError> {
        self.running
            .resumptions
            .get(continuation.id())
            .map_err(|error| ControlError::Resumption(error.into()))?;
        self.delivering = true;
        let location = self.location();
        let admitted = (|| {
            // Rich inputs and handler closure environments may use staged host handles.
            self.running.native_control_work()?;
            self.running.check_control_values(
                (
                    &continuation.parameter_owner().program,
                    continuation.parameter_owner().range,
                ),
                &args,
                ControlBoundary::Resume,
            )?;
            let delimiter = Delimiter {
                handlers: HandlerTable::Native(handlers.to_vec()),
                results: continuation.result_owner().clone(),
            };
            self.running.check_handlers(&delimiter)?;
            if let Saved::Suspended(stack) = self
                .running
                .resumptions
                .get(continuation.id())
                .expect("checked authority")
            {
                composition::check_transfer(
                    self.running.vm.memory.aggs(),
                    &args,
                    self.running
                        .continuation
                        .ctx
                        .instance
                        .as_ref()
                        .expect("host namespace"),
                    stack
                        .last()
                        .expect("captured leaf")
                        .ctx
                        .instance
                        .as_ref()
                        .expect("captured namespace"),
                    self.running.budget,
                    &self.running.vm.limits,
                    location.func,
                    true,
                )?;
            }
            let payload = args.iter().try_fold(0_usize, |total, value| {
                let bytes = match value {
                    Value::Str(text) => text.len(),
                    Value::Bytes(bytes) => bytes.len(),
                    _ => 0,
                };
                total.checked_add(bytes).ok_or(Trap::PayloadSizeOverflow)
            })?;
            self.running.budget.data.charge_payload_bytes(payload, 1)?;
            let saved = self
                .running
                .resumptions
                .take(continuation.id())
                .map_err(|error| Trap::Continuation(error.into()))?;
            self.running.continuation.control.callback_results = None;
            self.running.attach_saved(saved, args, delimiter, location)
        })();
        let result = match admitted {
            Err(trap) => Err(self.running.fail_native_control(location, trap)),
            Ok(true) => self
                .running
                .continuation
                .initialize(self.running.vm, self.running.budget, DataWork::default())
                .map_err(|error| {
                    self.running.finish(Err(error.clone()));
                    ControlError::Trapped(error)
                }),
            Ok(false) => Ok(()),
        };
        self.delivering = false;
        result
    }

    /// Consumes a saved computation and releases its frames without running guest cleanup.
    /// Consumes this view; the host operation remains pending. A denied foreign or stale
    /// capability preserves that operation, which can be queried again.
    pub fn discard_continuation(
        mut self,
        continuation: &ContinuationRef,
    ) -> Result<(), ControlError> {
        self.running
            .resumptions
            .get(continuation.id())
            .map_err(|error| ControlError::Resumption(error.into()))?;
        self.delivering = true;
        let location = self.location();
        let result = (|| {
            self.running.native_control_work()?;
            let saved = self
                .running
                .resumptions
                .take(continuation.id())
                .map_err(|error| Trap::Continuation(error.into()))?;
            self.running.cancel_stack(saved);
            Ok(())
        })();
        self.delivering = false;
        result.map_err(|trap| self.running.fail_native_control(location, trap))
    }
}

impl<H: Host> Running<'_, '_, H> {
    fn native_control_work(&mut self) -> Result<(), Trap> {
        if self.budget.fuel == 0 {
            return Err(Trap::FuelExceeded);
        }
        self.budget.fuel -= 1;
        let mut staging = self.continuation.host_results.take().unwrap_or_default();
        staging
            .promote(self.vm.memory.aggs_mut())
            .map_err(Trap::AggError)?;
        Ok(())
    }
    fn fail_native_control(&mut self, location: TraceLocation, trap: Trap) -> ControlError {
        let error = self.control_trap(location, trap);
        self.finish(Err(error.clone()));
        ControlError::Trapped(error)
    }
}
