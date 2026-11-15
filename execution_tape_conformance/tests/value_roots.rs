// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Registered value roots and explicit, moving collection.

use execution_tape::{
    aggregates::{AggError, AggHeap},
    asm::{Catch, FunctionAbi, FunctionSig, ProgramBuilder, Reg},
    contract::Contract,
    host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef},
    program::{TagId, ValueType},
    recording::{CaptureFault, RecordLimits, Recorder, ReplayOutcome},
    trace::{TraceMask, TraceSink, ValueEvent},
    value::{AggHandle, Closure, FuncId, FuncRef, Value},
    verifier::VerifiedProgram,
    vm::{
        CollectError, CollectionReport, Invocation, InvocationOptions, Limits, Retention,
        RootError, RunStatus, Trap, Vm,
    },
};
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
        panic!("the embedding completes every yielded request")
    }
}

fn sig(args: &[ValueType], rets: &[ValueType]) -> FunctionSig {
    FunctionSig {
        arg_types: args.to_vec(),
        ret_types: rets.to_vec(),
    }
}

fn aggregate(value: &Value) -> AggHandle {
    match value {
        Value::Agg(handle) => *handle,
        other => panic!("expected an aggregate, got {other:?}"),
    }
}

fn field(heap: &AggHeap, tuple: AggHandle, index: usize) -> &Value {
    heap.tuple_get_ref(tuple, index).unwrap()
}

#[test]
fn moving_collection_preserves_sharing_distinct_identity_and_owned_payloads() {
    let mut vm = Vm::new(NoHost, Limits::default());
    let heap = vm.aggregates_mut();
    heap.tuple_new(vec![]).unwrap(); // Unreachable, so every survivor moves.
    let text = String::from("shared payload");
    let text_at = text.as_ptr();
    let child = heap.tuple_new(vec![Value::Str(text)]).unwrap();
    let parent = heap
        .tuple_new(vec![Value::Agg(child), Value::Agg(child)])
        .unwrap();
    let distinct = heap
        .tuple_new(vec![Value::Str("shared payload".into())])
        .unwrap();
    let owned = String::from("owned root");
    let owned_at = owned.as_ptr();
    let root = vm
        .root(vec![
            Value::Agg(parent),
            Value::Agg(distinct),
            Value::Str(owned),
        ])
        .unwrap();

    let report = vm.collect().unwrap();
    assert_eq!(
        report.aggregates,
        Retention {
            live: 3,
            reclaimed: 1
        }
    );
    assert_eq!(report.moved, 3);
    assert_eq!((report.root_groups, report.released_groups), (1, 0));
    assert_eq!(report.roots, 3);
    assert!(report.aggregate_bytes.live > report.aggregate_bytes.reclaimed);

    let values = vm.resolve(&root).unwrap();
    let heap = vm.aggregates();
    let parent = aggregate(&values[0]);
    let (left, right) = (
        aggregate(field(heap, parent, 0)),
        aggregate(field(heap, parent, 1)),
    );
    assert_eq!(left, right, "shared structure stays shared");
    assert_ne!(left, aggregate(&values[1]), "equal nodes stay distinct");
    let Value::Str(text) = field(heap, left, 0) else {
        panic!("string field")
    };
    assert_eq!(text.as_ptr(), text_at, "payloads move with their node");
    let Value::Str(owned) = &values[2] else {
        panic!("owned root")
    };
    assert_eq!(owned.as_ptr(), owned_at, "registration moves root payloads");

    drop(root);
    let report = vm.collect().unwrap();
    assert_eq!(
        report.aggregates,
        Retention {
            live: 0,
            reclaimed: 3
        }
    );
    assert_eq!((report.root_groups, report.released_groups), (0, 1));
    assert_eq!(vm.aggregates().len_u32(), 0);
}

/// `make(n)` returns `(n,)` after allocating a tuple nothing keeps.
fn revisions() -> (VerifiedProgram, FuncId) {
    let mut b = ProgramBuilder::new();
    let make = b
        .declare_function(sig(&[ValueType::I64], &[ValueType::Agg]))
        .unwrap();
    let mut a = b.function_asm(make).unwrap();
    let n = b.argument(make, 0).unwrap();
    let tuple = a.alloc_register(ValueType::Agg);
    a.tuple_new(tuple, &[])
        .tuple_new(tuple, &[n])
        .ret(Reg::EFFECT, &[tuple]);
    b.define_function(make, a).unwrap();
    (b.build_verified().unwrap(), make)
}

