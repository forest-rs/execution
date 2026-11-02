// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! One capture authority across independently owned code segments.

use super::*;
use crate::instance::Instance;
use crate::recording::execution::{self, RecordedActivation};
use crate::trace::ControlEvent;
use alloc::{boxed::Box, collections::BTreeMap, vec};

/// Owns the boundary evidence of one execution, including independently compiled children.
///
/// Install the recommended [`Self::mask`] on the root. Children inherit the observer unless
/// explicitly replaced. Capture limits apply to the entire execution, and a missing child
/// observer makes the evidence incomplete. Captured values and code survive the VM and host.
pub struct Recorder {
    root: Option<SegmentRecorder>,
    session: Option<Session>,
    identity: Arc<()>,
    limits: RecordLimits,
    instructions: Option<InstructionCapture>,
}

impl Recorder {
    /// Retains the root's exact definition and one execution-wide capture allowance.
    pub fn new(program: Arc<VerifiedProgram>, limits: RecordLimits) -> Self {
        Self {
            root: Some(SegmentRecorder::new(program, limits.clone())),
            session: None,
            identity: Arc::new(()),
            limits,
            instructions: None,
        }
    }

    /// Requests instruction history, bounded across all code segments. Register watches apply
    /// to the root definition; child histories retain instructions without implicit ID-based
    /// propagation of root register watches to unrelated code.
    #[must_use]
    pub fn with_instructions(mut self, config: InstructionCapture) -> Self {
        if let Some(root) = self.root.take() {
            self.root = Some(root.with_instructions(config.clone()));
        }
        self.instructions = Some(config);
        self
    }

    /// Freezes owned evidence. Incomplete captures remain inspectable and reject checked replay.
    pub fn finish(self) -> Recording {
        match self.session {
            Some(session) => session.finish(self.identity),
            None => self.root.expect("root capture").finish(self.identity),
        }
    }

    fn dispatch(&mut self, callback: impl FnOnce(&mut SegmentRecorder)) {
        if let Some(session) = &mut self.session {
            session.dispatch(session.current(), callback);
        } else {
            callback(self.root.as_mut().expect("root capture"));
        }
    }

    fn active(&self) -> &SegmentRecorder {
        if let Some(session) = &self.session {
            session.slots[session.current()]
                .capture
                .as_ref()
                .expect("active segment capture")
        } else {
            self.root.as_ref().expect("root capture")
        }
    }
}
impl core::fmt::Debug for Recorder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Recorder")
            .field(
                "segments",
                &self
                    .session
                    .as_ref()
                    .map_or(1, |session| session.slots.len()),
            )
            .finish_non_exhaustive()
    }
}

struct Slot {
    meta: execution::Segment,
    capture: Option<SegmentRecorder>,
    location: Option<TraceLocation>,
    returned: bool,
}
struct Session {
    slots: Vec<Slot>,
    indices: BTreeMap<u64, usize>,
    stack: Vec<usize>,
    pending: Option<usize>,
    depth: usize,
    instances: Vec<Instance>,
    instance_indices: BTreeMap<usize, usize>,
    transfers: Vec<execution::Transfer>,
    limits: RecordLimits,
    used: RecordLimits,
    steps: usize,
    instructions: Option<InstructionCapture>,
    fault: Option<(CaptureFault, Option<TraceLocation>, Arc<VerifiedProgram>)>,
    target: Option<RecordedActivation>,
    failure: Option<u64>,
}

fn counts(capture: &SegmentRecorder) -> RecordLimits {
    RecordLimits {
        values: capture.builder.arena.values.len(),
        aggregates: capture.builder.arena.nodes.len(),
        payload_bytes: capture.builder.arena.payload_bytes,
        host_calls: capture.calls.len(),
        control_commands: capture.commands.len(),
        call_frames: capture.frames.len(),
    }
}
fn steps(capture: &SegmentRecorder) -> usize {
    capture
        .history
        .as_ref()
        .map_or(0, |history| history.steps.len())
}
fn activation(id: &crate::vm::ActivationId) -> RecordedActivation {
    RecordedActivation {
        segment: id.segment_ordinal(),
        activation: id.ordinal(),
    }
}

