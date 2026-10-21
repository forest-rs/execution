// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use super::*;
use crate::asm::{Asm, FunctionSig, ProgramBuilder};
use crate::host::{Host, HostContext, HostError, SigHash, ValueRef};
use crate::trace::{TraceLocation, TraceMask, TraceOutcome, TraceSink};
use crate::value::{Decimal, Value};
use crate::vm::{ExecutionContext, Limits, Trap, Vm};

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

fn program(asm: Asm, args: &[ValueType], returns: &[ValueType]) -> Program {
    let mut builder = ProgramBuilder::new();
    builder
        .push_function_checked(
            asm,
            FunctionSig {
                arg_types: args.into(),
                ret_types: returns.into(),
            },
        )
        .unwrap();
    let p = builder.build();
    Program::decode(&p.encode()).unwrap()
}

fn verify(asm: Asm, args: &[ValueType], returns: &[ValueType]) -> VerifiedProgram {
    verify_program_owned(program(asm, args, returns), &VerifyConfig::default()).unwrap()
}

fn run(p: &VerifiedProgram, args: &[Value]) -> Vec<Value> {
    Vm::new(NoHost, Limits::default())
        .run(p, FuncId(0), args, TraceMask::NONE, None)
        .unwrap()
}

#[test]
fn parallel_edges_execute_every_three_register_cycle_and_fanout() {
    // All 27 assignments, including self edges, swaps, three-cycles, and fan-out. The source
    // registers are also parameter destinations, so sequential implementations give wrong values.
    for a in 1..=3 {
        for b in 1..=3 {
            for c in 1..=3 {
                let mut asm = Asm::new();
                let join = asm.block(&[
                    (1, ValueType::I64),
                    (2, ValueType::I64),
                    (3, ValueType::I64),
                ]);
                asm.jmp(join.with_args(&[a, b, c]));
                asm.place(join).unwrap();
                asm.ret(0, &[1, 2, 3]);
                let p = verify(asm, &[ValueType::I64; 3], &[ValueType::I64; 3]);
                let values = [Value::I64(11), Value::I64(22), Value::I64(33)];
                assert_eq!(
                    run(&p, &values),
                    [
                        values[a as usize - 1].clone(),
                        values[b as usize - 1].clone(),
                        values[c as usize - 1].clone()
                    ]
                );
                assert_eq!(p.verified(FuncId(0)).unwrap().instrs.len(), 2);
            }
        }
    }
}

#[test]
fn equal_target_arms_keep_distinct_arguments_and_loop_backedges_work() {
    let mut asm = Asm::new();
    let join = asm.block(&[(2, ValueType::I64), (3, ValueType::I64)]);
    asm.br(1, join.with_args(&[2, 3]), join.with_args(&[3, 2]));
    asm.place(join).unwrap();
    asm.ret(0, &[2, 3]);
    let p = verify(
        asm,
        &[ValueType::Bool, ValueType::I64, ValueType::I64],
        &[ValueType::I64; 2],
    );
    for cond in [false, true] {
        assert_eq!(
            run(&p, &[Value::Bool(cond), Value::I64(7), Value::I64(9)]),
            if cond {
                vec![Value::I64(7), Value::I64(9)]
            } else {
                vec![Value::I64(9), Value::I64(7)]
            }
        );
    }
    let mut asm = Asm::new();
    let head = asm.block(&[(2, ValueType::I64), (3, ValueType::I64)]);
    let body = asm.label();
    let done = asm.block(&[(8, ValueType::I64)]);
    asm.const_i64(4, 0)
        .const_i64(5, 1)
        .jmp(head.with_args(&[1, 4]));
    asm.place(head).unwrap();
    asm.i64_gt(6, 2, 4).br(6, body, done.with_args(&[3]));
    asm.place(body).unwrap();
    asm.i64_add(7, 3, 2)
        .i64_sub(2, 2, 5)
        .jmp(head.with_args(&[2, 7]));
    asm.place(done).unwrap();
    asm.ret(0, &[8]);
    let p = verify(asm, &[ValueType::I64], &[ValueType::I64]);
    for n in 0..100 {
        assert_eq!(run(&p, &[Value::I64(n)]), [Value::I64(n * (n + 1) / 2)]);
    }
}

