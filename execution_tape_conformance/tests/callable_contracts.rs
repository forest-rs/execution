// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Callable contracts through ordinary authoring, storage, verification and execution.

use execution_tape::asm::{Asm, FunctionAbi, FunctionSig, ProgramBuilder, Reg};
use execution_tape::contract::{Contract, ContractId, ContractPath, ContractPosition};
use execution_tape::host::{Host, HostContext, HostError, SigHash, ValueRef};
use execution_tape::program::{Program, ValueType};
use execution_tape::trace::TraceMask;
use execution_tape::value::FuncRef;
use execution_tape::value::UnitId;
use execution_tape::value::{Closure, FuncId, Value};
use execution_tape::verifier::{ProgramAnalysis, ValueFact, VerifyConfig, VerifyError};
use execution_tape::vm::{Limits, Trap, Vm};

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
        panic!("unexpected host call")
    }
}
fn body(kinds: &[ValueType]) -> Asm {
    let mut a = Asm::new();
    a.set_register_types(kinds.to_vec());
    a
}
fn sig(args: &[ValueType], rets: &[ValueType]) -> FunctionSig {
    FunctionSig {
        arg_types: args.to_vec(),
        ret_types: rets.to_vec(),
    }
}
fn integer(b: &mut ProgramBuilder) -> ContractId {
    b.contract(Contract::Kind(ValueType::I64)).unwrap()
}
fn nested(b: &mut ProgramBuilder) -> ContractId {
    let i = integer(b);
    let inner = b.contract(Contract::Tuple(&[Some(i)])).unwrap();
    b.contract(Contract::Tuple(&[Some(inner)])).unwrap()
}
fn maker(b: &mut ProgramBuilder, result: ContractId) -> FuncId {
    let mut a = body(&[
        ValueType::Unit,
        ValueType::I64,
        ValueType::Agg,
        ValueType::Agg,
    ]);
    a.const_i64(Reg(1), 42)
        .tuple_new(Reg(2), &[Reg(1)])
        .tuple_new(Reg(3), &[Reg(2)])
        .ret(Reg(0), &[Reg(3)]);
    b.push_function_checked(
        a,
        FunctionAbi {
            args: vec![],
            rets: vec![result.into()],
        },
    )
    .unwrap()
}
#[test]
fn rich_indirect_results_roundtrip_and_feed_projection_and_inspection() {
    let mut b = ProgramBuilder::new();
    let result = nested(&mut b);
    let target = maker(&mut b, result);
    let callable = b
        .contract(Contract::Function {
            args: &[],
            rets: &[result],
        })
        .unwrap();
    let call = b
        .call_sig(FunctionAbi {
            args: vec![],
            rets: vec![result.into()],
        })
        .unwrap();
    let mut a = body(&[
        ValueType::Unit,
        ValueType::Func,
        ValueType::Agg,
        ValueType::I64,
    ]);
    let pc = a.pc();
    a.call_indirect(Reg(0), call, Reg(1), Reg(0), &[], &[Reg(2)])
        .tuple_get(Reg(2), Reg(2), 0)
        .tuple_get(Reg(3), Reg(2), 0)
        .ret(Reg(0), &[Reg(3)]);
    let entry = b
        .push_function_checked(
            a,
            FunctionAbi {
                args: vec![callable.into()],
                rets: vec![ValueType::I64.into()],
            },
        )
        .unwrap();
    let p = b.build_checked().unwrap();
    let p = Program::decode(&p.encode()).unwrap();
    let verified =
        execution_tape::verifier::verify_program_owned(p, &VerifyConfig::default()).unwrap();
    let mut facts = ProgramAnalysis::from(&verified).function(entry).unwrap();
    let at = facts.instruction(pc).unwrap();
    let ValueFact::Contract(id) = at.register(Reg(1)).unwrap().before else {
        panic!("missing callable fact")
    };
    assert!(
        matches!(at.contracts().get(id),Some(Contract::Function{args:[],rets}) if rets.len()==1)
    );
    let ValueFact::Contract(id) = at.register(Reg(2)).unwrap().after else {
        panic!("missing result fact")
    };
    assert!(matches!(at.contracts().get(id), Some(Contract::Tuple(_))));
    assert_eq!(
        {
            let mut vm = Vm::new(NoHost, Limits::default());
            let unit = vm.install(verified.clone()).unwrap();
            vm.run(
                FuncRef::new(unit, entry),
                &[Value::Func(FuncRef::new(UnitId(0), target))],
                TraceMask::NONE,
                None,
            )
        }
        .unwrap(),
        vec![Value::I64(42)]
    );
}
#[test]
fn callable_values_survive_aggregate_projection_and_aliasing() {
    let mut b = ProgramBuilder::new();
    let result = nested(&mut b);
    let target = maker(&mut b, result);
    let call = b
        .call_sig(FunctionAbi {
            args: vec![],
            rets: vec![result.into()],
        })
        .unwrap();
    let mut a = body(&[
        ValueType::Unit,
        ValueType::Func,
        ValueType::Agg,
        ValueType::I64,
    ]);
    a.const_func(Reg(1), target)
        .tuple_new(Reg(2), &[Reg(1)])
        .tuple_get(Reg(1), Reg(2), 0)
        .call_indirect(Reg(0), call, Reg(1), Reg(0), &[], &[Reg(2)])
        .tuple_get(Reg(2), Reg(2), 0)
        .tuple_get(Reg(3), Reg(2), 0)
        .ret(Reg(0), &[Reg(3)]);
    let entry = b
        .push_function_checked(a, sig(&[], &[ValueType::I64]))
        .unwrap();
    let p = b.build_verified().unwrap();
    assert_eq!(
        {
            let mut vm = Vm::new(NoHost, Limits::default());
            let unit = vm.install(p.clone()).unwrap();
            vm.run(FuncRef::new(unit, entry), &[], TraceMask::NONE, None)
        }
        .unwrap(),
        vec![Value::I64(42)]
    );
}
fn closure_body(b: &mut ProgramBuilder, environment: ContractId) -> FuncId {
    let mut a = body(&[ValueType::Unit, ValueType::Agg, ValueType::I64]);
    a.tuple_get(Reg(2), Reg(1), 0).ret(Reg(0), &[Reg(2)]);
    b.push_function_checked(
        a,
        FunctionAbi {
            args: vec![environment.into()],
            rets: vec![ValueType::I64.into()],
        },
    )
    .unwrap()
}
#[test]
fn closure_construction_preserves_exposed_signature_and_checks_concrete_environment() {
    let mut b = ProgramBuilder::new();
    let i = integer(&mut b);
    let env = b.contract(Contract::Tuple(&[Some(i)])).unwrap();
    let target = closure_body(&mut b, env);
    let call = b.call_sig(sig(&[], &[ValueType::I64])).unwrap();
    let mut a = body(&[
        ValueType::Unit,
        ValueType::Agg,
        ValueType::Func,
        ValueType::Closure,
        ValueType::I64,
    ]);
    a.const_func(Reg(2), target)
        .closure_new(Reg(3), Reg(2), Reg(1))
        .call_indirect(Reg(0), call, Reg(3), Reg(0), &[], &[Reg(4)])
        .ret(Reg(0), &[Reg(4)]);
    let entry = b
        .push_function_checked(a, sig(&[ValueType::Agg], &[ValueType::I64]))
        .unwrap();
    let p = b.build_verified().unwrap();
    let mut vm = Vm::new(NoHost, Limits::default());
    let good = vm.aggregates_mut().tuple_new(vec![Value::I64(42)]).unwrap();
    let unit = vm.install(p.clone()).unwrap();
    assert_eq!(
        vm.run(
            FuncRef::new(unit, entry),
            &[Value::Agg(good)],
            TraceMask::NONE,
            None
        )
        .unwrap(),
        vec![Value::I64(42)]
    );
    let bad = vm
        .aggregates_mut()
        .tuple_new(vec![Value::Bool(true)])
        .unwrap();
    assert!(
        matches!(vm.run(FuncRef::new(unit, entry),&[Value::Agg(bad)],TraceMask::NONE,None).unwrap_err().trap,Trap::ContractMismatch{function,position:ContractPosition::Argument{index:0,..},..} if function==FuncRef::new(UnitId(0), target))
    );
}
#[test]
fn rich_closure_boundaries_check_hidden_environments_and_do_not_reuse_another_targets_proof() {
    let mut b = ProgramBuilder::new();
    let i = integer(&mut b);
    let boolean = b.contract(Contract::Kind(ValueType::Bool)).unwrap();
    let ints = b.contract(Contract::Tuple(&[Some(i)])).unwrap();
    let bools = b.contract(Contract::Tuple(&[Some(boolean)])).unwrap();
    let first = closure_body(&mut b, ints);
    let mut other = body(&[ValueType::Unit, ValueType::Agg, ValueType::I64]);
    other.const_i64(Reg(2), 7).ret(Reg(0), &[Reg(2)]);
    let second = b
        .push_function_checked(
            other,
            FunctionAbi {
                args: vec![bools.into()],
                rets: vec![i.into()],
            },
        )
        .unwrap();
    let closure = b
        .contract(Contract::Closure {
            args: &[],
            rets: &[i],
        })
        .unwrap();
    let mut a = body(&[ValueType::Unit, ValueType::Closure, ValueType::Closure]);
    a.ret(Reg(0), &[]);
    let entry = b
        .push_function_checked(
            a,
            FunctionAbi {
                args: vec![closure.into(), closure.into()],
                rets: vec![],
            },
        )
        .unwrap();
    let source = b.build_checked().unwrap();
    let decoded = Program::decode(&source.encode()).unwrap();
    assert_eq!(source, decoded);
    let p =
        execution_tape::verifier::verify_program_owned(decoded, &VerifyConfig::default()).unwrap();
    let mut vm = Vm::new(NoHost, Limits::default());
    let env = vm.aggregates_mut().tuple_new(vec![Value::I64(42)]).unwrap();
    let unit = vm.install(p.clone()).unwrap();
    let error = vm
        .run(
            FuncRef::new(unit, entry),
            &[
                Value::Closure(Closure {
                    func: FuncRef::new(UnitId(0), first),
                    env,
                }),
                Value::Closure(Closure {
                    func: FuncRef::new(UnitId(0), second),
                    env,
                }),
            ],
            TraceMask::NONE,
            None,
        )
        .unwrap_err();
    let Trap::ContractMismatch {
        position: ContractPosition::Argument { index: 1, .. },
        error,
        ..
    } = error.trap
    else {
        panic!("wrong failure")
    };
    assert_eq!(
        error.path,
        vec![ContractPath::Environment, ContractPath::Member(0)]
    );
    assert!(error.to_string().contains(".environment[0]"));
}
#[test]
fn exact_rich_identity_rejects_equal_kind_hashes_before_dispatch() {
    let mut b = ProgramBuilder::new();
    let result = nested(&mut b);
    let target = maker(&mut b, result);
    let boolean = b.contract(Contract::Kind(ValueType::Bool)).unwrap();
    let wrong = b.contract(Contract::Tuple(&[Some(boolean)])).unwrap();
    let call = b
        .call_sig(FunctionAbi {
            args: vec![],
            rets: vec![wrong.into()],
        })
        .unwrap();
    let mut a = body(&[ValueType::Unit, ValueType::Func, ValueType::Agg]);
    a.call_indirect(Reg(0), call, Reg(1), Reg(0), &[], &[Reg(2)])
        .ret(Reg(0), &[]);
    let entry = b
        .push_function_checked(a, sig(&[ValueType::Func], &[]))
        .unwrap();
    let p = b.build_verified().unwrap();
    assert!(matches!({
 let mut vm = Vm::new(NoHost,Limits::default());
 let unit = vm.install(p.clone()).unwrap();
 vm.run(FuncRef::new(unit, entry), &[Value::Func(FuncRef::new(UnitId(0), target))], TraceMask::NONE, None)
 }.unwrap_err().trap,Trap::CallableSignatureMismatch{signature,function} if signature==call && function==FuncRef::new(UnitId(0), target)));
}
#[test]
fn statically_known_incompatible_indirect_targets_are_rejected() {
    let mut b = ProgramBuilder::new();
    let result = nested(&mut b);
    let target = maker(&mut b, result);
    let call = b.call_sig(sig(&[], &[ValueType::Agg])).unwrap();
    let mut a = body(&[ValueType::Unit, ValueType::Func, ValueType::Agg]);
    a.const_func(Reg(1), target)
        .call_indirect(Reg(0), call, Reg(1), Reg(0), &[], &[Reg(2)])
        .ret(Reg(0), &[]);
    b.push_function_checked(a, sig(&[], &[])).unwrap();
    assert!(matches!(
        b.build_verified(),
        Err(execution_tape::asm::BuildError::Verify(rejected))
            if matches!(rejected.error, VerifyError::CallIndirectContractMismatch { .. })
    ));
}

