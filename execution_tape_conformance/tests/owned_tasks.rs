// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Tasks own their VM and keep their computation there between polls, with the same budgets,
//! evidence and cancellation boundary as one invocation.

use execution_tape::value::FuncRef;
use execution_tape::value::UnitId;
use execution_tape::{
    aggregates::AggHeap,
    asm::{FunctionAbi, FunctionSig, ProgramBuilder, Reg},
    callable::Callable,
    contract::Contract,
    host::{AccessSink, Host, HostContext, HostError, HostSig, ResourceKeyRef, SigHash, ValueRef},
    program::{Pc, SpanId, ValueType},
    recording::{CaptureFault, RecordLimits, Recorder, ReplayOutcome},
    trace::TraceSink,
    value::{Closure, Value},
    vm::{ComputationId, InvocationOptions, Limits, RunStatus, Trap, TrapInfo, Vm},
};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
    task::{Context, Poll},
};

#[derive(Default)]
struct Effects {
    calls: Cell<usize>,
    drops: Cell<usize>,
}
struct Echo(Rc<Effects>);
impl Drop for Echo {
    fn drop(&mut self) {
        self.0.drops.set(self.0.drops.get() + 1);
    }
}
impl Host for Echo {
    fn call(
        &mut self,
        symbol: &str,
        _: SigHash,
        args: &[ValueRef<'_>],
        rets: &mut [Value],
        mut ctx: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        if symbol != "echo" {
            return Err(HostError::UnknownSymbol);
        }
        self.0.calls.set(self.0.calls.get() + 1);
        ctx.record_read(ResourceKeyRef::Input("file"));
        ctx.record_write(ResourceKeyRef::Input("calls"));
        rets[0] = args[0].to_value();
        Ok(2)
    }
}
#[derive(Default)]
struct Reads(Vec<String>, Vec<String>);
impl AccessSink for Reads {
    fn read(&mut self, key: ResourceKeyRef<'_>) {
        if let ResourceKeyRef::Input(name) = key {
            self.0.push(name.into());
        }
    }
    fn write(&mut self, key: ResourceKeyRef<'_>) {
        if let ResourceKeyRef::Input(name) = key {
            self.1.push(name.into());
        }
    }
}
fn echo() -> Callable {
    let mut builder = ProgramBuilder::new();
    let entry = builder
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::Str],
            ret_types: vec![ValueType::Str],
        })
        .unwrap();
    let import = builder.host_sig_for(
        "echo",
        HostSig {
            args: vec![ValueType::Str],
            rets: vec![ValueType::Str],
        },
    );
    let mut asm = builder.function_asm(entry).unwrap();
    let temporary = asm.alloc_register(ValueType::Agg);
    let answer = asm.alloc_register(ValueType::Str);
    asm.tuple_new(temporary, &[Reg(1)])
        .host_call_sig_id(Reg::EFFECT, import, Reg::EFFECT, &[Reg(1)], &[answer])
        .ret(Reg::EFFECT, &[answer]);
    builder.define_function(entry, asm).unwrap();
    Callable::function(Arc::new(builder.build_verified().unwrap()), entry).unwrap()
}
fn retained_closure() -> Callable {
    let mut builder = ProgramBuilder::new();
    let integer = builder.contract(Contract::Kind(ValueType::I64)).unwrap();
    let environment = builder.contract(Contract::Tuple(&[Some(integer)])).unwrap();
    let body = builder
        .declare_function(FunctionAbi {
            args: vec![environment.into(), integer.into()],
            rets: vec![integer.into()],
        })
        .unwrap();
    let mut asm = builder.function_asm(body).unwrap();
    let capture = asm.alloc_register(ValueType::I64);
    let answer = asm.alloc_register(ValueType::I64);
    asm.tuple_get(capture, Reg(1), 0)
        .i64_add(answer, capture, Reg(2))
        .ret(Reg::EFFECT, &[answer]);
    builder.define_function(body, asm).unwrap();
    let program = Arc::new(builder.build_verified().unwrap());
    let mut heap = AggHeap::new();
    let env = heap.tuple_new(vec![Value::I64(40)]).unwrap();
    Callable::retain(
        program,
        &heap,
        ValueRef::Closure(Closure {
            func: FuncRef::new(UnitId(0), body),
            env,
        }),
    )
    .unwrap()
}
fn require_static<T: 'static>(_: &T) {}

