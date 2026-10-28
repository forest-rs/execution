// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Paused source boundaries and object roots remain valid through calls and bank reuse.

use execution_tape::{
    asm::{FunctionSig, ProgramBuilder},
    host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef},
    program::{HostTypeId, ValueType},
    source::{SourceDocument, SourceId, SourceLimits, SourceMap, SourceMapping},
    trace::TraceLocation,
    value::{FuncId, Obj, ObjHandle, Value},
    verifier::VerifiedProgram,
    vm::{InvocationOptions, Limits, ObjectRootOrigin, RunStatus, Vm},
};

const OBJECT: HostTypeId = HostTypeId(73);
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
        panic!("no host calls");
    }
}

fn calls() -> (VerifiedProgram, FuncId, u32, u32) {
    let mut b = ProgramBuilder::new();
    let child = b
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::Obj(OBJECT)],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let mut a = b.function_asm(child).unwrap();
    let result = a.alloc_register(ValueType::I64);
    a.const_i64(result, 7).ret(0, &[result]);
    b.define_function(child, a).unwrap();
    let entry = b
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::Obj(OBJECT)],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let mut a = b.function_asm(entry).unwrap();
    let result = a.alloc_register(ValueType::I64); // r2, uninitialized until first return
    let _unused = a.alloc_register(ValueType::Obj(OBJECT)); // r3 must never appear as an object root
    let old = a.alloc_register(ValueType::I64); // r4 has a value before the second call
    a.const_i64(old, 99);
    let first = a.pc();
    a.call(0, child, 0, &[1], &[result]);
    let second = a.pc();
    a.call(0, child, 0, &[1], &[old])
        .i64_add(result, result, old)
        .ret(0, &[result]);
    b.define_function(entry, a).unwrap();
    (b.build_verified().unwrap(), entry, first, second)
}

#[test]
fn waiting_callers_show_the_pending_call_and_only_arrived_results() {
    let (p, entry, first, second) = calls();
    let args = [Value::Obj(object(42))];
    let mut vm = Vm::new(NoHost, Limits::default());
    let mut run = vm
        .start(&p, entry, &args, InvocationOptions::default())
        .unwrap();
    let root = run.inspect().frames().next().unwrap().activation;
    assert_eq!(run.resume(2).unwrap(), RunStatus::Suspended); // initialize old, enter child
    let child = {
        let mut inspection = run.inspect();
        let frames: Vec<_> = inspection.frames().collect();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[1].activation, root);
        assert!(frames[1].waiting_for_return);
        assert_eq!(
            frames[1].location,
            TraceLocation {
                func: entry,
                pc: first
            }
        );
        assert_eq!(frames[0].caller, Some(frames[1].location));
        assert!(!frames[0].waiting_for_return);
        assert!(inspection.register(&root, 2).unwrap().value.is_none());
        assert!(inspection.register(&root, 3).unwrap().value.is_none());
        assert_eq!(
            inspection.register(&root, 4).unwrap().value,
            Some(ValueRef::I64(99))
        );
        assert!(
            inspection
                .register(&frames[0].activation, 2)
                .unwrap()
                .value
                .is_none()
        );
        assert_eq!(
            inspection.register(&root, 0).unwrap().value,
            Some(ValueRef::Unit)
        );
        frames[0].activation.clone()
    };
    run.resume(1).unwrap(); // child defines result
    assert_eq!(
        run.inspect().register(&child, 2).unwrap().value,
        Some(ValueRef::I64(7))
    );
    run.resume(1).unwrap(); // return to root
    {
        let mut inspection = run.inspect();
        assert!(
            inspection.register(&child, 2).is_none(),
            "returned activation is retired"
        );
        assert_eq!(
            inspection.register(&root, 2).unwrap().value,
            Some(ValueRef::I64(7))
        );
        assert_eq!(inspection.frames().next().unwrap().location.pc, second);
    }
    run.resume(1).unwrap(); // enter the same child at the same physical bank base
    {
        let mut inspection = run.inspect();
        let replacement = inspection.frames().next().unwrap();
        assert_ne!(replacement.activation, child);
        assert!(inspection.register(&child, 2).is_none());
        assert!(
            inspection
                .register(&replacement.activation, 2)
                .unwrap()
                .value
                .is_none(),
            "bank reuse must not fabricate initialization"
        );
        assert_eq!(
            inspection.register(&root, 4).unwrap().value,
            Some(ValueRef::I64(99)),
            "pending result must not replace the old value"
        );
    }
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::Complete);
    assert_eq!(run.take_outputs().unwrap(), [Value::I64(14)]);
    assert_eq!(run.inspect().frames().len(), 0);
    assert!(run.inspect().register(&root, 2).is_none());
}

