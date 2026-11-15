// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! VM-owned computations survive turn boundaries without retaining scheduler borrows.

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
    vm::{ControlError, Limits, RunStatus, Trap, TurnOptions, TurnOutcome, Vm},
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
    let body = Vm::new(Echo(Rc::new(Effects::default())), Limits::default())
        .function(&program, body)
        .unwrap();
    Callable::retain(&heap, ValueRef::Closure(&Closure { func: body, env })).unwrap()
}
fn require_static<T: 'static>(_: &T) {}

#[test]
fn a_vm_moves_between_containers_without_copying_its_pending_host_input() {
    let callable = echo();
    let code = Arc::downgrade(callable.program());
    let definition = callable.program().as_ref().clone();
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
    let (request, first_work) = {
        let mut turn = vm.turn(TurnOptions::default().yield_host_calls()).unwrap();
        turn.start(&callable, vec![Value::Str(input)]).unwrap();
        let RunStatus::AwaitingHost(id) = turn.drive(u64::MAX).unwrap() else {
            panic!("pending echo request")
        };
        let work = turn.work();
        assert!(matches!(
            turn.finish().unwrap(),
            TurnOutcome::AwaitingHost(_)
        ));
        (id, work)
    };
    require_static(&vm);
    let mut tasks = vec![vm];
    drop(callable);
    assert!(
        code.upgrade().is_none(),
        "the outer Arc wrapper is not the code identity"
    );
    let mut vm = tasks.pop().unwrap();
    let mut turn = vm.turn(TurnOptions::default().yield_host_calls()).unwrap();
    assert_eq!(
        turn.drive(0).unwrap(),
        RunStatus::AwaitingHost(request.clone())
    );
    assert_eq!(turn.host().0.calls.get(), 0);
    {
        let pending = turn.pending_host_call(&request).unwrap();
        assert!(pending.program().shares_definition_with(&definition));
        let ValueRef::Str(input) = pending.arguments().next().unwrap() else {
            panic!("string input")
        };
        assert_eq!(input.as_ptr(), pointer);
    }
    turn.pending_host_call(&request)
        .unwrap()
        .dispatch()
        .unwrap();
    assert_eq!(turn.drive(1).unwrap(), RunStatus::Complete);
    assert_eq!(first_work.checked_add(turn.work()).unwrap().fuel, 5);
    let roots = turn.finish().unwrap().into_returned().unwrap();
    let vm = Box::new(vm);
    let mut vm = *vm;
    assert_eq!(
        vm.resolve(&roots).unwrap().values(),
        [Value::Str("owned input payload".into())]
    );
    vm.collect().unwrap();
    assert_eq!(vm.aggregates().len_u32(), 0);
    drop(vm);
    assert_eq!(effects.calls.get(), 1);
    assert_eq!(effects.drops.get(), 1);
    assert!(code.upgrade().is_none());
}

#[test]
fn owned_and_borrowed_observers_share_work_resource_observations_and_replay() {
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
            TurnOptions::default()
                .trace_owned(mask, Rc::clone(&recorder))
                .access_owned(Rc::clone(&reads))
        } else {
            TurnOptions::default()
                .trace(mask, &mut observer)
                .access(&mut access)
        };
        let mut turn = vm.turn(options).unwrap();
        turn.start(&callable, vec![Value::Str("hello".into())])
            .unwrap();
        while turn.drive(1).unwrap() == RunStatus::Suspended {}
        let work = turn.work();
        let roots = turn.finish().unwrap().into_returned().unwrap();
        let outcome = (vm.resolve(&roots).unwrap().values().to_vec(), work);
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
fn aborting_a_turn_preserves_the_vm_and_marks_recorded_evidence_incomplete() {
    let callable = echo();
    let effects = Rc::new(Effects::default());
    let recorder = Rc::new(RefCell::new(Recorder::new(
        Arc::clone(callable.program()),
        RecordLimits::default(),
    )));
    let mask = recorder.borrow().mask();
    let mut vm = Vm::new(Echo(Rc::clone(&effects)), Limits::default());
    let mut turn = vm
        .turn(
            TurnOptions::default()
                .yield_host_calls()
                .trace_owned(mask, Rc::clone(&recorder)),
        )
        .unwrap();
    turn.start(&callable, vec![Value::Str("cancel".into())])
        .unwrap();
    assert!(matches!(
        turn.drive(u64::MAX).unwrap(),
        RunStatus::AwaitingHost(_)
    ));
    drop(turn);
    assert_eq!(vm.aggregates().len_u32(), 0);
    assert_eq!(vm.inspect().unwrap().frames().count(), 0);
    assert_eq!(
        effects.drops.get(),
        0,
        "aborting a turn does not destroy host state"
    );
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
    drop(vm);
    assert_eq!(effects.drops.get(), 1);
}