/// A task: its VM and the computation it saved there.
struct Task {
    vm: Vm<Echo>,
    computation: ComputationId,
}

#[test]
fn a_task_moves_between_containers_with_its_saved_request_and_input_payload() {
    let callable = echo();
    let code = Arc::downgrade(callable.program());
    let effects = Rc::new(Effects::default());
    let input = String::from("owned input payload");
    let pointer = input.as_ptr();
    let mut vm = Vm::new(
        Echo(Rc::clone(&effects)),
        Limits {
            fuel: 100,
            ..Limits::default()
        },
    );
    let unit = vm.install(Arc::clone(callable.program())).unwrap();
    let mut call = callable
        .start(
            &mut vm,
            unit,
            vec![Value::Str(input)],
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    assert_eq!(call.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    let computation = call.save().unwrap();
    let task = Task { vm, computation };
    require_static(&task);
    let mut tasks = vec![task];
    drop(callable);
    assert!(code.upgrade().is_some(), "the VM keeps the code it runs");
    let task = Box::new(tasks.pop().unwrap());
    let mut task = *task;
    let mut call = task
        .vm
        .resume(
            task.computation,
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    assert_eq!(call.host().0.calls.get(), 0);
    {
        let request = call.pending_host_call().unwrap();
        let ValueRef::Str(input) = request.arguments().next().unwrap() else {
            panic!("string input")
        };
        assert_eq!(input.as_ptr(), pointer, "moving the task copied nothing");
    }
    call.pending_host_call().unwrap().dispatch().unwrap();
    assert_eq!(call.resume(1).unwrap(), RunStatus::Complete);
    assert_eq!(
        call.result().unwrap().unwrap(),
        [Value::Str("owned input payload".into())]
    );
    drop(call);
    // The temporary the first invocation built stays until the task collects.
    assert_eq!(task.vm.aggregates().len_u32(), 1);
    task.vm.collect().unwrap();
    assert_eq!(task.vm.aggregates().len_u32(), 0);
    drop(task);
    assert_eq!(effects.calls.get(), 1);
    assert_eq!(effects.drops.get(), 1);
    assert!(code.upgrade().is_none());
}

#[test]
fn saved_and_unsaved_runs_share_results_resource_observations_and_replay() {
    let callable = echo();
    for saved in [false, true] {
        let effects = Rc::new(Effects::default());
        let recorder = Rc::new(RefCell::new(Recorder::new(
            Arc::clone(callable.program()),
            RecordLimits::default(),
        )));
        let reads = Rc::new(RefCell::new(Reads::default()));
        let mask = recorder.borrow().mask();
        // A function, not a closure, so each invocation's options borrow the VM anew.
        fn shared<'a>(
            mask: execution_tape::trace::TraceMask,
            recorder: &Rc<RefCell<Recorder>>,
            reads: &Rc<RefCell<Reads>>,
        ) -> InvocationOptions<'a> {
            InvocationOptions::default()
                .yield_host_calls()
                .trace_owned(mask, Rc::clone(recorder))
                .access_owned(Rc::clone(reads))
        }
        let mut vm = Vm::new(
            Echo(Rc::clone(&effects)),
            Limits {
                fuel: 100,
                ..Limits::default()
            },
        );
        let unit = vm.install(Arc::clone(callable.program())).unwrap();
        let mut call = callable
            .start(
                &mut vm,
                unit,
                vec![Value::Str("hello".into())],
                shared(mask, &recorder, &reads),
            )
            .unwrap();
        assert_eq!(call.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
        let mut call = if saved {
            let id = call.save().unwrap();
            vm.resume(id, shared(mask, &recorder, &reads)).unwrap()
        } else {
            call
        };
        call.pending_host_call().unwrap().dispatch().unwrap();
        assert_eq!(call.resume(u64::MAX).unwrap(), RunStatus::Complete);
        assert_eq!(call.take_outputs().unwrap(), [Value::Str("hello".into())]);
        drop(call);
        assert_eq!(reads.borrow().0, ["file"]);
        assert_eq!(reads.borrow().1, ["calls"]);
        let recording = Rc::try_unwrap(recorder)
            .expect("observer retired")
            .into_inner()
            .finish();
        assert!(recording.status().is_ok());
        assert!(
            matches!(recording.replay().unwrap(), ReplayOutcome::Returned(result) if result.values() == [Value::Str("hello".into())])
        );
        assert_eq!(
            effects.calls.get(),
            1,
            "replay does not dispatch the original host"
        );
    }
}

#[test]
fn dropping_a_task_cancels_its_saved_computation_and_leaves_incomplete_evidence() {
    let callable = echo();
    let effects = Rc::new(Effects::default());
    let recorder = Rc::new(RefCell::new(Recorder::new(
        Arc::clone(callable.program()),
        RecordLimits::default(),
    )));
    let mask = recorder.borrow().mask();
    let mut vm = Vm::new(Echo(Rc::clone(&effects)), Limits::default());
    let unit = vm.install(Arc::clone(callable.program())).unwrap();
    let mut call = callable
        .start(
            &mut vm,
            unit,
            vec![Value::Str("cancel".into())],
            InvocationOptions::default()
                .yield_host_calls()
                .trace_owned(mask, Rc::clone(&recorder)),
        )
        .unwrap();
    assert_eq!(call.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    let computation = call.save().unwrap();
    let task = Task { vm, computation };
    assert_eq!(task.vm.heap_stats().saved_computations, 1);
    drop(task);
    assert_eq!(effects.drops.get(), 1);
    assert_eq!(effects.calls.get(), 0);
    let recording = Rc::try_unwrap(recorder)
        .expect("observer retired")
        .into_inner()
        .finish();
    assert!(matches!(
        recording.status(),
        Err(CaptureFault::MissingCompletion)
    ));
    assert!(recording.replay().is_err());
}

#[test]
fn failed_entry_and_failed_capture_admission_have_no_host_effects() {
    let effects = Rc::new(Effects::default());
    let mut vm = Vm::new(Echo(Rc::clone(&effects)), Limits::default());
    let echo = echo();
    let unit = vm.install(Arc::clone(echo.program())).unwrap();
    assert!(matches!(
        echo.start(
            &mut vm,
            unit,
            vec![Value::I64(7)],
            InvocationOptions::default()
        )
        .unwrap_err()
        .trap,
        Trap::TypeMismatch { .. }
    ));
    let closure = retained_closure();
    let mut vm = Vm::new(
        Echo(Rc::clone(&effects)),
        Limits {
            max_aggregate_work: 1,
            ..Limits::default()
        },
    );
    let unit = vm.install(Arc::clone(closure.program())).unwrap();
    assert!(matches!(
        closure
            .start(
                &mut vm,
                unit,
                vec![Value::I64(2)],
                InvocationOptions::default()
            )
            .unwrap_err()
            .trap,
        Trap::AggregateWorkExceeded { .. }
    ));
    assert_eq!(vm.aggregates().len_u32(), 0, "no capture was imported");
    assert_eq!(effects.calls.get(), 0);
}

#[test]
fn closure_entry_uses_the_same_capture_import_work_and_recording_contract() {
    let callable = retained_closure();
    let effects = Rc::new(Effects::default());
    let recorder = Rc::new(RefCell::new(Recorder::new(
        Arc::clone(callable.program()),
        RecordLimits::default(),
    )));
    let mask = recorder.borrow().mask();
    let mut vm = Vm::new(
        Echo(Rc::clone(&effects)),
        Limits {
            max_aggregate_work: 2,
            ..Limits::default()
        },
    );
    let unit = vm.install(Arc::clone(callable.program())).unwrap();
    let mut call = callable
        .start(
            &mut vm,
            unit,
            vec![Value::I64(2)],
            InvocationOptions::default().trace_owned(mask, Rc::clone(&recorder)),
        )
        .unwrap();
    drop(callable);
    assert_eq!(call.data_work().aggregate, 2);
    assert_eq!(call.resume(u64::MAX).unwrap(), RunStatus::Complete);
    assert_eq!(call.result().unwrap().unwrap(), [Value::I64(42)]);
    drop(call);
    let recording = Rc::try_unwrap(recorder).unwrap().into_inner().finish();
    assert!(
        matches!(recording.replay().unwrap(), ReplayOutcome::Returned(result) if result.values() == [Value::I64(42)])
    );
}

/// A graph task that owns its VM and resumes its computation on every poll.
struct GraphGuest {
    vm: Vm<Echo>,
    computation: ComputationId,
    reply: Rc<RefCell<Option<String>>>,
}
impl std::fmt::Debug for GraphGuest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GraphGuest").finish_non_exhaustive()
    }
}
impl execution_graph::NodeTask<String, TrapInfo> for GraphGuest {
    fn poll(
        &mut self,
        _: &mut Context<'_>,
        outputs: &mut Vec<String>,
        access: &mut execution_graph::NodeAccess<'_, String>,
    ) -> Poll<Result<(), TrapInfo>> {
        let mut call = self
            .vm
            .resume(
                self.computation,
                InvocationOptions::default().yield_host_calls(),
            )
            .expect("each poll resumes what the last one saved");
        loop {
            match call.resume(u64::MAX) {
                Err(error) => return Poll::Ready(Err(error)),
                Ok(RunStatus::AwaitingHost) => {
                    access.read_host_state(execution_graph::HostOpId::new(1), 0);
                    let Some(reply) = self.reply.borrow_mut().take() else {
                        self.computation = call.save().expect("waiting at its request");
                        return Poll::Pending;
                    };
                    call.pending_host_call()
                        .unwrap()
                        .complete(&mut [Value::Str(reply)], Ok(0))?;
                }
                Ok(RunStatus::Complete) => {
                    let Value::Str(result) = call.take_outputs().unwrap().pop().unwrap() else {
                        panic!("string result")
                    };
                    outputs.push(result);
                    return Poll::Ready(Ok(()));
                }
                Ok(status) => unreachable!("unexpected run status {status:?}"),
            }
        }
    }
}

#[test]
fn graph_tasks_own_their_vm_and_cancelled_ones_ignore_late_replies() {
    use execution_graph::{ExecutionGraph, FnExecutor, FnNode};
    let callable = echo();
    let effects = Rc::new(Effects::default());
    let replies = Rc::new(RefCell::new(Vec::new()));
    let mut graph = ExecutionGraph::new(FnExecutor::<String, TrapInfo>::new());
    let node = graph
        .add_node(
            FnNode::deferred({
                let effects = Rc::clone(&effects);
                let replies = Rc::clone(&replies);
                move |inputs: &[String], _| {
                    let reply = Rc::new(RefCell::new(None));
                    replies.borrow_mut().push(Rc::clone(&reply));
                    let mut vm = Vm::new(Echo(Rc::clone(&effects)), Limits::default());
                    let unit = vm.install(Arc::clone(callable.program())).unwrap();
                    let call = callable.start(
                        &mut vm,
                        unit,
                        vec![Value::Str(inputs[0].clone())],
                        InvocationOptions::default().yield_host_calls(),
                    )?;
                    let computation = call.save().expect("not yet run");
                    Ok(Box::new(GraphGuest {
                        vm,
                        computation,
                        reply,
                    }))
                }
            }),
            vec!["source".into()],
            vec!["result".into()],
        )
        .unwrap();
    graph.set_input_value(node, "source", "old".into()).unwrap();
    graph.run_all().unwrap();
    assert_eq!(effects.drops.get(), 0);
    graph.set_input_value(node, "source", "new".into()).unwrap();
    assert_eq!(effects.drops.get(), 1, "the old task's VM is gone");
    replies.borrow()[0].borrow_mut().replace("obsolete".into());
    graph.run_all().unwrap();
    replies.borrow()[1].borrow_mut().replace("current".into());
    graph.run_all().unwrap();
    assert_eq!(
        graph
            .node_outputs(node)
            .unwrap()
            .get("result")
            .map(String::as_str),
        Some("current")
    );
    assert_eq!(effects.drops.get(), 2);
    assert_eq!(effects.calls.get(), 0);
}

#[test]
fn shared_owned_trace_observers_forward_subscription_filtering_and_every_event_family() {
    use execution_tape::{
        program::Program,
        trace::{
            RegisterEvent, RegisterPhase, ScopeKind, TraceLocation, TraceMask, TraceOutcome,
            ValueEvent,
        },
    };
    #[derive(Default)]
    struct Events {
        subscribed: Option<TraceMask>,
        filters: Cell<usize>,
        starts: usize,
        ends: usize,
        values: usize,
        instructions: usize,
        completions: usize,
        enters: usize,
        exits: usize,
    }
    fn mask() -> TraceMask {
        TraceMask::RUN
            | TraceMask::CALL
            | TraceMask::HOST
            | TraceMask::INSTR
            | TraceMask::VALUES
            | TraceMask::REGISTERS
    }
    impl TraceSink for Events {
        fn mask(&self) -> TraceMask {
            mask()
        }
        fn run_mask(&mut self, _: &Program, mask: TraceMask) {
            self.subscribed = Some(mask);
        }
        fn wants_registers(&self, _: TraceLocation, _: RegisterPhase) -> bool {
            self.filters.set(self.filters.get() + 1);
            false
        }
        fn registers(&mut self, _: &Program, _: RegisterEvent<'_>) {
            panic!("register filtering was not forwarded");
        }
        fn run_start(&mut self, _: &Program, _: FuncRef, _: usize) {
            self.starts += 1;
        }
        fn run_end(&mut self, _: &Program, outcome: TraceOutcome<'_>) {
            assert!(matches!(outcome, TraceOutcome::Ok));
            self.ends += 1;
        }
        fn values(&mut self, _: &Program, _: ValueEvent<'_>) {
            self.values += 1;
        }
        fn instr(
            &mut self,
            _: &Program,
            _location: TraceLocation,
            _: Pc,
            _: Option<SpanId>,
            _: u16,
        ) {
            self.instructions += 1;
        }
        fn instr_end(
            &mut self,
            _: &Program,
            _: TraceLocation,
            _: Option<TraceLocation>,
            _: TraceOutcome<'_>,
        ) {
            self.completions += 1;
        }
        fn scope_enter(
            &mut self,
            _: &Program,
            _: ScopeKind,
            _: usize,
            _location: TraceLocation,
            _: Option<SpanId>,
        ) {
            self.enters += 1;
        }
        fn scope_exit(
            &mut self,
            _: &Program,
            _: ScopeKind,
            _: usize,
            _location: TraceLocation,
            _: Option<SpanId>,
            _: TraceOutcome<'_>,
        ) {
            self.exits += 1;
        }
    }
    let observer = Rc::new(RefCell::new(Events::default()));
    assert_eq!(observer.mask(), mask());
    let callable = echo();
    let mut vm = Vm::new(Echo(Rc::new(Effects::default())), Limits::default());
    let unit = vm.install(Arc::clone(callable.program())).unwrap();
    let mut call = callable
        .start(
            &mut vm,
            unit,
            vec![Value::Str("observed".into())],
            InvocationOptions::default().trace_owned(mask(), Rc::clone(&observer)),
        )
        .unwrap();
    call.resume(u64::MAX).unwrap();
    drop(call);
    let events = observer.borrow();
    assert_eq!(events.subscribed, Some(mask()));
    assert!(events.filters.get() > 0);
    assert_eq!((events.starts, events.ends), (1, 1));
    assert_eq!((events.instructions, events.completions), (3, 3));
    assert_eq!((events.enters, events.exits), (2, 2));
    assert_eq!(events.values, 3);
}

fn pack_text() -> Callable {
    let mut builder = ProgramBuilder::new();
    let entry = builder
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::Str],
            ret_types: vec![ValueType::Agg],
        })
        .unwrap();
    let mut body = builder.function_asm(entry).unwrap();
    let packed = body.alloc_register(ValueType::Agg);
    body.tuple_new(packed, &[Reg(1)])
        .ret(Reg::EFFECT, &[packed]);
    builder.define_function(entry, body).unwrap();
    Callable::function(Arc::new(builder.build_verified().unwrap()), entry).unwrap()
}

