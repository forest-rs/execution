// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Checked composition drives the ordinary control owner in one fresh store.

use super::*;
use crate::{
    instance::{Instance, InstanceError},
    recording::execution::{self as evidence, Action as TransferAction, RecordedActivation},
    trace::{ControlEvent, RegisterEvent, RegisterPhase},
    vm::{InvocationOptions, RunStatus},
};
use alloc::{boxed::Box, vec};

#[derive(Default)]
pub(super) struct Identities {
    pub(super) controls: ControlBindings,
    /// Original execution's preexisting prefix and replay's restored prefix. Segment entry
    /// prefixes differ: an older guest value can first become observable in a later child.
    pub(super) heap_base: u32,
    pub(super) initial_count: u32,
    forward: BTreeMap<u32, AggHandle>,
    inverse: BTreeMap<u32, u32>,
}
impl Identities {
    pub(super) fn bind(&mut self, origin: u32, actual: AggHandle) -> bool {
        if self
            .forward
            .get(&origin)
            .is_some_and(|&handle| handle != actual)
            || self.inverse.get(&actual.0).is_some_and(|&id| id != origin)
        {
            return false;
        }
        self.forward.insert(origin, actual);
        self.inverse.insert(actual.0, origin);
        true
    }
    pub(super) fn handle(&self, origin: u32) -> Option<AggHandle> {
        self.forward.get(&origin).copied()
    }
    pub(super) fn candidate(&self, origin: u32) -> Option<AggHandle> {
        self.handle(origin).or_else(|| {
            origin
                .checked_sub(self.heap_base)?
                .checked_add(self.initial_count)
                .map(AggHandle)
        })
    }
}

struct Context<'a> {
    root: &'a Recording,
    data: &'a evidence::ExecutionData,
    states: Vec<Option<Rc<RefCell<State>>>>,
    instances: Vec<Instance>,
    stack: Vec<usize>,
    selected: usize,
    pending: Option<usize>,
    next: usize,
    started: Vec<bool>,
    finished: Vec<bool>,
    denied: Vec<bool>,
    locations: Vec<Option<TraceLocation>>,
    target: Option<RecordedActivation>,
    failure: Option<u64>,
    error: Option<ReplayError>,
}

fn endpoint(id: &crate::vm::ActivationId) -> RecordedActivation {
    RecordedActivation {
        segment: id.segment_ordinal(),
        activation: id.ordinal(),
    }
}
fn limits_match(a: &crate::vm::Limits, b: &crate::vm::Limits) -> bool {
    a.fuel == b.fuel
        && a.max_call_depth == b.max_call_depth
        && a.max_continuations == b.max_continuations
        && a.max_host_calls == b.max_host_calls
        && a.max_contract_work == b.max_contract_work
        && a.max_aggregate_work == b.max_aggregate_work
        && a.max_payload_work == b.max_payload_work
}