#[test]
fn higher_order_returns_and_nested_callable_failures_keep_their_contracts() {
    let mut b = ProgramBuilder::new();
    let i = integer(&mut b);
    let callable = b
        .contract(Contract::Function {
            args: &[],
            rets: &[i],
        })
        .unwrap();
    let tuple = b.contract(Contract::Tuple(&[Some(callable)])).unwrap();
    let mut target = body(&[ValueType::Unit, ValueType::I64]);
    target.const_i64(Reg(1), 42).ret(Reg(0), &[Reg(1)]);
    let target = b
        .push_function_checked(target, sig(&[], &[ValueType::I64]))
        .unwrap();
    let mut get = body(&[ValueType::Unit, ValueType::Func]);
    get.const_func(Reg(1), target).ret(Reg(0), &[Reg(1)]);
    let get = b
        .push_function_checked(
            get,
            FunctionAbi {
                args: vec![],
                rets: vec![callable.into()],
            },
        )
        .unwrap();
    let call = b.call_sig(sig(&[], &[ValueType::I64])).unwrap();
    let mut main = body(&[ValueType::Unit, ValueType::Func, ValueType::I64]);
    main.call(Reg(0), get, Reg(0), &[], &[Reg(1)])
        .call_indirect(Reg(0), call, Reg(1), Reg(0), &[], &[Reg(2)])
        .ret(Reg(0), &[Reg(2)]);
    let entry = b
        .push_function_checked(main, sig(&[], &[ValueType::I64]))
        .unwrap();
    let mut take = body(&[ValueType::Unit, ValueType::Agg]);
    take.ret(Reg(0), &[]);
    let take = b
        .push_function_checked(
            take,
            FunctionAbi {
                args: vec![tuple.into()],
                rets: vec![],
            },
        )
        .unwrap();
    let p = b.build_verified().unwrap();
    let mut vm = Vm::new(NoHost, Limits::default());
    let unit = vm.install(p.clone()).unwrap();
    assert_eq!(
        vm.run(FuncRef::new(unit, entry), &[], TraceMask::NONE, None)
            .unwrap(),
        vec![Value::I64(42)]
    );
    let invalid = vm
        .aggregates_mut()
        .tuple_new(vec![Value::Func(FuncRef::new(UnitId(0), FuncId(u32::MAX)))])
        .unwrap();
    let error = vm
        .run(
            FuncRef::new(unit, take),
            &[Value::Agg(invalid)],
            TraceMask::NONE,
            None,
        )
        .unwrap_err();
    let Trap::ContractMismatch { error, .. } = error.trap else {
        panic!("missing nested failure")
    };
    assert_eq!(error.path, vec![ContractPath::Member(0)]);
}

