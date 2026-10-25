// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Public consumer checks for compositional guest contracts.

use execution_tape::asm::{Asm, FunctionAbi, FunctionSig, ProgramBuilder};
use execution_tape::contract::{Contract, ContractId, ContractMismatch, ContractPosition};
use execution_tape::host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef};
use execution_tape::program::{Program, ValueType};
use execution_tape::trace::TraceMask;
use execution_tape::value::Value;
use execution_tape::verifier::{VerifyConfig, VerifyError, verify_program_owned};
use execution_tape::vm::{ExecutionContext, Limits, Trap, Vm};

struct TestHost(Value);
impl Host for TestHost {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        _: &[ValueRef<'_>],
        rets: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        rets[0] = self.0.clone();
        Ok(0)
    }
}
fn integer(builder: &mut ProgramBuilder) -> ContractId {
    builder.contract(Contract::Kind(ValueType::I64)).unwrap()
}
fn tuple(builder: &mut ProgramBuilder, fields: &[Option<ContractId>]) -> ContractId {
    builder.contract(Contract::Tuple(fields)).unwrap()
}
fn body(types: &[ValueType]) -> Asm {
    let mut asm = Asm::new();
    asm.set_register_types(types.to_vec());
    asm
}

#[test]
fn nested_result_survives_codec_call_and_aliased_projections() {
    let mut builder = ProgramBuilder::new();
    let i64_type = integer(&mut builder);
    let inner = tuple(&mut builder, &[Some(i64_type)]);
    let outer = tuple(&mut builder, &[Some(inner)]);
    let mut make = body(&[
        ValueType::Unit,
        ValueType::I64,
        ValueType::Agg,
        ValueType::Agg,
    ]);
    make.const_i64(1, 42)
        .tuple_new(2, &[1])
        .tuple_new(3, &[2])
        .ret(0, &[3]);
    let make = builder
        .push_function_checked(
            make,
            FunctionAbi {
                args: vec![],
                rets: vec![outer.into()],
            },
        )
        .unwrap();
    let mut main = body(&[ValueType::Unit, ValueType::Agg, ValueType::I64]);
    main.call(0, make, 0, &[], &[1])
        .tuple_get(1, 1, 0)
        .tuple_get(2, 1, 0)
        .ret(0, &[2]);
    let entry = builder
        .push_function_checked(
            main,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
            },
        )
        .unwrap();
    let source = builder.build_checked().unwrap();
    let encoded = source.encode();
    let decoded = Program::decode(&encoded).unwrap();
    assert_eq!(source, decoded);
    let prepared = verify_program_owned(decoded, &VerifyConfig::default()).unwrap();
    assert_eq!(prepared.function(make).unwrap().ret_contracts(), &[outer]);
    assert_eq!(
        prepared.function(make).unwrap().ret_types(),
        &[ValueType::Agg]
    );
    let mut vm = Vm::new(TestHost(Value::Unit), Limits::default());
    assert_eq!(
        vm.run(&prepared, entry, &[], TraceMask::NONE, None)
            .unwrap(),
        vec![Value::I64(42)]
    );
}

#[test]
fn local_nested_values_need_no_extra_annotations() {
    let mut builder = ProgramBuilder::new();
    let mut main = body(&[
        ValueType::Unit,
        ValueType::I64,
        ValueType::Agg,
        ValueType::Agg,
        ValueType::I64,
    ]);
    main.const_i64(1, 42)
        .tuple_new(2, &[1])
        .tuple_new(3, &[2])
        .tuple_get(3, 3, 0)
        .tuple_get(4, 3, 0)
        .ret(0, &[4]);
    let entry = builder
        .push_function_checked(
            main,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
            },
        )
        .unwrap();
    let prepared = builder.build_verified().unwrap();
    let mut vm = Vm::new(TestHost(Value::Unit), Limits::default());
    assert_eq!(
        vm.run(&prepared, entry, &[], TraceMask::NONE, None)
            .unwrap(),
        vec![Value::I64(42)]
    );
}

