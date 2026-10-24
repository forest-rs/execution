// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use crate::{
    Call, CallOutcome, CaptureFault, CaptureResource, End, Entry, InstructionCapture,
    InstructionOutcome, InstructionStatus, RecordLimits, Recording,
    frames::{Frame, FrameId},
    instructions::{History, Step},
    values::Builder,
};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::Range;
use execution_tape::host::ValueRef;
use execution_tape::program::Program;
use execution_tape::trace::{
    HostCallOutcome, RegisterEvent, RegisterPhase, ScopeKind, TraceLocation, TraceMask,
    TraceOutcome, TraceSink, ValueEvent,
};
use execution_tape::value::FuncId;
use execution_tape::verifier::VerifiedProgram;
use execution_tape::vm::Trap;

/// Owns boundary observations for one invocation of a shared prepared program.
///
/// Pass [`Self::mask`] to the ordinary VM run, then consume the recorder with [`Self::finish`].
/// Capture failure stops value/graph retention; guest execution and live host effects continue.
/// The final VM trap is still retained when its payload budget permits. Create a new
/// recorder for each invocation. The recorder never changes the VM, host or supplied values.
pub struct Recorder {
    history: Option<alloc::boxed::Box<History>>,
    program: Arc<VerifiedProgram>,
    builder: Builder,
    entry: Option<Entry>,
    calls: Vec<Call>,
    frames: Vec<Frame>,
    top: Option<FrameId>,
    outcome_frame: Option<FrameId>,
    candidate: Option<(TraceLocation, Range<usize>)>,
    end: Option<End>,
    fault: Option<CaptureFault>,
    fault_location: Option<TraceLocation>,
    started: bool,
}

impl Recorder {
    /// Creates a recorder sharing `program`; execution must use this same prepared instance.
    pub fn new(program: Arc<VerifiedProgram>, limits: RecordLimits) -> Self {
        Self {
            history: None,
            program,
            builder: Builder::new(limits),
            entry: None,
            calls: Vec::new(),
            frames: Vec::new(),
            top: None,
            outcome_frame: None,
            candidate: None,
            end: None,
            fault: None,
            fault_location: None,
            started: false,
        }
    }

    /// Enables bounded instruction history. Configure before starting the invocation.
    ///
    /// Requests instruction callbacks in [`Self::mask`]. Reaching `max_steps` preserves a
    /// labelled prefix; boundary recording and guest execution continue normally.
    #[must_use]
    pub fn with_instructions(mut self, config: InstructionCapture) -> Self {
        self.history = Some(alloc::boxed::Box::new(History::new(config)));
        if self.started {
            self.history.as_mut().expect("installed history").status =
                InstructionStatus::Incomplete;
        }
        self
    }

    /// Freezes retained observations and discards construction lookup indexes.
    ///
    /// Inspect [`Recording::status`] before relying on the record; replay enforces it as well.
    pub fn finish(mut self) -> Recording {
        if let Some(history) = &mut self.history {
            history.finish();
            if self.fault.is_some() {
                history.status = InstructionStatus::Incomplete;
            }
            if history.status == InstructionStatus::Complete
                && matches!(self.end, Some(End::Returned))
                && !history.steps.last().is_some_and(|step| {
                    step.outcome == InstructionOutcome::Completed
                        && step.next.is_none()
                        && self
                            .candidate
                            .as_ref()
                            .is_some_and(|(location, _)| *location == step.location)
                })
            {
                history.status = InstructionStatus::Incomplete;
            }
        }
        if self.entry.is_none() {
            self.fail(CaptureFault::MissingEntry);
        }
        if self.end.is_none() {
            self.fail(CaptureFault::MissingCompletion);
        }
        let (arena, initial) = self.builder.finish();
        Recording {
            history: self.history,
            program: self.program,
            arena,
            initial,
            entry: self.entry,
            calls: self.calls,
            frames: self.frames,
            outcome_frame: if self.fault.is_none() {
                self.outcome_frame
            } else {
                None
            },
            candidate: self.candidate,
            end: self.end,
            fault: self.fault,
            fault_location: self.fault_location,
        }
    }

