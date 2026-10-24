// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Exact indirect dispatch and the additional cost of rich callable/value boundaries.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use execution_tape::asm::{Asm, FunctionAbi, ProgramBuilder};
use execution_tape::contract::Contract;
use execution_tape::host::{Host, HostContext, HostError, SigHash, ValueRef};
use execution_tape::program::ValueType;
use execution_tape::trace::TraceMask;
use execution_tape::value::{Closure, Value};
use execution_tape::vm::{ExecutionContext, Limits, Vm};
use std::{hint::black_box, time::Duration};

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
fn callables(c: &mut Criterion) {
    let mut group = c.benchmark_group("callable_run");
    for (label, closure, aggregate) in [
        ("function", false, false),
        ("closure", true, false),
        ("nested_value", false, true),
    ] {
        for count in [1, 1000] {
            for checked in [false, true] {
                let mut b = ProgramBuilder::new();
                let integer = b.contract(Contract::Kind(ValueType::I64)).unwrap();
                let kind = if aggregate {
                    ValueType::Agg
                } else {
                    ValueType::I64
                };
                let value_contract = if aggregate && checked {
                    b.contract(Contract::Tuple(&[Some(integer)])).unwrap()
                } else {
                    b.contract(Contract::Kind(kind)).unwrap()
                };
                let mut target = Asm::new();
                let mut args = vec![value_contract.into()];
                if closure {
                    target.set_register_types(vec![ValueType::Unit, ValueType::Agg, kind]);
                    target.ret(0, &[2]);
                    args.insert(0, ValueType::Agg.into());
                } else {
                    target.set_register_types(vec![ValueType::Unit, kind]);
                    target.ret(0, &[1]);
                }
                let target = b
                    .push_function_checked(
                        target,
                        FunctionAbi {
                            args,
                            rets: vec![value_contract.into()],
                        },
                    )
                    .unwrap();
                let callable_kind = if closure {
                    ValueType::Closure
                } else {
                    ValueType::Func
                };
                let callable = if checked {
                    let args = [value_contract];
                    let rets = [value_contract];
                    b.contract(if closure {
                        Contract::Closure {
                            args: &args,
                            rets: &rets,
                        }
                    } else {
                        Contract::Function {
                            args: &args,
                            rets: &rets,
                        }
                    })
                    .unwrap()
                } else {
                    b.contract(Contract::Kind(callable_kind)).unwrap()
                };
                let signature = b
                    .call_sig(FunctionAbi {
                        args: vec![value_contract.into()],
                        rets: vec![value_contract.into()],
                    })
                    .unwrap();
                let mut caller = Asm::new();
                caller.set_register_types(vec![ValueType::Unit, callable_kind, kind, kind]);
                for _ in 0..count {
                    caller.call_indirect(0, signature, 1, 0, &[2], &[3]);
                }
                caller.ret(0, &[3]);
                let entry = b
                    .push_function_checked(
                        caller,
                        FunctionAbi {
                            args: vec![callable.into(), value_contract.into()],
                            rets: vec![kind.into()],
                        },
                    )
                    .unwrap();
                let p = b.build_verified().unwrap();
                // This window explicitly includes cloning the owned input and dropping preparation.
                group.bench_function(
                    BenchmarkId::new(
                        format!(
                            "prepare_with_clone/{label}/{}",
                            if checked { "rich" } else { "kind_only" }
                        ),
                        count,
                    ),
                    |b| {
                        b.iter(|| {
                            black_box(
                                execution_tape::verifier::verify_program_owned(
                                    p.program().clone(),
                                    &execution_tape::verifier::VerifyConfig::default(),
                                )
                                .unwrap(),
                            )
                        });
                    },
                );
                let mut vm = Vm::new(NoHost, Limits::default());
                let value = if aggregate {
                    Value::Agg(vm.aggregates_mut().tuple_new(vec![Value::I64(42)]).unwrap())
                } else {
                    Value::I64(42)
                };
                let callee = if closure {
                    let env = vm.aggregates_mut().tuple_new(vec![]).unwrap();
                    Value::Closure(Closure { func: target, env })
                } else {
                    Value::Func(target)
                };
                let args = [callee, value.clone()];
                let mut output = Vec::new();
                let mut ctx = ExecutionContext::new();
                vm.run_with_ctx(
                    &mut ctx,
                    &p,
                    entry,
                    &args,
                    &mut output,
                    TraceMask::NONE,
                    None,
                    None,
                )
                .unwrap();
                assert_eq!(output, vec![value]);
                group.bench_function(
                    BenchmarkId::new(
                        format!("{label}/{}", if checked { "rich" } else { "kind_only" }),
                        count,
                    ),
                    |b| {
                        b.iter(|| {
                            output.clear();
                            vm.run_with_ctx(
                                &mut ctx,
                                &p,
                                entry,
                                black_box(&args),
                                &mut output,
                                TraceMask::NONE,
                                None,
                                None,
                            )
                            .unwrap();
                            black_box(&output);
                        });
                    },
                );
            }
        }
    }
    group.finish();
}
criterion_group! {name=benches; config=Criterion::default().warm_up_time(Duration::from_millis(300)).measurement_time(Duration::from_millis(1200)).sample_size(40); targets=callables}
criterion_main!(benches);
