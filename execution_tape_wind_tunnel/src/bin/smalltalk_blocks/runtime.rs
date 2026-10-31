// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Language-owned lexical homes and cooperative ensure scopes over one VM invocation.

use super::program::{CELL, Fixture, HOME, Result};
use execution_tape::{
    callable::Callable,
    host::{Host, HostContext, HostError, SigHash, ValueRef},
    recording::{InstructionCapture, RecordLimits, Recorder, Recording},
    trace::{TraceLocation, TraceSink},
    value::{Obj, ObjHandle, Value},
    vm::{ActivationId, Invocation, InvocationOptions, Limits, RunStatus, Vm},
};
use std::{
    cell::RefCell,
    rc::Rc,
    sync::atomic::{AtomicU64, Ordering},
};

fn cell() -> Obj {
    Obj {
        host_type: CELL,
        handle: ObjHandle(1),
    }
}
static NEXT_HOME: AtomicU64 = AtomicU64::new(1);

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
    token: Obj,
    site: Site,
    name: String,
}
struct Protection {
    body: ActivationId,
    depth: usize,
    // Borrowed guest handle: the original Block.ensure request remains pending through cleanup.
    cleanup: Value,
}
struct Transfer {
    home: Home,
    value: i64,
    depth: usize,
}
enum After {
    Body(ActivationId),
    Cleanup {
        body: ActivationId,
        normal_results: Option<Vec<Value>>,
    },
}
struct Waiting {
    site: Site,
    after: After,
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
    EnsureEntered { body: u64 },
    CleanupStarted { body: u64 },
    CleanupFinished { body: u64 },
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
    homes: Vec<Home>,
    protections: Vec<Protection>,
    waiting: Vec<Waiting>,
    transfer: Option<Transfer>,
    cannot_return: Option<CannotReturn>,
    native_wait: bool,
    events: Vec<Event>,
}
impl Runtime {
    pub(super) fn new(fixture: &Fixture, limits: Limits) -> Result<Self> {
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
            homes: vec![],
            protections: vec![],
            waiting: vec![],
            transfer: None,
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
    fn guest(&mut self, site: Site, block: Value, after: After) -> Result<()> {
        self.run
            .pending_host_call()
            .ok_or("no pending request for block activation")?
            .call_guest(block, vec![])?;
        self.waiting.push(Waiting { site, after });
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
            self.protections.clear();
            self.waiting.clear();
            self.transfer = None;
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
        let mut request = self
            .run
            .pending_host_call()
            .ok_or("missing admitted host request")?;
        if let Some(values) = request.take_callback_results() {
            drop(request);
            let index = self
                .waiting
                .iter()
                .rposition(|waiting| {
                    waiting.site.activation == site.activation
                        && waiting.site.location == site.location
                })
                .ok_or("block returned without its language continuation")?;
            match self.waiting.remove(index).after {
                After::Body(body) => {
                    let protection = self
                        .protections
                        .pop()
                        .ok_or("protected body left without an ensure scope")?;
                    if protection.body != body {
                        return Err("ensure scopes left out of order".into());
                    }
                    self.event(
                        site.location,
                        Policy::CleanupStarted {
                            body: protection.body.ordinal(),
                        },
                    );
                    self.guest(
                        site,
                        protection.cleanup,
                        After::Cleanup {
                            body: protection.body,
                            normal_results: Some(values),
                        },
                    )?;
                }
                After::Cleanup {
                    body,
                    normal_results,
                } => {
                    if !values.is_empty() {
                        return Err("cleanup must return no values".into());
                    }
                    self.event(
                        site.location,
                        Policy::CleanupFinished {
                            body: body.ordinal(),
                        },
                    );
                    if let Some(values) = normal_results {
                        self.complete(values)?;
                    } else {
                        self.unwind(site)?;
                    }
                }
            }
            return Ok(Status::Running);
        }
        let symbol = request.symbol().to_string();
        let args: Vec<_> = request.arguments().map(|value| value.to_value()).collect();
        drop(request);
        match symbol.as_str() {
            "Block.home" => {
                if self.homes.len() >= 64 {
                    return Err("example lexical-home budget exhausted".into());
                }
                #[allow(deprecated, reason = "fetch_update supports the workspace MSRV")]
                let id = NEXT_HOME
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
                    .map_err(|_| "lexical home identity space exhausted")?;
                let token = Obj {
                    host_type: HOME,
                    handle: ObjHandle(id),
                };
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
                self.homes.push(Home { token, site, name });
                self.complete(vec![Value::Obj(token)])?;
            }
            "Block.ensure" => {
                let [body, cleanup] = args.as_slice() else {
                    return Err("ensure expects body and cleanup closures".into());
                };
                self.run
                    .pending_host_call()
                    .ok_or("ensure request disappeared")?
                    .call_guest(body.clone(), vec![])?;
                let inspection = self.run.inspect();
                let frame = inspection
                    .frames()
                    .next()
                    .ok_or("protected block did not enter")?;
                let depth = inspection.frames().len();
                let body = frame.activation;
                drop(inspection);
                self.event(
                    site.location,
                    Policy::EnsureEntered {
                        body: body.ordinal(),
                    },
                );
                self.protections.push(Protection {
                    body: body.clone(),
                    depth,
                    cleanup: cleanup.clone(),
                });
                self.waiting.push(Waiting {
                    site,
                    after: After::Body(body),
                });
            }
            "Block.return" => {
                let [Value::Obj(token), Value::I64(value)] = args.as_slice() else {
                    return Err("block return expects home and integer value".into());
                };
                let home = self
                    .homes
                    .iter()
                    .find(|home| home.token == *token)
                    .cloned()
                    .ok_or("foreign lexical home token")?;
                let inspection = self.run.inspect();
                let depth = inspection
                    .frames()
                    .enumerate()
                    .find(|(_, frame)| frame.activation == home.site.activation)
                    .map(|(index, _)| inspection.frames().len() - index);
                drop(inspection);
                if let Some(depth) = depth {
                    self.event(
                        site.location,
                        Policy::ReturnSelected {
                            home: home.site.activation.ordinal(),
                            value: *value,
                        },
                    );
                    self.transfer = Some(Transfer {
                        home,
                        value: *value,
                        depth,
                    });
                    self.unwind(site)?;
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
            "Demo.wait" => {
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
    fn unwind(&mut self, site: Site) -> Result<()> {
        let depth = self
            .transfer
            .as_ref()
            .ok_or("no selected non-local return")?
            .depth;
        // Consume first: cleanup cannot execute twice if control leaves while it runs.
        if let Some(protection) = self
            .protections
            .pop_if(|protection| protection.depth >= depth)
        {
            self.event(
                site.location,
                Policy::CleanupStarted {
                    body: protection.body.ordinal(),
                },
            );
            return self.guest(
                site,
                protection.cleanup,
                After::Cleanup {
                    body: protection.body,
                    normal_results: None,
                },
            );
        }
        let transfer = self.transfer.take().ok_or("non-local return disappeared")?;
        let mut request = self
            .run
            .pending_host_call()
            .ok_or("non-local return requires pending request")?;
        let boxed = match request
            .context()
            .tuple_new(vec![Value::I64(transfer.value)])
        {
            Ok(boxed) => boxed,
            Err(error) => return request.complete(&mut [], Err(error)).map_err(Into::into),
        };
        request.return_from(&transfer.home.site.activation, vec![Value::Agg(boxed)])?;
        let live: Vec<_> = self
            .run
            .inspect()
            .frames()
            .map(|frame| frame.activation)
            .collect();
        self.waiting
            .retain(|waiting| live.contains(&waiting.site.activation));
        Ok(())
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
