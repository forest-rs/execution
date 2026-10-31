// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A finite language task, not a second interpreter or a general task scheduler.
//!
//! Each VM keeps its own program, heap and activation identities. This runtime connects them
//! with retained callables, scalar language tokens and explicit control decisions.

use super::program::{self, Compiled, Result};
use execution_tape::{
    callable::{Callable, CodeSlot, CodeUpdate},
    host::{Host, HostContext, HostError, SigHash, ValueRef},
    recording::{InstructionCapture, RecordLimits, Recorder, Recording},
    trace::{TraceLocation, TraceSink},
    value::Value,
    vm::{ActivationId, ExecutionWork, Invocation, InvocationOptions, Limits, RunStatus, Trap, Vm},
};
use execution_tape_observatory::{TaskCode, TaskDecision, TaskExplorer, TaskFrame};
use std::{cell::RefCell, error::Error, fmt, rc::Rc, sync::Arc};

// Private identities in this single task. They are not VM handles or inter-task capabilities.
const HOME_TOKEN: u64 = 1;
const BLOCK_TOKEN: u64 = 2;

/// Every host request in this fixture is deliberately handled by the language task.
struct Services;
impl Host for Services {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        _: &[ValueRef<'_>],
        _: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> std::result::Result<u64, HostError> {
        Err(HostError::UnknownSymbol)
    }
}

#[derive(Debug)]
pub(super) enum Failure {
    Protocol(&'static str),
    FuelExhausted,
    CannotReturn,
}
impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Protocol(message) => message,
            Self::FuelExhausted => "the language task exhausted its shared instruction fuel",
            Self::CannotReturn => "the block's exact lexical home has ended",
        })
    }
}
impl Error for Failure {}

