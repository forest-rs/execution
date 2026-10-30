// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Embedding-controlled typed message dispatch and live method replacement.

#[path = "message_dispatch/driver.rs"]
mod driver;
#[path = "message_dispatch/model.rs"]
mod model;

use driver::{Cells, DispatchError, Ledger, METHODS, MethodFailure, Methods};
use execution_tape::{
    callable::CodeVersion,
    host::{HostError, ResourceKeyRef, ValueRef},
    recording::{RecordLimits, Recorder, Recording, ReplayOutcome},
    trace::{TraceLocation, TraceSink},
    value::Value,
    vm::{Invocation, InvocationOptions, Limits, PendingHostCall, RunStatus, TrapInfo, Vm},
};
use execution_tape_observatory::{LiveExplorer, ObjectView, RecordingExplorer};
use model::{Compiled, Failure, Result, receiver};
use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    None,
    Snapshot,
    Interactive,
}
struct Options {
    view: View,
    request: usize,
    fuel: u64,
    selector: String,
    capture: Option<PathBuf>,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            view: View::None,
            request: 0,
            fuel: 100,
            selector: "increment".into(),
            capture: None,
        }
    }
}
struct Link {
    location: TraceLocation,
    revision: Option<u64>,
    reply: Option<i64>,
    method_fuel: Option<u64>,
    method_record: Option<usize>,
}
struct RecordedMethod {
    revision: u64,
    recording: Recording,
}
struct Report {
    caller: Compiled,
    old: Compiled,
    new: Compiled,
    host: Cells,
    ledger: Ledger,
    outcome: std::result::Result<[i64; 2], TrapInfo>,
    remaining_fuel: u64,
    published_revision: u64,
    links: Vec<Link>,
    caller_recording: Recording,
    methods: Vec<RecordedMethod>,
    snapshots: Vec<(String, String)>,
}

struct MethodContext<'a> {
    caller: &'a Compiled,
    old: &'a Compiled,
    new: &'a Compiled,
    methods: &'a mut Methods,
    host: &'a Cells,
    ledger: &'a Ledger,
    options: &'a Options,
    records: &'a mut Vec<RecordedMethod>,
    snapshots: &'a mut Vec<(String, String)>,
}
fn execute_method(
    mut call: Invocation<'_, Cells>,
    request: &mut PendingHostCall<'_, '_, Cells>,
    publication: &CodeVersion,
    context: &mut MethodContext<'_>,
    index: usize,
    fuel: u64,
) -> Result<(i64, u64)> {
    // The old target has entered and read its +1 before lookup publishes the +10 body.
    call.resume(1)?;
    if index == 0 {
        context.methods.publish(context.new)?;
        request.context().record_write(ResourceKeyRef::HostState {
            op: METHODS,
            key: 0,
        });
    }
    let selected = index == context.options.request;
    if context.options.capture.is_some() || (selected && context.options.view != View::None) {
        let sources = if publication.revision() == 0 {
            &context.old.sources
        } else {
            &context.new.sources
        };
        let mut view = LiveExplorer::new(call)
            .sources(sources)?
            .requested_by(request, Some(&context.caller.sources))?
            .object_inspector(|host: &Cells, object, _limit| {
                if object != receiver() {
                    return None;
                }
                Some(ObjectView {
                    name: "CounterCell".into(),
                    fields: vec![("value".into(), Value::I64(host.0.borrow().value))],
                    total_fields: Some(1),
                })
            });
        view.inspect_object(receiver());
        let text = view.render_text(160, 48)?;
        if selected && context.options.view == View::Snapshot {
            print!("{text}");
        }
        context
            .snapshots
            .push((format!("method-{index}-start.txt"), text));
        if selected && context.options.view == View::Interactive {
            eprintln!(
                "Send {} of 2 selected method revision {}. Lookup now publishes revision 1.\nThe caller handed Object.send to the embedding for method lookup; no I/O is pending.\nYou control the paused method. The external caller is waiting for its reply.\nc finishes the method; q leaves the view and cancels unfinished work.",
                index + 1,
                publication.revision()
            );
            view.run()?;
        }
        let cancelled =
            selected && context.options.view == View::Interactive && view.result().is_none();
        view.inspect_caller();
        let text = view.render_text(160, 48)?;
        if selected && context.options.view == View::Snapshot {
            print!("{text}");
        }
        context
            .snapshots
            .push((format!("caller-{index}-before-reply.txt"), text));
        if cancelled {
            return Err(Box::new(DispatchError::Cancelled));
        }
        call = view.into_invocation();
    }
    match call.resume(u64::MAX)? {
        RunStatus::Complete => {}
        _ => return Err(Box::new(DispatchError::UnexpectedReply)),
    }
    let values = call
        .result()
        .expect("completed method")
        .map_err(Clone::clone)?;
    let [Value::I64(value)] = values else {
        return Err(Box::new(DispatchError::UnexpectedReply));
    };
    Ok((*value, fuel - call.remaining_fuel()))
}
fn run_method(
    request: &mut PendingHostCall<'_, '_, Cells>,
    publication: CodeVersion,
    context: &mut MethodContext<'_>,
    index: usize,
    fuel: u64,
) -> Result<(i64, u64)> {
    let mut vm = Vm::new(
        context.host.clone(),
        Limits {
            fuel,
            max_host_calls: 1,
            ..Limits::default()
        },
    );
    let mut ledger = context.ledger.clone();
    let mut recorder = Recorder::new(
        Arc::clone(publication.callable().program()),
        RecordLimits::default(),
    );
    let result = match publication.callable().start(
        &mut vm,
        vec![Value::Obj(receiver())],
        InvocationOptions::default()
            .trace(recorder.mask(), &mut recorder)
            .access(&mut ledger),
    ) {
        Ok(call) => execute_method(call, request, &publication, context, index, fuel),
        Err(error) => Err(Box::new(error) as Failure),
    };
    context.records.push(RecordedMethod {
        revision: publication.revision(),
        recording: recorder.finish(),
    });
    result
}

