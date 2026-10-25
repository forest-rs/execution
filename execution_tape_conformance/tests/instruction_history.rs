// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Observed paths, independently bounded from boundary recording and checked by replay.

use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
use execution_tape::host::{Host, HostContext, HostError, SigHash, ValueRef};
use execution_tape::opcode::Opcode;
use execution_tape::program::ValueType;
use execution_tape::recording::{
    InstructionCapture, InstructionOutcome, InstructionStatus, RecordLimits, Recorder,
    ReplayOutcome,
};
use execution_tape::trace::{TraceMask, TraceSink};
use execution_tape::value::{FuncId, Value};
use execution_tape::verifier::VerifiedProgram;
use execution_tape::vm::{Limits, Vm};
use std::sync::Arc;

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
        Err(HostError::UnknownSymbol)
    }
}
fn fixture() -> Arc<VerifiedProgram> {
    let mut b = ProgramBuilder::new();
    let child = b
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let mut a = Asm::new();
    a.set_register_types(vec![ValueType::Unit, ValueType::I64, ValueType::I64]);
    a.i64_add(2, 1, 1).ret(0, &[2]);
    b.define_function(child, a).unwrap();
    let mut a = Asm::new();
    a.set_register_types(vec![
        ValueType::Unit,
        ValueType::Bool,
        ValueType::I64,
        ValueType::I64,
    ]);
    let yes = a.label();
    let no = a.label();
    let join = a.block(&[2]);
    a.br(1, yes, no);
    a.place(yes).unwrap();
    a.const_i64(2, 21).jmp(join.with_args(&[2]));
    a.place(no).unwrap();
    a.const_i64(2, 7).jmp(join.with_args(&[2]));
    a.place(join).unwrap();
    a.call(0, child, 0, &[2], &[3]).ret(0, &[3]);
    b.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![ValueType::Bool],
            ret_types: vec![ValueType::I64],
        },
    )
    .unwrap();
    Arc::new(b.build_verified().unwrap())
}
fn capture(
    condition: bool,
    max_steps: usize,
    fuel: u64,
    instructions: bool,
) -> execution_tape::recording::Recording {
    let p = fixture();
    let mut recorder = Recorder::new(Arc::clone(&p), RecordLimits::default()).with_instructions(
        InstructionCapture {
            max_steps,
            ..InstructionCapture::default()
        },
    );
    let mut vm = Vm::new(
        NoHost,
        Limits {
            fuel,
            ..Limits::default()
        },
    );
    let mask = if instructions {
        recorder.mask()
    } else {
        TraceMask::RUN | TraceMask::VALUES | TraceMask::CALL
    };
    let _ = vm.run(
        &p,
        FuncId(1),
        &[Value::Bool(condition)],
        mask,
        Some(&mut recorder),
    );
    recorder.finish()
}
#[test]
fn actual_branches_calls_returns_and_zero_cost_headers_are_distinguished() {
    for (condition, expected) in [(true, 42), (false, 14)] {
        let record = capture(condition, 100, 100, true);
        assert_eq!(record.status(), Ok(()));
        assert_eq!(
            record.instruction_status(),
            Some(InstructionStatus::Complete)
        );
        let steps: Vec<_> = record.instructions().collect();
        assert_eq!(steps.len(), 7);
        for pair in steps.windows(2) {
            assert_eq!(pair[0].next, Some(pair[1].location));
        }
        assert_eq!(steps[0].opcode, Opcode::Br.byte());
        assert_eq!(steps[3].opcode, Opcode::Call.byte());
        assert_eq!(steps[3].next.unwrap().func, FuncId(0));
        assert_eq!(steps[4].stack.as_ref().unwrap().len(), 2);
        assert_eq!(steps[5].opcode, Opcode::Ret.byte());
        assert_eq!(steps[5].next.unwrap().func, FuncId(1));
        assert_eq!(steps[5].stack.as_ref().unwrap().len(), 2);
        assert_eq!(steps[6].next, None);
        assert!(steps.iter().all(
            |s| s.outcome == InstructionOutcome::Completed && s.opcode != Opcode::Block.byte()
        ));
        assert!(record.instruction(7).is_none());
        let ReplayOutcome::Returned(replayed) = record.replay().unwrap() else {
            panic!("expected return");
        };
        assert!(matches!(replayed.values(), [Value::I64(value)] if *value == expected));
    }
    assert_ne!(
        capture(true, 100, 100, true).instruction(0).unwrap().next,
        capture(false, 100, 100, true).instruction(0).unwrap().next
    );
}
#[test]
fn bounded_prefix_preserves_complete_boundary_capture_and_replay() {
    for max_steps in [0, 1, 3, 6] {
        let record = capture(true, max_steps, 100, true);
        assert_eq!(record.instructions().len(), max_steps);
        assert_eq!(
            record.instruction_status(),
            Some(InstructionStatus::Truncated { limit: max_steps })
        );
        assert_eq!(record.status(), Ok(()));
        assert!(
            record
                .instructions()
                .all(|s| s.outcome == InstructionOutcome::Completed)
        );
        record.replay().unwrap();
    }
}
#[test]
fn exhaustion_before_an_attempt_is_not_an_instruction_trap() {
    for fuel in [0, 2] {
        let record = capture(true, 100, fuel, true);
        assert_eq!(
            record.instruction_status(),
            Some(InstructionStatus::Complete)
        );
        assert_eq!(record.instructions().len(), usize::try_from(fuel).unwrap());
        assert!(
            record
                .instructions()
                .all(|s| s.outcome == InstructionOutcome::Completed)
        );
        assert!(matches!(
            record.replay().unwrap(),
            ReplayOutcome::Trapped(_)
        ));
    }
}
#[test]
fn omitted_instruction_callbacks_are_explicitly_incomplete() {
    let record = capture(true, 100, 100, false);
    assert_eq!(record.status(), Ok(()));
    assert_eq!(
        record.instruction_status(),
        Some(InstructionStatus::Incomplete)
    );
    assert_eq!(record.instructions().len(), 0);
    record.replay().unwrap();
}
#[test]
fn normal_recorders_have_no_instruction_history() {
    let recorder = Recorder::new(fixture(), RecordLimits::default());
    assert!(!recorder.mask().contains(TraceMask::INSTR));
    let record = recorder.finish();
    assert_eq!(record.instruction_status(), None);
    assert_eq!(record.stats().instructions, 0);
}