#[test]
fn callable_signature_positions_and_cached_visits_are_budgeted() {
    let mut b = ProgramBuilder::new();
    let i = integer(&mut b);
    let callable = b
        .contract(Contract::Function {
            args: &[i],
            rets: &[i],
        })
        .unwrap();
    let mut target = body(&[ValueType::Unit, ValueType::I64]);
    target.ret(Reg(0), &[Reg(1)]);
    let target = b
        .push_function_checked(target, sig(&[ValueType::I64], &[ValueType::I64]))
        .unwrap();
    let mut entry = body(&[ValueType::Unit, ValueType::Func, ValueType::Func]);
    entry.ret(Reg(0), &[]);
    let entry = b
        .push_function_checked(
            entry,
            FunctionAbi {
                args: vec![callable.into(), callable.into()],
                rets: vec![],
            },
        )
        .unwrap();
    let p = b.build_verified().unwrap();
    // First callable: value visit + argument position + return position. Second: cached visit.
    for limit in [0, 2, 3, 4] {
        let result = {
            let mut vm = Vm::new(
                NoHost,
                Limits {
                    max_contract_work: limit,
                    ..Limits::default()
                },
            );
            let unit = vm.install(p.clone()).unwrap();
            vm.run(
                FuncRef::new(unit, entry),
                &[
                    Value::Func(FuncRef::new(UnitId(0), target)),
                    Value::Func(FuncRef::new(UnitId(0), target)),
                ],
                TraceMask::NONE,
                None,
            )
        };
        if limit == 4 {
            result.unwrap();
        } else {
            assert!(
                matches!(result.unwrap_err().trap,Trap::ContractWorkExceeded{position:ContractPosition::Argument{index,..},..} if index==u32::from(limit==3))
            );
        }
    }
}

