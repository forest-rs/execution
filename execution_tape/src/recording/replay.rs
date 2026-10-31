// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use crate::aggregates::{AggError, AggHeap, ValueSnapshot};
use crate::host::{Host, HostContext, HostError, SigHash, ValueRef};
use crate::program::{HostSigId, Program};
use crate::recording::control::Action;
use crate::recording::{AggregateId, CallOutcome, CaptureFault, End, RecordedValue, Recording};
use crate::trace::{ScopeKind, TraceLocation, TraceMask, TraceOutcome, TraceSink, ValueEvent};
use crate::value::{AggHandle, AggType, Closure, FuncId, Value};
use crate::vm::{Trap, TrapInfo, Vm};
use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;

/// Difference between the replay and the captured boundary contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Divergence {
    /// Host invocation occurred at another guest instruction.
    Location {
        /// Recorded instruction.
        expected: TraceLocation,
        /// Observed replay instruction, if any.
        actual: Option<TraceLocation>,
    },
    /// A different program-local import was invoked.
    Import {
        /// Recorded import.
        expected: HostSigId,
        /// Replay import.
        actual: HostSigId,
    },
    /// A root value, or an immediate field of the identified aggregate, differs.
    Value {
        /// Containing recorded aggregate; `None` identifies a boundary root.
        aggregate: Option<AggregateId>,
        /// Argument/result/field index.
        index: usize,
    },
    /// Shared or distinct aggregate identities changed.
    Identity {
        /// Recorded aggregate whose binding differs.
        aggregate: AggregateId,
    },
    /// A captured aggregate cannot be resolved in the replay heap.
    UnresolvedAggregate {
        /// Recorded aggregate.
        aggregate: AggregateId,
    },
    /// The number of host calls differs.
    HostCalls {
        /// Recorded count.
        expected: usize,
        /// Observed count, including a refused extra call.
        actual: usize,
    },
    /// The number of embedding commands differs.
    ControlCommands {
        /// Recorded count.
        expected: usize,
        /// Observed command admissions.
        actual: usize,
    },
    /// A host outcome, command or guest return occurred out of observation order.
    ObservationOrder {
        /// Recorded observation ordinal.
        expected: usize,
        /// Next ordinal in this replay.
        actual: usize,
    },
    /// A recorded transfer target is no longer an active activation in this replay.
    ControlTarget {
        /// Invocation-local ordinal requested by the recording.
        activation: u64,
    },
    /// The number of argument/result values differs.
    Arity {
        /// Recorded argument/result/field count.
        expected: usize,
        /// Observed replay count.
        actual: usize,
    },
    /// Final return/trap or return-candidate presence differs.
    Outcome,
    /// Guest activation ancestry, call site or observed stack differs.
    CallStack,
    /// A recorded instruction, successor, completion or activation differs.
    Instruction,
}

/// Boundary being checked when replay diverged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReplayBoundary {
    /// Checking one retained instruction attempt or completion.
    Instruction {
        /// Zero-based attempt ordinal.
        index: usize,
    },
    /// Restoring recorded initial values before guest execution.
    #[default]
    InitialState,
    /// Checking one host invocation in execution order.
    HostCall {
        /// Zero-based ordinal, also indexing [`Recording::host_calls`].
        index: usize,
    },
    /// Checking an embedding command's admission.
    ControlCommand {
        /// Zero-based command ordinal.
        index: usize,
    },
    /// Checking a command's actual guest return.
    ControlReturn {
        /// Zero-based command ordinal.
        index: usize,
    },
    /// Checking the top-level return candidate before heap commit.
    ReturnCandidate,
    /// Comparing final call counts and run outcomes.
    Completion,
}

/// Replay could not establish the captured invocation's result.
#[derive(Clone, Debug)]
pub enum ReplayError {
    /// A partial capture cannot be replayed as complete.
    Incomplete(CaptureFault),
    /// Guest execution diverged from recorded boundary values or outcomes.
    Diverged {
        /// Boundary and host-call ordinal being checked.
        boundary: ReplayBoundary,
        /// Best available replay instruction location.
        location: Option<TraceLocation>,
        /// Structured mismatch.
        reason: Divergence,
    },
    /// Rebuilding or retaining the replay heap failed.
    Aggregate(AggError),
}

impl core::fmt::Display for ReplayError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "replay failed: {self:?}")
    }
}
impl core::error::Error for ReplayError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Incomplete(error) => Some(error),
            Self::Aggregate(error) => Some(error),
            Self::Diverged { .. } => None,
        }
    }
}

/// Actual result produced by a fresh VM after it matched the recording.
#[derive(Debug)]
pub enum ReplayOutcome {
    /// Independently retained values computed by guest execution during replay.
    Returned(ValueSnapshot),
    /// Actual replay trap, including the original typed host error when applicable.
    Trapped(TrapInfo),
}

#[derive(Default)]
struct State {
    instructions: usize,
    pending: Vec<usize>,
    commands: usize,
    sequence: usize,
    active_commands: Vec<usize>,
    boundary: ReplayBoundary,
    bindings: BTreeMap<AggregateId, AggHandle>,
    inverse: BTreeMap<u32, AggregateId>,
    initial_count: u32,
    calls: usize,
    location: Option<TraceLocation>,
    candidate: bool,
    frames: usize,
    top: Option<crate::recording::frames::FrameId>,
    final_top: Option<crate::recording::frames::FrameId>,
    error: Option<ReplayError>,
}

impl State {
    fn mismatch(&self, reason: Divergence) -> ReplayError {
        ReplayError::Diverged {
            boundary: self.boundary,
            location: self.location,
            reason,
        }
    }

    fn bind(&mut self, id: AggregateId, handle: AggHandle) -> Result<bool, ReplayError> {
        if let Some(&existing) = self.bindings.get(&id) {
            return if existing == handle {
                Ok(false)
            } else {
                Err(self.mismatch(Divergence::Identity { aggregate: id }))
            };
        }
        if self.inverse.contains_key(&handle.0) {
            return Err(self.mismatch(Divergence::Identity { aggregate: id }));
        }
        self.bindings.insert(id, handle);
        self.inverse.insert(handle.0, id);
        Ok(true)
    }

    // Scalar roots and previously checked immutable identities need no work allocation.
    fn match_value(
        &mut self,
        expected: &RecordedValue,
        actual: ValueRef<'_>,
        aggregate: Option<AggregateId>,
        index: usize,
        work: &mut Vec<(AggregateId, AggHandle)>,
    ) -> Result<(), ReplayError> {
        let pair = match (expected, actual) {
            (RecordedValue::Unit, ValueRef::Unit) => None,
            (RecordedValue::Bool(a), ValueRef::Bool(b)) if *a == b => None,
            (RecordedValue::I64(a), ValueRef::I64(b)) if *a == b => None,
            (RecordedValue::U64(a), ValueRef::U64(b)) if *a == b => None,
            (RecordedValue::F64(a), ValueRef::F64(b)) if *a == b.to_bits() => None,
            (RecordedValue::Decimal(a), ValueRef::Decimal(b)) if a == b => None,
            (RecordedValue::Bytes(a), ValueRef::Bytes(b)) if a.as_ref() == b => None,
            (RecordedValue::Str(a), ValueRef::Str(b)) if a.as_ref() == b => None,
            (RecordedValue::Obj(a), ValueRef::Obj(b)) if *a == b => None,
            (RecordedValue::Func(a), ValueRef::Func(b)) if *a == b => None,
            (RecordedValue::Agg(id), ValueRef::Agg(handle)) => Some((*id, handle)),
            (RecordedValue::Closure { func, env }, ValueRef::Closure(value))
                if *func == value.func =>
            {
                Some((*env, value.env))
            }
            _ => return Err(self.mismatch(Divergence::Value { aggregate, index })),
        };
        let Some((id, handle)) = pair else {
            return Ok(());
        };
        if self.bind(id, handle)? {
            work.push((id, handle));
        }
        Ok(())
    }