fn watches(program: &VerifiedProgram) -> Vec<execution_tape::recording::RegisterWatch> {
    (0..program.program().functions.len())
        .flat_map(|index| {
            let func = FuncId(u32::try_from(index).unwrap());
            let source =
                execution_tape::disasm::disassemble_function(program.program(), func).unwrap();
            source
                .instrs()
                .map(|i| execution_tape::recording::RegisterWatch {
                    location: execution_tape::trace::TraceLocation { func, pc: i.pc() },
                    registers: i.reads().chain(i.writes()).filter(|&r| r != 0).collect(),
                })
                .collect::<Vec<_>>()
        })
        .collect()
}
#[test]
fn watched_inputs_local_writes_and_call_dispatch_have_honest_boundaries() {
    let p = fixture();
    let mut selected = watches(&p);
    selected.reverse();
    let duplicate = selected
        .iter()
        .find(|watch| watch.location.func == FuncId(1) && watch.location.pc == 0)
        .unwrap()
        .clone();
    selected.push(execution_tape::recording::RegisterWatch {
        registers: vec![0, 1, 1],
        ..duplicate
    });
    let config = InstructionCapture {
        max_steps: 100,
        watches: selected,
    };
    let mut recorder =
        Recorder::new(Arc::clone(&p), RecordLimits::default()).with_instructions(config);
    let mut vm = Vm::new(NoHost, Limits::default());
    vm.run(
        &p,
        FuncId(1),
        &[Value::Bool(false)],
        recorder.mask(),
        Some(&mut recorder),
    )
    .unwrap();
    let record = recorder.finish();
    assert_eq!(
        record.instruction_status(),
        Some(InstructionStatus::Complete)
    );
    assert_eq!(record.status(), Ok(()));
    let branch = record.instruction(0).unwrap();
    assert!(matches!(
        branch.inputs.iter().collect::<Vec<_>>().as_slice(),
        [(1, execution_tape::recording::RecordedValue::Bool(false))]
    ));
    let constant = record.instruction(1).unwrap();
    assert!(constant.inputs.is_empty());
    assert!(matches!(
        constant.outputs.iter().collect::<Vec<_>>().as_slice(),
        [(2, execution_tape::recording::RecordedValue::I64(7))]
    ));
    let call = record.instruction(3).unwrap();
    assert!(call.outputs.is_empty()); // caller r3 was allocated but is not initialized yet
    let add = record.instruction(4).unwrap();
    assert_eq!(add.inputs.len(), 2); // repeated read occurrence is preserved
    assert!(matches!(
        add.outputs.iter().collect::<Vec<_>>().as_slice(),
        [(2, execution_tape::recording::RecordedValue::I64(14))]
    ));
    assert!(record.instruction(5).unwrap().outputs.is_empty());
    record.replay().unwrap();
}
#[test]
fn trapping_instruction_has_inputs_but_never_successful_writes() {
    let mut b = ProgramBuilder::new();
    let mut a = Asm::new();
    a.set_register_types(vec![
        ValueType::Unit,
        ValueType::I64,
        ValueType::I64,
        ValueType::I64,
    ]);
    a.const_i64(2, 0).i64_div(3, 1, 2).ret(0, &[3]);
    b.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::I64],
        },
    )
    .unwrap();
    let p = Arc::new(b.build_verified().unwrap());
    let mut recorder = Recorder::new(Arc::clone(&p), RecordLimits::default()).with_instructions(
        InstructionCapture {
            max_steps: 10,
            watches: watches(&p),
        },
    );
    let mut vm = Vm::new(NoHost, Limits::default());
    assert!(
        vm.run(
            &p,
            FuncId(0),
            &[Value::I64(42)],
            recorder.mask(),
            Some(&mut recorder)
        )
        .is_err()
    );
    let record = recorder.finish();
    assert_eq!(
        record.instruction_status(),
        Some(InstructionStatus::Complete)
    );
    let div = record.instruction(1).unwrap();
    assert_eq!(div.outcome, InstructionOutcome::Trapped);
    assert_eq!(div.next, None);
    assert_eq!(div.inputs.len(), 2);
    assert!(div.outputs.is_empty());
    assert!(matches!(
        record.replay().unwrap(),
        ReplayOutcome::Trapped(_)
    ));
}
#[test]
fn aliased_aggregate_watches_preserve_before_after_graph_identity() {
    let mut b = ProgramBuilder::new();
    let mut a = Asm::new();
    a.set_register_types(vec![
        ValueType::Unit,
        ValueType::Str,
        ValueType::Agg,
        ValueType::Agg,
    ]);
    a.tuple_new(2, &[1])
        .tuple_new(3, &[2])
        .tuple_get(3, 3, 0)
        .tuple_get(1, 3, 0)
        .ret(0, &[1]);
    b.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![ValueType::Str],
            ret_types: vec![ValueType::Str],
        },
    )
    .unwrap();
    let p = Arc::new(b.build_verified().unwrap());
    let mut recorder = Recorder::new(Arc::clone(&p), RecordLimits::default()).with_instructions(
        InstructionCapture {
            max_steps: 10,
            watches: watches(&p),
        },
    );
    let mut vm = Vm::new(NoHost, Limits::default());
    vm.run(
        &p,
        FuncId(0),
        &[Value::Str("retained text".into())],
        recorder.mask(),
        Some(&mut recorder),
    )
    .unwrap();
    drop(vm);
    let record = recorder.finish();
    let project = record.instruction(2).unwrap();
    let (_, execution_tape::recording::RecordedValue::Agg(before)) =
        project.inputs.iter().next().unwrap()
    else {
        panic!("before aggregate");
    };
    let (_, execution_tape::recording::RecordedValue::Agg(after)) =
        project.outputs.iter().next().unwrap()
    else {
        panic!("after aggregate");
    };
    assert_ne!(before, after);
    assert!(
        matches!(record.aggregate(*before).unwrap().values, [execution_tape::recording::RecordedValue::Agg(child)] if child == after)
    );
    assert!(
        matches!(record.aggregate(*after).unwrap().values, [execution_tape::recording::RecordedValue::Str(text)] if &**text == "retained text")
    );
    record.replay().unwrap();
}

