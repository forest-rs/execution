// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Language-owned lexical homes over one VM invocation. `ensure:` cleanup is ordinary guest code
//! in protected regions; a non-local return is a guest exception this runtime raises.

use super::program::{CELL, Fixture, NON_LOCAL, Result};
use execution_tape::{
    callable::Callable,
    host::{Host, HostContext, HostError, SigHash, ValueRef},
    program::TagId,
    recording::{InstructionCapture, RecordLimits, Recorder, Recording},
    trace::{TraceLocation, TraceSink},
    value::{Obj, ObjHandle, Value},
    vm::{ActivationId, Invocation, InvocationOptions, Limits, RunStatus, Vm},
};
use std::{cell::RefCell, rc::Rc};

fn cell() -> Obj {
    Obj {
        host_type: CELL,
        handle: ObjHandle(1),
    }
}

#[derive(Debug)]
struct ProtocolFailure(String);
impl std::fmt::Display for ProtocolFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "block protocol: {}", self.0)
    }
}
impl std::error::Error for ProtocolFailure {}

#[derive(Default)]
struct Cells {
    value: Option<i64>,
    cleanups: Vec<i64>,
    after_ensure: bool,
}
impl Host for Cells {
    fn call(
        &mut self,
        symbol: &str,
        _: SigHash,
        args: &[ValueRef<'_>],
        results: &mut [Value],
        context: HostContext<'_, '_>,
    ) -> std::result::Result<u64, HostError> {
        match (symbol, args) {
            ("Object.requireBlock", [ValueRef::Agg(boxed)]) => {
                let Value::Closure(block) =
                    context.tuple_get(*boxed, 0).map_err(HostError::custom)?
                else {
                    return Err(HostError::SignatureMismatch);
                };
                results[0] = Value::Closure(block);
            }
            ("Cell.new", [ValueRef::I64(initial)]) if self.value.is_none() => {
                self.value = Some(*initial);
                results[0] = Value::Obj(cell());
            }
            ("Cell.read", [ValueRef::Obj(receiver)]) if *receiver == cell() => {
                results[0] = Value::I64(self.value.ok_or(HostError::Failed)?);
            }
            ("Cell.write", [ValueRef::Obj(receiver), ValueRef::I64(value)])
                if *receiver == cell() =>
            {
                self.value = Some(*value);
                results[0] = Value::I64(*value);
            }
            ("Cell.cleaned", [ValueRef::Obj(receiver), ValueRef::I64(tag)])
                if *receiver == cell() =>
            {
                self.cleanups.push(*tag)
            }
            ("Demo.afterEnsure", []) => self.after_ensure = true,
            _ => return Err(HostError::UnknownSymbol),
        }
        Ok(0)
    }
}

#[derive(Clone, Debug)]
struct Site {
    activation: ActivationId,
    location: TraceLocation,
}
#[derive(Clone, Debug)]
struct Home {
    id: i64,
    site: Site,
    name: String,
}

/// A language diagnostic with the attempted return and its expired lexical home kept distinct.
/// The driver's explicit fallback policy is an example choice, not Pharo exception resumability.
#[derive(Clone, Debug)]
pub(super) struct CannotReturn {
    pub(super) block: TraceLocation,
    pub(super) home: TraceLocation,
    pub(super) home_activation: u64,
    pub(super) home_name: String,
    pub(super) attempted_value: i64,
}
impl std::fmt::Display for CannotReturn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "BlockCannotReturn: lexical home {} (activation #{}) has returned; block cannot return {}",
            self.home_name, self.home_activation, self.attempted_value
        )
    }
}
#[derive(Clone, Debug)]
pub(super) enum Policy {
    HomeCaptured { activation: u64 },
    ReturnSelected { home: u64, value: i64 },
    CannotReturn(CannotReturn),
    ReturnFallback { value: i64 },
}
#[derive(Clone, Debug)]
pub(super) struct Event {
    pub(super) location: TraceLocation,
    pub(super) policy: Policy,
}
#[derive(Clone, Debug)]
pub(super) enum Status {
    Running,
    Waiting,
    CannotReturn(CannotReturn),
    Complete,
}

