// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Execution-owned control composition. Segments share storage and a single transaction.

use super::*;
use crate::trace::{ControlEvent, EntryOrigin};
use crate::value::AggType;
use crate::value::FunctionRef;
use alloc::collections::BTreeSet;

impl<'state, H: Host> Running<'_, 'state, H> {
    pub(super) fn new_control(&mut self) -> ExecutionControl {
        let mut control = ExecutionControl::start(self.continuation.control.yield_host_calls);
        control.inherit_execution(&self.continuation.control);
        control.segment = *self.next_segment;
        *self.next_segment = self
            .next_segment
            .checked_add(1)
            .expect("segment creation spends hard fuel");
        control
    }

    /// Build owned control state uniformly; admission and initialization remain explicit.
    pub(super) fn segment(
        &mut self,
        target: &FunctionRef,
        environment: Option<AggHandle>,
        mut args: Vec<Value>,
        control: ExecutionControl,
        location: TraceLocation,
    ) -> continuation::Continuation<'state> {
        if let Some(environment) = environment {
            args.insert(0, Value::Agg(environment));
        }
        let mut child = continuation::Continuation::with_control(
            CodeOwner::Instance(target.instance().clone()),
            target.id(),
            Cow::Owned(args),
            InvocationOptions::default(),
            control,
        );
        child.trace.mask = self.continuation.trace.mask;
        child.trace.sink = Observer::share(&mut self.continuation.trace.sink);
        child.access = Observer::share(&mut self.continuation.access);
        child.parent = Some((
            self.continuation.control.activation(
                &self.continuation.ctx,
                self.continuation.ctx.frames.len() - 1,
            ),
            location,
        ));
        child
    }

    pub(super) fn admit_segment(
        &mut self,
        child: &continuation::Continuation<'_>,
        environment: Option<AggHandle>,
    ) -> Result<(), Trap> {
        let target = child.program.instance().expect("qualified segment");
        let mut args = child.args.as_ref();
        if environment.is_some() {
            // A closure carries its environment's code interpretation. Only explicit
            // arguments originate in the invoking definition.
            check_transfer(
                self.vm.memory.aggs(),
                &args[..1],
                target,
                target,
                self.budget,
                &self.vm.limits,
                child.entry,
                true,
            )?;
            args = &args[1..];
        }
        check_transfer(
            self.vm.memory.aggs(),
            args,
            self.continuation
                .ctx
                .instance
                .as_ref()
                .expect("source namespace"),
            target,
            self.budget,
            &self.vm.limits,
            child.entry,
            true,
        )
    }

    pub(super) fn has_parents(&self) -> bool {
        !self.parents.is_empty()
    }

    pub(super) fn check_return_values(&mut self, values: &[Value]) -> Result<(), Trap> {
        let Some(parent) = self.parents.last() else {
            return Ok(());
        };
        check_transfer(
            self.vm.memory.aggs(),
            values,
            self.continuation
                .ctx
                .instance
                .as_ref()
                .expect("child instance"),
            parent.ctx.instance.as_ref().expect("parent instance"),
            self.budget,
            &self.vm.limits,
            self.continuation.entry,
            false,
        )
    }

    fn check_return_transfer(&mut self) -> Result<(), Trap> {
        let Some(parent) = self.parents.last() else {
            return Ok(());
        };
        check_transfer(
            self.vm.memory.aggs(),
            &self.continuation.outputs,
            self.continuation
                .ctx
                .instance
                .as_ref()
                .expect("child instance"),
            parent.ctx.instance.as_ref().expect("parent instance"),
            self.budget,
            &self.vm.limits,
            self.continuation.entry,
            false,
        )
    }

    pub(super) fn enter_segment(
        &mut self,
        target: &FunctionRef,
        environment: Option<AggHandle>,
        args: Vec<Value>,
        origin: EntryOrigin,
    ) -> Result<(), TrapInfo> {
        let (location, _) = self
            .continuation
            .control
            .waiting
            .site()
            .expect("pending call");
        let span = self.continuation.ctx.span_at(
            self.continuation.program.program(),
            location.func,
            location.pc,
        );
        self.continuation.control.enable_identity();
        let control = self.new_control();
        let child = self.segment(target, environment, args, control, location);
        let instance = target.instance();
        let entry = target.id();
        let callee = match environment {
            Some(env) => Value::Closure(Closure {
                func: target.clone(),
                env,
            }),
            None => Value::Func(target.clone()),
        };
        let parent_id = child.parent.as_ref().expect("source activation").0.clone();
        let child_id = child.control.root_activation();
        let result = (|| {
            let mut staged = self.continuation.host_results.take().unwrap_or_default();
            let work = staged.work;
            let created = staged
                .promote(self.vm.memory.aggs_mut())
                .map_err(Trap::AggError)?;
            self.continuation
                .trace
                .control(self.continuation.program.program(), || {
                    ControlEvent::EnterInstance {
                        origin,
                        location,
                        parent: &parent_id,
                        child: &child_id,
                        source_instance: self
                            .continuation
                            .ctx
                            .instance
                            .as_ref()
                            .expect("source instance"),
                        instance,
                        entry,
                        callee: &callee,
                        args: &child.args[usize::from(environment.is_some())..],
                        work,
                        created: &created,
                        aggregates: self.vm.memory.aggs(),
                    }
                });
            if origin == EntryOrigin::HostRequest {
                if self.budget.fuel == 0 {
                    return Err(Trap::FuelExceeded);
                }
                self.budget.fuel -= 1;
                let payload = child.args.iter().try_fold(0_usize, |total, value| {
                    let bytes = match value {
                        Value::Bytes(bytes) => bytes.len(),
                        Value::Str(text) => text.len(),
                        _ => 0,
                    };
                    total.checked_add(bytes).ok_or(Trap::PayloadSizeOverflow)
                })?;
                self.budget.data.charge_payload_bytes(payload, 1)?;
            }
            self.admit_segment(&child, environment)
                .map_err(|trap| trap.in_contract_definition(instance.program()))?;
            Ok(())
        })();
        if let Err(trap) = result {
            let error = self
                .continuation
                .ctx
                .trap(location.func, location.pc, span, trap);
            self.continuation
                .trace
                .control(self.continuation.program.program(), || {
                    ControlEvent::RejectInstance {
                        location,
                        parent: &parent_id,
                        child: &child_id,
                        error: &error,
                    }
                });
            self.finish(Err(error.clone()));
            return Err(error);
        }
        // Install both owners before initialization can notify arbitrary observers.
        let parent = core::mem::replace(self.continuation, child);
        self.parents.push(parent);
        let result = self
            .continuation
            .initialize(self.vm, self.budget, DataWork::default());
        if let Err(error) = result {
            self.finish(Err(error.clone()));
            return Err(error);
        }
        Ok(())
    }
}