#[test]
fn opaque_closures_validate_even_an_unused_unrefined_environment_before_entry() {
    let mut b = ProgramBuilder::new();
    let mut target = body(&[ValueType::Unit, ValueType::Agg, ValueType::I64]);
    target.const_i64(Reg(2), 42).ret(Reg(0), &[Reg(2)]);
    let target = b
        .push_function_checked(target, sig(&[ValueType::Agg], &[ValueType::I64]))
        .unwrap();
    let call = b.call_sig(sig(&[], &[ValueType::I64])).unwrap();
    let mut entry = body(&[ValueType::Unit, ValueType::Closure, ValueType::I64]);
    entry
        .call_indirect(Reg(0), call, Reg(1), Reg(0), &[], &[Reg(2)])
        .ret(Reg(0), &[Reg(2)]);
    let entry = b
        .push_function_checked(entry, sig(&[ValueType::Closure], &[ValueType::I64]))
        .unwrap();
    let p = b.build_verified().unwrap();
    let error = {
        let mut vm = Vm::new(NoHost, Limits::default());
        let unit = vm.install(p.clone()).unwrap();
        vm.run(
            FuncRef::new(unit, entry),
            &[Value::Closure(Closure {
                func: FuncRef::new(UnitId(0), target),
                env: execution_tape::value::AggHandle(0),
            })],
            TraceMask::NONE,
            None,
        )
    }
    .unwrap_err();
    assert!(
        matches!(error.trap,Trap::ContractMismatch{function,position:ContractPosition::Argument{index:0,..},..} if function==FuncRef::new(UnitId(0), target))
    );
}

