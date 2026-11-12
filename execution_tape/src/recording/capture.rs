// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use crate::host::ValueRef;
use crate::program::Program;
use crate::recording::{
    Call, CallOutcome, CaptureFault, CaptureResource, Caught, End, Entry, InstructionCapture,
    InstructionOutcome, InstructionStatus, Propagation, RecordLimits, Recording, Store, TagRecord,
    Thrown, UnitRecord,
    control::{Action, Command, Returned},
    frames::{ActivationExit, ActivationOutcome, Frame, FrameId},
    instructions::History,
    values::Builder,
};
use crate::trace::{
    HostCallOutcome, RegisterEvent, RegisterPhase, ScopeKind, TraceLocation, TraceMask,
    TraceOutcome, TraceSink, ValueEvent,
};
use crate::value::{FuncRef, UnitId};
use crate::verifier::VerifiedProgram;
use crate::vm::{StoreEvent, Trap, Units};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::Range;

/// Owns boundary observations for one invocation and the units it runs.
///
/// At entry the recorder snapshots the VM's units and tags, sharing their programs; installs
/// while the invocation is suspended are recorded where they happen. Replay rebuilds the same
/// units and tags with the same identities.
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
    commands: Vec<Command>,
    sequence: usize,
    active_commands: Vec<usize>,
    frames: Vec<Frame>,
    top: Option<FrameId>,
    outcome_frame: Option<FrameId>,
    candidate: Option<(TraceLocation, Range<usize>)>,
    thrown: Vec<Thrown>,
    propagations: Vec<Propagation>,
    end: Option<End>,
    fault: Option<CaptureFault>,
    fault_location: Option<TraceLocation>,
    started: bool,
    /// The recorded VM's units, indexed by [`UnitId`].
    units: Vec<UnitRecord>,
    store: Vec<Store>,
    /// Entries of the VM's store log already recorded.
    logged: usize,
}