#[test]
fn statically_wrong_nested_result_reports_member_path() {
    let mut builder = ProgramBuilder::new();
    let i64_type = integer(&mut builder);
    let inner = tuple(&mut builder, &[Some(i64_type)]);
    let outer = tuple(&mut builder, &[Some(inner)]);
    let mut make = body(&[
        ValueType::Unit,
        ValueType::Bool,
        ValueType::Agg,
        ValueType::Agg,
    ]);
    make.const_bool(1, true)
        .tuple_new(2, &[1])
        .tuple_new(3, &[2])
        .ret(0, &[3]);
    builder
        .push_function_checked(
            make,
            FunctionAbi {
                args: vec![],
                rets: vec![outer.into()],
            },
        )
        .unwrap();
    match builder.build_verified().unwrap_err() {
        execution_tape::asm::BuildError::Verify(VerifyError::ContractMismatch {
            position: ContractPosition::Result { index: 0 },
            error,
            ..
        }) => {
            assert_eq!(
                error.path,
                vec![
                    execution_tape::contract::ContractPath::Member(0),
                    execution_tape::contract::ContractPath::Member(0)
                ]
            );
            assert_eq!(
                error.mismatch,
                ContractMismatch::Kind {
                    expected: ValueType::I64,
                    actual: ValueType::Bool
                }
            );
        }
        error => panic!("unexpected error: {error:?}"),
    }
}

#[test]
fn opaque_host_result_is_checked_before_return_and_rolls_back_outputs() {
    let mut builder = ProgramBuilder::new();
    let i64_type = integer(&mut builder);
    let inner = tuple(&mut builder, &[Some(i64_type)]);
    let outer = tuple(&mut builder, &[Some(inner)]);
    let import = builder.host_sig_for(
        "opaque",
        HostSig {
            args: vec![],
            rets: vec![ValueType::Agg],
        },
    );
    let mut make = body(&[ValueType::Unit, ValueType::Agg]);
    make.host_call(0, import, 0, &[], &[1]).ret(0, &[1]);
    let entry = builder
        .push_function_checked(
            make,
            FunctionAbi {
                args: vec![],
                rets: vec![outer.into()],
            },
        )
        .unwrap();
    let prepared = builder.build_verified().unwrap();
    let mut vm = Vm::new(TestHost(Value::Unit), Limits::default());
    let bad_inner = vm
        .aggregates_mut()
        .tuple_new(vec![Value::Bool(true)])
        .unwrap();
    let bad_outer = vm
        .aggregates_mut()
        .tuple_new(vec![Value::Agg(bad_inner)])
        .unwrap();
    vm.host_mut().0 = Value::Agg(bad_outer);
    let mut ctx = ExecutionContext::new();
    let mut outputs = vec![Value::I64(99)];
    let error = vm
        .run_with_ctx(
            &mut ctx,
            &prepared,
            entry,
            &[],
            &mut outputs,
            TraceMask::NONE,
            None,
            None,
        )
        .unwrap_err();
    assert_eq!(outputs, vec![Value::I64(99)]);
    assert_eq!(error.func, entry);
    match error.trap {
        Trap::ContractMismatch {
            position: ContractPosition::Result { index: 0 },
            error,
            ..
        } => assert_eq!(
            error.path,
            vec![
                execution_tape::contract::ContractPath::Member(0),
                execution_tape::contract::ContractPath::Member(0)
            ]
        ),
        error => panic!("unexpected trap: {error:?}"),
    }
    // Reusing both the context and numeric handles must not reuse an earlier proof.
    vm.aggregates_mut().clear();
    let good_inner = vm.aggregates_mut().tuple_new(vec![Value::I64(42)]).unwrap();
    let good_outer = vm
        .aggregates_mut()
        .tuple_new(vec![Value::Agg(good_inner)])
        .unwrap();
    vm.host_mut().0 = Value::Agg(good_outer);
    assert!(
        vm.run_with_ctx(
            &mut ctx,
            &prepared,
            entry,
            &[],
            &mut outputs,
            TraceMask::NONE,
            None,
            None
        )
        .is_ok()
    );
}

#[test]
fn deep_shared_graph_check_is_bounded_and_nonrecursive() {
    let mut builder = ProgramBuilder::new();
    let mut contract = integer(&mut builder);
    for _ in 0..2048 {
        contract = tuple(&mut builder, &[Some(contract), Some(contract)]);
    }
    let mut main = body(&[ValueType::Unit, ValueType::Agg]);
    main.ret(0, &[]);
    let entry = builder
        .push_function_checked(
            main,
            FunctionAbi {
                args: vec![contract.into()],
                rets: vec![],
            },
        )
        .unwrap();
    let prepared = builder.build_verified().unwrap();
    let mut vm = Vm::new(TestHost(Value::Unit), Limits::default());
    let mut value = Value::I64(42);
    for _ in 0..2048 {
        value = Value::Agg(
            vm.aggregates_mut()
                .tuple_new(vec![value.clone(), value])
                .unwrap(),
        );
    }
    assert_eq!(
        vm.run(&prepared, entry, &[value.clone()], TraceMask::NONE, None)
            .unwrap(),
        Vec::<Value>::new()
    );
    let heap = vm.aggregates().clone();
    let mut bounded = Vm::new(
        TestHost(Value::Unit),
        Limits {
            max_contract_work: 10,
            ..Limits::default()
        },
    );
    *bounded.aggregates_mut() = heap;
    let error = bounded
        .run(&prepared, entry, &[value], TraceMask::NONE, None)
        .unwrap_err();
    assert!(matches!(error.trap, Trap::ContractWorkExceeded { .. }));
}