impl Session {
    fn new(
        root: SegmentRecorder,
        limits: RecordLimits,
        instructions: Option<InstructionCapture>,
    ) -> Self {
        let used = counts(&root);
        let steps = steps(&root);
        let entry = root.entry.as_ref().map(|entry| entry.func);
        let fault = root
            .fault
            .clone()
            .map(|fault| (fault, root.fault_location, Arc::clone(&root.program)));
        let program = Arc::clone(&root.program);
        let mut indices = BTreeMap::new();
        indices.insert(0, 0);
        Self {
            slots: vec![Slot {
                meta: execution::Segment {
                    id: 0,
                    parent: None,
                    instance: None,
                    program,
                    entry,
                    denied: false,
                },
                capture: Some(root),
                location: None,
                returned: false,
            }],
            indices,
            stack: vec![0],
            pending: None,
            depth: 1,
            instances: Vec::new(),
            instance_indices: BTreeMap::new(),
            transfers: Vec::new(),
            limits,
            used,
            steps,
            instructions,
            fault,
            target: None,
            failure: None,
        }
    }
    fn current(&self) -> usize {
        self.stack.last().copied().unwrap_or(0)
    }
    fn fail(&mut self, fault: CaptureFault, location: Option<TraceLocation>) {
        if self.fault.is_none() {
            self.fault = Some((
                fault,
                location,
                Arc::clone(&self.slots[self.current()].meta.program),
            ));
        }
    }
    fn instance(&mut self, instance: &Instance) -> usize {
        // The retained instance vector pins every key until these construction indexes disappear.
        let key = Arc::as_ptr(&instance.0) as usize;
        if let Some(&index) = self.instance_indices.get(&key) {
            return index;
        }
        let index = self.instances.len();
        self.instances.push(instance.clone());
        self.instance_indices.insert(key, index);
        index
    }
    fn ensure_capture(&mut self, index: usize) {
        if self.slots[index].capture.is_none() {
            let mut capture = SegmentRecorder::new(
                Arc::clone(&self.slots[index].meta.program),
                self.limits.clone(),
            );
            if let Some(config) = &self.instructions {
                let mut child_config = config.clone();
                child_config.watches.clear();
                capture = capture.with_instructions(child_config);
            }
            self.slots[index].capture = Some(capture);
        }
    }
    fn dispatch(&mut self, index: usize, callback: impl FnOnce(&mut SegmentRecorder)) {
        if self.fault.is_some() {
            return;
        }
        self.dispatch_even_if_incomplete(index, callback);
    }
    fn dispatch_even_if_incomplete(
        &mut self,
        index: usize,
        callback: impl FnOnce(&mut SegmentRecorder),
    ) {
        self.ensure_capture(index);
        let capture = self.slots[index].capture.as_mut().expect("capture slot");
        let before = counts(capture);
        let before_steps = steps(capture);
        macro_rules! allowance { ($($field:ident),*) => { $(capture.builder.limits.$field = before.$field + self.limits.$field.saturating_sub(self.used.$field);)* }; }
        allowance!(
            values,
            aggregates,
            payload_bytes,
            host_calls,
            control_commands,
            call_frames
        );
        if let Some(history) = &mut capture.history {
            history.config.max_steps = before_steps
                + self
                    .instructions
                    .as_ref()
                    .map_or(0, |config| config.max_steps)
                    .saturating_sub(self.steps);
        }
        callback(capture);
        let after = counts(capture);
        macro_rules! debit { ($($field:ident),*) => { $(self.used.$field += after.$field - before.$field;)* }; }
        debit!(
            values,
            aggregates,
            payload_bytes,
            host_calls,
            control_commands,
            call_frames
        );
        self.steps += steps(capture) - before_steps;
        let fault = capture
            .fault
            .clone()
            .map(|fault| (fault, capture.fault_location));
        if let Some((mut fault, location)) = fault {
            if let CaptureFault::Budget { resource, limit } = &mut fault {
                *limit = match resource {
                    CaptureResource::Values => self.limits.values,
                    CaptureResource::Aggregates => self.limits.aggregates,
                    CaptureResource::PayloadBytes => self.limits.payload_bytes,
                    CaptureResource::HostCalls => self.limits.host_calls,
                    CaptureResource::ControlCommands => self.limits.control_commands,
                    CaptureResource::CallFrames => self.limits.call_frames,
                };
            }
            self.fail(fault, location);
        }
    }
    fn command(&mut self, location: TraceLocation) -> bool {
        if self.used.control_commands >= self.limits.control_commands {
            self.fail(
                CaptureFault::Budget {
                    resource: CaptureResource::ControlCommands,
                    limit: self.limits.control_commands,
                },
                Some(location),
            );
            false
        } else {
            self.used.control_commands += 1;
            true
        }
    }
    fn capture_values(
        &mut self,
        index: usize,
        values: &[crate::value::Value],
        created: &Range<u32>,
        heap: &crate::aggregates::AggHeap,
    ) -> Option<(Range<usize>, Vec<crate::recording::AggregateId>)> {
        let mut result = None;
        self.dispatch(index, |capture| {
            let captured = (|| {
                capture.builder.roots(
                    created
                        .clone()
                        .map(|handle| ValueRef::Agg(crate::value::AggHandle(handle))),
                    heap,
                )?;
                let constructed = capture.builder.constructed(created.clone())?;
                let values = capture
                    .builder
                    .roots(values.iter().map(ValueRef::from_value), heap)?;
                Ok((values, constructed))
            })();
            match captured {
                Ok(values) => result = Some(values),
                Err(fault) => capture.fail(fault),
            }
        });
        result
    }
    fn control(&mut self, program: &Program, event: ControlEvent<'_>) {
        if self.fault.is_some() {
            return;
        }
        let index = self.current();
        if !core::ptr::eq(program, self.slots[index].meta.program.program()) {
            self.fail(CaptureFault::ProgramMismatch, None);
            return;
        }
        if !self.slots[0].capture.as_ref().expect("root").started {
            self.fail(CaptureFault::MissingCompletion, None);
            return;
        }
        match event {
            ControlEvent::EnterInstance {
                origin,
                location,
                parent,
                child,
                source_instance,
                instance,
                entry,
                args,
                work,
                created,
                aggregates,
            } => {
                if self.pending.is_some()
                    || parent.segment_ordinal() != self.slots[index].meta.id
                    || self.indices.contains_key(&child.segment_ordinal())
                {
                    self.fail(CaptureFault::InvalidSequence, Some(location));
                    return;
                }
                if !self.command(location) {
                    return;
                }
                let Some((args, constructed)) =
                    self.capture_values(index, args, created, aggregates)
                else {
                    return;
                };
                let source = self.instance(source_instance);
                self.slots[index].meta.instance = Some(source);
                let target = self.instance(instance);
                let child_index = self.slots.len();
                self.indices.insert(child.segment_ordinal(), child_index);
                self.slots.push(Slot {
                    meta: execution::Segment {
                        id: child.segment_ordinal(),
                        parent: Some(activation(parent)),
                        instance: Some(target),
                        program: Arc::new(instance.program().clone()),
                        entry: Some(entry),
                        denied: false,
                    },
                    capture: None,
                    location: None,
                    returned: false,
                });
                self.pending = Some(child_index);
                self.transfers.push(execution::Transfer {
                    frame: self.slots[index]
                        .capture
                        .as_ref()
                        .and_then(|capture| capture.top),
                    segment: self.slots[index].meta.id,
                    sequence: self.slots[index].capture.as_ref().expect("source").sequence,
                    location,
                    action: execution::Action::Enter {
                        origin,
                        parent: activation(parent),
                        child: activation(child),
                        args,
                        work,
                        constructed,
                    },
                });
                self.slots[index]
                    .capture
                    .as_mut()
                    .expect("admitting segment")
                    .sequence += 1;
            }
            ControlEvent::RejectInstance {
                location,
                parent,
                child,
                error: _,
            } => {
                let Some(&child_index) = self.indices.get(&child.segment_ordinal()) else {
                    self.fail(CaptureFault::InvalidSequence, Some(location));
                    return;
                };
                if self.pending != Some(child_index)
                    || self.slots[child_index].meta.parent != Some(activation(parent))
                    || self.slots[child_index].capture.is_some()
                {
                    self.fail(CaptureFault::InvalidSequence, Some(location));
                    return;
                }
                self.slots[child_index].meta.denied = true;
                self.pending = None;
                self.transfers.push(execution::Transfer {
                    frame: self.slots[index]
                        .capture
                        .as_ref()
                        .and_then(|capture| capture.top),
                    segment: self.slots[index].meta.id,
                    sequence: self.slots[index].capture.as_ref().expect("parent").sequence,
                    location,
                    action: execution::Action::Rejected {
                        parent: activation(parent),
                        child: activation(child),
                    },
                });
                self.slots[index]
                    .capture
                    .as_mut()
                    .expect("admitting segment")
                    .sequence += 1;
            }
            ControlEvent::ReturnInstance {
                location,
                child,
                parent,
                values,
                aggregates,
                ..
            } => {
                let Some(&child_index) = self.indices.get(&child.segment_ordinal()) else {
                    self.fail(CaptureFault::InvalidSequence, Some(location));
                    return;
                };
                if parent.segment_ordinal() != self.slots[index].meta.id
                    || self.slots[child_index].returned
                    || self.slots[child_index].meta.parent != Some(activation(parent))
                {
                    self.fail(CaptureFault::InvalidSequence, Some(location));
                    return;
                }
                if !self.slots[child_index]
                    .capture
                    .as_ref()
                    .is_some_and(|capture| matches!(capture.end, Some(End::Returned)))
                {
                    self.fail(CaptureFault::MissingCompletion, None);
                    return;
                }
                let Some((values, _)) = self.capture_values(index, values, &(0..0), aggregates)
                else {
                    return;
                };
                self.slots[child_index].returned = true;
                self.transfers.push(execution::Transfer {
                    frame: self.slots[index]
                        .capture
                        .as_ref()
                        .and_then(|capture| capture.top),
                    segment: self.slots[index].meta.id,
                    sequence: self.slots[index].capture.as_ref().expect("parent").sequence,
                    location,
                    action: execution::Action::Returned {
                        child: activation(child),
                        parent: activation(parent),
                        values,
                    },
                });
                self.slots[index]
                    .capture
                    .as_mut()
                    .expect("admitting segment")
                    .sequence += 1;
            }
            ControlEvent::ReturnAcross {
                location,
                source,
                target,
                target_location,
                unwound_callbacks,
                values,
                work,
                created,
                aggregates,
                ..
            } => {
                let Some(&target_index) = self.indices.get(&target.segment_ordinal()) else {
                    self.fail(CaptureFault::InvalidSequence, Some(location));
                    return;
                };
                if source.segment_ordinal() != self.slots[index].meta.id
                    || !self.stack.contains(&target_index)
                    || target_index == index
                {
                    self.fail(CaptureFault::InvalidSequence, Some(location));
                    return;
                }
                if !self.command(location) {
                    return;
                }
                let Some((values, constructed)) =
                    self.capture_values(index, values, created, aggregates)
                else {
                    return;
                };
                self.target = Some(activation(target));
                let target_position = self
                    .stack
                    .iter()
                    .position(|&active| active == target_index)
                    .expect("active target");
                for &crossed in &self.stack[target_position + 1..] {
                    let capture = self.slots[crossed]
                        .capture
                        .as_mut()
                        .expect("active crossed segment");
                    capture.active_commands.clear();
                    if let Some(history) = &mut capture.history {
                        history.begin_transfer(target.ordinal());
                    }
                }
                let capture = self.slots[target_index]
                    .capture
                    .as_mut()
                    .expect("target evidence");
                let Some(keep) = capture.active_commands.len().checked_sub(unwound_callbacks)
                else {
                    self.fail(CaptureFault::InvalidSequence, Some(location));
                    return;
                };
                capture.active_commands.truncate(keep);
                if let Some(history) = &mut capture.history {
                    history.begin_transfer(target.ordinal());
                }
                capture.external_root_exit = target.ordinal() == 0;
                self.transfers.push(execution::Transfer {
                    frame: self.slots[index]
                        .capture
                        .as_ref()
                        .and_then(|capture| capture.top),
                    segment: self.slots[index].meta.id,
                    sequence: self.slots[index].capture.as_ref().expect("source").sequence,
                    location,
                    action: execution::Action::ReturnFrom {
                        source: activation(source),
                        target: activation(target),
                        target_location,
                        unwound_callbacks,
                        values,
                        work,
                        constructed,
                    },
                });
                self.slots[index]
                    .capture
                    .as_mut()
                    .expect("admitting segment")
                    .sequence += 1;
            }
        }
    }
    fn run_start(&mut self, program: &Program, entry: FuncId, args: usize) {
        self.depth += 1;
        if self.fault.is_some() {
            return;
        }
        let Some(index) = self.pending.take() else {
            self.fail(CaptureFault::MultipleRuns, None);
            return;
        };
        self.stack.push(index);
        self.dispatch(index, |capture| capture.run_start(program, entry, args));
    }
    fn run_end(&mut self, program: &Program, outcome: TraceOutcome<'_>) {
        self.depth = self.depth.saturating_sub(1);
        if self.fault.is_some() {
            if self.depth == 0 {
                self.dispatch_even_if_incomplete(0, |capture| capture.run_end(program, outcome));
            }
            return;
        }
        let index = self.current();
        if matches!(outcome, TraceOutcome::Trap(_)) && self.failure.is_none() {
            self.failure = Some(self.slots[index].meta.id);
        }
        if let TraceOutcome::Unwound { .. } = outcome {
            if index == 0 {
                self.fail(CaptureFault::InvalidSequence, None);
                return;
            }
            let Some(target) = self.target else {
                self.fail(CaptureFault::InvalidSequence, None);
                return;
            };
            let location = self.slots[index].location.unwrap_or(TraceLocation {
                func: self.slots[index].meta.entry.unwrap_or(FuncId(0)),
                pc: 0,
            });
            self.dispatch(index, |capture| {
                if capture.top.is_some() {
                    capture.fail(CaptureFault::InvalidSequence);
                }
                if let Some(history) = &mut capture.history {
                    history.finished = true;
                }
                capture.end = Some(End::Transferred { target, location });
            });
        } else {
            self.dispatch(index, |capture| capture.run_end(program, outcome));
        }
        self.stack.pop();
    }
    fn finish(mut self, identity: Arc<()>) -> Recording {
        let failure_definition = self
            .failure
            .and_then(|id| self.indices.get(&id))
            .map(|&index| Arc::clone(&self.slots[index].meta.program));
        for (index, slot) in self.slots.iter().enumerate() {
            if index != 0 && slot.capture.is_none() && !slot.meta.denied && self.fault.is_none() {
                self.fault = Some((
                    CaptureFault::MissingCompletion,
                    slot.location,
                    Arc::clone(&slot.meta.program),
                ));
            }
        }
        let mut records = Vec::with_capacity(self.slots.len());
        let mut metadata = Vec::with_capacity(self.slots.len());
        for (index, slot) in self.slots.drain(..).enumerate() {
            metadata.push(slot.meta);
            records.push(slot.capture.map(|capture| {
                let mut record = capture.finish(Arc::clone(&identity));
                record.local_outcome = slot.location;
                record.context_required = index != 0;
                if matches!(record.end, Some(End::Trapped(_))) {
                    record.outcome_definition = failure_definition.clone();
                }
                record
            }));
        }
        if self.fault.is_none() {
            self.fault = records.iter().flatten().find_map(|record| {
                record
                    .fault
                    .clone()
                    .map(|fault| (fault, record.fault_location, Arc::clone(&record.program)))
            });
        }
        let mut root = records.remove(0).expect("root evidence");
        if let Some((fault, location, program)) = self.fault {
            root.fault = Some(fault);
            root.fault_location = location;
            root.fault_definition = Some(program);
        }
        root.execution = Some(Box::new(execution::ExecutionData {
            segments: metadata,
            indices: self.indices,
            instances: self.instances,
            transfers: self.transfers,
            children: records,
            failure: self.failure,
        }));
        root
    }
}