#[test]
fn replacement_histories_reclaim_obsolete_results() {
    let (program, make) = revisions();
    let mut vm = Vm::new(NoHost, Limits::default());
    let make = FuncRef::new(vm.install(program).unwrap(), make);
    let mut current = None;
    for revision in 0..200 {
        let outputs = vm
            .run(make, &[Value::I64(revision)], TraceMask::NONE, None)
            .unwrap();
        // Replacing the handle drops the previous revision's registration.
        current = Some(vm.root(outputs).unwrap());
        let report = vm.collect().unwrap();
        assert_eq!(report.aggregates.live, 1);
        assert_eq!(report.root_groups, 1);
        let values = vm.resolve(current.as_ref().unwrap()).unwrap();
        assert_eq!(
            field(vm.aggregates(), aggregate(&values[0]), 0),
            &Value::I64(revision)
        );
        let stats = vm.heap_stats();
        assert_eq!(stats.root_groups, 1, "dropped registrations are reused");
        assert_eq!(stats.aggregates, 1);
    }
    assert_eq!(vm.heap_stats().collections, 200);
    let values = vm.release(current.unwrap()).unwrap();
    assert_eq!(values.len(), 1);
    assert_eq!(vm.collect().unwrap().aggregates.live, 0);
}

#[test]
fn failed_execution_rolls_back_its_suffix_without_losing_older_roots() {
    let mut b = ProgramBuilder::new();
    let entry = b.declare_function(sig(&[], &[])).unwrap();
    let mut a = b.function_asm(entry).unwrap();
    let tuple = a.alloc_register(ValueType::Agg);
    a.tuple_new(tuple, &[]).trap(77);
    b.define_function(entry, a).unwrap();
    let mut vm = Vm::new(NoHost, Limits::default());
    let entry = FuncRef::new(vm.install(b.build_verified().unwrap()).unwrap(), entry);
    let kept = vm.aggregates_mut().tuple_new(vec![Value::I64(42)]).unwrap();
    let root = vm.root(vec![Value::Agg(kept)]).unwrap();
    vm.run(entry, &[], TraceMask::NONE, None).unwrap_err();
    assert_eq!(vm.aggregates().len_u32(), 1);
    assert_eq!(vm.collect().unwrap().aggregates.live, 1);
    let values = vm.resolve(&root).unwrap();
    assert_eq!(
        field(vm.aggregates(), aggregate(&values[0]), 0),
        &Value::I64(42)
    );
}

/// Declares a closure body over an environment `(i64,)`, returning `i64`.
fn captured_body(b: &mut ProgramBuilder) -> FuncId {
    let integer = b.contract(Contract::Kind(ValueType::I64)).unwrap();
    let captured = b.contract(Contract::Tuple(&[Some(integer)])).unwrap();
    b.declare_function(FunctionAbi {
        args: vec![captured.into()],
        rets: vec![integer.into()],
    })
    .unwrap()
}

/// `body(env)` returns the integer its environment holds, and `apply(f)` calls the closure `f`.
fn closures() -> (VerifiedProgram, FuncId, FuncId) {
    let mut b = ProgramBuilder::new();
    let body = captured_body(&mut b);
    let apply = b
        .declare_function(sig(&[ValueType::Closure], &[ValueType::I64]))
        .unwrap();
    let thunk = b.call_sig(sig(&[], &[ValueType::I64])).unwrap();
    let mut a = b.function_asm(body).unwrap();
    let env = b.argument(body, 0).unwrap();
    let n = a.alloc_register(ValueType::I64);
    a.tuple_get(n, env, 0).ret(Reg::EFFECT, &[n]);
    b.define_function(body, a).unwrap();
    let mut a = b.function_asm(apply).unwrap();
    let f = b.argument(apply, 0).unwrap();
    let n = a.alloc_register(ValueType::I64);
    a.call_indirect(Reg::EFFECT, thunk, f, Reg::EFFECT, &[], &[n])
        .ret(Reg::EFFECT, &[n]);
    b.define_function(apply, a).unwrap();
    (b.build_verified().unwrap(), body, apply)
}

#[test]
fn independent_groups_share_storage_and_remap_closure_environments() {
    let (program, body, apply) = closures();
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(program).unwrap();
    let heap = vm.aggregates_mut();
    heap.tuple_new(vec![]).unwrap();
    let shared = heap.tuple_new(vec![Value::I64(42)]).unwrap();
    let parent = heap.tuple_new(vec![Value::Agg(shared)]).unwrap();
    let ordinary = vm.root(vec![Value::Agg(parent)]).unwrap();
    let callable = vm
        .root(vec![Value::Closure(Closure {
            func: FuncRef::new(unit, body),
            env: shared,
        })])
        .unwrap();
    assert_eq!(vm.collect().unwrap().aggregates.live, 2);
    let parent = aggregate(&vm.resolve(&ordinary).unwrap()[0]);
    let shared = aggregate(field(vm.aggregates(), parent, 0));
    let [Value::Closure(closure)] = vm.resolve(&callable).unwrap() else {
        panic!("closure root")
    };
    assert_eq!(closure.env, shared, "both groups name one moved node");

    drop(ordinary);
    let report = vm.collect().unwrap();
    assert_eq!(
        (
            report.aggregates.live,
            report.root_groups,
            report.released_groups
        ),
        (1, 1, 1)
    );
    let closure = vm.resolve(&callable).unwrap()[0].clone();
    let result = vm
        .run(FuncRef::new(unit, apply), &[closure], TraceMask::NONE, None)
        .unwrap();
    assert_eq!(result, [Value::I64(42)], "the moved environment still runs");
}