fn deliver(
    request: &mut PendingHostCall<'_, '_, Cells>,
    context: &mut MethodContext<'_>,
    link: &mut Link,
    index: usize,
    fuel: u64,
) -> std::result::Result<(i64, u64), HostError> {
    request.context().record_read(ResourceKeyRef::HostState {
        op: METHODS,
        key: 0,
    });
    let publication = {
        let mut args = request.arguments();
        let (Some(ValueRef::Obj(object)), Some(ValueRef::Str(selector)), None) =
            (args.next(), args.next(), args.next())
        else {
            return Err(HostError::SignatureMismatch);
        };
        context.methods.lookup(object, selector).map_err(|error| {
            HostError::custom(MethodFailure {
                revision: context.methods.increment.current().revision(),
                cause: error,
            })
        })?
    };
    link.revision = Some(publication.revision());
    link.method_record = Some(context.records.len());
    let revision = publication.revision();
    let (reply, spent) =
        run_method(request, publication, context, index, fuel).map_err(|error| {
            HostError::custom(MethodFailure {
                revision,
                cause: error,
            })
        })?;
    link.reply = Some(reply);
    link.method_fuel = Some(spent);
    Ok((reply, spent))
}

fn run(options: &Options) -> Result<Report> {
    let caller = model::caller(&options.selector)?;
    let old = model::method(1)?;
    let new = model::method(10)?;
    let host = Cells::default();
    let ledger = Ledger::default();
    let mut caller_access = ledger.clone();
    let mut methods = Methods::new(&old)?;
    let mut records = Vec::new();
    let mut links = Vec::new();
    let mut snapshots = Vec::new();
    let mut recorder = Recorder::new(Arc::clone(&caller.program), RecordLimits::default());
    let mut vm = Vm::new(
        host.clone(),
        Limits {
            fuel: options.fuel,
            max_host_calls: 2,
            ..Limits::default()
        },
    );
    let mut call =
        execution_tape::callable::Callable::function(Arc::clone(&caller.program), caller.entry)?
            .start(
                &mut vm,
                vec![Value::Obj(receiver())],
                InvocationOptions::default()
                    .yield_host_calls()
                    .trace(recorder.mask(), &mut recorder)
                    .access(&mut caller_access),
            )?;
    let outcome = loop {
        match call.resume(64) {
            Ok(RunStatus::Suspended) => {}
            Err(error) => break Err(error),
            Ok(RunStatus::Complete) => {
                let [Value::Agg(tuple)] = call
                    .result()
                    .expect("completed caller")
                    .expect("successful caller")
                else {
                    return Err(Box::new(DispatchError::UnexpectedReply));
                };
                let (Value::I64(first), Value::I64(second)) = (
                    call.aggregates().tuple_get(*tuple, 0)?,
                    call.aggregates().tuple_get(*tuple, 1)?,
                ) else {
                    return Err(Box::new(DispatchError::UnexpectedReply));
                };
                break Ok([first, second]);
            }
            Ok(RunStatus::AwaitingHost) => {
                let fuel = call.remaining_fuel();
                let mut request = call.pending_host_call().expect("yielded caller request");
                let index = links.len();
                let mut link = Link {
                    location: request.location(),
                    revision: None,
                    reply: None,
                    method_fuel: None,
                    method_record: None,
                };
                // Yielding gives the embedding control of lookup and nested execution.
                // This example answers immediately; it does not need an async executor.
                let mut context = MethodContext {
                    caller: &caller,
                    old: &old,
                    new: &new,
                    methods: &mut methods,
                    host: &host,
                    ledger: &ledger,
                    options,
                    records: &mut records,
                    snapshots: &mut snapshots,
                };
                let (mut slots, outcome) =
                    match deliver(&mut request, &mut context, &mut link, index, fuel) {
                        Ok((reply, spent)) => ([Value::I64(reply)], Ok(spent)),
                        Err(error) => ([Value::Unit], Err(error)),
                    };
                let result = request.complete(&mut slots, outcome);
                links.push(link);
                if let Err(error) = result {
                    break Err(error);
                }
            }
        }
    };
    let remaining_fuel = call.remaining_fuel();
    drop(call);
    Ok(Report {
        caller,
        old,
        new,
        host,
        ledger,
        outcome,
        remaining_fuel,
        published_revision: methods.increment.current().revision(),
        links,
        caller_recording: recorder.finish(),
        methods: records,
        snapshots,
    })
}