pub(super) struct Session {
    pub(super) recording: Recording,
    pub(super) events: Vec<Event>,
    pub(super) cleanups: Vec<i64>,
    pub(super) after_ensure: bool,
}
pub(super) struct Runtime {
    run: Invocation<'static, Cells>,
    recorder: Rc<RefCell<Recorder>>,
    non_local: TagId,
    homes: Vec<Home>,
    cannot_return: Option<CannotReturn>,
    native_wait: bool,
    events: Vec<Event>,
}
impl Runtime {
    pub(super) fn new(fixture: &Fixture, limits: Limits) -> Result<Self> {
        let code = fixture.program.program();
        let non_local = (0..code.tags.len())
            .map(|index| TagId(u32::try_from(index).expect("bounded tag table")))
            .find(|&tag| code.tag_name(tag) == Some(NON_LOCAL))
            .ok_or("story declares no non-local return tag")?;
        let recorder = Rc::new(RefCell::new(
            Recorder::new(fixture.program.clone(), RecordLimits::default())
                .with_instructions(InstructionCapture::default()),
        ));
        let callable = Callable::function(fixture.program.clone(), fixture.entry)?;
        let run = callable.start_owned(
            Vm::new(Cells::default(), limits),
            vec![],
            InvocationOptions::default()
                .yield_host_calls()
                .trace_owned(recorder.mask(), recorder.clone()),
        )?;
        Ok(Self {
            run,
            recorder,
            non_local,
            homes: vec![],
            cannot_return: None,
            native_wait: false,
            events: vec![],
        })
    }
    fn site(&self) -> Result<Site> {
        let frame = self
            .run
            .inspect()
            .frames()
            .next()
            .ok_or("no active guest frame")?;
        Ok(Site {
            activation: frame.activation,
            location: frame.location,
        })
    }
    fn event(&mut self, location: TraceLocation, policy: Policy) {
        self.events.push(Event { location, policy });
    }
    fn complete(&mut self, mut values: Vec<Value>) -> Result<()> {
        self.run
            .pending_host_call()
            .ok_or("no pending request to complete")?
            .complete(&mut values, Ok(0))?;
        Ok(())
    }
    pub(super) fn poll(&mut self, quantum: u64) -> Result<Status> {
        if let Some(error) = &self.cannot_return {
            return Ok(Status::CannotReturn(error.clone()));
        }
        if self.native_wait {
            return Ok(Status::Waiting);
        }
        let result = self.step(quantum);
        if let Err(error) = &result {
            // Hard failures retire native bookkeeping without executing guest cleanup.
            self.cannot_return = None;
            self.native_wait = false;
            if let Some(request) = self.run.pending_host_call() {
                let error = HostError::custom(ProtocolFailure(error.to_string()));
                let _ = request.complete(&mut [], Err(error));
            }
        }
        result
    }
    fn step(&mut self, quantum: u64) -> Result<Status> {
        match self.run.resume(quantum)? {
            RunStatus::Suspended => return Ok(Status::Running),
            RunStatus::Complete => return Ok(Status::Complete),
            RunStatus::AwaitingHost => {}
        }
        let site = self.site()?;
        let request = self
            .run
            .pending_host_call()
            .ok_or("missing admitted host request")?;
        let symbol = request.symbol().to_string();
        let args: Vec<_> = request.arguments().map(|value| value.to_value()).collect();
        drop(request);
        match (symbol.as_str(), args.as_slice()) {
            ("Block.home", []) => {
                if self.homes.len() >= 64 {
                    return Err("example lexical-home budget exhausted".into());
                }
                let id = i64::try_from(self.homes.len())? + 1;
                let name = self
                    .run
                    .program()
                    .program()
                    .function_name(site.location.func.0)
                    .unwrap_or("unnamed")
                    .to_string();
                self.event(
                    site.location,
                    Policy::HomeCaptured {
                        activation: site.activation.ordinal(),
                    },
                );
                self.homes.push(Home { id, site, name });
                self.complete(vec![Value::I64(id)])?;
            }
            ("Block.return", [Value::I64(id), Value::I64(value)]) => {
                let home = self
                    .homes
                    .iter()
                    .find(|home| home.id == *id)
                    .cloned()
                    .ok_or("foreign lexical home")?;
                let live = self
                    .run
                    .inspect()
                    .frames()
                    .any(|frame| frame.activation == home.site.activation);
                if live {
                    self.event(
                        site.location,
                        Policy::ReturnSelected {
                            home: home.site.activation.ordinal(),
                            value: *value,
                        },
                    );
                    // The home method's region answers the boxed value; cleanup runs on the way.
                    let mut request = self
                        .run
                        .pending_host_call()
                        .ok_or("non-local return requires a pending request")?;
                    let boxed = match request.context().tuple_new(vec![Value::I64(*value)]) {
                        Ok(boxed) => boxed,
                        Err(error) => {
                            return request
                                .complete(&mut [], Err(error))
                                .map_err(Into::into)
                                .map(|()| Status::Running);
                        }
                    };
                    request.throw(self.non_local, vec![Value::I64(home.id), Value::Agg(boxed)])?;
                } else {
                    let error = CannotReturn {
                        block: site.location,
                        home: home.site.location,
                        home_activation: home.site.activation.ordinal(),
                        home_name: home.name,
                        attempted_value: *value,
                    };
                    self.event(site.location, Policy::CannotReturn(error.clone()));
                    self.cannot_return = Some(error.clone());
                    return Ok(Status::CannotReturn(error));
                }
            }
            ("Demo.wait", []) => {
                self.native_wait = true;
                return Ok(Status::Waiting);
            }
            _ => self
                .run
                .pending_host_call()
                .ok_or("host request disappeared")?
                .dispatch()?,
        }
        Ok(Status::Running)
    }
    pub(super) fn finish_native_wait(&mut self) -> Result<()> {
        if !self.native_wait {
            return Err("not waiting for native cleanup work".into());
        }
        self.native_wait = false;
        self.complete(vec![])
    }
    pub(super) fn supply_return_fallback(&mut self, value: i64) -> Result<()> {
        let error = self
            .cannot_return
            .take()
            .ok_or("no BlockCannotReturn condition is pending")?;
        self.event(error.block, Policy::ReturnFallback { value });
        self.complete(vec![Value::I64(value)])
    }
    pub(super) fn paused_frames(&self) -> Vec<execution_tape::vm::PausedFrame> {
        self.run.inspect().frames().collect()
    }
    pub(super) fn outputs(&self) -> Result<[i64; 4]> {
        let values = self
            .run
            .result()
            .ok_or("invocation is still active")?
            .map_err(Clone::clone)?;
        decode_outputs(values, self.run.aggregates())
    }
    pub(super) fn into_session(self) -> Session {
        let Self {
            run,
            recorder,
            events,
            ..
        } = self;
        let cleanups = run.host().cleanups.clone();
        let after_ensure = run.host().after_ensure;
        drop(run);
        let recording = Rc::try_unwrap(recorder)
            .expect("invocation observer retired")
            .into_inner()
            .finish();
        Session {
            recording,
            events,
            cleanups,
            after_ensure,
        }
    }
}

pub(super) fn decode_outputs(
    values: &[Value],
    heap: &execution_tape::aggregates::AggHeap,
) -> Result<[i64; 4]> {
    let [Value::Agg(results)] = values else {
        return Err("story result must be an aggregate".into());
    };
    let mut outputs = [0; 4];
    for (index, output) in outputs.iter_mut().enumerate() {
        let Value::I64(value) = heap.tuple_get(*results, index)? else {
            return Err("story result member must be an integer".into());
        };
        *output = value;
    }
    Ok(outputs)
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
