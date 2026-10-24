// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The cost of enforcing a declared contract on shared immutable input data.
//! Same guest body/data in both cases; unrefined kinds impose no structural obligation.

use std::hint::black_box;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use execution_tape::asm::{Asm, FunctionAbi, ProgramBuilder};
use execution_tape::contract::Contract;
use execution_tape::host::{Host, HostContext, HostError, SigHash, ValueRef};
use execution_tape::program::ValueType;
use execution_tape::trace::TraceMask;
use execution_tape::value::Value;
use execution_tape::vm::{ExecutionContext, Limits, Vm};

#[derive(Debug)]
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

fn bench_contracts(c: &mut Criterion) {
    let mut group = c.benchmark_group("shared_contract_run");
    for depth in [1, 16, 256] {
        for checked in [false, true] {
            let mut builder = ProgramBuilder::new();
            let mut contract = builder.contract(Contract::Kind(ValueType::I64)).unwrap();
            for _ in 0..depth {
                contract = builder
                    .contract(Contract::Tuple(&[Some(contract), Some(contract)]))
                    .unwrap();
            }
            if !checked {
                contract = builder.contract(Contract::Kind(ValueType::Agg)).unwrap();
            }
            let mut body = Asm::new();
            body.set_register_types(vec![ValueType::Unit, ValueType::Agg]);
            body.ret(0, &[1]);
            let entry = builder
                .push_function_checked(
                    body,
                    FunctionAbi {
                        args: vec![contract.into()],
                        rets: vec![contract.into()],
                    },
                )
                .unwrap();
            let program = builder.build_verified().unwrap();
            let mut vm = Vm::new(NoHost, Limits::default());
            let mut value = Value::I64(42);
            for _ in 0..depth {
                value = Value::Agg(
                    vm.aggregates_mut()
                        .tuple_new(vec![value.clone(), value])
                        .unwrap(),
                );
            }
            let args = [value];
            let mut outputs = Vec::new();
            let mut context = ExecutionContext::new();
            vm.run_with_ctx(
                &mut context,
                &program,
                entry,
                &args,
                &mut outputs,
                TraceMask::NONE,
                None,
                None,
            )
            .unwrap();
            assert_eq!(outputs, args);
            assert_eq!(vm.aggregates().len_u32(), depth);
            // Walk one path to check the actual data without unfolding its exponential tree view.
            let mut leaf = outputs[0].clone();
            for _ in 0..depth {
                let Value::Agg(handle) = leaf else {
                    panic!("missing shared node");
                };
                leaf = vm.aggregates().tuple_get(handle, 0).unwrap();
            }
            assert_eq!(leaf, Value::I64(42));
            group.bench_with_input(
                BenchmarkId::new(if checked { "checked" } else { "kind_only" }, depth),
                &depth,
                |b, _| {
                    b.iter(|| {
                        outputs.clear();
                        vm.run_with_ctx(
                            &mut context,
                            &program,
                            entry,
                            black_box(&args),
                            &mut outputs,
                            TraceMask::NONE,
                            None,
                            None,
                        )
                        .unwrap();
                        black_box(&outputs);
                    });
                },
            );
        }
    }
    group.finish();
}
criterion_group! {
    name=benches;
    config=Criterion::default().warm_up_time(Duration::from_millis(300)).measurement_time(Duration::from_millis(1200)).sample_size(40);
    targets=bench_contracts
}
criterion_main!(benches);