#[test]
fn a_planning_failure_moves_nothing() {
    let mut vm = Vm::new(NoHost, Limits::default());
    let heap = vm.aggregates_mut();
    heap.tuple_new(vec![]).unwrap();
    let kept = heap.tuple_new(vec![Value::I64(1)]).unwrap();
    let lost = heap.tuple_new(vec![Value::I64(2)]).unwrap();
    let kept = vm.root(vec![Value::Agg(kept)]).unwrap();
    let lost = vm.root(vec![Value::Agg(lost)]).unwrap();
    let dropped = vm.root(vec![Value::I64(3)]).unwrap();
    drop(dropped);
    // Shrinking the heap behind the VM leaves a registered handle dangling.
    let mut nodes = vm.aggregates_mut().take_nodes();
    let heap = vm.aggregates_mut();
    heap.tuple_new(vec![]).unwrap();
    heap.tuple_new(vec![Value::I64(1)]).unwrap();
    assert_eq!(
        vm.collect(),
        Err(CollectError::DanglingHandle {
            handle: AggHandle(2)
        })
    );
    assert_eq!(vm.aggregates().len_u32(), 2, "nothing was reclaimed");
    assert_eq!(vm.resolve(&kept).unwrap(), [Value::Agg(AggHandle(1))]);
    assert_eq!(vm.heap_stats().root_groups, 3, "nothing was unregistered");
    assert_eq!(vm.heap_stats().collections, 0);

    nodes.tuple_new(vec![]).unwrap();
    *vm.aggregates_mut() = nodes;
    assert_eq!(vm.collect().unwrap().released_groups, 1);
    let values = vm.resolve(&lost).unwrap();
    assert_eq!(
        field(vm.aggregates(), aggregate(&values[0]), 0),
        &Value::I64(2)
    );
}

#[test]
fn roots_are_checked_and_belong_to_their_vm() {
    let (program, body, _) = closures();
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(Arc::new(program)).unwrap();
    assert_eq!(
        vm.root(vec![Value::I64(1), Value::Agg(AggHandle(0))])
            .unwrap_err(),
        RootError::DanglingHandle {
            index: 1,
            handle: AggHandle(0)
        }
    );
    let foreign = FuncRef::new(execution_tape::value::UnitId(unit.0 + 1), body);
    assert_eq!(
        vm.root(vec![Value::Func(foreign)]).unwrap_err(),
        RootError::UnknownUnit {
            index: 0,
            unit: foreign.unit
        }
    );
    let missing = FuncRef::new(unit, FuncId(99));
    assert_eq!(
        vm.root(vec![Value::Func(missing)]).unwrap_err(),
        RootError::UnknownFunction {
            index: 0,
            function: missing
        }
    );
    vm.aggregates_mut().tuple_new(vec![]).unwrap(); // Reclaimed, so the survivor moves.
    let tuple = vm.aggregates_mut().tuple_new(vec![]).unwrap();
    let kept = vm.root(vec![Value::Agg(tuple)]).unwrap();
    vm.collect().unwrap();
    assert_eq!(
        vm.root(vec![Value::Agg(tuple)]).unwrap_err(),
        RootError::DanglingHandle {
            index: 0,
            handle: tuple
        },
        "a handle from before the collection names nothing"
    );
    drop(kept);
    let root = vm
        .root(vec![Value::Func(FuncRef::new(unit, body))])
        .unwrap();
    let mut other = Vm::new(NoHost, Limits::default());
    assert_eq!(other.resolve(&root).unwrap_err(), RootError::ForeignVm);
    assert_eq!(other.release(root).unwrap_err(), RootError::ForeignVm);
    assert_eq!(
        vm.heap_stats().root_groups,
        2,
        "a dropped group goes at collection"
    );
}

/// A guest that waits at a host request while it holds live and dead values of every kind.
struct Paused {
    program: Arc<VerifiedProgram>,
    entry: FuncId,
}