#[test]
fn absent_signature_ids_are_rejected_before_any_kind_is_interned() {
    use execution_tape::asm::{AsmError, ContractRef};
    use execution_tape::contract::ContractBuildError;
    for position in 0..3 {
        let mut builder = ProgramBuilder::new();
        let (args, rets) = match position {
            0 => (vec![], vec![ContractRef::Id(ContractId(0))]),
            1 => (vec![ContractRef::Id(ContractId(0))], vec![]),
            _ => (
                vec![ValueType::Unit.into()],
                vec![ContractRef::Id(ContractId(0))],
            ),
        };
        let mut asm = body(&[ValueType::Unit]);
        asm.ret(0, &[]);
        assert!(matches!(
            builder.push_function_checked(asm, FunctionAbi { args, rets }),
            Err(AsmError::Contract(ContractBuildError::UnknownChild(
                ContractId(0)
            )))
        ));
        assert_eq!(
            builder.contract(Contract::Kind(ValueType::I64)).unwrap(),
            ContractId(0)
        );
        let mut asm = body(&[ValueType::Unit]);
        asm.ret(0, &[]);
        assert_eq!(
            builder
                .push_function_checked(
                    asm,
                    FunctionSig {
                        arg_types: vec![],
                        ret_types: vec![]
                    }
                )
                .unwrap()
                .0,
            0
        );
        builder.build_verified().unwrap();
    }
}

#[test]
fn branches_and_aliased_selects_keep_common_nested_structure() {
    for select in [false, true] {
        let mut builder = ProgramBuilder::new();
        let mut asm = body(&[
            ValueType::Unit,
            ValueType::Bool,
            ValueType::I64,
            ValueType::Agg,
            ValueType::Agg,
            ValueType::Agg,
            ValueType::Agg,
            ValueType::Agg,
            ValueType::U64,
        ]);
        asm.const_i64(2, 42)
            .tuple_new(3, &[2])
            .tuple_new(4, &[1])
            .tuple_new(5, &[3])
            .tuple_new(6, &[4]);
        if select {
            asm.select(5, 1, 5, 6).tuple_get(7, 5, 0);
        } else {
            let join = asm.block(&[7]);
            asm.br(1, join.with_args(&[5]), join.with_args(&[6]));
            asm.place(join).unwrap();
            asm.tuple_get(7, 7, 0);
        }
        asm.tuple_len(8, 7).ret(0, &[8]);
        let entry = builder
            .push_function_checked(
                asm,
                FunctionSig {
                    arg_types: vec![ValueType::Bool],
                    ret_types: vec![ValueType::U64],
                },
            )
            .unwrap();
        let prepared = builder.build_verified().unwrap();
        let mut vm = Vm::new(TestHost(Value::Unit), Limits::default());
        for flag in [true, false] {
            assert_eq!(
                vm.run(
                    &prepared,
                    entry,
                    &[Value::Bool(flag)],
                    TraceMask::NONE,
                    None
                )
                .unwrap(),
                vec![Value::U64(1)]
            );
        }
    }
}

#[test]
fn one_handle_must_satisfy_each_distinct_contract() {
    let mut builder = ProgramBuilder::new();
    let integer = integer(&mut builder);
    let boolean = builder.contract(Contract::Kind(ValueType::Bool)).unwrap();
    let ints = tuple(&mut builder, &[Some(integer)]);
    let bools = tuple(&mut builder, &[Some(boolean)]);
    let mut asm = body(&[ValueType::Unit, ValueType::Agg, ValueType::Agg]);
    asm.ret(0, &[]);
    let entry = builder
        .push_function_checked(
            asm,
            FunctionAbi {
                args: vec![ints.into(), bools.into()],
                rets: vec![],
            },
        )
        .unwrap();
    let program = builder.build_verified().unwrap();
    let mut vm = Vm::new(TestHost(Value::Unit), Limits::default());
    let shared = vm.aggregates_mut().tuple_new(vec![Value::I64(42)]).unwrap();
    let error = vm
        .run(
            &program,
            entry,
            &[Value::Agg(shared), Value::Agg(shared)],
            TraceMask::NONE,
            None,
        )
        .unwrap_err();
    assert!(matches!(
        error.trap,
        Trap::ContractMismatch {
            position: ContractPosition::Argument { index: 1, .. },
            ..
        }
    ));
}