#[test]
fn all_scalar_and_borrowed_payload_classes_transfer_without_changing_bits() {
    use crate::program::HostTypeId;
    use crate::value::ObjHandle;
    let types = [
        ValueType::Unit,
        ValueType::Bool,
        ValueType::I64,
        ValueType::U64,
        ValueType::F64,
        ValueType::Decimal,
        ValueType::Bytes,
        ValueType::Str,
        ValueType::Obj(HostTypeId(42)),
        ValueType::Func,
    ];
    let mut asm = Asm::new();
    // One swap cycle in each bank, with mixed classes interleaved in the declaration.
    let args: Vec<_> = types.iter().flat_map(|&ty| [ty, ty]).collect();
    let params: Vec<_> = args
        .iter()
        .enumerate()
        .map(|(i, &ty)| (u32::try_from(i).unwrap() + 1, ty))
        .collect();
    let join = asm.block(&params);
    let sources: Vec<_> = (1..=u32::try_from(args.len()).unwrap())
        .map(|r| if r % 2 == 1 { r + 1 } else { r - 1 })
        .collect();
    asm.jmp(join.with_args(&sources));
    asm.place(join).unwrap();
    let rets: Vec<_> = (1..=u32::try_from(args.len()).unwrap()).collect();
    asm.ret(0, &rets);
    let p = verify(asm, &args, &args);
    let pairs = [
        [Value::Unit, Value::Unit],
        [Value::Bool(false), Value::Bool(true)],
        [Value::I64(i64::MIN), Value::I64(i64::MAX)],
        [Value::U64(0), Value::U64(u64::MAX)],
        [
            Value::F64(f64::from_bits(0x7ff8000000001234)),
            Value::F64(-0.0),
        ],
        [
            Value::Decimal(Decimal {
                mantissa: 123,
                scale: 2,
            }),
            Value::Decimal(Decimal {
                mantissa: -123,
                scale: 3,
            }),
        ],
        [Value::Bytes(vec![0, 255]), Value::Bytes(vec![1, 2, 3])],
        [Value::Str("héllo".into()), Value::Str("世界".into())],
        [
            Value::Obj(crate::value::Obj {
                handle: ObjHandle(7),
                host_type: HostTypeId(42),
            }),
            Value::Obj(crate::value::Obj {
                handle: ObjHandle(9),
                host_type: HostTypeId(42),
            }),
        ],
        [Value::Func(FuncId(0)), Value::Func(FuncId(0))],
    ];
    let values: Vec<_> = pairs.into_iter().flatten().collect();
    let out = run(&p, &values);
    for (ix, (actual, expected)) in out
        .iter()
        .zip(sources.iter().map(|&r| &values[r as usize - 1]))
        .enumerate()
    {
        if let (Value::F64(a), Value::F64(b)) = (actual, expected) {
            assert_eq!(a.to_bits(), b.to_bits());
        } else {
            assert_eq!(actual, expected, "slot {ix}");
        }
    }
    let counts = p.verified(FuncId(0)).unwrap().reg_layout.counts;
    assert_eq!(counts.i64s, 3);
    assert_eq!(counts.bytes, 3);
    assert_eq!(counts.strs, 3);
}

#[test]
fn aggregate_and_closure_handles_survive_parallel_cycles_and_run_cleanup() {
    let mut asm = Asm::new();
    let join = asm.block(&[
        (2, ValueType::Agg),
        (3, ValueType::Agg),
        (5, ValueType::Closure),
        (6, ValueType::Closure),
    ]);
    asm.const_i64(1, 7)
        .tuple_new(2, &[1])
        .tuple_new(3, &[])
        .const_func(4, FuncId(0));
    asm.closure_new(5, 4, 2)
        .closure_new(6, 4, 3)
        .jmp(join.with_args(&[3, 2, 6, 5]));
    asm.place(join).unwrap();
    asm.ret(0, &[2, 3, 5, 6]);
    let p = verify(
        asm,
        &[],
        &[
            ValueType::Agg,
            ValueType::Agg,
            ValueType::Closure,
            ValueType::Closure,
        ],
    );
    let mut vm = Vm::new(NoHost, Limits::default());
    let out = vm.run(&p, FuncId(0), &[], TraceMask::NONE, None).unwrap();
    let [
        Value::Agg(empty),
        Value::Agg(full),
        Value::Closure(a),
        Value::Closure(b),
    ] = &out[..]
    else {
        panic!("wrong outputs")
    };
    assert_eq!(a.env, *empty);
    assert_eq!(b.env, *full);
    assert_eq!(vm.aggregates().tuple_get(*full, 0).unwrap(), Value::I64(7));
    assert!(vm.aggregates().tuple_get(*empty, 0).is_err());
}

