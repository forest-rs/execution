// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Public consumers of optional verifier facts. No interpreter state is fabricated.

use execution_tape::asm::{Asm, FunctionAbi, FunctionSig, ProgramBuilder};
use execution_tape::contract::{Contract, ContractId, ContractTable};
use execution_tape::program::{Program, ValueType};
use execution_tape::value::FuncId;
use execution_tape::verifier::{FactOrigin, ProgramAnalysis, ValueFact, VerifyConfig};

fn asm(types: &[ValueType]) -> Asm {
    let mut a = Asm::new();
    a.set_register_types(types.to_vec());
    a
}
fn signature(args: &[ValueType], rets: &[ValueType]) -> FunctionSig {
    FunctionSig {
        arg_types: args.to_vec(),
        ret_types: rets.to_vec(),
    }
}
fn id(fact: ValueFact) -> ContractId {
    match fact {
        ValueFact::Contract(id) => id,
        _ => panic!("expected an initialized contract: {fact:?}"),
    }
}
fn member(table: &ContractTable, fact: ValueFact, index: usize) -> ValueFact {
    let Some(Contract::Tuple(members)) = table.get(id(fact)) else {
        panic!("expected tuple");
    };
    ValueFact::Contract(members[index].expect("known tuple member"))
}
fn kind(table: &ContractTable, fact: ValueFact, expected: ValueType) {
    assert_eq!(
        table.get(id(fact)),
        Some(Contract::Kind(expected)),
        "expected exact scalar contract for {fact:?}"
    );
}

fn nested() -> (Program, FuncId, Vec<u32>) {
    let mut b = ProgramBuilder::new();
    let integer = b.contract(Contract::Kind(ValueType::I64)).unwrap();
    let boolean = b.contract(Contract::Kind(ValueType::Bool)).unwrap();
    let inner = b.contract(Contract::Tuple(&[Some(integer)])).unwrap();
    let outer = b
        .contract(Contract::Tuple(&[Some(inner), Some(boolean)]))
        .unwrap();
    let mut make = asm(&[
        ValueType::Unit,
        ValueType::I64,
        ValueType::Bool,
        ValueType::Agg,
        ValueType::Agg,
    ]);
    make.const_i64(1, 42)
        .const_bool(2, true)
        .tuple_new(3, &[1])
        .tuple_new(4, &[3, 2])
        .ret(0, &[4]);
    let make = b
        .push_function_checked(
            make,
            FunctionAbi {
                args: vec![],
                rets: vec![outer.into()],
            },
        )
        .unwrap();
    let mut main = asm(&[ValueType::Unit, ValueType::Agg, ValueType::I64]);
    let mut pcs = vec![main.pc()];
    main.call(0, make, 0, &[], &[1]);
    pcs.push(main.pc());
    main.tuple_get(1, 1, 0);
    pcs.push(main.pc());
    main.tuple_get(2, 1, 0);
    pcs.push(main.pc());
    main.ret(0, &[2]);
    let entry = b
        .push_function_checked(main, signature(&[], &[ValueType::I64]))
        .unwrap();
    (b.build_checked().unwrap(), entry, pcs)
}

