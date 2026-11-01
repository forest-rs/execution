// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Suspended control and its exact code/observer owners, independent of host and heap ownership.
//!
//! The execution lease still owns the heap checkpoint. Moving a control stack cannot grant it
//! permission to truncate shared storage: other suspended stacks may retain those values.

use super::*;
use alloc::borrow::Cow;
use alloc::sync::Arc;

pub(super) enum ProgramRef<'a> {
    Borrowed(&'a VerifiedProgram),
    Owned(Arc<VerifiedProgram>),
}
impl core::ops::Deref for ProgramRef<'_> {
    type Target = VerifiedProgram;
    fn deref(&self) -> &Self::Target {
        match self {
            Self::Borrowed(program) => program,
            Self::Owned(program) => program,
        }
    }
}

pub(super) struct Continuation<'a> {
    pub(super) program: ProgramRef<'a>,
    pub(super) entry: FuncId,
    pub(super) args: Cow<'a, [Value]>,
    pub(super) ctx: ExecutionContext,
    pub(super) control: ExecutionControl,
    pub(super) trace: TraceCtx<'a>,
    pub(super) access: Option<BorrowedOrOwned<'a, dyn AccessSink + 'a>>,
    pub(super) host_results: Option<crate::host::HostResults>,
    pub(super) register_state: observations::RegisterState,
    pub(super) outputs: Vec<Value>,
    pub(super) outcome: Option<Result<(), TrapInfo>>,
}

impl<'a> Continuation<'a> {
    pub(super) fn new(
        program: ProgramRef<'a>,
        entry: FuncId,
        args: Cow<'a, [Value]>,
        options: InvocationOptions<'a>,
    ) -> Self {
        Self {
            program,
            entry,
            args,
            ctx: ExecutionContext::new(),
            control: ExecutionControl::start(options.yield_host_calls),
            trace: TraceCtx::with_observer(options.mask.unwrap_or(TraceMask::NONE), options.trace),
            access: options.access,
            host_results: None,
            register_state: observations::RegisterState::default(),
            outputs: Vec::new(),
            outcome: None,
        }
    }

    pub(super) fn initialize<H: Host>(
        &mut self,
        vm: &mut Vm<H>,
        work: DataWork,
    ) -> Result<(), TrapInfo> {
        self.trace
            .run_start(self.program.program(), self.entry, self.args.len());
        self.trace
            .values(self.program.program(), || ValueEvent::Entry {
                entry: self.entry,
                args: &self.args,
                aggregates: vm.memory.aggs(),
                limits: &vm.limits,
                work,
            });
        work.check(&vm.limits)
            .map_err(|trap| self.ctx.trap(self.entry, 0, None, trap))?;
        if self.trace.active() {
            vm.initialize_run::<WithTracePolicy>(
                &mut self.ctx,
                &self.program,
                self.entry,
                &self.args,
                &mut self.trace,
            )?;
        } else {
            vm.initialize_run::<NoTracePolicy>(
                &mut self.ctx,
                &self.program,
                self.entry,
                &self.args,
                &mut self.trace,
            )?;
        }
        self.ctx.aggregate_work = work.aggregate;
        self.ctx.payload_work = work.payload;
        Ok(())
    }
}

impl Continuation<'_> {
    /// The store is borrowed only for this scheduling quantum. It is not part of suspended
    /// control. The outer execution lease owns the transaction and selects its commit boundary.
    pub(super) fn advance<H: Host>(
        &mut self,
        vm: &mut Vm<H>,
        quantum: u64,
    ) -> Result<Progress, TrapInfo> {
        self.control.remaining = quantum;
        let access = self
            .access
            .as_mut()
            .map(|sink| &mut **sink as &mut dyn AccessSink);
        if self.trace.active() {
            vm.execute_loop::<WithTracePolicy, true>(
                &mut self.ctx,
                &self.program,
                self.entry,
                &self.args,
                &mut self.outputs,
                &mut self.trace,
                access,
                &mut self.register_state,
                &mut self.control,
            )
        } else {
            vm.execute_loop::<NoTracePolicy, true>(
                &mut self.ctx,
                &self.program,
                self.entry,
                &self.args,
                &mut self.outputs,
                &mut self.trace,
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
        body.ret(0, &[tuple]);
        builder.define_function(function, body).unwrap();
        Arc::new(builder.build_verified().unwrap())
    }

    fn start(vm: &mut Vm<NoHost>, program: Arc<VerifiedProgram>) -> Continuation<'static> {
        let mut continuation = Continuation::new(
            ProgramRef::Owned(program),
            FuncId(0),
            Cow::Owned(vec![]),
            InvocationOptions::default().yield_host_calls(),
        );
        continuation.initialize(vm, DataWork::default()).unwrap();
        continuation
    }

    #[test]
    fn returning_control_does_not_compact_another_suspended_stacks_values() {
        let mut vm = Vm::new(NoHost, Limits::default());
        let checkpoint = vm.memory.checkpoint();
        let mut parent = start(&mut vm, program("parent"));
        assert!(matches!(
            parent.advance(&mut vm, 3),
            Ok(Progress::Suspended)
        ));
        assert_eq!(vm.aggregates().len_u32(), 2);

        let mut child = start(&mut vm, program("child"));
        assert!(matches!(
            child.advance(&mut vm, 10),
            Ok(Progress::Returned(_))
        ));
        assert_eq!(vm.aggregates().len_u32(), 4);
        assert_eq!(child.outputs, [Value::Agg(AggHandle(3))]);
        assert_eq!(
            vm.aggregates().tuple_get(AggHandle(1), 0).unwrap(),
            Value::Str("parent".into())
        );
        assert!(matches!(
            parent.advance(&mut vm, 10),
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
        let mut parent = start(&mut vm, program("old revision"));
        // The register holds a borrowed constant source, not a materialized payload copy.
        assert!(matches!(
            parent.advance(&mut vm, 1),
            Ok(Progress::Suspended)
        ));
        let mut replacement = start(&mut vm, program("new revision"));
        assert!(matches!(
            replacement.advance(&mut vm, 10),
            Ok(Progress::Returned(_))
        ));
        assert!(matches!(
            parent.advance(&mut vm, 10),
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
        let checkpoint = vm.memory.checkpoint();
        let mut parent = start(&mut vm, Arc::new(builder.build_verified().unwrap()));
        assert!(matches!(
            parent.advance(&mut vm, 10),
            Ok(Progress::AwaitingHost)
        ));
        let mut child = start(&mut vm, program("shared result"));
        assert!(matches!(
            child.advance(&mut vm, 10),
            Ok(Progress::Returned(_))
        ));
        let child_output = child.outputs[0].clone();
        Running::new(&mut vm, &mut parent, checkpoint)
            .pending_host_call()
            .unwrap()
            .complete(&mut [child_output.clone()], Ok(0))
            .unwrap();
        assert!(matches!(
            parent.advance(&mut vm, 10),
            Ok(Progress::Returned(_))
        ));
        assert_eq!(parent.outputs, [child_output]);
        assert_eq!(
            vm.aggregates().len_u32(),
            2,
            "no import or intermediate compaction"
        );
    }
}