#[test]
fn matching_shapes_propagate_but_conflicting_or_opaque_edges_cannot_invent_them() {
    for mode in 0..3 {
        let mut asm = Asm::new();
        let join = asm.block(&[(5, ValueType::Agg)]);
        asm.const_i64(2, 17).tuple_new(3, &[2]);
        if mode == 0 {
            asm.tuple_new(4, &[2]);
        } else if mode == 1 {
            asm.tuple_new(4, &[]);
        } else {
            asm.mov(4, 6);
        }
        asm.tuple_new(5, &[2]);
        asm.br(1, join.with_args(&[3]), join.with_args(&[4]));
        asm.place(join).unwrap();
        asm.tuple_get(7, 5, 0);
        let use_pc = asm.pc();
        asm.ret(0, &[7]);
        let p = program(
            asm,
            &[
                ValueType::Bool,
                ValueType::I64,
                ValueType::Agg,
                ValueType::Agg,
                ValueType::Agg,
                ValueType::Agg,
            ],
            &[ValueType::I64],
        );
        let result = verify_program_owned(p, &VerifyConfig::default());
        if mode == 0 {
            assert!(result.is_ok(), "{result:?}");
        } else {
            assert_eq!(
                result.unwrap_err(),
                VerifyError::UnknownTypeAtUse {
                    func: 0,
                    pc: use_pc,
                    reg: 7,
                    expected: ValueType::I64
                }
            );
        }
    }
    // Copying a source fact into a parameter must leave the source fact usable too.
    let mut asm = Asm::new();
    let join = asm.block(&[(3, ValueType::Agg)]);
    asm.const_i64(1, 11)
        .tuple_new(2, &[1])
        .jmp(join.with_args(&[2]));
    asm.place(join).unwrap();
    asm.tuple_get(4, 2, 0).tuple_get(5, 3, 0).ret(0, &[4, 5]);
    assert_eq!(
        run(&verify(asm, &[], &[ValueType::I64; 2]), &[]),
        [Value::I64(11), Value::I64(11)]
    );
}

#[test]
fn edge_errors_identify_the_source_arm_target_and_argument() {
    for cond in [false, true] {
        let mut asm = Asm::new();
        let join = asm.block(&[(4, ValueType::I64)]);
        asm.const_bool(1, cond).const_i64(2, 7).const_bool(3, true);
        let pc = asm.pc();
        asm.br(1, join.with_args(&[2]), join.with_args(&[3]));
        let target = asm.pc();
        asm.place(join).unwrap();
        asm.ret(0, &[4]);
        assert_eq!(
            verify_program_owned(
                program(asm, &[], &[ValueType::I64]),
                &VerifyConfig::default()
            )
            .unwrap_err(),
            VerifyError::BlockEdge {
                func: 0,
                pc,
                arm: 1,
                target,
                error: BlockEdgeError::Type {
                    index: 0,
                    reg: 3,
                    expected: ValueType::I64,
                    actual: Some(ValueType::Bool)
                }
            }
        );
    }
    let mut asm = Asm::new();
    let join = asm.block(&[(1, ValueType::I64)]);
    asm.jmp(join);
    let target = asm.pc();
    asm.place(join).unwrap();
    asm.ret(0, &[1]);
    assert_eq!(
        verify_program_owned(
            program(asm, &[], &[ValueType::I64]),
            &VerifyConfig::default()
        )
        .unwrap_err(),
        VerifyError::BlockEdge {
            func: 0,
            pc: 0,
            arm: 0,
            target,
            error: BlockEdgeError::Arity {
                expected: 1,
                actual: 0
            }
        }
    );
}