#[test]
fn nested_calls_and_aliased_projections_have_exact_before_and_after_facts() {
    let (program, entry, pcs) = nested();
    let source = execution_tape::disasm::disassemble_function(&program, entry).unwrap();
    assert_eq!(source.instruction(pcs[1]).unwrap().pc(), pcs[1]);
    assert!(source.instruction(pcs[0] + 1).is_none());
    let proof = ProgramAnalysis::new(&program, &VerifyConfig::default()).unwrap();
    assert!(proof.function(FuncId(u32::MAX)).is_none());
    let mut function = proof.function(entry).unwrap();
    {
        let call = function.instruction(pcs[0]).unwrap();
        assert!(call.reachable());
        assert_eq!(call.function(), entry);
        let r1 = call.register(1).unwrap();
        assert_eq!(r1.storage_kind, ValueType::Agg);
        assert_eq!(r1.before, ValueFact::Uninitialized);
        assert_eq!(r1.origin_before, None);
        assert_eq!(
            r1.origin_after,
            Some(FactOrigin::Instruction { pc: pcs[0] })
        );
        kind(
            call.contracts(),
            member(call.contracts(), member(call.contracts(), r1.after, 0), 0),
            ValueType::I64,
        );
        kind(
            call.contracts(),
            member(call.contracts(), r1.after, 1),
            ValueType::Bool,
        );
        assert!(call.register(999).is_none());
    }
    {
        let project = function.instruction(pcs[1]).unwrap();
        let r1 = project.register(1).unwrap();
        assert_eq!(
            r1.origin_before,
            Some(FactOrigin::Instruction { pc: pcs[0] })
        );
        assert_eq!(
            r1.origin_after,
            Some(FactOrigin::Instruction { pc: pcs[1] })
        );
        assert_eq!(member(project.contracts(), r1.before, 0), r1.after);
        kind(
            project.contracts(),
            member(project.contracts(), r1.after, 0),
            ValueType::I64,
        );
    }
    {
        let project = function.instruction(pcs[2]).unwrap();
        let r2 = project.register(2).unwrap();
        assert_eq!(r2.before, ValueFact::Uninitialized);
        kind(project.contracts(), r2.after, ValueType::I64);
    }
    // Query backwards, repeat a location, and then move forwards; no stale cursor facts survive.
    for &pc in [pcs[3], pcs[0], pcs[0], pcs[1]].iter() {
        let view = function.instruction(pc).unwrap();
        if pc == pcs[0] {
            assert_eq!(view.register(1).unwrap().before, ValueFact::Uninitialized);
        }
    }
    assert!(function.instruction(pcs[0] + 1).is_none());
    assert!(function.instruction(u32::MAX).is_none());
}

#[test]
fn prepared_programs_lend_their_proof_without_cloning_source() {
    let (program, entry, pcs) = nested();
    let prepared =
        execution_tape::verifier::verify_program_owned(program, &VerifyConfig::default()).unwrap();
    let mut analysis = ProgramAnalysis::from(&prepared).function(entry).unwrap();
    let report = analysis.instruction(pcs[1]).unwrap();
    let before = report.register(1).unwrap().before;
    kind(
        report.contracts(),
        member(report.contracts(), member(report.contracts(), before, 0), 0),
        ValueType::I64,
    );
}

#[test]
fn unreachable_and_uninitialized_are_distinct_from_declared_kinds() {
    let mut b = ProgramBuilder::new();
    let mut a = asm(&[ValueType::Unit, ValueType::I64]);
    a.ret(0, &[]);
    let dead = a.pc();
    a.const_i64(1, 1).ret(0, &[]);
    let entry = b.push_function_checked(a, signature(&[], &[])).unwrap();
    let p = b.build_checked().unwrap();
    let mut f = ProgramAnalysis::new(&p, &VerifyConfig::default())
        .unwrap()
        .function(entry)
        .unwrap();
    {
        let live = f.instruction(0).unwrap();
        assert_eq!(live.register(1).unwrap().before, ValueFact::Uninitialized);
        assert_eq!(
            live.register(0).unwrap().origin_before,
            Some(FactOrigin::Effect)
        );
    }
    let dead = f.instruction(dead).unwrap();
    assert!(!dead.reachable());
    let reg = dead.register(1).unwrap();
    assert_eq!(reg.storage_kind, ValueType::I64);
    assert_eq!(reg.before, ValueFact::Unreachable);
    assert_eq!(reg.after, ValueFact::Unreachable);
    assert_eq!(reg.origin_before, None);
    assert_eq!(reg.origin_after, None);
}

#[test]
fn arguments_start_initialized_but_unused_registers_do_not() {
    let mut b = ProgramBuilder::new();
    let integer = b.contract(Contract::Kind(ValueType::I64)).unwrap();
    let tuple = b.contract(Contract::Tuple(&[Some(integer)])).unwrap();
    let mut a = asm(&[ValueType::Unit, ValueType::Agg, ValueType::Agg]);
    a.ret(0, &[]);
    let entry = b
        .push_function_checked(
            a,
            FunctionAbi {
                args: vec![tuple.into()],
                rets: vec![],
            },
        )
        .unwrap();
    let p = b.build_checked().unwrap();
    let mut f = ProgramAnalysis::new(&p, &VerifyConfig::default())
        .unwrap()
        .function(entry)
        .unwrap();
    let view = f.instruction(0).unwrap();
    let arg = view.register(1).unwrap();
    assert_eq!(arg.origin_before, Some(FactOrigin::Argument { index: 0 }));
    kind(
        view.contracts(),
        member(view.contracts(), arg.before, 0),
        ValueType::I64,
    );
    assert_eq!(view.register(2).unwrap().before, ValueFact::Uninitialized);
}

