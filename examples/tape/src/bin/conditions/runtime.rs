// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The language runtime owns the dynamic handler and restart stacks, handler masking and restart
//! policy. Guest code owns its extents: each scope pops itself when its protected region exits,
//! and protected cleanup is ordinary guest code. Exiting to a restart is a guest exception that
//! this runtime raises at the pending request; the VM unwinds it.

use execution_tape::value::UnitId;
use execution_tape::{
    callable::Callable,
    host::{Host, HostContext, HostError, SigHash, ValueRef},
    program::{TagId, ValueType},
    recording::{InstructionCapture, RecordLimits, Recorder, Recording},
    trace::{TraceLocation, TraceSink},
    value::{FuncId, Obj, ObjHandle, Value},
    verifier::VerifiedProgram,
    vm::{ActivationId, Invocation, InvocationOptions, Limits, RunStatus, TrapInfo, Vm},
};
use std::{
    cell::RefCell,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use super::language::{Class, Policy, RESTART_TOKEN, RestartMode};
const SCOPE_LIMIT: usize = 256;
const EVENT_LIMIT: usize = 4096;
static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

impl Class {
    fn decode(value: i64) -> Result<Self, Error> {
        match value {
            0 => Ok(Self::Condition),
            1 => Ok(Self::Warning),
            2 => Ok(Self::Error),
            3 => Ok(Self::ArithmeticError),
            4 => Ok(Self::DivisionByZero),
            5 => Ok(Self::InvalidRestart),
            _ => Err(Error::Abi("unknown condition class")),
        }
    }
    fn is_a(self, expected: Self) -> bool {
        self == expected
            || expected == Self::Condition
            || match self {
                Self::DivisionByZero => matches!(expected, Self::ArithmeticError | Self::Error),
                Self::ArithmeticError | Self::InvalidRestart => expected == Self::Error,
                _ => false,
            }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Condition {
    pub(super) class: Class,
    pub(super) payload: i64,
}
#[derive(Clone, Debug)]
pub(super) enum Error {
    Vm(TrapInfo),
    Abi(&'static str),
    ExpiredRestart,
    ForeignRestart,
    ScopeLimit,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Vm(error) => error.fmt(f),
            Self::Abi(message) => write!(f, "condition protocol: {message}"),
            Self::ExpiredRestart => f.write_str("restart's dynamic extent has ended"),
            Self::ForeignRestart => f.write_str("restart token belongs to another runtime"),
            Self::ScopeLimit => f.write_str("condition runtime scope budget exhausted"),
        }
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Vm(error) => Some(error),
            _ => None,
        }
    }
}
impl From<TrapInfo> for Error {
    fn from(error: TrapInfo) -> Self {
        Self::Vm(error)
    }
}

#[derive(Debug)]
pub(super) enum Status {
    Running,
    Waiting(String),
    Unhandled(Condition),
    RestartReturned(Vec<Value>),
    Complete,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Action {
    ScopeEntered {
        scope: usize,
        kind: ExtentKind,
    },
    ScopeExpired(usize),
    HandlerCalled(usize, Class),
    HandlerDeclined(usize),
    Recovered(usize),
    RestartFound(i64),
    RestartCalled(i64),
    ExitSelected {
        scope: usize,
        reply: Reply,
    },
    InvalidRestart {
        reason: RestartDenial,
        argument: i64,
    },
    Native(i64),
    Unhandled(Class),
    HardAbort,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Reply {
    pub(super) value: i64,
    pub(super) flag: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RestartDenial {
    Expired,
    Foreign,
    Absent,
}
impl RestartDenial {
    // Payload convention for the example InvalidRestart condition.
    fn code(self) -> i64 {
        match self {
            Self::Expired => 1,
            Self::Foreign => 2,
            Self::Absent => 3,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PolicyEnd {
    Returned,
    Trapped,
    Cancelled,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ExtentKind {
    Handler {
        class: Class,
        policy: Policy,
    },
    Restart {
        name: i64,
        mode: RestartMode,
        class: Class,
        token: Obj,
    },
}
#[derive(Clone, Debug)]
pub(super) struct Event {
    pub(super) activation: Option<u64>,
    pub(super) location: TraceLocation,
    pub(super) action: Action,
}
#[derive(Clone, Copy, Debug)]
pub(super) struct Available {
    pub(super) name: i64,
    pub(super) token: Obj,
}

struct NoHost;
impl Host for NoHost {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        _: &[ValueRef<'_>],
        _: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        panic!("the condition runtime must drive yielded requests explicitly")
    }
}
/// The pending request that is waiting for this runtime.
#[derive(Clone)]
struct Key {
    activation: ActivationId,
    location: TraceLocation,
}
#[derive(Clone)]
enum Binding {
    Handler {
        class: Class,
        handler: Value,
        policy: Policy,
    },
    Restart {
        name: i64,
        function: Value,
        mode: RestartMode,
        class: Class,
        token: Obj,
    },
    // Keep only identity after retirement, never borrowed guest capture handles.
    Retired {
        restart: Option<Obj>,
    },
}
impl Binding {
    fn retire(&mut self) -> Self {
        let restart = match self {
            Self::Restart { token, .. }
            | Self::Retired {
                restart: Some(token),
            } => Some(*token),
            _ => None,
        };
        std::mem::replace(self, Self::Retired { restart })
    }
}
struct Mask {
    callback: ActivationId,
    hidden: Vec<usize>,
}
struct Signal {
    key: Key,
    condition: Condition,
    remaining: Vec<usize>,
}
enum AfterRestart {
    Guest,
    Debugger(Signal),
}
enum Continuation {
    Handler {
        scope: usize,
        policy: Policy,
        signal: Signal,
    },
    Restart {
        scope: usize,
        after: AfterRestart,
    },
}
/// A guest callback this runtime entered, and what to do when it returns to `key`.
struct Waiting {
    key: Key,
    callback: ActivationId,
    continuation: Continuation,
}

/// An owned in-memory example capture. Language policy evidence and VM evidence keep their
/// separate completeness: guest replay executes bytecode, not this native policy implementation.
pub(super) struct Session {
    pub(super) recording: Recording,
    pub(super) policy: Vec<Event>,
    pub(super) policy_truncated: bool,
    pub(super) policy_end: PolicyEnd,
}

pub(super) struct Runtime {
    run: Invocation<'static, NoHost>,
    recorder: Rc<RefCell<Recorder>>,
    exit: TagId,
    scopes: Vec<Binding>,
    active: Vec<usize>,
    waiting: Vec<Waiting>,
    masks: Vec<Mask>,
    unhandled: Option<Signal>,
    debugger_reply: Option<Vec<Value>>,
    failed: Option<Error>,
    events: Vec<Event>,
    events_truncated: bool,
    saved_restart: Option<Obj>,
    native_wait: Option<Key>,
}
impl Runtime {
    pub(super) fn new(
        program: Arc<VerifiedProgram>,
        entry: FuncId,
        limits: Limits,
    ) -> Result<Self, Error> {
        let code = program.program();
        let exit = (0..code.tags.len())
            .map(|index| TagId(u32::try_from(index).expect("bounded tag table")))
            .find(|&tag| code.tag_name(tag) == Some(super::program::EXIT_TAG))
            .ok_or(Error::Abi("program declares no exit tag"))?;
        let recorder = Rc::new(RefCell::new(
            Recorder::new(program.clone(), RecordLimits::default())
                .with_instructions(InstructionCapture::default()),
        ));
        let callable =
            Callable::function(program, entry).map_err(|_| Error::Abi("missing entry"))?;
        let mut owned_vm = Vm::new(NoHost, limits);
        let unit = owned_vm.install(Arc::clone(callable.program())).unwrap();
        let run = callable.start_owned(
            owned_vm,
            unit,
            vec![],
            InvocationOptions::default()
                .yield_host_calls()
                .trace_owned(recorder.mask(), recorder.clone()),
        )?;
        Ok(Self {
            run,
            recorder,
            exit,
            scopes: Vec::new(),
            active: Vec::new(),
            waiting: Vec::new(),
            masks: Vec::new(),
            unhandled: None,
            debugger_reply: None,
            failed: None,
            events: Vec::new(),
            events_truncated: false,
            saved_restart: None,
            native_wait: None,
        })
    }
    pub(super) fn explanation(&self) -> String {
        let mut text = String::from("Dynamic bindings (innermost first):\n");
        for id in self.active.iter().rev() {
            let role = match &self.scopes[*id] {
                Binding::Handler { class, policy, .. } => format!(
                    "handler {class:?} [{policy:?}]{}",
                    if self.masked(*id) {
                        " (masked during handler execution)"
                    } else {
                        ""
                    }
                ),
                Binding::Restart { name, mode, .. } => {
                    format!("restart {} [{mode:?}]", super::program::restart_name(*name))
                }
                Binding::Retired { .. } => "retired binding".into(),
            };
            text.push_str(&format!("  scope {id}: {role}\n"));
        }
        text.push_str("Guest frames (innermost first):\n");
        for frame in self.run.inspect().frames() {
            let name = self
                .run
                .program()
                .program()
                .function_name(frame.location.func.0)
                .unwrap_or("unnamed");
            text.push_str(&format!(
                "  activation #{} {name}:{:06}{}\n",
                frame.activation.ordinal(),
                frame.location.pc,
                if frame.waiting_for_return {
                    " (waiting)"
                } else {
                    ""
                }
            ));
        }
        text
    }
    pub(super) fn outputs(&self) -> Result<&[Value], Error> {
        self.run
            .result()
            .ok_or(Error::Abi("invocation is still active"))?
            .map_err(|error| Error::Vm(error.clone()))
    }
    pub(super) fn into_session(self) -> Session {
        let Self {
            run,
            recorder,
            events,
            events_truncated,
            ..
        } = self;
        let policy_end = match run.result() {
            Some(Ok(_)) => PolicyEnd::Returned,
            Some(Err(_)) => PolicyEnd::Trapped,
            None => PolicyEnd::Cancelled,
        };
        drop(run);
        Session {
            recording: Rc::try_unwrap(recorder)
                .expect("invocation observer retired")
                .into_inner()
                .finish(),
            policy: events,
            policy_truncated: events_truncated,
            policy_end,
        }
    }
    fn key(&self) -> Result<Key, Error> {
        let inspection = self.run.inspect();
        let frame = inspection
            .frames()
            .next()
            .ok_or(Error::Abi("no active guest frame"))?;
        Ok(Key {
            activation: frame.activation,
            location: frame.location,
        })
    }
    fn event(&mut self, key: &Key, action: Action) {
        self.event_at(Some(key.activation.ordinal()), key.location, action);
    }
    fn event_at(&mut self, activation: Option<u64>, location: TraceLocation, action: Action) {
        if self.events.len() < EVENT_LIMIT {
            self.events.push(Event {
                activation,
                location,
                action,
            });
        } else {
            self.events_truncated = true;
        }
    }
    fn callable(&self, value: &Value, args: &[ValueType], rets: &[ValueType]) -> Result<(), Error> {
        let Value::Closure(closure) = value else {
            return Err(Error::Abi("expected a guest closure"));
        };
        let function = self
            .run
            .program()
            .function(closure.func.func)
            .ok_or(Error::Abi("unknown closure body"))?;
        let expected = std::iter::once(ValueType::Agg).chain(args.iter().copied());
        if !function.arg_types().iter().copied().eq(expected)
            || !function
                .ret_types()
                .iter()
                .copied()
                .eq(rets.iter().copied())
        {
            return Err(Error::Abi(
                "closure has the wrong language protocol signature",
            ));
        }
        Ok(())
    }
    fn guest(
        &mut self,
        key: Key,
        callee: Value,
        args: Vec<Value>,
        continuation: Continuation,
    ) -> Result<ActivationId, Error> {
        if self.waiting.len() >= SCOPE_LIMIT {
            return Err(Error::ScopeLimit);
        }
        self.run
            .pending_host_call()
            .ok_or(Error::Abi("guest callback requires a pending request"))?
            .call_guest(callee, args)?;
        let callback = self.key()?.activation;
        self.waiting.push(Waiting {
            key,
            callback: callback.clone(),
            continuation,
        });
        Ok(callback)
    }
    fn complete(&mut self, mut values: Vec<Value>) -> Result<(), Error> {
        self.run
            .pending_host_call()
            .ok_or(Error::Abi("completion requires a pending request"))?
            .complete(&mut values, Ok(0))?;
        Ok(())
    }
    pub(super) fn complete_native(&mut self, values: Vec<Value>) -> Result<(), Error> {
        if let Some(error) = &self.failed {
            return Err(error.clone());
        }
        let waiting = self
            .native_wait
            .as_ref()
            .ok_or(Error::Abi("not waiting for native work"))?;
        let current = self.key()?;
        if waiting.activation != current.activation || waiting.location != current.location {
            return Err(Error::Abi(
                "native wait no longer matches the pending request",
            ));
        }
        self.native_wait = None;
        self.complete(values).map_err(|error| self.fail(error))
    }
    pub(super) fn poll(&mut self, quantum: u64) -> Result<Status, Error> {
        if let Some(error) = &self.failed {
            return Err(error.clone());
        }
        if let Some(values) = self.debugger_reply.take() {
            return Ok(Status::RestartReturned(values));
        }
        if let Some(signal) = &self.unhandled {
            return Ok(Status::Unhandled(signal.condition));
        }
        self.step(quantum).map_err(|error| self.fail(error))
    }
    fn fail(&mut self, error: Error) -> Error {
        let key = self.key().ok();
        let location = key.as_ref().map(|key| key.location).or(match &error {
            Error::Vm(trap) => Some(TraceLocation {
                unit: UnitId(0),
                func: trap.func,
                pc: trap.pc,
            }),
            _ => None,
        });
        if let Some(location) = location {
            self.event_at(
                key.map(|key| key.activation.ordinal()),
                location,
                Action::HardAbort,
            );
        }
        // Native ABI/policy faults are hard host failures. Invalid language restart requests
        // take the signal path instead. No guest cleanup runs on hard abort or Drop.
        if let Some(request) = self.run.pending_host_call() {
            let _ = request.complete(&mut [], Err(HostError::custom(error.clone())));
        }
        self.active.clear();
        self.masks.clear();
        self.waiting.clear();
        self.unhandled = None;
        self.debugger_reply = None;
        self.native_wait = None;
        for scope in &mut self.scopes {
            scope.retire();
        }
        self.failed = Some(error.clone());
        error
    }
    /// Forgets callbacks and masks that an exception abandoned with their guest frames.
    fn prune(&mut self) {
        let live: Vec<_> = self
            .run
            .inspect()
            .frames()
            .map(|frame| frame.activation)
            .collect();
        self.waiting
            .retain(|waiting| live.contains(&waiting.callback));
        self.masks.retain(|mask| live.contains(&mask.callback));
    }
    fn step(&mut self, quantum: u64) -> Result<Status, Error> {
        match self.run.resume(quantum)? {
            RunStatus::Suspended => return Ok(Status::Running),
            RunStatus::Complete => return Ok(Status::Complete),
            RunStatus::AwaitingHost => {}
        }
        let key = self.key()?;
        let mut request = self
            .run
            .pending_host_call()
            .ok_or(Error::Abi("missing pending request"))?;
        if let Some(values) = request.take_callback_results() {
            drop(request);
            // The returned callback is the newest one entered from this request.
            let index = self
                .waiting
                .iter()
                .rposition(|waiting| {
                    waiting.key.activation == key.activation && waiting.key.location == key.location
                })
                .ok_or(Error::Abi(
                    "callback returned without a language continuation",
                ))?;
            let waiting = self.waiting.remove(index);
            self.masks.retain(|mask| mask.callback != waiting.callback);
            self.returned(key, waiting.continuation, values)?;
            return Ok(Status::Running);
        }
        let symbol = request.symbol().to_string();
        let args: Vec<_> = request.arguments().map(|value| value.to_value()).collect();
        drop(request);
        self.prune();
        self.dispatch(key, &symbol, args)
    }
    fn push(&mut self, key: &Key, binding: Binding) -> Result<(), Error> {
        if self.scopes.len() >= SCOPE_LIMIT {
            return Err(Error::ScopeLimit);
        }
        let id = self.scopes.len();
        let kind = match &binding {
            Binding::Handler { class, policy, .. } => ExtentKind::Handler {
                class: *class,
                policy: *policy,
            },
            Binding::Restart {
                name,
                mode,
                class,
                token,
                ..
            } => ExtentKind::Restart {
                name: *name,
                mode: *mode,
                class: *class,
                token: *token,
            },
            Binding::Retired { .. } => return Err(Error::Abi("new scope cannot be retired")),
        };
        self.scopes.push(binding);
        self.active.push(id);
        self.event(key, Action::ScopeEntered { scope: id, kind });
        self.complete(vec![Value::I64(scope_value(id))])
    }
    fn pop(&mut self, key: &Key, scope: i64) -> Result<(), Error> {
        let id = usize::try_from(scope).map_err(|_| Error::Abi("negative scope"))?;
        if self.active.last() != Some(&id) {
            return Err(Error::Abi("dynamic scopes left out of order"));
        }
        self.active.pop();
        self.scopes[id].retire();
        self.event(key, Action::ScopeExpired(id));
        self.complete(vec![])
    }
    fn masked(&self, scope: usize) -> bool {
        self.masks.iter().any(|mask| mask.hidden.contains(&scope))
    }
    fn signal(&mut self, key: Key, condition: Condition) -> Result<(), Error> {
        // Snapshot outermost first, then pop innermost without shifting the remaining handlers.
        let remaining = self
            .active
            .iter()
            .copied()
            .filter(|id| {
                !self.masked(*id)
                    && matches!(self.scopes[*id], Binding::Handler { class, .. } if condition.class.is_a(class))
            })
            .collect();
        self.next_handler(Signal {
            key,
            condition,
            remaining,
        })
    }
    fn next_handler(&mut self, mut signal: Signal) -> Result<(), Error> {
        while let Some(id) = signal.remaining.pop() {
            let Binding::Handler {
                handler, policy, ..
            } = self.scopes[id].clone()
            else {
                continue;
            };
            if self.masked(id) {
                continue;
            }
            // While a handler runs, it and every handler bound inside it are masked.
            let position = self
                .active
                .iter()
                .position(|scope| *scope == id)
                .ok_or(Error::Abi("handler is not dynamically active"))?;
            let hidden = self.active[position..]
                .iter()
                .copied()
                .filter(|scope| matches!(self.scopes[*scope], Binding::Handler { .. }))
                .collect();
            let key = signal.key.clone();
            let condition = signal.condition;
            self.event(&key, Action::HandlerCalled(id, condition.class));
            let callback = self.guest(
                key,
                handler,
                vec![
                    Value::I64(condition.class.code()),
                    Value::I64(condition.payload),
                ],
                Continuation::Handler {
                    scope: id,
                    policy,
                    signal,
                },
            )?;
            self.masks.push(Mask { callback, hidden });
            return Ok(());
        }
        self.event(&signal.key, Action::Unhandled(signal.condition.class));
        self.unhandled = Some(signal);
        Ok(())
    }
    pub(super) fn available(&self, condition: Condition) -> Vec<Available> {
        self.available_for(Some(condition))
    }
    fn available_for(&self, condition: Option<Condition>) -> Vec<Available> {
        self.active
            .iter()
            .rev()
            .filter_map(|id| match self.scopes[*id] {
                Binding::Restart {
                    name, token, class, ..
                } if condition.is_none_or(|condition| condition.class.is_a(class)) => {
                    Some(Available { name, token })
                }
                _ => None,
            })
            .collect()
    }
    fn restart(&self, token: Obj) -> Result<usize, Error> {
        let id = self
            .scopes
            .iter()
            .position(|scope| matches!(scope, Binding::Restart { token: known, .. } | Binding::Retired { restart: Some(known) } if *known == token))
            .ok_or(Error::ForeignRestart)?;
        if matches!(self.scopes[id], Binding::Retired { .. }) {
            return Err(Error::ExpiredRestart);
        }
        Ok(id)
    }
    fn invoke_restart(
        &mut self,
        key: Key,
        id: usize,
        argument: i64,
        after: AfterRestart,
    ) -> Result<(), Error> {
        let Binding::Restart { name, function, .. } = self.scopes[id].clone() else {
            return Err(Error::Abi("restart binding expected"));
        };
        self.event(&key, Action::RestartCalled(name));
        self.guest(
            key,
            function,
            vec![Value::I64(argument)],
            Continuation::Restart { scope: id, after },
        )?;
        Ok(())
    }
    pub(super) fn choose_restart(&mut self, token: Obj, argument: i64) -> Result<(), Error> {
        let id = self.restart(token)?;
        let signal = self
            .unhandled
            .as_ref()
            .ok_or(Error::Abi("no unhandled condition"))?;
        let Binding::Restart { class, .. } = self.scopes[id] else {
            return Err(Error::Abi("restart binding expected"));
        };
        if !signal.condition.class.is_a(class) {
            return Err(Error::Abi("restart is not applicable to this condition"));
        }
        let signal = self.unhandled.take().expect("checked signal");
        self.invoke_restart(
            signal.key.clone(),
            id,
            argument,
            AfterRestart::Debugger(signal),
        )
        .map_err(|error| self.fail(error))
    }
    pub(super) fn recover(&mut self, value: i64, flag: bool) -> Result<(), Error> {
        let _signal = self
            .unhandled
            .take()
            .ok_or(Error::Abi("no unhandled condition"))?;
        self.complete(vec![Value::I64(value), Value::Bool(flag)])
            .map_err(|error| self.fail(error))
    }
    fn returned(
        &mut self,
        key: Key,
        continuation: Continuation,
        values: Vec<Value>,
    ) -> Result<(), Error> {
        match continuation {
            Continuation::Handler {
                scope,
                policy,
                signal,
            } => {
                let [Value::Bool(accepted), Value::I64(value), Value::Bool(flag)] =
                    values.as_slice()
                else {
                    return Err(Error::Abi("handler reply must be (accepted, value, flag)"));
                };
                if policy == Policy::Dylan && *accepted {
                    self.event(&key, Action::Recovered(scope));
                    self.complete(vec![Value::I64(*value), Value::Bool(*flag)])?;
                } else {
                    self.event(&key, Action::HandlerDeclined(scope));
                    self.next_handler(signal)?;
                }
            }
            Continuation::Restart { scope, after } => {
                let reply = pair(&values)?;
                let Binding::Restart { mode, .. } = self.scopes[scope] else {
                    return Err(Error::Abi("restart binding expected"));
                };
                if mode == RestartMode::Exit {
                    // The guest unwinds to the restart's region, running cleanup on the way.
                    self.event(&key, Action::ExitSelected { scope, reply });
                    let mut request = self
                        .run
                        .pending_host_call()
                        .ok_or(Error::Abi("exit requires a pending request"))?;
                    let exit = request
                        .context()
                        .tag(self.exit)
                        .ok_or(Error::Abi("the program declares the exit tag"))?;
                    request.throw(
                        exit,
                        vec![
                            Value::I64(scope_value(scope)),
                            Value::I64(reply.value),
                            Value::Bool(reply.flag),
                        ],
                    )?;
                } else {
                    match after {
                        AfterRestart::Guest => self.complete(values)?,
                        AfterRestart::Debugger(signal) => {
                            self.unhandled = Some(signal);
                            self.debugger_reply = Some(values);
                        }
                    }
                }
            }
        }
        Ok(())
    }
    fn dispatch(&mut self, key: Key, symbol: &str, args: Vec<Value>) -> Result<Status, Error> {
        match (symbol, args.as_slice()) {
            ("condition.push_handler", [handler, Value::I64(class), Value::U64(policy)]) => {
                self.callable(
                    handler,
                    &[ValueType::I64, ValueType::I64],
                    &[ValueType::Bool, ValueType::I64, ValueType::Bool],
                )?;
                let policy = match policy {
                    0 => Policy::CommonLisp,
                    1 => Policy::Dylan,
                    _ => return Err(Error::Abi("unknown handler policy")),
                };
                let binding = Binding::Handler {
                    class: Class::decode(*class)?,
                    handler: handler.clone(),
                    policy,
                };
                self.push(&key, binding)?;
            }
            (
                "condition.push_restart",
                [
                    function,
                    Value::I64(name),
                    Value::U64(mode),
                    Value::I64(class),
                ],
            ) => {
                self.callable(
                    function,
                    &[ValueType::I64],
                    &[ValueType::I64, ValueType::Bool],
                )?;
                let mode = match mode {
                    0 => RestartMode::Function,
                    1 => RestartMode::Exit,
                    _ => return Err(Error::Abi("unknown restart mode")),
                };
                let binding = Binding::Restart {
                    name: *name,
                    function: function.clone(),
                    mode,
                    class: Class::decode(*class)?,
                    token: restart_token()?,
                };
                self.push(&key, binding)?;
            }
            ("condition.pop", [Value::I64(scope)]) => self.pop(&key, *scope)?,
            ("condition.signal", [Value::I64(class), Value::I64(payload)]) => self.signal(
                key,
                Condition {
                    class: Class::decode(*class)?,
                    payload: *payload,
                },
            )?,
            ("condition.find", [Value::I64(name), Value::I64(class), Value::I64(payload)]) => {
                let condition = if *class == -1 {
                    None
                } else {
                    Some(Condition {
                        class: Class::decode(*class)?,
                        payload: *payload,
                    })
                };
                let restart = self
                    .available_for(condition)
                    .into_iter()
                    .find(|restart| restart.name == *name);
                if restart.is_some() {
                    self.event(&key, Action::RestartFound(*name));
                }
                // Explicit optional reply. Token zero is absent, not a VM-wide null object.
                let token = restart.map_or(
                    Obj {
                        host_type: RESTART_TOKEN,
                        handle: ObjHandle(0),
                    },
                    |restart| restart.token,
                );
                self.complete(vec![Value::Bool(restart.is_some()), Value::Obj(token)])?;
            }
            ("condition.invoke", [Value::Obj(token), Value::I64(argument)]) => {
                match self.restart(*token) {
                    Ok(id) => self.invoke_restart(key, id, *argument, AfterRestart::Guest)?,
                    Err(error) => {
                        let reason = match error {
                            Error::ExpiredRestart => RestartDenial::Expired,
                            Error::ForeignRestart if token.handle.0 == 0 => RestartDenial::Absent,
                            Error::ForeignRestart => RestartDenial::Foreign,
                            other => return Err(other),
                        };
                        self.event(
                            &key,
                            Action::InvalidRestart {
                                reason,
                                argument: *argument,
                            },
                        );
                        self.signal(
                            key,
                            Condition {
                                class: Class::InvalidRestart,
                                payload: reason.code(),
                            },
                        )?;
                    }
                }
            }
            ("demo.save_restart", [Value::Obj(token)]) => {
                self.saved_restart = Some(*token);
                self.complete(vec![])?;
            }
            ("demo.load_restart", []) => {
                let token = self
                    .saved_restart
                    .ok_or(Error::Abi("no saved restart token"))?;
                self.complete(vec![Value::Obj(token)])?;
            }
            ("demo.emit", [Value::I64(value)]) => {
                self.event(&key, Action::Native(*value));
                self.complete(vec![])?;
            }
            ("demo.wait", []) => {
                self.native_wait = Some(key);
                return Ok(Status::Waiting(symbol.into()));
            }
            _ => return Err(Error::Abi("unknown service or wrong host argument shape")),
        }
        Ok(Status::Running)
    }
}
fn scope_value(id: usize) -> i64 {
    i64::try_from(id).expect("scope count is bounded")
}
fn pair(values: &[Value]) -> Result<Reply, Error> {
    if let [Value::I64(value), Value::Bool(flag)] = values {
        Ok(Reply {
            value: *value,
            flag: *flag,
        })
    } else {
        Err(Error::Abi("restart reply must be (value, flag)"))
    }
}

// The replacement try_update API is newer than the workspace MSRV.
#[allow(deprecated, reason = "fetch_update is available on the workspace MSRV")]
fn restart_token() -> Result<Obj, Error> {
    let serial = NEXT_TOKEN
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
        .map_err(|_| Error::Abi("restart token namespace exhausted"))?;
    Ok(Obj {
        host_type: RESTART_TOKEN,
        handle: ObjHandle(serial),
    })
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