#[test]
fn failed_entry_and_capture_admission_do_not_dispatch_the_host() {
    let effects = Rc::new(Effects::default());
    {
        let mut vm = Vm::new(Echo(Rc::clone(&effects)), Limits::default());
        assert!(matches!(
            vm.run(echo(), vec![Value::I64(7)], TurnOptions::default()),
            Err(ControlError::Trapped(error)) if matches!(error.trap, Trap::TypeMismatch { .. })
        ));
        assert_eq!(effects.drops.get(), 0);
        assert_eq!(vm.inspect().unwrap().frames().count(), 0);
    }
    {
        let mut vm = Vm::new(
            Echo(Rc::clone(&effects)),
            Limits {
                max_aggregate_work: 1,
                ..Limits::default()
            },
        );
        assert!(matches!(
            vm.run(
                retained_closure(),
                vec![Value::I64(2)],
                TurnOptions::default()
            ),
            Err(ControlError::Input(Trap::AggregateWorkExceeded { .. }))
        ));
        assert_eq!(
            vm.aggregates().len_u32(),
            0,
            "capture admission precedes copying"
        );
    }
    assert_eq!(effects.drops.get(), 2);
    assert_eq!(effects.calls.get(), 0);
}

#[test]
fn retained_closure_entry_charges_capture_import_once_and_replays() {
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
    let mut turn = vm
        .turn(TurnOptions::default().trace_owned(mask, Rc::clone(&recorder)))
        .unwrap();
    turn.start(&callable, vec![Value::I64(2)]).unwrap();
    drop(callable);
    assert_eq!(turn.work().data.aggregate, 2);
    assert_eq!(turn.drive(u64::MAX).unwrap(), RunStatus::Complete);
    let roots = turn.finish().unwrap().into_returned().unwrap();
    assert_eq!(vm.resolve(&roots).unwrap().values(), [Value::I64(42)]);
    let recording = Rc::try_unwrap(recorder).unwrap().into_inner().finish();
    assert!(
        matches!(recording.replay().unwrap(), ReplayOutcome::Returned(result) if result.values() == [Value::I64(42)])
    );
    assert_eq!(effects.drops.get(), 0);
    drop(vm);
    assert_eq!(effects.drops.get(), 1);
}