#[test]
fn retiring_a_vm_moves_its_results_without_copying_their_nested_payloads() {
    let target = pack_text();
    let code = Arc::downgrade(target.program());
    let effects = Rc::new(Effects::default());
    let mut vm = Vm::new(Echo(effects.clone()), Limits::default());
    let unit = vm.install(Arc::clone(target.program())).unwrap();
    let mut call = target
        .start(
            &mut vm,
            unit,
            vec![Value::Str("packed".into())],
            InvocationOptions::default(),
        )
        .unwrap();
    drop(target);
    call.resume(u64::MAX).unwrap();
    let owner = Arc::clone(call.program());
    assert!(Arc::ptr_eq(&owner, &code.upgrade().unwrap()));
    let outputs = call.take_outputs().unwrap();
    drop(call);
    let Value::Agg(root) = outputs[0] else {
        panic!("root")
    };
    let Value::Str(text) = vm.aggregates().tuple_get_ref(root, 0).unwrap() else {
        panic!("text")
    };
    let pointer = text.as_ptr();
    let snapshot = vm.into_snapshot(outputs).unwrap();
    assert_eq!(effects.drops.get(), 1, "retirement releases the VM/host");
    let Value::Agg(root) = snapshot.values()[0] else {
        panic!("snapshot root")
    };
    let Value::Str(text) = snapshot.aggregates().tuple_get_ref(root, 0).unwrap() else {
        panic!("snapshot text")
    };
    assert_eq!(
        text.as_ptr(),
        pointer,
        "aggregate payload moved without a copy"
    );
    drop(owner);
    assert!(
        code.upgrade().is_none(),
        "unscoped snapshots require explicit code retention"
    );
    assert_eq!(
        text, "packed",
        "payload remains owned after VM/code retirement"
    );
}