#[test]
fn selects_and_nominal_arrays_keep_signatures_without_merging_function_identity() {
    let mut b = ProgramBuilder::new();
    let mut targets = Vec::new();
    for number in [42, 7] {
        let mut target = body(&[ValueType::Unit, ValueType::I64]);
        target.const_i64(Reg(1), number).ret(Reg(0), &[Reg(1)]);
        targets.push(
            b.push_function_checked(target, sig(&[], &[ValueType::I64]))
                .unwrap(),
        );
    }
    let element = b.array_elem(ValueType::Func);
    let call = b.call_sig(sig(&[], &[ValueType::I64])).unwrap();
    let mut entry = body(&[
        ValueType::Unit,
        ValueType::Bool,
        ValueType::Func,
        ValueType::Func,
        ValueType::Agg,
        ValueType::I64,
    ]);
    entry
        .const_func(Reg(2), targets[0])
        .const_func(Reg(3), targets[1])
        .select(Reg(2), Reg(1), Reg(2), Reg(3))
        .array_new(Reg(4), element, &[Reg(2), Reg(3)])
        .array_get_imm(Reg(2), Reg(4), 0)
        .call_indirect(Reg(0), call, Reg(2), Reg(0), &[], &[Reg(5)])
        .ret(Reg(0), &[Reg(5)]);
    let entry = b
        .push_function_checked(entry, sig(&[ValueType::Bool], &[ValueType::I64]))
        .unwrap();
    let p = b.build_verified().unwrap();
    let mut vm = Vm::new(NoHost, Limits::default());
    for (flag, number) in [(true, 42), (false, 7)] {
        let unit = vm.install(p.clone()).unwrap();
        assert_eq!(
            vm.run(
                FuncRef::new(unit, entry),
                &[Value::Bool(flag)],
                TraceMask::NONE,
                None
            )
            .unwrap(),
            vec![Value::I64(number)]
        );
    }
}