#[derive(Clone)]
pub(super) struct Site {
    pub(super) code: Arc<Compiled>,
    pub(super) location: TraceLocation,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Action {
    RetainedBlock,
    HandlerEntered,
    Published(u64),
    Recovery(i64),
    ExitSelected,
    CleanupStarted,
    Cleaned(i64),
    CleanupFinished,
    HomeReturned,
    AfterMethod,
    AfterSend,
}

pub(super) struct Event {
    pub(super) site: Site,
    pub(super) action: Action,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Status {
    Running,
    NeedValue,
    CleanupWaiting,
    Complete,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Termination {
    Returned,
    Trapped,
    Abandoned,
    Cancelled,
}

pub(super) struct Evidence {
    pub(super) label: &'static str,
    pub(super) code: Arc<Compiled>,
    pub(super) termination: Termination,
    pub(super) recording: Recording,
    pub(super) work: ExecutionWork,
}

struct Frame {
    label: &'static str,
    code: Arc<Compiled>,
    call: Invocation<'static, Services>,
    recorder: Rc<RefCell<Recorder>>,
}

impl Frame {
    fn start(
        label: &'static str,
        code: Arc<Compiled>,
        callable: &Callable,
        args: Vec<Value>,
        fuel: u64,
    ) -> Result<Self> {
        if !Arc::ptr_eq(callable.program(), &code.program) {
            return Err(
                Failure::Protocol("callable and source must retain the same code owner").into(),
            );
        }
        let recorder = Rc::new(RefCell::new(
            Recorder::new(Arc::clone(&code.program), RecordLimits::default())
                .with_instructions(InstructionCapture::default()),
        ));
        let mask = recorder.borrow().mask();
        let call = callable.start_owned(
            Vm::new(
                Services,
                Limits {
                    fuel,
                    ..Limits::default()
                },
            ),
            args,
            InvocationOptions::default()
                .yield_host_calls()
                .trace_owned(mask, Rc::clone(&recorder)),
        )?;
        Ok(Self {
            label,
            code,
            call,
            recorder,
        })
    }

    fn site(&mut self) -> Result<Site> {
        let request = self
            .call
            .pending_host_call()
            .ok_or(Failure::Protocol("expected a waiting language service"))?;
        Ok(Site {
            code: Arc::clone(&self.code),
            location: request.location(),
        })
    }

    fn request(&mut self) -> Result<(String, Vec<Value>)> {
        let request = self
            .call
            .pending_host_call()
            .ok_or(Failure::Protocol("expected a waiting language service"))?;
        // These raw values never cross code domains. Retain closures through this frame first;
        // only the scalar language tokens and answers are passed to other invocations.
        Ok((
            request.symbol().into(),
            request.arguments().map(ValueRef::to_value).collect(),
        ))
    }

    fn retain(&mut self, value: &Value) -> Result<Callable> {
        Ok(Callable::retain(
            Arc::clone(&self.code.program),
            self.call.aggregates(),
            ValueRef::from_value(value),
        )?)
    }

    fn reply(&mut self, mut values: Vec<Value>) -> Result<()> {
        self.call
            .pending_host_call()
            .ok_or(Failure::Protocol("service already completed"))?
            .complete(&mut values, Ok(0))?;
        Ok(())
    }

    fn advance(&mut self, quantum: u64, remaining: &mut u64) -> Result<RunStatus> {
        if *remaining == 0 {
            return Err(Failure::FuelExhausted.into());
        }
        let before = self.call.work();
        let status = self.call.resume(quantum.min(*remaining));
        *remaining -= self
            .call
            .work()
            .checked_sub(before)
            .expect("monotonic VM work")
            .fuel;
        Ok(status?)
    }

    fn answer(&self) -> Result<i64> {
        match self.call.result() {
            Some(Ok([Value::I64(value)])) => Ok(*value),
            _ => Err(Failure::Protocol("expected one successful integer answer").into()),
        }
    }

    fn abandon(mut self) -> Evidence {
        self.call
            .pending_host_call()
            .expect("crossed pending language operation")
            .abandon();
        self.retire()
    }

    fn retire(self) -> Evidence {
        let work = self.call.work();
        let termination = match self.call.result() {
            Some(Ok(_)) => Termination::Returned,
            Some(Err(error)) if matches!(error.trap, Trap::Abandoned) => Termination::Abandoned,
            Some(Err(_)) => Termination::Trapped,
            None => Termination::Cancelled,
        };
        let Self {
            label,
            code,
            call,
            recorder,
        } = self;
        drop(call); // Close actual VM scopes before freezing the recorder.
        let recorder = Rc::try_unwrap(recorder)
            .unwrap_or_else(|_| panic!("frame owns its observer"))
            .into_inner();
        Evidence {
            label,
            code,
            termination,
            recording: recorder.finish(),
            work,
        }
    }
}

struct Home {
    activation: ActivationId,
}
struct Transfer {
    target: ActivationId,
    value: i64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Home,
    Method,
    Block,
    Handler,
    Cleanup,
    Complete,
    Cancelled,
}

/// One deliberately small, inspectable composition policy for the accompanying story.
pub(super) struct Task {
    phase: Phase,
    waiting: Option<Status>,
    root: Option<Frame>,
    method: Option<Frame>,
    block: Option<Frame>,
    handler: Option<Frame>,
    cleanup: Option<Frame>,
    home: Option<Home>,
    retained_block: Option<Callable>,
    retained_cleanup: Option<Callable>,
    method_code: Arc<Compiled>,
    replacement: Arc<Compiled>,
    handler_code: Arc<Compiled>,
    methods: CodeSlot,
    transfer: Option<Transfer>,
    body_answer: Option<i64>,
    pub(super) remaining_fuel: u64,
    pub(super) answer: Option<i64>,
    pub(super) events: Vec<Event>,
    pub(super) evidence: Vec<Evidence>,
}

impl Task {
    /// Borrows every actual paused invocation and the source-bound native decisions.
    pub(super) fn explorer(&self) -> Result<TaskExplorer<'_>> {
        fn code<'a>(compiled: &'a Compiled, cache: &mut Vec<TaskCode<'a>>) -> Result<TaskCode<'a>> {
            if let Some(code) = cache
                .iter()
                .find(|code| std::ptr::eq(code.program(), compiled.program.as_ref()))
            {
                return Ok(*code);
            }
            let code = TaskCode::new(&compiled.program).sources(&compiled.sources)?;
            cache.push(code);
            Ok(code)
        }
        let mut cache = Vec::new();
        let mut frames = Vec::new();
        for frame in [
            &self.root,
            &self.method,
            &self.block,
            &self.handler,
            &self.cleanup,
        ]
        .into_iter()
        .flatten()
        {
            frames.push(TaskFrame::new(
                frame.label,
                frame.call.inspect(),
                code(&frame.code, &mut cache)?,
            )?);
        }
        let mut decisions = Vec::new();
        for event in &self.events {
            decisions.push(TaskDecision::new(
                &crate::describe(&event.action),
                code(&event.site.code, &mut cache)?,
                event.site.location,
            )?);
        }
        Ok(TaskExplorer::new(frames, decisions)?)
    }

    pub(super) fn new(non_local: bool, fuel: u64) -> Result<Self> {
        let home = program::home(non_local)?;
        let method_code = program::method(10)?;
        let replacement = program::method(100)?;
        let handler_code = program::handler()?;
        let callable = Callable::function(Arc::clone(&home.program), home.entry)?;
        let root = Frame::start("lexical home", home, &callable, vec![], fuel)?;
        let methods = CodeSlot::new(Callable::function(
            Arc::clone(&method_code.program),
            method_code.entry,
        )?);
        Ok(Self {
            phase: Phase::Home,
            waiting: None,
            root: Some(root),
            method: None,
            block: None,
            handler: None,
            cleanup: None,
            home: None,
            retained_block: None,
            retained_cleanup: None,
            method_code,
            replacement,
            handler_code,
            methods,
            transfer: None,
            body_answer: None,
            remaining_fuel: fuel,
            answer: None,
            events: Vec::new(),
            evidence: Vec::new(),
        })
    }

    pub(super) fn revision(&self) -> u64 {
        self.methods.current().revision()
    }

    /// Counts each live or retired VM once, including owned entry preparation and staging.
    pub(super) fn work(&self) -> Result<ExecutionWork> {
        self.evidence
            .iter()
            .map(|frame| frame.work)
            .chain(
                [
                    &self.root,
                    &self.method,
                    &self.block,
                    &self.handler,
                    &self.cleanup,
                ]
                .into_iter()
                .flatten()
                .map(|frame| frame.call.work()),
            )
            .try_fold(ExecutionWork::default(), |sum, work| {
                sum.checked_add(work)
                    .ok_or_else(|| Failure::Protocol("task work counters overflowed").into())
            })
    }

    pub(super) fn poll(&mut self, quantum: u64) -> Result<Status> {
        let result = self.poll_inner(quantum);
        if result.is_err() {
            self.cancel();
        }
        result
    }

    fn poll_inner(&mut self, quantum: u64) -> Result<Status> {
        if let Some(waiting) = self.waiting {
            return Ok(waiting);
        }
        if self.phase == Phase::Complete {
            return Ok(Status::Complete);
        }
        if self.phase == Phase::Cancelled {
            return Ok(Status::Cancelled);
        }
        let phase = self.phase;
        let frame = match phase {
            Phase::Home => self.root.as_mut(),
            Phase::Method => self.method.as_mut(),
            Phase::Block => self.block.as_mut(),
            Phase::Handler => self.handler.as_mut(),
            Phase::Cleanup => self.cleanup.as_mut(),
            _ => unreachable!(),
        }
        .expect("phase owns its frame");
        match frame.advance(quantum, &mut self.remaining_fuel)? {
            RunStatus::Suspended => return Ok(Status::Running),
            RunStatus::Complete => {
                self.returned(phase)?;
                return Ok(Status::Running);
            }
            RunStatus::AwaitingHost => {}
        }
        let site = frame.site()?;
        let (symbol, args) = frame.request()?;
        match (phase, symbol.as_str(), args.as_slice()) {
            (Phase::Home, "Runtime.home", []) => {
                self.home = Some(Home {
                    activation: frame
                        .call
                        .inspect()
                        .frames()
                        .next()
                        .expect("active lexical home")
                        .activation,
                });
                frame.reply(vec![Value::U64(HOME_TOKEN)])?;
            }
            (Phase::Home, "Runtime.retain", [closure @ Value::Closure(_)]) => {
                self.retained_block = Some(frame.retain(closure)?);
                frame.reply(vec![Value::U64(BLOCK_TOKEN)])?;
                self.events.push(Event {
                    site,
                    action: Action::RetainedBlock,
                });
            }
            (Phase::Home, "Runtime.send", [Value::U64(BLOCK_TOKEN)]) => {
                let current = self.methods.current();
                self.method = Some(Frame::start(
                    "protected method / revision 0",
                    Arc::clone(&self.method_code),
                    current.callable(),
                    vec![Value::U64(BLOCK_TOKEN)],
                    self.remaining_fuel,
                )?);
                self.phase = Phase::Method;
            }
            (
                Phase::Method,
                "Runtime.protect",
                [Value::U64(BLOCK_TOKEN), cleanup @ Value::Closure(_)],
            ) => {
                self.retained_cleanup = Some(frame.retain(cleanup)?);
                let callable = self
                    .retained_block
                    .as_ref()
                    .ok_or(Failure::Protocol("block token has no retained callable"))?;
                self.block = Some(Frame::start(
                    "escaped block",
                    Arc::clone(&self.root.as_ref().expect("live home").code),
                    callable,
                    vec![],
                    self.remaining_fuel,
                )?);
                self.phase = Phase::Block;
            }
            (Phase::Block, "Runtime.signal", []) => {
                let callable = Callable::function(
                    Arc::clone(&self.handler_code.program),
                    self.handler_code.entry,
                )?;
                self.handler = Some(Frame::start(
                    "NeedValue handler",
                    Arc::clone(&self.handler_code),
                    &callable,
                    vec![],
                    self.remaining_fuel,
                )?);
                self.events.push(Event {
                    site,
                    action: Action::HandlerEntered,
                });
                self.phase = Phase::Handler;
            }
            (Phase::Handler, "Runtime.chooseRecovery", []) => {
                self.waiting = Some(Status::NeedValue);
                return Ok(Status::NeedValue);
            }
            (Phase::Block, "Runtime.return", [Value::U64(home), Value::I64(value)]) => {
                let target = self.home_target(*home)?;
                self.transfer = Some(Transfer {
                    target,
                    value: *value,
                });
                self.events.push(Event {
                    site,
                    action: Action::ExitSelected,
                });
                self.begin_cleanup()?;
            }
            (Phase::Cleanup, "Runtime.waitForCleanup", []) => {
                self.waiting = Some(Status::CleanupWaiting);
                return Ok(Status::CleanupWaiting);
            }
            (Phase::Cleanup, "Runtime.cleaned", [Value::I64(marker)]) => {
                frame.reply(vec![])?;
                self.events.push(Event {
                    site,
                    action: Action::Cleaned(*marker),
                });
            }
            (Phase::Method, "Runtime.afterMethod", []) => {
                frame.reply(vec![])?;
                self.events.push(Event {
                    site,
                    action: Action::AfterMethod,
                });
            }
            (Phase::Home, "Runtime.afterSend", []) => {
                frame.reply(vec![])?;
                self.events.push(Event {
                    site,
                    action: Action::AfterSend,
                });
            }
            _ => return Err(Failure::Protocol("unexpected service in this language story").into()),
        }
        Ok(Status::Running)
    }

    fn home_target(&self, token: u64) -> Result<ActivationId> {
        let home = self.home.as_ref().ok_or(Failure::CannotReturn)?;
        let live = token == HOME_TOKEN
            && self.root.as_ref().is_some_and(|root| {
                root.call
                    .inspect()
                    .frames()
                    .any(|frame| frame.activation == home.activation)
            });
        if !live {
            return Err(Failure::CannotReturn.into());
        }
        Ok(home.activation.clone())
    }

    fn returned(&mut self, phase: Phase) -> Result<()> {
        match phase {
            Phase::Handler => {
                let handler = self.handler.take().expect("returned handler");
                let answer = handler.answer()?;
                self.evidence.push(handler.retire());
                self.block
                    .as_mut()
                    .expect("signaling block")
                    .reply(vec![Value::I64(answer)])?;
                self.phase = Phase::Block;
            }
            Phase::Block => {
                let block = self.block.take().expect("returned block");
                self.body_answer = Some(block.answer()?);
                self.evidence.push(block.retire());
                self.begin_cleanup()?;
            }
            Phase::Cleanup => self.finish_cleanup()?,
            Phase::Method => {
                let method = self.method.take().expect("returned method");
                let answer = method.answer()?;
                self.evidence.push(method.retire());
                self.root
                    .as_mut()
                    .expect("waiting sender")
                    .reply(vec![Value::I64(answer)])?;
                self.phase = Phase::Home;
            }
            Phase::Home => {
                let root = self.root.take().expect("returned home");
                self.answer = Some(root.answer()?);
                self.evidence.push(root.retire());
                self.retained_cleanup = None;
                self.phase = Phase::Complete;
            }
            _ => return Err(Failure::Protocol("terminal phase has no guest return").into()),
        }
        Ok(())
    }

    fn begin_cleanup(&mut self) -> Result<()> {
        let method = self.method.as_mut().expect("protected method");
        let site = method.site()?;
        // Consuming the binding precedes entry. A cancelled or failing cleanup cannot restart it.
        let callable = self
            .retained_cleanup
            .take()
            .ok_or(Failure::Protocol("cleanup binding already consumed"))?;
        self.cleanup = Some(Frame::start(
            "ensure block / revision 0",
            Arc::clone(&method.code),
            &callable,
            vec![],
            self.remaining_fuel,
        )?);
        self.events.push(Event {
            site,
            action: Action::CleanupStarted,
        });
        self.phase = Phase::Cleanup;
        Ok(())
    }

    fn finish_cleanup(&mut self) -> Result<()> {
        let cleanup = self.cleanup.take().expect("returned cleanup");
        if !matches!(cleanup.call.result(), Some(Ok([]))) {
            return Err(Failure::Protocol("cleanup must return no values").into());
        }
        self.evidence.push(cleanup.retire());
        let site = self.method.as_mut().expect("protected method").site()?;
        self.events.push(Event {
            site,
            action: Action::CleanupFinished,
        });
        if let Some(transfer) = self.transfer.take() {
            // Each crossed VM records its own explicit stop. The language event ties those
            // stops to the lexical exit; no raw child heap values cross this boundary.
            self.evidence
                .push(self.block.take().expect("returning block").abandon());
            self.evidence
                .push(self.method.take().expect("crossed method").abandon());
            if self.remaining_fuel == 0 {
                return Err(Failure::FuelExhausted.into());
            }
            let root = self.root.as_mut().expect("live lexical home");
            let site = root.site()?;
            let before = root.call.work();
            let result = root
                .call
                .pending_host_call()
                .expect("waiting send")
                .return_from(&transfer.target, vec![Value::I64(transfer.value)]);
            self.remaining_fuel -= root
                .call
                .work()
                .checked_sub(before)
                .expect("monotonic VM work")
                .fuel;
            result?;
            self.events.push(Event {
                site,
                action: Action::HomeReturned,
            });
            // Returning from the root can finish without another scheduling quantum.
            self.returned(Phase::Home)?;
        } else {
            let answer = self.body_answer.take().expect("normal body answer");
            self.method
                .as_mut()
                .expect("waiting protection")
                .reply(vec![Value::I64(answer)])?;
            self.phase = Phase::Method;
        }
        Ok(())
    }

    pub(super) fn publish_replacement(&mut self) -> Result<()> {
        if self.waiting != Some(Status::NeedValue) || self.revision() != 0 {
            return Err(
                Failure::Protocol("publish once while the original handler is waiting").into(),
            );
        }
        let callable = Callable::function(
            Arc::clone(&self.replacement.program),
            self.replacement.entry,
        )?;
        let CodeUpdate::Replaced { .. } = self.methods.replace(callable)? else {
            return Err(Failure::Protocol("expected a compatible method replacement").into());
        };
        let site = self.handler.as_mut().expect("waiting handler").site()?;
        self.events.push(Event {
            site,
            action: Action::Published(self.revision()),
        });
        Ok(())
    }

    pub(super) fn recover(&mut self, answer: i64) -> Result<()> {
        if self.waiting != Some(Status::NeedValue) {
            return Err(Failure::Protocol("no handler is waiting for a recovery value").into());
        }
        let handler = self.handler.as_mut().expect("waiting handler");
        let site = handler.site()?;
        handler.reply(vec![Value::I64(answer)])?;
        self.events.push(Event {
            site,
            action: Action::Recovery(answer),
        });
        self.waiting = None;
        Ok(())
    }

    pub(super) fn finish_cleanup_wait(&mut self) -> Result<()> {
        if self.waiting != Some(Status::CleanupWaiting) {
            return Err(Failure::Protocol("no cleanup is waiting for native work").into());
        }
        self.cleanup
            .as_mut()
            .expect("waiting cleanup")
            .reply(vec![])?;
        self.waiting = None;
        Ok(())
    }

    pub(super) fn cancel(&mut self) {
        if matches!(self.phase, Phase::Complete | Phase::Cancelled) {
            return;
        }
        for frame in [
            &mut self.cleanup,
            &mut self.handler,
            &mut self.block,
            &mut self.method,
            &mut self.root,
        ] {
            if let Some(frame) = frame.take() {
                self.evidence.push(frame.retire());
            }
        }
        self.retained_block = None;
        self.retained_cleanup = None;
        self.transfer = None;
        self.waiting = None;
        self.phase = Phase::Cancelled;
    }

    /// Read-only source-linked physical frames, grouped by their independent invocation.
    pub(super) fn stack(&self) -> Vec<(&'static str, Site)> {
        [
            &self.root,
            &self.method,
            &self.block,
            &self.handler,
            &self.cleanup,
        ]
        .into_iter()
        .flatten()
        .flat_map(|frame| {
            frame
                .call
                .inspect()
                .frames()
                .rev()
                .map(|activation| {
                    (
                        frame.label,
                        Site {
                            code: Arc::clone(&frame.code),
                            location: activation.location,
                        },
                    )
                })
                .collect::<Vec<_>>()
        })
        .collect()
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