impl<H: Host> Invocation<'_, H> {
    pub(super) fn finish_segment_return(&mut self, site: ReturnSite) -> Result<(), TrapInfo> {
        self.running().check_return_transfer().map_err(|trap| {
            self.continuation
                .ctx
                .trap(site.location.func, site.location.pc, site.span, trap)
        })?;
        let continuation = &mut self.continuation;
        continuation.returned_at = Some(site.location);
        continuation.trace.call_scope_exit_if_enabled(
            continuation.program.program(),
            continuation.ctx.frames.len(),
            site.location.func,
            site.location.pc,
            site.span,
        );
        continuation
            .trace
            .instr_end(continuation.program.program(), None, TraceOutcome::Ok);
        self.budget.frames -= 1;
        self.running().finish(Ok(()));
        Ok(())
    }

    pub(super) fn restore_parent(&mut self) -> Result<RunStatus, TrapInfo> {
        let mut parent = self.parents.pop().expect("successful child has parent");
        let child_id = self.continuation.control.root_activation();
        let instance = self
            .continuation
            .ctx
            .instance
            .clone()
            .expect("child instance");
        let location = self
            .continuation
            .returned_at
            .expect("actual child return site");
        if let Some(pending) = parent.control.waiting.guest() {
            let parent_id = parent.control.activation(&parent.ctx, pending.depth - 1);
            let values = core::mem::take(&mut self.continuation.outputs);
            self.continuation = parent;
            let parent = &mut self.continuation;
            parent
                .control
                .waiting
                .guest_mut()
                .expect("waiting guest call")
                .abandoned = true;
            parent
                .trace
                .control(parent.program.program(), || ControlEvent::ReturnInstance {
                    location,
                    child: &child_id,
                    parent: &parent_id,
                    instance: &instance,
                    values: &values,
                    aggregates: self.vm.memory.aggs(),
                });
            let frame = &parent.ctx.frames[pending.depth - 1];
            let base = frame.base;
            let function = parent
                .program
                .verified(frame.func)
                .expect("retained caller definition");
            let destinations = function.vregs(pending.rets);
            if destinations.len() != values.len() {
                return Err(parent.ctx.trap(
                    pending.location.func,
                    pending.location.pc,
                    None,
                    Trap::ArityMismatch,
                ));
            }
            for (&destination, value) in destinations.iter().zip(values) {
                parent
                    .ctx
                    .write_owned_value(base, destination, value)
                    .map_err(|trap| {
                        parent
                            .ctx
                            .trap(pending.location.func, pending.location.pc, None, trap)
                    })?;
            }
            let next = pending.instruction + 1;
            let pc = function
                .pc_at_ix(next)
                .expect("verified guest call has a successor");
            let frame = &mut parent.ctx.frames[pending.depth - 1];
            frame.pc = pc;
            frame.instr_ix = next;
            parent.control.waiting = Waiting::Ready;
            self.budget.select(&parent.control);
            return Ok(RunStatus::Suspended);
        }
        let pending = parent
            .control
            .waiting
            .host()
            .expect("suspended parent host");
        let parent_id = parent.control.activation(&parent.ctx, pending.depth - 1);
        parent.control.callbacks.get_or_insert_default().returned =
            Some(core::mem::take(&mut self.continuation.outputs));
        // Install the parent before observers run. A panicking observer cannot lose the live
        // transaction owner or allow this transition to be observed a second time.
        self.continuation = parent;
        self.continuation
            .control
            .waiting
            .host_mut()
            .expect("resumed host")
            .abandoned = true;
        let parent = &mut self.continuation;
        parent
            .trace
            .control(parent.program.program(), || ControlEvent::ReturnInstance {
                location,
                child: &child_id,
                parent: &parent_id,
                instance: &instance,
                values: parent
                    .control
                    .callbacks
                    .as_ref()
                    .expect("return slots")
                    .returned
                    .as_deref()
                    .expect("child return"),
                aggregates: self.vm.memory.aggs(),
            });
        self.continuation
            .control
            .waiting
            .host_mut()
            .expect("resumed host")
            .abandoned = false;
        self.budget.select(&self.continuation.control);
        Ok(RunStatus::AwaitingHost)
    }
}

