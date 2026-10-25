// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use crate::aggregates::{AggError, AggHeap, ValueSnapshot};
use crate::host::{Host, HostContext, HostError, SigHash, ValueRef};
use crate::program::{HostSigId, Program};
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
            (RecordedValue::Decimal(a), ValueRef::Decimal(b)) if *a == b => None,
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

struct RecordedHost<'a> {
    record: &'a Recording,
    state: Rc<RefCell<State>>,
}
impl RecordedHost<'_> {
    fn invoke(
        &mut self,
        args: &[ValueRef<'_>],
        rets: &mut [Value],
        ctx: HostContext<'_, '_>,
    ) -> Result<u64, ReplayError> {
        let mut state = self.state.borrow_mut();
        if let Some(error) = &state.error {
            return Err(error.clone());
        }
        let index = state.calls;
        state.boundary = ReplayBoundary::HostCall { index };
        let call = self.record.calls.get(index).ok_or_else(|| {
            state.mismatch(Divergence::HostCalls {
                expected: self.record.calls.len(),
                actual: index + 1,
            })
        })?;
        if state.location != Some(call.location) {
            return Err(state.mismatch(Divergence::Location {
                expected: call.location,
                actual: state.location,
            }));
        }
        if !self.record.frames.is_empty() && call.frame != state.top {
            return Err(state.mismatch(Divergence::CallStack));
        }
        if ctx.host_sig_id() != call.import {
            return Err(state.mismatch(Divergence::Import {
                expected: call.import,
                actual: ctx.host_sig_id(),
            }));
        }
        state.matches(
            self.record,
            self.record.arena.slice(&call.args),
            args.iter().copied(),
            ctx.aggregates(),
        )?;
        state.calls += 1;
        match &call.outcome {
            CallOutcome::Failed(_) => Ok(0),
            CallOutcome::Returned { values, fuel } => {
                if values.len() != rets.len() {
                    return Err(state.mismatch(Divergence::Arity {
                        expected: values.len(),
                        actual: rets.len(),
                    }));
                }
                for (slot, value) in rets.iter_mut().zip(self.record.arena.slice(values)) {
                    *slot = state.materialize_value(self.record, value, ctx.aggregates())?;
                }
                Ok(*fuel)
            }
        }
    }
}
impl Host for RecordedHost<'_> {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        args: &[ValueRef<'_>],
        rets: &mut [Value],
        ctx: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        let index = self.state.borrow().calls;
        match self.invoke(args, rets, ctx) {
            Err(error) => {
                self.state.borrow_mut().error = Some(error);
                Err(HostError::Failed)
            }
            Ok(fuel) => match &self.record.calls[index].outcome {
                CallOutcome::Failed(error) => Err(error.clone()),
                CallOutcome::Returned { .. } => Ok(fuel),
            },
        }
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
            .instructions
            .checked_sub(1)
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
        let Some(index) = state.instructions.checked_sub(1) else {
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
        state.instructions += 1;
        if state.error.is_some() {
            return;
        }
        if let Some(history) = &self.record.history {
            state.boundary = ReplayBoundary::Instruction { index };
            if let Some(step) = history.steps.get(index) {
                if step.location != (TraceLocation { func, pc })
                    || step.opcode != opcode
                    || step.frame.is_some_and(|frame| Some(frame) != state.top)
                {
                    state.error = Some(state.mismatch(Divergence::Instruction));
                }
            } else if history.status == crate::recording::InstructionStatus::Complete {
                state.error = Some(state.mismatch(Divergence::Instruction));
            }
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
        let Some(index) = state.instructions.checked_sub(1) else {
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
        };
        if step.outcome != crate::recording::InstructionOutcome::Pending
            && (step.location != location || step.next != next || step.outcome != actual)
        {
            state.boundary = ReplayBoundary::Instruction { index };
            state.error = Some(state.mismatch(Divergence::Instruction));
        }
    }
    fn values(&mut self, _: &Program, event: ValueEvent<'_>) {
        if let ValueEvent::ReturnCandidate {
            location,
            values,
            aggregates,
        } = event
        {
            let mut state = self.state.borrow_mut();
            if state.error.is_some() {
                return;
            }
            state.candidate = true;
            state.final_top = state.top;
            state.boundary = ReplayBoundary::ReturnCandidate;
            let result = match &self.record.candidate {
                Some((expected, roots)) if *expected == location => state.matches(
                    self.record,
                    self.record.arena.slice(roots),
                    values.iter().map(ValueRef::from_value),
                    aggregates,
                ),
                _ => Err(state.mismatch(Divergence::Outcome)),
            };
            if let Err(error) = result {
                state.error = Some(error);
            }
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
        let mut vm = Vm::new(
            RecordedHost {
                record: self,
                state: Rc::clone(&state),
            },
            entry.limits.clone(),
        );
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
        let result = vm.run(
            &self.program,
            entry.func,
            &args,
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
            Some(&mut check),
        );
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
}
