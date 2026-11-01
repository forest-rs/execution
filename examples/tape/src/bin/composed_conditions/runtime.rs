// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Language policy chooses handlers, restarts and cleanup; the VM owns the actual control tree.

use super::{
    Mode,
    program::{Compiled, Programs, Result},
};
use execution_tape::{
    host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef, sig_hash},
    instance::Instance,
    recording::{InstructionCapture, RecordLimits, Recorder, ReplayOutcome},
    trace::TraceSink,
    value::{FuncId, Value},
    vm::{ActivationId, Invocation, InvocationOptions, Limits, RunStatus, Vm},
};
use execution_tape_observatory::{LiveExplorer, RecordingExplorer};
use std::{cell::RefCell, rc::Rc, sync::Arc};

#[derive(Default)]
pub(super) struct Native {
    cleanups: usize,
}
impl Host for Native {
    fn call(
        &mut self,
        symbol: &str,
        signature: SigHash,
        _: &[ValueRef<'_>],
        _: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> std::result::Result<u64, HostError> {
        if symbol != "native.cleanup" {
            return Err(HostError::UnknownSymbol);
        }
        if signature
            != sig_hash(&HostSig {
                args: vec![],
                rets: vec![],
            })
        {
            return Err(HostError::SignatureMismatch);
        }
        self.cleanups += 1;
        Ok(0)
    }
}

#[derive(Clone)]
struct Entry {
    instance: Instance,
    function: FuncId,
}
impl Entry {
    fn bind(vm: &mut Vm<Native>, definition: &Compiled) -> Result<Self> {
        Ok(Self {
            instance: vm.instantiate(&definition.code)?,
            function: definition.entry,
        })
    }
}

struct Handler {
    class: &'static str,
    home: ActivationId,
    entry: Entry,
}
struct Restart {
    name: &'static str,
    home: ActivationId,
    entry: Entry,
}

pub(super) struct Runtime<'code, 'vm> {
    programs: &'code Programs,
    invocation: Option<Invocation<'vm, Native>>,
    recorder: Option<Rc<RefCell<Recorder>>>,
    home: ActivationId,
    method: Entry,
    replacement: Entry,
    restart: Entry,
    cleanup: Entry,
    handlers: Vec<Handler>,
    restarts: Vec<Restart>,
    mode: Mode,
    published: bool,
    snapshot: bool,
    explained: bool,
    explain: bool,
    explore: bool,
}

impl<'code, 'vm> Runtime<'code, 'vm> {
    pub(super) fn new(
        programs: &'code Programs,
        vm: &'vm mut Vm<Native>,
        mode: Mode,
        snapshot: bool,
        explain: bool,
        explore: bool,
    ) -> Result<Self> {
        let method = Entry::bind(vm, &programs.divide)?;
        let replacement = Entry::bind(vm, &programs.replacement)?;
        let restart = Entry::bind(vm, &programs.restart)?;
        let cleanup = Entry::bind(vm, &programs.cleanup)?;
        let handler = Entry::bind(
            vm,
            if mode == Mode::Recover {
                &programs.recovery
            } else {
                &programs.handler
            },
        )?;
        let recorder = Rc::new(RefCell::new(
            Recorder::new(
                Arc::new(programs.main.code.clone()),
                RecordLimits::default(),
            )
            .with_instructions(InstructionCapture::default()),
        ));
        let mask = recorder.borrow().mask();
        let invocation = vm.start(
            &programs.main.code,
            programs.main.entry,
            &[],
            InvocationOptions::default()
                .yield_host_calls()
                .trace_owned(mask, recorder.clone()),
        )?;
        let home = invocation
            .inspect()
            .frames()
            .next()
            .expect("initialized lexical home")
            .activation;
        Ok(Self {
            programs,
            invocation: Some(invocation),
            recorder: Some(recorder),
            home: home.clone(),
            method,
            replacement,
            restart,
            cleanup,
            handlers: vec![Handler {
                class: "ZeroDivisor",
                home,
                entry: handler,
            }],
            restarts: Vec::new(),
            mode,
            published: false,
            snapshot,
            explained: false,
            explain,
            explore,
        })
    }

    fn show_stack(&self) {
        let run = self.invocation.as_ref().expect("execution owner");
        println!("Actual VM stack, innermost first:");
        for frame in run.inspect().frames() {
            let name = frame.program.program().name().unwrap_or("unnamed");
            let source = self
                .programs
                .all()
                .into_iter()
                .find(|compiled| compiled.code.shares_definition_with(&frame.program))
                .and_then(|compiled| compiled.sources.bind(frame.program.program()).ok())
                .and_then(|sources| sources.lookup(frame.location));
            println!(
                "  {name} · f{}:{:06} · {}",
                frame.location.func.0,
                frame.location.pc,
                source.map_or_else(
                    || "source unavailable".into(),
                    |range| range.text().replace('\n', " ")
                )
            );
        }
    }

    fn show_snapshot(&mut self) -> Result<()> {
        let invocation = self.invocation.take().expect("execution owner");
        let mut view = LiveExplorer::new(invocation);
        for definition in self.programs.all() {
            view = view.sources_for(&definition.code, &definition.sources)?;
        }
        println!("{}", view.render_text(148, 46)?);
        self.invocation = Some(view.into_invocation());
        Ok(())
    }

    fn deliver(&mut self, symbol: &str) -> Result<()> {
        let run = self.invocation.as_mut().expect("execution owner");
        let mut request = run.pending_host_call().expect("yielded boundary");
        let live: Vec<_> = request
            .inspect()
            .frames()
            .map(|frame| frame.activation)
            .collect();
        // Language capabilities expire with actual activations, independently of equal local IDs.
        self.restarts.retain(|restart| live.contains(&restart.home));
        let returned = request.take_callback_results();
        let args = request.arguments().map(ValueRef::to_value).collect();
        match symbol {
            "Object.send" => {
                if let Some(mut values) = returned {
                    println!("Method returned {values:?}; completing its original send.");
                    request.complete(&mut values, Ok(0))?;
                } else {
                    println!(
                        "Lookup chooses {}.",
                        self.method
                            .instance
                            .program()
                            .program()
                            .name()
                            .unwrap_or("method")
                    );
                    request.call_instance(
                        &self.method.instance,
                        self.method.function,
                        args,
                        InvocationOptions::default().yield_host_calls(),
                    )?;
                }
            }
            "Condition.signal.ZeroDivisor" => {
                if let Some(mut values) = returned {
                    println!("Handler resumes the division with divisor {values:?}.");
                    request.complete(&mut values, Ok(0))?;
                } else {
                    let scope = live.first().expect("signalling method").clone();
                    self.restarts.push(Restart {
                        name: "use_divisor",
                        home: scope,
                        entry: self.restart.clone(),
                    });
                    let handler = self
                        .handlers
                        .iter()
                        .rev()
                        .find(|handler| {
                            handler.class == "ZeroDivisor" && live.contains(&handler.home)
                        })
                        .ok_or("unhandled ZeroDivisor condition")?;
                    if !self.published {
                        self.method = self.replacement.clone();
                        self.published = true;
                        println!(
                            "Published divide/v2; the suspended divide/v1 still owns its original code."
                        );
                    }
                    println!("Dynamic handler receives the shared condition tuple.");
                    request.call_instance(
                        &handler.entry.instance,
                        handler.entry.function,
                        args,
                        InvocationOptions::default().yield_host_calls(),
                    )?;
                }
            }
            "Restart.invoke.use_divisor" if self.mode == Mode::Escape => {
                if returned.is_some() {
                    println!(
                        "Guest cleanup returned; returning 42 from the lexical main activation."
                    );
                    request.return_from(&self.home, vec![Value::I64(42)])?;
                } else {
                    println!("Exit policy runs protected guest cleanup before non-local return.");
                    request.call_instance(
                        &self.cleanup.instance,
                        self.cleanup.function,
                        vec![],
                        InvocationOptions::default().yield_host_calls(),
                    )?;
                }
            }
            "Restart.invoke.use_divisor" => {
                if let Some(mut values) = returned {
                    println!("Restart returned {values:?}; handler continues.");
                    request.complete(&mut values, Ok(0))?;
                } else {
                    let restart = self
                        .restarts
                        .iter()
                        .rev()
                        .find(|restart| {
                            restart.name == "use_divisor" && live.contains(&restart.home)
                        })
                        .ok_or("restart home has expired")?;
                    println!("Invoking the nearest live use_divisor restart.");
                    request.call_instance(
                        &restart.entry.instance,
                        restart.entry.function,
                        args,
                        InvocationOptions::default().yield_host_calls(),
                    )?;
                }
            }
            "native.cleanup" => {
                println!("Native cleanup effect runs inside the guest cleanup function.");
                request.dispatch()?;
            }
            _ => return Err(format!("unknown language boundary {symbol:?}").into()),
        }
        Ok(())
    }

    pub(super) fn run(&mut self) -> Result<()> {
        loop {
            let status = self
                .invocation
                .as_mut()
                .expect("execution owner")
                .resume(128)?;
            match status {
                RunStatus::Suspended => {}
                RunStatus::Complete => {
                    let run = self.invocation.as_mut().expect("execution owner");
                    println!(
                        "Execution result: {:?}",
                        run.take_outputs().expect("root return")
                    );
                    println!(
                        "Cleanup effects: {}; execution work: {:?}",
                        run.host().cleanups,
                        run.work()
                    );
                    self.replay()?;
                    return Ok(());
                }
                RunStatus::AwaitingHost => {
                    let symbol = self
                        .invocation
                        .as_mut()
                        .expect("execution owner")
                        .pending_host_call()
                        .expect("yielded boundary")
                        .symbol()
                        .to_owned();
                    if self.explain {
                        self.show_stack();
                    }
                    if self.snapshot && !self.explained && symbol == "Restart.invoke.use_divisor" {
                        self.show_snapshot()?;
                        self.explained = true;
                    }
                    self.deliver(&symbol)?;
                }
            }
        }
    }

    fn replay(&mut self) -> Result<()> {
        // The execution must release its observer before its evidence can be frozen. This
        // also proves replay has no access to the original invocation or its host effects.
        drop(self.invocation.take());
        let recorder = Rc::try_unwrap(self.recorder.take().expect("capture owner"))
            .map_err(|_| "execution still holds the recorder")?
            .into_inner();
        let record = recorder.finish();
        record.status().map_err(Clone::clone)?;
        println!("Recorded execution, in actual admission order:");
        for segment in record.segments() {
            println!(
                "  #{} · {} · parent {}",
                segment.id,
                segment.program.program().name().unwrap_or("unnamed"),
                segment.parent.map_or_else(
                    || "application".into(),
                    |parent| format!("#{}:{}", parent.segment, parent.activation)
                )
            );
        }
        match record.replay()? {
            ReplayOutcome::Returned(values) => {
                println!("Checked replay matched: {:?}", values.values());
            }
            ReplayOutcome::Trapped(error) => return Err(error.into()),
        }
        println!(
            "Replay executes guest code and recorded control; native effects are not repeated."
        );
        if self.snapshot || self.explore {
            let handler = record
                .segments()
                .find(|segment| {
                    segment
                        .program
                        .shares_definition_with(&self.programs.handler.code)
                        || segment
                            .program
                            .shares_definition_with(&self.programs.recovery.code)
                })
                .ok_or("handler evidence missing")?;
            let configured = || -> Result<RecordingExplorer<'_>> {
                let mut view = RecordingExplorer::new(&record)
                    .segment(handler.id)?
                    .instructions();
                for definition in self.programs.all() {
                    view = view.sources_for(&definition.code, &definition.sources)?;
                }
                Ok(view)
            };
            if self.snapshot {
                println!(
                    "Recorded handler evidence (execution already finished):\n{}",
                    configured()?.render_text(148, 46)?
                );
            }
            if self.explore {
                configured()?.run()?;
            }
        }
        Ok(())
    }
}

pub(super) fn store(fuel: u64) -> Vm<Native> {
    Vm::new(
        Native::default(),
        Limits {
            fuel,
            ..Limits::default()
        },
    )
}