/// `entry(x)` builds `kept = (x, "text")` and a string borrowed from it, then garbage: two
/// tuples whose register it overwrites with an empty one, and owned strings it overwrites. It
/// passes `kept` to `pause`, whose answer it pairs with the borrowed string and `kept`.
fn paused() -> Paused {
    let mut b = ProgramBuilder::new();
    let pause = b.host_sig_for(
        "pause",
        HostSig {
            args: vec![ValueType::Agg],
            rets: vec![ValueType::I64],
        },
    );
    let text = b.const_str("text");
    let empty = b.const_str("");
    let entry = b
        .declare_function(sig(
            &[ValueType::I64],
            &[ValueType::I64, ValueType::Str, ValueType::Agg],
        ))
        .unwrap();
    let mut a = b.function_asm(entry).unwrap();
    let x = b.argument(entry, 0).unwrap();
    let (literal, kept, borrowed, nothing, scratch, dead, answer) = (
        a.alloc_register(ValueType::Str),
        a.alloc_register(ValueType::Agg),
        a.alloc_register(ValueType::Str),
        a.alloc_register(ValueType::Str),
        a.alloc_register(ValueType::Str),
        a.alloc_register(ValueType::Agg),
        a.alloc_register(ValueType::I64),
    );
    a.const_pool(literal, text)
        .tuple_new(kept, &[x, literal])
        .tuple_get(borrowed, kept, 1)
        .const_pool(nothing, empty)
        .tuple_new(dead, &[x])
        .tuple_new(dead, &[dead, dead])
        .tuple_new(dead, &[]);
    for _ in 0..3 {
        a.str_concat(scratch, borrowed, nothing);
    }
    a.host_call_sig_id(Reg::EFFECT, pause, Reg::EFFECT, &[kept], &[answer])
        .i64_add(answer, answer, x)
        .ret(Reg::EFFECT, &[answer, borrowed, kept]);
    b.define_function(entry, a).unwrap();
    Paused {
        program: Arc::new(b.build_verified().unwrap()),
        entry,
    }
}

#[test]
fn collection_while_a_host_request_waits_relocates_its_registers() {
    let f = paused();
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(Arc::clone(&f.program)).unwrap();
    let older = vm.aggregates_mut().tuple_new(vec![]).unwrap();
    let kept_before = vm.aggregates_mut().tuple_new(vec![Value::I64(-1)]).unwrap();
    let root = vm.root(vec![Value::Agg(kept_before)]).unwrap();
    let _ = older;
    let options = InvocationOptions::default().yield_host_calls();
    let mut run = vm
        .start(FuncRef::new(unit, f.entry), &[Value::I64(5)], options)
        .unwrap();
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    assert_eq!(run.aggregates().len_u32(), 6);
    let before = {
        let request = run.pending_host_call().unwrap();
        let [ValueRef::Agg(kept)] = request.arguments().collect::<Vec<_>>()[..] else {
            panic!("pause argument")
        };
        kept
    };
    assert_eq!(before, AggHandle(2));

    let report = run.collect().unwrap();
    // The older unrooted tuple and both overwritten tuples go. The root, `kept` and the empty
    // tuple a written register still holds survive.
    assert_eq!(
        report.aggregates,
        Retention {
            live: 3,
            reclaimed: 3
        }
    );
    assert_eq!(report.moved, 3);
    // Two of the three concatenations are dead; the literal, the borrowed field, the empty
    // string and the last concatenation stay.
    assert_eq!(
        report.payloads,
        Retention {
            live: 4,
            reclaimed: 2
        }
    );
    assert_eq!(
        report.payload_bytes,
        Retention {
            live: 4,
            reclaimed: 8
        }
    );

    let request = run.pending_host_call().unwrap();
    let [ValueRef::Agg(kept)] = request.arguments().collect::<Vec<_>>()[..] else {
        panic!("pause argument")
    };
    assert_ne!(kept, before, "the argument register moved with its node");
    assert_eq!(
        request.aggregates().tuple_get(before, 1),
        Err(AggError::BadHandle),
        "the handle read before the collection names nothing now"
    );
    assert_eq!(
        request.aggregates().tuple_get(kept, 1).unwrap(),
        Value::Str("text".into())
    );
    request.complete(&mut [Value::I64(10)], Ok(0)).unwrap();
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::Complete);
    let outputs = run.take_outputs().unwrap();
    assert_eq!(outputs[..2], [Value::I64(15), Value::Str("text".into())]);
    let kept = aggregate(&outputs[2]);
    assert_eq!(run.aggregates().tuple_get(kept, 0).unwrap(), Value::I64(5));
    drop(run);
    let values = vm.resolve(&root).unwrap();
    assert_eq!(
        field(vm.aggregates(), aggregate(&values[0]), 0),
        &Value::I64(-1)
    );
    assert_eq!(
        vm.aggregates().len_u32(),
        2,
        "the run kept only its outputs"
    );
    let stats = vm.heap_stats();
    assert_eq!(
        (stats.live_after_collection, stats.aggregates),
        (1, 2),
        "the run's own survivors count as growth, not as retained"
    );
}

#[test]
fn a_trap_after_collection_rolls_back_to_where_the_run_began() {
    let f = paused();
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(Arc::clone(&f.program)).unwrap();
    vm.aggregates_mut().tuple_new(vec![]).unwrap();
    let older = vm.aggregates_mut().tuple_new(vec![Value::I64(-1)]).unwrap();
    let root = vm.root(vec![Value::Agg(older)]).unwrap();
    let options = InvocationOptions::default().yield_host_calls();
    let mut run = vm
        .start(FuncRef::new(unit, f.entry), &[Value::I64(5)], options)
        .unwrap();
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    // The run began after two nodes; once one older node is reclaimed, it begins after one.
    run.collect().unwrap();
    let request = run.pending_host_call().unwrap();
    request
        .complete(&mut [], Err(HostError::UnknownSymbol))
        .unwrap_err();
    drop(run);
    assert_eq!(
        vm.aggregates().len_u32(),
        1,
        "only the run's nodes are discarded"
    );
    let values = vm.resolve(&root).unwrap();
    assert_eq!(
        field(vm.aggregates(), aggregate(&values[0]), 0),
        &Value::I64(-1)
    );
}