#[test]
fn consuming_an_invocation_copies_its_results_and_an_unfinished_one_cancels() {
    let target = pack_text();
    let effects = Rc::new(Effects::default());
    let mut vm = Vm::new(Echo(effects.clone()), Limits::default());
    let args = [Value::Str("borrowed".into())];
    let unit = vm.install(target.program().clone()).unwrap();
    let mut call = vm
        .start(
            FuncRef::new(unit, target.function_id()),
            &args,
            InvocationOptions::default(),
        )
        .unwrap();
    assert!(
        Arc::ptr_eq(call.program(), target.program()),
        "every entry retains its installed code"
    );
    call.resume(u64::MAX).unwrap();
    let Value::Agg(original) = call.result().unwrap().unwrap()[0] else {
        panic!("root")
    };
    let snapshot = call.into_snapshot().unwrap().unwrap();
    let Value::Agg(retained) = snapshot.values()[0] else {
        panic!("retained root")
    };
    assert_eq!(
        vm.aggregates().tuple_get(original, 0),
        Ok(Value::Str("borrowed".into()))
    );
    assert_eq!(
        snapshot.aggregates().tuple_get(retained, 0),
        Ok(Value::Str("borrowed".into()))
    );
    let call = target
        .start(&mut vm, unit, args.to_vec(), InvocationOptions::default())
        .unwrap();
    assert!(call.into_snapshot().unwrap().is_none());
    assert_eq!(
        effects.calls.get(),
        0,
        "consuming an unfinished invocation cancels without execution"
    );
}

