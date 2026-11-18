// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! A host's synchronous imports are answered inline even in turns that yield host calls,
//! spending and observing exactly what a yielded, dispatched call does.

use execution_tape::{
    asm::{FunctionSig, ProgramBuilder, Reg},
    host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef},
    program::{HostSigId, Program, SpanId, ValueType},
    recording::{RecordLimits, Recorder, ReplayOutcome},
    trace::{ScopeKind, TraceLocation, TraceMask, TraceOutcome, TraceSink, ValueEvent},
    value::{FuncId, FuncRef, Value},
    verifier::VerifiedProgram,
    vm::{Limits, RunStatus, Trap, Turn, TurnOptions, TurnOutcome, Vm},
};
use std::{cell::RefCell, rc::Rc, sync::Arc};

const CONVERT: &str = "convert";
const ASK: &str = "ask";

/// Answers `convert(n) = n + 1` and `ask(n) = n * 10`. With `SYNC`, `convert` is synchronous.
#[derive(Default)]
struct Answers<const SYNC: bool> {
    calls: Vec<&'static str>,
}
impl<const SYNC: bool> Host for Answers<SYNC> {
    fn call(
        &mut self,
        symbol: &str,
        _: SigHash,
        args: &[ValueRef<'_>],
        rets: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        let [ValueRef::I64(n)] = args else {
            return Err(HostError::SignatureMismatch);
        };
        match symbol {
            CONVERT => {
                self.calls.push(CONVERT);
                rets[0] = Value::I64(n + 1);
                Ok(3)
            }
            ASK => {
                self.calls.push(ASK);
                rets[0] = Value::I64(n * 10);
                Ok(0)
            }
            _ => Err(HostError::Failed),
        }
    }

    fn is_synchronous(&self, symbol: &str, _: SigHash) -> bool {
        SYNC && symbol == CONVERT
    }
}

/// `entry(n)` threads `n` through these host calls in order, then returns it.
fn program(calls: &[&str]) -> (Arc<VerifiedProgram>, FuncId) {
    let mut b = ProgramBuilder::new();
    let sig = HostSig {
        args: vec![ValueType::I64],
        rets: vec![ValueType::I64],
    };
    let convert = b.host_sig_for(CONVERT, sig.clone());
    let ask = b.host_sig_for(ASK, sig);
    let entry = b
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let mut a = b.function_asm(entry).unwrap();
    for &call in calls {
        let import: HostSigId = if call == CONVERT { convert } else { ask };
        a.host_call_sig_id(Reg::EFFECT, import, Reg::EFFECT, &[Reg(1)], &[Reg(1)]);
    }
    a.ret(Reg::EFFECT, &[Reg(1)]);
    b.define_function(entry, a).unwrap();
    (Arc::new(b.build_verified().unwrap()), entry)
}

fn returned<H: Host>(turn: Turn<'_, H>) -> Vec<Value> {
    match turn.finish() {
        Ok(TurnOutcome::Returned(values)) => values,
        _ => panic!("the turn returned"),
    }
}

#[test]
fn a_yielding_turn_stops_only_at_imports_that_are_not_synchronous() {
    let (program, entry) = program(&[CONVERT, ASK, CONVERT]);
    let mut vm = Vm::new(Answers::<true>::default(), Limits::default());
    let unit = vm.install(program).unwrap();
    let args = [Value::I64(1)];
    let mut turn = vm
        .start(
            FuncRef::new(unit, entry),
            &args,
            TurnOptions::default().yield_host_calls(),
        )
        .unwrap();
    assert_eq!(turn.drive(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    let request = turn.pending_host_call().unwrap();
    assert_eq!(request.symbol(), ASK);
    // The synchronous call before it already ran.
    assert_eq!(request.arguments().next(), Some(ValueRef::I64(2)));
    request.complete(&mut [Value::I64(20)], Ok(0)).unwrap();
    assert_eq!(turn.drive(u64::MAX).unwrap(), RunStatus::Complete);
    assert_eq!(turn.work().host_calls, 3);
    assert_eq!(returned(turn), [Value::I64(21)]);
    assert_eq!(vm.host().calls, [CONVERT, CONVERT]);
}

#[test]
fn synchronous_imports_change_nothing_without_yielded_host_calls() {
    let (program, entry) = program(&[CONVERT, ASK, CONVERT]);
    for yields in [false, true] {
        let mut vm = Vm::new(Answers::<false>::default(), Limits::default());
        let unit = vm.install(Arc::clone(&program)).unwrap();
        let args = [Value::I64(1)];
        let options = TurnOptions::default();
        let options = if yields {
            options.yield_host_calls()
        } else {
            options
        };
        let mut turn = vm.start(FuncRef::new(unit, entry), &args, options).unwrap();
        let mut yielded = 0;
        loop {
            match turn.drive(u64::MAX).unwrap() {
                RunStatus::Complete => break,
                RunStatus::AwaitingHost => {
                    yielded += 1;
                    turn.pending_host_call().unwrap().dispatch().unwrap();
                }
                status => panic!("unexpected {status:?}"),
            }
        }
        assert_eq!(yielded, if yields { 3 } else { 0 });
        assert_eq!(returned(turn), [Value::I64(21)]);
    }
}

/// A trace observer that logs scopes, host values and the run's end.
#[derive(Clone, Default)]
struct Log(Rc<RefCell<Vec<String>>>);
impl TraceSink for Log {
    fn instr(
        &mut self,
        _: &Program,
        at: TraceLocation,
        _: execution_tape::program::Pc,
        _: Option<SpanId>,
        _: u16,
    ) {
        self.0.borrow_mut().push(format!("instr {:?}", at.pc));
    }
    fn scope_enter(
        &mut self,
        _: &Program,
        kind: ScopeKind,
        depth: usize,
        at: TraceLocation,
        _: Option<SpanId>,
    ) {
        self.0
            .borrow_mut()
            .push(format!("enter {kind:?} {depth} {:?}", at.pc));
    }
    fn scope_exit(
        &mut self,
        _: &Program,
        kind: ScopeKind,
        depth: usize,
        at: TraceLocation,
        _: Option<SpanId>,
        outcome: TraceOutcome<'_>,
    ) {
        self.0
            .borrow_mut()
            .push(format!("exit {kind:?} {depth} {:?} {outcome:?}", at.pc));
    }
    fn values(&mut self, _: &Program, event: ValueEvent<'_>) {
        if let ValueEvent::HostCall {
            location,
            args,
            outcome,
            work,
            ..
        } = event
        {
            self.0.borrow_mut().push(format!(
                "host {:?} {args:?} {outcome:?} {work:?}",
                location.pc
            ));
        }
    }
    fn run_end(&mut self, _: &Program, outcome: TraceOutcome<'_>) {
        self.0.borrow_mut().push(format!("end {outcome:?}"));
    }
}

/// Drives a yielding turn of `H`, dispatching every request, and returns its observations,
/// work and result.
fn observe<H: Host + Default>(
    program: &Arc<VerifiedProgram>,
    entry: FuncId,
) -> (Vec<String>, String, Vec<Value>) {
    let log = Log::default();
    let mut sink = log.clone();
    let mut vm = Vm::new(H::default(), Limits::default());
    let unit = vm.install(Arc::clone(program)).unwrap();
    let args = [Value::I64(1)];
    let mut turn = vm
        .start(
            FuncRef::new(unit, entry),
            &args,
            TurnOptions::default().yield_host_calls().trace(
                TraceMask::RUN
                    | TraceMask::INSTR
                    | TraceMask::CALL
                    | TraceMask::HOST
                    | TraceMask::VALUES,
                &mut sink,
            ),
        )
        .unwrap();
    loop {
        match turn.drive(1).unwrap() {
            RunStatus::Complete => break,
            RunStatus::Suspended => {}
            RunStatus::AwaitingHost => turn.pending_host_call().unwrap().dispatch().unwrap(),
            status => panic!("unexpected {status:?}"),
        }
    }
    let work = format!("{:?}", turn.work());
    let values = returned(turn);
    let observations = log.0.borrow().clone();
    (observations, work, values)
}

#[test]
fn a_synchronous_call_spends_and_observes_what_a_dispatched_one_does() {
    let (program, entry) = program(&[CONVERT, ASK, CONVERT, CONVERT]);
    let dispatched = observe::<Answers<false>>(&program, entry);
    let synchronous = observe::<Answers<true>>(&program, entry);
    assert_eq!(synchronous, dispatched);
    assert_eq!(synchronous.2, [Value::I64(22)]);
}

#[test]
fn synchronous_calls_count_against_the_host_call_allowance() {
    let (program, entry) = program(&[CONVERT, CONVERT]);
    let mut vm = Vm::new(
        Answers::<true>::default(),
        Limits {
            max_host_calls: 1,
            ..Limits::default()
        },
    );
    let unit = vm.install(program).unwrap();
    let args = [Value::I64(1)];
    let mut turn = vm
        .start(
            FuncRef::new(unit, entry),
            &args,
            TurnOptions::default().yield_host_calls(),
        )
        .unwrap();
    let trap = turn.drive(u64::MAX).unwrap_err();
    assert!(matches!(trap.trap, Trap::HostCallLimitExceeded));
    drop(turn);
    assert_eq!(vm.host().calls, [CONVERT]);
}

#[test]
fn a_failing_synchronous_call_traps_where_it_runs() {
    #[derive(Default)]
    struct Failing;
    impl Host for Failing {
        fn call(
            &mut self,
            _: &str,
            _: SigHash,
            _: &[ValueRef<'_>],
            _: &mut [Value],
            _: HostContext<'_, '_>,
        ) -> Result<u64, HostError> {
            Err(HostError::Failed)
        }
        fn is_synchronous(&self, _: &str, _: SigHash) -> bool {
            true
        }
    }
    let (program, entry) = program(&[CONVERT]);
    let mut vm = Vm::new(Failing, Limits::default());
    let unit = vm.install(program).unwrap();
    let args = [Value::I64(1)];
    let mut turn = vm
        .start(
            FuncRef::new(unit, entry),
            &args,
            TurnOptions::default().yield_host_calls(),
        )
        .unwrap();
    let trap = turn.drive(u64::MAX).unwrap_err();
    assert!(matches!(trap.trap, Trap::HostCallFailed { .. }));
    assert!(matches!(turn.finish(), Ok(TurnOutcome::Trapped(_))));
}

#[test]
fn a_computation_saved_around_synchronous_calls_replays() {
    let (program, entry) = program(&[CONVERT, CONVERT, ASK, CONVERT, ASK, CONVERT]);
    let mut vm = Vm::new(Answers::<true>::default(), Limits::default());
    let unit = vm.install(Arc::clone(&program)).unwrap();
    let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
    let mask = recorder.mask();
    let args = [Value::I64(1)];
    let mut turn = vm
        .start(
            FuncRef::new(unit, entry),
            &args,
            TurnOptions::default()
                .yield_host_calls()
                .trace(mask, &mut recorder),
        )
        .unwrap();
    // Save at every instruction boundary and at every request, so saves fall right before
    // and right after synchronous calls.
    let values = loop {
        let status = turn.drive(1).unwrap();
        if status == RunStatus::Complete {
            break returned(turn);
        }
        let id = match turn.finish() {
            Ok(TurnOutcome::Saved(id)) => id,
            _ => panic!("the computation saves"),
        };
        turn = vm
            .resume(
                id,
                TurnOptions::default()
                    .yield_host_calls()
                    .trace(mask, &mut recorder),
            )
            .unwrap();
        if status == RunStatus::AwaitingHost {
            turn.pending_host_call().unwrap().dispatch().unwrap();
        }
    };
    assert_eq!(values, [Value::I64(311)]);
    let recording = recorder.finish();
    assert_eq!(recording.status(), Ok(()));
    let ReplayOutcome::Returned(replayed) = recording.replay().unwrap() else {
        panic!("replay returned");
    };
    assert_eq!(replayed.values(), values);
}

#[test]
fn a_guest_callback_answers_synchronous_calls_inline() {
    let mut b = ProgramBuilder::new();
    let sig = HostSig {
        args: vec![ValueType::I64],
        rets: vec![ValueType::I64],
    };
    let convert = b.host_sig_for(CONVERT, sig.clone());
    let ask = b.host_sig_for(ASK, sig);
    let fsig = FunctionSig {
        arg_types: vec![ValueType::I64],
        ret_types: vec![ValueType::I64],
    };
    let entry = b.declare_function(fsig.clone()).unwrap();
    let callback = b.declare_function(fsig).unwrap();
    let mut a = b.function_asm(entry).unwrap();
    a.host_call_sig_id(Reg::EFFECT, ask, Reg::EFFECT, &[Reg(1)], &[Reg(1)])
        .ret(Reg::EFFECT, &[Reg(1)]);
    b.define_function(entry, a).unwrap();
    let mut a = b.function_asm(callback).unwrap();
    a.host_call_sig_id(Reg::EFFECT, convert, Reg::EFFECT, &[Reg(1)], &[Reg(1)])
        .ret(Reg::EFFECT, &[Reg(1)]);
    b.define_function(callback, a).unwrap();
    let program = Arc::new(b.build_verified().unwrap());
    let mut vm = Vm::new(Answers::<true>::default(), Limits::default());
    let unit = vm.install(program).unwrap();
    let args = [Value::I64(4)];
    let mut turn = vm
        .start(
            FuncRef::new(unit, entry),
            &args,
            TurnOptions::default().yield_host_calls(),
        )
        .unwrap();
    assert_eq!(turn.drive(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    turn.pending_host_call()
        .unwrap()
        .call_guest(
            Value::Func(FuncRef::new(unit, callback)),
            vec![Value::I64(4)],
        )
        .unwrap();
    // The callback's synchronous call does not stop it.
    assert_eq!(turn.drive(u64::MAX).unwrap(), RunStatus::CallbackReturned);
    let mut request = turn.pending_host_call().unwrap();
    assert_eq!(request.take_callback_results(), Some(vec![Value::I64(5)]));
    request.complete(&mut [Value::I64(50)], Ok(0)).unwrap();
    assert_eq!(turn.drive(u64::MAX).unwrap(), RunStatus::Complete);
    assert_eq!(returned(turn), [Value::I64(50)]);
}

#[test]
fn a_replaced_host_answers_for_its_own_type() {
    // Installed under a host with no synchronous imports, then run under one with some, as an
    // embedding that parks its VM with an idle host and lends it a working one per call does.
    let (program, entry) = program(&[CONVERT, ASK, CONVERT]);
    let mut parked = Vm::new(Answers::<false>::default(), Limits::default());
    let unit = parked.install(program).unwrap();
    let (mut vm, _) = parked.replace_host(Answers::<true>::default());
    let args = [Value::I64(1)];
    let mut turn = vm
        .start(
            FuncRef::new(unit, entry),
            &args,
            TurnOptions::default().yield_host_calls(),
        )
        .unwrap();
    assert_eq!(turn.drive(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    assert_eq!(turn.pending_host_call().unwrap().symbol(), ASK);
    turn.pending_host_call().unwrap().dispatch().unwrap();
    assert_eq!(turn.drive(u64::MAX).unwrap(), RunStatus::Complete);
    assert_eq!(returned(turn), [Value::I64(21)]);
    // And back: the parked host's type yields every call again.
    let (mut vm, _) = vm.replace_host(Answers::<false>::default());
    let mut turn = vm
        .start(
            FuncRef::new(unit, entry),
            &args,
            TurnOptions::default().yield_host_calls(),
        )
        .unwrap();
    assert_eq!(turn.drive(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    assert_eq!(turn.pending_host_call().unwrap().symbol(), CONVERT);
}

/// Builds `(n, n + 1)` for `make(n)`, and raises `failed((x,))` with a tuple it builds for
/// `risky(x)`. With `SYNC`, both are synchronous.
struct Builder<const SYNC: bool> {
    failed: execution_tape::program::TagId,
}
impl<const SYNC: bool> Host for Builder<SYNC> {
    fn call(
        &mut self,
        symbol: &str,
        _: SigHash,
        args: &[ValueRef<'_>],
        rets: &mut [Value],
        mut ctx: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        let [ValueRef::I64(n)] = args else {
            return Err(HostError::SignatureMismatch);
        };
        let n = *n;
        match symbol {
            "make" => {
                rets[0] = Value::Agg(ctx.tuple_new(vec![Value::I64(n), Value::I64(n + 1)])?);
                Ok(1)
            }
            "risky" => {
                let tag = ctx.tag(self.failed).expect("declared tag");
                let payload = ctx.tuple_new(vec![Value::I64(n)])?;
                Err(HostError::throw(tag, vec![Value::Agg(payload)]))
            }
            _ => Err(HostError::Failed),
        }
    }

    fn is_synchronous(&self, _: &str, _: SigHash) -> bool {
        SYNC
    }
}

/// Logs every observation, with registers at every instruction.
#[derive(Clone, Default)]
struct Everything(Rc<RefCell<Vec<String>>>);
impl TraceSink for Everything {
    fn wants_registers(&self, _: TraceLocation, _: execution_tape::trace::RegisterPhase) -> bool {
        true
    }
    fn registers(&mut self, _: &Program, event: execution_tape::trace::RegisterEvent<'_>) {
        self.0.borrow_mut().push(format!("registers {event:?}"));
    }
    fn run_start(&mut self, _: &Program, entry: FuncRef, count: usize) {
        self.0.borrow_mut().push(format!("start {entry:?} {count}"));
    }
    fn values(&mut self, _: &Program, event: ValueEvent<'_>) {
        self.0.borrow_mut().push(format!("values {event:?}"));
    }
    fn instr(
        &mut self,
        _: &Program,
        at: TraceLocation,
        next: execution_tape::program::Pc,
        _: Option<SpanId>,
        opcode: u16,
    ) {
        self.0
            .borrow_mut()
            .push(format!("instr {at:?} {next:?} {opcode}"));
    }
    fn instr_end(
        &mut self,
        _: &Program,
        at: TraceLocation,
        next: Option<TraceLocation>,
        outcome: TraceOutcome<'_>,
    ) {
        self.0
            .borrow_mut()
            .push(format!("end {at:?} {next:?} {outcome:?}"));
    }
    fn scope_enter(
        &mut self,
        _: &Program,
        kind: ScopeKind,
        depth: usize,
        at: TraceLocation,
        _: Option<SpanId>,
    ) {
        self.0
            .borrow_mut()
            .push(format!("enter {kind:?} {depth} {at:?}"));
    }
    fn scope_exit(
        &mut self,
        _: &Program,
        kind: ScopeKind,
        depth: usize,
        at: TraceLocation,
        _: Option<SpanId>,
        outcome: TraceOutcome<'_>,
    ) {
        self.0
            .borrow_mut()
            .push(format!("exit {kind:?} {depth} {at:?} {outcome:?}"));
    }
    fn run_end(&mut self, _: &Program, outcome: TraceOutcome<'_>) {
        self.0.borrow_mut().push(format!("run end {outcome:?}"));
    }
}

#[test]
fn synchronous_calls_that_build_aggregates_and_throw_match_dispatched_ones_event_for_event() {
    use execution_tape::asm::Catch;
    use execution_tape::contract::Contract;
    // entry(n): a = first(make(n)); try risky(a) catch failed((x,)) => x + 100, where
    // first((a, b)) = a checks the host's tuple against its contract on entry.
    let mut b = ProgramBuilder::new();
    let integer = b.contract(Contract::Kind(ValueType::I64)).unwrap();
    let single = b.contract(Contract::Tuple(&[Some(integer)])).unwrap();
    let failed = b.declare_tag("failed", &[single.into()]).unwrap();
    let make = b.host_sig_for(
        "make",
        HostSig {
            args: vec![ValueType::I64],
            rets: vec![ValueType::Agg],
        },
    );
    let risky = b.host_sig_for(
        "risky",
        HostSig {
            args: vec![ValueType::I64],
            rets: vec![ValueType::I64],
        },
    );
    let pair_contract = b
        .contract(Contract::Tuple(&[Some(integer), Some(integer)]))
        .unwrap();
    let first_of = b
        .declare_function(execution_tape::asm::FunctionAbi {
            args: vec![pair_contract.into()],
            rets: vec![integer.into()],
        })
        .unwrap();
    let mut a = b.function_asm(first_of).unwrap();
    let field = a.alloc_register(ValueType::I64);
    a.tuple_get(field, Reg(1), 0).ret(Reg::EFFECT, &[field]);
    b.define_function(first_of, a).unwrap();
    let entry = b
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let mut a = b.function_asm(entry).unwrap();
    let pair = a.alloc_register(ValueType::Agg);
    let first = a.alloc_register(ValueType::I64);
    let caught = a.alloc_register(ValueType::Agg);
    let hundred = a.alloc_register(ValueType::I64);
    let handler = a.block(&[caught]);
    a.host_call_sig_id(Reg::EFFECT, make, Reg::EFFECT, &[Reg(1)], &[pair])
        .call(Reg::EFFECT, first_of, Reg::EFFECT, &[pair], &[first]);
    a.begin_region(&[Catch::Tag(failed, handler)]);
    a.host_call_sig_id(Reg::EFFECT, risky, Reg::EFFECT, &[first], &[first]);
    a.end_region();
    a.ret(Reg::EFFECT, &[first]);
    a.place(handler).unwrap();
    a.tuple_get(first, caught, 0)
        .const_i64(hundred, 100)
        .i64_add(first, first, hundred)
        .ret(Reg::EFFECT, &[first]);
    b.define_function(entry, a).unwrap();
    let program = Arc::new(b.build_verified().unwrap());

    fn observe<H: Host>(
        host: H,
        program: &Arc<VerifiedProgram>,
        entry: FuncId,
    ) -> (Vec<String>, String, Vec<Value>) {
        let log = Everything::default();
        let mut sink = log.clone();
        let mut vm = Vm::new(host, Limits::default());
        let unit = vm.install(Arc::clone(program)).unwrap();
        let args = [Value::I64(4)];
        let mask = TraceMask::RUN
            | TraceMask::INSTR
            | TraceMask::CALL
            | TraceMask::HOST
            | TraceMask::VALUES
            | TraceMask::REGISTERS;
        let mut turn = vm
            .start(
                FuncRef::new(unit, entry),
                &args,
                TurnOptions::default()
                    .yield_host_calls()
                    .trace(mask, &mut sink),
            )
            .unwrap();
        loop {
            match turn.drive(1).unwrap() {
                RunStatus::Complete => break,
                RunStatus::Suspended => {}
                RunStatus::AwaitingHost => turn.pending_host_call().unwrap().dispatch().unwrap(),
                status => panic!("unexpected {status:?}"),
            }
        }
        let work = format!("{:?}", turn.work());
        let values = returned(turn);
        let events = log.0.borrow().clone();
        (events, work, values)
    }
    let dispatched = observe(Builder::<false> { failed }, &program, entry);
    let synchronous = observe(Builder::<true> { failed }, &program, entry);
    assert_eq!(synchronous.2, [Value::I64(104)]);
    assert!(synchronous.0.len() > 20, "a full trace");
    assert_eq!(synchronous.0, dispatched.0);
    assert_eq!(synchronous.1, dispatched.1);
    assert_eq!(synchronous.2, dispatched.2);
}

#[test]
fn a_bound_host_answers_the_imports_its_registry_marks_inline() {
    use execution_tape::host_binding::{HostFunctions, HostImport};
    const CONVERT_IMPORT: HostImport =
        HostImport::new(CONVERT, &[ValueType::I64], &[ValueType::I64]);
    const ASK_IMPORT: HostImport = HostImport::new(ASK, &[ValueType::I64], &[ValueType::I64]);
    let (program, entry) = program(&[CONVERT, ASK, CONVERT]);
    let mut functions = HostFunctions::<u32>::new();
    functions
        .implement(CONVERT_IMPORT, |calls, args, rets, _| {
            let [ValueRef::I64(n)] = args else {
                return Err(HostError::SignatureMismatch);
            };
            *calls += 1;
            rets[0] = Value::I64(n + 1);
            Ok(0)
        })
        .unwrap();
    functions
        .implement(ASK_IMPORT, |_, _, _, _| Err(HostError::Failed))
        .unwrap();
    assert!(functions.mark_synchronous(CONVERT_IMPORT));
    assert!(!functions.mark_synchronous(HostImport::new(CONVERT, &[ValueType::U64], &[])));
    let host = functions.bind(&program).unwrap().with_state(0);
    let mut vm = Vm::new(host, Limits::default());
    let unit = vm.install(program).unwrap();
    let args = [Value::I64(1)];
    let mut turn = vm
        .start(
            FuncRef::new(unit, entry),
            &args,
            TurnOptions::default().yield_host_calls(),
        )
        .unwrap();
    assert_eq!(turn.drive(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    let request = turn.pending_host_call().unwrap();
    assert_eq!(request.symbol(), ASK);
    request.complete(&mut [Value::I64(20)], Ok(0)).unwrap();
    assert_eq!(turn.drive(u64::MAX).unwrap(), RunStatus::Complete);
    assert_eq!(returned(turn), [Value::I64(21)]);
    assert_eq!(*vm.host().state(), 2);
}