/// A guest whose host builds an aggregate at one request and returns it after calling back.
struct Promoting {
    program: Arc<VerifiedProgram>,
    entry: FuncId,
    nested: FuncId,
    echo: FuncId,
}

/// `entry()` returns what `outer()` returns. `nested()` builds `(42,)`, passes it to `inner`
/// and returns what `inner` returns. `echo(x)` returns `x`.
fn promoting() -> Promoting {
    let mut b = ProgramBuilder::new();
    let outer = b.host_sig_for(
        "outer",
        HostSig {
            args: vec![],
            rets: vec![ValueType::Agg],
        },
    );
    let inner = b.host_sig_for(
        "inner",
        HostSig {
            args: vec![ValueType::Agg],
            rets: vec![ValueType::Agg],
        },
    );
    let entry = b.declare_function(sig(&[], &[ValueType::Agg])).unwrap();
    let nested = b.declare_function(sig(&[], &[ValueType::Agg])).unwrap();
    let echo = b
        .declare_function(sig(&[ValueType::Agg], &[ValueType::Agg]))
        .unwrap();

    let mut a = b.function_asm(entry).unwrap();
    let result = a.alloc_register(ValueType::Agg);
    a.host_call_sig_id(Reg::EFFECT, outer, Reg::EFFECT, &[], &[result])
        .ret(Reg::EFFECT, &[result]);
    b.define_function(entry, a).unwrap();

    let mut a = b.function_asm(nested).unwrap();
    let (n, tuple, result) = (
        a.alloc_register(ValueType::I64),
        a.alloc_register(ValueType::Agg),
        a.alloc_register(ValueType::Agg),
    );
    a.const_i64(n, 42)
        .tuple_new(tuple, &[n])
        .host_call_sig_id(Reg::EFFECT, inner, Reg::EFFECT, &[tuple], &[result])
        .ret(Reg::EFFECT, &[result]);
    b.define_function(nested, a).unwrap();

    let mut a = b.function_asm(echo).unwrap();
    a.ret(Reg::EFFECT, &[b.argument(echo, 0).unwrap()]);
    b.define_function(echo, a).unwrap();
    Promoting {
        program: Arc::new(b.build_verified().unwrap()),
        entry,
        nested,
        echo,
    }
}

/// Starts `entry`, builds `mine = (7,)` at `outer` if asked, calls back into `nested` and
/// returns at its `inner` request, with `mine`.
fn at_inner<'vm>(
    vm: &'vm mut Vm<NoHost>,
    f: &Promoting,
    build: bool,
) -> (Invocation<'vm, NoHost>, Option<AggHandle>) {
    let unit = vm.install(Arc::clone(&f.program)).unwrap();
    let options = InvocationOptions::default().yield_host_calls();
    let mut run = vm.start(FuncRef::new(unit, f.entry), &[], options).unwrap();
    assert_eq!(
        run.resume(u64::MAX).unwrap(),
        RunStatus::AwaitingHost,
        "the fixture reaches its next step"
    );
    let mut request = run.pending_host_call().unwrap();
    let mine = build.then(|| request.context().tuple_new(vec![Value::I64(7)]).unwrap());
    request
        .call_guest(Value::Func(FuncRef::new(unit, f.nested)), vec![])
        .unwrap();
    assert_eq!(
        run.resume(u64::MAX).unwrap(),
        RunStatus::AwaitingHost,
        "the fixture reaches its next step"
    );
    (run, mine)
}

/// Completes the waiting `inner` request with its own argument and the outer one with `value`.
fn finish(run: &mut Invocation<'_, NoHost>, value: Value) -> Value {
    let request = run.pending_host_call().unwrap();
    assert_eq!(request.symbol(), "inner", "the callback waits");
    let mut argument = vec![request.arguments().next().unwrap().to_value()];
    request.complete(&mut argument, Ok(0)).unwrap();
    assert_eq!(
        run.resume(u64::MAX).unwrap(),
        RunStatus::CallbackReturned,
        "the fixture reaches its next step"
    );
    let request = run.pending_host_call().unwrap();
    assert_eq!(request.symbol(), "outer", "the original request waits");
    request.complete(&mut [value], Ok(0)).unwrap();
    assert_eq!(
        run.resume(u64::MAX).unwrap(),
        RunStatus::Complete,
        "the fixture reaches its next step"
    );
    run.result().unwrap().unwrap()[0].clone()
}

