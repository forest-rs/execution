// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use super::*;
use crate::asm::{Asm, FunctionSig, ProgramBuilder};
use crate::host::{HostError, HostSig, SigHash, ValueRef};
use crate::program::StructTypeDef;
use alloc::vec;

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
        Err(HostError::UnknownSymbol)
    }
}

fn program(
    args: &[ValueType],
    rets: &[ValueType],
    kinds: Vec<ValueType>,
    emit: impl FnOnce(&mut Asm, &mut ProgramBuilder),
) -> VerifiedProgram {
    let mut builder = ProgramBuilder::new();
    let mut body = Asm::new();
    body.set_register_types(kinds);
    emit(&mut body, &mut builder);
    builder
        .push_function_checked(
            body,
            FunctionSig {
                arg_types: args.into(),
                ret_types: rets.into(),
            },
        )
        .unwrap();
    builder.build_verified().unwrap()
}

fn vm(limit: u64) -> Vm<NoHost> {
    Vm::new(
        NoHost,
        Limits {
            max_payload_work: limit,
            ..Limits::default()
        },
    )
}

fn exact_work(p: &VerifiedProgram, args: &[Value], required: u64) {
    for limit in [required.saturating_sub(1), required] {
        let mut vm = vm(limit);
        let retained = vm
            .aggregates_mut()
            .tuple_new(vec![Value::Str("retained".into())])
            .unwrap();
        let mut ctx = ExecutionContext::new();
        let mut outputs = vec![Value::I64(99)];
        let result = vm.run_with_ctx(
            &mut ctx,
            p,
            FuncId(0),
            args,
            &mut outputs,
            TraceMask::NONE,
            None,
            None,
        );
        if limit == required {
            result.unwrap();
            assert_eq!(ctx.payload_work, required);
        } else {
            let error = result.unwrap_err();
            let Trap::PayloadWorkExceeded {
                limit: actual,
                used,
                requested,
            } = error.trap
            else {
                panic!("{error:?}")
            };
            assert_eq!(actual, limit);
            assert_eq!(used + requested, required);
            assert_eq!(ctx.payload_work, used);
            assert_eq!(outputs, [Value::I64(99)]);
            assert_eq!(vm.aggregates().len_u32(), 1);
            assert_eq!(
                vm.aggregates().tuple_get(retained, 0).unwrap(),
                Value::Str("retained".into())
            );
        }
    }
}

#[test]
fn doubling_traps_before_next_copy_and_context_resets() {
    for text in [false, true] {
        let kind = if text {
            ValueType::Str
        } else {
            ValueType::Bytes
        };
        let p = program(
            &[kind],
            &[ValueType::U64],
            vec![ValueType::Unit, kind, ValueType::U64],
            |a, _| {
                for _ in 0..7 {
                    if text {
                        a.str_concat(1, 1, 1);
                    } else {
                        a.bytes_concat(1, 1, 1);
                    }
                }
                if text {
                    a.str_len(2, 1);
                } else {
                    a.bytes_len(2, 1);
                }
                a.ret(0, &[2]);
            },
        );
        let args = [if text {
            Value::Str("x".into())
        } else {
            Value::Bytes(vec![1])
        }];
        let mut vm = vm(126);
        let mut ctx = ExecutionContext::new();
        let mut outputs = vec![Value::I64(99)];
        let error = vm
            .run_with_ctx(
                &mut ctx,
                &p,
                FuncId(0),
                &args,
                &mut outputs,
                TraceMask::NONE,
                None,
                None,
            )
            .unwrap_err();
        assert!(matches!(
            error.trap,
            Trap::PayloadWorkExceeded {
                limit: 126,
                used: 126,
                requested: 128
            }
        ));
        assert_eq!(ctx.payload_work, 126);
        assert_eq!(outputs, [Value::I64(99)]);
        vm.limits.max_payload_work = 254;
        for _ in 0..2 {
            outputs.clear();
            vm.run_with_ctx(
                &mut ctx,
                &p,
                FuncId(0),
                &args,
                &mut outputs,
                TraceMask::NONE,
                None,
                None,
            )
            .unwrap();
            assert_eq!(outputs, [Value::U64(128)]);
            assert_eq!(ctx.payload_work, 254);
        }
        ctx.clear();
        assert_eq!((ctx.payload_work, ctx.payload_limit), (0, 0));
    }
}

