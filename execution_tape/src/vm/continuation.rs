// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Suspended control and its exact code owners, independent of host and heap ownership.
//!
//! The execution lease still owns the heap checkpoint. Moving a control stack cannot grant it
//! permission to truncate shared storage: other suspended stacks may retain those values.

use super::*;
use alloc::borrow::Cow;
use alloc::sync::Arc;

/// Both forms retain the verified definition. Preserve a callable's original outer `Arc`
/// for code-owner observers without requiring that extra wrapper for raw entries.
pub(super) enum CodeOwner {
    Definition(VerifiedProgram),
    Original(Arc<VerifiedProgram>),
    RetainedInstance {
        program: Arc<VerifiedProgram>,
        instance: Instance,
    },
    Instance(Instance),
}
impl core::ops::Deref for CodeOwner {
    type Target = VerifiedProgram;
    fn deref(&self) -> &Self::Target {
        match self {
            Self::Definition(program) => program,
            Self::Original(program) => program,
            Self::RetainedInstance { program, .. } => program,
            Self::Instance(instance) => instance.program(),
        }
    }
}

impl CodeOwner {
    pub(super) fn instance(&self) -> Option<&Instance> {
        match self {
            Self::Instance(instance) | Self::RetainedInstance { instance, .. } => Some(instance),
            _ => None,
        }
    }
}

pub(super) struct Continuation<'a> {
    pub(super) program: CodeOwner,
    pub(super) entry: FuncId,
    pub(super) parent: Option<(ActivationId, TraceLocation)>,
    pub(super) delimiter: Option<invocation::delimited::Delimiter>,
    pub(super) structural_results: bool,
    pub(super) returned_at: Option<TraceLocation>,
    pub(super) args: Cow<'a, [Value]>,
    pub(super) ctx: Registers,
    pub(super) control: ExecutionControl,
    pub(super) trace: TraceState,
    pub(super) host_results: Option<crate::host::HostResults>,
    pub(super) register_state: observations::RegisterState,
    pub(super) outputs: Vec<Value>,
    pub(super) output_start: usize,
    pub(super) outcome: Option<Result<(), TrapInfo>>,
}

impl<'a> Continuation<'a> {
    pub(super) fn new(
        program: CodeOwner,
        entry: FuncId,
        args: Cow<'a, [Value]>,
        mask: TraceMask,
        control: ExecutionControl,
    ) -> Self {
        Self {
            program,
            entry,
            parent: None,
            delimiter: None,
            returned_at: None,
            structural_results: false,
            args,
            ctx: Registers::default(),
            control,
            trace: TraceState::new(mask),
            host_results: None,
            register_state: observations::RegisterState::default(),
            outputs: Vec::new(),
            output_start: 0,
            outcome: None,
        }
    }

    /// Detaching state never detaches a borrowed observer. Entry inputs become owned only
    /// when needed; already owned segment arguments move without copying payloads.
    pub(super) fn into_owned(self) -> Continuation<'static> {
        Continuation {
            args: Cow::Owned(self.args.into_owned()),
            program: self.program,
            entry: self.entry,
            parent: self.parent,
            delimiter: self.delimiter,
            structural_results: self.structural_results,
            returned_at: self.returned_at,
            ctx: self.ctx,
            control: self.control,
            trace: self.trace,
            host_results: self.host_results,
            register_state: self.register_state,
            outputs: self.outputs,
            output_start: self.output_start,
            outcome: self.outcome,
        }
    }

    pub(super) fn initialize<H: Host>(
        &mut self,
        vm: &mut Vm<H>,
        budget: &mut Budget,
        work: DataWork,
        observers: &mut Observers<'_>,
    ) -> Result<(), TrapInfo> {
        budget.select(&self.control);
        // Every entry path installs ownership before it calls this method. Leave an
        // interrupted admission poisoned, whether an observer panics before or after
        // frame setup; a later resume must neither retry entry nor execute guest code.
        self.control.waiting = Waiting::Transition(TraceLocation {
            func: self.entry,
            pc: 0,
        });
        if self.trace.mask != TraceMask::NONE && observers.trace.is_some() {
            self.control.enable_identity();
            self.trace.stack = Some(self.control.root_activation());
        }
        let tracing = self.trace.active();
        let mut trace = self.trace.observe(observers);
        trace.run_start(self.program.program(), self.entry, self.args.len());
        trace.values(self.program.program(), || ValueEvent::Entry {
            entry: self.entry,
            args: &self.args,
            aggregates: vm.memory.aggs(),
            limits: &vm.limits,
            work,
        });
        budget
            .charge_import(work)
            .map_err(|trap| self.ctx.trap(self.entry, 0, None, trap))?;
        if tracing {
            vm.initialize_run::<WithTracePolicy>(
                &mut self.ctx,
                budget,
                &self.program,
                self.entry,
                &self.args,
                &mut trace,
                self.program.instance(),
            )?;
        } else {
            vm.initialize_run::<NoTracePolicy>(
                &mut self.ctx,
                budget,
                &self.program,
                self.entry,
                &self.args,
                &mut trace,
                self.program.instance(),
            )?;
        }
        self.control.waiting = Waiting::Ready;
        Ok(())
    }
}