fn verify(report: &Report) -> Result<usize> {
    let before = (
        report.host.0.borrow().value,
        report.host.0.borrow().native_calls,
        report.ledger.0.borrow().len(),
    );
    let root = report.caller_recording.replay()?;
    match (&report.outcome, root) {
        (Ok(expected), ReplayOutcome::Returned(values)) => {
            let [Value::Agg(tuple)] = values.values() else {
                return Err("replay did not return a tuple".into());
            };
            for (index, value) in expected.iter().enumerate() {
                if values.aggregates().tuple_get(*tuple, index)? != Value::I64(*value) {
                    return Err("caller replay changed the reply".into());
                }
            }
        }
        (Err(_), ReplayOutcome::Trapped(_)) => {}
        _ => return Err("caller replay changed its outcome".into()),
    }
    let mut checked = 1;
    for method in &report.methods {
        if method.recording.status().is_ok() {
            method.recording.replay()?;
            checked += 1;
        }
    }
    if before
        != (
            report.host.0.borrow().value,
            report.host.0.borrow().native_calls,
            report.ledger.0.borrow().len(),
        )
    {
        return Err("replay changed live host state or resource observations".into());
    }
    Ok(checked)
}
fn json_string(text: &str) -> String {
    let mut value = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => value.push_str("\\\""),
            '\\' => value.push_str("\\\\"),
            c if c.is_control() => {
                use std::fmt::Write;
                write!(value, "\\u{:04x}", u32::from(c)).expect("string formatting");
            }
            c => value.push(c),
        }
    }
    value.push('"');
    value
}
impl Report {
    fn json(&self, replays: usize) -> String {
        let outcome = match &self.outcome {
            Ok(values) => format!("{{\"returned\":[{},{}]}}", values[0], values[1]),
            Err(error) => format!(
                "{{\"trapped\":{},\"function\":{},\"pc\":{}}}",
                json_string(&error.to_string()),
                error.func.0,
                error.pc
            ),
        };
        let links: Vec<_> = self.links.iter().map(|link| format!("    {{\"caller_function\":{},\"caller_pc\":{},\"method_revision\":{},\"reply\":{},\"callback_fuel\":{},\"method_recording\":{}}}", link.location.func.0, link.location.pc, link.revision.map_or_else(|| "null".into(), |n| n.to_string()), link.reply.map_or_else(|| "null".into(), |n| n.to_string()), link.method_fuel.map_or_else(|| "null".into(), |n| n.to_string()), link.method_record.map_or_else(|| "null".into(), |n| n.to_string()))).collect();
        format!(
            "{{\n  \"schema\":\"execution-message-dispatch-example/1\",\n  \"outcome\":{outcome},\n  \"published_revision\":{},\n  \"cell_value\":{},\n  \"native_calls\":{},\n  \"caller_fuel_remaining\":{},\n  \"checked_replays\":{replays},\n  \"resource_observations\":{},\n  \"requests\":[\n{}\n  ]\n}}\n",
            self.published_revision,
            self.host.0.borrow().value,
            self.host.0.borrow().native_calls,
            self.remaining_fuel,
            self.ledger.0.borrow().len(),
            links.join(",\n")
        )
    }
    fn capture(&self, directory: &Path, replays: usize) -> Result<()> {
        let write = |name: &str, bytes: &[u8]| -> Result<()> {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(directory.join(name))?
                .write_all(bytes)?;
            Ok(())
        };
        for (name, compiled) in [
            ("caller", &self.caller),
            ("old", &self.old),
            ("new", &self.new),
        ] {
            write(
                &format!("{name}.tape"),
                &compiled.program.program().encode(),
            )?;
            write(&format!("{name}.sources"), &compiled.sources.encode())?;
        }
        write("story.json", self.json(replays).as_bytes())?;
        for (name, text) in &self.snapshots {
            write(name, text.as_bytes())?;
        }
        write(
            "caller-recording.txt",
            RecordingExplorer::new(&self.caller_recording)
                .sources(&self.caller.sources)?
                .render_text(160, 48)?
                .as_bytes(),
        )?;
        for (index, method) in self.methods.iter().enumerate() {
            let sources = if method.revision == 0 {
                &self.old.sources
            } else {
                &self.new.sources
            };
            write(
                &format!("method-recording-{index}.txt"),
                RecordingExplorer::new(&method.recording)
                    .sources(sources)?
                    .render_text(160, 48)?
                    .as_bytes(),
            )?;
        }
        Ok(())
    }
}
fn entry() -> Result<()> {
    let mut options = Options::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--snapshot" | "--interactive" => {
                if options.view != View::None {
                    return Err("choose one of --snapshot or --interactive".into());
                }
                options.view = if arg == "--snapshot" {
                    View::Snapshot
                } else {
                    View::Interactive
                };
            }
            "--request" => {
                options.request = args.next().ok_or("--request needs 0 or 1")?.parse()?
            }
            "--fuel" => {
                options.fuel = args
                    .next()
                    .ok_or("--fuel needs an instruction budget")?
                    .parse()?
            }
            "--selector" => {
                options.selector = args.next().ok_or("--selector needs a method name")?
            }
            "--capture" => {
                options.capture = Some(args.next().ok_or("--capture needs a new directory")?.into())
            }
            "--color=always" => ratatui::crossterm::style::force_color_output(true),
            "--help" | "-h" => {
                println!(
                    "message_dispatch [--snapshot | --interactive] [--request 0|1] [--capture NEW_DIR]\n                 [--fuel N] [--selector NAME] [--color=always]\n\nA guest sends increment twice. Its first method keeps +1 while lookup publishes +10;\nreplies are 1 and 11. The caller yields Object.send so the embedding can select and\nrun a method. Delivery is immediate: this example has no I/O wait or async executor.\n--request 0 opens the original +1 method; --request 1 opens the replacement +10.\nThe caller waits while you control the paused method. Select the external caller\nto inspect its send and source; stepping still controls the method.\ns executes one instruction; c finishes the method; q leaves the view. Leaving\nbefore return cancels unfinished work and preserves performed effects.\nFuel remaining is this method's execution budget, not elapsed time. Each guest\ninstruction costs one unit; successful callback work is also charged to the caller.\nPausing costs no fuel. Exhaustion terminates execution rather than pausing it.\n--snapshot prints method and caller views, then completes. --capture creates a new\ndirectory of exact tapes, source sidecars, text views and a result survey.\nThese are example artifacts; recording views are not portable replay archives."
                );
                return Ok(());
            }
            _ => return Err(format!("unknown option {arg}; use --help").into()),
        }
    }
    if options.request > 1 {
        return Err("--request must be 0 or 1 (the guest sends two messages)".into());
    }
    if options.selector.is_empty()
        || options.selector.len() > 32
        || !options
            .selector
            .starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        || !options
            .selector
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err("--selector must be a nonempty ASCII identifier of at most 32 bytes".into());
    }
    if let Some(directory) = &options.capture {
        std::fs::create_dir(directory)?;
    }
    let report = run(&options)?;
    let replays = verify(&report)?;
    println!("{}", report.json(replays));
    if let Some(directory) = &options.capture {
        report.capture(directory, replays)?;
    }
    if let Err(error) = &report.outcome {
        return Err(Box::new(error.clone()));
    }
    Ok(())
}

fn main() {
    if let Err(error) = entry() {
        eprintln!("{error}");
        let mut cause = error.source();
        while let Some(error) = cause {
            eprintln!("  caused by: {error}");
            cause = error.source();
        }
        std::process::exit(1);
    }
}

#[cfg(test)]
#[path = "message_dispatch/tests.rs"]
mod tests;