#[test]
fn activation_ids_reject_other_invocations_and_remain_stable_across_large_quanta() {
    let (p, entry, _, _) = calls();
    let args = [Value::Obj(object(42))];
    let mut a = Vm::new(NoHost, Limits::default());
    let mut b = Vm::new(NoHost, Limits::default());
    let mut left = a
        .start(&p, entry, &args, InvocationOptions::default())
        .unwrap();
    let right = b
        .start(&p, entry, &args, InvocationOptions::default())
        .unwrap();
    let old = left.inspect().frames().next().unwrap().activation;
    let other = right.inspect().frames().next().unwrap().activation;
    assert_eq!(old.ordinal(), other.ordinal());
    assert_ne!(old, other);
    assert!(right.inspect().register(&old, 1).is_none());
    left.resume(4).unwrap(); // first child entered and retired before this suspension
    assert_eq!(left.inspect().frames().next().unwrap().activation, old);
    left.resume(1).unwrap();
    assert_eq!(
        left.inspect().frames().next().unwrap().activation.ordinal(),
        2
    );
    drop(left);
    let replacement = a
        .start(&p, entry, &args, InvocationOptions::default())
        .unwrap();
    assert_ne!(
        replacement.inspect().frames().next().unwrap().activation,
        old
    );
}

#[test]
fn paused_call_sites_resolve_to_exact_authored_source() {
    let (p, entry, first, _) = calls();
    let text = "first(receiver)\nsecond(receiver)\n";
    let map = SourceMap::new(
        p.program(),
        vec![SourceDocument {
            name: "methods.expr".into(),
            text: text.into(),
        }],
        vec![SourceMapping {
            location: TraceLocation {
                func: entry,
                pc: first,
            },
            document: SourceId(0),
            range: 0..15,
        }],
        SourceLimits::default(),
    )
    .unwrap();
    let bound = map.bind(p.program()).unwrap();
    let args = [Value::Obj(object(42))];
    let mut vm = Vm::new(NoHost, Limits::default());
    let mut run = vm
        .start(&p, entry, &args, InvocationOptions::default())
        .unwrap();
    run.resume(2).unwrap();
    let caller = run.inspect().frames().nth(1).unwrap();
    assert_eq!(
        bound.lookup(caller.location).unwrap().text(),
        "first(receiver)"
    );
}

struct Store {
    live: Vec<u64>,
}
impl Host for Store {
    fn call(
        &mut self,
        symbol: &str,
        _: SigHash,
        args: &[ValueRef<'_>],
        rets: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        if symbol == "new" {
            rets[0] = Value::Obj(object(44));
        } else {
            let [ValueRef::Obj(object)] = args else {
                return Err(HostError::Failed);
            };
            if !self.live.contains(&object.handle.0) {
                return Err(HostError::Failed);
            }
            rets[0] = Value::I64(i64::try_from(object.handle.0).unwrap());
        }
        Ok(0)
    }
}

#[test]
fn conservative_roots_support_a_real_host_collection_between_quanta() {
    let mut b = ProgramBuilder::new();
    let new = b.host_sig_for(
        "new",
        HostSig {
            args: vec![],
            rets: vec![ValueType::Obj(OBJECT)],
        },
    );
    let check = b.host_sig_for(
        "check",
        HostSig {
            args: vec![ValueType::Obj(OBJECT)],
            rets: vec![ValueType::I64],
        },
    );
    let entry = b
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::Obj(OBJECT)],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let mut a = b.function_asm(entry).unwrap();
    let temp = a.alloc_register(ValueType::Obj(OBJECT));
    let tuple = a.alloc_register(ValueType::Agg);
    let result = a.alloc_register(ValueType::I64);
    let _unused = a.alloc_register(ValueType::Obj(OBJECT));
    a.host_call_sig_id(0, new, 0, &[], &[temp])
        .tuple_new(tuple, &[temp])
        .mov(temp, 1)
        .host_call_sig_id(0, check, 0, &[1], &[result])
        .tuple_get(temp, tuple, 0)
        .host_call_sig_id(0, check, 0, &[temp], &[result])
        .ret(0, &[result]);
    b.define_function(entry, a).unwrap();
    let p = b.build_verified().unwrap();
    let mut vm = Vm::new(
        Store {
            live: vec![42, 43, 44, 99],
        },
        Limits::default(),
    );
    let retained = vm
        .aggregates_mut()
        .tuple_new(vec![Value::Obj(object(43))])
        .unwrap();
    let args = [Value::Obj(object(42))];
    let mut run = vm
        .start(&p, entry, &args, InvocationOptions::default())
        .unwrap();
    run.resume(3).unwrap();
    let roots = {
        let mut roots = Vec::new();
        run.inspect().visit_objects(|root| roots.push(root));
        roots
    };
    assert!(
        roots
            .iter()
            .all(|root| [42, 43, 44].contains(&root.object.handle.0)),
        "uninitialized object defaults are absent"
    );
    assert!(
        roots
            .iter()
            .any(|root| matches!(root.origin, ObjectRootOrigin::Argument { index: 0 }))
    );
    assert!(roots.iter().any(|root| matches!(root.origin, ObjectRootOrigin::Aggregate { aggregate, .. } if aggregate == retained)));
    assert!(
        roots.iter().any(|root| root.object == object(44)),
        "temporary aggregate contents remain rooted after overwriting their source register"
    );
    run.host_mut()
        .live
        .retain(|id| roots.iter().any(|root| root.object.handle.0 == *id));
    assert_eq!(run.host().live, [42, 43, 44]);
    assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::Complete);
    assert_eq!(run.take_outputs().unwrap(), [Value::I64(44)]);
    drop(run);
    assert_eq!(
        vm.aggregates().len_u32(),
        1,
        "unreturned temporary aggregates are reclaimed"
    );
    assert_eq!(vm.aggregates().object_references().count(), 1);
}