#[test]
fn successful_proofs_do_not_survive_reset_and_numeric_handle_reuse() {
    let mut builder = ProgramBuilder::new();
    let integer = integer(&mut builder);
    let tuple = tuple(&mut builder, &[Some(integer)]);
    let mut asm = body(&[ValueType::Unit, ValueType::Agg]);
    asm.ret(0, &[]);
    let entry = builder
        .push_function_checked(
            asm,
            FunctionAbi {
                args: vec![tuple.into()],
                rets: vec![],
            },
        )
        .unwrap();
    let program = builder.build_verified().unwrap();
    let mut vm = Vm::new(TestHost(Value::Unit), Limits::default());
    let mut ctx = ExecutionContext::new();
    let mut outputs = vec![];
    let good = vm.aggregates_mut().tuple_new(vec![Value::I64(42)]).unwrap();
    vm.run_with_ctx(
        &mut ctx,
        &program,
        entry,
        &[Value::Agg(good)],
        &mut outputs,
        TraceMask::NONE,
        None,
        None,
    )
    .unwrap();
    vm.aggregates_mut().clear();
    let bad = vm
        .aggregates_mut()
        .tuple_new(vec![Value::Bool(false)])
        .unwrap();
    assert_eq!(good, bad);
    let error = vm
        .run_with_ctx(
            &mut ctx,
            &program,
            entry,
            &[Value::Agg(bad)],
            &mut outputs,
            TraceMask::NONE,
            None,
            None,
        )
        .unwrap_err();
    assert!(matches!(error.trap, Trap::ContractMismatch { .. }));
}

#[test]
fn even_unused_nominal_refinements_must_match_the_schema() {
    use execution_tape::contract::ContractDefinitionError;
    use execution_tape::program::{ElemTypeId, StructTypeDef, TypeId};
    for mode in 0..4 {
        let mut builder = ProgramBuilder::new();
        let integer = integer(&mut builder);
        let boolean = builder.contract(Contract::Kind(ValueType::Bool)).unwrap();
        let schema = builder.struct_type(StructTypeDef {
            field_names: vec!["value".into()],
            field_types: vec![ValueType::I64],
        });
        let array = builder.array_elem(ValueType::I64);
        let contract = match mode {
            0 => Contract::Struct {
                type_id: TypeId(99),
                fields: &[integer],
            },
            1 => Contract::Struct {
                type_id: schema,
                fields: &[],
            },
            2 => Contract::Struct {
                type_id: schema,
                fields: &[boolean],
            },
            _ => Contract::Array {
                elem_type_id: ElemTypeId(array.0),
                element: boolean,
            },
        };
        let id = builder.contract(contract).unwrap();
        let error = builder.build_verified().unwrap_err();
        assert!(
            matches!(error,execution_tape::asm::BuildError::Verify(VerifyError::InvalidContract{contract,error:ContractDefinitionError::UnknownStruct(_)|ContractDefinitionError::FieldCount{..}|ContractDefinitionError::MemberKind{..}}) if contract==id)
        );
    }
}

#[test]
fn rejected_root_candidate_is_retained_and_checked_during_replay() {
    use execution_tape::recording::{RecordLimits, Recorder, ReplayOutcome};
    use execution_tape::trace::TraceSink;
    use std::sync::Arc;
    let mut builder = ProgramBuilder::new();
    let integer = integer(&mut builder);
    let inner = tuple(&mut builder, &[Some(integer)]);
    let outer = tuple(&mut builder, &[Some(inner)]);
    let import = builder.host_sig_for(
        "opaque",
        HostSig {
            args: vec![],
            rets: vec![ValueType::Agg],
        },
    );
    let mut asm = body(&[ValueType::Unit, ValueType::Agg, ValueType::Agg]);
    asm.host_call(0, import, 0, &[], &[1])
        .tuple_new(2, &[1])
        .ret(0, &[2]);
    let entry = builder
        .push_function_checked(
            asm,
            FunctionAbi {
                args: vec![],
                rets: vec![outer.into()],
            },
        )
        .unwrap();
    let program = Arc::new(builder.build_verified().unwrap());
    let mut vm = Vm::new(TestHost(Value::Unit), Limits::default());
    let bad = vm
        .aggregates_mut()
        .tuple_new(vec![Value::Bool(false)])
        .unwrap();
    vm.host_mut().0 = Value::Agg(bad);
    let mut recorder = Recorder::new(program.clone(), RecordLimits::default());
    let error = vm
        .run(&program, entry, &[], recorder.mask(), Some(&mut recorder))
        .unwrap_err();
    assert!(matches!(error.trap, Trap::ContractMismatch { .. }));
    let record = recorder.finish();
    record.status().unwrap();
    // The wrapper was created after the host boundary and exists only in the rejected candidate.
    assert_eq!(record.stats().aggregates, 2);
    assert_eq!(record.stats().values, 4);
    drop(vm);
    assert!(matches!(
        record.replay().unwrap(),
        ReplayOutcome::Trapped(_)
    ));
}

