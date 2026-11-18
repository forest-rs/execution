// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use crate::host::ValueRef;
use crate::program::{Pc, Program, SpanId};
use crate::recording::{
    Answer, Call, CallOutcome, CaptureFault, CaptureResource, Caught, End, Entry,
    InstructionCapture, InstructionOutcome, InstructionStatus, Propagation, RecordLimits,
    RecordedRelink, Recording, Resumption, Store, TagRecord, Thrown, UnitRecord, Yielded,
    control::{Action, Command, Returned},
    frames::{ActivationExit, ActivationOutcome, Frame, FrameId},
    instructions::History,
    values::Builder,
};
use crate::trace::{
    HostCallOutcome, RegisterEvent, RegisterPhase, ScopeKind, SuspensionAnswer, TraceLocation,
    TraceMask, TraceOutcome, TraceSink, ValueEvent,
};
use crate::value::{FuncRef, UnitId};
use crate::verifier::VerifiedProgram;
use crate::vm::{StackId, StoreEvent, Trap, Units};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::Range;

/// Owns boundary observations for one computation and the units it runs, across the
/// turns that save and resume it.
///
/// At entry the recorder snapshots the VM's units and tags, sharing their programs; installs
/// while the turn is suspended are recorded where they happen. Replay rebuilds the same
/// units and tags with the same identities.
///
/// Pass [`Self::mask`] to the ordinary VM run, then consume the recorder with [`Self::finish`].
/// Capture failure stops value/graph retention; guest execution and live host effects continue.
/// The final VM trap is still retained when its payload budget permits. Create a new
/// recorder for each computation, and pass it to every turn that runs it. The recorder
/// never changes the VM, host or supplied values.
pub struct Recorder {
    history: Option<alloc::boxed::Box<History>>,
    program: Arc<VerifiedProgram>,
    builder: Builder,
    entry: Option<Entry>,
    calls: Vec<Call>,
    commands: Vec<Command>,
    suspensions: Vec<Yielded>,
    sequence: usize,
    active_commands: Vec<usize>,
    frames: Vec<Frame>,
    top: Option<FrameId>,
    /// An answered yielded suspension's root activation re-enters next.
    reentering: bool,
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
    /// The recorded computation, once it was first saved.
    stack: Option<StackId>,
    /// Where the recorded computation was saved, until it is resumed.
    saved: Option<Saved>,
    resumptions: Vec<Resumption>,
}

/// A recorded computation's state when its turn saved it.
#[derive(Clone, Copy)]
struct Saved {
    sequence: usize,
    fuel: u64,
    /// How many turns ran the computation before the one that saved it.
    turn: u64,
    /// The heap's end, which must not move before the computation is resumed.
    heap_end: u32,
}