    fn fail(&mut self, error: CaptureFault) {
        self.fail_at(error, None);
    }
    fn fail_at(&mut self, error: CaptureFault, location: Option<TraceLocation>) {
        if self.fault.is_none() {
            self.fault = Some(error);
            self.top = None;
            self.outcome_frame = None;
            self.fault_location = location;
        }
    }
    fn check(&mut self, program: &Program) -> bool {
        if !core::ptr::eq(program, self.program.program()) {
            self.fail(CaptureFault::ProgramMismatch);
        }
        self.fault.is_none()
    }

    fn check_location(&self, location: TraceLocation) -> Result<(), CaptureFault> {
        if self
            .top
            .is_some_and(|index| self.frames[index.index()].func != location.func)
        {
            Err(CaptureFault::InvalidSequence)
        } else {
            Ok(())
        }
    }

    fn capture(&mut self, event: ValueEvent<'_>) -> Result<(), CaptureFault> {
        match event {
            ValueEvent::Entry {
                entry,
                args,
                aggregates,
                limits,
            } => {
                if self.entry.is_some() || self.end.is_some() {
                    return Err(CaptureFault::MultipleRuns);
                }
                self.builder.initial_count = aggregates.len_u32();
                let args = self
                    .builder
                    .roots(args.iter().map(ValueRef::from_value), aggregates)?;
                self.entry = Some(Entry {
                    func: entry,
                    args,
                    limits: limits.clone(),
                });
            }
            ValueEvent::HostCall {
                location,
                host_sig,
                args,
                outcome,
                aggregates,
            } => {
                if self.entry.is_none() || self.end.is_some() || self.candidate.is_some() {
                    return Err(CaptureFault::InvalidSequence);
                }
                if self.calls.len() >= self.builder.limits.host_calls {
                    return Err(CaptureFault::Budget {
                        resource: CaptureResource::HostCalls,
                        limit: self.builder.limits.host_calls,
                    });
                }
                let args = self.builder.roots(args.iter().copied(), aggregates)?;
                let outcome = match outcome {
                    HostCallOutcome::Returned { values, extra_fuel } => CallOutcome::Returned {
                        values: self
                            .builder
                            .roots(values.iter().map(ValueRef::from_value), aggregates)?,
                        fuel: extra_fuel,
                    },
                    HostCallOutcome::Failed(error) => CallOutcome::Failed(error.clone()),
                };
                self.check_location(location)?;
                self.calls.push(Call {
                    frame: self.top,
                    location,
                    import: host_sig,
                    args,
                    outcome,
                });
            }
            ValueEvent::ReturnCandidate {
                location,
                values,
                aggregates,
            } => {
                if self.entry.is_none() || self.end.is_some() || self.candidate.is_some() {
                    return Err(CaptureFault::InvalidSequence);
                }
                let values = self
                    .builder
                    .roots(values.iter().map(ValueRef::from_value), aggregates)?;
                self.check_location(location)?;
                self.outcome_frame = self.top;
                self.candidate = Some((location, values));
            }
        }
        Ok(())
    }
}

impl core::fmt::Debug for Recorder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Recorder")
            .field("fault", &self.fault)
            .field("host_calls", &self.calls.len())
            .finish_non_exhaustive()
    }
}

impl TraceSink for Recorder {
    fn run_mask(&mut self, program: &Program, mask: TraceMask) {
        if !self.check(program) {
            return;
        }
        if let Some(history) = &mut self.history {
            history.mask_seen = true;
            if !mask.contains(TraceMask::INSTR)
                || (!history.config.watches.is_empty() && !mask.contains(TraceMask::REGISTERS))
            {
                history.status = InstructionStatus::Incomplete;
            }
        }
    }
    fn wants_registers(&self, location: TraceLocation, _: RegisterPhase) -> bool {
        self.fault.is_none()
            && self.history.as_ref().is_some_and(|h| {
                h.status == InstructionStatus::Complete
                    && h.pending
                        .is_some_and(|index| h.steps[index].location == location)
                    && h.watch(location).is_some()
            })
    }

