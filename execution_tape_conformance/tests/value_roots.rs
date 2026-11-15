// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Registered value roots and explicit, moving collection.

use execution_tape::{
    aggregates::AggHeap,
    asm::{FunctionAbi, FunctionSig, ProgramBuilder, Reg},
    contract::Contract,
    host::{Host, HostContext, HostError, SigHash, ValueRef},
    program::ValueType,
    recording::{RecordLimits, Recorder, ReplayOutcome},
    trace::{TraceMask, TraceSink},
    value::{AggHandle, Closure, FuncId, FuncRef, Value},
    verifier::VerifiedProgram,
    vm::{CollectError, Limits, Retention, RootError, Vm},
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
        panic!("fixtures call no host")
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