#[test]
fn indirect_functions_and_closures_keep_real_activation_and_environment_roots() {
    for closure in [false, true] {
        let mut b = ProgramBuilder::new();
        let child = b
            .declare_function(FunctionSig {
                arg_types: vec![if closure {
                    ValueType::Agg
                } else {
                    ValueType::Obj(OBJECT)
                }],
                ret_types: vec![ValueType::I64],
            })
            .unwrap();
        let mut a = b.function_asm(child).unwrap();
        let result = a.alloc_register(ValueType::I64);
        a.const_i64(result, 7).ret(0, &[result]);
        b.define_function(child, a).unwrap();
        let signature = b
            .call_sig(FunctionSig {
                arg_types: if closure {
                    vec![]
                } else {
                    vec![ValueType::Obj(OBJECT)]
                },
                ret_types: vec![ValueType::I64],
            })
            .unwrap();
        let entry = b
            .declare_function(FunctionSig {
                arg_types: vec![ValueType::Obj(OBJECT)],
                ret_types: vec![ValueType::I64],
            })
            .unwrap();
        let mut a = b.function_asm(entry).unwrap();
        let function = a.alloc_register(ValueType::Func);
        a.const_func(function, child);
        let callee = if closure {
            let env = a.alloc_register(ValueType::Agg);
            let value = a.alloc_register(ValueType::Closure);
            a.tuple_new(env, &[1]).closure_new(value, function, env);
            value
        } else {
            function
        };
        let result = a.alloc_register(ValueType::I64);
        let call_pc = a.pc();
        a.call_indirect(
            0,
            signature,
            callee,
            0,
            if closure { &[] } else { &[1] },
            &[result],
        )
        .ret(0, &[result]);
        b.define_function(entry, a).unwrap();
        let p = b.build_verified().unwrap();
        let mut vm = Vm::new(NoHost, Limits::default());
        let args = [Value::Obj(object(42))];
        let mut run = vm
            .start(&p, entry, &args, InvocationOptions::default())
            .unwrap();
        assert_eq!(
            run.resume(if closure { 4 } else { 2 }).unwrap(),
            RunStatus::Suspended
        );
        {
            let mut view = run.inspect();
            let frames: Vec<_> = view.frames().collect();
            assert_eq!(frames[0].location.func, child);
            assert_eq!(
                frames[0].caller,
                Some(TraceLocation {
                    func: entry,
                    pc: call_pc
                })
            );
            assert!(
                view.register(&frames[0].activation, 2)
                    .unwrap()
                    .value
                    .is_none()
            );
            let mut roots = Vec::new();
            view.visit_objects(|root| roots.push(root));
            assert!(roots.iter().all(|root| root.object == object(42)));
            if closure {
                assert!(matches!(
                    view.register(&frames[0].activation, 1).unwrap().value,
                    Some(ValueRef::Agg(_))
                ));
                assert!(
                    roots
                        .iter()
                        .any(|root| matches!(root.origin, ObjectRootOrigin::Aggregate { .. }))
                );
            }
        }
        assert_eq!(run.resume(u64::MAX).unwrap(), RunStatus::Complete);
        assert_eq!(run.take_outputs().unwrap(), [Value::I64(7)]);
    }
}