impl<'a> Context<'a> {
    fn record(&self, index: usize) -> Option<&'a Recording> {
        self.data.evidence(self.root, index)
    }
    fn current(&self) -> usize {
        self.selected
    }
    fn index_for(&self, event: crate::trace::TraceContext<'_>) -> Option<usize> {
        event.stack().map_or(Some(self.current()), |stack| {
            self.data.index(stack.segment_ordinal())
        })
    }
    fn select(&mut self, event: crate::trace::TraceContext<'_>) -> Option<usize> {
        let Some(index) = self.index_for(event) else {
            self.fail(self.current(), Divergence::CallStack);
            return None;
        };
        self.selected = index;
        Some(index)
    }
    fn qualify(&self, index: usize, error: ReplayError) -> ReplayError {
        ReplayError::InSegment {
            segment: self.data.segments[index].id,
            error: Box::new(error),
        }
    }
    fn mismatch(&self, index: usize, reason: Divergence) -> ReplayError {
        let error = if let Some(state) = &self.states[index] {
            state.borrow().mismatch(reason)
        } else {
            ReplayError::Diverged {
                boundary: ReplayBoundary::Completion,
                location: None,
                reason,
            }
        };
        self.qualify(index, error)
    }
    fn fail(&mut self, index: usize, reason: Divergence) {
        if self.error.is_none() {
            self.error = Some(self.mismatch(index, reason));
        }
    }
    fn check_program(&mut self, index: usize, program: &Program) -> bool {
        if self.error.is_some() {
            return false;
        }
        if !core::ptr::eq(self.data.segments[index].program.program(), program) {
            self.fail(index, Divergence::Outcome);
            false
        } else {
            true
        }
    }
    fn dispatch(&mut self, index: usize, program: &Program, callback: impl FnOnce(&mut Check<'_>)) {
        if !self.check_program(index, program) {
            return;
        }
        let Some(record) = self.record(index) else {
            self.fail(0, Divergence::Outcome);
            return;
        };
        let state = Rc::clone(self.states[index].as_ref().expect("segment state"));
        callback(&mut Check {
            record,
            state: Rc::clone(&state),
        });
        if let Some(error) = &state.borrow().error {
            self.error = Some(self.qualify(index, error.clone()));
        }
    }
    fn check_control(
        &mut self,
        program: &Program,
        event: ControlEvent<'_>,
    ) -> Result<(), ReplayError> {
        let index = self.current();
        if !self.check_program(index, program) {
            return Err(self.error.clone().expect("program divergence"));
        }
        let Some(transfer) = self.data.transfers.get(self.next) else {
            return Err(self.mismatch(
                index,
                Divergence::ControlCommands {
                    expected: self.data.transfers.len(),
                    actual: self.next + 1,
                },
            ));
        };
        let record = self.record(index).expect("active segment evidence");
        let mut state = self.states[index]
            .as_ref()
            .expect("segment state")
            .borrow_mut();
        state.boundary = ReplayBoundary::Transfer { index: self.next };
        if transfer.segment != self.data.segments[index].id || transfer.sequence != state.sequence {
            return Err(self.qualify(
                index,
                state.mismatch(Divergence::ObservationOrder {
                    expected: transfer.sequence,
                    actual: state.sequence,
                }),
            ));
        }
        let actual_location = match event {
            ControlEvent::EnterInstance { location, .. }
            | ControlEvent::TailTransfer { location, .. }
            | ControlEvent::RejectInstance { location, .. }
            | ControlEvent::ReturnInstance { location, .. }
            | ControlEvent::ReturnAcross { location, .. } => location,
        };
        if transfer.location != actual_location {
            let owner = match event {
                ControlEvent::ReturnInstance { child, .. } => {
                    self.data.index(child.segment_ordinal()).unwrap_or(index)
                }
                _ => index,
            };
            return Err(self.qualify(
                owner,
                ReplayError::Diverged {
                    boundary: ReplayBoundary::Transfer { index: self.next },
                    location: Some(actual_location),
                    reason: Divergence::Location {
                        expected: transfer.location,
                        actual: Some(actual_location),
                    },
                },
            ));
        }
        state.location = if matches!(event, ControlEvent::ReturnInstance { .. }) {
            self.locations[index]
        } else {
            Some(actual_location)
        };
        let result = match (&transfer.action, event) {
            (
                TransferAction::Enter {
                    origin,
                    parent,
                    child,
                    callee,
                    args,
                    work,
                    constructed,
                },
                ControlEvent::EnterInstance {
                    origin: actual_origin,
                    parent: actual_parent,
                    child: actual_child,
                    source_instance,
                    instance,
                    entry,
                    callee: actual_callee,
                    args: actual_args,
                    work: actual_work,
                    created,
                    aggregates,
                    ..
                },
            ) => {
                let target = self
                    .data
                    .index(child.segment)
                    .ok_or_else(|| state.mismatch(Divergence::Outcome))?;
                let meta = &self.data.segments[target];
                if *origin != actual_origin
                    || endpoint(actual_parent) != *parent
                    || endpoint(actual_child) != *child
                    || meta.parent != Some(*parent)
                    || meta.entry != Some(entry)
                    || *work != actual_work
                    || meta.instance.and_then(|id| self.instances.get(id)) != Some(instance)
                    || self.data.segments[index]
                        .instance
                        .and_then(|id| self.instances.get(id))
                        != Some(source_instance)
                {
                    return Err(self.qualify(index, state.mismatch(Divergence::CallStack)));
                }
                check_created(record, &mut state, constructed, created)?;
                state.matches(
                    record,
                    record.arena.slice(callee),
                    [ValueRef::from_value(actual_callee)],
                    aggregates,
                )?;
                state.matches(
                    record,
                    record.arena.slice(args),
                    actual_args.iter().map(ValueRef::from_value),
                    aggregates,
                )?;
                self.pending = Some(target);
                Ok(())
            }
            (
                TransferAction::Rejected { parent, child },
                ControlEvent::RejectInstance {
                    parent: actual_parent,
                    child: actual_child,
                    error,
                    ..
                },
            ) => {
                let target = self
                    .data
                    .index(child.segment)
                    .ok_or_else(|| state.mismatch(Divergence::Outcome))?;
                if endpoint(actual_parent) != *parent
                    || endpoint(actual_child) != *child
                    || self.pending != Some(target)
                    || !self.data.segments[target].denied
                    || !matches!(&record.end, Some(End::Trapped(expected)) if state.same_trap(expected, error))
                {
                    return Err(self.qualify(index, state.mismatch(Divergence::Outcome)));
                }
                self.denied[target] = true;
                self.pending = None;
                Ok(())
            }
            (
                TransferAction::Returned {
                    child,
                    parent,
                    values,
                },
                ControlEvent::ReturnInstance {
                    child: actual_child,
                    parent: actual_parent,
                    instance,
                    values: actual_values,
                    aggregates,
                    ..
                },
            ) => {
                let child_index = self
                    .data
                    .index(child.segment)
                    .ok_or_else(|| state.mismatch(Divergence::Outcome))?;
                if endpoint(actual_child) != *child
                    || endpoint(actual_parent) != *parent
                    || !self.finished[child_index]
                    || self.data.segments[child_index].parent != Some(*parent)
                    || self.data.segments[child_index]
                        .instance
                        .and_then(|id| self.instances.get(id))
                        != Some(instance)
                {
                    return Err(self.qualify(index, state.mismatch(Divergence::CallStack)));
                }
                state.matches(
                    record,
                    record.arena.slice(values),
                    actual_values.iter().map(ValueRef::from_value),
                    aggregates,
                )
            }
            (
                TransferAction::ReturnFrom {
                    source,
                    target,
                    target_location,

                    values,
                    work,
                    constructed,
                },
                ControlEvent::ReturnAcross {
                    source: actual_source,
                    target: actual_target,
                    instance,
                    target_location: actual_target_location,

                    values: actual_values,
                    work: actual_work,
                    created,
                    aggregates,
                    ..
                },
            ) => {
                let target_index = self
                    .data
                    .index(target.segment)
                    .ok_or_else(|| state.mismatch(Divergence::Outcome))?;
                if endpoint(actual_source) != *source
                    || endpoint(actual_target) != *target
                    || *work != actual_work
                    || *target_location != actual_target_location
                    || target_index == index
                    || !self.stack.contains(&target_index)
                    || self.data.segments[target_index]
                        .instance
                        .and_then(|id| self.instances.get(id))
                        != Some(instance)
                {
                    return Err(self.qualify(index, state.mismatch(Divergence::CallStack)));
                }
                check_created(record, &mut state, constructed, created)?;
                state.matches(
                    record,
                    record.arena.slice(values),
                    actual_values.iter().map(ValueRef::from_value),
                    aggregates,
                )?;
                self.target = Some(*target);
                Ok(())
            }
            _ => Err(state.mismatch(Divergence::Outcome)),
        };
        result.map_err(|error| self.qualify(index, error))?;
        state.sequence += 1;
        self.next += 1;
        Ok(())
    }
    fn begin(&mut self, program: &Program) {
        if self.error.is_some() {
            return;
        }
        let index = if self.stack.is_empty() && !self.started[0] {
            0
        } else if let Some(pending) = self.pending.take() {
            pending
        } else {
            self.fail(self.current(), Divergence::CallStack);
            return;
        };
        if self.record(index).is_none() || self.started[index] {
            self.fail(self.current(), Divergence::Outcome);
            return;
        }
        if !self.check_program(index, program) {
            return;
        }
        self.started[index] = true;
        self.stack.push(index);
    }
    fn end(&mut self, program: &Program, outcome: TraceOutcome<'_>) {
        if self.error.is_some() {
            return;
        }
        let index = self.current();
        if !self.check_program(index, program) {
            return;
        }
        let record = self.record(index).expect("ending segment");
        let mut state = self.states[index]
            .as_ref()
            .expect("segment state")
            .borrow_mut();
        if let TraceOutcome::Trap(_) = outcome
            && self.failure.is_none()
        {
            self.failure = Some(self.data.segments[index].id);
        }
        let matches = match (&record.end, outcome) {
            (Some(End::Returned), TraceOutcome::Ok) => state.candidate,
            (Some(End::Trapped(expected)), TraceOutcome::Trap(actual)) => {
                state.same_trap(expected, actual)
            }
            (Some(End::Transferred { target, .. }), TraceOutcome::Unwound { target: actual }) => {
                self.target == Some(*target) && target.activation == actual
            }
            _ => false,
        };
        if !matches {
            self.error = Some(self.qualify(index, state.mismatch(Divergence::Outcome)));
            return;
        }
        if let Err(error) = check_finished(record, &mut state) {
            self.error = Some(self.qualify(index, error));
            return;
        }
        if record.local_outcome.is_some() && record.local_outcome != self.locations[index] {
            self.error = Some(self.qualify(index, state.mismatch(Divergence::CallStack)));
            return;
        }
        self.finished[index] = true;
        if self.stack.last() == Some(&index) {
            self.stack.pop();
        }
    }
}

