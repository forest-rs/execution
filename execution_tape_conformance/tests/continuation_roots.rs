// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Host objects held by continuations stay visible to the embedding collector.
//!
//! Each fixture collects in the middle of an invocation: the `collect` host call keeps only the
//! objects [`InvocationInspection::visit_objects`] reports, and `check` fails for an object that
//! was collected while the guest could still use it.

use execution_tape::{
    asm::{Asm, Catch, FunctionSig, Label, ProgramBuilder},
    host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef},
    program::{CallSigId, HostSigId, HostTypeId, TagId, ValueType},
    value::{FuncId, FuncRef, Obj, ObjHandle, Value},
    verifier::VerifiedProgram,
    vm::{
        ActivationId, InvocationInspection, InvocationOptions, Limits, ObjectRoot,
        ObjectRootOrigin, RunStatus, Vm,
    },
};
use std::collections::BTreeSet;
use std::sync::Arc;

const OBJECT: HostTypeId = HostTypeId(73);
const OBJ: ValueType = ValueType::Obj(OBJECT);

fn object(n: u64) -> Obj {
    Obj {
        host_type: OBJECT,
        handle: ObjHandle(n),
    }
}

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
        panic!("every host call yields to the driver")
    }
}

fn sig(args: &[ValueType], rets: &[ValueType]) -> FunctionSig {
    FunctionSig {
        arg_types: args.to_vec(),
        ret_types: rets.to_vec(),
    }
}

struct Imports {
    make: HostSigId,
    collect: HostSigId,
    check: HostSigId,
}

impl Imports {
    fn new(b: &mut ProgramBuilder) -> Self {
        Self {
            make: b.host_sig_for(
                "make",
                HostSig {
                    args: vec![],
                    rets: vec![OBJ],
                },
            ),
            collect: b.host_sig_for(
                "collect",
                HostSig {
                    args: vec![],
                    rets: vec![],
                },
            ),
            check: b.host_sig_for(
                "check",
                HostSig {
                    args: vec![OBJ],
                    rets: vec![],
                },
            ),
        }
    }
    fn make(&self, asm: &mut Asm, dst: u32) {
        asm.host_call_sig_id(0, self.make, 0, &[], &[dst]);
    }
    fn collect(&self, asm: &mut Asm) {
        asm.host_call_sig_id(0, self.collect, 0, &[], &[]);
    }
    fn check(&self, asm: &mut Asm, src: u32) {
        asm.host_call_sig_id(0, self.check, 0, &[src], &[]);
    }
}

/// What a driven invocation reported to its embedding.
#[derive(Default)]
struct Collector {
    next: u64,
    live: BTreeSet<u64>,
    /// The roots each `collect` saw, in order.
    collections: Vec<Vec<ObjectRoot>>,
    /// The activation of each `check`'s caller.
    checkers: Vec<ActivationId>,
}

impl Collector {
    fn roots_of(&self, collection: usize, n: u64) -> Vec<&ObjectRoot> {
        self.collections[collection]
            .iter()
            .filter(|root| root.object == object(n))
            .collect()
    }
}

