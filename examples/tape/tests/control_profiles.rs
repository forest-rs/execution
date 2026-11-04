// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Physical scopes follow attached stacks; parked siblings retain only logical descriptions.

use execution_tape::{
    asm::{FunctionSig, ProgramBuilder},
    host::{Host, HostContext, HostError, SigHash, ValueRef},
    program::ValueType,
    trace::{ScopeKind, StackActivity, TraceContext, TraceMask, TraceOutcome, TraceSink},
    value::{FuncId, Value},
    verifier::VerifiedProgram,
    vm::{ActivationId, Limits, Vm},
};
use execution_tape_footfall::FootfallSink;
use execution_tape_tracy::ProfilingTraceSink;
use footfall::{
    ContextHandle,
    runtime::{Observation, Runtime},
};

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
        panic!("the fixture uses only guest control");
    }
}

/// Capture two siblings. Tail-resume the first, passing the second as its response; the
/// first discards its parked sibling and completes. No old root remains beneath it.
fn parked_siblings() -> (VerifiedProgram, FuncId) {
    let mut builder = ProgramBuilder::new();
    let computes_number = FunctionSig {
        arg_types: vec![],
        ret_types: vec![ValueType::I64],
    };
    let supplies_sibling = FunctionSig {
        arg_types: vec![],
        ret_types: vec![ValueType::Continuation],
    };
    let initial = builder.call_sig(computes_number.clone()).unwrap();
    let operation = builder
        .control_tag("sibling", supplies_sibling.clone())
        .unwrap();
    let protocol = builder.call_sig(supplies_sibling).unwrap();
    let response = builder
        .call_sig(FunctionSig {
            arg_types: vec![ValueType::Continuation],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let root = builder.declare_function(computes_number.clone()).unwrap();
    let worker = builder.declare_function(computes_number).unwrap();

    let mut body = builder.function_asm(worker).unwrap();
    let tag = body.alloc_register(ValueType::Tag);
    let answer = body.alloc_register(ValueType::I64);
    body.const_tag(tag, operation);
    let sibling = builder.emit_suspend(&mut body, tag, protocol, &[]).unwrap()[0];
    body.cont_drop(sibling)
        .const_i64(answer, 42)
        .ret(0, &[answer]);
    builder.define_function(worker, body).unwrap();

    let mut body = builder.function_asm(root).unwrap();
    let target = body.alloc_register(ValueType::Func);
    let tag = body.alloc_register(ValueType::Tag);
    body.const_func(target, worker).const_tag(tag, operation);
    let returned = builder.result_block(&mut body, initial).unwrap();
    let first = builder.handler_block(&mut body, tag, protocol).unwrap();
    let second = builder.handler_block(&mut body, tag, protocol).unwrap();
    let child = builder
        .emit_continuation(&mut body, target, initial)
        .unwrap();
    builder
        .emit_resume(
            &mut body,
            child,
            initial,
            &[],
            returned.label(),
            &[first.arm()],
        )
        .unwrap();
    body.place(first.label()).unwrap();
    let child = builder
        .emit_continuation(&mut body, target, initial)
        .unwrap();
    builder
        .emit_resume(
            &mut body,
            child,
            initial,
            &[],
            returned.label(),
            &[second.arm()],
        )
        .unwrap();
    body.place(second.label()).unwrap();
    builder
        .emit_tail_resume(
            &mut body,
            first.continuation(),
            response,
            &[second.continuation()],
        )
        .unwrap();
    body.place(returned.label()).unwrap();
    body.ret(0, returned.results());
    builder.define_function(root, body).unwrap();
    (builder.build_verified().unwrap(), root)
}

struct Profiles {
    footfall: FootfallSink,
    tracy: ProfilingTraceSink<'static>,
    started: Vec<ActivationId>,
    ended: Vec<ActivationId>,
    parked: Vec<ActivationId>,
    attached: Vec<ActivationId>,
}
impl Profiles {
    fn new() -> Self {
        Self {
            footfall: FootfallSink::new(ContextHandle::ROOT),
            tracy: ProfilingTraceSink::new(),
            started: vec![],
            ended: vec![],
            parked: vec![],
            attached: vec![],
        }
    }
    fn check(&self) {
        assert!(
            self.footfall.is_consistent(),
            "Footfall scope ownership diverged"
        );
        assert!(self.tracy.is_consistent(), "Tracy scope ownership diverged");
    }
}
impl TraceSink for Profiles {
    fn run_start(&mut self, context: TraceContext<'_>, entry: FuncId, args: usize) {
        self.started.push(context.stack().unwrap().clone());
        self.footfall.run_start(context, entry, args);
    }
    fn run_end(&mut self, context: TraceContext<'_>, outcome: TraceOutcome<'_>) {
        self.ended.push(context.stack().unwrap().clone());
        self.footfall.run_end(context, outcome);
        self.check();
    }
    fn stack_activity(&mut self, context: TraceContext<'_>, activity: StackActivity<'_>) {
        let id = context.stack().unwrap().clone();
        match activity {
            StackActivity::Parked => self.parked.push(id),
            StackActivity::Attached { parent } => {
                assert!(
                    parent.is_none(),
                    "tail attachment replaces the execution root"
                );
                self.attached.push(id);
            }
        }
        self.footfall.stack_activity(context, activity);
        self.tracy.stack_activity(context, activity);
        self.check();
    }
    fn scope_enter(
        &mut self,
        context: TraceContext<'_>,
        kind: ScopeKind,
        depth: usize,
        func: FuncId,
        pc: u32,
        span: Option<u64>,
    ) {
        self.footfall
            .scope_enter(context, kind, depth, func, pc, span);
        self.tracy.scope_enter(context, kind, depth, func, pc, span);
        self.check();
    }
    fn scope_exit(
        &mut self,
        context: TraceContext<'_>,
        kind: ScopeKind,
        depth: usize,
        func: FuncId,
        pc: u32,
        span: Option<u64>,
        outcome: TraceOutcome<'_>,
    ) {
        self.footfall
            .scope_exit(context, kind, depth, func, pc, span, outcome);
        self.tracy
            .scope_exit(context, kind, depth, func, pc, span, outcome);
        self.check();
    }
}

#[test]
fn tail_attachment_and_parked_sibling_cancellation_preserve_physical_scope_ownership() {
    let runtime = Runtime::builder()
        .event_capacity(2048)
        .install_global()
        .unwrap();
    let (program, entry) = parked_siblings();
    // Neither CONTROL nor instruction tracing is needed for structural scope delivery.
    for mask in [
        TraceMask::RUN,
        TraceMask::CALL,
        TraceMask::RUN | TraceMask::CALL,
    ] {
        let cursor = runtime.cursor();
        let mut profiles = Profiles::new();
        let mut vm = Vm::new(NoHost, Limits::default());
        assert_eq!(
            vm.run(&program, entry, &[], mask, Some(&mut profiles))
                .unwrap(),
            [Value::I64(42)]
        );
        profiles.check();
        assert_eq!(profiles.parked.len(), 2);
        assert_eq!(profiles.attached, profiles.parked[..1]);
        if mask.contains(TraceMask::RUN) {
            assert_eq!(
                profiles.started.len(),
                3,
                "attachment is not another run start"
            );
            assert_eq!(profiles.ended.len(), 3);
            for id in &profiles.started {
                assert!(profiles.ended.contains(id));
            }
        }
        drop(profiles);
        let snapshot = runtime.snapshot_since(cursor);
        assert_eq!(snapshot.window().missed(), 0);
        let mut physical = vec![];
        let mut run_segments = vec![];
        let mut starts = 0;
        for event in snapshot.events() {
            match event.observation() {
                Observation::ScopeBegin {
                    token, name, args, ..
                } => {
                    let segment = (*name == "execution_tape.run")
                        .then(|| args.get("segment").unwrap().as_unsigned().unwrap());
                    if let Some(segment) = segment {
                        run_segments.push(segment);
                    }
                    physical.push((*token, segment));
                    starts += 1;
                }
                Observation::ScopeEnd { token } => assert_eq!(
                    physical.pop().map(|(token, _)| token),
                    Some(*token),
                    "physical scopes must close inner to outer"
                ),
                Observation::Message { name, args, .. }
                    if *name == "execution_tape.stack_park" && mask.contains(TraceMask::RUN) =>
                {
                    let segment = args.get("segment").unwrap().as_unsigned().unwrap();
                    assert!(
                        !physical.iter().any(|(_, active)| *active == Some(segment)),
                        "parking closes that stack's physical interval"
                    );
                }
                Observation::Message { name, args, .. }
                    if *name == "execution_tape.stack_attach" && mask.contains(TraceMask::RUN) =>
                {
                    let segment = args.get("segment").unwrap().as_unsigned().unwrap();
                    assert!(
                        physical.iter().any(|(_, active)| *active == Some(segment)),
                        "attachment reopens the same logical stack"
                    );
                }
                _ => {}
            }
        }
        assert!(physical.is_empty());
        assert!(starts >= 4, "the resumed child reopens physical scopes");
        if mask.contains(TraceMask::RUN) {
            assert_eq!(
                run_segments,
                [0, 1, 2, 1],
                "only the resumed sibling gets another physical run interval"
            );
        }
    }
}

#[test]
#[cfg(panic = "unwind")]
fn interrupted_park_or_attachment_cannot_repeat_a_control_transfer() {
    use execution_tape::vm::{InvocationOptions, Trap};
    use std::{cell::Cell, rc::Rc};
    struct Interrupt {
        park: bool,
        notifications: Rc<Cell<usize>>,
    }
    impl TraceSink for Interrupt {
        fn stack_activity(&mut self, _: TraceContext<'_>, activity: StackActivity<'_>) {
            if self.park == matches!(activity, StackActivity::Parked) {
                self.notifications.set(self.notifications.get() + 1);
                panic!("interrupted structural observation");
            }
        }
    }
    let (program, entry) = parked_siblings();
    for park in [true, false] {
        let mut vm = Vm::new(NoHost, Limits::default());
        let notifications = Rc::new(Cell::new(0));
        let mut run = vm
            .start(
                &program,
                entry,
                &[],
                InvocationOptions::default().trace_owned(
                    TraceMask::RUN,
                    Interrupt {
                        park,
                        notifications: notifications.clone(),
                    },
                ),
            )
            .unwrap();
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                while run.resume(1000).unwrap() != execution_tape::vm::RunStatus::Complete {}
            }))
            .is_err()
        );
        for _ in 0..2 {
            assert!(matches!(
                run.resume(1000).unwrap_err().trap,
                Trap::GuestTransferAbandoned
            ));
        }
        drop(run);
        assert_eq!(
            notifications.get(),
            1,
            "interrupted transition is never retried"
        );
        assert_eq!(vm.aggregates().len_u32(), 0);
    }
}