struct Observer<'a>(Rc<RefCell<Context<'a>>>);
impl TraceSink for Observer<'_> {
    fn run_mask(&mut self, event_context: crate::trace::TraceContext<'_>, _: TraceMask) {
        let program = event_context.program();
        let mut context = self.0.borrow_mut();
        let Some(index) = context.select(event_context) else {
            return;
        };
        context.check_program(index, program);
    }
    fn run_start(&mut self, event_context: crate::trace::TraceContext<'_>, _: FuncId, _: usize) {
        let program = event_context.program();
        let mut context = self.0.borrow_mut();
        if context.select(event_context).is_some() {
            context.begin(program);
        }
    }
    fn run_end(
        &mut self,
        event_context: crate::trace::TraceContext<'_>,
        outcome: TraceOutcome<'_>,
    ) {
        let program = event_context.program();
        let mut context = self.0.borrow_mut();
        if context.select(event_context).is_some() {
            context.end(program, outcome);
        }
    }
    fn wants_registers(
        &self,
        event_context: crate::trace::TraceContext<'_>,
        location: TraceLocation,
        phase: RegisterPhase,
    ) -> bool {
        let context = self.0.borrow();
        if context.error.is_some() {
            return false;
        }
        let Some(index) = context.index_for(event_context) else {
            return false;
        };
        let record = context.record(index).expect("active record");
        Check {
            record,
            state: Rc::clone(context.states[index].as_ref().expect("state")),
        }
        .wants_registers(event_context, location, phase)
    }
    fn registers(
        &mut self,
        event_context: crate::trace::TraceContext<'_>,
        event: RegisterEvent<'_>,
    ) {
        let program = event_context.program();
        let mut context = self.0.borrow_mut();
        let Some(index) = context.select(event_context) else {
            return;
        };
        context.dispatch(index, program, |check| {
            check.registers(event_context, event);
        });
    }
    fn control(&mut self, event_context: crate::trace::TraceContext<'_>, event: ControlEvent<'_>) {
        let program = event_context.program();
        let mut context = self.0.borrow_mut();
        if context.error.is_some() || context.select(event_context).is_none() {
            return;
        }
        if let Err(error) = context.check_control(program, event) {
            context.error = Some(if matches!(error, ReplayError::InSegment { .. }) {
                error
            } else {
                context.qualify(context.current(), error)
            });
        }
    }
    fn values(&mut self, event_context: crate::trace::TraceContext<'_>, event: ValueEvent<'_>) {
        let program = event_context.program();
        let mut context = self.0.borrow_mut();
        let Some(index) = context.select(event_context) else {
            return;
        };
        if context.error.is_some() {
            return;
        }
        if let ValueEvent::Entry {
            entry,
            args,
            aggregates,
            limits,
            work,
        } = event
        {
            let record = context.record(index).expect("entry record");
            let expected = record.entry.as_ref().expect("captured entry");
            let result = {
                let mut state = context.states[index].as_ref().expect("state").borrow_mut();
                state.initial_count = aggregates.len_u32();
                if entry != expected.func
                    || work != expected.work
                    || !limits_match(limits, &expected.limits)
                {
                    Err(state.mismatch(Divergence::Outcome))
                } else {
                    state.matches(
                        record,
                        record.arena.slice(&expected.args),
                        args.iter().map(ValueRef::from_value),
                        aggregates,
                    )
                }
            };
            if let Err(error) = result {
                context.error = Some(context.qualify(index, error));
                return;
            }
        }
        context.dispatch(index, program, |check| check.values(event_context, event));
    }
    fn instr(
        &mut self,
        event_context: crate::trace::TraceContext<'_>,
        func: FuncId,
        pc: u32,
        next: u32,
        span: Option<u64>,
        opcode: u8,
    ) {
        let program = event_context.program();
        let mut context = self.0.borrow_mut();
        let Some(index) = context.select(event_context) else {
            return;
        };
        context.locations[index] = Some(TraceLocation { func, pc });
        context.dispatch(index, program, |check| {
            check.instr(event_context, func, pc, next, span, opcode);
        });
    }
    fn instr_end(
        &mut self,
        event_context: crate::trace::TraceContext<'_>,
        location: TraceLocation,
        next: Option<TraceLocation>,
        outcome: TraceOutcome<'_>,
    ) {
        let program = event_context.program();
        let mut context = self.0.borrow_mut();
        let Some(index) = context.select(event_context) else {
            return;
        };
        context.dispatch(index, program, |check| {
            check.instr_end(event_context, location, next, outcome);
        });
    }
    fn scope_enter(
        &mut self,
        event_context: crate::trace::TraceContext<'_>,
        kind: ScopeKind,
        depth: usize,
        func: FuncId,
        pc: u32,
        span: Option<u64>,
    ) {
        let program = event_context.program();
        let mut context = self.0.borrow_mut();
        let Some(index) = context.select(event_context) else {
            return;
        };
        if context
            .record(index)
            .is_some_and(|record| !record.frames.is_empty())
        {
            context.dispatch(index, program, |check| {
                check.scope_enter(event_context, kind, depth, func, pc, span);
            });
        }
    }
    fn scope_exit(
        &mut self,
        event_context: crate::trace::TraceContext<'_>,
        kind: ScopeKind,
        depth: usize,
        func: FuncId,
        pc: u32,
        span: Option<u64>,
        outcome: TraceOutcome<'_>,
    ) {
        let program = event_context.program();
        let mut context = self.0.borrow_mut();
        let Some(index) = context.select(event_context) else {
            return;
        };
        context.locations[index] = Some(TraceLocation { func, pc });
        if context
            .record(index)
            .is_some_and(|record| !record.frames.is_empty())
        {
            context.dispatch(index, program, |check| {
                check.scope_exit(event_context, kind, depth, func, pc, span, outcome);
            });
        }
    }
}