#[test]
fn opaque_results_fail_at_the_callee_return_for_all_call_forms() {
    use execution_tape::recording::{RecordLimits, Recorder, ReplayOutcome};
    use execution_tape::trace::{ScopeKind, TraceLocation, TraceOutcome, TraceSink};
    use std::sync::Arc;
    struct Completions {
        call: TraceLocation,
        success: bool,
        frames: Vec<bool>,
    }
    impl TraceSink for Completions {
        fn instr_end(
            &mut self,
            _: &Program,
            location: TraceLocation,
            _: Option<TraceLocation>,
            outcome: TraceOutcome<'_>,
        ) {
            if location == self.call && matches!(outcome, TraceOutcome::Ok) {
                self.success = true;
            }
        }
        fn scope_exit(
            &mut self,
            _: &Program,
            kind: ScopeKind,
            _: usize,
            _: execution_tape::value::FuncId,
            _: u32,
            _: Option<u64>,
            outcome: TraceOutcome<'_>,
        ) {
            if matches!(kind, ScopeKind::CallFrame { .. }) {
                self.frames.push(matches!(outcome, TraceOutcome::Ok));
            }
        }
    }
    for mode in 0..3 {
        let mut builder = ProgramBuilder::new();
        let integer = integer(&mut builder);
        let required = tuple(&mut builder, &[Some(integer)]);
        let import = builder.host_sig_for(
            "opaque",
            HostSig {
                args: vec![],
                rets: vec![ValueType::Agg],
            },
        );
        let mut producer = body(if mode == 2 {
            &[ValueType::Unit, ValueType::Agg, ValueType::Agg]
        } else {
            &[ValueType::Unit, ValueType::Agg]
        });
        let result = if mode == 2 { 2 } else { 1 };
        producer
            .host_call(0, import, 0, &[], &[result])
            .ret(0, &[result]);
        let target = builder
            .push_function_checked(
                producer,
                FunctionAbi {
                    args: if mode == 2 {
                        vec![ValueType::Agg.into()]
                    } else {
                        vec![]
                    },
                    rets: vec![required.into()],
                },
            )
            .unwrap();
        let mut main = body(&[
            ValueType::Unit,
            ValueType::Func,
            ValueType::Agg,
            ValueType::Closure,
            ValueType::Agg,
        ]);
        main.const_func(1, target);
        if mode == 2 {
            main.tuple_new(2, &[]).closure_new(3, 1, 2);
        }
        let call_pc = main.pc();
        if mode == 0 {
            main.call(0, target, 0, &[], &[4]);
        } else {
            let sig = builder
                .call_sig(FunctionAbi {
                    args: vec![],
                    rets: vec![required.into()],
                })
                .unwrap();
            main.call_indirect(0, sig, if mode == 2 { 3 } else { 1 }, 0, &[], &[4]);
        }
        main.ret(0, &[4]);
        let entry = builder
            .push_function_checked(
                main,
                FunctionSig {
                    arg_types: vec![],
                    ret_types: vec![ValueType::Agg],
                },
            )
            .unwrap();
        let program = Arc::new(builder.build_verified().unwrap());
        let return_pc = program
            .function(target)
            .unwrap()
            .instructions()
            .last()
            .unwrap()
            .pc;
        let mut vm = Vm::new(TestHost(Value::Unit), Limits::default());
        let bad = vm
            .aggregates_mut()
            .tuple_new(vec![Value::Bool(false)])
            .unwrap();
        vm.host_mut().0 = Value::Agg(bad);
        let mut recorder = Recorder::new(program.clone(), RecordLimits::default());
        let error = vm
            .run(&program, entry, &[], recorder.mask(), Some(&mut recorder))
            .unwrap_err();
        assert_eq!((error.func, error.pc), (target, return_pc));
        let record = recorder.finish();
        record.status().unwrap();
        assert_eq!(
            record.outcome_stack().unwrap().collect::<Vec<_>>(),
            vec![
                TraceLocation {
                    func: target,
                    pc: return_pc
                },
                TraceLocation {
                    func: entry,
                    pc: call_pc
                }
            ]
        );
        assert!(
            matches!(record.replay().unwrap(),ReplayOutcome::Trapped(error) if error.func==target && error.pc==return_pc)
        );
        let mut observer = Completions {
            call: TraceLocation {
                func: entry,
                pc: call_pc,
            },
            success: false,
            frames: vec![],
        };
        vm.run(
            &program,
            entry,
            &[],
            TraceMask::INSTR | TraceMask::CALL,
            Some(&mut observer),
        )
        .unwrap_err();
        // A call instruction completes its dispatch into the callee. Invocation completion is
        // reported by the frame outcomes; neither frame may succeed after the callee return fails.
        assert!(observer.success);
        assert_eq!(observer.frames, vec![false, false]);
    }
}