    fn matches<'a>(
        &mut self,
        record: &Recording,
        expected: &[RecordedValue],
        actual: impl IntoIterator<Item = ValueRef<'a>>,
        heap: &'a AggHeap,
    ) -> Result<(), ReplayError> {
        let actual = actual.into_iter();
        let mut work = Vec::new();
        let mut count = 0;
        for (index, value) in actual.enumerate() {
            if let Some(expected) = expected.get(index) {
                self.match_value(expected, value, None, index, &mut work)?;
            }
            count += 1;
        }
        if count != expected.len() {
            return Err(self.mismatch(Divergence::Arity {
                expected: expected.len(),
                actual: count,
            }));
        }
        while let Some((id, handle)) = work.pop() {
            let node = record
                .aggregate(id)
                .ok_or_else(|| self.mismatch(Divergence::UnresolvedAggregate { aggregate: id }))?;
            let actual_kind = heap.agg_type(handle).map_err(ReplayError::Aggregate)?;
            if node.kind != actual_kind {
                return Err(self.mismatch(Divergence::Value {
                    aggregate: Some(id),
                    index: 0,
                }));
            }
            let count = match actual_kind {
                AggType::Tuple { .. } => heap.tuple_len(handle),
                AggType::Struct { .. } => heap.struct_field_count(handle),
                AggType::Array { .. } => heap.array_len(handle),
            }
            .map_err(ReplayError::Aggregate)?;
            if count != node.values.len() {
                return Err(self.mismatch(Divergence::Arity {
                    expected: node.values.len(),
                    actual: count,
                }));
            }
            for (index, expected) in node.values.iter().enumerate() {
                let value = match actual_kind {
                    AggType::Tuple { .. } => heap.tuple_get_ref(handle, index),
                    AggType::Struct { .. } => heap.struct_get_ref(handle, index),
                    AggType::Array { .. } => heap.array_get_ref(handle, index),
                }
                .map_err(ReplayError::Aggregate)?;
                self.match_value(
                    expected,
                    ValueRef::from_value(value),
                    Some(id),
                    index,
                    &mut work,
                )?;
            }
        }
        Ok(())
    }

    fn materialize(
        &mut self,
        record: &Recording,
        values: &[RecordedValue],
        heap: &AggHeap,
    ) -> Result<Vec<Value>, ReplayError> {
        let mut output = Vec::with_capacity(values.len());
        for value in values {
            output.push(self.materialize_value(record, value, heap)?);
        }
        Ok(output)
    }

    fn materialize_value(
        &mut self,
        record: &Recording,
        value: &RecordedValue,
        heap: &AggHeap,
    ) -> Result<Value, ReplayError> {
        let reference = match value {
            RecordedValue::Agg(id) => Some(*id),
            RecordedValue::Closure { env, .. } => Some(*env),
            _ => None,
        };
        if let Some(id) = reference
            && !self.bindings.contains_key(&id)
        {
            // The VM appends guest-created nodes until root commit. Replaying the same
            // guest allocations preserves their ordinal relative to the initial heap,
            // even when only observed initial nodes were restored. Check contents/aliasing
            // before using this mapping; no host mutation or guessed persistent ID needed.
            let node = &record.arena.nodes[id.0 as usize];
            let handle = node
                .created_offset
                .and_then(|offset| self.initial_count.checked_add(offset))
                .map(AggHandle)
                .ok_or_else(|| self.mismatch(Divergence::UnresolvedAggregate { aggregate: id }))?;
            self.matches(
                record,
                core::slice::from_ref(value),
                [match value {
                    RecordedValue::Agg(_) => ValueRef::Agg(handle),
                    RecordedValue::Closure { func, .. } => ValueRef::Closure(Closure {
                        func: *func,
                        env: handle,
                    }),
                    _ => unreachable!(),
                }],
                heap,
            )?;
        }
        Ok(match value {
            RecordedValue::Unit => Value::Unit,
            RecordedValue::Bool(value) => Value::Bool(*value),
            RecordedValue::I64(value) => Value::I64(*value),
            RecordedValue::U64(value) => Value::U64(*value),
            RecordedValue::F64(value) => Value::F64(f64::from_bits(*value)),
            RecordedValue::Decimal(value) => Value::Decimal(*value),
            RecordedValue::Bytes(value) => Value::Bytes(value.to_vec()),
            RecordedValue::Str(value) => Value::Str(value.as_ref().into()),
            RecordedValue::Obj(value) => Value::Obj(*value),
            RecordedValue::Func(value) => Value::Func(*value),
            RecordedValue::Agg(id) => Value::Agg(self.bindings[id]),
            RecordedValue::Closure { func, env } => Value::Closure(Closure {
                func: *func,
                env: self.bindings[env],
            }),
        })
    }
}

enum Delivery {
    ReturnFrom {
        target: crate::vm::ActivationId,
        values: Vec<Value>,
    },
    CallGuest {
        callee: Value,
        args: Vec<Value>,
    },
    Host(Result<(Vec<Value>, u64), HostError>),
}
fn restore_construction(
    record: &Recording,
    state: &mut State,
    request: &mut crate::vm::PendingHostCall<'_, '_, NoReplayEffects>,
    work: crate::vm::DataWork,
    constructed: &[AggregateId],
) -> Result<(), ReplayError> {
    // A recorded debit may include discarded staging, but cannot be smaller than the
    // nodes/fields and immediate payloads it asks replay to reconstruct.
    let mut minimum = crate::vm::DataWork::default();
    for &id in constructed {
        let node = record
            .aggregate(id)
            .ok_or_else(|| state.mismatch(Divergence::UnresolvedAggregate { aggregate: id }))?;
        minimum.aggregate = minimum
            .aggregate
            .checked_add(
                u64::try_from(node.values.len())
                    .ok()
                    .and_then(|fields| fields.checked_add(1))
                    .ok_or_else(|| state.mismatch(Divergence::Outcome))?,
            )
            .ok_or_else(|| state.mismatch(Divergence::Outcome))?;
        for value in node.values {
            let bytes = match value {
                RecordedValue::Bytes(bytes) => bytes.len(),
                RecordedValue::Str(text) => text.len(),
                _ => 0,
            };
            minimum.payload = minimum
                .payload
                .checked_add(u64::try_from(bytes).map_err(|_| state.mismatch(Divergence::Outcome))?)
                .ok_or_else(|| state.mismatch(Divergence::Outcome))?;
        }
    }
    if work.aggregate < minimum.aggregate || work.payload < minimum.payload {
        return Err(state.mismatch(Divergence::Outcome));
    }
    request
        .restore_host_work(work)
        .map_err(|_| state.mismatch(Divergence::Outcome))?;
    for &id in constructed {
        let node = record
            .aggregate(id)
            .ok_or_else(|| state.mismatch(Divergence::UnresolvedAggregate { aggregate: id }))?;
        let values = state.materialize(record, node.values, request.aggregates())?;
        let handle = request
            .restore_host_node(node.kind, values)
            .map_err(ReplayError::Aggregate)?;
        state.bind(id, handle)?;
    }
    Ok(())
}
fn check_created(
    record: &Recording,
    state: &mut State,
    constructed: &[AggregateId],
    created: &core::ops::Range<u32>,
) -> Result<(), ReplayError> {
    if constructed.len() != (created.end - created.start) as usize {
        return Err(state.mismatch(Divergence::Outcome));
    }
    for (&id, handle) in constructed.iter().zip(created.clone()) {
        let offset = record
            .arena
            .nodes
            .get(id.0 as usize)
            .and_then(|node| node.created_offset);
        if offset != handle.checked_sub(state.initial_count) {
            return Err(state.mismatch(Divergence::Identity { aggregate: id }));
        }
        state.bind(id, AggHandle(handle))?;
    }
    Ok(())
}
fn check_sequence(state: &State, expected: usize) -> Result<(), ReplayError> {
    if state.sequence != expected {
        return Err(state.mismatch(Divergence::ObservationOrder {
            expected,
            actual: state.sequence,
        }));
    }
    Ok(())
}