#[test]
fn opaque_call_overwrites_clear_prior_structure_without_uninitializing() {
    let mut b = ProgramBuilder::new();
    let mut make = asm(&[ValueType::Unit, ValueType::Agg]);
    make.tuple_new(1, &[]).ret(0, &[1]);
    let make = b
        .push_function_checked(make, signature(&[], &[ValueType::Agg]))
        .unwrap();
    let mut a = asm(&[ValueType::Unit, ValueType::I64, ValueType::Agg]);
    a.const_i64(1, 7).tuple_new(2, &[1]);
    let call = a.pc();
    a.call(0, make, 0, &[], &[2]).ret(0, &[]);
    let entry = b.push_function_checked(a, signature(&[], &[])).unwrap();
    let p = b.build_checked().unwrap();
    let mut f = ProgramAnalysis::new(&p, &VerifyConfig::default())
        .unwrap()
        .function(entry)
        .unwrap();
    let view = f.instruction(call).unwrap();
    let reg = view.register(2).unwrap();
    kind(
        view.contracts(),
        member(view.contracts(), reg.before, 0),
        ValueType::I64,
    );
    kind(view.contracts(), reg.after, ValueType::Agg);
}

#[test]
fn same_target_typed_edges_keep_their_simultaneous_bindings_and_common_join_fact() {
    let mut b = ProgramBuilder::new();
    let mut a = asm(&[
        ValueType::Unit,
        ValueType::Bool,
        ValueType::I64,
        ValueType::Bool,
        ValueType::Agg,
        ValueType::Agg,
        ValueType::Agg,
        ValueType::Agg,
    ]);
    let join = a.block(&[6, 7]);
    a.const_i64(2, 42)
        .const_bool(3, true)
        .tuple_new(4, &[2])
        .tuple_new(5, &[3]);
    let branch = a.pc();
    a.br(1, join.with_args(&[4, 5]), join.with_args(&[5, 4]));
    let header = a.pc();
    a.place(join).unwrap();
    a.ret(0, &[]);
    let entry = b
        .push_function_checked(a, signature(&[ValueType::Bool], &[]))
        .unwrap();
    let p = b.build_checked().unwrap();
    let mut f = ProgramAnalysis::new(&p, &VerifyConfig::default())
        .unwrap()
        .function(entry)
        .unwrap();
    {
        let branch = f.instruction(branch).unwrap();
        assert_eq!(branch.register(6).unwrap().after, ValueFact::Uninitialized);
        assert_eq!(branch.register(7).unwrap().after, ValueFact::Uninitialized);
    }
    let view = f.instruction(header).unwrap();
    assert!(view.is_block_entry());
    // A header admits edge parameters; it does not perform a second assignment.
    let merged = view.register(6).unwrap();
    assert_eq!(merged.before, merged.after);
    assert_eq!(
        merged.origin_before,
        Some(FactOrigin::BlockEntry { pc: header })
    );
    assert_eq!(
        view.contracts().get(id(merged.before)),
        Some(Contract::Tuple(&[None]))
    );
    let edges: Vec<_> = view.incoming_edges().collect();
    assert_eq!(edges.len(), 2);
    for (arm, edge) in edges.iter().enumerate() {
        assert_eq!(edge.source_pc(), branch);
        assert_eq!(edge.arm(), arm);
        assert_eq!(edge.target_pc(), header);
        let bindings: Vec<_> = edge.bindings().collect();
        assert_eq!(bindings[0].parameter, 6);
        assert_eq!(bindings[0].argument, if arm == 0 { 4 } else { 5 });
        assert_eq!(bindings[1].parameter, 7);
        kind(
            view.contracts(),
            member(view.contracts(), bindings[0].value, 0),
            if arm == 0 {
                ValueType::I64
            } else {
                ValueType::Bool
            },
        );
        kind(
            view.contracts(),
            member(view.contracts(), bindings[1].value, 0),
            if arm == 0 {
                ValueType::Bool
            } else {
                ValueType::I64
            },
        );
    }
}