#[test]
fn declarations_reject_invalid_parameters_and_bypassed_headers() {
    for params in [
        vec![(0, ValueType::Unit)],
        vec![(1, ValueType::I64), (1, ValueType::I64)],
    ] {
        let mut asm = Asm::new();
        let join = asm.block(&params);
        asm.jmp(join.with_args(&[1, 1][..params.len()]));
        let pc = asm.pc();
        asm.place(join).unwrap();
        asm.ret(0, &[]);
        assert!(
            matches!(verify_program_owned(program(asm,&[ValueType::I64],&[]),&VerifyConfig::default()),Err(VerifyError::BlockDeclaration{pc:p,..}) if p==pc)
        );
    }
    let mut asm = Asm::new();
    let entry = asm.block(&[(1, ValueType::I64)]);
    asm.place(entry).unwrap();
    asm.ret(0, &[]);
    assert!(matches!(
        verify_program_owned(program(asm, &[], &[]), &VerifyConfig::default()),
        Err(VerifyError::BlockDeclaration {
            error: BlockDeclarationError::EntryParameters,
            ..
        })
    ));
    let mut asm = Asm::new();
    let join = asm.block(&[(2, ValueType::I64)]);
    let interior = asm.label();
    asm.jmp(interior);
    let header = asm.pc();
    asm.place(join).unwrap();
    let target = asm.pc();
    asm.place(interior).unwrap();
    asm.ret(0, &[2]);
    assert_eq!(
        verify_program_owned(
            program(asm, &[ValueType::I64], &[ValueType::I64]),
            &VerifyConfig::default()
        )
        .unwrap_err(),
        VerifyError::BlockEdge {
            func: 0,
            pc: 0,
            arm: 0,
            target,
            error: BlockEdgeError::BypassesHeader { header }
        }
    );
    let mut asm = Asm::new();
    let join = asm.block(&[(2, ValueType::I64)]);
    asm.const_i64(1, 7);
    asm.place(join).unwrap();
    asm.ret(0, &[2]);
    assert!(matches!(
        verify_program_owned(
            program(asm, &[], &[ValueType::I64]),
            &VerifyConfig::default()
        ),
        Err(VerifyError::MissingTerminator { .. })
    ));
}

#[test]
fn source_headers_lexical_ends_successors_spans_and_fuel_stay_distinct() {
    #[derive(Default)]
    struct Events {
        attempts: Vec<(u32, u32, Option<u64>, u8)>,
        ends: Vec<(u32, Option<u32>)>,
    }
    impl TraceSink for Events {
        fn instr(
            &mut self,
            _: &Program,
            _: FuncId,
            pc: u32,
            end: u32,
            span: Option<u64>,
            opcode: u8,
        ) {
            self.attempts.push((pc, end, span, opcode));
        }
        fn instr_end(
            &mut self,
            _: &Program,
            location: TraceLocation,
            next: Option<TraceLocation>,
            outcome: TraceOutcome<'_>,
        ) {
            assert!(matches!(outcome, TraceOutcome::Ok));
            self.ends.push((location.pc, next.map(|l| l.pc)));
        }
    }
    let mut asm = Asm::new();
    let entry = asm.block(&[]);
    let join = asm.block(&[(1, ValueType::I64), (2, ValueType::I64)]);
    asm.place(entry).unwrap();
    let branch = asm.pc();
    asm.span(SpanId(core::num::NonZeroU64::new(11).unwrap()))
        .jmp(join.with_args(&[2, 1]));
    let header = asm.pc();
    asm.span(SpanId(core::num::NonZeroU64::new(22).unwrap()))
        .place(join)
        .unwrap();
    let body = asm.pc();
    asm.ret(0, &[1, 2]);
    let p = verify(asm, &[ValueType::I64; 2], &[ValueType::I64; 2]);
    let source = crate::disasm::disassemble_function(p.program(), FuncId(0)).unwrap();
    assert_eq!(source.executable_pc(0), Some(branch));
    assert_eq!(source.executable_pc(header), Some(body));
    let prepared = p.function(FuncId(0)).unwrap();
    let locations: Vec<_> = prepared.instructions().collect();
    assert_eq!(locations.len(), 2);
    assert_eq!(
        (locations[0].pc, locations[0].next_pc, locations[0].opcode),
        (branch, header, Opcode::JmpArgs)
    );
    assert_eq!(locations[1].pc, body);
    assert_eq!(prepared.register_slots(ValueType::I64), 3);
    assert_eq!(prepared.stats().operand_count, 8);

    let edge = source
        .instrs()
        .find(|iv| iv.pc() == branch)
        .unwrap()
        .edge(0)
        .unwrap();
    assert_eq!(edge.target, header);
    assert_eq!(edge.args, [2, 1]);
    for fuel in 0..=2 {
        let mut vm = Vm::new(
            NoHost,
            Limits {
                fuel,
                ..Limits::default()
            },
        );
        let mut events = Events::default();
        let mut ctx = ExecutionContext::new();
        let mut outputs = vec![Value::I64(99)];
        let result = vm.run_with_ctx(
            &mut ctx,
            &p,
            FuncId(0),
            &[Value::I64(7), Value::I64(9)],
            &mut outputs,
            TraceMask::INSTR,
            Some(&mut events),
            None,
        );
        assert_eq!(events.attempts.len(), usize::try_from(fuel).unwrap());
        if fuel < 2 {
            let trap = result.unwrap_err();
            assert!(matches!(trap.trap, Trap::FuelExceeded));
            assert_eq!(trap.pc, if fuel == 0 { branch } else { body });
            assert_eq!(outputs, [Value::I64(99)]);
        } else {
            result.unwrap();
            assert_eq!(outputs, [Value::I64(99), Value::I64(9), Value::I64(7)]);
        }
        if fuel > 0 {
            assert_eq!(
                events.attempts[0],
                (branch, header, Some(11), Opcode::JmpArgs as u8)
            );
            assert_eq!(events.ends[0], (branch, Some(body)));
        }
        if fuel == 2 {
            assert_eq!(events.attempts[1].2, Some(22));
        }
    }
}

