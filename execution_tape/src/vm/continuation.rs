// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Suspended control and its exact code/observer owners, independent of host and heap ownership.
//!
//! The execution lease still owns the heap checkpoint. Moving a control stack cannot grant it
//! permission to truncate shared storage: other suspended stacks may retain those values.

use super::*;
use alloc::borrow::Cow;
use alloc::sync::Arc;

/// One computation's execution state: its code, registers, control and outcome.
///
/// It owns everything and borrows nothing, and holds no observers, so the VM can keep it between
/// leases. The lease that drives it adds its inputs, if borrowed, and its observers.
pub(super) struct Stack {
    /// The entry unit's program, retained so suspended control never borrows the unit table.
    pub(super) program: Arc<VerifiedProgram>,
    pub(super) unit: UnitId,
    pub(super) entry: FuncId,
    /// The register file and control this stack runs on, lent by its VM.
    pub(super) ctx: Registers,
    pub(super) control: ExecutionControl,
    pub(super) host_results: Option<crate::host::HostResults>,
    pub(super) outputs: Vec<Value>,
    pub(super) outcome: Option<Result<(), TrapInfo>>,
}

impl Stack {
    /// Moves this stack out, leaving an empty one of the same code in its place.
    pub(super) fn take(&mut self) -> Self {
        let empty = Self {
            program: Arc::clone(&self.program),
            unit: self.unit,
            entry: self.entry,
            ctx: Registers::default(),
            control: ExecutionControl::default(),
            host_results: None,
            outputs: Vec::new(),
            outcome: None,
        };
        core::mem::replace(self, empty)
    }
}

/// A stack driven by one lease, with that lease's inputs and observers.
pub(super) struct Continuation<'a> {
    pub(super) stack: Stack,
    pub(super) args: Cow<'a, [Value]>,
    pub(super) trace: TraceCtx<'a>,
    pub(super) access: Option<BorrowedOrOwned<'a, dyn AccessSink + 'a>>,
    pub(super) register_state: observations::RegisterState,
}

impl<'a> Continuation<'a> {
    /// A stack identified by `stack` that runs on the VM's lent register file and control.
    pub(super) fn new(
        exec: &mut Exec,
        stack: u64,
        program: Arc<VerifiedProgram>,
        entry: FuncRef,
        args: Cow<'a, [Value]>,
        options: InvocationOptions<'a>,
    ) -> Self {
        let mut control = core::mem::take(&mut exec.control);
        control.start(stack, options.yield_host_calls, options.yield_suspensions);
        // A trap before the entry frame exists, such as a refused capture import, releases what
        // the stack holds; that must be nothing from an earlier lease.
        let mut ctx = core::mem::take(&mut exec.ctx);
        ctx.clear();
        Self {
            stack: Stack {
                program,
                unit: entry.unit,
                entry: entry.func,
                ctx,
                control,
                host_results: None,
                outputs: Vec::new(),
                outcome: None,
            },
            args,
            trace: TraceCtx::with_observer(options.mask.unwrap_or(TraceMask::NONE), options.trace),
            access: options.access,
            register_state: observations::RegisterState::default(),
        }
    }

    /// Returns the register file and control to the execution state they were taken from.
    pub(super) fn give_back(&mut self, exec: &mut Exec) {
        self.stack.ctx.release_payloads();
        exec.ctx = core::mem::take(&mut self.stack.ctx);
        exec.control = core::mem::take(&mut self.stack.control);
    }