    fn registers(&mut self, program: &Program, event: RegisterEvent<'_>) {
        if !self.check(program) || !self.wants_registers(event.location, event.phase) {
            return;
        }
        let history = self.history.as_mut().expect("enabled history");
        let index = history.pending.expect("pending attempt");
        let watch = history
            .config
            .watches
            .binary_search_by_key(&(event.location.func.0, event.location.pc), |watch| {
                (watch.location.func.0, watch.location.pc)
            })
            .expect("requested watch");
        let wanted = &history.config.watches[watch].registers;
        let start = history.registers.len();
        for (reg, value) in event
            .values
            .iter()
            .filter(|(reg, _)| wanted.binary_search(reg).is_ok())
        {
            let Some(value) = value else {
                self.fail_at(CaptureFault::InvalidSequence, Some(event.location));
                return;
            };
            match self
                .builder
                .roots(core::iter::once(value), event.aggregates)
            {
                Ok(range) => history.registers.push((reg, range.start)),
                Err(error) => {
                    self.fail_at(error, Some(event.location));
                    return;
                }
            }
        }
        let range = start..history.registers.len();
        match event.phase {
            RegisterPhase::Inputs => {
                history.steps[index].inputs = range;
                history.steps[index].inputs_seen = true;
            }
            RegisterPhase::Outputs => history.steps[index].outputs = range,
        }
    }

    fn mask(&self) -> TraceMask {
        TraceMask::RUN
            | TraceMask::VALUES
            | TraceMask::CALL
            | if self.history.is_some() {
                TraceMask::INSTR
            } else {
                TraceMask::NONE
            }
            | if self
                .history
                .as_ref()
                .is_some_and(|h| !h.config.watches.is_empty())
            {
                TraceMask::REGISTERS
            } else {
                TraceMask::NONE
            }
    }

    fn run_start(&mut self, program: &Program, _: FuncId, _: usize) {
        if self.started || self.entry.is_some() {
            self.fail(CaptureFault::MultipleRuns);
        }
        self.started = true;
        if let Some(history) = &mut self.history {
            if history.started {
                history.status = InstructionStatus::Incomplete;
            }
            history.started = true;
        }
        self.check(program);
    }

    fn instr(
        &mut self,
        program: &Program,
        func: FuncId,
        pc: u32,
        _: u32,
        _: Option<u64>,
        opcode: u8,
    ) {
        if !self.check(program) {
            return;
        }
        let Some(history) = &mut self.history else {
            return;
        };
        if history.status != InstructionStatus::Complete {
            return;
        }
        if history.pending.is_some() || !history.started || history.finished {
            history.status = InstructionStatus::Incomplete;
            return;
        }
        if history
            .steps
            .last()
            .is_some_and(|step| step.next != Some(TraceLocation { func, pc }))
        {
            history.status = InstructionStatus::Incomplete;
            return;
        }
        if history.steps.len() >= history.config.max_steps {
            history.status = InstructionStatus::Truncated {
                limit: history.config.max_steps,
            };
            return;
        }
        history.pending = Some(history.steps.len());
        history.steps.push(Step {
            inputs_seen: false,
            inputs: history.registers.len()..history.registers.len(),
            outputs: history.registers.len()..history.registers.len(),
            location: TraceLocation { func, pc },
            opcode,
            next: None,
            outcome: InstructionOutcome::Pending,
            frame: self.top,
        });
    }

    fn instr_end(
        &mut self,
        program: &Program,
        location: TraceLocation,
        next: Option<TraceLocation>,
        outcome: TraceOutcome<'_>,
    ) {
        if !self.check(program) {
            return;
        }
        let Some(history) = &mut self.history else {
            return;
        };
        if history.status != InstructionStatus::Complete {
            return;
        }
        let Some(index) = history.pending.take() else {
            history.status = InstructionStatus::Incomplete;
            return;
        };
        let watched = history.watch(location).is_some();
        let step = &mut history.steps[index];
        if !step.inputs_seen && watched {
            history.status = InstructionStatus::Incomplete;
        }
        if step.location != location {
            history.status = InstructionStatus::Incomplete;
            return;
        }
        step.next = next;
        step.outcome = match outcome {
            TraceOutcome::Ok => InstructionOutcome::Completed,
            TraceOutcome::Trap(_) => InstructionOutcome::Trapped,
        };
    }