#[test]
fn wide_reverse_chains_and_many_cycles_prepare_and_execute() {
    const WIDTH: u32 = 4096;
    for cycle in [false, true] {
        let types = vec![ValueType::I64; WIDTH as usize + 1];
        let mut params: Vec<_> = (1..=WIDTH).map(|reg| (reg, ValueType::I64)).collect();
        let mut sources: Vec<_> = (1..=WIDTH)
            .map(|reg| {
                if cycle {
                    if reg % 2 == 1 { reg + 1 } else { reg - 1 }
                } else {
                    reg + 1
                }
            })
            .collect();
        let expected = sources.clone();
        params.reverse();
        sources.reverse();
        let mut asm = Asm::new();
        let join = asm.block(&params);
        asm.jmp(join.with_args(&sources));
        asm.place(join).unwrap();
        let returns: Vec<_> = (1..=WIDTH).collect();
        asm.ret(0, &returns);
        let p = verify(asm, &types, &types[..WIDTH as usize]);
        let values: Vec<_> = (1..=WIDTH + 1)
            .map(|value| Value::I64(i64::from(value)))
            .collect();
        assert_eq!(
            run(&p, &values),
            expected
                .iter()
                .map(|&value| Value::I64(i64::from(value)))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            p.verified(FuncId(0)).unwrap().reg_layout.counts.i64s,
            WIDTH as usize + 1 + usize::from(cycle)
        );
    }
}

#[test]
fn malformed_headers_lists_and_splits_fail_without_panics() {
    use crate::codec::{Operand, decode_instruction, encode_instruction};
    let mut header = Vec::new();
    encode_instruction(
        Opcode::Block,
        &[
            Operand::RegList(&[1]),
            Operand::ValueTypeList(&[ValueType::Obj(crate::program::HostTypeId(u64::MAX))]),
        ],
        &mut header,
    )
    .unwrap();
    assert!(decode_instruction(&header).is_ok());
    for end in 0..header.len() {
        assert!(decode_instruction(&header[..end]).is_err());
    }
    assert!(decode_instruction(&[Opcode::Block as u8, 0, 0xff, 0xff, 0xff, 0xff, 0x0f]).is_err());
    assert!(decode_instruction(&[Opcode::Block as u8, 0, 1, 0xff]).is_err());
    for split in [0, 1] {
        let mut p = program(
            {
                let mut asm = Asm::new();
                asm.ret(0, &[]);
                asm
            },
            &[ValueType::Bool],
            &[],
        );
        let mut bytes = Vec::new();
        // Self target, no headers: valid empty arm is a loop, invalid split must be rejected
        // even when there is no declaration in this function.
        encode_instruction(
            Opcode::BrArgs,
            &[
                Operand::Reg(1),
                Operand::Pc(0),
                Operand::Pc(0),
                Operand::ImmU32(split),
                Operand::RegList(&[]),
            ],
            &mut bytes,
        )
        .unwrap();
        p.functions[0].bytecode.offset = 0;
        p.functions[0].bytecode.len = u32::try_from(bytes.len()).unwrap();
        p.bytecode_data = bytes;
        let result = verify_program_owned(p, &VerifyConfig::default());
        if split == 0 {
            assert!(result.is_ok());
        } else {
            assert!(matches!(
                result,
                Err(VerifyError::BlockEdge {
                    error: BlockEdgeError::InvalidArgumentSplit,
                    ..
                })
            ));
        }
    }
    let mut bytes = Vec::new();
    encode_instruction(
        Opcode::Block,
        &[Operand::RegList(&[1]), Operand::ValueTypeList(&[])],
        &mut bytes,
    )
    .unwrap();
    let mut p = program(
        {
            let mut asm = Asm::new();
            asm.ret(0, &[]);
            asm
        },
        &[ValueType::I64],
        &[],
    );
    p.functions[0].bytecode.offset = 0;
    p.functions[0].bytecode.len = u32::try_from(bytes.len()).unwrap();
    p.bytecode_data = bytes;
    assert!(matches!(
        verify_program_owned(p, &VerifyConfig::default()),
        Err(VerifyError::BlockDeclaration {
            error: BlockDeclarationError::LengthMismatch {
                registers: 1,
                types: 0
            },
            ..
        })
    ));
}