/// Runs `entry`, answering host calls and collecting at each `collect` and yielded suspension.
fn drive(program: &Arc<VerifiedProgram>, entry: FuncId) -> Collector {
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(Arc::clone(program)).unwrap();
    let mut run = vm
        .start(
            FuncRef::new(unit, entry),
            &[],
            InvocationOptions::default()
                .yield_host_calls()
                .yield_suspensions(),
        )
        .unwrap();
    let mut heap = Collector::default();
    let collect = |heap: &mut Collector, inspection: &mut InvocationInspection<'_>| {
        let mut roots = Vec::new();
        inspection.visit_objects(|root| roots.push(root));
        assert!(
            roots
                .iter()
                .all(|root| heap.live.contains(&root.object.handle.0)),
            "only objects the host made are reported: {roots:?}"
        );
        heap.live
            .retain(|n| roots.iter().any(|root| root.object == object(*n)));
        heap.collections.push(roots);
    };
    loop {
        match run.resume(u64::MAX).unwrap() {
            RunStatus::Complete => break,
            RunStatus::AwaitingHandler => {
                let pending = run.pending_suspension().unwrap();
                collect(&mut heap, &mut pending.inspect());
                pending.resume(vec![]).unwrap();
            }
            RunStatus::AwaitingHost => {
                let call = run.pending_host_call().unwrap();
                match call.symbol() {
                    "make" => {
                        heap.next += 1;
                        heap.live.insert(heap.next);
                        call.complete(&mut [Value::Obj(object(heap.next))], Ok(0))
                            .unwrap();
                    }
                    "collect" => {
                        collect(&mut heap, &mut call.inspect());
                        call.complete(&mut [], Ok(0)).unwrap();
                    }
                    "check" => {
                        let Some(ValueRef::Obj(checked)) = call.arguments().next() else {
                            panic!("check takes an object")
                        };
                        assert!(
                            heap.live.contains(&checked.handle.0),
                            "object {} was collected while the guest could still use it",
                            checked.handle.0
                        );
                        heap.checkers
                            .push(call.inspect().frames().next().unwrap().activation);
                        call.complete(&mut [], Ok(0)).unwrap();
                    }
                    other => panic!("unexpected host call {other}"),
                }
            }
            status => panic!("unexpected {status:?}"),
        }
    }
    assert_eq!(run.take_outputs().unwrap(), [], "fixtures return nothing");
    heap
}

struct Fixture {
    program: Arc<VerifiedProgram>,
    holder: FuncId,
    holder_suspend: u32,
    taker: FuncId,
    asker: FuncId,
    suspended: FuncId,
    unstarted: FuncId,
    bound_suspension: FuncId,
    dropped: FuncId,
    consumed: FuncId,
    caught: FuncId,
    suspended_exception: FuncId,
    yielded: FuncId,
}

/// Emits `resume call_sig cont()` protected by `On(tag, handler)`.
fn resume_handling(asm: &mut Asm, tag: TagId, handler: Label, call_sig: CallSigId, cont: u32) {
    asm.begin_region(&[Catch::On(tag, handler)]);
    asm.resume(0, call_sig, cont, 0, &[], &[]);
    asm.end_region();
}