    fn values(&mut self, program: &Program, event: ValueEvent<'_>) {
        let location = match event {
            ValueEvent::Entry { .. } => None,
            ValueEvent::HostCall { location, .. }
            | ValueEvent::ReturnCandidate { location, .. } => Some(location),
        };
        if self.check(program)
            && let Err(error) = self.capture(event)
        {
            self.fail_at(error, location);
        }
    }

    fn scope_enter(
        &mut self,
        program: &Program,
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
        if !self.check(program) {
            return;
        }
        let location = Some(TraceLocation { func, pc });
        let expected_depth = self
            .top
            .map_or(1, |top| self.frames[top.index()].depth.saturating_add(1));
        let valid_parent = match (self.top, caller) {
            (None, None) => {
                self.frames.is_empty()
                    && self
                        .entry
                        .as_ref()
                        .is_some_and(|entry| entry.func == callee)
            }
            (Some(parent), Some(site)) => self.frames[parent.index()].func == site.func,
            _ => false,
        };
        if self.end.is_some()
            || self.candidate.is_some()
            || u32::try_from(depth).ok() != Some(expected_depth)
            || callee != func
            || !valid_parent
        {
            self.fail_at(CaptureFault::InvalidSequence, location);
        } else if self.frames.len() >= self.builder.limits.call_frames.min(u32::MAX as usize) {
            self.fail_at(
                CaptureFault::Budget {
                    resource: CaptureResource::CallFrames,
                    limit: self.builder.limits.call_frames.min(u32::MAX as usize),
                },
                location,
            );
        } else {
            let top = FrameId::new(self.frames.len()).expect("bounded activation count");
            self.frames.push(Frame {
                func: callee,
                caller,
                parent: self.top,
                depth: expected_depth,
            });
            self.top = Some(top);
        }
    }

    fn scope_exit(
        &mut self,
        program: &Program,
        kind: ScopeKind,
        depth: usize,
        func: FuncId,
        pc: u32,
        _: Option<u64>,
        outcome: TraceOutcome<'_>,
    ) {
        let ScopeKind::CallFrame {
            func: callee,
            caller,
        } = kind
        else {
            return;
        };
        if !self.check(program) {
            return;
        }
        let Some(top) = self.top else {
            self.fail_at(
                CaptureFault::InvalidSequence,
                Some(TraceLocation { func, pc }),
            );
            return;
        };
        let frame = self.frames[top.index()];
        if (frame.func, frame.caller, frame.depth as usize) != (callee, caller, depth) {
            self.fail_at(
                CaptureFault::InvalidSequence,
                Some(TraceLocation { func, pc }),
            );
            return;
        }
        if matches!(outcome, TraceOutcome::Trap(_)) && self.outcome_frame.is_none() {
            // The first exit still has the entire leaf-to-root ancestry. Later exits are unwind.
            self.outcome_frame = Some(top);
        }
        self.top = frame.parent;
    }

    fn run_end(&mut self, program: &Program, outcome: TraceOutcome<'_>) {
        if let Some(history) = &mut self.history {
            history.finished = true;
            if history.pending.is_some() {
                history.status = InstructionStatus::Incomplete;
            }
        }
        if !core::ptr::eq(program, self.program.program()) {
            self.fail(CaptureFault::ProgramMismatch);
            return;
        }
        if self.end.is_some() {
            self.fail(CaptureFault::MultipleRuns);
            return;
        }
        if self.top.is_some() {
            self.fail(CaptureFault::InvalidSequence);
        }
        match outcome {
            TraceOutcome::Ok => {
                if self.entry.is_none() && self.fault.is_none() {
                    self.fail(CaptureFault::MissingEntry);
                } else if self.candidate.is_none() && self.fault.is_none() {
                    self.fail(CaptureFault::InvalidSequence);
                }
                self.end = Some(End::Returned);
            }
            TraceOutcome::Trap(error) => {
                let bytes = match &error.trap {
                    Trap::HostCallFailed { symbol, .. } => symbol.len(),
                    _ => 0,
                };
                if let Err(fault) = self.builder.charge(bytes) {
                    self.fail_at(
                        fault,
                        Some(TraceLocation {
                            func: error.func,
                            pc: error.pc,
                        }),
                    );
                } else {
                    self.end = Some(End::Trapped(error.clone()));
                }
            }
        }
    }
}
