// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Live execution with real host objects, nested calls and explicit authored-source connections.

use execution_tape::{
    asm::{FunctionSig, ProgramBuilder},
    host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef},
    program::{HostTypeId, ValueType},
    source::{SourceDocument, SourceId, SourceLimits, SourceMap},
    value::{FuncId, Obj, ObjHandle, Value},
    verifier::VerifiedProgram,
    vm::{InvocationOptions, Limits, Vm},
};
use execution_tape_observatory::LiveExplorer;

const COUNTER: HostTypeId = HostTypeId(1);
const SOURCE: &str = "fn main(receiver) {\n    let answer = double(receiver);\n    publish(receiver, answer);\n    return (receiver, answer);\n}\n\nfn double(receiver) {\n    let sample = read(receiver);\n    return sample + sample;\n}\n";

#[derive(Debug)]
struct Counter {
    value: i64,
    reads: usize,
    published: Vec<i64>,
}
impl Host for Counter {
    fn call(
        &mut self,
        symbol: &str,
        _: SigHash,
        args: &[ValueRef<'_>],
        rets: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        let Some(ValueRef::Obj(receiver)) = args.first() else {
            return Err(HostError::Failed);
        };
        if *receiver != object() {
            return Err(HostError::Failed);
        }
        match symbol {
            "Counter.read" => {
                self.reads += 1;
                rets[0] = Value::I64(self.value);
            }
            "Counter.publish" => {
                let Some(ValueRef::I64(value)) = args.get(1) else {
                    return Err(HostError::Failed);
                };
                self.published.push(*value);
            }
            _ => return Err(HostError::Failed),
        }
        Ok(0)
    }
}
fn object() -> Obj {
    Obj {
        host_type: COUNTER,
        handle: ObjHandle(42),
    }
}
fn source(text: &str) -> std::ops::Range<usize> {
    let start = SOURCE.find(text).expect("demo source fragment");
    start..start + text.len()
}
fn program() -> Result<(VerifiedProgram, FuncId, SourceMap), Box<dyn std::error::Error>> {
    // This example assembles its tiny source directly; it is not a language compiler.
    // A real frontend supplies the same exact instruction-to-source mappings as a sidecar.
    let mut b = ProgramBuilder::new();
    b.set_program_name("Live receiver workflow");
    let read = b.host_sig_for(
        "Counter.read",
        HostSig {
            args: vec![ValueType::Obj(COUNTER)],
            rets: vec![ValueType::I64],
        },
    );
    let publish = b.host_sig_for(
        "Counter.publish",
        HostSig {
            args: vec![ValueType::Obj(COUNTER), ValueType::I64],
            rets: vec![],
        },
    );
    let double = b.declare_function(FunctionSig {
        arg_types: vec![ValueType::Obj(COUNTER)],
        ret_types: vec![ValueType::I64],
    })?;
    let mut a = b.function_asm(double)?;
    a.source(SourceId(0), source("read(receiver)"));
    let receiver = b.argument(double, 0)?;
    let [sample] = b.emit_call(&mut a, read, &[receiver])?;
    a.source(SourceId(0), source("sample + sample"));
    let answer = a.alloc_register(ValueType::I64);
    a.i64_add(answer, sample, sample);
    a.source(SourceId(0), source("return sample + sample;"));
    a.ret(0, &[answer]);
    let mut mappings: Vec<_> = a.source_mappings(double).collect();
    b.define_function(double, a)?;
    b.set_function_name(double, "double")?;
    let main = b.declare_function(FunctionSig {
        arg_types: vec![ValueType::Obj(COUNTER)],
        ret_types: vec![ValueType::Agg],
    })?;
    let mut a = b.function_asm(main)?;
    let receiver = b.argument(main, 0)?;
    a.source(SourceId(0), source("double(receiver)"));
    let [answer] = b.emit_call(&mut a, double, &[receiver])?;
    a.source(SourceId(0), source("publish(receiver, answer)"));
    let [] = b.emit_call(&mut a, publish, &[receiver, answer])?;
    a.source(SourceId(0), source("(receiver, answer)"));
    let tuple = a.alloc_register(ValueType::Agg);
    a.tuple_new(tuple, &[receiver, answer]);
    a.source(SourceId(0), source("return (receiver, answer);"));
    a.ret(0, &[tuple]);
    mappings.extend(a.source_mappings(main));
    b.define_function(main, a)?;
    b.set_function_name(main, "main")?;
    let program = b.build_verified()?;
    let sources = SourceMap::new(
        program.program(),
        vec![SourceDocument {
            name: "receiver.expr".into(),
            text: SOURCE.into(),
        }],
        mappings,
        SourceLimits::default(),
    )?;
    Ok((program, main, sources))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut steps = 1;
    let mut snapshot = false;
    let mut complete = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--snapshot" => snapshot = true,
            "--complete" => complete = true,
            "--steps" => {
                steps = args
                    .next()
                    .ok_or("--steps needs an instruction count")?
                    .parse()?
            }
            "--color=always" => ratatui::crossterm::style::force_color_output(true),
            "--help" | "-h" => {
                println!(
                    "live_debugger [--snapshot] [--steps N] [--complete] [--color=always]\n\nStarts inside double, before the first real host call.\ns: into  n: over  u: out  c: continue  Space: pause  b: breakpoint\nTab: pane  Enter: aggregate  Backspace: up  o: object roots  q: leave\n\n--snapshot renders 140 × 42 without terminal modes. --complete runs to completion first."
                );
                return Ok(());
            }
            _ => return Err(format!("unknown option {arg}; use --help").into()),
        }
    }
    let (p, entry, sources) = program()?;
    let mut vm = Vm::new(
        Counter {
            value: 21,
            reads: 0,
            published: Vec::new(),
        },
        Limits::default(),
    );
    let args = [Value::Obj(object())];
    let invocation = vm.start(&p, entry, &args, InvocationOptions::default())?;
    let mut view = LiveExplorer::new(invocation).sources(&sources)?;
    view.resume(if complete { u64::MAX } else { steps })?;
    if snapshot {
        print!("{}", view.render_text(140, 42)?);
    } else {
        view.run()?;
    }
    let returned = view.result().is_some_and(|result| result.is_ok());
    drop(view); // Cancels unfinished guest work; already-observed host effects remain.
    eprintln!(
        "{} · host reads: {} · published: {:?}",
        if returned { "Returned" } else { "Cancelled" },
        vm.host().reads,
        vm.host().published
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn live_source_receiver_workflow_returns_shared_object_and_published_answer() {
        let (p, entry, sources) = program().unwrap();
        let mut vm = Vm::new(
            Counter {
                value: 21,
                reads: 0,
                published: Vec::new(),
            },
            Limits::default(),
        );
        let args = [Value::Obj(object())];
        let invocation = vm
            .start(&p, entry, &args, InvocationOptions::default())
            .unwrap();
        let mut view = LiveExplorer::new(invocation).sources(&sources).unwrap();
        view.step().unwrap();
        let text = view.render_text(140, 42).unwrap();
        assert!(text.contains("Counter.read"));
        assert!(text.contains("read(receiver)"));
        assert!(text.contains("waiting for return"));
        view.resume(u64::MAX).unwrap();
        let output = view.take_outputs().unwrap();
        drop(view);
        let [Value::Agg(tuple)] = output.as_slice() else {
            panic!("expected rich result");
        };
        assert_eq!(
            vm.aggregates().tuple_get(*tuple, 0).unwrap(),
            Value::Obj(object())
        );
        assert_eq!(
            vm.aggregates().tuple_get(*tuple, 1).unwrap(),
            Value::I64(42)
        );
        assert_eq!(vm.host().reads, 1);
        assert_eq!(vm.host().published, [42]);
    }
}