fn fixture() -> Fixture {
    let mut b = ProgramBuilder::new();
    let host = Imports::new(&mut b);
    let pause = b.declare_tag("pause", &[]).unwrap();
    let ask = b
        .declare_tag_with_results("ask", &[], &[OBJ.into()])
        .unwrap();
    let boom = b.declare_tag("boom", &[OBJ.into()]).unwrap();
    let unit_to_unit = b.call_sig(sig(&[], &[])).unwrap();
    let obj_to_unit = b.call_sig(sig(&[OBJ], &[])).unwrap();

    // `holder()` makes 1 and a tuple holding 2, suspends `pause`, then checks both.
    let holder = b.declare_function(sig(&[], &[])).unwrap();
    let mut asm = b.function_asm(holder).unwrap();
    let (held, other, tuple) = (
        asm.alloc_register(OBJ),
        asm.alloc_register(OBJ),
        asm.alloc_register(ValueType::Agg),
    );
    host.make(&mut asm, held);
    host.make(&mut asm, other);
    asm.tuple_new(tuple, &[other]);
    asm.mov(other, held);
    let holder_suspend = asm.pc();
    asm.suspend(0, pause, 0, &[], &[]);
    host.check(&mut asm, held);
    asm.tuple_get(other, tuple, 0);
    host.check(&mut asm, other);
    asm.ret(0, &[]);
    b.define_function(holder, asm).unwrap();

    // `keeper()` makes 1, suspends `pause`, then checks it.
    let keeper = b.declare_function(sig(&[], &[])).unwrap();
    let mut asm = b.function_asm(keeper).unwrap();
    let held = asm.alloc_register(OBJ);
    host.make(&mut asm, held);
    asm.suspend(0, pause, 0, &[], &[]);
    host.check(&mut asm, held);
    asm.ret(0, &[]);
    b.define_function(keeper, asm).unwrap();

    // `taker(x)` checks `x`.
    let taker = b.declare_function(sig(&[OBJ], &[])).unwrap();
    let mut asm = b.function_asm(taker).unwrap();
    let x = b.argument(taker, 0).unwrap();
    host.check(&mut asm, x);
    asm.ret(0, &[]);
    b.define_function(taker, asm).unwrap();

    // `asker()` suspends `ask` and checks the object it is resumed with.
    let asker = b.declare_function(sig(&[], &[])).unwrap();
    let mut asm = b.function_asm(asker).unwrap();
    let answer = asm.alloc_register(OBJ);
    asm.suspend(0, ask, 0, &[], &[answer]);
    host.check(&mut asm, answer);
    asm.ret(0, &[]);
    b.define_function(asker, asm).unwrap();

    // `exn_holder()` catches `boom(1)` by reference only, makes 2 over 1's register and suspends
    // `pause`; once resumed it rethrows and checks the payload it catches again.
    let exn_holder = b.declare_function(sig(&[], &[])).unwrap();
    let mut asm = b.function_asm(exn_holder).unwrap();
    let held = asm.alloc_register(OBJ);
    let caught = asm.alloc_register(ValueType::Exn);
    let payload = asm.alloc_register(OBJ);
    let (by_ref, by_tag) = (asm.block(&[caught]), asm.block(&[payload]));
    host.make(&mut asm, held);
    asm.begin_region(&[Catch::AllRef(by_ref)]);
    b.emit_throw(&mut asm, boom, &[held]).unwrap();
    asm.end_region();
    asm.place(by_ref).unwrap();
    host.make(&mut asm, held);
    asm.suspend(0, pause, 0, &[], &[]);
    asm.begin_region(&[Catch::Tag(boom, by_tag)]);
    asm.throw_ref(0, caught);
    asm.end_region();
    asm.place(by_tag).unwrap();
    host.check(&mut asm, payload);
    asm.ret(0, &[]);
    b.define_function(exn_holder, asm).unwrap();

    // `run_paused(f)` starts `f`, collects while it is suspended at `pause`, then resumes it.
    let run_paused =
        |b: &mut ProgramBuilder, target: FuncId, before_resume: &dyn Fn(&mut Asm, u32) -> bool| {
            let func = b.declare_function(sig(&[], &[])).unwrap();
            let mut asm = b.function_asm(func).unwrap();
            let (f, cont, rest) = (
                asm.alloc_register(ValueType::Func),
                asm.alloc_register(ValueType::Cont),
                asm.alloc_register(ValueType::Cont),
            );
            let paused = asm.block(&[rest]);
            asm.const_func(f, target);
            asm.cont_new(cont, unit_to_unit, f);
            resume_handling(&mut asm, pause, paused, unit_to_unit, cont);
            asm.ret(0, &[]);
            asm.place(paused).unwrap();
            if before_resume(&mut asm, rest) {
                asm.resume(0, unit_to_unit, rest, 0, &[], &[]);
            }
            asm.ret(0, &[]);
            b.define_function(func, asm).unwrap();
            func
        };
    let suspended = run_paused(&mut b, holder, &|asm, _| {
        host.collect(asm);
        true
    });
    let suspended_exception = run_paused(&mut b, exn_holder, &|asm, _| {
        host.collect(asm);
        true
    });
    let dropped = run_paused(&mut b, keeper, &|asm, rest| {
        asm.cont_drop(rest);
        host.collect(asm);
        false
    });
    let consumed = run_paused(&mut b, keeper, &|asm, rest| {
        asm.resume(0, unit_to_unit, rest, 0, &[], &[]);
        host.collect(asm);
        false
    });

    // `unstarted()` binds 1 to `taker`, leaves a second `taker` unbound, makes 2 over 1's
    // register and collects before resuming the bound one.
    let unstarted = b.declare_function(sig(&[], &[])).unwrap();
    let mut asm = b.function_asm(unstarted).unwrap();
    let (f, cont, bound, unbound, held) = (
        asm.alloc_register(ValueType::Func),
        asm.alloc_register(ValueType::Cont),
        asm.alloc_register(ValueType::Cont),
        asm.alloc_register(ValueType::Cont),
        asm.alloc_register(OBJ),
    );
    host.make(&mut asm, held);
    asm.const_func(f, taker);
    asm.cont_new(cont, obj_to_unit, f);
    asm.cont_new(unbound, obj_to_unit, f);
    asm.cont_bind(bound, obj_to_unit, unit_to_unit, cont, &[held]);
    host.make(&mut asm, held);
    host.collect(&mut asm);
    asm.resume(0, unit_to_unit, bound, 0, &[], &[]);
    asm.ret(0, &[]);
    b.define_function(unstarted, asm).unwrap();

    // `bound_suspension()` answers `asker`'s `ask` by binding 1, makes 2 over 1's register and
    // collects before resuming it.
    let bound_suspension = b.declare_function(sig(&[], &[])).unwrap();
    let mut asm = b.function_asm(bound_suspension).unwrap();
    let (f, cont, rest, held) = (
        asm.alloc_register(ValueType::Func),
        asm.alloc_register(ValueType::Cont),
        asm.alloc_register(ValueType::Cont),
        asm.alloc_register(OBJ),
    );
    let asked = asm.block(&[rest]);
    asm.const_func(f, asker);
    asm.cont_new(cont, unit_to_unit, f);
    resume_handling(&mut asm, ask, asked, unit_to_unit, cont);
    asm.ret(0, &[]);
    asm.place(asked).unwrap();
    host.make(&mut asm, held);
    asm.cont_bind(rest, obj_to_unit, unit_to_unit, rest, &[held]);
    host.make(&mut asm, held);
    host.collect(&mut asm);
    asm.resume(0, unit_to_unit, rest, 0, &[], &[]);
    asm.ret(0, &[]);
    b.define_function(bound_suspension, asm).unwrap();

    // `caught()` catches `boom(1)` by reference only, makes 2 over 1's register and collects;
    // then it rethrows and checks the payload it catches again.
    let caught_fn = b.declare_function(sig(&[], &[])).unwrap();
    let mut asm = b.function_asm(caught_fn).unwrap();
    let held = asm.alloc_register(OBJ);
    let caught = asm.alloc_register(ValueType::Exn);
    let payload = asm.alloc_register(OBJ);
    let (by_ref, by_tag) = (asm.block(&[caught]), asm.block(&[payload]));
    host.make(&mut asm, held);
    asm.begin_region(&[Catch::AllRef(by_ref)]);
    b.emit_throw(&mut asm, boom, &[held]).unwrap();
    asm.end_region();
    asm.place(by_ref).unwrap();
    host.make(&mut asm, held);
    host.collect(&mut asm);
    asm.begin_region(&[Catch::Tag(boom, by_tag)]);
    asm.throw_ref(0, caught);
    asm.end_region();
    asm.place(by_tag).unwrap();
    host.check(&mut asm, payload);
    asm.ret(0, &[]);
    b.define_function(caught_fn, asm).unwrap();

    // `yielded()` makes 1 and suspends `pause` to the embedding, which collects.
    let yielded = b.declare_function(sig(&[], &[])).unwrap();
    let mut asm = b.function_asm(yielded).unwrap();
    let held = asm.alloc_register(OBJ);
    host.make(&mut asm, held);
    asm.suspend(0, pause, 0, &[], &[]);
    host.check(&mut asm, held);
    asm.ret(0, &[]);
    b.define_function(yielded, asm).unwrap();

    Fixture {
        program: Arc::new(b.build_verified().unwrap()),
        holder,
        holder_suspend,
        taker,
        asker,
        suspended,
        unstarted,
        bound_suspension,
        dropped,
        consumed,
        caught: caught_fn,
        suspended_exception,
        yielded,
    }
}