    pub(super) fn initialize<H: Host>(
        &mut self,
        vm: &mut Vm<H>,
        budget: &mut Budget,
        work: DataWork,
    ) -> Result<(), TrapInfo> {
        budget.select(&self.stack.control);
        self.trace.run_start(
            self.stack.program.program(),
            FuncRef::new(self.stack.unit, self.stack.entry),
            self.args.len(),
        );
        self.trace
            .values(self.stack.program.program(), || ValueEvent::Entry {
                entry: FuncRef::new(self.stack.unit, self.stack.entry),
                args: &self.args,
                aggregates: vm.memory.aggs(),
                limits: &vm.limits,
                work,
                units: vm.units(),
            });
        budget
            .charge_import(work)
            .map_err(|trap| self.stack.ctx.trap(self.stack.entry, 0, None, trap))?;
        if self.trace.active() {
            vm.initialize_run::<WithTracePolicy>(
                &mut self.stack.ctx,
                budget,
                &self.stack.program,
                self.stack.unit,
                self.stack.entry,
                &self.args,
                &mut self.trace,
            )?;
        } else {
            vm.initialize_run::<NoTracePolicy>(
                &mut self.stack.ctx,
                budget,
                &self.stack.program,
                self.stack.unit,
                self.stack.entry,
                &self.args,
                &mut self.trace,
            )?;
        }
        Ok(())
    }
}