pub(in crate::vm) fn check_transfer(
    heap: &AggHeap,
    values: &[Value],
    source: &Instance,
    target: &Instance,
    budget: &mut Budget,
    limits: &Limits,
    function: FuncId,
    arguments: bool,
) -> Result<(), Trap> {
    let same_definition = source.program().shares_definition_with(target.program());
    let mut seen = BTreeSet::new();
    for (index, value) in values.iter().enumerate() {
        let mut work = vec![(core::slice::from_ref(value).iter(), false)];
        while let Some((frame, encapsulated)) = work.last_mut() {
            let Some(value) = frame.next() else {
                work.pop();
                continue;
            };
            let encapsulated = *encapsulated;
            budget
                .contract_scratch
                .charge(limits.max_contract_work)
                .map_err(|_| Trap::ContractWorkExceeded {
                    function,
                    position: if arguments {
                        ContractPosition::Argument {
                            callee: function,
                            index: u32::try_from(index).unwrap_or(u32::MAX),
                        }
                    } else {
                        ContractPosition::Result {
                            index: u32::try_from(index).unwrap_or(u32::MAX),
                        }
                    },
                    limit: limits.max_contract_work,
                })?;
            match value {
                Value::Func(function) => {
                    if !Arc::ptr_eq(&function.instance().0.store, &target.0.store) {
                        return Err(Trap::ForeignStoreValue {
                            kind: ValueType::Func,
                        });
                    }
                }
                Value::Closure(closure) => {
                    if !Arc::ptr_eq(&closure.func.instance().0.store, &target.0.store) {
                        return Err(Trap::ForeignStoreValue {
                            kind: ValueType::Closure,
                        });
                    }
                    work.push((
                        heap.fields(closure.env).map_err(Trap::AggError)?.iter(),
                        true,
                    ));
                }
                Value::Agg(handle) if seen.insert((handle.0, encapsulated)) => {
                    let kind = heap.agg_type(*handle).map_err(Trap::AggError)?;
                    // Nominal captures stay inside a code-qualified closure. Passing that
                    // closure does not expose its private schemas to the receiving code.
                    if !encapsulated && !same_definition && !matches!(kind, AggType::Tuple { .. }) {
                        return Err(Trap::CrossInstanceValue {
                            kind: ValueType::Agg,
                        });
                    }
                    work.push((
                        heap.fields(*handle).map_err(Trap::AggError)?.iter(),
                        encapsulated,
                    ));
                }
                _ => {}
            }
        }
    }
    Ok(())
}