#[test]
fn collection_refuses_to_move_aggregates_a_waiting_host_built() {
    let f = promoting();
    let mut vm = Vm::new(NoHost, Limits::default());
    vm.aggregates_mut().tuple_new(vec![]).unwrap();
    let (mut run, mine) = at_inner(&mut vm, &f, true);
    let mine = mine.unwrap();
    // Only the host holds `mine`. Reclaiming the filler would move it, so nothing moves.
    let window = (run.aggregates().len_u32(), run.aggregates().handle_end());
    assert_eq!(run.collect(), Err(CollectError::HostHandles));
    assert_eq!(
        (run.aggregates().len_u32(), run.aggregates().handle_end()),
        window
    );
    assert_eq!(run.aggregates().tuple_get(mine, 0).unwrap(), Value::I64(7));
    let result = aggregate(&finish(&mut run, Value::Agg(mine)));
    assert_eq!(
        run.aggregates().tuple_get(result, 0).unwrap(),
        Value::I64(7)
    );
    // Once the request completed, collection can move everything again.
    assert!(run.collect().unwrap().aggregates.reclaimed > 0);
}

#[test]
fn a_collection_that_moves_nothing_succeeds_while_a_host_holds_promoted_aggregates() {
    let f = promoting();
    let mut vm = Vm::new(NoHost, Limits::default());
    let (mut run, mine) = at_inner(&mut vm, &f, true);
    let report = run.collect().unwrap();
    assert_eq!((report.aggregates.reclaimed, report.moved), (0, 0));
    let result = aggregate(&finish(&mut run, Value::Agg(mine.unwrap())));
    assert_eq!(
        run.aggregates().tuple_get(result, 0).unwrap(),
        Value::I64(7)
    );
}

#[test]
fn a_host_built_aggregate_passed_to_a_nested_callback_is_never_moved() {
    let f = promoting();
    let mut vm = Vm::new(NoHost, Limits::default());
    vm.aggregates_mut().tuple_new(vec![]).unwrap();
    let (mut run, mine) = at_inner(&mut vm, &f, true);
    let mine = mine.unwrap();
    let unit = run.unit();
    assert_eq!(run.collect(), Err(CollectError::HostHandles));
    // `mine` belongs to the outer request; the inner one calls back with it.
    let request = run.pending_host_call().unwrap();
    request
        .call_guest(
            Value::Func(FuncRef::new(unit, f.echo)),
            vec![Value::Agg(mine)],
        )
        .unwrap();
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::CallbackReturned);
    assert_eq!(run.collect(), Err(CollectError::HostHandles));
    let mut request = run.pending_host_call().unwrap();
    let [Value::Agg(echoed)] = request.take_callback_results().unwrap()[..] else {
        panic!("echoed tuple")
    };
    assert_eq!(echoed, mine);
    drop(request);
    let result = aggregate(&finish(&mut run, Value::Agg(mine)));
    assert_eq!(
        run.aggregates().tuple_get(result, 0).unwrap(),
        Value::I64(7)
    );
}

#[test]
fn a_trap_after_a_refused_collection_rolls_back_cleanly() {
    let f = promoting();
    let mut vm = Vm::new(NoHost, Limits::default());
    vm.aggregates_mut().tuple_new(vec![]).unwrap();
    let older = vm.aggregates_mut().tuple_new(vec![Value::I64(-1)]).unwrap();
    let root = vm.root(vec![Value::Agg(older)]).unwrap();
    let (mut run, _mine) = at_inner(&mut vm, &f, true);
    assert_eq!(run.collect(), Err(CollectError::HostHandles));
    let request = run.pending_host_call().unwrap();
    request
        .complete(&mut [], Err(HostError::UnknownSymbol))
        .unwrap_err();
    drop(run);
    assert_eq!(vm.aggregates().len_u32(), 2, "the run left nothing behind");
    assert_eq!(vm.collect().unwrap().aggregates.reclaimed, 1);
    let values = vm.resolve(&root).unwrap();
    assert_eq!(
        field(vm.aggregates(), aggregate(&values[0]), 0),
        &Value::I64(-1)
    );
}

#[test]
fn a_handle_read_before_a_collection_traps_when_returned_after_it() {
    let f = promoting();
    let mut vm = Vm::new(NoHost, Limits::default());
    vm.aggregates_mut().tuple_new(vec![]).unwrap(); // Reclaimed, so every survivor moves.
    let (mut run, _) = at_inner(&mut vm, &f, false);
    let stale = {
        let request = run.pending_host_call().unwrap();
        let [ValueRef::Agg(tuple)] = request.arguments().collect::<Vec<_>>()[..] else {
            panic!("inner argument")
        };
        tuple
    };
    run.collect().unwrap();
    let request = run.pending_host_call().unwrap();
    let error = request
        .complete(&mut [Value::Agg(stale)], Ok(0))
        .unwrap_err();
    assert!(
        matches!(error.trap, Trap::AggError(AggError::BadHandle)),
        "{error:?}"
    );
}