#[test]
fn suspended_continuation_registers_are_roots() {
    let f = fixture();
    let heap = drive(&f.program, f.suspended);
    // Object 1 is in two of `holder`'s registers, object 2 only in its tuple.
    let held = heap.roots_of(0, 1);
    assert_eq!(held.len(), 2, "{:?}", heap.collections);
    for root in held {
        let ObjectRootOrigin::Continuation {
            activation: Some(activation),
            location,
            ..
        } = &root.origin
        else {
            panic!("expected a suspended frame's register: {root:?}")
        };
        // The activation keeps its identity once resumed, where `holder` checks the object.
        assert_eq!(activation, &heap.checkers[0]);
        assert_eq!(location.func, f.holder);
        assert_eq!(location.pc, f.holder_suspend);
    }
    let tupled = heap.roots_of(0, 2);
    assert_eq!(tupled.len(), 1);
    assert!(matches!(
        tupled[0].origin,
        ObjectRootOrigin::Aggregate { index: 0, .. }
    ));
    assert_eq!(heap.checkers.len(), 2);
}

#[test]
fn arguments_bound_to_an_unstarted_continuation_are_roots() {
    let f = fixture();
    let heap = drive(&f.program, f.unstarted);
    // The unbound `taker` holds only a default object register, which is never reported.
    assert_eq!(heap.collections[0].len(), 2, "{:?}", heap.collections);
    let bound = heap.roots_of(0, 1);
    assert_eq!(bound.len(), 1);
    let ObjectRootOrigin::Continuation {
        activation,
        location,
        register,
    } = &bound[0].origin
    else {
        panic!("expected an unstarted frame's register: {:?}", bound[0])
    };
    assert_eq!(
        *activation, None,
        "an unstarted frame has no activation yet"
    );
    assert_eq!(location.func, f.taker);
    assert_eq!(*register, 1, "taker's argument");
    assert!(matches!(
        heap.roots_of(0, 2)[..],
        [ObjectRoot {
            origin: ObjectRootOrigin::Register { .. },
            ..
        }]
    ));
    assert_eq!(heap.checkers.len(), 1);
}