#[test]
fn an_opaque_host_write_erases_the_destinations_previous_callable_proof() {
    use execution_tape::host::HostSig;
    struct Choose(FuncId);
    impl Host for Choose {
        fn call(
            &mut self,
            _: &str,
            _: SigHash,
            _: &[ValueRef<'_>],
            rets: &mut [Value],
            _: HostContext<'_, '_>,
        ) -> Result<u64, HostError> {
            rets[0] = Value::Func(FuncRef::new(UnitId(0), self.0));
            Ok(0)
        }
    }
    let mut b = ProgramBuilder::new();
    let mut good = body(&[ValueType::Unit, ValueType::I64]);
    good.const_i64(Reg(1), 42).ret(Reg(0), &[Reg(1)]);
    let good = b
        .push_function_checked(good, sig(&[], &[ValueType::I64]))
        .unwrap();
    let mut bad = body(&[ValueType::Unit, ValueType::Bool]);
    bad.const_bool(Reg(1), true).ret(Reg(0), &[Reg(1)]);
    let bad = b
        .push_function_checked(bad, sig(&[], &[ValueType::Bool]))
        .unwrap();
    let host = b.host_sig_for(
        "choose",
        HostSig {
            args: vec![],
            rets: vec![ValueType::Func],
        },
    );
    let call = b.call_sig(sig(&[], &[ValueType::I64])).unwrap();
    let mut entry = body(&[ValueType::Unit, ValueType::Func, ValueType::I64]);
    entry
        .const_func(Reg(1), good)
        .host_call(Reg(0), host, Reg(0), &[], &[Reg(1)])
        .call_indirect(Reg(0), call, Reg(1), Reg(0), &[], &[Reg(2)])
        .ret(Reg(0), &[Reg(2)]);
    let entry = b
        .push_function_checked(entry, sig(&[], &[ValueType::I64]))
        .unwrap();
    let p = b.build_verified().unwrap();
    assert!(matches!(
        {
            let mut vm = Vm::new(Choose(bad), Limits::default());
            let unit = vm.install(p.clone()).unwrap();
            vm.run(FuncRef::new(unit, entry), &[], TraceMask::NONE, None)
        }
        .unwrap_err()
        .trap,
        Trap::TypeMismatch {
            expected: ValueType::I64,
            actual: ValueType::Bool
        }
    ));
}
