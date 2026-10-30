// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Independently stored invocations use the same budgets, evidence and cancellation boundary.

use execution_tape::{
    aggregates::AggHeap,
    asm::{FunctionAbi, FunctionSig, ProgramBuilder},
    callable::Callable,
    contract::Contract,
    host::{AccessSink, Host, HostContext, HostError, HostSig, ResourceKeyRef, SigHash, ValueRef},
    program::ValueType,
    recording::{CaptureFault, RecordLimits, Recorder, ReplayOutcome},
    trace::TraceSink,
    value::{Closure, Value},
    vm::{Invocation, InvocationOptions, Limits, RunStatus, Trap, TrapInfo, Vm},
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
    asm.tuple_new(temporary, &[1])
        .host_call_sig_id(0, import, 0, &[1], &[answer])
        .ret(0, &[answer]);
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
    asm.tuple_get(capture, 1, 0)
        .i64_add(answer, capture, 2)
        .ret(0, &[answer]);
    builder.define_function(body, asm).unwrap();
    let program = Arc::new(builder.build_verified().unwrap());
    let mut heap = AggHeap::new();
    let env = heap.tuple_new(vec![Value::I64(40)]).unwrap();
    Callable::retain(
        program,
        &heap,
        ValueRef::Closure(Closure { func: body, env }),
    )
    .unwrap()
}
fn require_static<T: 'static>(_: &T) {}

