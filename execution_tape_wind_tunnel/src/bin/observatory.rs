// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Browse a compiled tape, or record and explore a read/compute/store demonstration.

#[path = "observatory/cli.rs"]
mod cli;

use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
use execution_tape::host::{HostContext, HostError, HostSig, ValueRef};
use execution_tape::host_binding::HostFunctions;
use execution_tape::program::ValueType;
use execution_tape::trace::TraceSink;
use execution_tape::value::Value;
use execution_tape::vm::{Limits, Vm};
use execution_tape_recording::{RecordLimits, Recorder, Recording};
use std::sync::Arc;

type Stored = Vec<[i64; 3]>;
const SAMPLES: [i64; 3] = [7, 5, 9];

fn read_sample(
    _: &mut Stored,
    args: &[ValueRef<'_>],
    rets: &mut [Value],
    _: HostContext<'_, '_>,
) -> Result<u64, HostError> {
    let [ValueRef::I64(index)] = args else {
        return Err(HostError::Failed);
    };
    let sample = usize::try_from(*index)
        .ok()
        .and_then(|index| SAMPLES.get(index))
        .ok_or(HostError::Failed)?;
    rets[0] = Value::I64(*sample);
    Ok(0)
}

fn store_result(
    stored: &mut Stored,
    args: &[ValueRef<'_>],
    rets: &mut [Value],
    ctx: HostContext<'_, '_>,
) -> Result<u64, HostError> {
    let [ValueRef::Agg(tuple)] = args else {
        return Err(HostError::Failed);
    };
    let fields = [
        ctx.tuple_get_ref(*tuple, 0),
        ctx.tuple_get_ref(*tuple, 1),
        ctx.tuple_get_ref(*tuple, 2),
    ];
    let [
        Ok(ValueRef::I64(index)),
        Ok(ValueRef::I64(raw)),
        Ok(ValueRef::I64(scaled)),
    ] = fields
    else {
        return Err(HostError::Failed);
    };
    // Copy the scalar fields consumed by this call; do not keep its borrowed heap handle.
    stored.push([index, raw, scaled]);
    rets[0] = Value::Bool(true);
    Ok(0)
}

fn demo_recording() -> Result<Recording, Box<dyn std::error::Error>> {
    let read = HostSig {
        args: vec![ValueType::I64],
        rets: vec![ValueType::I64],
    };
    let store = HostSig {
        args: vec![ValueType::Agg],
        rets: vec![ValueType::Bool],
    };
    let mut functions = HostFunctions::new();
    functions.register("read_sample", read.clone(), read_sample)?;
    functions.register("store_result", store.clone(), store_result)?;
    let mut builder = ProgramBuilder::new();
    builder.set_program_name("Double and sum three samples");
    let read = builder.host_sig_for("read_sample", read);
    let store = builder.host_sig_for("store_result", store);
    let mut sample = Asm::new();
    sample.set_register_types(vec![
        ValueType::Unit,
        ValueType::I64,
        ValueType::I64,
        ValueType::I64,
        ValueType::I64,
    ]);
    sample.host_call_sig_id(0, read, 0, &[1], &[2]);
    sample.const_i64(3, 2).i64_mul(4, 2, 3).ret(0, &[2, 4]);
    let sample = builder.push_function_checked(
        sample,
        FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::I64, ValueType::I64],
        },
    )?;
    builder.set_function_name(sample, "read_and_double")?;
    let mut body = Asm::new();
    body.set_register_types(vec![
        ValueType::Unit,
        ValueType::I64,
        ValueType::I64,
        ValueType::I64,
        ValueType::I64,
        ValueType::Agg,
        ValueType::Bool,
        ValueType::I64,
        ValueType::Bool,
        ValueType::I64,
    ]);
    let head = body.label();
    let accepted = body.label();
    let rejected = body.label();
    let done = body.label();
    body.const_i64(1, 0)
        .const_i64(2, 3)
        .const_i64(7, 0)
        .const_i64(9, 1)
        .jmp(head);
    body.place(head)?;
    body.call(0, sample, 0, &[1], &[3, 4])
        .tuple_new(5, &[1, 3, 4]);
    body.host_call_sig_id(0, store, 0, &[5], &[6])
        .br(6, accepted, rejected);
    body.place(accepted)?;
    body.i64_add(7, 7, 4)
        .i64_add(1, 1, 9)
        .i64_lt(8, 1, 2)
        .br(8, head, done);
    body.place(rejected)?;
    body.trap(1);
    body.place(done)?;
    body.ret(0, &[7]);
    let entry = builder.push_function_checked(
        body,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
        },
    )?;
    builder.set_function_name(entry, "double_and_sum")?;
    let program = Arc::new(builder.build_verified()?);
    let bindings = functions.bind(&program)?;
    let mut vm = Vm::new(bindings.with_state(Stored::new()), Limits::default());
    let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
    let output = vm.run(&program, entry, &[], recorder.mask(), Some(&mut recorder))?;
    if output != [Value::I64(42)] || vm.host().state() != &[[0, 7, 14], [1, 5, 10], [2, 9, 18]] {
        return Err("demo did not produce the expected stored samples and sum".into());
    }
    let record = recorder.finish();
    record.status().map_err(Clone::clone)?;
    drop(vm);
    Ok(record)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    use cli::{Action, Options};
    use std::io::Write;
    let options = Options::parse(std::env::args_os().skip(1))?;
    if options.force_colors && !options.snapshot {
        ratatui::crossterm::style::force_color_output(true);
    }
    match options.action {
        Action::Help => print!("{}", cli::HELP),
        Action::Program(path) => {
            let program = cli::load_program(&path)?;
            if options.snapshot {
                print!(
                    "{}",
                    execution_tape_observatory::render_program(&program, 120, 36)?
                );
            } else {
                execution_tape_observatory::browse(&program)?;
            }
        }
        Action::WriteDemo(path) => {
            let record = demo_recording()?;
            let bytes = record.program().program().encode();
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .map_err(|error| format!("cannot create demo tape {path:?}: {error}"))?;
            file.write_all(&bytes)?;
            println!("Wrote {} bytes to {path:?}", bytes.len());
        }
        Action::Demo => {
            let record = demo_recording()?;
            if options.snapshot {
                print!(
                    "{}",
                    execution_tape_observatory::render_text(&record, 120, 36)?
                );
            } else {
                execution_tape_observatory::explore(&record)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use execution_tape_recording::{Outcome, ReplayOutcome};

    #[test]
    fn real_read_compute_store_workflow_replays_after_host_state_is_dropped() {
        let record = demo_recording().unwrap();
        let calls: Vec<_> = record.host_calls().collect();
        assert_eq!(calls.len(), 6);
        assert_eq!(record.stats().call_frames, 4);
        assert_eq!(calls[0].stack.as_ref().unwrap().len(), 2);
        assert_eq!(calls[1].stack.as_ref().unwrap().len(), 1);
        assert_ne!(calls[0].import, calls[1].import);
        assert_ne!(calls[0].location, calls[1].location);
        assert_eq!(calls[0].location, calls[2].location);
        assert!(matches!(
            record.outcome(),
            Some(Outcome::Returned([
                execution_tape_recording::RecordedValue::I64(42)
            ]))
        ));
        let ReplayOutcome::Returned(output) = record.replay().unwrap() else {
            panic!("expected independent replay sum")
        };
        assert_eq!(output.values(), [Value::I64(42)]);
        let screen = execution_tape_observatory::render_text(&record, 80, 24).unwrap();
        assert!(screen.contains("Double and sum three samples"));
        assert!(screen.contains("returned 42"));
        assert!(screen.contains("6 host calls"));
        assert!(screen.contains("read_sample(0) returned 7"));
        assert!(screen.contains("read_sample(i64) → (i64)"));
        assert!(screen.contains("CALL VALUES · read_sample"));
        assert!(screen.contains("→ Argument[0]"));
        assert!(screen.contains("← Return[0]"));
        assert!(screen.contains("0 · i64"));
        assert!(screen.contains("7 · i64"));
        assert!(screen.contains("host read_sample(r1)"));
        assert!(screen.contains("Application → double_and_sum() · VM invocation"));
        assert!(screen.contains("App → double_and_sum"));
        assert!(screen.contains("root ← Application"));
        assert!(screen.contains("STACK · 2"));
        assert!(screen.contains("read_and_double"));
        assert!(screen.contains("double_and_sum"));
    }
}