struct RecordedHost<'a> {
    record: &'a Recording,
    state: Rc<RefCell<State>>,
}
impl RecordedHost<'_> {
    fn invoke(
        &mut self,
        request: &mut crate::vm::PendingHostCall<'_, '_, NoReplayEffects>,
    ) -> Result<Delivery, ReplayError> {
        let mut state = self.state.borrow_mut();
        if let Some(error) = &state.error {
            return Err(error.clone());
        }
        state.location = Some(request.location());
        if let Some(command) = self.record.commands.get(state.commands)
            && command.sequence <= state.sequence
        {
            state.boundary = ReplayBoundary::ControlCommand {
                index: state.commands,
            };
            check_sequence(&state, command.sequence)?;
            if command.location != request.location() {
                return Err(state.mismatch(Divergence::Location {
                    expected: command.location,
                    actual: Some(request.location()),
                }));
            }
            if !self.record.frames.is_empty() && command.frame != state.top {
                return Err(state.mismatch(Divergence::CallStack));
            }
            let (work, constructed) = command.action.construction();
            restore_construction(self.record, &mut state, request, work, constructed)?;
            return match &command.action {
                Action::CallGuest { callee, args, .. } => {
                    let callee = state.materialize_value(
                        self.record,
                        &self.record.arena.values[*callee],
                        request.aggregates(),
                    )?;
                    let args = state.materialize(
                        self.record,
                        self.record.arena.slice(args),
                        request.aggregates(),
                    )?;
                    Ok(Delivery::CallGuest { callee, args })
                }
                Action::ReturnFrom { target, values, .. } => {
                    let target = request
                        .inspect()
                        .frames()
                        .find(|frame| frame.activation.ordinal() == *target)
                        .map(|frame| frame.activation)
                        .ok_or_else(|| {
                            state.mismatch(Divergence::ControlTarget {
                                activation: *target,
                            })
                        })?;
                    let values = state.materialize(
                        self.record,
                        self.record.arena.slice(values),
                        request.aggregates(),
                    )?;
                    Ok(Delivery::ReturnFrom { target, values })
                }
            };
        }
        let index = state.calls;
        state.boundary = ReplayBoundary::HostCall { index };
        let call = self.record.calls.get(index).ok_or_else(|| {
            state.mismatch(Divergence::HostCalls {
                expected: self.record.calls.len(),
                actual: index + 1,
            })
        })?;
        check_sequence(&state, call.sequence)?;
        if request.location() != call.location {
            return Err(state.mismatch(Divergence::Location {
                expected: call.location,
                actual: state.location,
            }));
        }
        if !self.record.frames.is_empty() && call.frame != state.top {
            return Err(state.mismatch(Divergence::CallStack));
        }
        let import = request.import();
        if import != call.import {
            return Err(state.mismatch(Divergence::Import {
                expected: call.import,
                actual: import,
            }));
        }
        state.matches(
            self.record,
            self.record.arena.slice(&call.args),
            request.arguments(),
            request.aggregates(),
        )?;
        restore_construction(
            self.record,
            &mut state,
            request,
            call.work,
            &call.constructed,
        )?;
        match &call.outcome {
            CallOutcome::Failed(error) => Ok(Delivery::Host(Err(error.clone()))),
            CallOutcome::Returned { values, fuel } => {
                let values = state.materialize(
                    self.record,
                    self.record.arena.slice(values),
                    request.aggregates(),
                )?;
                Ok(Delivery::Host(Ok((values, *fuel))))
            }
        }
    }
}

struct NoReplayEffects;
impl Host for NoReplayEffects {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        _: &[ValueRef<'_>],
        _: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        unreachable!("checked replay delivers recorded outcomes through yielded host requests")
    }
}