#[test]
fn host_staging_refuses_collection_until_the_request_completes() {
    let f = paused();
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(Arc::clone(&f.program)).unwrap();
    let options = InvocationOptions::default().yield_host_calls();
    let mut run = vm
        .start(FuncRef::new(unit, f.entry), &[Value::I64(1)], options)
        .unwrap();
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    let mut request = run.pending_host_call().unwrap();
    request.context().tuple_new(vec![]).unwrap();
    drop(request);
    let length = run.aggregates().len_u32();
    assert_eq!(run.collect(), Err(CollectError::HostStaging));
    assert_eq!(run.aggregates().len_u32(), length);
    let request = run.pending_host_call().unwrap();
    request.complete(&mut [Value::I64(0)], Ok(0)).unwrap();
    assert!(run.collect().is_ok());
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::Complete);
}

/// A guest that waits at a yielded suspension while it holds a continuation it has not resumed
/// and an exception reference, each holding aggregates only they reach.
struct Suspended {
    program: Arc<VerifiedProgram>,
    entry: FuncId,
    boxed: TagId,
}

/// `body(env)` returns the integer its environment holds. `entry(x)` makes an unstarted
/// continuation of `body` over `(x,)`, catches `boxed((x + 1,))` from `raise(x)` as a
/// reference, and then asks the embedding. Afterwards it resumes the continuation and rethrows the reference into its own
/// handler, which extracts the payload. It returns the continuation's result plus the payload
/// plus the answer.
fn suspended() -> Suspended {
    let mut b = ProgramBuilder::new();
    let ask = b
        .declare_tag_with_results("ask", &[ValueType::Agg.into()], &[ValueType::I64.into()])
        .unwrap();
    let integer = b.contract(Contract::Kind(ValueType::I64)).unwrap();
    let captured = b.contract(Contract::Tuple(&[Some(integer)])).unwrap();
    let boxed = b.declare_tag("boxed", &[captured.into()]).unwrap();
    let thunk = b.call_sig(sig(&[], &[ValueType::I64])).unwrap();
    let body = captured_body(&mut b);
    let raise = b
        .declare_function(sig(&[ValueType::I64], &[ValueType::I64]))
        .unwrap();
    let rethrow = b
        .declare_function(sig(&[ValueType::Exn], &[ValueType::I64]))
        .unwrap();
    let entry = b
        .declare_function(sig(&[ValueType::I64], &[ValueType::I64]))
        .unwrap();

    let mut a = b.function_asm(body).unwrap();
    let env = b.argument(body, 0).unwrap();
    let n = a.alloc_register(ValueType::I64);
    a.tuple_get(n, env, 0).ret(Reg::EFFECT, &[n]);
    b.define_function(body, a).unwrap();

    let mut a = b.function_asm(raise).unwrap();
    let x = b.argument(raise, 0).unwrap();
    let (one, payload) = (
        a.alloc_register(ValueType::I64),
        a.alloc_register(ValueType::Agg),
    );
    a.const_i64(one, 1)
        .i64_add(x, x, one)
        .tuple_new(payload, &[x]);
    b.emit_throw(&mut a, boxed, &[payload]).unwrap();
    b.define_function(raise, a).unwrap();

    let mut a = b.function_asm(rethrow).unwrap();
    let exn = b.argument(rethrow, 0).unwrap();
    a.throw_ref(Reg::EFFECT, exn);
    b.define_function(rethrow, a).unwrap();

    let mut a = b.function_asm(entry).unwrap();
    let x = b.argument(entry, 0).unwrap();
    let (env, garbage, function, closure, cont, caught) = (
        a.alloc_register(ValueType::Agg),
        a.alloc_register(ValueType::Agg),
        a.alloc_register(ValueType::Func),
        a.alloc_register(ValueType::Closure),
        a.alloc_register(ValueType::Cont),
        a.alloc_register(ValueType::Exn),
    );
    let (answer, from_cont, unboxed, from_exn) = (
        a.alloc_register(ValueType::I64),
        a.alloc_register(ValueType::I64),
        a.alloc_register(ValueType::Agg),
        a.alloc_register(ValueType::I64),
    );
    a.tuple_new(garbage, &[])
        .tuple_new(garbage, &[garbage])
        .tuple_new(env, &[x])
        .const_func(function, body)
        .closure_new(closure, function, env)
        .cont_new(cont, thunk, closure);
    let handler = a.block(&[caught]);
    a.begin_region(&[Catch::AllRef(handler)]);
    let [_] = b.emit_call(&mut a, raise, &[x]).unwrap();
    a.end_region();
    a.trap(2);
    a.place(handler).unwrap();
    // Only the exception reference reaches its payload; both garbage tuples become unreachable.
    a.mov(garbage, env)
        .tuple_new(env, &[])
        .suspend(Reg::EFFECT, ask, Reg::EFFECT, &[env], &[answer])
        .resume(Reg::EFFECT, thunk, cont, Reg::EFFECT, &[], &[from_cont]);
    let unbox = a.block(&[unboxed]);
    a.begin_region(&[Catch::Tag(boxed, unbox)]);
    let [_] = b.emit_call(&mut a, rethrow, &[caught]).unwrap();
    a.end_region();
    a.trap(1);
    a.place(unbox).unwrap();
    a.tuple_get(from_exn, unboxed, 0)
        .i64_add(answer, answer, from_cont)
        .i64_add(answer, answer, from_exn)
        .ret(Reg::EFFECT, &[answer]);
    b.define_function(entry, a).unwrap();
    Suspended {
        program: Arc::new(b.build_verified().unwrap()),
        entry,
        boxed,
    }
}

