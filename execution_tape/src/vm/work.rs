// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Work quantities shared by invocation inspection and embedding accounting.

use super::DataWork;

/// Cumulative admitted work from an invocation, or a sum across independent invocations.
///
/// Units match [`super::Limits`]. This is work accounting, not live memory or elapsed time.
/// Checked arithmetic makes overflow and snapshots taken in the wrong order explicit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExecutionWork {
    /// Debited instruction and host extra fuel.
    pub fuel: u64,
    /// Admitted host instructions, including requests still waiting for completion.
    pub host_calls: u64,
    /// Charged value/contract checks and child edges, including entry and failed checks.
    pub contract: u64,
    /// Aggregate construction/import and payload work, including pending host staging.
    pub data: DataWork,
}

impl ExecutionWork {
    /// Adds independent work quantities, returning `None` if any counter overflows.
    #[must_use]
    pub fn checked_add(self, other: Self) -> Option<Self> {
        Some(Self {
            fuel: self.fuel.checked_add(other.fuel)?,
            host_calls: self.host_calls.checked_add(other.host_calls)?,
            contract: self.contract.checked_add(other.contract)?,
            data: DataWork {
                aggregate: self.data.aggregate.checked_add(other.data.aggregate)?,
                payload: self.data.payload.checked_add(other.data.payload)?,
            },
        })
    }

    /// Subtracts an earlier snapshot, returning `None` if any counter regresses.
    /// Snapshots must describe the same invocation; numeric counters cannot establish identity.
    #[must_use]
    pub fn checked_sub(self, earlier: Self) -> Option<Self> {
        Some(Self {
            fuel: self.fuel.checked_sub(earlier.fuel)?,
            host_calls: self.host_calls.checked_sub(earlier.host_calls)?,
            contract: self.contract.checked_sub(earlier.contract)?,
            data: DataWork {
                aggregate: self.data.aggregate.checked_sub(earlier.data.aggregate)?,
                payload: self.data.payload.checked_sub(earlier.data.payload)?,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        asm::{FunctionAbi, ProgramBuilder},
        contract::Contract,
        host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef},
        program::ValueType,
        trace::TraceMask,
        value::{FuncId, Value},
        vm::{InvocationOptions, Limits, RunStatus, Vm},
    };
    use alloc::{vec, vec::Vec};

    struct Producer;
    impl Host for Producer {
        fn call(
            &mut self,
            _: &str,
            _: SigHash,
            _: &[ValueRef<'_>],
            rets: &mut [Value],
            mut ctx: HostContext<'_, '_>,
        ) -> Result<u64, HostError> {
            rets[0] = Value::Agg(ctx.tuple_new(vec![Value::Str("hello".into())])?);
            Ok(7)
        }
    }

    fn program() -> crate::verifier::VerifiedProgram {
        let mut b = ProgramBuilder::new();
        let string = b.contract(Contract::Kind(ValueType::Str)).unwrap();
        let tuple = b.contract(Contract::Tuple(&[Some(string)])).unwrap();
        let function = b
            .declare_function(FunctionAbi {
                args: vec![],
                rets: vec![tuple.into()],
            })
            .unwrap();
        let produce = b.host_sig_for(
            "produce",
            HostSig {
                args: vec![],
                rets: vec![ValueType::Agg],
            },
        );
        let mut body = b.function_asm(function).unwrap();
        let [value] = b.emit_call(&mut body, produce, &[]).unwrap();
        body.ret(0, &[value]);
        b.define_function(function, body).unwrap();
        b.build_verified().unwrap()
    }

    #[test]
    fn staging_promotion_and_success_do_not_double_count_work() {
        let program = program();
        let mut vm = Vm::new(Producer, Limits::default());
        let mut invocation = vm
            .start(
                &program,
                FuncId(0),
                &[],
                InvocationOptions::default().yield_host_calls(),
            )
            .unwrap();
        assert_eq!(invocation.work(), ExecutionWork::default());
        assert_eq!(invocation.resume(10).unwrap(), RunStatus::AwaitingHost);
        assert_eq!(invocation.work().host_calls, 1);
        let before = invocation.work();
        let mut request = invocation.pending_host_call().unwrap();
        let aggregate = request
            .context()
            .tuple_new(vec![Value::Str("hello".into())])
            .unwrap();
        drop(request);
        let staged = invocation.work();
        assert_eq!(
            staged.data,
            DataWork {
                aggregate: 2,
                payload: 5
            }
        );
        assert_eq!(staged.checked_sub(before).unwrap().data, staged.data);
        invocation
            .pending_host_call()
            .unwrap()
            .complete(&mut [Value::Agg(aggregate)], Ok(7))
            .unwrap();
        assert_eq!(invocation.work().data, staged.data);
        assert_eq!(invocation.resume(10).unwrap(), RunStatus::Complete);
        let work = invocation.work();
        assert_eq!(work.fuel, 9);
        assert_eq!(work.host_calls, 1);
        assert!(work.contract > 0);
        assert_eq!(work.data, staged.data);
        assert_eq!(invocation.resume(10).unwrap(), RunStatus::Complete);
        assert_eq!(invocation.work(), work);
        drop(invocation);
        // The ordinary unobserved path remains usable without a work observer.
        vm.run(&program, FuncId(0), &[], TraceMask::NONE, None)
            .unwrap();
    }

    #[test]
    fn failed_return_checks_preserve_admitted_work_and_clipped_fuel() {
        let program = program();
        let mut vm = Vm::new(
            Producer,
            Limits {
                fuel: 3,
                ..Limits::default()
            },
        );
        let mut invocation = vm
            .start(
                &program,
                FuncId(0),
                &[],
                InvocationOptions::default().yield_host_calls(),
            )
            .unwrap();
        invocation.resume(1).unwrap();
        let error = invocation
            .pending_host_call()
            .unwrap()
            .complete(&mut [Value::I64(1)], Ok(100));
        assert!(error.is_err());
        let work = invocation.work();
        assert_eq!(
            work.fuel, 3,
            "fuel reports the actual debit, not the host's request"
        );
        assert_eq!(work.host_calls, 1);
        assert_eq!(work.data, DataWork::default());
        assert!(invocation.resume(10).is_err());
        assert_eq!(invocation.work(), work);
    }

    #[test]
    fn work_arithmetic_checks_every_dimension() {
        let unit = ExecutionWork {
            fuel: 1,
            host_calls: 1,
            contract: 1,
            data: DataWork {
                aggregate: 1,
                payload: 1,
            },
        };
        let maxima: Vec<_> = [
            ExecutionWork {
                fuel: u64::MAX,
                ..Default::default()
            },
            ExecutionWork {
                host_calls: u64::MAX,
                ..Default::default()
            },
            ExecutionWork {
                contract: u64::MAX,
                ..Default::default()
            },
            ExecutionWork {
                data: DataWork {
                    aggregate: u64::MAX,
                    payload: 0,
                },
                ..Default::default()
            },
            ExecutionWork {
                data: DataWork {
                    aggregate: 0,
                    payload: u64::MAX,
                },
                ..Default::default()
            },
        ]
        .into();
        for maximum in maxima {
            assert!(maximum.checked_add(unit).is_none());
            assert!(ExecutionWork::default().checked_sub(maximum).is_none());
        }
        assert_eq!(
            unit.checked_add(unit).unwrap().checked_sub(unit),
            Some(unit)
        );
    }
}