impl TraceSink for Recorder {
    fn mask(&self) -> TraceMask {
        self.active().mask()
    }
    fn wants_registers(&self, location: TraceLocation, phase: RegisterPhase) -> bool {
        self.session
            .as_ref()
            .is_none_or(|session| session.fault.is_none())
            && self.active().wants_registers(location, phase)
    }
    fn run_mask(&mut self, program: &Program, mask: TraceMask) {
        if let Some(session) = &mut self.session {
            let index = session.pending.unwrap_or_else(|| session.current());
            session.dispatch(index, |capture| capture.run_mask(program, mask));
        } else {
            self.dispatch(|capture| capture.run_mask(program, mask));
        }
    }
    fn run_start(&mut self, program: &Program, entry: FuncId, args: usize) {
        if let Some(session) = &mut self.session {
            session.run_start(program, entry, args);
        } else {
            self.dispatch(|capture| capture.run_start(program, entry, args));
        }
    }
    fn run_end(&mut self, program: &Program, outcome: TraceOutcome<'_>) {
        if let Some(session) = &mut self.session {
            session.run_end(program, outcome);
        } else {
            self.dispatch(|capture| capture.run_end(program, outcome));
        }
    }
    fn control(&mut self, program: &Program, event: ControlEvent<'_>) {
        if self.session.is_none() {
            self.session = Some(Session::new(
                self.root.take().expect("root capture"),
                self.limits.clone(),
                self.instructions.clone(),
            ));
        }
        self.session
            .as_mut()
            .expect("session")
            .control(program, event);
    }
    fn values(&mut self, program: &Program, event: ValueEvent<'_>) {
        self.dispatch(|capture| capture.values(program, event));
    }
    fn registers(&mut self, program: &Program, event: RegisterEvent<'_>) {
        self.dispatch(|capture| capture.registers(program, event));
    }
    fn instr(
        &mut self,
        program: &Program,
        func: FuncId,
        pc: u32,
        next: u32,
        span: Option<u64>,
        opcode: u8,
    ) {
        if let Some(session) = &mut self.session {
            let index = session.current();
            session.slots[index].location = Some(TraceLocation { func, pc });
        }
        self.dispatch(|capture| capture.instr(program, func, pc, next, span, opcode));
    }
    fn instr_end(
        &mut self,
        program: &Program,
        location: TraceLocation,
        next: Option<TraceLocation>,
        outcome: TraceOutcome<'_>,
    ) {
        self.dispatch(|capture| capture.instr_end(program, location, next, outcome));
    }
    fn scope_enter(
        &mut self,
        program: &Program,
        kind: ScopeKind,
        depth: usize,
        func: FuncId,
        pc: u32,
        span: Option<u64>,
    ) {
        self.dispatch(|capture| capture.scope_enter(program, kind, depth, func, pc, span));
    }
    fn scope_exit(
        &mut self,
        program: &Program,
        kind: ScopeKind,
        depth: usize,
        func: FuncId,
        pc: u32,
        span: Option<u64>,
        outcome: TraceOutcome<'_>,
    ) {
        if let Some(session) = &mut self.session {
            let index = session.current();
            session.slots[index].location = Some(TraceLocation { func, pc });
        }
        self.dispatch(|capture| capture.scope_exit(program, kind, depth, func, pc, span, outcome));
    }
}