#[test]
fn aggregate_fields_and_repeated_root_returns_charge_each_owned_copy() {
    for constructor in 0..3 {
        for text in [false, true] {
            let kind = if text {
                ValueType::Str
            } else {
                ValueType::Bytes
            };
            let p = program(
                &[kind],
                &[ValueType::Agg, kind, kind],
                vec![ValueType::Unit, kind, ValueType::Agg],
                |a, b| {
                    match constructor {
                        0 => {
                            a.tuple_new(2, &[1, 1]);
                        }
                        1 => {
                            let ty = b.struct_type(StructTypeDef {
                                field_names: vec!["a".into(), "b".into()],
                                field_types: vec![kind, kind],
                            });
                            a.struct_new(2, ty, &[1, 1]);
                        }
                        _ => {
                            let ty = b.array_elem(kind);
                            a.array_new(2, ty, &[1, 1]);
                        }
                    }
                    a.ret(0, &[2, 1, 1]);
                },
            );
            exact_work(
                &p,
                &[if text {
                    Value::Str("abcd".into())
                } else {
                    Value::Bytes(vec![1; 4])
                }],
                16,
            );
        }
    }
}

#[test]
fn equality_uses_deterministic_length_work() {
    for text in [false, true] {
        let kind = if text {
            ValueType::Str
        } else {
            ValueType::Bytes
        };
        let p = program(
            &[kind, kind],
            &[ValueType::Bool],
            vec![ValueType::Unit, kind, kind, ValueType::Bool],
            |a, _| {
                if text {
                    a.str_eq(3, 1, 2);
                } else {
                    a.bytes_eq(3, 1, 2);
                }
                a.ret(0, &[3]);
            },
        );
        for unequal in [false, true] {
            let args = if text {
                vec![
                    Value::Str("abcd".into()),
                    Value::Str(if unequal { "zbcd" } else { "abcd" }.into()),
                ]
            } else {
                vec![
                    Value::Bytes(vec![1; 4]),
                    Value::Bytes(vec![if unequal { 2 } else { 1 }; 4]),
                ]
            };
            exact_work(&p, &args, 4);
        }
        let args = if text {
            vec![Value::Str("abcd".into()), Value::Str("x".into())]
        } else {
            vec![Value::Bytes(vec![1; 4]), Value::Bytes(vec![1])]
        };
        exact_work(&p, &args, 0);
    }
}

#[test]
fn slices_charge_output_bytes_after_index_and_character_validation() {
    for text in [false, true] {
        let kind = if text {
            ValueType::Str
        } else {
            ValueType::Bytes
        };
        let p = program(
            &[kind, ValueType::U64, ValueType::U64],
            &[ValueType::U64],
            vec![
                ValueType::Unit,
                kind,
                ValueType::U64,
                ValueType::U64,
                kind,
                ValueType::U64,
            ],
            |a, _| {
                if text {
                    a.str_slice(4, 1, 2, 3).str_len(5, 4);
                } else {
                    a.bytes_slice(4, 1, 2, 3).bytes_len(5, 4);
                }
                a.ret(0, &[5]);
            },
        );
        let value = if text {
            Value::Str("éxy".into())
        } else {
            Value::Bytes(vec![1; 4])
        };
        exact_work(&p, &[value.clone(), Value::U64(2), Value::U64(4)], 2);
        for (start, end) in [(4, 2), (0, 5), (1, 2)] {
            let result = vm(0).run(
                &p,
                FuncId(0),
                &[value.clone(), Value::U64(start), Value::U64(end)],
                TraceMask::NONE,
                None,
            );
            let trap = result.unwrap_err().trap;
            if start == 1 && !text {
                assert!(matches!(
                    trap,
                    Trap::PayloadWorkExceeded { requested: 1, .. }
                ));
            } else if start == 1 {
                assert!(matches!(trap, Trap::StrNotCharBoundary));
            } else {
                assert!(matches!(trap, Trap::IndexOutOfBounds));
            }
        }
    }
}