#[test]
fn collection_while_a_suspension_waits_relocates_continuations_and_exceptions() {
    let f = suspended();
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(Arc::clone(&f.program)).unwrap();
    let options = InvocationOptions::default().yield_suspensions();
    let mut run = vm
        .start(FuncRef::new(unit, f.entry), &[Value::I64(5)], options)
        .unwrap();
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHandler);
    let _ = f.boxed;
    let report = run.collect().unwrap();
    // Two unreachable tuples go. The continuation's environment, the exception's payload and
    // the suspension's empty payload survive, at new handles: had the held continuation or the
    // exception kept its old handle, resuming would read the wrong node.
    assert_eq!(
        report.aggregates,
        Retention {
            live: 3,
            reclaimed: 2
        }
    );
    assert_eq!(report.moved, 3);
    let suspension = run.pending_suspension().unwrap();
    let [ValueRef::Agg(payload)] = suspension.payload().collect::<Vec<_>>()[..] else {
        panic!("ask payload")
    };
    assert_eq!(suspension.aggregates().tuple_len(payload).unwrap(), 0);
    suspension.resume(vec![Value::I64(100)]).unwrap();
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::Complete);
    assert_eq!(run.result().unwrap().unwrap(), [Value::I64(100 + 5 + 6)]);
}

/// Counts collection events and checks the heap they report.
#[derive(Default)]
struct Collections {
    seen: Vec<CollectionReport>,
}

impl TraceSink for Collections {
    fn mask(&self) -> TraceMask {
        TraceMask::VALUES
    }
    fn values(&mut self, _: &execution_tape::program::Program, event: ValueEvent<'_>) {
        if let ValueEvent::Collected { report, aggregates } = event {
            assert_eq!(
                u64::from(aggregates.len_u32()),
                report.aggregates.live,
                "the event shows the collected heap"
            );
            self.seen.push(*report);
        }
    }
}

#[test]
fn observers_see_a_collection_and_recordings_refuse_to_replay_across_it() {
    let f = paused();
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(Arc::clone(&f.program)).unwrap();
    let entry = FuncRef::new(unit, f.entry);

    let mut sink = Collections::default();
    let options = InvocationOptions::default()
        .yield_host_calls()
        .trace(TraceMask::VALUES, &mut sink);
    let mut run = vm.start(entry, &[Value::I64(1)], options).unwrap();
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    let report = run.collect().unwrap();
    drop(run);
    assert_eq!(sink.seen, [report]);

    let mut recorder = Recorder::new(Arc::clone(&f.program), RecordLimits::default());
    let options = InvocationOptions::default()
        .yield_host_calls()
        .trace(recorder.mask(), &mut recorder);
    let mut run = vm.start(entry, &[Value::I64(1)], options).unwrap();
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    run.collect().unwrap();
    let request = run.pending_host_call().unwrap();
    request.complete(&mut [Value::I64(2)], Ok(0)).unwrap();
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::Complete);
    drop(run);
    let recording = recorder.finish();
    assert_eq!(recording.status(), Err(&CaptureFault::Relocated));
}

#[test]
fn recordings_replay_runs_between_collections() {
    let (program, make) = revisions();
    let program = Arc::new(program);
    let mut vm = Vm::new(NoHost, Limits::default());
    let make = FuncRef::new(vm.install(Arc::clone(&program)).unwrap(), make);
    let first = vm
        .run(make, &[Value::I64(1)], TraceMask::NONE, None)
        .unwrap();
    let first = vm.root(first).unwrap();
    vm.run(make, &[Value::I64(2)], TraceMask::NONE, None)
        .unwrap();
    assert_eq!(vm.collect().unwrap().aggregates.reclaimed, 1);
    let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
    let outputs = vm
        .run(make, &[Value::I64(3)], recorder.mask(), Some(&mut recorder))
        .unwrap();
    let recording = recorder.finish();
    recording.status().unwrap();
    let ReplayOutcome::Returned(replayed) = recording.replay().unwrap() else {
        panic!("replay returns")
    };
    let tuple = aggregate(&replayed.values()[0]);
    assert_eq!(
        replayed.aggregates().tuple_get(tuple, 0).unwrap(),
        Value::I64(3)
    );
    assert_eq!(
        vm.aggregates()
            .tuple_get(aggregate(&outputs[0]), 0)
            .unwrap(),
        Value::I64(3)
    );
    assert_eq!(vm.resolve(&first).unwrap().len(), 1);
}