#[test]
fn contract_work_limit_has_exact_accounting_and_boundary_context() {
    let mut builder = ProgramBuilder::new();
    let integer = integer(&mut builder);
    let required = tuple(&mut builder, &[Some(integer)]);
    let mut asm = body(&[ValueType::Unit, ValueType::Agg, ValueType::Agg]);
    asm.ret(0, &[]);
    let entry = builder
        .push_function_checked(
            asm,
            FunctionAbi {
                args: vec![required.into(), required.into()],
                rets: vec![],
            },
        )
        .unwrap();
    let program = builder.build_verified().unwrap();
    // First root: pair + child edge + scalar value = 3. Second cached pair costs one visit.
    for limit in [0, 2, 3, 4] {
        let mut vm = Vm::new(
            TestHost(Value::Unit),
            Limits {
                max_contract_work: limit,
                ..Limits::default()
            },
        );
        let value = vm.aggregates_mut().tuple_new(vec![Value::I64(42)]).unwrap();
        let result = vm.run(
            &program,
            entry,
            &[Value::Agg(value), Value::Agg(value)],
            TraceMask::NONE,
            None,
        );
        if limit == 4 {
            result.unwrap();
        } else {
            assert!(
                matches!(result.unwrap_err().trap,Trap::ContractWorkExceeded{function,position:ContractPosition::Argument{index,..},limit:reported} if function==entry && reported==limit && index==u32::from(limit==3))
            );
        }
    }
}

fn replace_wire_section(bytes: &[u8], tag: u8, replacement: &[u8]) -> Vec<u8> {
    use execution_tape::format::{Reader, Writer};
    let mut r = Reader::new(&bytes[12..]);
    let mut w = Writer::new();
    w.write_bytes(&bytes[..12]);
    while r.offset() != bytes.len() - 12 {
        let next = r.read_u8().unwrap();
        let len = usize::try_from(r.read_uleb128_u64().unwrap()).unwrap();
        let payload = r.read_bytes(len).unwrap();
        let payload = if next == tag { replacement } else { payload };
        w.write_u8(next);
        w.write_uleb128_u64(payload.len() as u64);
        w.write_bytes(payload);
    }
    w.into_vec()
}

#[test]
fn wire_contracts_normalize_duplicates_and_reject_invalid_topology() {
    let mut builder = ProgramBuilder::new();
    let integer = integer(&mut builder);
    let required = tuple(&mut builder, &[Some(integer)]);
    let mut asm = body(&[ValueType::Unit, ValueType::Agg]);
    asm.ret(0, &[]);
    builder
        .push_function_checked(
            asm,
            FunctionAbi {
                args: vec![required.into()],
                rets: vec![],
            },
        )
        .unwrap();
    let source = builder.build_checked().unwrap();
    let bytes = source.encode();
    // Two equivalent I64 nodes followed by tuple[second I64]. Signature references raw node 2.
    let duplicate = replace_wire_section(&bytes, 12, &[3, 0, 2, 0, 2, 1, 1, 1, 1]);
    let duplicate = replace_wire_section(&duplicate, 7, &[1, 1, 2, 0, 0, 0]);
    let decoded = Program::decode(&duplicate).unwrap();
    assert_eq!(decoded, source);
    assert_eq!(decoded.encode(), bytes);
    for bad in [
        vec![1, 1, 1, 1, 0],
        vec![1, 1, 1, 1, 1],
        vec![1, 3, 0, 0],
        vec![1, 9],
    ] {
        let bad = replace_wire_section(&bytes, 12, &bad);
        assert!(Program::decode(&bad).is_err());
    }
    for prefix in 0..bytes.len() {
        assert!(
            Program::decode(&bytes[..prefix]).is_err(),
            "prefix {prefix}"
        );
    }
}

