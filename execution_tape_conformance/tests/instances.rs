// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Code definitions, host namespaces and VM stores have distinct identities.

use execution_tape::{
    asm::{FunctionSig, ProgramBuilder},
    host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef},
    instance::{Instance, InstanceError},
    program::{Program, ValueType},
    recording::{RecordLimits, Recorder, ReplayOutcome},
    trace::{TraceMask, TraceSink},
    value::{FuncId, Value},
    verifier::VerifiedProgram,
    vm::{InvocationOptions, Limits, RunStatus, Vm},
};
use std::{collections::HashMap, sync::Arc};

#[derive(Default)]
struct Counters(HashMap<Instance, u64>);
impl Host for Counters {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        _: &[ValueRef<'_>],
        out: &mut [Value],
        ctx: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        assert_eq!(
            ctx.instance().program().program(),
            ctx.program(),
            "host code and namespace must belong to the same instance"
        );
        let count = self.0.entry(ctx.instance().clone()).or_default();
        *count += 1;
        out[0] = Value::U64(*count);
        Ok(0)
    }
}

fn program(name: &str) -> (VerifiedProgram, FuncId) {
    let mut builder = ProgramBuilder::new();
    builder.set_program_name(name);
    let next = builder.host_sig_for(
        "next",
        HostSig {
            args: vec![],
            rets: vec![ValueType::U64],
        },
    );
    let entry = builder
        .declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::U64],
        })
        .unwrap();
    let mut body = builder.function_asm(entry).unwrap();
    let [count] = builder.emit_call(&mut body, next, &[]).unwrap();
    body.ret(0, &[count]);
    builder.define_function(entry, body).unwrap();
    (builder.build_verified().unwrap(), entry)
}

fn run(vm: &mut Vm<Counters>, instance: &Instance, entry: FuncId) -> u64 {
    let mut call = instance
        .start(vm, entry, &[], InvocationOptions::default())
        .unwrap();
    assert_eq!(
        call.instance(),
        Some(instance),
        "entry retains its namespace"
    );
    assert_eq!(
        call.resume(u64::MAX).unwrap(),
        RunStatus::Complete,
        "counter returns without another scheduling boundary"
    );
    let [Value::U64(count)] = call.result().unwrap().unwrap() else {
        panic!("counter")
    };
    *count
}

#[test]
fn shared_code_can_have_distinct_host_namespaces_in_one_store() {
    let (program, entry) = program("counter");
    let mut vm = Vm::new(Counters::default(), Limits::default());
    let first = vm.instantiate(&program).unwrap();
    let second = vm.instantiate(&program.clone()).unwrap();
    assert_ne!(first, second);
    assert_eq!(first, first.clone());
    assert!(first.program().shares_definition_with(second.program()));
    assert!(vm.owns_instance(&first));
    assert_eq!(run(&mut vm, &first, entry), 1);
    assert_eq!(run(&mut vm, &second, entry), 1);
    assert_eq!(run(&mut vm, &first, entry), 2);
    assert_eq!(vm.host().0.len(), 2);
}

#[test]
fn default_namespaces_survive_switching_between_shared_definitions() {
    let (first, entry) = program("first");
    let (second, other_entry) = program("second");
    let mut vm = Vm::new(Counters::default(), Limits::default());
    assert_eq!(
        vm.run(&first, entry, &[], TraceMask::NONE, None).unwrap(),
        [Value::U64(1)]
    );
    assert_eq!(
        vm.run(&second, other_entry, &[], TraceMask::NONE, None)
            .unwrap(),
        [Value::U64(1)]
    );
    assert_eq!(
        vm.run(&first.clone(), entry, &[], TraceMask::NONE, None)
            .unwrap(),
        [Value::U64(2)]
    );
    assert_eq!(vm.host().0.len(), 2);
}

#[test]
fn foreign_store_denial_precedes_entry_observation_and_host_effects() {
    struct NoEntry;
    impl TraceSink for NoEntry {
        fn run_start(&mut self, _: &Program, _: FuncId, _: usize) {
            panic!("foreign entry was observed")
        }
    }
    let (program, entry) = program("same code");
    let mut source = Vm::new(Counters::default(), Limits::default());
    let foreign = source.instantiate(&program).unwrap();
    drop(source);
    let mut target = Vm::new(Counters::default(), Limits::default());
    let local = target.instantiate(&program).unwrap();
    assert!(!target.owns_instance(&foreign));
    assert!(local.program().shares_definition_with(foreign.program()));
    assert!(matches!(
        foreign.start(
            &mut target,
            entry,
            &[],
            InvocationOptions::default().trace(TraceMask::RUN, &mut NoEntry)
        ),
        Err(InstanceError::ForeignStore)
    ));
    assert!(target.host().0.is_empty());
    assert_eq!(run(&mut target, &local, entry), 1);
}

#[test]
fn paused_entry_and_pending_request_keep_the_selected_instance() {
    let (program, entry) = program("selected");
    let mut vm = Vm::new(Counters::default(), Limits::default());
    let mut recording = Recorder::new(Arc::new(program.clone()), RecordLimits::default());
    let selected = vm.instantiate(&program).unwrap();
    let expected = selected.clone();
    let mut call = selected
        .start(
            &mut vm,
            entry,
            &[],
            InvocationOptions::default()
                .yield_host_calls()
                .trace(recording.mask(), &mut recording),
        )
        .unwrap();
    drop(selected);
    drop(program);
    assert_eq!(call.resume(1).unwrap(), RunStatus::AwaitingHost);
    let request = call.pending_host_call().unwrap();
    assert_eq!(request.instance(), &expected);
    request.dispatch().unwrap();
    assert_eq!(call.resume(1).unwrap(), RunStatus::Complete);
    assert_eq!(call.result().unwrap().unwrap(), [Value::U64(1)]);
    drop(call);
    drop(vm);
    let record = recording.finish();
    assert!(
        matches!(record.replay().unwrap(), ReplayOutcome::Returned(values) if values.values() == [Value::U64(1)])
    );
}