impl Recorder {
    /// Creates a recorder for an invocation entering a unit of `program`.
    ///
    /// The entry unit must run this same prepared instance; other units may run any program.
    pub fn new(program: Arc<VerifiedProgram>, limits: RecordLimits) -> Self {
        Self {
            history: None,
            program,
            builder: Builder::new(limits),
            entry: None,
            calls: Vec::new(),
            commands: Vec::new(),
            sequence: 0,
            active_commands: Vec::new(),
            frames: Vec::new(),
            top: None,
            outcome_frame: None,
            candidate: None,
            thrown: Vec::new(),
            propagations: Vec::new(),
            end: None,
            fault: None,
            fault_location: None,
            started: false,
            units: Vec::new(),
            store: Vec::new(),
            logged: 0,
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
            units: self.units,
            store: self.store,
            arena,
            initial,
            entry: self.entry,
            calls: self.calls,
            commands: self.commands,
            frames: self.frames,
            outcome_frame: if self.fault.is_none() {
                self.outcome_frame
            } else {
                None
            },
            candidate: self.candidate,
            thrown: self.thrown,
            propagations: self.propagations,
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
    /// Checks that `program` is the entry program, before the unit table is known.
    fn check(&mut self, program: &Program) -> bool {
        if !core::ptr::eq(program, self.program.program()) {
            self.fail(CaptureFault::ProgramMismatch);
        }
        self.fault.is_none()
    }

    /// Checks that an event's `program` is the program of the recorded `unit`.
    fn check_at(&mut self, program: &Program, unit: UnitId) -> bool {
        match self.units.get(unit.0 as usize) {
            Some(record) if core::ptr::eq(program, record.program.program()) => {}
            Some(_) => self.fail(CaptureFault::ProgramMismatch),
            None => self.fail(CaptureFault::ForeignUnit),
        }
        self.fault.is_none()
    }

    /// Checks that a location names a unit whose installation was recorded.
    fn known(&mut self, location: TraceLocation) -> TraceLocation {
        if location.unit.0 as usize >= self.units.len() {
            self.fail(CaptureFault::ForeignUnit);
        }
        location
    }

    /// Records the VM's installs and tag definitions not yet recorded.
    ///
    /// `installed` is the observation sequence for installs during the invocation.
    fn record_store(&mut self, units: Units<'_>, installed: Option<usize>) {
        for &event in &units.log()[self.logged.min(units.log().len())..] {
            match event {
                StoreEvent::Installed(unit) => {
                    if unit.0 as usize != self.units.len() {
                        self.fail(CaptureFault::ForeignUnit);
                        return;
                    }
                    self.store.push(Store::Unit(self.units.len()));
                    self.units.push(UnitRecord {
                        unit,
                        program: Arc::clone(units.program(unit).expect("logged unit")),
                        tags: units.tags(unit).expect("logged unit").into(),
                        installed,
                    });
                }
                StoreEvent::DefinedTag(tag) => self.store.push(Store::Tag(TagRecord {
                    tag,
                    name: units.tag_name(tag).expect("logged tag").into(),
                    payload: units.tag_payload(tag).expect("logged tag").into(),
                })),
            }
        }
        self.logged = units.log().len();
    }

    fn check_location(&self, location: TraceLocation) -> Result<(), CaptureFault> {
        if self.top.is_some_and(|index| {
            let func = self.frames[index.index()].func;
            (func.unit, func.func) != (location.unit, location.func)
        }) {
            Err(CaptureFault::InvalidSequence)
        } else {
            Ok(())
        }
    }

    /// A new raise requires a started invocation whose previous exception, if any, was caught.
    fn check_raise(&self) -> Result<(), CaptureFault> {
        if self.entry.is_none()
            || self.end.is_some()
            || self.candidate.is_some()
            || self
                .propagations
                .last()
                .is_some_and(|last| last.caught.is_none())
        {
            return Err(CaptureFault::InvalidSequence);
        }
        Ok(())
    }

    fn capture(&mut self, event: ValueEvent<'_>) -> Result<(), CaptureFault> {
        match event {
            // The unit table is recorded before capture; see `values`.
            ValueEvent::Installed { .. } => {}
            ValueEvent::CallbackEnter {
                location,
                callee,
                args,
                work,
                created,
                aggregates,
            } => {
                if self.entry.is_none() || self.end.is_some() || self.candidate.is_some() {
                    return Err(CaptureFault::InvalidSequence);
                }
                if self.commands.len() >= self.builder.limits.control_commands {
                    return Err(CaptureFault::Budget {
                        resource: CaptureResource::ControlCommands,
                        limit: self.builder.limits.control_commands,
                    });
                }
                self.check_location(location)?;
                self.builder.roots(
                    created
                        .clone()
                        .map(|handle| ValueRef::Agg(crate::value::AggHandle(handle))),
                    aggregates,
                )?;
                let constructed = self.builder.constructed(created.clone())?;
                let callee = self
                    .builder
                    .roots(core::iter::once(ValueRef::from_value(callee)), aggregates)?
                    .start;
                let args = self
                    .builder
                    .roots(args.iter().map(ValueRef::from_value), aggregates)?;
                self.active_commands.push(self.commands.len());
                self.commands.push(Command {
                    sequence: self.sequence,
                    location,
                    frame: self.top,
                    action: Action::CallGuest {
                        callee,
                        args,
                        work,
                        constructed,
                        returned: None,
                    },
                });
                self.sequence += 1;
                if let Some(history) = &mut self.history {
                    history.begin_callback(location);
                }
            }
            ValueEvent::CallbackReturn {
                location,
                caller,
                values,
                aggregates,
            } => {
                let index = self
                    .active_commands
                    .pop()
                    .ok_or(CaptureFault::InvalidSequence)?;
                if self.commands[index].location != caller {
                    return Err(CaptureFault::InvalidSequence);
                }
                self.check_location(location)?;
                let values = self
                    .builder
                    .roots(values.iter().map(ValueRef::from_value), aggregates)?;
                let Action::CallGuest { returned, .. } = &mut self.commands[index].action;
                if returned.is_some() {
                    return Err(CaptureFault::InvalidSequence);
                }
                *returned = Some(Returned {
                    sequence: self.sequence,
                    location,
                    values,
                    frame: self.top,
                });
                self.sequence += 1;
                if let Some(history) = &mut self.history {
                    history.end_callback();
                }
            }
            ValueEvent::Entry {
                entry,
                args,
                aggregates,
                limits,
                work,
                ..
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
                    work,
                });
            }
            ValueEvent::HostCall {
                location,
                host_sig,
                args,
                outcome,
                aggregates,
                work,
                created,
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
                    HostCallOutcome::Threw { tag, payload, .. } => CallOutcome::Threw {
                        tag,
                        payload: self
                            .builder
                            .roots(payload.iter().map(ValueRef::from_value), aggregates)?,
                    },
                };
                let constructed = self.builder.constructed(created.clone())?;
                self.check_location(location)?;
                self.calls.push(Call {
                    sequence: self.sequence,
                    constructed,
                    work,
                    frame: self.top,
                    location,
                    import: host_sig,
                    args,
                    outcome,
                });
                self.sequence += 1;
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
            ValueEvent::Thrown {
                location,
                tag,
                values,
                aggregates,
                ..
            } => {
                self.check_raise()?;
                let values = self
                    .builder
                    .roots(values.iter().map(ValueRef::from_value), aggregates)?;
                self.check_location(location)?;
                self.propagations.push(Propagation {
                    exception: self.thrown.len(),
                    location,
                    caught: None,
                });
                self.thrown.push(Thrown {
                    location,
                    tag,
                    values,
                });
            }
            ValueEvent::Rethrown {
                location,
                tag,
                exception,
                origin,
                ..
            } => {
                self.check_raise()?;
                // A rethrow links to its original throw; the payload is never recorded twice.
                if !self
                    .thrown
                    .get(exception)
                    .is_some_and(|thrown| thrown.tag == tag && thrown.location == origin)
                {
                    return Err(CaptureFault::InvalidSequence);
                }
                self.check_location(location)?;
                self.propagations.push(Propagation {
                    exception,
                    location,
                    caught: None,
                });
            }
            ValueEvent::Caught {
                location,
                tag,
                handler,
                depth,
                region,
                clause,
                unwound_callbacks,
                ..
            } => {
                let thrown = &self.thrown;
                let Some(last) = self.propagations.last_mut().filter(|last| {
                    last.caught.is_none()
                        && last.location == location
                        && thrown[last.exception].tag == tag
                }) else {
                    return Err(CaptureFault::InvalidSequence);
                };
                last.caught = Some(Caught {
                    handler,
                    depth,
                    region,
                    clause,
                    unwound_callbacks,
                });
                // Abandoned callbacks never return to their commands.
                let active = self
                    .active_commands
                    .len()
                    .checked_sub(unwound_callbacks)
                    .ok_or(CaptureFault::InvalidSequence)?;
                self.active_commands.truncate(active);
                if let Some(history) = &mut self.history {
                    history.begin_catch();
                }
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
        // Watches name locations of the recorder's program, in any unit running it.
        let watched = self
            .units
            .get(location.unit.0 as usize)
            .is_some_and(|unit| core::ptr::eq(unit.program.program(), self.program.program()));
        watched
            && self.fault.is_none()
            && self.history.as_ref().is_some_and(|h| {
                h.status != InstructionStatus::Incomplete
                    && h.current()
                        .is_some_and(|index| h.steps[index].location == location)
                    && h.watch(location).is_some()
            })
    }

    fn registers(&mut self, program: &Program, event: RegisterEvent<'_>) {
        if !self.check_at(program, event.location.unit)
            || !self.wants_registers(event.location, event.phase)
        {
            return;
        }
        let location = event.location;
        let history = self.history.as_mut().expect("enabled history");
        let index = history.current().expect("pending attempt");
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
                self.fail_at(CaptureFault::InvalidSequence, Some(location));
                return;
            };
            match self
                .builder
                .roots(core::iter::once(value), event.aggregates)
            {
                Ok(range) => history.registers.push((reg, range.start)),
                Err(error) => {
                    self.fail_at(error, Some(location));
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

    fn run_start(&mut self, program: &Program, _: FuncRef, _: usize) {
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
        location: TraceLocation,
        _: u32,
        _: Option<u64>,
        opcode: u8,
    ) {
        if !self.check_at(program, location.unit) {
            return;
        }
        let Some(history) = &mut self.history else {
            return;
        };
        history.record_instruction(location, opcode, self.top);
    }

    fn instr_end(
        &mut self,
        program: &Program,
        location: TraceLocation,
        next: Option<TraceLocation>,
        outcome: TraceOutcome<'_>,
    ) {
        if !self.check_at(program, location.unit) {
            return;
        }
        let next = next.map(|next| self.known(next));
        let Some(history) = &mut self.history else {
            return;
        };
        history.complete_instruction(location, next, outcome);
    }

    fn values(&mut self, program: &Program, event: ValueEvent<'_>) {
        let unit = match event {
            ValueEvent::Entry { entry, units, .. } => {
                if self.units.is_empty() && self.fault.is_none() {
                    self.record_store(units, None);
                }
                entry.unit
            }
            ValueEvent::Installed { unit, units } => {
                if self.entry.is_none() || self.end.is_some() {
                    self.fail(CaptureFault::InvalidSequence);
                }
                if self.fault.is_none() {
                    self.record_store(units, Some(self.sequence));
                }
                self.check_at(program, unit);
                return;
            }
            ValueEvent::HostCall { location, .. }
            | ValueEvent::CallbackEnter { location, .. }
            | ValueEvent::CallbackReturn { location, .. }
            | ValueEvent::ReturnCandidate { location, .. }
            | ValueEvent::Thrown { location, .. }
            | ValueEvent::Rethrown { location, .. }
            | ValueEvent::Caught { location, .. } => location.unit,
        };
        let location = match event {
            ValueEvent::Entry { .. } | ValueEvent::Installed { .. } => None,
            ValueEvent::HostCall { location, .. }
            | ValueEvent::CallbackEnter { location, .. }
            | ValueEvent::CallbackReturn { location, .. }
            | ValueEvent::ReturnCandidate { location, .. }
            | ValueEvent::Thrown { location, .. }
            | ValueEvent::Rethrown { location, .. }
            | ValueEvent::Caught { location, .. } => Some(location),
        };
        if self.check_at(program, unit)
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
        location: TraceLocation,
        _: Option<u64>,
    ) {
        let ScopeKind::CallFrame {
            func: callee,
            caller,
        } = kind
        else {
            return;
        };
        if !self.check_at(program, location.unit) {
            return;
        }
        let caller = caller.map(|caller| self.known(caller));
        let func = FuncRef::new(location.unit, location.func);
        let location = Some(location);
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
            (Some(parent), Some(site)) => {
                self.frames[parent.index()].func == FuncRef::new(site.unit, site.func)
            }
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
                entered: self.sequence,
                exit: None,
            });
            self.top = Some(top);
        }
    }

    fn scope_exit(
        &mut self,
        program: &Program,
        kind: ScopeKind,
        depth: usize,
        location: TraceLocation,
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
        if !self.check_at(program, location.unit) {
            return;
        }
        let caller = caller.map(|caller| self.known(caller));
        let Some(top) = self.top else {
            self.fail_at(CaptureFault::InvalidSequence, Some(location));
            return;
        };
        let frame = self.frames[top.index()];
        if (frame.func, frame.caller, frame.depth as usize) != (callee, caller, depth) {
            self.fail_at(CaptureFault::InvalidSequence, Some(location));
            return;
        }
        if matches!(outcome, TraceOutcome::Trap(_)) && self.outcome_frame.is_none() {
            // The first exit still has the entire leaf-to-root ancestry. Later exits are unwind.
            self.outcome_frame = Some(top);
        }
        self.frames[top.index()].exit = Some(ActivationExit {
            location,
            outcome: ActivationOutcome::of(outcome),
            sequence: self.sequence,
        });
        self.top = frame.parent;
    }

    fn run_end(&mut self, program: &Program, outcome: TraceOutcome<'_>) {
        if let Some(history) = &mut self.history {
            history.finished = true;
            if history.has_pending() {
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
            TraceOutcome::Caught { .. } => {
                self.fail(CaptureFault::InvalidSequence);
            }
            TraceOutcome::Cancelled => self.fail(CaptureFault::MissingCompletion),
            TraceOutcome::Ok => {
                if !self.active_commands.is_empty() {
                    self.fail(CaptureFault::InvalidSequence);
                }
                if self.entry.is_none() && self.fault.is_none() {
                    self.fail(CaptureFault::MissingEntry);
                } else if self.candidate.is_none() && self.fault.is_none() {
                    self.fail(CaptureFault::InvalidSequence);
                }
                self.end = Some(End::Returned);
            }
            TraceOutcome::Trap(error) => {
                // An uncaught payload was charged when its `Thrown` event was captured.
                let bytes = match &error.trap {
                    Trap::HostCallFailed { symbol, .. } => symbol.len(),
                    Trap::Uncaught(exception) => exception.name().len(),
                    _ => 0,
                };
                if let Trap::Uncaught(exception) = &error.trap
                    && !self.propagations.last().is_some_and(|last| {
                        last.caught.is_none()
                            && self.thrown[last.exception].tag == exception.tag()
                            && self.thrown[last.exception].location == exception.origin()
                            && last.location.unit == error.unit
                            && last.location.func == error.func
                            && last.location.pc == error.pc
                    })
                {
                    self.fail(CaptureFault::InvalidSequence);
                }
                if let Err(fault) = self.builder.charge(bytes) {
                    self.fail_at(
                        fault,
                        Some(TraceLocation {
                            unit: error.unit,
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