#[test]
fn nested_struct_and_array_results_propagate_without_retaining_temporaries() {
    use execution_tape::program::StructTypeDef;
    let mut b = ProgramBuilder::new();
    let integer = integer(&mut b);
    let leaf = tuple(&mut b, &[Some(integer)]);
    let struct_id = b.struct_type(StructTypeDef {
        field_names: vec!["value".into()],
        field_types: vec![ValueType::Agg],
    });
    let elem_id = b.array_elem(ValueType::Agg);
    let structure = b
        .contract(Contract::Struct {
            type_id: struct_id,
            fields: &[leaf],
        })
        .unwrap();
    let array = b
        .contract(Contract::Array {
            elem_type_id: elem_id,
            element: structure,
        })
        .unwrap();
    let mut make = body(&[
        ValueType::Unit,
        ValueType::I64,
        ValueType::Agg,
        ValueType::Agg,
        ValueType::Agg,
    ]);
    make.const_i64(1, 42)
        .tuple_new(2, &[1])
        .struct_new(3, struct_id, &[2])
        .array_new(4, elem_id, &[3])
        .ret(0, &[4]);
    let make = b
        .push_function_checked(
            make,
            FunctionAbi {
                args: vec![],
                rets: vec![array.into()],
            },
        )
        .unwrap();
    let mut main = body(&[ValueType::Unit, ValueType::Agg, ValueType::I64]);
    main.call(0, make, 0, &[], &[1])
        .array_get_imm(1, 1, 0)
        .struct_get(1, 1, 0)
        .tuple_get(2, 1, 0)
        .ret(0, &[2]);
    let entry = b
        .push_function_checked(
            main,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
            },
        )
        .unwrap();
    let source = b.build_checked().unwrap();
    let program = verify_program_owned(
        Program::decode(&source.encode()).unwrap(),
        &VerifyConfig::default(),
    )
    .unwrap();
    let mut vm = Vm::new(TestHost(Value::Unit), Limits::default());
    assert_eq!(
        vm.run(&program, entry, &[], TraceMask::NONE, None).unwrap(),
        vec![Value::I64(42)]
    );
    assert_eq!(vm.aggregates().len_u32(), 0);
}

#[test]
fn recursive_functions_use_declared_finite_result_contracts() {
    let mut b = ProgramBuilder::new();
    let integer = integer(&mut b);
    let result = tuple(&mut b, &[Some(integer)]);
    let entry = b
        .declare_function(FunctionAbi {
            args: vec![ValueType::U64.into()],
            rets: vec![result.into()],
        })
        .unwrap();
    let mut asm = body(&[
        ValueType::Unit,
        ValueType::U64,
        ValueType::U64,
        ValueType::Bool,
        ValueType::U64,
        ValueType::Agg,
        ValueType::I64,
    ]);
    let base = asm.label();
    let recursive = asm.label();
    asm.const_u64(2, 0).u64_eq(3, 1, 2).br(3, base, recursive);
    asm.place(base).unwrap();
    asm.const_i64(6, 42).tuple_new(5, &[6]).ret(0, &[5]);
    asm.place(recursive).unwrap();
    asm.const_u64(2, 1)
        .u64_sub(4, 1, 2)
        .call(0, entry, 0, &[4], &[5])
        .ret(0, &[5]);
    b.define_function(entry, asm).unwrap();
    let program = b.build_verified().unwrap();
    let mut vm = Vm::new(TestHost(Value::Unit), Limits::default());
    let out = vm
        .run(&program, entry, &[Value::U64(4)], TraceMask::NONE, None)
        .unwrap();
    let [Value::Agg(root)] = out[..] else {
        panic!("expected tuple");
    };
    assert_eq!(vm.aggregates().tuple_get(root, 0).unwrap(), Value::I64(42));
}