#[test]
fn conversions_charge_copy_and_utf8_work_before_it_occurs() {
    for to_text in [false, true] {
        let kind = if to_text {
            ValueType::Bytes
        } else {
            ValueType::Str
        };
        let result_kind = if to_text {
            ValueType::Str
        } else {
            ValueType::Bytes
        };
        let p = program(
            &[kind],
            &[ValueType::U64],
            vec![ValueType::Unit, kind, result_kind, ValueType::U64],
            |a, _| {
                if to_text {
                    a.bytes_to_str(2, 1).str_len(3, 2);
                } else {
                    a.str_to_bytes(2, 1).bytes_len(3, 2);
                }
                a.ret(0, &[3]);
            },
        );
        exact_work(
            &p,
            &[if to_text {
                Value::Bytes(vec![b'x'; 4])
            } else {
                Value::Str("xxxx".into())
            }],
            if to_text { 8 } else { 4 },
        );
        if to_text {
            let bad = [Value::Bytes(vec![255; 4])];
            assert!(matches!(
                vm(7)
                    .run(&p, FuncId(0), &bad, TraceMask::NONE, None)
                    .unwrap_err()
                    .trap,
                Trap::PayloadWorkExceeded { requested: 8, .. }
            ));
            assert!(matches!(
                vm(8)
                    .run(&p, FuncId(0), &bad, TraceMask::NONE, None)
                    .unwrap_err()
                    .trap,
                Trap::InvalidUtf8
            ));
        }
    }
}

#[test]
fn borrowed_constants_and_guest_transfers_do_not_charge() {
    let mut builder = ProgramBuilder::new();
    let mut body = Asm::new();
    body.set_register_types(vec![
        ValueType::Unit,
        ValueType::Str,
        ValueType::Str,
        ValueType::U64,
    ]);
    let text = builder.const_str("large enough to require payload storage");
    body.const_pool(1, text)
        .call(0, FuncId(1), 0, &[1], &[2])
        .str_len(3, 2)
        .ret(0, &[3]);
    builder
        .push_function_checked(
            body,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::U64],
            },
        )
        .unwrap();
    let mut callee = Asm::new();
    callee.set_register_types(vec![ValueType::Unit, ValueType::Str]);
    callee.mov(1, 1).ret(0, &[1]);
    builder
        .push_function_checked(
            callee,
            FunctionSig {
                arg_types: vec![ValueType::Str],
                ret_types: vec![ValueType::Str],
            },
        )
        .unwrap();
    exact_work(&builder.build_verified().unwrap(), &[], 0);
}

#[test]
fn zero_budget_accepts_empty_copies_and_rejects_nonempty_root_materialization() {
    for text in [false, true] {
        let kind = if text {
            ValueType::Str
        } else {
            ValueType::Bytes
        };
        let p = program(&[kind], &[kind], vec![ValueType::Unit, kind], |a, _| {
            a.ret(0, &[1]);
        });
        exact_work(
            &p,
            &[if text {
                Value::Str(String::new())
            } else {
                Value::Bytes(Vec::new())
            }],
            0,
        );
        exact_work(
            &p,
            &[if text {
                Value::Str("abc".into())
            } else {
                Value::Bytes(vec![1; 3])
            }],
            3,
        );
    }
}

