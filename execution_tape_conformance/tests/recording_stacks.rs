// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Owned guest stack observations through the ordinary interpreter and recorder.

use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
use execution_tape::host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef};
use execution_tape::program::ValueType;
use execution_tape::recording::{
    CaptureFault, CaptureResource, Outcome, RecordLimits, Recorder, Recording, ReplayOutcome,
};
use execution_tape::trace::{TraceMask, TraceSink};
use execution_tape::value::{FuncId, Value};
use execution_tape::verifier::VerifiedProgram;
use execution_tape::vm::{Limits, Vm};
use std::sync::Arc;

struct Echo {
    calls: usize,
    fail: bool,
}
impl Host for Echo {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        args: &[ValueRef<'_>],
        rets: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        self.calls += 1;
        rets[0] = args[0].to_value();
        if self.fail {
            Err(HostError::Failed)
        } else {
            Ok(0)
        }
    }
}

fn recursive() -> Arc<VerifiedProgram> {
    let mut b = ProgramBuilder::new();
    let f = b
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let h = b.host_sig_for(
        "echo",
        HostSig {
            args: vec![ValueType::I64],
            rets: vec![ValueType::I64],
        },
    );
    let mut a = Asm::new();
    a.set_register_types(vec![
        ValueType::Unit,
        ValueType::I64,
        ValueType::I64,
        ValueType::Bool,
        ValueType::I64,
        ValueType::I64,
    ]);
    let descend = a.label();
    let observe = a.label();
    a.const_i64(2, 0).i64_gt(3, 1, 2).br(3, descend, observe);
    a.place(descend).unwrap();
    a.const_i64(2, 1)
        .i64_sub(4, 1, 2)
        .call(0, f, 0, &[4], &[5])
        .jmp(observe);
    a.place(observe).unwrap();
    a.host_call_sig_id(0, h, 0, &[1], &[5]).ret(0, &[5]);
    b.define_function(f, a).unwrap();
    Arc::new(b.build_verified().unwrap())
}

fn record(
    p: &Arc<VerifiedProgram>,
    limits: Limits,
    budget: RecordLimits,
    fail: bool,
    calls: bool,
) -> (Recording, usize) {
    let mut vm = Vm::new(Echo { calls: 0, fail }, limits);
    let mut recorder = Recorder::new(Arc::clone(p), budget);
    let mask = if calls {
        recorder.mask()
    } else {
        TraceMask::RUN | TraceMask::VALUES
    };
    let _ = vm.run(p, FuncId(0), &[Value::I64(3)], mask, Some(&mut recorder));
    (recorder.finish(), vm.host().calls)
}

#[test]
fn recursive_boundaries_share_real_ancestry_after_the_vm_is_dropped() {
    let p = recursive();
    let (record, effects) = record(&p, Limits::default(), RecordLimits::default(), false, true);
    record.status().unwrap();
    assert_eq!(effects, 4);
    assert_eq!(record.stats().call_frames, 4);
    let source = execution_tape::disasm::disassemble_function(p.program(), FuncId(0)).unwrap();
    let call_pc = source.instrs().find(|i| matches!(i.operands(), execution_tape::disasm::Operands::Call(call) if matches!(call.callee, execution_tape::disasm::CallTarget::Func(_)))).unwrap().pc();
    for (ordinal, call) in record.host_calls().enumerate() {
        let mut stack = call.stack.unwrap();
        assert_eq!(stack.len(), 4 - ordinal);
        assert_eq!(stack.next(), Some(call.location));
        assert!(stack.all(|site| site.func == FuncId(0) && site.pc == call_pc));
    }
    let mut stack = record.outcome_stack().unwrap();
    assert_eq!(stack.len(), 1);
    assert_eq!(stack.next(), record.outcome_location());
    assert_eq!(stack.len(), 0);
    assert_eq!(stack.next(), None);
    assert_eq!(stack.next(), None);
    assert!(matches!(
        record.replay().unwrap(),
        ReplayOutcome::Returned(_)
    ));
}

