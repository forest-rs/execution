// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Record a read/compute/store workflow, then explore it after dropping the live VM and host.

use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
use execution_tape::host::{HostContext, HostError, HostSig, ValueRef};
use execution_tape::host_binding::HostFunctions;
use execution_tape::program::ValueType;
use execution_tape::trace::TraceSink;
use execution_tape::value::{FuncId, Value};
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
    let mut body = Asm::new();
    body.set_register_types(vec![
        ValueType::Unit,
        ValueType::I64,
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
        .const_i64(5, 2)
        .const_i64(8, 0)
        .const_i64(10, 1)
        .jmp(head);
    body.place(head)?;
    body.host_call_sig_id(0, read, 0, &[1], &[3]);
    body.i64_mul(4, 3, 5).tuple_new(6, &[1, 3, 4]);
    body.host_call_sig_id(0, store, 0, &[6], &[7]);
    body.br(7, accepted, rejected);
    body.place(accepted)?;
    body.i64_add(8, 8, 4)
        .i64_add(1, 1, 10)
        .i64_lt(9, 1, 2)
        .br(9, head, done);
    body.place(rejected)?;
    body.trap(1);
    body.place(done)?;
    body.ret(0, &[8]);
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
    let output = vm.run(
        &program,
        FuncId(0),
        &[],
        recorder.mask(),
        Some(&mut recorder),
    )?;
    if output != [Value::I64(42)] || vm.host().state() != &[[0, 7, 14], [1, 5, 10], [2, 9, 18]] {
        return Err("demo did not produce the expected stored samples and sum".into());
    }
    let record = recorder.finish();
    record.status().map_err(Clone::clone)?;
    drop(vm);
    Ok(record)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if !args.is_empty() && args != ["--snapshot"] {
        return Err("usage: observatory [--snapshot]".into());
    }
    let record = demo_recording()?;
    if args.is_empty() {
        execution_tape_observatory::explore(&record)?;
    } else {
        print!(
            "{}",
            execution_tape_observatory::render_text(&record, 120, 36)?
        );
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
    }
}