#[test]
fn a_vm_retired_with_saved_computations_leaves_its_snapshot_unpinned() {
    let target = pack_text();
    let effects = Rc::new(Effects::default());
    let mut vm = Vm::new(Echo(effects), Limits::default());
    let unit = vm.install(Arc::clone(target.program())).unwrap();
    let mut call = target
        .start(
            &mut vm,
            unit,
            vec![Value::Str("packed".into())],
            InvocationOptions::default(),
        )
        .unwrap();
    call.resume(u64::MAX).unwrap();
    let outputs = call.take_outputs().unwrap();
    drop(call);
    // A computation waiting at its host request pins the VM's heap.
    let echo = echo();
    let echoes = vm.install(Arc::clone(echo.program())).unwrap();
    let mut waiting = echo
        .start(
            &mut vm,
            echoes,
            vec![Value::Str("waiting".into())],
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    assert_eq!(waiting.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    waiting.save().ok().unwrap();

    let snapshot = vm.into_snapshot(outputs).unwrap();
    let mut copy = snapshot.aggregates().clone();
    let mut fresh = Vm::new(Echo(Rc::default()), Limits::default());
    *fresh.aggregates_mut() = copy.clone();
    copy.clear();
    fresh.aggregates_mut().clear();
    fresh.collect().unwrap();
}