#[test]
fn values_bound_to_a_suspended_continuation_are_roots() {
    let f = fixture();
    let heap = drive(&f.program, f.bound_suspension);
    // `asker`'s result register is written by `cont.bind`, not before its `suspend`.
    let bound = heap.roots_of(0, 1);
    assert_eq!(bound.len(), 1, "{:?}", heap.collections);
    assert!(matches!(
        &bound[0].origin,
        ObjectRootOrigin::Continuation {
            activation: Some(activation),
            location,
            ..
        } if location.func == f.asker && *activation == heap.checkers[0]
    ));
    assert_eq!(heap.roots_of(0, 2).len(), 1);
    assert_eq!(heap.checkers.len(), 1);
}

#[test]
fn exception_references_root_their_payload_objects() {
    let f = fixture();
    for (entry, suspended) in [(f.caught, false), (f.suspended_exception, true)] {
        let heap = drive(&f.program, entry);
        // Object 1 is only in the payload of the exception the `Exn` register references.
        let payload = heap.roots_of(0, 1);
        assert_eq!(payload.len(), 1, "{:?}", heap.collections);
        assert_eq!(
            matches!(payload[0].origin, ObjectRootOrigin::Continuation { .. }),
            suspended,
            "{payload:?}"
        );
        assert_eq!(heap.roots_of(0, 2).len(), 1);
        assert_eq!(heap.checkers.len(), 1);
    }
}

#[test]
fn dropped_and_consumed_continuations_are_not_roots() {
    let f = fixture();
    let dropped = drive(&f.program, f.dropped);
    assert_eq!(
        dropped.collections[0],
        [],
        "a dropped continuation holds nothing"
    );
    assert!(dropped.checkers.is_empty());

    let consumed = drive(&f.program, f.consumed);
    assert_eq!(
        consumed.collections[0],
        [],
        "a resumed continuation that returned holds nothing"
    );
    assert_eq!(consumed.checkers.len(), 1);
}

#[test]
fn suspensions_yielded_to_the_embedding_stay_on_the_stack() {
    let f = fixture();
    let heap = drive(&f.program, f.yielded);
    let held = heap.roots_of(0, 1);
    assert_eq!(held.len(), 1, "{:?}", heap.collections);
    assert!(matches!(
        &held[0].origin,
        ObjectRootOrigin::Register { activation, .. } if *activation == heap.checkers[0]
    ));
}