#[test]
fn loop_queries_preserve_parallel_swaps_and_share_deep_contract_graphs() {
    let mut b = ProgramBuilder::new();
    let mut contract = b.contract(Contract::Kind(ValueType::I64)).unwrap();
    for _ in 0..2048 {
        contract = b
            .contract(Contract::Tuple(&[Some(contract), Some(contract)]))
            .unwrap();
    }
    let mut a = asm(&[
        ValueType::Unit,
        ValueType::Agg,
        ValueType::Agg,
        ValueType::Bool,
    ]);
    let head = a.block(&[1, 2]);
    let done = a.label();
    a.jmp(head.with_args(&[1, 2]));
    let header = a.pc();
    a.place(head).unwrap();
    a.br(3, head.with_args(&[2, 1]), done);
    a.place(done).unwrap();
    a.ret(0, &[1]);
    let entry = b
        .push_function_checked(
            a,
            FunctionAbi {
                args: vec![contract.into(), contract.into(), ValueType::Bool.into()],
                rets: vec![contract.into()],
            },
        )
        .unwrap();
    let p = b.build_checked().unwrap();
    let mut f = ProgramAnalysis::new(&p, &VerifyConfig::default())
        .unwrap()
        .function(entry)
        .unwrap();
    assert!(f.contracts().len() < 2060, "shared DAG must not unfold");
    for _ in 0..3 {
        let view = f.instruction(header).unwrap();
        assert_eq!(view.incoming_edges().len(), 2);
        let r1 = view.register(1).unwrap();
        assert_eq!(r1.before, r1.after);
        let mut fact = r1.before;
        for _ in 0..2048 {
            let left = member(view.contracts(), fact, 0);
            assert_eq!(left, member(view.contracts(), fact, 1));
            fact = left;
        }
        kind(view.contracts(), fact, ValueType::I64);
        let back = view
            .incoming_edges()
            .find(|edge| edge.arm() == 0 && edge.source_pc() != 0)
            .unwrap();
        let bindings: Vec<_> = back.bindings().collect();
        assert_eq!(bindings[0].argument, 2);
        assert_eq!(bindings[1].argument, 1);
    }
}

#[test]
fn rejected_programs_never_yield_inferred_facts() {
    let (mut p, _, _) = nested();
    p.contract_ids[0] = ContractId(u32::MAX);
    assert!(ProgramAnalysis::new(&p, &VerifyConfig::default()).is_err());
}

#[test]
fn untyped_joins_exclude_dead_predecessors_and_intersect_initialization() {
    let mut b = ProgramBuilder::new();
    let mut a = asm(&[
        ValueType::Unit,
        ValueType::Bool,
        ValueType::I64,
        ValueType::Bool,
        ValueType::Agg,
        ValueType::I64,
    ]);
    let left = a.label();
    let right = a.label();
    let join = a.label();
    a.const_i64(2, 42).const_bool(3, true).br(1, left, right);
    a.place(left).unwrap();
    a.tuple_new(4, &[2]).const_i64(5, 7).jmp(join);
    a.place(right).unwrap();
    a.tuple_new(4, &[3]).jmp(join);
    let header = a.pc();
    a.place(join).unwrap();
    a.ret(0, &[]);
    a.tuple_new(4, &[2]).jmp(join); // This syntactic predecessor is unreachable.
    let entry = b
        .push_function_checked(a, signature(&[ValueType::Bool], &[]))
        .unwrap();
    let p = b.build_checked().unwrap();
    let mut f = ProgramAnalysis::new(&p, &VerifyConfig::default())
        .unwrap()
        .function(entry)
        .unwrap();
    let view = f.instruction(header).unwrap();
    assert_eq!(view.incoming_edges().len(), 2);
    let reg = view.register(4).unwrap();
    assert_eq!(
        view.contracts().get(id(reg.before)),
        Some(Contract::Tuple(&[None]))
    );
    assert_eq!(view.register(5).unwrap().before, ValueFact::Uninitialized);
    for edge in view.incoming_edges() {
        assert_eq!(edge.bindings().len(), 0);
        let child = member(view.contracts(), edge.register(4).unwrap(), 0);
        assert!(matches!(
            view.contracts().get(id(child)),
            Some(Contract::Kind(ValueType::I64 | ValueType::Bool))
        ));
    }
}