impl Recorder {
    /// Creates a recorder for a turn entering a unit of `program`.
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
            suspensions: Vec::new(),
            sequence: 0,
            active_commands: Vec::new(),
            frames: Vec::new(),
            top: None,
            reentering: false,
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
            stack: None,
            saved: None,
            resumptions: Vec::new(),
        }
    }

    /// Enables bounded instruction history. Configure before starting the turn.
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
            resumptions: self.resumptions,
            program: self.program,
            units: self.units,
            store: self.store,
            arena,
            initial,
            entry: self.entry,
            calls: self.calls,
            commands: self.commands,
            suspensions: self.suspensions,
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
    /// `installed` is the observation sequence for those made after entry, during an
    /// turn or while the computation was saved; `None` records the store at entry. A
    /// unit installed after entry is recorded with the links its program declares, which
    /// replay's install gives it too; [`Self::record_relinks`] records any relink since. A
    /// unit recorded at entry keeps its links as they are.
    fn record_store(&mut self, units: Units<'_>, installed: Option<usize>) {
        for &event in &units.log()[self.logged.min(units.log().len())..] {
            match event {
                StoreEvent::Installed(unit) => {
                    if unit.0 as usize != self.units.len() {
                        self.fail(CaptureFault::ForeignUnit);
                        return;
                    }
                    let program = units.program(unit).expect("logged unit");
                    let links = if installed.is_some() {
                        program
                            .program()
                            .links
                            .iter()
                            .map(|link| FuncRef::new(unit, link.target))
                            .collect()
                    } else {
                        units.links(unit).expect("logged unit").into()
                    };
                    self.store.push(Store::Unit(self.units.len()));
                    self.units.push(UnitRecord {
                        unit,
                        program: Arc::clone(program),
                        tags: units.tags(unit).expect("logged unit").into(),
                        links,
                        installed,
                    });
                }
                StoreEvent::DefinedTag(tag) => self.store.push(Store::Tag(TagRecord {
                    tag,
                    name: units.tag_name(tag).expect("logged tag").into(),
                    payload: units.tag_payload(tag).expect("logged tag").into(),
                    results: units.tag_results(tag).expect("logged tag").into(),
                    defined: installed,
                })),
            }
        }
        self.logged = units.log().len();
    }

    /// Records as relinks the links that differ from what the recording has them name, with
    /// the observation sequence before which they changed.
    fn record_relinks(&mut self, units: Units<'_>) {
        let mut expected: Vec<Vec<FuncRef>> =
            self.units.iter().map(|unit| unit.links.to_vec()).collect();
        for store in &self.store {
            if let Store::Relink(relink) = store {
                expected[relink.unit.0 as usize][relink.link.0 as usize] = relink.target;
            }
        }
        for (index, links) in expected.into_iter().enumerate() {
            let unit = self.units[index].unit;
            let Some(current) = units.links(unit) else {
                self.fail(CaptureFault::ForeignUnit);
                return;
            };
            for (link, (&was, &now)) in links.iter().zip(current).enumerate() {
                if was != now {
                    self.store.push(Store::Relink(RecordedRelink {
                        unit,
                        link: crate::program::LinkId(
                            u32::try_from(link).expect("link ids fit u32"),
                        ),
                        target: now,
                        relinked: self.sequence,
                    }));
                }
            }
        }
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

    /// A new raise requires a started turn whose previous exception, if any, was caught.
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
            ValueEvent::Installed { .. }
            | ValueEvent::Relinked { .. }
            | ValueEvent::Collected { .. }
            | ValueEvent::Saved { .. }
            | ValueEvent::Resumed { .. } => {}
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
                resident_frames,
                yields_suspensions,
                work,
                ..
            } => {
                if self.entry.is_some() || self.end.is_some() {
                    return Err(CaptureFault::MultipleRuns);
                }
                self.builder.initial_count = aggregates.handle_end();
                let args = self
                    .builder
                    .roots(args.iter().map(ValueRef::from_value), aggregates)?;
                self.entry = Some(Entry {
                    func: entry,
                    args,
                    limits: limits.clone(),
                    resident_frames,
                    yields_suspensions,
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
            ValueEvent::SuspensionYielded {
                location,
                tag,
                payload,
                aggregates,
                ..
            } => {
                if self.entry.is_none()
                    || self.end.is_some()
                    || self.candidate.is_some()
                    || self
                        .suspensions
                        .last()
                        .is_some_and(|last| last.answer.is_none())
                {
                    return Err(CaptureFault::InvalidSequence);
                }
                if self.suspensions.len() >= self.builder.limits.suspensions {
                    return Err(CaptureFault::Budget {
                        resource: CaptureResource::Suspensions,
                        limit: self.builder.limits.suspensions,
                    });
                }
                self.check_location(location)?;
                let payload = self.builder.roots(payload.iter().copied(), aggregates)?;
                // The answer takes the next observation; installs while it waits come before.
                self.suspensions.push(Yielded {
                    sequence: self.sequence,
                    location,
                    tag,
                    payload,
                    frame: self.top,
                    answer: None,
                });
            }
            ValueEvent::SuspensionAnswered {
                location,
                tag,
                answer,
                aggregates,
            } => {
                let sequence = self.sequence;
                let Some(yielded) = self.suspensions.last_mut().filter(|last| {
                    last.answer.is_none()
                        && last.location == location
                        && last.tag == tag
                        && last.sequence == sequence
                }) else {
                    return Err(CaptureFault::InvalidSequence);
                };
                let (answer, reentering) = match answer {
                    SuspensionAnswer::Resumed { values } => (
                        Answer::Resumed(
                            self.builder
                                .roots(values.iter().map(ValueRef::from_value), aggregates)?,
                        ),
                        true,
                    ),
                    SuspensionAnswer::Threw { tag, payload, .. } => (
                        Answer::Threw {
                            tag,
                            payload: self
                                .builder
                                .roots(payload.iter().map(ValueRef::from_value), aggregates)?,
                        },
                        true,
                    ),
                    SuspensionAnswer::Discarded => (Answer::Discarded, false),
                };
                yielded.answer = Some(answer);
                if let Some(history) = &mut self.history {
                    history.begin_answer();
                }
                // Every suspended activation closed its scope; resuming re-enters the root.
                self.reentering = reentering && self.top.is_none() && !self.frames.is_empty();
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
                Ok(range) => history.registers.push((reg.0, range.start)),
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
        _: Pc,
        _: Option<SpanId>,
        opcode: u16,
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
            ValueEvent::Relinked {
                unit, link, target, ..
            } => {
                if self.entry.is_none() || self.end.is_some() {
                    self.fail(CaptureFault::InvalidSequence);
                }
                // Both units were installed, and so recorded, before the relink.
                if self.check_at(program, unit) && (target.unit.0 as usize) < self.units.len() {
                    self.store.push(Store::Relink(RecordedRelink {
                        unit,
                        link,
                        target,
                        relinked: self.sequence,
                    }));
                } else {
                    self.fail(CaptureFault::ForeignUnit);
                }
                return;
            }
            ValueEvent::Collected { .. } => {
                // Replay rebuilds guest-created aggregates by their order in an append-only
                // heap. A moving collection breaks that arithmetic, so stop retaining evidence
                // rather than record handles replay could not resolve.
                self.fail(CaptureFault::Relocated);
                return;
            }
            ValueEvent::Saved {
                stack,
                work,
                turn,
                aggregates,
            } => {
                if self.entry.is_none() || self.end.is_some() || self.saved.is_some() {
                    self.fail(CaptureFault::InvalidSequence);
                }
                if *self.stack.get_or_insert(stack) != stack {
                    // Another computation, whose entry this recorder did not see.
                    self.fail(CaptureFault::MissingEntry);
                    return;
                }
                self.saved = Some(Saved {
                    sequence: self.sequence,
                    fuel: work.fuel,
                    turn,
                    heap_end: aggregates.handle_end(),
                });
                return;
            }
            ValueEvent::Resumed {
                stack,
                turn,
                limits,
                resident_frames,
                yields_suspensions,
                aggregates,
                units,
            } => {
                let Some(saved) = self.saved.take() else {
                    self.fail(CaptureFault::InvalidSequence);
                    return;
                };
                if self.stack != Some(stack) {
                    // Another computation, whose entry this recorder did not see.
                    self.fail(CaptureFault::MissingEntry);
                    return;
                }
                if turn != saved.turn + 1 {
                    self.fail(CaptureFault::Unrecorded);
                    return;
                }
                if aggregates.handle_end() != saved.heap_end {
                    self.fail(CaptureFault::Interleaved);
                    return;
                }
                if self.fault.is_none() {
                    self.record_store(units, Some(self.sequence));
                    self.record_relinks(units);
                }
                self.resumptions.push(Resumption {
                    sequence: saved.sequence,
                    fuel: saved.fuel,
                    stored: self.store.len(),
                    limits: limits.clone(),
                    resident_frames,
                    yields_suspensions,
                });
                return;
            }
            ValueEvent::HostCall { location, .. }
            | ValueEvent::CallbackEnter { location, .. }
            | ValueEvent::CallbackReturn { location, .. }
            | ValueEvent::ReturnCandidate { location, .. }
            | ValueEvent::Thrown { location, .. }
            | ValueEvent::Rethrown { location, .. }
            | ValueEvent::Caught { location, .. }
            | ValueEvent::SuspensionYielded { location, .. }
            | ValueEvent::SuspensionAnswered { location, .. } => location.unit,
        };
        let location = match event {
            ValueEvent::Entry { .. }
            | ValueEvent::Installed { .. }
            | ValueEvent::Relinked { .. }
            | ValueEvent::Collected { .. }
            | ValueEvent::Saved { .. }
            | ValueEvent::Resumed { .. } => None,
            ValueEvent::HostCall { location, .. }
            | ValueEvent::CallbackEnter { location, .. }
            | ValueEvent::CallbackReturn { location, .. }
            | ValueEvent::ReturnCandidate { location, .. }
            | ValueEvent::Thrown { location, .. }
            | ValueEvent::Rethrown { location, .. }
            | ValueEvent::Caught { location, .. }
            | ValueEvent::SuspensionYielded { location, .. }
            | ValueEvent::SuspensionAnswered { location, .. } => Some(location),
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
        _: Option<SpanId>,
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
                (self.frames.is_empty() || self.reentering)
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
        self.reentering = false;
    }

    fn scope_exit(
        &mut self,
        program: &Program,
        kind: ScopeKind,
        depth: usize,
        location: TraceLocation,
        _: Option<SpanId>,
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

    fn attach(&mut self, _program: &Program, _attachment: crate::trace::Attachment<'_>) {
        // A recording follows one computation from its entry: it rejoins the computation it
        // saw saved, and its open frames are still its own.
        if self.entry.is_none() {
            self.fail(CaptureFault::MissingEntry);
        } else if self.saved.is_none() {
            self.fail(CaptureFault::InvalidSequence);
        }
    }

    fn release(&mut self, _program: &Program) {
        if self.saved.is_none() {
            self.fail(CaptureFault::InvalidSequence);
        }
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
            TraceOutcome::Caught { .. } | TraceOutcome::Suspended => {
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