#[test]
fn first_trapping_exit_preserves_the_leaf_stack_before_unwinding() {
    let p = recursive();
    let (record, effects) = record(&p, Limits::default(), RecordLimits::default(), true, true);
    record.status().unwrap();
    assert_eq!(effects, 1);
    assert!(matches!(record.outcome(), Some(Outcome::Trapped(_))));
    let call = record.host_calls().next().unwrap();
    assert_eq!(
        call.stack.unwrap().collect::<Vec<_>>(),
        record.outcome_stack().unwrap().collect::<Vec<_>>()
    );
    assert_eq!(record.outcome_stack().unwrap().len(), 4);
    assert!(matches!(
        record.replay().unwrap(),
        ReplayOutcome::Trapped(_)
    ));
}

#[test]
fn call_depth_denial_does_not_invent_an_entered_callee() {
    let p = recursive();
    let (record, effects) = record(
        &p,
        Limits {
            max_call_depth: 2,
            ..Limits::default()
        },
        RecordLimits::default(),
        false,
        true,
    );
    record.status().unwrap();
    assert_eq!(effects, 0);
    assert_eq!(record.stats().call_frames, 2);
    assert_eq!(record.outcome_stack().unwrap().len(), 2);
    assert_eq!(
        record.outcome_stack().unwrap().next(),
        record.outcome_location()
    );
    assert!(matches!(
        record.replay().unwrap(),
        ReplayOutcome::Trapped(_)
    ));
}

#[test]
fn rejected_entry_and_fuel_exhaustion_have_different_observed_stacks() {
    let p = recursive();
    for (limits, expected) in [
        (
            Limits {
                max_call_depth: 0,
                ..Limits::default()
            },
            None,
        ),
        (
            Limits {
                fuel: 0,
                ..Limits::default()
            },
            Some(1),
        ),
    ] {
        let (record, effects) = record(&p, limits, RecordLimits::default(), false, true);
        record.status().unwrap();
        assert_eq!(effects, 0);
        assert_eq!(record.outcome_stack().map(|stack| stack.len()), expected);
        assert!(matches!(
            record.replay().unwrap(),
            ReplayOutcome::Trapped(_)
        ));
    }
}

#[test]
fn activation_budget_is_explicit_and_does_not_change_live_execution() {
    let p = recursive();
    let (record, effects) = record(
        &p,
        Limits::default(),
        RecordLimits {
            call_frames: 2,
            ..RecordLimits::default()
        },
        false,
        true,
    );
    assert!(matches!(
        record.status(),
        Err(CaptureFault::Budget {
            resource: CaptureResource::CallFrames,
            limit: 2
        })
    ));
    assert_eq!(effects, 4);
    assert_eq!(record.stats().call_frames, 2);
    assert!(record.outcome_stack().is_none());
    assert!(record.replay().is_err());
}

#[test]
fn missing_call_callbacks_leave_stacks_unavailable_without_inference() {
    let p = recursive();
    let (record, effects) = record(&p, Limits::default(), RecordLimits::default(), false, false);
    record.status().unwrap();
    assert_eq!(effects, 4);
    assert_eq!(record.stats().call_frames, 0);
    assert!(record.host_calls().all(|call| call.stack.is_none()));
    assert!(record.outcome_stack().is_none());
    assert!(matches!(
        record.replay().unwrap(),
        ReplayOutcome::Returned(_)
    ));
}

#[test]
fn later_capture_failure_keeps_prior_stacks_but_not_a_stale_final_stack() {
    let p = recursive();
    let (record, effects) = record(
        &p,
        Limits::default(),
        RecordLimits {
            host_calls: 1,
            ..RecordLimits::default()
        },
        false,
        true,
    );
    assert!(record.status().is_err());
    assert_eq!(effects, 4);
    assert_eq!(record.host_calls().next().unwrap().stack.unwrap().len(), 4);
    assert!(record.outcome_stack().is_none());
}