#[test]
fn nominal_struct_and_array_facts_preserve_nested_member_contracts() {
    use execution_tape::program::StructTypeDef;
    let mut b = ProgramBuilder::new();
    let integer = b.contract(Contract::Kind(ValueType::I64)).unwrap();
    let tuple = b.contract(Contract::Tuple(&[Some(integer)])).unwrap();
    let schema = b.struct_type(StructTypeDef {
        field_names: vec!["payload".into()],
        field_types: vec![ValueType::Agg],
    });
    let element = b.array_elem(ValueType::Agg);
    let mut a = asm(&[
        ValueType::Unit,
        ValueType::Agg,
        ValueType::Agg,
        ValueType::Agg,
        ValueType::Agg,
        ValueType::I64,
    ]);
    let make_struct = a.pc();
    a.struct_new(2, schema, &[1]);
    let make_array = a.pc();
    a.array_new(3, element, &[2]);
    a.array_get_imm(4, 3, 0)
        .struct_get(1, 4, 0)
        .tuple_get(5, 1, 0)
        .ret(0, &[5]);
    let entry = b
        .push_function_checked(
            a,
            FunctionAbi {
                args: vec![tuple.into()],
                rets: vec![ValueType::I64.into()],
            },
        )
        .unwrap();
    let p = b.build_checked().unwrap();
    let mut f = ProgramAnalysis::new(&p, &VerifyConfig::default())
        .unwrap()
        .function(entry)
        .unwrap();
    {
        let view = f.instruction(make_struct).unwrap();
        let Some(Contract::Struct { type_id, fields }) =
            view.contracts().get(id(view.register(2).unwrap().after))
        else {
            panic!("expected nominal struct")
        };
        assert_eq!(type_id, schema);
        kind(
            view.contracts(),
            member(view.contracts(), ValueFact::Contract(fields[0]), 0),
            ValueType::I64,
        );
    }
    let view = f.instruction(make_array).unwrap();
    let Some(Contract::Array {
        elem_type_id,
        element: child,
    }) = view.contracts().get(id(view.register(3).unwrap().after))
    else {
        panic!("expected nominal array")
    };
    assert_eq!(elem_type_id, element);
    assert!(
        matches!(view.contracts().get(child),Some(Contract::Struct{type_id,..}) if type_id==schema)
    );
}

#[test]
fn block_zero_meets_invocation_seeds_with_backedges_without_fabricating_a_cfg_edge() {
    use execution_tape::host::HostSig;
    let mut b = ProgramBuilder::new();
    let integer = b.contract(Contract::Kind(ValueType::I64)).unwrap();
    let tuple = b.contract(Contract::Tuple(&[Some(integer)])).unwrap();
    let host = b.host_sig_for(
        "replace",
        HostSig {
            args: vec![],
            rets: vec![ValueType::Agg],
        },
    );
    let mut a = asm(&[ValueType::Unit, ValueType::Agg, ValueType::Bool]);
    let head = a.label();
    let done = a.label();
    a.place(head).unwrap();
    a.host_call(0, host, 0, &[], &[1]).br(2, head, done);
    a.place(done).unwrap();
    a.ret(0, &[]);
    let entry = b
        .push_function_checked(
            a,
            FunctionAbi {
                args: vec![tuple.into(), ValueType::Bool.into()],
                rets: vec![],
            },
        )
        .unwrap();
    let p = b.build_checked().unwrap();
    let mut f = ProgramAnalysis::new(&p, &VerifyConfig::default())
        .unwrap()
        .function(entry)
        .unwrap();
    let view = f.instruction(0).unwrap();
    let r1 = view.register(1).unwrap();
    kind(view.contracts(), r1.before, ValueType::Agg);
    assert_eq!(r1.origin_before, Some(FactOrigin::BlockEntry { pc: 0 }));
    // There is one real backedge; the external invocation is an additional analysis seed.
    assert_eq!(view.incoming_edges().len(), 1);
}