#[test]
fn nominal_identity_is_not_layout_equality_and_empty_arrays_are_vacuous() {
    use execution_tape::program::{ElemTypeId, StructTypeDef, TypeId};
    let mut b = ProgramBuilder::new();
    let integer = integer(&mut b);
    let leaf = tuple(&mut b, &[Some(integer)]);
    let schema = b.struct_type(StructTypeDef {
        field_names: vec!["value".into()],
        field_types: vec![ValueType::Agg],
    });
    let elem = b.array_elem(ValueType::Agg);
    let structure = b
        .contract(Contract::Struct {
            type_id: schema,
            fields: &[leaf],
        })
        .unwrap();
    let array = b
        .contract(Contract::Array {
            elem_type_id: elem,
            element: structure,
        })
        .unwrap();
    let mut asm = body(&[ValueType::Unit, ValueType::Agg]);
    asm.ret(0, &[]);
    let check_struct = b
        .push_function_checked(
            asm.clone(),
            FunctionAbi {
                args: vec![structure.into()],
                rets: vec![],
            },
        )
        .unwrap();
    let check_array = b
        .push_function_checked(
            asm,
            FunctionAbi {
                args: vec![array.into()],
                rets: vec![],
            },
        )
        .unwrap();
    let mut source = b.build();
    source.types.structs.push(source.types.structs[0].clone());
    source.types.array_elems.push(ValueType::Agg);
    let program = verify_program_owned(source, &VerifyConfig::default()).unwrap();
    let mut vm = Vm::new(TestHost(Value::Unit), Limits::default());
    let empty = vm.aggregates_mut().array_new(elem, vec![]).unwrap();
    vm.run(
        &program,
        check_array,
        &[Value::Agg(empty)],
        TraceMask::NONE,
        None,
    )
    .unwrap();
    let child = vm.aggregates_mut().tuple_new(vec![Value::I64(42)]).unwrap();
    let wrong = vm
        .aggregates_mut()
        .struct_new(TypeId(1), vec![Value::Agg(child)])
        .unwrap();
    let error = vm
        .run(
            &program,
            check_struct,
            &[Value::Agg(wrong)],
            TraceMask::NONE,
            None,
        )
        .unwrap_err();
    assert!(matches!(
        error.trap,
        Trap::ContractMismatch {
            error: execution_tape::contract::ContractViolation {
                mismatch: ContractMismatch::Aggregate { .. },
                ..
            },
            ..
        }
    ));
    let wrong = vm
        .aggregates_mut()
        .array_new(ElemTypeId(1), vec![])
        .unwrap();
    assert!(matches!(
        vm.run(
            &program,
            check_array,
            &[Value::Agg(wrong)],
            TraceMask::NONE,
            None
        )
        .unwrap_err()
        .trap,
        Trap::ContractMismatch { .. }
    ));
}

#[test]
fn opaque_calls_erase_previous_projection_facts() {
    for mode in 0..3 {
        let mut b = ProgramBuilder::new();
        let mut producer = body(&[ValueType::Unit, ValueType::Agg]);
        producer.tuple_new(1, &[]).ret(0, &[1]);
        let target = b
            .push_function_checked(
                producer,
                FunctionSig {
                    arg_types: vec![],
                    ret_types: vec![ValueType::Agg],
                },
            )
            .unwrap();
        let host = b.host_sig_for(
            "opaque",
            HostSig {
                args: vec![],
                rets: vec![ValueType::Agg],
            },
        );
        let call = b
            .call_sig(FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::Agg],
            })
            .unwrap();
        let mut main = body(&[
            ValueType::Unit,
            ValueType::I64,
            ValueType::Agg,
            ValueType::Func,
        ]);
        main.const_i64(1, 42).tuple_new(2, &[1]);
        match mode {
            0 => {
                main.call(0, target, 0, &[], &[2]);
            }
            1 => {
                main.host_call(0, host, 0, &[], &[2]);
            }
            _ => {
                main.const_func(3, target)
                    .call_indirect(0, call, 3, 0, &[], &[2]);
            }
        }
        let projection = main.pc();
        main.tuple_get(1, 2, 0).ret(0, &[1]);
        let entry = b
            .push_function_checked(
                main,
                FunctionSig {
                    arg_types: vec![],
                    ret_types: vec![ValueType::I64],
                },
            )
            .unwrap();
        assert!(
            matches!(b.build_verified(),Err(execution_tape::asm::BuildError::Verify(VerifyError::UnknownAggregateShape{func,pc,reg:2})) if func==entry.0&&pc==projection)
        );
    }
}

#[test]
fn aliased_opaque_member_does_not_inherit_its_containers_shape() {
    let mut b = ProgramBuilder::new();
    let opaque = b.contract(Contract::Kind(ValueType::Agg)).unwrap();
    let outer = tuple(&mut b, &[Some(opaque)]);
    let mut main = body(&[ValueType::Unit, ValueType::Agg, ValueType::I64]);
    main.tuple_get(1, 1, 0);
    let projection = main.pc();
    main.tuple_get(2, 1, 0).ret(0, &[2]);
    let entry = b
        .push_function_checked(
            main,
            FunctionAbi {
                args: vec![outer.into()],
                rets: vec![ValueType::I64.into()],
            },
        )
        .unwrap();
    assert!(matches!(
        b.build_verified(),
        Err(execution_tape::asm::BuildError::Verify(
            VerifyError::UnknownAggregateShape { func, pc, reg: 1 }
        )) if func == entry.0 && pc == projection
    ));
}