enum Next {
    Ordinary(Delivery),
    Enter {
        callee: Value,
        args: Vec<Value>,
    },
    Exit {
        target: crate::vm::ActivationId,
        values: Vec<Value>,
    },
}

fn delivery(
    context: &mut Context<'_>,
    request: &mut crate::vm::PendingHostCall<'_, '_, NoReplayEffects>,
) -> Result<Next, ReplayError> {
    if let Some(error) = &context.error {
        return Err(error.clone());
    }
    let index = context.current();
    let record = context.record(index).expect("request segment");
    let shared = Rc::clone(context.states[index].as_ref().expect("state"));
    if let Some(transfer) = context.data.transfers.get(context.next)
        && transfer.segment == context.data.segments[index].id
        && transfer.sequence == shared.borrow().sequence
        && transfer.action.is_command()
    {
        let mut state = shared.borrow_mut();
        state.boundary = ReplayBoundary::Transfer {
            index: context.next,
        };
        state.location = Some(request.location());
        if transfer.location != request.location() {
            return Err(context.qualify(
                index,
                state.mismatch(Divergence::Location {
                    expected: transfer.location,
                    actual: Some(request.location()),
                }),
            ));
        }
        let (work, constructed) = transfer.action.construction();
        restore_construction(record, &mut state, request, work, constructed)
            .map_err(|error| context.qualify(index, error))?;
        return match &transfer.action {
            TransferAction::Enter { callee, args, .. } => {
                let mut callee = state
                    .materialize(record, record.arena.slice(callee), request.aggregates())
                    .map_err(|error| context.qualify(index, error))?;
                let callee = callee
                    .pop()
                    .ok_or_else(|| context.qualify(index, state.mismatch(Divergence::Outcome)))?;
                let args = state
                    .materialize(record, record.arena.slice(args), request.aggregates())
                    .map_err(|error| context.qualify(index, error))?;
                Ok(Next::Enter { callee, args })
            }
            TransferAction::ReturnFrom { target, values, .. } => {
                let activation = request
                    .inspect()
                    .frames()
                    .find(|frame| endpoint(&frame.activation) == *target)
                    .map(|frame| frame.activation)
                    .ok_or_else(|| {
                        context.qualify(
                            index,
                            state.mismatch(Divergence::ControlTarget {
                                activation: target.activation,
                            }),
                        )
                    })?;
                let values = state
                    .materialize(record, record.arena.slice(values), request.aggregates())
                    .map_err(|error| context.qualify(index, error))?;
                Ok(Next::Exit {
                    target: activation,
                    values,
                })
            }
            _ => Err(context.qualify(index, state.mismatch(Divergence::Outcome))),
        };
    }
    RecordedHost {
        record,
        state: shared,
    }
    .invoke(request)
    .map(Next::Ordinary)
    .map_err(|error| context.qualify(index, error))
}