#[test]
fn omitted_watch_callbacks_do_not_claim_complete_value_history() {
    let p = fixture();
    let mut recorder = Recorder::new(Arc::clone(&p), RecordLimits::default()).with_instructions(
        InstructionCapture {
            max_steps: 100,
            watches: watches(&p),
        },
    );
    let mut vm = Vm::new(NoHost, Limits::default());
    vm.run(
        &p,
        FuncId(1),
        &[Value::Bool(true)],
        TraceMask::RUN | TraceMask::VALUES | TraceMask::CALL | TraceMask::INSTR,
        Some(&mut recorder),
    )
    .unwrap();
    let record = recorder.finish();
    assert_eq!(record.status(), Ok(()));
    assert_eq!(
        record.instruction_status(),
        Some(InstructionStatus::Incomplete)
    );
    record.replay().unwrap();
}

#[test]
fn replay_rejects_a_retained_successor_that_was_not_executed() {
    use execution_tape::aggregates::AggHeap;
    use execution_tape::recording::{Divergence, ReplayBoundary, ReplayError};
    use execution_tape::trace::{TraceLocation, TraceOutcome, ValueEvent};
    let mut b = ProgramBuilder::new();
    let mut a = Asm::new();
    a.set_register_types(vec![ValueType::Unit, ValueType::I64]);
    a.const_i64(1, 42);
    let ret = a.pc();
    a.ret(0, &[1]);
    b.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
        },
    )
    .unwrap();
    let p = Arc::new(b.build_verified().unwrap());
    let mut recorder = Recorder::new(Arc::clone(&p), RecordLimits::default())
        .with_instructions(InstructionCapture::default());
    let heap = AggHeap::new();
    let limits = Limits::default();
    let location = TraceLocation {
        func: FuncId(0),
        pc: 0,
    };
    recorder.run_start(p.program(), FuncId(0), 0);
    recorder.values(
        p.program(),
        ValueEvent::Entry {
            entry: FuncId(0),
            args: &[],
            aggregates: &heap,
            limits: &limits,
        },
    );
    recorder.instr(
        p.program(),
        FuncId(0),
        0,
        ret,
        None,
        Opcode::ConstI64.byte(),
    );
    recorder.instr_end(p.program(), location, Some(location), TraceOutcome::Ok); // false self-loop
    recorder.values(
        p.program(),
        ValueEvent::ReturnCandidate {
            location: TraceLocation {
                func: FuncId(0),
                pc: ret,
            },
            values: &[Value::I64(42)],
            aggregates: &heap,
        },
    );
    recorder.run_end(p.program(), TraceOutcome::Ok);
    let record = recorder.finish();
    assert_eq!(record.status(), Ok(()));
    assert!(matches!(
        record.replay(),
        Err(ReplayError::Diverged {
            boundary: ReplayBoundary::Instruction { index: 0 },
            reason: Divergence::Instruction,
            ..
        })
    ));
}