#[test]
fn direct_host_payloads_charge_only_after_all_return_kinds_are_checked() {
    struct Returning {
        second: Value,
        fail: bool,
    }
    impl Host for Returning {
        fn call(
            &mut self,
            _: &str,
            _: SigHash,
            _: &[ValueRef<'_>],
            rets: &mut [Value],
            _: HostContext<'_, '_>,
        ) -> Result<u64, HostError> {
            rets[0] = Value::Bytes(vec![1; 4]);
            rets[1] = self.second.clone();
            if self.fail {
                Err(HostError::UnknownSymbol)
            } else {
                Ok(0)
            }
        }
    }
    let p = program(
        &[],
        &[ValueType::U64],
        vec![
            ValueType::Unit,
            ValueType::Bytes,
            ValueType::Str,
            ValueType::U64,
        ],
        |a, b| {
            let host = b.host_sig_for(
                "payload",
                HostSig {
                    args: vec![],
                    rets: vec![ValueType::Bytes, ValueType::Str],
                },
            );
            a.host_call_sig_id(0, host, 0, &[], &[1, 2])
                .bytes_len(3, 1)
                .ret(0, &[3]);
        },
    );
    for (second, fail, limit) in [
        (Value::Str("abc".into()), false, 6),
        (Value::Bool(true), false, 0),
        (Value::Str("abc".into()), true, 0),
    ] {
        let mut vm = Vm::new(
            Returning { second, fail },
            Limits {
                max_payload_work: limit,
                ..Limits::default()
            },
        );
        let mut ctx = ExecutionContext::new();
        let error = vm
            .run_with_ctx(
                &mut ctx,
                &p,
                FuncId(0),
                &[],
                &mut Vec::new(),
                TraceMask::NONE,
                None,
                None,
            )
            .unwrap_err();
        match error.trap {
            Trap::PayloadWorkExceeded {
                used: 0,
                requested: 7,
                ..
            } if !fail => {}
            Trap::TypeMismatch {
                actual: ValueType::Bool,
                ..
            } if !fail => {}
            Trap::HostCallFailed { .. } if fail => {}
            other => panic!("{other:?}"),
        }
        assert_eq!(ctx.payload_work, 0);
    }
    let mut vm = Vm::new(
        Returning {
            second: Value::Str("abc".into()),
            fail: false,
        },
        Limits {
            max_payload_work: 7,
            ..Limits::default()
        },
    );
    assert_eq!(
        vm.run(&p, FuncId(0), &[], TraceMask::NONE, None).unwrap(),
        [Value::U64(4)]
    );
}

#[test]
fn overflow_is_explicit_and_rejected_debits_do_not_change_usage() {
    let mut ctx = ExecutionContext::new();
    ctx.reset(0, u64::MAX);
    ctx.charge_payload_bytes(1, u64::MAX).unwrap();
    assert!(matches!(
        ctx.charge_payload_bytes(1, 1),
        Err(Trap::PayloadWorkExceeded {
            limit: u64::MAX,
            used: u64::MAX,
            requested: 1
        })
    ));
    assert!(matches!(
        ctx.charge_payload_bytes(2, u64::MAX),
        Err(Trap::PayloadSizeOverflow)
    ));
    assert_eq!(ctx.payload_work, u64::MAX);
}

#[test]
fn existing_aggregate_storage_is_not_charged_but_projected_root_copies_are() {
    struct Existing(AggHandle);
    impl Host for Existing {
        fn call(
            &mut self,
            _: &str,
            _: SigHash,
            _: &[ValueRef<'_>],
            rets: &mut [Value],
            _: HostContext<'_, '_>,
        ) -> Result<u64, HostError> {
            rets[0] = Value::Agg(self.0);
            Ok(0)
        }
    }
    let projected = |return_payload| {
        let mut b = ProgramBuilder::new();
        let bytes = b.contract(Contract::Kind(ValueType::Bytes)).unwrap();
        let tuple = b.contract(Contract::Tuple(&[Some(bytes)])).unwrap();
        let mut a = Asm::new();
        a.set_register_types(vec![
            ValueType::Unit,
            ValueType::Agg,
            ValueType::Bytes,
            ValueType::U64,
        ]);
        a.tuple_get(2, 1, 0)
            .bytes_len(3, 2)
            .ret(0, &[if return_payload { 2 } else { 3 }]);
        b.push_function_checked(
            a,
            crate::asm::FunctionAbi {
                args: vec![tuple.into()],
                rets: vec![
                    if return_payload {
                        ValueType::Bytes
                    } else {
                        ValueType::U64
                    }
                    .into(),
                ],
            },
        )
        .unwrap();
        b.build_verified().unwrap()
    };
    let mut vm = Vm::new(
        Existing(AggHandle(0)),
        Limits {
            max_payload_work: 0,
            ..Limits::default()
        },
    );
    let root = vm
        .aggregates_mut()
        .tuple_new(vec![Value::Bytes(vec![1; 4])])
        .unwrap();
    vm.host_mut().0 = root;
    assert_eq!(
        vm.run(
            &projected(false),
            FuncId(0),
            &[Value::Agg(root)],
            TraceMask::NONE,
            None
        )
        .unwrap(),
        [Value::U64(4)]
    );
    assert!(matches!(
        vm.run(
            &projected(true),
            FuncId(0),
            &[Value::Agg(root)],
            TraceMask::NONE,
            None
        )
        .unwrap_err()
        .trap,
        Trap::PayloadWorkExceeded {
            used: 0,
            requested: 4,
            ..
        }
    ));
    let returned = program(
        &[],
        &[ValueType::Agg],
        vec![ValueType::Unit, ValueType::Agg],
        |a, b| {
            let host = b.host_sig_for(
                "existing",
                HostSig {
                    args: vec![],
                    rets: vec![ValueType::Agg],
                },
            );
            a.host_call_sig_id(0, host, 0, &[], &[1]).ret(0, &[1]);
        },
    );
    assert_eq!(
        vm.run(&returned, FuncId(0), &[], TraceMask::NONE, None)
            .unwrap(),
        [Value::Agg(root)]
    );
}