impl Continuation<'_> {
    /// The store is borrowed only for this scheduling quantum. It is not part of suspended
    /// control. The outer execution lease owns the transaction and selects its commit boundary.
    #[cfg(test)]
    pub(super) fn advance<H: Host>(
        &mut self,
        vm: &mut Vm<H>,
        budget: &mut Budget,
        quantum: u64,
    ) -> Result<Progress, TrapInfo> {
        self.advance_mode::<H, true>(vm, budget, quantum, &mut Observers::default())
    }

    pub(super) fn advance_mode<H: Host, const COOPERATIVE: bool>(
        &mut self,
        vm: &mut Vm<H>,
        budget: &mut Budget,
        quantum: u64,
        observers: &mut Observers<'_>,
    ) -> Result<Progress, TrapInfo> {
        budget.select(&self.control);
        self.control.remaining = quantum;
        let tracing = self.trace.active();
        let Observers {
            trace: sink,
            access,
        } = observers;
        let mut trace = TraceCtx {
            state: &mut self.trace,
            sink: sink.as_mut().map(|sink| &mut **sink as &mut dyn TraceSink),
        };
        let access = access
            .as_mut()
            .map(|sink| &mut **sink as &mut dyn AccessSink);
        if tracing {
            vm.execute_loop::<WithTracePolicy, COOPERATIVE>(
                &mut self.ctx,
                budget,
                &self.program,
                self.entry,
                &self.args,
                &mut self.outputs,
                &mut trace,
                access,
                &mut self.register_state,
                &mut self.control,
            )
        } else {
            vm.execute_loop::<NoTracePolicy, COOPERATIVE>(
                &mut self.ctx,
                budget,
                &self.program,
                self.entry,
                &self.args,
                &mut self.outputs,
                &mut trace,
                access,
                &mut (),
                &mut self.control,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asm::{FunctionSig, ProgramBuilder};
    use crate::control::arena::Resumptions;
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

    #[test]
    fn detached_state_owns_inputs_and_does_not_retain_a_borrowed_observer() {
        use core::cell::Cell;
        struct BorrowedObserver<'a>(&'a Cell<usize>);
        impl TraceSink for BorrowedObserver<'_> {}
        impl Drop for BorrowedObserver<'_> {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }
        let mut builder = ProgramBuilder::new();
        let entry = builder
            .declare_function(FunctionSig {
                arg_types: vec![ValueType::Str],
                ret_types: vec![ValueType::Str],
            })
            .unwrap();
        let mut body = builder.function_asm(entry).unwrap();
        body.ret(0, &[1]);
        builder.define_function(entry, body).unwrap();
        let program = builder.build_verified().unwrap();
        for already_owned in [false, true] {
            let dropped = Cell::new(0);
            let mut vm = Vm::new(NoHost, Limits::default());
            let mut budget = Budget::new(&vm.limits);
            let mut detached: Continuation<'static> = {
                let inputs = vec![Value::Str("retained input".into())];
                let Value::Str(text) = &inputs[0] else {
                    unreachable!()
                };
                let pointer = text.as_ptr();
                let args = if already_owned {
                    Cow::Owned(inputs)
                } else {
                    Cow::Borrowed(inputs.as_slice())
                };
                let mut observer = BorrowedObserver(&dropped);
                let mut observers = Observers {
                    trace: Some(BorrowedOrOwned::Borrowed(&mut observer)),
                    access: None,
                };
                let mut stack = Continuation::new(
                    CodeOwner::Definition(program.clone()),
                    entry,
                    args,
                    TraceMask::RUN,
                    ExecutionControl::start(false),
                );
                stack
                    .initialize(&mut vm, &mut budget, DataWork::default(), &mut observers)
                    .unwrap();
                let detached = stack.into_owned();
                let Value::Str(text) = &detached.args[0] else {
                    unreachable!()
                };
                assert_eq!(
                    text.as_ptr() == pointer,
                    already_owned,
                    "owned inputs move; borrowed inputs become independently owned"
                );
                detached
            };
            assert_eq!(
                dropped.get(),
                1,
                "saved state cannot extend the observer lease"
            );
            assert!(matches!(
                detached.advance(&mut vm, &mut budget, 1),
                Ok(Progress::Returned(_))
            ));
            assert_eq!(detached.outputs, [Value::Str("retained input".into())]);
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
        body.ret(0, &[tuple]);
        builder.define_function(function, body).unwrap();
        Arc::new(builder.build_verified().unwrap())
    }

    fn start(
        vm: &mut Vm<NoHost>,
        budget: &mut Budget,
        program: Arc<VerifiedProgram>,
    ) -> Continuation<'static> {
        let mut continuation = Continuation::new(
            CodeOwner::Original(program),
            FuncId(0),
            Cow::Owned(vec![]),
            TraceMask::NONE,
            ExecutionControl::start(true),
        );
        continuation
            .initialize(vm, budget, DataWork::default(), &mut Observers::default())
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
        assert_eq!(child.outputs, [Value::Agg(AggHandle(3))]);
        assert_eq!(
            vm.aggregates().tuple_get(AggHandle(1), 0).unwrap(),
            Value::Str("parent".into())
        );
        assert!(matches!(
            parent.advance(&mut vm, &mut budget, 10),
            Ok(Progress::Returned(_))
        ));
        assert_eq!(parent.outputs, [Value::Agg(AggHandle(1))]);

        // Only the execution owner knows the complete surviving root set. Both candidate
        // outputs are still private here; publication and remapping happen together.
        let mut roots = vec![parent.outputs.pop().unwrap(), child.outputs.pop().unwrap()];
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
            let Value::Agg(handle) = continuation.outputs[0] else {
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
        body.ret(0, &[answer]);
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
        let child_output = child.outputs[0].clone();
        Running {
            vm: &mut vm,
            observers: &mut Observers::default(),
            continuation: &mut parent,
            budget: &mut budget,
            checkpoint,
            parents: &mut Vec::new(),
            next_segment: &mut 1,
            resumptions: &mut Resumptions::default(),
        }
        .pending_host_call()
        .unwrap()
        .complete(&mut [child_output.clone()], Ok(0))
        .unwrap();
        assert!(matches!(
            parent.advance(&mut vm, &mut budget, 10),
            Ok(Progress::Returned(_))
        ));
        assert_eq!(parent.outputs, [child_output]);
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
                assert_eq!(budget.data.used.aggregate, 6);
                assert_eq!(budget.data.used.payload, 6);
            } else if limits.max_aggregate_work == 5 {
                assert!(matches!(
                    error.trap,
                    Trap::AggregateWorkExceeded {
                        used: 4,
                        requested: 2,
                        ..
                    }
                ));
                assert_eq!(budget.data.used.aggregate, 4);
            } else {
                assert!(matches!(
                    error.trap,
                    Trap::PayloadWorkExceeded {
                        used: 3,
                        requested: 3,
                        ..
                    }
                ));
                assert_eq!(budget.data.used.payload, 3);
            }
            let Value::Agg(handle) = first.outputs[0] else {
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
        let mut second = Continuation::new(
            CodeOwner::Original(program("second")),
            FuncId(0),
            Cow::Owned(vec![]),
            TraceMask::NONE,
            ExecutionControl::start(false),
        );
        let error = second
            .initialize(
                &mut vm,
                &mut budget,
                DataWork::default(),
                &mut Observers::default(),
            )
            .unwrap_err();
        assert!(matches!(error.trap, Trap::CallDepthExceeded));
        assert_eq!(budget.frames, 1);
        assert!(matches!(
            first.advance(&mut vm, &mut budget, 10),
            Ok(Progress::Returned(_))
        ));
    }

    fn checked_identity(kind: ValueType) -> Arc<VerifiedProgram> {
        use crate::asm::FunctionAbi;
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
        body.ret(0, &[1]);
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
            let tuple = vm
                .aggregate_builder()
                .tuple_new(vec![Value::I64(42)])
                .unwrap();
            let mut first = Continuation::new(
                CodeOwner::Original(checked_identity(ValueType::I64)),
                FuncId(0),
                Cow::Owned(vec![Value::Agg(tuple)]),
                TraceMask::NONE,
                ExecutionControl::start(false),
            );
            first
                .initialize(
                    &mut vm,
                    &mut budget,
                    DataWork::default(),
                    &mut Observers::default(),
                )
                .unwrap();
            let before = budget.contract_scratch.work_used();
            assert_eq!(before, 3);
            // The contract and heap IDs collide, but the second program requires bool.
            let mut second = Continuation::new(
                CodeOwner::Original(checked_identity(ValueType::Bool)),
                FuncId(0),
                Cow::Owned(vec![Value::Agg(tuple)]),
                TraceMask::NONE,
                ExecutionControl::start(false),
            );
            let error = second
                .initialize(
                    &mut vm,
                    &mut budget,
                    DataWork::default(),
                    &mut Observers::default(),
                )
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
        body.ret(0, &[]);
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

    #[test]
    fn pending_hosts_cannot_each_spend_the_same_data_allowance() {
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
        body.ret(0, &[answer]);
        builder.define_function(entry, body).unwrap();
        let program = Arc::new(builder.build_verified().unwrap());

        for limits in [
            Limits {
                max_aggregate_work: 3,
                ..Limits::default()
            },
            Limits {
                max_payload_work: 5,
                ..Limits::default()
            },
        ] {
            let mut vm = Vm::new(NoHost, limits.clone());
            let mut budget = Budget::new(&limits);
            let checkpoint = vm.memory.checkpoint();
            let mut first = start(&mut vm, &mut budget, Arc::clone(&program));
            let mut second = start(&mut vm, &mut budget, Arc::clone(&program));
            for stack in [&mut first, &mut second] {
                assert!(matches!(
                    stack.advance(&mut vm, &mut budget, 1),
                    Ok(Progress::AwaitingHost)
                ));
            }
            Running {
                vm: &mut vm,
                observers: &mut Observers::default(),
                continuation: &mut first,
                budget: &mut budget,
                checkpoint,
                parents: &mut Vec::new(),
                next_segment: &mut 1,
                resumptions: &mut Resumptions::default(),
            }
            .pending_host_call()
            .unwrap()
            .context()
            .tuple_new(vec![Value::Str("one".into())])
            .unwrap();
            let error = Running {
                vm: &mut vm,
                observers: &mut Observers::default(),
                continuation: &mut second,
                budget: &mut budget,
                checkpoint,
                parents: &mut Vec::new(),
                next_segment: &mut 1,
                resumptions: &mut Resumptions::default(),
            }
            .pending_host_call()
            .unwrap()
            .context()
            .tuple_new(vec![Value::Str("two".into())])
            .unwrap_err();
            let HostError::Custom(error) = error else {
                panic!("typed denial")
            };
            if limits.max_aggregate_work == 3 {
                assert!(matches!(
                    error.downcast_ref::<Trap>(),
                    Some(Trap::AggregateWorkExceeded {
                        used: 2,
                        requested: 2,
                        ..
                    })
                ));
            } else {
                assert!(matches!(
                    error.downcast_ref::<Trap>(),
                    Some(Trap::PayloadWorkExceeded {
                        used: 3,
                        requested: 3,
                        ..
                    })
                ));
            }
            assert_eq!(
                (budget.data.used.aggregate, budget.data.used.payload),
                (2, 3)
            );
            assert_eq!(
                vm.aggregates().len_u32(),
                0,
                "neither pending result was promoted"
            );
        }
    }
}