pub(super) fn replay(root: &Recording) -> Result<ReplayOutcome, ReplayError> {
    let data = root.execution.as_ref().expect("composed recording");
    validate(root, data)?;
    let entry = root.entry.as_ref().expect("complete root entry");
    let mut vm = Vm::new(NoReplayEffects, entry.limits.clone());
    let identities = Rc::new(RefCell::new(Identities::default()));
    let namespace_map = prepare_instances(
        &mut vm,
        (0..data.segments.len()).filter_map(|index| data.evidence(root, index)),
        &data.instances,
    )?;
    let mut states = Vec::with_capacity(data.segments.len());
    for index in 0..data.segments.len() {
        states.push(data.evidence(root, index).map(|record| {
            Rc::new(RefCell::new(State {
                instances: Some(Rc::clone(&namespace_map)),
                global: Some(Rc::clone(&identities)),
                origins: record.arena.nodes.iter().map(|node| node.origin).collect(),
                ..State::default()
            }))
        }));
    }
    let mut instances = Vec::with_capacity(data.instances.len());
    for source in &data.instances {
        instances.push(namespace_map[&(Arc::as_ptr(&source.0) as usize)].clone());
    }
    // Restore the union of originally pre-existing nodes, including values first observed by
    // a child host. A child-entry snapshot also contains parent-created nodes; those must be
    // recomputed by the guest instead of being preallocated as initial state.
    let mut initial = BTreeMap::new();
    for index in 0..data.segments.len() {
        if let Some(record) = data.evidence(root, index) {
            for (local, node) in record.arena.nodes.iter().enumerate() {
                if node.origin < root.heap_base {
                    initial.entry(node.origin).or_insert((
                        index,
                        AggregateId(
                            u32::try_from(local)
                                .map_err(|_| ReplayError::Aggregate(AggError::HandleOverflow))?,
                        ),
                    ));
                }
            }
        }
    }
    for (index, id) in initial_order(initial, |index| {
        data.evidence(root, index).expect("initial node source")
    })? {
        let record = data.evidence(root, index).expect("initial node source");
        let node = record
            .aggregate(id)
            .ok_or(ReplayError::Aggregate(AggError::BadHandle))?;
        let state = states[index].as_ref().expect("initial state");
        let values = state
            .borrow_mut()
            .materialize(record, node.values, vm.aggregates())?;
        let handle = match node.kind {
            AggType::Tuple { .. } => vm.aggregates_mut().tuple_new(values),
            AggType::Struct { type_id } => vm.aggregates_mut().struct_new(type_id, values),
            AggType::Array { elem_type_id } => vm.aggregates_mut().array_new(elem_type_id, values),
        }
        .map_err(ReplayError::Aggregate)?;
        state.borrow_mut().bind(id, handle)?;
    }
    states[0]
        .as_ref()
        .expect("root state")
        .borrow_mut()
        .initial_count = vm.aggregates().len_u32();
    identities.borrow_mut().heap_base = root.heap_base;
    identities.borrow_mut().initial_count = vm.aggregates().len_u32();
    let args = states[0]
        .as_ref()
        .expect("root state")
        .borrow_mut()
        .materialize(root, root.arena.slice(&entry.args), vm.aggregates())?;
    let root_instance = data.segments[0]
        .instance
        .and_then(|id| instances.get(id))
        .ok_or_else(|| {
            states[0]
                .as_ref()
                .expect("root state")
                .borrow()
                .mismatch(Divergence::Outcome)
        })?
        .clone();
    let count = data.segments.len();
    let context = Rc::new(RefCell::new(Context {
        root,
        data,
        states,
        instances,
        stack: Vec::new(),
        selected: 0,
        pending: None,
        next: 0,
        started: vec![false; count],
        finished: vec![false; count],
        denied: vec![false; count],
        locations: vec![None; count],
        target: None,
        failure: None,
        error: None,
    }));
    let mask = TraceMask::RUN
        | TraceMask::INSTR
        | TraceMask::VALUES
        | TraceMask::CALL
        | TraceMask::REGISTERS;
    let options = InvocationOptions::default()
        .yield_host_calls()
        .trace_owned(mask, Observer(Rc::clone(&context)));
    let result = match root_instance.start_prepaid(&mut vm, entry.func, &args, options, entry.work)
    {
        Err(InstanceError::Trapped(error)) => Err(error),
        Err(_) => return Err(context.borrow().mismatch(0, Divergence::Outcome)),
        Ok(mut invocation) => loop {
            let observer_error = context.borrow().error.clone();
            if let Some(error) = observer_error {
                return Err(error);
            }
            match invocation.resume(1024) {
                Err(error) => break Err(error),
                Ok(RunStatus::Suspended) => {}
                Ok(RunStatus::Complete) => {
                    break Ok(invocation.take_outputs().expect("actual root results"));
                }
                Ok(RunStatus::AwaitingHost) => {
                    let mut request = invocation
                        .pending_host_call()
                        .expect("active replay boundary");
                    let next = {
                        let mut state = context.borrow_mut();
                        delivery(&mut state, &mut request)
                    };
                    let next = next?;
                    match next {
                        Next::Enter { callee, args } => match request.call_guest(callee, args) {
                            Ok(()) => {}
                            Err(crate::vm::ControlError::Trapped(error)) => break Err(error),
                            Err(_) => {
                                return Err(context
                                    .borrow()
                                    .mismatch(context.borrow().current(), Divergence::Outcome));
                            }
                        },
                        Next::Exit { target, values }
                        | Next::Ordinary(Delivery::ReturnFrom { target, values }) => {
                            match request.return_from(&target, values) {
                                Ok(()) => {}
                                Err(crate::vm::ControlError::Trapped(error)) => break Err(error),
                                Err(_) => {
                                    return Err(context.borrow().mismatch(
                                        context.borrow().current(),
                                        Divergence::ControlTarget {
                                            activation: target.ordinal(),
                                        },
                                    ));
                                }
                            }
                        }
                        Next::Ordinary(Delivery::CallGuest { callee, args }) => {
                            match request.call_guest(callee, args) {
                                Ok(()) => {}
                                Err(crate::vm::ControlError::Trapped(error)) => break Err(error),
                                Err(_) => {
                                    return Err(context.borrow().mismatch(
                                        context.borrow().current(),
                                        Divergence::Outcome,
                                    ));
                                }
                            }
                        }
                        Next::Ordinary(Delivery::Host(outcome)) => {
                            let (mut values, outcome) = match outcome {
                                Ok((values, fuel)) => (values, Ok(fuel)),
                                Err(error) => (Vec::new(), Err(error)),
                            };
                            if let Err(error) = request.complete(&mut values, outcome) {
                                break Err(error);
                            }
                        }
                    }
                }
            }
        },
    };
    let context = context.borrow();
    if let Some(error) = &context.error {
        return Err(error.clone());
    }
    if context.next != data.transfers.len()
        || !context.stack.is_empty()
        || context.failure != data.failure
    {
        return Err(context.mismatch(0, Divergence::Outcome));
    }
    for (index, segment) in data.segments.iter().enumerate() {
        if segment.denied != context.denied[index]
            || (data.evidence(root, index).is_some()
                && (!context.started[index] || !context.finished[index]))
        {
            return Err(context.mismatch(0, Divergence::CallStack));
        }
    }
    match (&root.end, result) {
        (Some(End::Returned), Ok(values)) => Ok(ReplayOutcome::Returned(
            vm.aggregates()
                .snapshot(&values)
                .map_err(ReplayError::Aggregate)?,
        )),
        (Some(End::Trapped(expected)), Err(actual))
            if context.states[0]
                .as_ref()
                .expect("root state")
                .borrow()
                .same_trap(expected, &actual) =>
        {
            Ok(ReplayOutcome::Trapped(actual))
        }
        _ => Err(context.mismatch(0, Divergence::Outcome)),
    }
}

