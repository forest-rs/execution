// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use crate::{
    Call, CallOutcome, CaptureFault, CaptureResource, End, Entry, RecordLimits, Recording,
    values::Builder,
};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::Range;
use execution_tape::host::ValueRef;
use execution_tape::program::Program;
use execution_tape::trace::{
    HostCallOutcome, TraceLocation, TraceMask, TraceOutcome, TraceSink, ValueEvent,
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
    program: Arc<VerifiedProgram>,
    builder: Builder,
    entry: Option<Entry>,
    calls: Vec<Call>,
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
            program,
            builder: Builder::new(limits),
            entry: None,
            calls: Vec::new(),
            candidate: None,
            end: None,
            fault: None,
            fault_location: None,
            started: false,
        }
    }

    /// Freezes retained observations and discards construction lookup indexes.
    ///
    /// Inspect [`Recording::status`] before relying on the record; replay enforces it as well.
    pub fn finish(mut self) -> Recording {
        if self.entry.is_none() {
            self.fail(CaptureFault::MissingEntry);
        }
        if self.end.is_none() {
            self.fail(CaptureFault::MissingCompletion);
        }
        let (arena, initial) = self.builder.finish();
        Recording {
            program: self.program,
            arena,
            initial,
            entry: self.entry,
            calls: self.calls,
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
            self.fault_location = location;
        }
    }
    fn check(&mut self, program: &Program) -> bool {
        if !core::ptr::eq(program, self.program.program()) {
            self.fail(CaptureFault::ProgramMismatch);
        }
        self.fault.is_none()
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
                self.calls.push(Call {
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
    fn mask(&self) -> TraceMask {
        TraceMask::RUN | TraceMask::VALUES
    }

    fn run_start(&mut self, program: &Program, _: FuncId, _: usize) {
        if self.started || self.entry.is_some() {
            self.fail(CaptureFault::MultipleRuns);
        }
        self.started = true;
        self.check(program);
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

    fn run_end(&mut self, program: &Program, outcome: TraceOutcome<'_>) {
        if !core::ptr::eq(program, self.program.program()) {
            self.fail(CaptureFault::ProgramMismatch);
            return;
        }
        if self.end.is_some() {
            self.fail(CaptureFault::MultipleRuns);
            return;
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