#[derive(Debug)]
struct GraphGuest {
    vm: Vm<Echo>,
    entry: Option<(Callable, Vec<Value>)>,
    reply: Rc<RefCell<Option<String>>>,
}
impl execution_graph::NodeTask<String, ControlError> for GraphGuest {
    fn poll(
        &mut self,
        _: &mut Context<'_>,
        outputs: &mut Vec<String>,
        access: &mut execution_graph::NodeAccess<'_, String>,
    ) -> Poll<Result<(), ControlError>> {
        let mut turn = self.vm.turn(TurnOptions::default().yield_host_calls())?;
        if let Some((callee, arguments)) = self.entry.take() {
            turn.start(callee, arguments)?;
        }
        loop {
            match turn.drive(u64::MAX) {
                Err(error) => return Poll::Ready(Err(ControlError::Trapped(error))),
                Ok(RunStatus::Suspended) => unreachable!("unlimited quantum"),
                Ok(RunStatus::AwaitingHost(id)) => {
                    access.read_host_state(execution_graph::HostOpId::new(1), 0);
                    let Some(reply) = self.reply.borrow_mut().take() else {
                        assert!(matches!(turn.finish()?, TurnOutcome::AwaitingHost(_)));
                        return Poll::Pending;
                    };
                    turn.pending_host_call(&id)
                        .unwrap()
                        .complete(&mut [Value::Str(reply)], Ok(0))
                        .map_err(ControlError::Trapped)?;
                }
                Ok(RunStatus::AwaitingEffect) => panic!("fixture has no escaping native effect"),
                Ok(RunStatus::Complete) => {
                    let roots = turn.finish()?.into_returned().expect("guest returned");
                    let view = self.vm.resolve(&roots).map_err(ControlError::Retention)?;
                    let [Value::Str(result)] = view.values() else {
                        panic!("string result")
                    };
                    outputs.push(result.clone());
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
    let mut graph = ExecutionGraph::new(FnExecutor::<String, ControlError>::new());
    let node = graph
        .add_node(
            FnNode::deferred({
                let effects = Rc::clone(&effects);
                let replies = Rc::clone(&replies);
                move |inputs: &[String], _| {
                    let reply = Rc::new(RefCell::new(None));
                    replies.borrow_mut().push(Rc::clone(&reply));
                    Ok(Box::new(GraphGuest {
                        vm: Vm::new(Echo(Rc::clone(&effects)), Limits::default()),
                        entry: Some((callable.clone(), vec![Value::Str(inputs[0].clone())])),
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
        fn run_mask(&mut self, _context: execution_tape::trace::TraceContext<'_>, mask: TraceMask) {
            self.subscribed = Some(mask);
        }
        fn wants_registers(
            &self,
            _context: execution_tape::trace::TraceContext<'_>,
            _: TraceLocation,
            _: RegisterPhase,
        ) -> bool {
            self.filters.set(self.filters.get() + 1);
            false
        }
        fn registers(
            &mut self,
            _context: execution_tape::trace::TraceContext<'_>,
            _: RegisterEvent<'_>,
        ) {
            panic!("register filtering was not forwarded");
        }
        fn run_start(
            &mut self,
            _context: execution_tape::trace::TraceContext<'_>,
            _: FuncId,
            _: usize,
        ) {
            self.starts += 1;
        }
        fn run_end(
            &mut self,
            _context: execution_tape::trace::TraceContext<'_>,
            outcome: TraceOutcome<'_>,
        ) {
            assert!(matches!(outcome, TraceOutcome::Ok));
            self.ends += 1;
        }
        fn values(&mut self, _context: execution_tape::trace::TraceContext<'_>, _: ValueEvent<'_>) {
            self.values += 1;
        }
        fn instr(
            &mut self,
            _context: execution_tape::trace::TraceContext<'_>,
            _: FuncId,
            _: u32,
            _: u32,
            _: Option<u64>,
            _: u8,
        ) {
            self.instructions += 1;
        }
        fn instr_end(
            &mut self,
            _context: execution_tape::trace::TraceContext<'_>,
            _: TraceLocation,
            _: Option<TraceLocation>,
            _: TraceOutcome<'_>,
        ) {
            self.completions += 1;
        }
        fn scope_enter(
            &mut self,
            _context: execution_tape::trace::TraceContext<'_>,
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
            _context: execution_tape::trace::TraceContext<'_>,
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
    let mut vm = Vm::new(Echo(Rc::new(Effects::default())), Limits::default());
    let _roots = vm
        .run(
            callable,
            vec![Value::Str("observed".into())],
            TurnOptions::default().trace_owned(mask(), Rc::clone(&observer)),
        )
        .unwrap()
        .into_returned()
        .unwrap();
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
        let mut vm = Vm::new(BorrowedHost(&calls), Limits::default());
        let roots = vm
            .run(
                echo(),
                vec![Value::Str("scoped".into())],
                TurnOptions::default(),
            )
            .unwrap()
            .into_returned()
            .unwrap();
        assert_eq!(
            vm.resolve(&roots).unwrap().values(),
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
fn transferring_an_idle_vms_heap_moves_nested_payloads_and_preserves_code_ownership() {
    let target = pack_text();
    let definition = target.program().as_ref().clone();
    let effects = Rc::new(Effects::default());
    let mut vm = Vm::new(Echo(effects.clone()), Limits::default());
    let roots = vm
        .run(
            &target,
            vec![Value::Str("packed".into())],
            TurnOptions::default(),
        )
        .unwrap()
        .into_returned()
        .unwrap();
    drop(target);
    let (owner, values, pointer) = {
        let view = vm.resolve(&roots).unwrap();
        let Value::Agg(root) = view.values()[0] else {
            panic!("root")
        };
        let Value::Str(text) = view.aggregates().tuple_get_ref(root, 0).unwrap() else {
            panic!("text")
        };
        (
            view.instance().clone(),
            view.values().to_vec(),
            text.as_ptr(),
        )
    };
    assert!(owner.program().shares_definition_with(&definition));
    assert!(
        vm.take_aggregates().is_err(),
        "live registrations prevent transferring their storage"
    );
    drop(roots);
    let heap = vm.take_aggregates().unwrap();
    drop(vm);
    let (snapshot, _) = heap.into_snapshot(values).unwrap();
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
    assert!(
        owner.program().shares_definition_with(&definition),
        "the exported nominal owner remains available after VM retirement"
    );
    assert_eq!(
        text, "packed",
        "payload remains owned after VM/code retirement"
    );
}

#[test]
fn independent_snapshots_preserve_live_vm_storage_and_aborting_does_not_execute() {
    let target = pack_text();
    let mut vm = Vm::new(Echo(Rc::new(Effects::default())), Limits::default());
    let args = [Value::Str("borrowed".into())];
    let roots = vm
        .run(&target, &args, TurnOptions::default())
        .unwrap()
        .into_returned()
        .unwrap();
    let (original, snapshot) = {
        let view = vm.resolve(&roots).unwrap();
        let Value::Agg(original) = view.values()[0] else {
            panic!("root")
        };
        (original, view.aggregates().snapshot(view.values()).unwrap())
    };
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
    let mut cancelled = Vm::new(Echo(effects.clone()), Limits::default());
    let mut turn = cancelled.turn(TurnOptions::default()).unwrap();
    turn.start(target, &args).unwrap();
    drop(turn);
    assert_eq!(cancelled.inspect().unwrap().frames().count(), 0);
    assert_eq!(cancelled.aggregates().len_u32(), 0);
    drop(cancelled);
    assert_eq!(effects.drops.get(), 1);
    assert_eq!(
        effects.calls.get(),
        0,
        "aborting an unfinished turn cancels without executing guest code"
    );
}