struct Check<'a> {
    record: &'a Recording,
    state: Rc<RefCell<State>>,
}
impl TraceSink for Check<'_> {
    fn wants_registers(&self, location: TraceLocation, phase: crate::trace::RegisterPhase) -> bool {
        let state = self.state.borrow();
        let Some(step) = state
            .pending
            .last()
            .copied()
            .and_then(|index| self.record.instruction(index))
        else {
            return false;
        };
        step.location == location
            && match phase {
                crate::trace::RegisterPhase::Inputs => !step.inputs.is_empty(),
                crate::trace::RegisterPhase::Outputs => !step.outputs.is_empty(),
            }
    }

    fn registers(&mut self, _: &Program, event: crate::trace::RegisterEvent<'_>) {
        let mut state = self.state.borrow_mut();
        if state.error.is_some() {
            return;
        }
        let Some(index) = state.pending.last().copied() else {
            return;
        };
        let Some(step) = self.record.instruction(index) else {
            return;
        };
        let expected = match event.phase {
            crate::trace::RegisterPhase::Inputs => step.inputs,
            crate::trace::RegisterPhase::Outputs => step.outputs,
        };
        state.boundary = ReplayBoundary::Instruction { index };
        let mut actual = event
            .values
            .iter()
            .filter(|(reg, _)| expected.iter().any(|(wanted, _)| wanted == *reg));
        for (reg, value) in expected.iter() {
            let Some((actual_reg, Some(actual_value))) = actual.next() else {
                state.error = Some(state.mismatch(Divergence::Instruction));
                return;
            };
            if reg != actual_reg {
                state.error = Some(state.mismatch(Divergence::Instruction));
                return;
            }
            if let Err(error) = state.matches(
                self.record,
                core::slice::from_ref(value),
                core::iter::once(actual_value),
                event.aggregates,
            ) {
                state.error = Some(error);
                return;
            }
        }
        if actual.next().is_some() {
            state.error = Some(state.mismatch(Divergence::Instruction));
        }
    }
    fn scope_enter(
        &mut self,
        _: &Program,
        kind: ScopeKind,
        depth: usize,
        func: FuncId,
        pc: u32,
        _: Option<u64>,
    ) {
        let ScopeKind::CallFrame {
            func: callee,
            caller,
        } = kind
        else {
            return;
        };
        let mut state = self.state.borrow_mut();
        if state.error.is_some() {
            return;
        }
        let expected = self.record.frames.get(state.frames);
        if !expected.is_some_and(|frame| {
            (frame.func, frame.caller, frame.parent, frame.depth as usize)
                == (callee, caller, state.top, depth)
        }) {
            state.boundary = ReplayBoundary::Completion;
            state.location = caller.or(Some(TraceLocation { func, pc }));
            state.error = Some(state.mismatch(Divergence::CallStack));
            return;
        }
        state.top = crate::recording::frames::FrameId::new(state.frames);
        state.frames += 1;
    }

    fn scope_exit(
        &mut self,
        _: &Program,
        kind: ScopeKind,
        _: usize,
        _: FuncId,
        _: u32,
        _: Option<u64>,
        outcome: TraceOutcome<'_>,
    ) {
        if !matches!(kind, ScopeKind::CallFrame { .. }) {
            return;
        }
        let mut state = self.state.borrow_mut();
        if state.error.is_some() {
            return;
        }
        if matches!(outcome, TraceOutcome::Trap(_)) && state.final_top.is_none() {
            state.final_top = state.top;
        }
        state.top = state
            .top
            .and_then(|top| self.record.frames[top.index()].parent);
    }

    fn instr(&mut self, _: &Program, func: FuncId, pc: u32, _: u32, _: Option<u64>, opcode: u8) {
        let mut state = self.state.borrow_mut();
        state.location = Some(TraceLocation { func, pc });
        let index = state.instructions;
        state.instructions = state.instructions.saturating_add(1);
        if state.error.is_some() {
            return;
        }
        if let Some(history) = &self.record.history {
            state.boundary = ReplayBoundary::Instruction { index };
            if let Some(step) = history.steps.get(index) {
                if step.location != (TraceLocation { func, pc })
                    || step.opcode != opcode
                    || step.frame.is_some_and(|frame| Some(frame) != state.top)
                    || step.parent
                        != state
                            .pending
                            .last()
                            .copied()
                            .filter(|&parent| parent < history.steps.len())
                {
                    state.error = Some(state.mismatch(Divergence::Instruction));
                }
            } else if history.status == crate::recording::InstructionStatus::Complete {
                state.error = Some(state.mismatch(Divergence::Instruction));
            }
        }
        if self.record.history.is_some() {
            state.pending.push(index);
        }
    }

    fn instr_end(
        &mut self,
        _: &Program,
        location: TraceLocation,
        next: Option<TraceLocation>,
        outcome: TraceOutcome<'_>,
    ) {
        let mut state = self.state.borrow_mut();
        if state.error.is_some() {
            return;
        }
        let Some(index) = state.pending.pop() else {
            return;
        };
        let Some(step) = self
            .record
            .history
            .as_ref()
            .and_then(|h| h.steps.get(index))
        else {
            return;
        };
        let actual = match outcome {
            TraceOutcome::Ok => crate::recording::InstructionOutcome::Completed,
            TraceOutcome::Trap(_) => crate::recording::InstructionOutcome::Trapped,
            TraceOutcome::Unwound { target } => {
                crate::recording::InstructionOutcome::Unwound { target }
            }
            TraceOutcome::Cancelled => {
                state.boundary = ReplayBoundary::Instruction { index };
                state.error = Some(state.mismatch(Divergence::Instruction));
                return;
            }
        };
        if step.outcome != crate::recording::InstructionOutcome::Pending
            && (step.location != location || step.next != next || step.outcome != actual)
        {
            state.boundary = ReplayBoundary::Instruction { index };
            state.error = Some(state.mismatch(Divergence::Instruction));
        }
    }
    fn values(&mut self, _: &Program, event: ValueEvent<'_>) {
        let mut state = self.state.borrow_mut();
        if state.error.is_some() {
            return;
        }
        let result = (|| -> Result<(), ReplayError> {
            match event {
                ValueEvent::ReturnFrom {
                    location,
                    target,
                    target_location,
                    unwound_callbacks,
                    values,
                    work,
                    created,
                    aggregates,
                } => {
                    let index = state.commands;
                    state.boundary = ReplayBoundary::ControlCommand { index };
                    state.location = Some(location);
                    let command = self.record.commands.get(index).ok_or_else(|| {
                        state.mismatch(Divergence::ControlCommands {
                            expected: self.record.commands.len(),
                            actual: index + 1,
                        })
                    })?;
                    check_sequence(&state, command.sequence)?;
                    if command.location != location {
                        return Err(state.mismatch(Divergence::Location {
                            expected: command.location,
                            actual: Some(location),
                        }));
                    }
                    if !self.record.frames.is_empty() && command.frame != state.top {
                        return Err(state.mismatch(Divergence::CallStack));
                    }
                    let Action::ReturnFrom {
                        target: expected_target,
                        target_location: expected_location,
                        unwound_callbacks: expected_callbacks,
                        values: expected_values,
                        work: expected_work,
                        constructed,
                    } = &command.action
                    else {
                        return Err(state.mismatch(Divergence::Outcome));
                    };
                    if target_location != *expected_location {
                        return Err(state.mismatch(Divergence::Location {
                            expected: *expected_location,
                            actual: Some(target_location),
                        }));
                    }
                    if target != *expected_target
                        || unwound_callbacks != *expected_callbacks
                        || work != *expected_work
                    {
                        return Err(state.mismatch(Divergence::Outcome));
                    }
                    check_created(self.record, &mut state, constructed, created)?;
                    state.matches(
                        self.record,
                        self.record.arena.slice(expected_values),
                        values.iter().map(ValueRef::from_value),
                        aggregates,
                    )?;
                    let active = state
                        .active_commands
                        .len()
                        .checked_sub(unwound_callbacks)
                        .ok_or_else(|| state.mismatch(Divergence::Outcome))?;
                    state.active_commands.truncate(active);
                    state.commands += 1;
                    state.sequence += 1;
                }
                ValueEvent::Entry { .. } => {}
                ValueEvent::HostCall {
                    location,
                    host_sig,
                    args,
                    outcome: _,
                    work,
                    created,
                    aggregates,
                } => {
                    let index = state.calls;
                    state.boundary = ReplayBoundary::HostCall { index };
                    state.location = Some(location);
                    let call = self.record.calls.get(index).ok_or_else(|| {
                        state.mismatch(Divergence::HostCalls {
                            expected: self.record.calls.len(),
                            actual: index + 1,
                        })
                    })?;
                    check_sequence(&state, call.sequence)?;
                    if call.location != location {
                        return Err(state.mismatch(Divergence::Location {
                            expected: call.location,
                            actual: Some(location),
                        }));
                    }
                    if call.import != host_sig {
                        return Err(state.mismatch(Divergence::Import {
                            expected: call.import,
                            actual: host_sig,
                        }));
                    }
                    if call.work != work {
                        return Err(state.mismatch(Divergence::Outcome));
                    }
                    check_created(self.record, &mut state, &call.constructed, created)?;
                    state.matches(
                        self.record,
                        self.record.arena.slice(&call.args),
                        args.iter().copied(),
                        aggregates,
                    )?;
                    state.calls += 1;
                    state.sequence += 1;
                }
                ValueEvent::CallbackEnter {
                    location,
                    callee,
                    args,
                    work,
                    created,
                    aggregates,
                } => {
                    let index = state.commands;
                    state.boundary = ReplayBoundary::ControlCommand { index };
                    state.location = Some(location);
                    let command = self.record.commands.get(index).ok_or_else(|| {
                        state.mismatch(Divergence::ControlCommands {
                            expected: self.record.commands.len(),
                            actual: index + 1,
                        })
                    })?;
                    check_sequence(&state, command.sequence)?;
                    if command.location != location {
                        return Err(state.mismatch(Divergence::Location {
                            expected: command.location,
                            actual: Some(location),
                        }));
                    }
                    if !self.record.frames.is_empty() && command.frame != state.top {
                        return Err(state.mismatch(Divergence::CallStack));
                    }
                    let Action::CallGuest {
                        callee: expected,
                        args: expected_args,
                        work: expected_work,
                        constructed,
                        ..
                    } = &command.action
                    else {
                        return Err(state.mismatch(Divergence::Outcome));
                    };
                    if *expected_work != work {
                        return Err(state.mismatch(Divergence::Outcome));
                    }
                    check_created(self.record, &mut state, constructed, created)?;
                    state.matches(
                        self.record,
                        core::slice::from_ref(&self.record.arena.values[*expected]),
                        core::iter::once(ValueRef::from_value(callee)),
                        aggregates,
                    )?;
                    state.matches(
                        self.record,
                        self.record.arena.slice(expected_args),
                        args.iter().map(ValueRef::from_value),
                        aggregates,
                    )?;
                    state.active_commands.push(index);
                    state.commands += 1;
                    state.sequence += 1;
                }
                ValueEvent::CallbackReturn {
                    location,
                    caller,
                    values,
                    aggregates,
                } => {
                    let index = state
                        .active_commands
                        .pop()
                        .ok_or_else(|| state.mismatch(Divergence::Outcome))?;
                    state.boundary = ReplayBoundary::ControlReturn { index };
                    state.location = Some(location);
                    let command = &self.record.commands[index];
                    let Action::CallGuest { returned, .. } = &command.action else {
                        return Err(state.mismatch(Divergence::Outcome));
                    };
                    let returned = returned
                        .as_ref()
                        .ok_or_else(|| state.mismatch(Divergence::Outcome))?;
                    check_sequence(&state, returned.sequence)?;
                    if caller != command.location || location != returned.location {
                        return Err(state.mismatch(Divergence::Location {
                            expected: returned.location,
                            actual: Some(location),
                        }));
                    }
                    if !self.record.frames.is_empty() && returned.frame != state.top {
                        return Err(state.mismatch(Divergence::CallStack));
                    }
                    state.matches(
                        self.record,
                        self.record.arena.slice(&returned.values),
                        values.iter().map(ValueRef::from_value),
                        aggregates,
                    )?;
                    state.sequence += 1;
                }
                ValueEvent::ReturnCandidate {
                    location,
                    values,
                    aggregates,
                } => {
                    state.candidate = true;
                    state.final_top = state.top;
                    state.boundary = ReplayBoundary::ReturnCandidate;
                    state.location = Some(location);
                    match &self.record.candidate {
                        Some((expected, roots)) if *expected == location => state.matches(
                            self.record,
                            self.record.arena.slice(roots),
                            values.iter().map(ValueRef::from_value),
                            aggregates,
                        )?,
                        _ => return Err(state.mismatch(Divergence::Outcome)),
                    }
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            state.error = Some(error);
        }
    }
}

impl Recording {
    /// Reexecutes guest code in a fresh VM and verifies its recorded boundary contract.
    ///
    /// Only discovered initial graph nodes are restored. Guest-created identities are matched
    /// across events and allocation ordinals; host results and fuel replace live host execution.
    /// The returned snapshot/trap is produced by this execution. External state and resource
    /// access side effects are not replayed. No portable archive or cross-platform float guarantee
    /// is implied; any observed value or outcome mismatch is reported.
    pub fn replay(&self) -> Result<ReplayOutcome, ReplayError> {
        self.status()
            .map_err(|fault| ReplayError::Incomplete(fault.clone()))?;
        let entry = self.entry.as_ref().expect("complete recording entry");
        let state = Rc::new(RefCell::new(State::default()));
        let mut recorded_host = RecordedHost {
            record: self,
            state: Rc::clone(&state),
        };
        let mut vm = Vm::new(NoReplayEffects, entry.limits.clone());
        for &id in &self.initial {
            let node = self.aggregate(id).expect("complete initial node");
            let values = state
                .borrow_mut()
                .materialize(self, node.values, vm.aggregates())?;
            let handle = match node.kind {
                AggType::Tuple { .. } => vm.aggregates_mut().tuple_new(values),
                AggType::Struct { type_id } => vm.aggregates_mut().struct_new(type_id, values),
                AggType::Array { elem_type_id } => {
                    vm.aggregates_mut().array_new(elem_type_id, values)
                }
            }
            .map_err(ReplayError::Aggregate)?;
            state.borrow_mut().bind(id, handle)?;
        }
        state.borrow_mut().initial_count = vm.aggregates().len_u32();
        let args =
            state
                .borrow_mut()
                .materialize(self, self.arena.slice(&entry.args), vm.aggregates())?;
        let mut check = Check {
            record: self,
            state: Rc::clone(&state),
        };
        let options = crate::vm::InvocationOptions::default()
            .yield_host_calls()
            .trace(
                TraceMask::INSTR
                    | TraceMask::VALUES
                    | if self
                        .history
                        .as_ref()
                        .is_some_and(|h| !h.config.watches.is_empty())
                    {
                        TraceMask::REGISTERS
                    } else {
                        TraceMask::NONE
                    }
                    | if self.frames.is_empty() {
                        TraceMask::NONE
                    } else {
                        TraceMask::CALL
                    },
                &mut check,
            );
        let result = match vm.start_prepaid(&self.program, entry.func, &args, options, entry.work) {
            Err(error) => Err(error),
            Ok(mut invocation) => loop {
                match invocation.resume(u64::MAX) {
                    Err(error) => break Err(error),
                    Ok(crate::vm::RunStatus::Suspended) => {}
                    Ok(crate::vm::RunStatus::Complete) => {
                        break Ok(invocation.take_outputs().expect("completed replay outputs"));
                    }
                    Ok(crate::vm::RunStatus::AwaitingHost) => {
                        let mut request = invocation
                            .pending_host_call()
                            .expect("yielded replay request");
                        let outcome = recorded_host.invoke(&mut request)?;
                        let outcome = match outcome {
                            Delivery::CallGuest { callee, args } => {
                                if let Err(error) = request.call_guest(callee, args) {
                                    break Err(error);
                                }
                                continue;
                            }
                            Delivery::ReturnFrom { target, values } => {
                                match request.return_from(&target, values) {
                                    Ok(()) => continue,
                                    Err(crate::vm::ControlError::Trapped(error)) => {
                                        break Err(error);
                                    }
                                    Err(_) => {
                                        return Err(state.borrow().mismatch(
                                            Divergence::ControlTarget {
                                                activation: target.ordinal(),
                                            },
                                        ));
                                    }
                                }
                            }
                            Delivery::Host(outcome) => outcome,
                        };
                        let (mut values, outcome) = match outcome {
                            Ok((values, fuel)) => (values, Ok(fuel)),
                            Err(error) => (Vec::new(), Err(error)),
                        };
                        if let Err(error) = request.complete(&mut values, outcome) {
                            break Err(error);
                        }
                    }
                }
            },
        };
        let mut state = state.borrow_mut();
        if let Some(error) = &state.error {
            return Err(error.clone());
        }
        state.boundary = ReplayBoundary::Completion;
        if let Some(history) = &self.history
            && (state.instructions < history.steps.len()
                || (history.status == crate::recording::InstructionStatus::Complete
                    && state.instructions != history.steps.len()))
        {
            return Err(state.mismatch(Divergence::Instruction));
        }
        if state.calls != self.calls.len() {
            return Err(state.mismatch(Divergence::HostCalls {
                expected: self.calls.len(),
                actual: state.calls,
            }));
        }
        if state.commands != self.commands.len() {
            return Err(state.mismatch(Divergence::ControlCommands {
                expected: self.commands.len(),
                actual: state.commands,
            }));
        }
        if !state.pending.is_empty()
            || state.active_commands.iter().any(|&index| {
                matches!(
                    &self.commands[index].action,
                    Action::CallGuest {
                        returned: Some(_),
                        ..
                    }
                )
            })
            || (matches!(self.end, Some(End::Returned)) && !state.active_commands.is_empty())
        {
            return Err(state.mismatch(Divergence::Outcome));
        }
        if state.frames != self.frames.len()
            || (!self.frames.is_empty() && self.outcome_frame != state.final_top)
        {
            return Err(state.mismatch(Divergence::CallStack));
        }
        match (
            self.end.as_ref().expect("complete recording outcome"),
            result,
        ) {
            (End::Returned, Ok(values)) if state.candidate => Ok(ReplayOutcome::Returned(
                vm.aggregates()
                    .snapshot(&values)
                    .map_err(ReplayError::Aggregate)?,
            )),
            (End::Trapped(expected), Err(actual)) if same_trap(expected, &actual) => {
                Ok(ReplayOutcome::Trapped(actual))
            }
            _ => Err(state.mismatch(Divergence::Outcome)),
        }
    }
}

fn same_host_error(a: &HostError, b: &HostError) -> bool {
    match (a, b) {
        (HostError::UnknownSymbol, HostError::UnknownSymbol)
        | (HostError::SignatureMismatch, HostError::SignatureMismatch)
        | (HostError::ProgramMismatch, HostError::ProgramMismatch)
        | (HostError::Failed, HostError::Failed) => true,
        (HostError::Custom(a), HostError::Custom(b)) => Arc::ptr_eq(a, b),
        _ => false,
    }
}

fn same_trap(a: &TrapInfo, b: &TrapInfo) -> bool {
    if a.func != b.func || a.pc != b.pc || a.span_id != b.span_id {
        return false;
    }
    match (&a.trap, &b.trap) {
        (Trap::FuelExceeded, Trap::FuelExceeded)
        | (Trap::CallDepthExceeded, Trap::CallDepthExceeded)
        | (Trap::HostCallLimitExceeded, Trap::HostCallLimitExceeded)
        | (Trap::AggregateSizeOverflow, Trap::AggregateSizeOverflow)
        | (Trap::PayloadSizeOverflow, Trap::PayloadSizeOverflow)
        | (Trap::InvalidPc, Trap::InvalidPc)
        | (Trap::RegOutOfBounds, Trap::RegOutOfBounds)
        | (Trap::ConstOutOfBounds, Trap::ConstOutOfBounds)
        | (Trap::TypeIdOutOfBounds, Trap::TypeIdOutOfBounds)
        | (Trap::ElemTypeIdOutOfBounds, Trap::ElemTypeIdOutOfBounds)
        | (Trap::ArityMismatch, Trap::ArityMismatch)
        | (Trap::IntCastOverflow, Trap::IntCastOverflow)
        | (Trap::DecimalScaleMismatch, Trap::DecimalScaleMismatch)
        | (Trap::DecimalOverflow, Trap::DecimalOverflow)
        | (Trap::DivByZero, Trap::DivByZero)
        | (Trap::IntDivOverflow, Trap::IntDivOverflow)
        | (Trap::FloatToIntInvalid, Trap::FloatToIntInvalid)
        | (Trap::IndexOutOfBounds, Trap::IndexOutOfBounds)
        | (Trap::StrNotCharBoundary, Trap::StrNotCharBoundary)
        | (Trap::InvalidUtf8, Trap::InvalidUtf8) => true,
        (
            Trap::TypeMismatch {
                expected: ae,
                actual: aa,
            },
            Trap::TypeMismatch {
                expected: be,
                actual: right_actual,
            },
        ) => ae == be && aa == right_actual,
        (
            Trap::PayloadWorkExceeded {
                limit: al,
                used: au,
                requested: ar,
            },
            Trap::PayloadWorkExceeded {
                limit: bl,
                used: bu,
                requested: br,
            },
        )
        | (
            Trap::AggregateWorkExceeded {
                limit: al,
                used: au,
                requested: ar,
            },
            Trap::AggregateWorkExceeded {
                limit: bl,
                used: bu,
                requested: br,
            },
        ) => al == bl && au == bu && ar == br,
        (Trap::AggError(a), Trap::AggError(b)) => a == b,
        (
            Trap::CallableSignatureMismatch {
                signature: a,
                function: af,
            },
            Trap::CallableSignatureMismatch {
                signature: b,
                function: bf,
            },
        ) => a == b && af == bf,
        (
            Trap::ContractWorkExceeded {
                function: af,
                position: ap,
                limit: al,
            },
            Trap::ContractWorkExceeded {
                function: bf,
                position: bp,
                limit: bl,
            },
        ) => af == bf && ap == bp && al == bl,

        (
            Trap::ContractMismatch {
                function: ac,
                position: aa,
                error: ae,
            },
            Trap::ContractMismatch {
                function: bc,
                position: right_arg,
                error: be,
            },
        ) => ac == bc && aa == right_arg && ae == be,
        (
            Trap::HostCallFailed {
                symbol: as_,
                sig_hash: ah,
                error: ae,
            },
            Trap::HostCallFailed {
                symbol: bs,
                sig_hash: bh,
                error: be,
            },
        ) => as_ == bs && ah == bh && same_host_error(ae, be),
        (
            Trap::HostReturnArityMismatch {
                expected: ae,
                actual: aa,
            },
            Trap::HostReturnArityMismatch {
                expected: be,
                actual: right_actual,
            },
        ) => ae == be && aa == right_actual,
        (Trap::TrapCode(a), Trap::TrapCode(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asm::{Asm, FunctionSig, ProgramBuilder};
    use crate::host::HostSig;
    use crate::program::ValueType;
    use crate::recording::{Call, RecordLimits, Recorder};
    use crate::vm::Limits;
    use alloc::vec;

    struct Echo;
    impl Host for Echo {
        fn call(
            &mut self,
            _: &str,
            _: SigHash,
            args: &[ValueRef<'_>],
            rets: &mut [Value],
            _: HostContext<'_, '_>,
        ) -> Result<u64, HostError> {
            rets[0] = args[0].to_value();
            Ok(0)
        }
    }

    fn record(host: bool, aggregate: bool) -> Recording {
        let kind = if aggregate {
            ValueType::Agg
        } else {
            ValueType::I64
        };
        let mut builder = ProgramBuilder::new();
        let mut body = Asm::new();
        body.set_register_types(vec![ValueType::Unit, kind, kind]);
        if host {
            let import = builder.host_sig_for(
                "echo",
                HostSig {
                    args: vec![kind],
                    rets: vec![kind],
                },
            );
            body.host_call_sig_id(0, import, 0, &[1], &[2]);
            body.ret(0, &[2]);
        } else {
            body.const_i64(2, 7);
            body.ret(0, &[2]);
        }
        builder
            .push_function_checked(
                body,
                FunctionSig {
                    arg_types: vec![kind],
                    ret_types: vec![kind],
                },
            )
            .unwrap();
        let program = Arc::new(builder.build_verified().unwrap());
        let mut vm = Vm::new(Echo, Limits::default());
        let arg = if aggregate {
            let first = vm.aggregates_mut().tuple_new(vec![Value::I64(7)]).unwrap();
            let second = vm.aggregates_mut().tuple_new(vec![Value::I64(7)]).unwrap();
            Value::Agg(
                vm.aggregates_mut()
                    .tuple_new(vec![Value::Agg(first), Value::Agg(second)])
                    .unwrap(),
            )
        } else {
            Value::I64(7)
        };
        let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
        vm.run(
            &program,
            FuncId(0),
            &[arg],
            recorder.mask(),
            Some(&mut recorder),
        )
        .unwrap();
        recorder.finish()
    }

    fn callback_recording() -> Recording {
        control_recording(false)
    }

    fn control_recording(exit: bool) -> Recording {
        use crate::trace::TraceSink;
        use crate::vm::{InvocationOptions, RunStatus};
        let mut b = ProgramBuilder::new();
        let sig = FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::I64],
        };
        let root = b.declare_function(sig.clone()).unwrap();
        let handler = b.declare_function(sig).unwrap();
        let import = b.host_sig_for(
            "signal",
            HostSig {
                args: vec![ValueType::I64],
                rets: vec![ValueType::I64],
            },
        );
        let mut a = b.function_asm(root).unwrap();
        let result = a.alloc_register(ValueType::I64);
        a.host_call_sig_id(0, import, 0, &[1], &[result])
            .ret(0, &[result]);
        b.define_function(root, a).unwrap();
        let mut a = b.function_asm(handler).unwrap();
        let one = a.alloc_register(ValueType::I64);
        let result = a.alloc_register(ValueType::I64);
        a.const_i64(one, 1)
            .i64_add(result, 1, one)
            .ret(0, &[result]);
        b.define_function(handler, a).unwrap();
        let program = Arc::new(b.build_verified().unwrap());
        let recorder = Rc::new(RefCell::new(
            Recorder::new(program.clone(), RecordLimits::default())
                .with_instructions(crate::recording::InstructionCapture::default()),
        ));
        let mut vm = Vm::new(Echo, Limits::default());
        let args = [Value::I64(41)];
        let mut run = vm
            .start(
                &program,
                root,
                &args,
                InvocationOptions::default()
                    .yield_host_calls()
                    .trace_owned(recorder.mask(), recorder.clone()),
            )
            .unwrap();
        assert_eq!(run.resume(100).unwrap(), RunStatus::AwaitingHost);
        run.pending_host_call()
            .unwrap()
            .call_guest(Value::Func(handler), args.to_vec())
            .unwrap();
        assert_eq!(run.resume(100).unwrap(), RunStatus::AwaitingHost);
        let mut request = run.pending_host_call().unwrap();
        let mut results = request.take_callback_results().unwrap();
        if exit {
            let target = request.inspect().frames().last().unwrap().activation;
            request
                .context()
                .tuple_new(vec![Value::Bytes(vec![7; 8])])
                .unwrap();
            request.return_from(&target, results).unwrap();
        } else {
            request.complete(&mut results, Ok(0)).unwrap();
        }
        assert_eq!(run.resume(100).unwrap(), RunStatus::Complete);
        drop(run);
        let record = Rc::try_unwrap(recorder).unwrap().into_inner().finish();
        record.replay().unwrap();
        record
    }

    #[test]
    fn corrupted_transfer_target_order_values_callback_count_and_work_are_rejected() {
        for change in 0..8 {
            let mut record = control_recording(true);
            let command = record.commands.last_mut().unwrap();
            let Action::ReturnFrom {
                target,
                target_location,
                unwound_callbacks,
                values,
                work,
                ..
            } = &mut command.action
            else {
                panic!("transfer fixture");
            };
            match change {
                0 => *target = 999,
                1 => command.sequence += 1,
                2 => command.location.pc += 1,
                3 => record.arena.values[values.start] = RecordedValue::I64(99),
                4 => *unwound_callbacks += 1,
                5 => work.aggregate = 0,
                6 => work.payload = 0,
                7 => target_location.pc += 1,
                _ => unreachable!(),
            }
            assert!(
                record.replay().is_err(),
                "corrupted transfer evidence {change} must diverge"
            );
        }
    }

    #[test]
    fn corrupted_callback_return_is_checked_against_reexecuted_guest() {
        let mut record = callback_recording();
        let Action::CallGuest {
            returned: Some(returned),
            ..
        } = &record.commands[0].action
        else {
            panic!("return")
        };
        record.arena.values[returned.values.start] = RecordedValue::I64(99);
        assert!(matches!(
            record.replay(),
            Err(ReplayError::Diverged {
                boundary: ReplayBoundary::ControlReturn { index: 0 },
                ..
            })
        ));
    }

    #[test]
    fn corrupted_control_order_sites_targets_arguments_and_parent_links_are_rejected() {
        for change in 0..8 {
            let mut record = callback_recording();
            match change {
                0 => record.commands[0].sequence += 1,
                1 => record.commands[0].location.pc += 1,
                2 => {
                    let Action::CallGuest { callee, .. } = record.commands[0].action else {
                        panic!("callback fixture");
                    };
                    record.arena.values[callee] = RecordedValue::Func(FuncId(99));
                }
                3 => {
                    let Action::CallGuest { ref args, .. } = record.commands[0].action else {
                        panic!("callback fixture");
                    };
                    record.arena.values[args.start] = RecordedValue::I64(99);
                }
                4 => {
                    let Action::CallGuest {
                        ref mut returned, ..
                    } = record.commands[0].action
                    else {
                        panic!("callback fixture");
                    };
                    *returned = None;
                }
                5 => record.calls[0].sequence += 1,
                6 => {
                    let history = record.history.as_mut().unwrap();
                    let child = history
                        .steps
                        .iter_mut()
                        .find(|step| step.parent.is_some())
                        .unwrap();
                    child.parent = None;
                }
                7 => record.commands.clear(),
                _ => unreachable!(),
            }
            assert!(
                record.replay().is_err(),
                "changed control evidence {change} must diverge"
            );
        }
    }

    fn reason(record: &Recording) -> Divergence {
        let Err(ReplayError::Diverged {
            location: Some(_),
            reason,
            ..
        }) = record.replay()
        else {
            panic!("changed evidence must diverge during guest execution");
        };
        reason
    }

    #[test]
    fn changed_payload_budget_diagnostic_fields_are_rejected() {
        for field in 0..3 {
            let mut builder = ProgramBuilder::new();
            let mut body = Asm::new();
            body.set_register_types(vec![ValueType::Unit, ValueType::Bytes]);
            body.ret(0, &[1]);
            builder
                .push_function_checked(
                    body,
                    FunctionSig {
                        arg_types: vec![ValueType::Bytes],
                        ret_types: vec![ValueType::Bytes],
                    },
                )
                .unwrap();
            let program = Arc::new(builder.build_verified().unwrap());
            let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
            Vm::new(
                Echo,
                Limits {
                    max_payload_work: 3,
                    ..Limits::default()
                },
            )
            .run(
                &program,
                FuncId(0),
                &[Value::Bytes(vec![1; 4])],
                recorder.mask(),
                Some(&mut recorder),
            )
            .unwrap_err();
            let mut record = recorder.finish();
            assert!(matches!(record.replay(), Ok(ReplayOutcome::Trapped(_))));
            let Some(End::Trapped(TrapInfo {
                trap:
                    Trap::PayloadWorkExceeded {
                        limit,
                        used,
                        requested,
                    },
                ..
            })) = &mut record.end
            else {
                panic!("payload trap")
            };
            match field {
                0 => *limit += 1,
                1 => *used += 1,
                _ => *requested += 1,
            }
            assert_eq!(reason(&record), Divergence::Outcome);
        }
    }

    #[test]
    fn replay_computes_guest_result_instead_of_returning_saved_values() {
        let mut record = record(false, false);
        let root = record.candidate.as_ref().unwrap().1.start;
        record.arena.values[root] = RecordedValue::I64(99);
        assert_eq!(
            reason(&record),
            Divergence::Value {
                aggregate: None,
                index: 0
            }
        );
    }

    #[test]
    fn changed_host_arguments_imports_locations_and_counts_are_rejected() {
        let mut changed = record(true, false);
        let root = changed.calls[0].args.start;
        changed.arena.values[root] = RecordedValue::I64(99);
        assert_eq!(
            reason(&changed),
            Divergence::Value {
                aggregate: None,
                index: 0
            }
        );

        let mut changed = record(true, false);
        changed.calls[0].args = 0..0;
        assert_eq!(
            reason(&changed),
            Divergence::Arity {
                expected: 0,
                actual: 1
            }
        );

        let mut changed = record(true, false);
        changed.calls[0].import = HostSigId(99);
        assert_eq!(
            reason(&changed),
            Divergence::Import {
                expected: HostSigId(99),
                actual: HostSigId(0)
            }
        );

        let mut changed = record(true, false);
        let actual = changed.calls[0].location;
        changed.calls[0].location.pc += 1;
        assert_eq!(
            reason(&changed),
            Divergence::Location {
                expected: changed.calls[0].location,
                actual: Some(actual)
            }
        );

        let mut changed = record(true, false);
        changed.calls.clear();
        assert_eq!(
            reason(&changed),
            Divergence::HostCalls {
                expected: 0,
                actual: 1
            }
        );

        let mut changed = record(true, false);
        let call = changed.calls.pop().unwrap();
        changed.calls.push(Call {
            sequence: call.sequence,
            work: call.work,
            constructed: call.constructed.clone(),
            frame: call.frame,
            location: call.location,
            import: call.import,
            args: call.args.clone(),
            outcome: CallOutcome::Failed(HostError::Failed),
        });
        changed.calls.push(call);
        assert_eq!(
            reason(&changed),
            Divergence::HostCalls {
                expected: 2,
                actual: 1
            }
        );
    }

    #[test]
    fn repeated_calls_at_one_instruction_report_the_exact_ordinal() {
        let mut builder = ProgramBuilder::new();
        let mut body = Asm::new();
        body.set_register_types(vec![
            ValueType::Unit,
            ValueType::I64,
            ValueType::I64,
            ValueType::I64,
            ValueType::Bool,
        ]);
        let import = builder.host_sig_for(
            "echo",
            HostSig {
                args: vec![ValueType::I64],
                rets: vec![ValueType::I64],
            },
        );
        let head = body.label();
        let exit = body.label();
        body.const_i64(1, 0).const_i64(2, 1).const_i64(3, 3);
        body.jmp(head);
        body.place(head).unwrap();
        body.host_call_sig_id(0, import, 0, &[1], &[1]);
        body.i64_add(1, 1, 2).i64_lt(4, 1, 3).br(4, head, exit);
        body.place(exit).unwrap();
        body.ret(0, &[1]);
        builder
            .push_function_checked(
                body,
                FunctionSig {
                    arg_types: vec![],
                    ret_types: vec![ValueType::I64],
                },
            )
            .unwrap();
        let program = Arc::new(builder.build_verified().unwrap());
        let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
        Vm::new(Echo, Limits::default())
            .run(
                &program,
                FuncId(0),
                &[],
                recorder.mask(),
                Some(&mut recorder),
            )
            .unwrap();
        let mut record = recorder.finish();
        assert_eq!(record.calls.len(), 3);
        let location = record.calls[0].location;
        assert!(record.calls.iter().all(|call| call.location == location));
        let root = record.calls[1].args.start;
        record.arena.values[root] = RecordedValue::I64(99);
        assert!(matches!(record.replay(), Err(ReplayError::Diverged {
            boundary: ReplayBoundary::HostCall { index: 1 },
            location: Some(actual),
            reason: Divergence::Value { aggregate: None, index: 0 },
        }) if actual == location));
    }

    #[test]
    fn changed_or_missing_activation_provenance_is_rejected() {
        let mut changed = record(true, false);
        changed.frames[0].caller = Some(changed.calls[0].location);
        assert_eq!(reason(&changed), Divergence::CallStack);
        let mut changed = record(true, false);
        changed.calls[0].frame = None;
        assert_eq!(reason(&changed), Divergence::CallStack);
        let mut changed = record(true, false);
        changed.outcome_frame = None;
        assert_eq!(reason(&changed), Divergence::CallStack);
        let mut changed = record(true, false);
        changed.frames.push(changed.frames[0]);
        assert_eq!(reason(&changed), Divergence::CallStack);
    }

    #[test]
    fn equal_contents_do_not_hide_changed_graph_identity() {
        let mut record = record(true, true);
        let root = record.calls[0].args.start;
        let RecordedValue::Agg(original) = record.arena.values[root] else {
            panic!("aggregate root")
        };
        // A second root node has equal contents but is a different identity. It was not
        // supplied by the guest: the invocation still binds the original root.
        let node = &record.arena.nodes[original.0 as usize];
        let duplicate = AggregateId(u32::try_from(record.arena.nodes.len()).unwrap());
        record.arena.nodes.push(crate::recording::values::Node {
            kind: node.kind,
            fields: node.fields.clone(),
            initial: true,
            created_offset: None,
        });
        record.arena.values[root] = RecordedValue::Agg(duplicate);
        assert_eq!(
            reason(&record),
            Divergence::Identity {
                aggregate: duplicate
            }
        );
    }
    #[test]
    fn changed_host_construction_evidence_cannot_hide_a_guest_budget_failure() {
        struct Produces;
        impl Host for Produces {
            fn call(
                &mut self,
                _: &str,
                _: SigHash,
                _: &[ValueRef<'_>],
                out: &mut [Value],
                mut cx: HostContext<'_, '_>,
            ) -> Result<u64, HostError> {
                out[0] = Value::Agg(cx.tuple_new(vec![Value::I64(42)])?);
                Ok(0)
            }
        }
        for remove_work in [false, true] {
            let mut b = ProgramBuilder::new();
            let entry = b
                .declare_function(FunctionSig {
                    arg_types: vec![],
                    ret_types: vec![ValueType::Agg],
                })
                .unwrap();
            let import = b.host_sig_for(
                "produce",
                HostSig {
                    args: vec![],
                    rets: vec![ValueType::Agg],
                },
            );
            let mut a = b.function_asm(entry).unwrap();
            let result = a.alloc_register(ValueType::Agg);
            let wrapper = a.alloc_register(ValueType::Agg);
            a.host_call_sig_id(0, import, 0, &[], &[result])
                .tuple_new(wrapper, &[result])
                .ret(0, &[wrapper]);
            b.define_function(entry, a).unwrap();
            let program = Arc::new(b.build_verified().unwrap());
            let mut recorder = Recorder::new(program.clone(), RecordLimits::default());
            let error = Vm::new(
                Produces,
                Limits {
                    max_aggregate_work: 2,
                    ..Limits::default()
                },
            )
            .run(&program, entry, &[], recorder.mask(), Some(&mut recorder))
            .unwrap_err();
            assert!(matches!(error.trap, Trap::AggregateWorkExceeded { .. }));
            let mut recording = recorder.finish();
            assert!(matches!(
                recording.replay().unwrap(),
                ReplayOutcome::Trapped(_)
            ));
            if remove_work {
                recording.calls[0].work = crate::vm::DataWork::default();
            } else {
                recording.calls[0].constructed.clear();
            }
            assert!(
                recording.replay().is_err(),
                "altered work or missing host nodes must fail checked replay"
            );
        }
    }
}