#[test]
fn calls_into_entry_declarations_keep_scope_locations_and_spans_together() {
    use crate::trace::ScopeKind;
    #[derive(Default)]
    struct Scopes(Vec<(FuncId, u32, Option<u64>)>);
    impl TraceSink for Scopes {
        fn scope_enter(
            &mut self,
            _: &Program,
            kind: ScopeKind,
            _: usize,
            func: FuncId,
            pc: u32,
            span: Option<u64>,
        ) {
            if matches!(kind, ScopeKind::CallFrame { .. }) {
                self.0.push((func, pc, span));
            }
        }
    }
    for indirect in [false, true] {
        let mut builder = ProgramBuilder::new();
        let mut body = Asm::new();
        let entry = body.block(&[]);
        body.span(SpanId(core::num::NonZeroU64::new(11).unwrap()))
            .place(entry)
            .unwrap();
        let body_pc = body.pc();
        body.span(SpanId(core::num::NonZeroU64::new(22).unwrap()))
            .ret(0, &[1]);
        let callee = builder
            .push_function_checked(
                body,
                FunctionSig {
                    arg_types: vec![ValueType::I64],
                    ret_types: vec![ValueType::I64],
                },
            )
            .unwrap();
        let mut caller = Asm::new();
        if indirect {
            let sig = builder.call_sig(&[ValueType::I64], &[ValueType::I64]);
            caller
                .const_func(2, callee)
                .call_indirect(0, sig, 2, 0, &[1], &[3]);
        } else {
            caller.call(0, callee, 0, &[1], &[3]);
        }
        caller.ret(0, &[3]);
        let entry = builder
            .push_function_checked(
                caller,
                FunctionSig {
                    arg_types: vec![ValueType::I64],
                    ret_types: vec![ValueType::I64],
                },
            )
            .unwrap();
        let p = builder.build_verified().unwrap();
        let mut scopes = Scopes::default();
        let out = Vm::new(NoHost, Limits::default())
            .run(
                &p,
                entry,
                &[Value::I64(7)],
                TraceMask::CALL,
                Some(&mut scopes),
            )
            .unwrap();
        assert_eq!(out, [Value::I64(7)]);
        assert_eq!(scopes.0, [(entry, 0, None), (callee, body_pc, Some(22))]);
    }
}

#[test]
fn aggregate_shape_cycles_read_the_original_predecessor_facts() {
    let mut asm = Asm::new();
    let join = asm.block(&[(2, ValueType::Agg), (3, ValueType::Agg)]);
    asm.const_i64(1, 17)
        .tuple_new(2, &[1])
        .const_bool(4, true)
        .tuple_new(3, &[4])
        .jmp(join.with_args(&[3, 2]));
    asm.place(join).unwrap();
    asm.tuple_get(5, 2, 0).tuple_get(6, 3, 0).ret(0, &[5, 6]);
    assert_eq!(
        run(&verify(asm, &[], &[ValueType::Bool, ValueType::I64]), &[]),
        [Value::Bool(true), Value::I64(17)]
    );
}

#[test]
fn uninitialized_edge_arguments_report_the_offending_arm_and_slot() {
    let mut asm = Asm::new();
    let join = asm.block(&[(2, ValueType::I64)]);
    asm.const_bool(1, true);
    let pc = asm.pc();
    asm.br(1, join.with_args(&[2]), join.with_args(&[2]));
    let target = asm.pc();
    asm.place(join).unwrap();
    asm.ret(0, &[2]);
    assert_eq!(
        verify_program_owned(
            program(asm, &[], &[ValueType::I64]),
            &VerifyConfig::default()
        )
        .unwrap_err(),
        VerifyError::BlockEdge {
            func: 0,
            pc,
            arm: 0,
            target,
            error: BlockEdgeError::Type {
                index: 0,
                reg: 2,
                expected: ValueType::I64,
                actual: None
            }
        }
    );
}