impl Continuation<'_> {
    /// The store is borrowed only for this scheduling quantum. It is not part of suspended
    /// control. The outer execution lease owns the transaction and selects its commit boundary.
    pub(super) fn advance<H: Host>(
        &mut self,
        vm: &mut Vm<H>,
        budget: &mut Budget,
        quantum: u64,
    ) -> Result<Progress, TrapInfo> {
        budget.select(&self.stack.control);
        self.stack.control.remaining = quantum;
        let access = self
            .access
            .as_mut()
            .map(|sink| &mut **sink as &mut dyn AccessSink);
        if self.trace.active() {
            vm.execute_loop::<WithTracePolicy, true>(
                &mut self.stack.ctx,
                budget,
                self.stack.entry,
                &self.args,
                &mut self.stack.outputs,
                &mut self.trace,
                access,
                &mut self.register_state,
                &mut self.stack.control,
            )
        } else {
            vm.execute_loop::<NoTracePolicy, true>(
                &mut self.stack.ctx,
                budget,
                self.stack.entry,
                &self.args,
                &mut self.stack.outputs,
                &mut self.trace,
                access,
                &mut (),
                &mut self.stack.control,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asm::{FunctionSig, ProgramBuilder, Reg};
    use crate::host::SigHash;

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
            panic!("these programs contain no host instructions")
        }
    }

    fn program(text: &str) -> Arc<VerifiedProgram> {
        let mut builder = ProgramBuilder::new();
        let text = builder.const_str(text);
        let function = builder
            .declare_function(FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::Agg],
            })
            .unwrap();
        let mut body = builder.function_asm(function).unwrap();
        let text_reg = body.alloc_register(ValueType::Str);
        let tuple = body.alloc_register(ValueType::Agg);
        body.const_pool(text_reg, text);
        // One unreachable allocation makes premature compaction observable.
        body.tuple_new(tuple, &[]);
        body.tuple_new(tuple, &[text_reg]);
        body.ret(Reg::EFFECT, &[tuple]);
        builder.define_function(function, body).unwrap();
        Arc::new(builder.build_verified().unwrap())
    }

    fn install(
        vm: &mut Vm<NoHost>,
        program: Arc<VerifiedProgram>,
    ) -> (Arc<VerifiedProgram>, FuncRef) {
        let unit = vm.install(Arc::clone(&program)).unwrap();
        (program, FuncRef::new(unit, FuncId(0)))
    }

    fn start(
        vm: &mut Vm<NoHost>,
        budget: &mut Budget,
        program: Arc<VerifiedProgram>,
    ) -> Continuation<'static> {
        let (program, entry) = install(vm, program);
        let mut continuation = Continuation::new(
            &mut Exec::default(),
            vm.next_stack(),
            program,
            entry,
            Cow::Owned(vec![]),
            InvocationOptions::default().yield_host_calls(),
        );
        continuation
            .initialize(vm, budget, DataWork::default())
            .unwrap();
        continuation
    }

    #[test]
    fn returning_control_does_not_compact_another_suspended_stacks_values() {
        let mut vm = Vm::new(NoHost, Limits::default());
        let mut budget = Budget::new(&vm.limits);
        let checkpoint = vm.memory.checkpoint();
        let mut parent = start(&mut vm, &mut budget, program("parent"));
        assert!(matches!(
            parent.advance(&mut vm, &mut budget, 3),
            Ok(Progress::Suspended)
        ));
        assert_eq!(vm.aggregates().len_u32(), 2);

        let mut child = start(&mut vm, &mut budget, program("child"));
        assert!(matches!(
            child.advance(&mut vm, &mut budget, 10),
            Ok(Progress::Returned(_))
        ));
        assert_eq!(vm.aggregates().len_u32(), 4);
        assert_eq!(child.stack.outputs, [Value::Agg(AggHandle(3))]);
        assert_eq!(
            vm.aggregates().tuple_get(AggHandle(1), 0).unwrap(),
            Value::Str("parent".into())
        );
        assert!(matches!(
            parent.advance(&mut vm, &mut budget, 10),
            Ok(Progress::Returned(_))
        ));
        assert_eq!(parent.stack.outputs, [Value::Agg(AggHandle(1))]);

        // Only the execution owner knows the complete surviving root set. Both candidate
        // outputs are still private here; publication and remapping happen together.
        let mut roots = vec![
            parent.stack.outputs.pop().unwrap(),
            child.stack.outputs.pop().unwrap(),
        ];
        vm.memory.commit(checkpoint, &mut roots).unwrap();
        assert_eq!(vm.aggregates().len_u32(), 2);
        for (root, expected) in roots.iter().zip(["parent", "child"]) {
            let Value::Agg(handle) = root else {
                panic!("expected tuple")
            };
            assert_eq!(
                vm.aggregates().tuple_get(*handle, 0).unwrap(),
                Value::Str(expected.into())
            );
        }
    }

    #[test]
    fn suspended_payload_sources_keep_their_exact_code_owner() {
        let mut vm = Vm::new(NoHost, Limits::default());
        let mut budget = Budget::new(&vm.limits);
        let mut parent = start(&mut vm, &mut budget, program("old revision"));
        // The register holds a borrowed constant source, not a materialized payload copy.
        assert!(matches!(
            parent.advance(&mut vm, &mut budget, 1),
            Ok(Progress::Suspended)
        ));
        let mut replacement = start(&mut vm, &mut budget, program("new revision"));
        assert!(matches!(
            replacement.advance(&mut vm, &mut budget, 10),
            Ok(Progress::Returned(_))
        ));
        assert!(matches!(
            parent.advance(&mut vm, &mut budget, 10),
            Ok(Progress::Returned(_))
        ));
        for (continuation, expected) in [(&parent, "old revision"), (&replacement, "new revision")]
        {
            let Value::Agg(handle) = continuation.stack.outputs[0] else {
                panic!("expected tuple")
            };
            assert_eq!(
                vm.aggregates().tuple_get(handle, 0).unwrap(),
                Value::Str(expected.into())
            );
        }
    }

    #[test]
    fn pending_protocol_borrows_control_and_store_without_an_owning_invocation() {
        use super::super::invocation::Running;
        use crate::host::HostSig;

        let mut builder = ProgramBuilder::new();
        let pause = builder.host_sig_for(
            "pause",
            HostSig {
                args: vec![],
                rets: vec![ValueType::Agg],
            },
        );
        let entry = builder
            .declare_function(FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::Agg],
            })
            .unwrap();
        let mut body = builder.function_asm(entry).unwrap();
        let [answer] = builder.emit_call(&mut body, pause, &[]).unwrap();
        body.ret(Reg::EFFECT, &[answer]);
        builder.define_function(entry, body).unwrap();

        let mut vm = Vm::new(NoHost, Limits::default());
        let mut budget = Budget::new(&vm.limits);
        let checkpoint = vm.memory.checkpoint();
        let mut parent = start(
            &mut vm,
            &mut budget,
            Arc::new(builder.build_verified().unwrap()),
        );
        assert!(matches!(
            parent.advance(&mut vm, &mut budget, 10),
            Ok(Progress::AwaitingHost)
        ));
        let mut child = start(&mut vm, &mut budget, program("shared result"));
        assert!(matches!(
            child.advance(&mut vm, &mut budget, 10),
            Ok(Progress::Returned(_))
        ));
        let child_output = child.stack.outputs[0].clone();
        Running::new(&mut vm, &mut parent, &mut budget, checkpoint)
            .pending_host_call()
            .unwrap()
            .complete(&mut [child_output.clone()], Ok(0))
            .unwrap();
        assert!(matches!(
            parent.advance(&mut vm, &mut budget, 10),
            Ok(Progress::Returned(_))
        ));
        assert_eq!(parent.stack.outputs, [child_output]);
        assert_eq!(
            vm.aggregates().len_u32(),
            2,
            "no import or intermediate compaction"
        );
    }

    #[test]
    fn independent_stacks_share_instruction_and_data_limits() {
        for limits in [
            Limits {
                fuel: 7,
                ..Limits::default()
            },
            Limits {
                max_aggregate_work: 5,
                ..Limits::default()
            },
            Limits {
                max_payload_work: 5,
                ..Limits::default()
            },
        ] {
            let mut budget = Budget::new(&limits);
            let mut vm = Vm::new(NoHost, limits.clone());
            let mut first = start(&mut vm, &mut budget, program("one"));
            assert!(matches!(
                first.advance(&mut vm, &mut budget, 10),
                Ok(Progress::Returned(_))
            ));
            let mut second = start(&mut vm, &mut budget, program("two"));
            let error = match second.advance(&mut vm, &mut budget, 10) {
                Err(error) => error,
                Ok(_) => panic!("the second stack replenished the shared limit"),
            };
            if limits.fuel == 7 {
                assert!(matches!(error.trap, Trap::FuelExceeded));
                assert_eq!(budget.fuel, 0);
                assert_eq!(budget.aggregate_work, 6);
                assert_eq!(budget.payload_work, 6);
            } else if limits.max_aggregate_work == 5 {
                assert!(matches!(
                    error.trap,
                    Trap::AggregateWorkExceeded {
                        used: 4,
                        requested: 2,
                        ..
                    }
                ));
                assert_eq!(budget.aggregate_work, 4);
            } else {
                assert!(matches!(
                    error.trap,
                    Trap::PayloadWorkExceeded {
                        used: 3,
                        requested: 3,
                        ..
                    }
                ));
                assert_eq!(budget.payload_work, 3);
            }
            let Value::Agg(handle) = first.stack.outputs[0] else {
                panic!("expected tuple")
            };
            assert_eq!(
                vm.aggregates().tuple_get(handle, 0).unwrap(),
                Value::Str("one".into())
            );
        }
    }

    #[test]
    fn suspended_stacks_count_toward_the_shared_call_depth() {
        let mut vm = Vm::new(
            NoHost,
            Limits {
                max_call_depth: 1,
                ..Limits::default()
            },
        );
        let mut budget = Budget::new(&vm.limits);
        let mut first = start(&mut vm, &mut budget, program("first"));
        assert!(matches!(
            first.advance(&mut vm, &mut budget, 1),
            Ok(Progress::Suspended)
        ));
        let (second_program, second_entry) = install(&mut vm, program("second"));
        let mut second = Continuation::new(
            &mut Exec::default(),
            vm.next_stack(),
            second_program,
            second_entry,
            Cow::Owned(vec![]),
            InvocationOptions::default(),
        );
        let error = second
            .initialize(&mut vm, &mut budget, DataWork::default())
            .unwrap_err();
        assert!(matches!(error.trap, Trap::CallDepthExceeded));
        assert_eq!(budget.frames, 1);
        assert!(matches!(
            first.advance(&mut vm, &mut budget, 10),
            Ok(Progress::Returned(_))
        ));
    }

    fn checked_identity(kind: ValueType) -> Arc<VerifiedProgram> {
        use crate::asm::{FunctionAbi, Reg};
        let mut builder = ProgramBuilder::new();
        let member = builder.contract(Contract::Kind(kind)).unwrap();
        let tuple = builder.contract(Contract::Tuple(&[Some(member)])).unwrap();
        let function = builder
            .declare_function(FunctionAbi {
                args: vec![tuple.into()],
                rets: vec![tuple.into()],
            })
            .unwrap();
        let mut body = builder.function_asm(function).unwrap();
        body.ret(Reg::EFFECT, &[Reg(1)]);
        builder.define_function(function, body).unwrap();
        Arc::new(builder.build_verified().unwrap())
    }

    #[test]
    fn changing_code_discards_local_proofs_without_resetting_contract_work() {
        for limit in [3, 100] {
            let mut vm = Vm::new(
                NoHost,
                Limits {
                    max_contract_work: limit,
                    ..Limits::default()
                },
            );
            let mut budget = Budget::new(&vm.limits);
            let tuple = vm.aggregates_mut().tuple_new(vec![Value::I64(42)]).unwrap();
            let mut first = {
                let (program, entry) = install(&mut vm, checked_identity(ValueType::I64));
                Continuation::new(
                    &mut Exec::default(),
                    vm.next_stack(),
                    program,
                    entry,
                    Cow::Owned(vec![Value::Agg(tuple)]),
                    InvocationOptions::default(),
                )
            };
            first
                .initialize(&mut vm, &mut budget, DataWork::default())
                .unwrap();
            let before = budget.contract_scratch.work_used();
            assert_eq!(before, 3);
            // The contract and heap IDs collide, but the second program requires bool.
            let mut second = {
                let (program, entry) = install(&mut vm, checked_identity(ValueType::Bool));
                Continuation::new(
                    &mut Exec::default(),
                    vm.next_stack(),
                    program,
                    entry,
                    Cow::Owned(vec![Value::Agg(tuple)]),
                    InvocationOptions::default(),
                )
            };
            let error = second
                .initialize(&mut vm, &mut budget, DataWork::default())
                .unwrap_err();
            if limit == 3 {
                assert!(matches!(error.trap, Trap::ContractWorkExceeded { .. }));
                assert_eq!(budget.contract_scratch.work_used(), before);
            } else {
                assert!(matches!(error.trap, Trap::ContractMismatch { .. }));
                assert!(budget.contract_scratch.work_used() > before);
            }
        }
    }

    #[test]
    fn independent_stacks_share_host_admission_limits() {
        use crate::host::HostSig;
        let mut builder = ProgramBuilder::new();
        let pause = builder.host_sig_for(
            "pause",
            HostSig {
                args: vec![],
                rets: vec![],
            },
        );
        let entry = builder
            .declare_function(FunctionSig {
                arg_types: vec![],
                ret_types: vec![],
            })
            .unwrap();
        let mut body = builder.function_asm(entry).unwrap();
        let [] = builder.emit_call(&mut body, pause, &[]).unwrap();
        body.ret(Reg::EFFECT, &[]);
        builder.define_function(entry, body).unwrap();
        let program = Arc::new(builder.build_verified().unwrap());
        let mut vm = Vm::new(
            NoHost,
            Limits {
                max_host_calls: 1,
                ..Limits::default()
            },
        );
        let mut budget = Budget::new(&vm.limits);
        let mut first = start(&mut vm, &mut budget, Arc::clone(&program));
        let mut second = start(&mut vm, &mut budget, program);
        assert!(matches!(
            first.advance(&mut vm, &mut budget, 1),
            Ok(Progress::AwaitingHost)
        ));
        let error = match second.advance(&mut vm, &mut budget, 1) {
            Err(error) => error,
            Ok(_) => panic!("host limit was replenished"),
        };
        assert!(matches!(error.trap, Trap::HostCallLimitExceeded));
        assert_eq!(budget.host_calls, 1);
    }
}