// Check ownership and range metadata before any journal-controlled indexing or guest work.
// Values/outcomes themselves remain checked against actual re-execution at their boundaries.
fn validate(root: &Recording, data: &evidence::ExecutionData) -> Result<(), ReplayError> {
    let invalid = |segment| ReplayError::InSegment {
        segment,
        error: Box::new(ReplayError::Diverged {
            boundary: ReplayBoundary::InitialState,
            location: None,
            reason: Divergence::Outcome,
        }),
    };
    if data.segments.is_empty()
        || data.children.len() != data.segments.len() - 1
        || data.indices.len() != data.segments.len()
        || data.segments[0].id != 0
        || data.segments[0].parent.is_some()
        || !data.segments[0]
            .program
            .shares_definition_with(root.program())
    {
        return Err(invalid(0));
    }
    for (index, segment) in data.segments.iter().enumerate() {
        let record = data.evidence(root, index);
        if data.index(segment.id) != Some(index)
            || segment.entry.is_none()
            || segment.denied == record.is_some()
            || segment
                .instance
                .and_then(|id| data.instances.get(id))
                .is_none_or(|instance| !instance.program().shares_definition_with(&segment.program))
            || (index != 0
                && segment
                    .parent
                    .and_then(|parent| data.index(parent.segment))
                    .is_none_or(|parent| parent >= index))
            || record.is_some_and(|record| {
                record.status().is_err()
                    || !record.program().shares_definition_with(&segment.program)
            })
        {
            return Err(invalid(segment.id));
        }
    }
    for transfer in &data.transfers {
        let record = data
            .index(transfer.segment)
            .and_then(|index| data.evidence(root, index))
            .ok_or_else(|| invalid(transfer.segment))?;
        let (values, endpoints) = match &transfer.action {
            TransferAction::Enter {
                args,
                parent,
                child,
                callee,
                ..
            } => {
                let Some([value]) = record.arena.values.get(callee.clone()) else {
                    return Err(invalid(transfer.segment));
                };
                let function = match value {
                    RecordedValue::Func(function) => function,
                    RecordedValue::Closure { func, .. } => func,
                    _ => return Err(invalid(transfer.segment)),
                };
                let child_index = data
                    .index(child.segment)
                    .ok_or_else(|| invalid(transfer.segment))?;
                let meta = &data.segments[child_index];
                if meta.entry != Some(function.id())
                    || meta.instance.and_then(|index| data.instances.get(index))
                        != Some(function.instance())
                {
                    return Err(invalid(transfer.segment));
                }
                (Some(args), [*parent, *child])
            }
            TransferAction::Rejected { parent, child }
            | TransferAction::Returned { parent, child, .. } => {
                let values = if let TransferAction::Returned { values, .. } = &transfer.action {
                    Some(values)
                } else {
                    None
                };
                (values, [*parent, *child])
            }
            TransferAction::ReturnFrom {
                values,
                source,
                target,
                ..
            } => (Some(values), [*source, *target]),
        };
        if endpoints
            .iter()
            .any(|endpoint| data.index(endpoint.segment).is_none())
            || values.is_some_and(|range| {
                range.start > range.end || range.end > record.arena.values.len()
            })
            || transfer
                .action
                .construction()
                .1
                .iter()
                .any(|id| id.0 as usize >= record.arena.nodes.len())
            || transfer
                .frame
                .is_some_and(|frame| frame.index() >= record.frames.len())
        {
            return Err(invalid(transfer.segment));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