#[test]
fn zero_budget_does_not_hide_disabled_instruction_callbacks_on_a_trap() {
    let mut b = ProgramBuilder::new();
    let mut a = Asm::new();
    a.set_register_types(vec![ValueType::Unit]);
    a.trap(7);
    b.push_function_checked(
        a,
        FunctionSig {
            arg_types: vec![],
            ret_types: vec![],
        },
    )
    .unwrap();
    let p = Arc::new(b.build_verified().unwrap());
    for enabled in [false, true] {
        let mut recorder = Recorder::new(Arc::clone(&p), RecordLimits::default())
            .with_instructions(InstructionCapture {
                max_steps: 0,
                ..InstructionCapture::default()
            });
        let mut vm = Vm::new(NoHost, Limits::default());
        let mask = if enabled {
            recorder.mask()
        } else {
            TraceMask::RUN | TraceMask::VALUES | TraceMask::CALL
        };
        assert!(
            vm.run(&p, FuncId(0), &[], mask, Some(&mut recorder))
                .is_err()
        );
        let record = recorder.finish();
        assert_eq!(record.status(), Ok(()));
        assert_eq!(
            record.instruction_status(),
            Some(if enabled {
                InstructionStatus::Truncated { limit: 0 }
            } else {
                InstructionStatus::Incomplete
            })
        );
        record.replay().unwrap();
    }
}

#[test]
fn rejected_entry_is_complete_zero_attempt_history_when_subscriptions_are_enabled() {
    let p = fixture();
    for max_steps in [0, 100] {
        let mut recorder = Recorder::new(Arc::clone(&p), RecordLimits::default())
            .with_instructions(InstructionCapture {
                max_steps,
                ..InstructionCapture::default()
            });
        let mut vm = Vm::new(NoHost, Limits::default());
        assert!(
            vm.run(&p, FuncId(1), &[], recorder.mask(), Some(&mut recorder))
                .is_err()
        );
        let record = recorder.finish();
        assert_eq!(record.status(), Ok(()));
        assert_eq!(
            record.instruction_status(),
            Some(InstructionStatus::Complete)
        );
        assert_eq!(record.instructions().len(), 0);
        assert!(matches!(
            record.replay().unwrap(),
            ReplayOutcome::Trapped(_)
        ));
    }
}