#[test]
fn an_owned_invocation_moves_between_containers_and_keeps_borrowed_inputs_stable() {
    let callable = echo();
    let code = Arc::downgrade(callable.program());
    let effects = Rc::new(Effects::default());
    let input = String::from("owned input payload");
    let pointer = input.as_ptr();
    let call = callable
        .start_owned(
            Vm::new(
                Echo(Rc::clone(&effects)),
                Limits {
                    fuel: 100,
                    ..Limits::default()
                },
            ),
            vec![Value::Str(input)],
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    require_static(&call);
    let mut tasks: Vec<Invocation<'static, Echo>> = vec![call];
    drop(callable);
    assert!(code.upgrade().is_some());
    let mut call = tasks.pop().unwrap();
    assert_eq!(call.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    assert_eq!(call.host().0.calls.get(), 0);
    {
        let request = call.pending_host_call().unwrap();
        let ValueRef::Str(input) = request.arguments().next().unwrap() else {
            panic!("string input")
        };
        assert_eq!(input.as_ptr(), pointer);
    }
    let call = Box::new(call);
    let mut call = *call;
    call.pending_host_call().unwrap().dispatch().unwrap();
    assert_eq!(call.resume(1).unwrap(), RunStatus::Complete);
    assert_eq!(
        call.result().unwrap().unwrap(),
        [Value::Str("owned input payload".into())]
    );
    assert_eq!(call.remaining_fuel(), 95);
    assert_eq!(call.aggregates().len_u32(), 0);
    drop(call);
    assert_eq!(effects.calls.get(), 1);
    assert_eq!(effects.drops.get(), 1);
    assert!(code.upgrade().is_none());
}

#[test]
fn owned_and_borrowed_entries_share_work_fuel_resource_observations_and_replay() {
    let callable = echo();
    let mut expected = None;
    for owned in [false, true] {
        let effects = Rc::new(Effects::default());
        let recorder = Rc::new(RefCell::new(Recorder::new(
            Arc::clone(callable.program()),
            RecordLimits::default(),
        )));
        let reads = Rc::new(RefCell::new(Reads::default()));
        let mask = recorder.borrow().mask();
        let mut observer = Rc::clone(&recorder);
        let mut access = Rc::clone(&reads);
        let mut vm = Vm::new(
            Echo(Rc::clone(&effects)),
            Limits {
                fuel: 100,
                ..Limits::default()
            },
        );
        let options = if owned {
            InvocationOptions::default()
                .trace_owned(mask, Rc::clone(&recorder))
                .access_owned(Rc::clone(&reads))
        } else {
            InvocationOptions::default()
                .trace(mask, &mut observer)
                .access(&mut access)
        };
        let mut call = if owned {
            callable
                .start_owned(vm, vec![Value::Str("hello".into())], options)
                .unwrap()
        } else {
            callable
                .start(&mut vm, vec![Value::Str("hello".into())], options)
                .unwrap()
        };
        while call.resume(1).unwrap() == RunStatus::Suspended {}
        let outcome = (
            call.take_outputs().unwrap(),
            call.remaining_fuel(),
            call.data_work(),
        );
        drop(call);
        drop(observer);
        drop(access);
        assert_eq!(reads.borrow().0, ["file"]);
        assert_eq!(reads.borrow().1, ["calls"]);
        if let Some(expected) = &expected {
            assert_eq!(&outcome, expected);
        } else {
            expected = Some(outcome);
        }
        let recorder = Rc::try_unwrap(recorder)
            .expect("observer retired")
            .into_inner();
        let recording = recorder.finish();
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
fn cancellation_releases_the_owned_vm_and_retains_explicitly_incomplete_evidence() {
    let callable = echo();
    let effects = Rc::new(Effects::default());
    let recorder = Rc::new(RefCell::new(Recorder::new(
        Arc::clone(callable.program()),
        RecordLimits::default(),
    )));
    let mask = recorder.borrow().mask();
    let mut call = callable
        .start_owned(
            Vm::new(Echo(Rc::clone(&effects)), Limits::default()),
            vec![Value::Str("cancel".into())],
            InvocationOptions::default()
                .yield_host_calls()
                .trace_owned(mask, Rc::clone(&recorder)),
        )
        .unwrap();
    assert_eq!(call.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    assert_eq!(call.aggregates().len_u32(), 1);
    drop(call);
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
fn failed_entry_and_failed_capture_admission_release_owned_host_state_without_effects() {
    let effects = Rc::new(Effects::default());
    assert!(matches!(
        echo()
            .start_owned(
                Vm::new(Echo(Rc::clone(&effects)), Limits::default()),
                vec![Value::I64(7)],
                InvocationOptions::default()
            )
            .unwrap_err()
            .trap,
        Trap::TypeMismatch { .. }
    ));
    let closure = retained_closure();
    assert!(matches!(
        closure
            .start_owned(
                Vm::new(
                    Echo(Rc::clone(&effects)),
                    Limits {
                        max_aggregate_work: 1,
                        ..Limits::default()
                    }
                ),
                vec![Value::I64(2)],
                InvocationOptions::default()
            )
            .unwrap_err()
            .trap,
        Trap::AggregateWorkExceeded { .. }
    ));
    assert_eq!(effects.drops.get(), 2);
    assert_eq!(effects.calls.get(), 0);
}

#[test]
fn owned_closure_entry_uses_the_same_capture_import_work_and_recording_contract() {
    let callable = retained_closure();
    let effects = Rc::new(Effects::default());
    let recorder = Rc::new(RefCell::new(Recorder::new(
        Arc::clone(callable.program()),
        RecordLimits::default(),
    )));
    let mask = recorder.borrow().mask();
    let mut call = callable
        .start_owned(
            Vm::new(
                Echo(Rc::clone(&effects)),
                Limits {
                    max_aggregate_work: 2,
                    ..Limits::default()
                },
            ),
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
    assert_eq!(effects.drops.get(), 1);
}

#[derive(Debug)]
struct GraphGuest {
    call: Invocation<'static, Echo>,
    reply: Rc<RefCell<Option<String>>>,
}
impl execution_graph::NodeTask<String, TrapInfo> for GraphGuest {
    fn poll(
        &mut self,
        _: &mut Context<'_>,
        outputs: &mut Vec<String>,
        access: &mut execution_graph::NodeAccess<'_, String>,
    ) -> Poll<Result<(), TrapInfo>> {
        loop {
            match self.call.resume(u64::MAX) {
                Err(error) => return Poll::Ready(Err(error)),
                Ok(RunStatus::Suspended) => unreachable!("unlimited quantum"),
                Ok(RunStatus::AwaitingHost) => {
                    access.read_host_state(execution_graph::HostOpId::new(1), 0);
                    let Some(reply) = self.reply.borrow_mut().take() else {
                        return Poll::Pending;
                    };
                    self.call
                        .pending_host_call()
                        .unwrap()
                        .complete(&mut [Value::Str(reply)], Ok(0))?;
                }
                Ok(RunStatus::Complete) => {
                    let Value::Str(result) = self.call.take_outputs().unwrap().pop().unwrap()
                    else {
                        panic!("string result")
                    };
                    outputs.push(result);
                    return Poll::Ready(Ok(()));
                }
            }
        }
    }
}

#[test]
fn graph_owned_guest_tasks_cancel_old_vms_and_reject_their_late_replies() {
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
                    let call = callable.start_owned(
                        Vm::new(Echo(Rc::clone(&effects)), Limits::default()),
                        vec![Value::Str(inputs[0].clone())],
                        InvocationOptions::default().yield_host_calls(),
                    )?;
                    Ok(Box::new(GraphGuest { call, reply }))
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
    assert_eq!(effects.drops.get(), 1);
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
        value::FuncId,
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
        fn run_start(&mut self, _: &Program, _: FuncId, _: usize) {
            self.starts += 1;
        }
        fn run_end(&mut self, _: &Program, outcome: TraceOutcome<'_>) {
            assert!(matches!(outcome, TraceOutcome::Ok));
            self.ends += 1;
        }
        fn values(&mut self, _: &Program, _: ValueEvent<'_>) {
            self.values += 1;
        }
        fn instr(&mut self, _: &Program, _: FuncId, _: u32, _: u32, _: Option<u64>, _: u8) {
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
            _: FuncId,
            _: u32,
            _: Option<u64>,
        ) {
            self.enters += 1;
        }
        fn scope_exit(
            &mut self,
            _: &Program,
            _: ScopeKind,
            _: usize,
            _: FuncId,
            _: u32,
            _: Option<u64>,
            _: TraceOutcome<'_>,
        ) {
            self.exits += 1;
        }
    }
    let observer = Rc::new(RefCell::new(Events::default()));
    assert_eq!(observer.mask(), mask());
    let callable = echo();
    let mut call = callable
        .start_owned(
            Vm::new(Echo(Rc::new(Effects::default())), Limits::default()),
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

#[test]
fn owning_the_vm_does_not_require_host_resources_to_be_static() {
    struct BorrowedHost<'a>(&'a Cell<usize>);
    impl Host for BorrowedHost<'_> {
        fn call(
            &mut self,
            _: &str,
            _: SigHash,
            args: &[ValueRef<'_>],
            rets: &mut [Value],
            _: HostContext<'_, '_>,
        ) -> Result<u64, HostError> {
            self.0.set(self.0.get() + 1);
            rets[0] = args[0].to_value();
            Ok(0)
        }
    }
    let calls = Cell::new(0);
    {
        let mut call = echo()
            .start_owned(
                Vm::new(BorrowedHost(&calls), Limits::default()),
                vec![Value::Str("scoped".into())],
                InvocationOptions::default(),
            )
            .unwrap();
        assert_eq!(call.resume(u64::MAX).unwrap(), RunStatus::Complete);
        assert_eq!(
            call.result().unwrap().unwrap(),
            [Value::Str("scoped".into())]
        );
    }
    assert_eq!(calls.get(), 1);
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
    body.tuple_new(packed, &[1]).ret(0, &[packed]);
    builder.define_function(entry, body).unwrap();
    Callable::function(Arc::new(builder.build_verified().unwrap()), entry).unwrap()
}

#[test]
fn consume_owned_results_without_copying_their_nested_payloads() {
    let target = pack_text();
    let code = Arc::downgrade(target.program());
    let effects = Rc::new(Effects::default());
    let mut call = target
        .start_owned(
            Vm::new(Echo(effects.clone()), Limits::default()),
            vec![Value::Str("packed".into())],
            InvocationOptions::default(),
        )
        .unwrap();
    drop(target);
    call.resume(u64::MAX).unwrap();
    let owner = call
        .program_owner()
        .cloned()
        .expect("callable code retained");
    assert!(Arc::ptr_eq(&owner, &code.upgrade().unwrap()));
    let Value::Agg(root) = call.result().unwrap().unwrap()[0] else {
        panic!("root")
    };
    let Value::Str(text) = call.aggregates().tuple_get_ref(root, 0).unwrap() else {
        panic!("text")
    };
    let pointer = text.as_ptr();
    let snapshot = call.into_snapshot().unwrap().unwrap();
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
        "owned aggregate payload moved without a copy"
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
fn consume_borrowed_results_preserves_raw_handles_and_unfinished_consumption_cancels() {
    let target = pack_text();
    let mut vm = Vm::new(Echo(Rc::new(Effects::default())), Limits::default());
    let args = [Value::Str("borrowed".into())];
    let mut call = vm
        .start(
            target.program(),
            target.function_id(),
            &args,
            InvocationOptions::default(),
        )
        .unwrap();
    assert!(
        call.program_owner().is_none(),
        "raw entry only borrows its code"
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
    let effects = Rc::new(Effects::default());
    let call = target
        .start_owned(
            Vm::new(Echo(effects.clone()), Limits::default()),
            args.to_vec(),
            InvocationOptions::default(),
        )
        .unwrap();
    assert!(call.into_snapshot().unwrap().is_none());
    assert_eq!(effects.drops.get(), 1);
    assert_eq!(
        effects.calls.get(),
        0,
        "consuming an unfinished invocation cancels without execution"
    );
}
