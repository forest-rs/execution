// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Real capture boundaries live outside the dependency-free core and adapter crates.

#[path = "../src/footfall_demo.rs"]
mod demo;

use demo::{Behavior, DemoHost, ENTRY, GUEST_TRAP};
use execution_tape::host::{Host, HostContext, HostError, SigHash, ValueRef};
use execution_tape::opcode::Opcode;
use execution_tape::trace::{TraceMask, TraceSink};
use execution_tape::value::Value;
use execution_tape::vm::{Limits, Vm};
use execution_tape_footfall::FootfallSink;
use footfall::runtime::{Observation, Runtime, Snapshot};
use footfall::{InstrumentationLevel, LabelKey, RegisteredContext};

static PROGRAM: LabelKey = LabelKey::new("program");

fn balanced(snapshot: &Snapshot) {
    let mut stack = Vec::new();
    for event in snapshot.events() {
        match event.observation() {
            Observation::ScopeBegin { token, .. } => stack.push(*token),
            Observation::ScopeEnd { token } => assert_eq!(stack.pop(), Some(*token)),
            Observation::Message { name, args, .. }
                if matches!(
                    *name,
                    "execution_tape.scope_end"
                        | "execution_tape.scope_trap"
                        | "execution_tape.run_end"
                        | "execution_tape.run_trap"
                ) =>
            {
                assert_eq!(
                    args.get("scope_token")
                        .and_then(|value| value.as_unsigned()),
                    stack.last().map(|token| token.get())
                );
            }
            _ => {}
        }
    }
    assert!(stack.is_empty(), "all scopes close inner-to-outer");
}

fn count(snapshot: &Snapshot, wanted: &str) -> usize {
    snapshot
        .events()
        .iter()
        .filter(|event| match event.observation() {
            Observation::ScopeBegin { name, .. } | Observation::Message { name, .. } => {
                *name == wanted
            }
            _ => false,
        })
        .count()
}

// One test owns this binary's process-wide backend and its level transitions.
#[test]
fn captures_control_flow_host_work_failures_masks_and_loss() {
    let runtime = Runtime::builder()
        .event_capacity(1024)
        .install_global()
        .expect("one runtime per test binary");
    let context = RegisteredContext::new(&footfall::context!(PROGRAM = 7_u64));
    let program = demo::program();
    let mut vm = Vm::new(
        DemoHost {
            context: context.handle(),
            behavior: Behavior::Success,
        },
        Limits::default(),
    );
    let mut sink = FootfallSink::new(context.handle());
    let all = sink.mask() | TraceMask::INSTR;
    let arguments = [Value::Bool(true), Value::I64(41)];
    let start = runtime.snapshot().next_cursor();
    assert_eq!(
        vm.run(&program, ENTRY, &arguments, all, Some(&mut sink))
            .unwrap(),
        [Value::I64(42)]
    );
    let success = runtime.snapshot_since(start);
    balanced(&success);
    assert_eq!(success.window().missed(), 0);
    assert_eq!(count(&success, "execution_tape.run"), 1);
    assert_eq!(count(&success, "execution_tape.call"), 2);
    assert_eq!(count(&success, "execution_tape.host"), 1);
    assert_eq!(count(&success, "demo.host_work"), 1);
    assert_eq!(count(&success, "execution_tape.instr"), 6);
    assert_eq!(count(&success, "execution_tape.instr_end"), 6);
    assert_eq!(count(&success, "execution_tape.run_trap"), 0);
    let mut branch_target = None;
    let mut attempted_pc = None;
    for event in success.events() {
        match event.observation() {
            Observation::ScopeBegin { context_id, .. }
            | Observation::Message { context_id, .. } => {
                assert_eq!(context_id.get(), context.handle().get());
            }
            _ => {}
        }
        match event.observation() {
            Observation::Message { name, args, .. } if *name == "execution_tape.instr" => {
                if args.get("opcode").unwrap().as_unsigned() == Some(u64::from(Opcode::Br as u8)) {
                    attempted_pc = Some(args.get("pc").unwrap());
                }
            }
            Observation::Message { name, args, .. }
                if *name == "execution_tape.instr_end"
                    && args.get("pc") == attempted_pc
                    && args.get("func").and_then(|value| value.as_unsigned())
                        == Some(u64::from(ENTRY.0)) =>
            {
                branch_target = args.get("next_pc").and_then(|value| value.as_unsigned());
            }
            _ => {}
        }
    }
    let taken = branch_target.expect("branch reports its actual target");

    let start = runtime.snapshot().next_cursor();
    assert_eq!(
        vm.run(
            &program,
            ENTRY,
            &[Value::Bool(false), Value::I64(41)],
            all,
            Some(&mut sink),
        )
        .unwrap(),
        [Value::I64(0)]
    );
    let skipped = runtime.snapshot_since(start);
    balanced(&skipped);
    assert_eq!(count(&skipped, "execution_tape.host"), 0);
    let skipped_target = skipped.events().iter().find_map(|event| {
        if let Observation::Message { name, args, .. } = event.observation()
            && *name == "execution_tape.instr_end"
            && args.get("func").and_then(|value| value.as_unsigned()) == Some(u64::from(ENTRY.0))
            && args.get("pc").and_then(|value| value.as_unsigned()) == Some(0)
        {
            args.get("next_pc").and_then(|value| value.as_unsigned())
        } else {
            None
        }
    });
    assert!(skipped_target.is_some_and(|target| target != taken));

    for behavior in [Behavior::Failure, Behavior::InvalidReturn] {
        vm.host_mut().behavior = behavior;
        let start = runtime.snapshot().next_cursor();
        vm.run(&program, ENTRY, &arguments, all, Some(&mut sink))
            .unwrap_err();
        let trapped = runtime.snapshot_since(start);
        balanced(&trapped);
        assert_eq!(count(&trapped, "execution_tape.host"), 1);
        assert_eq!(count(&trapped, "execution_tape.scope_trap"), 3);
        assert_eq!(count(&trapped, "execution_tape.instr_trap"), 1);
        assert_eq!(count(&trapped, "execution_tape.run_trap"), 1);
        assert_eq!(count(&trapped, "execution_tape.run_end"), 0);
    }

    let start = runtime.snapshot().next_cursor();
    vm.run(&program, GUEST_TRAP, &[], all, Some(&mut sink))
        .unwrap_err();
    let trapped = runtime.snapshot_since(start);
    balanced(&trapped);
    assert_eq!(count(&trapped, "execution_tape.instr_trap"), 1);
    assert_eq!(count(&trapped, "execution_tape.scope_trap"), 1);

    // Profiling must not materialize arbitrary custom error payloads during scope unwinding.
    #[derive(Debug)]
    struct OpaqueFailure;
    impl std::fmt::Display for OpaqueFailure {
        fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            panic!("profiling must not format this payload");
        }
    }
    impl std::error::Error for OpaqueFailure {}
    struct CustomFailureHost;
    impl Host for CustomFailureHost {
        fn call(
            &mut self,
            _: &str,
            _: SigHash,
            _: &[ValueRef<'_>],
            _: &mut [Value],
            _: HostContext<'_, '_>,
        ) -> Result<u64, HostError> {
            Err(HostError::custom(OpaqueFailure))
        }
    }
    let mut custom = Vm::new(CustomFailureHost, Limits::default());
    let failure = custom
        .run(&program, ENTRY, &arguments, all, Some(&mut sink))
        .unwrap_err();
    assert!(matches!(
        failure.trap,
        execution_tape::vm::Trap::HostCallFailed {
            error: HostError::Custom(source), ..
        } if source.downcast_ref::<OpaqueFailure>().is_some()
    ));

    vm.host_mut().behavior = Behavior::Success;
    for mask in [
        TraceMask::CALL | TraceMask::HOST,
        TraceMask::HOST,
        TraceMask::CALL,
        TraceMask::INSTR,
        TraceMask::RUN,
        TraceMask::NONE,
    ] {
        let start = runtime.snapshot().next_cursor();
        vm.run(&program, ENTRY, &arguments, mask, Some(&mut sink))
            .unwrap();
        let snapshot = runtime.snapshot_since(start);
        balanced(&snapshot);
        assert_eq!(
            count(&snapshot, "execution_tape.run"),
            usize::from(mask.contains(TraceMask::RUN))
        );
        assert_eq!(
            count(&snapshot, "execution_tape.call"),
            2 * usize::from(mask.contains(TraceMask::CALL))
        );
        assert_eq!(
            count(&snapshot, "execution_tape.host"),
            usize::from(mask.contains(TraceMask::HOST))
        );
        assert_eq!(
            count(&snapshot, "execution_tape.instr"),
            6 * usize::from(mask.contains(TraceMask::INSTR))
        );

        vm.host_mut().behavior = Behavior::Failure;
        let start = runtime.snapshot().next_cursor();
        vm.run(&program, ENTRY, &arguments, mask, Some(&mut sink))
            .unwrap_err();
        let trapped = runtime.snapshot_since(start);
        balanced(&trapped);
        assert_eq!(
            count(&trapped, "execution_tape.scope_trap"),
            2 * usize::from(mask.contains(TraceMask::CALL))
                + usize::from(mask.contains(TraceMask::HOST))
        );
        assert_eq!(
            count(&trapped, "execution_tape.instr_trap"),
            usize::from(mask.contains(TraceMask::INSTR))
        );
        assert_eq!(
            count(&trapped, "execution_tape.run_trap"),
            usize::from(mask.contains(TraceMask::RUN))
        );
        vm.host_mut().behavior = Behavior::Success;
    }

    // Scope guards are thread-bound and close on sink drop, without fabricating success.
    struct PanicHost;
    impl Host for PanicHost {
        fn call(
            &mut self,
            _: &str,
            _: SigHash,
            _: &[ValueRef<'_>],
            _: &mut [Value],
            _: HostContext<'_, '_>,
        ) -> Result<u64, HostError> {
            panic!("deliberate host panic");
        }
    }
    let start = runtime.snapshot().next_cursor();
    let mut interrupted_sink = FootfallSink::new(context.handle());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut vm = Vm::new(PanicHost, Limits::default());
        let _ = vm.run(
            &program,
            ENTRY,
            &arguments,
            all,
            Some(&mut interrupted_sink),
        );
    }));
    assert!(result.is_err());
    drop(interrupted_sink);
    let interrupted = runtime.snapshot_since(start);
    balanced(&interrupted);
    assert_eq!(count(&interrupted, "execution_tape.run_end"), 0);
    assert_eq!(count(&interrupted, "execution_tape.run_trap"), 0);
    let start = runtime.snapshot().next_cursor();
    let mut replacement = FootfallSink::new(context.handle());
    vm.run(
        &program,
        ENTRY,
        &arguments,
        TraceMask::CALL | TraceMask::HOST,
        Some(&mut replacement),
    )
    .unwrap();
    balanced(&runtime.snapshot_since(start));

    runtime.set_level(InstrumentationLevel::Off);
    assert_eq!(sink.mask(), TraceMask::NONE);
    runtime.set_level(InstrumentationLevel::Counters);
    assert_eq!(sink.mask(), TraceMask::NONE);
    runtime.set_level(InstrumentationLevel::Scoped);
    assert_eq!(
        sink.mask(),
        TraceMask::RUN | TraceMask::CALL | TraceMask::HOST
    );
    runtime.set_level(InstrumentationLevel::Off);
    let start = runtime.snapshot().next_cursor();
    vm.run(&program, ENTRY, &arguments, all, Some(&mut sink))
        .unwrap();
    assert!(runtime.snapshot_since(start).events().is_empty());
    runtime.set_level(InstrumentationLevel::Scoped);

    // A level drop inside a host call must still close every scope opened at run entry.
    struct DisableDuringCall {
        runtime: &'static Runtime,
        inner: DemoHost,
    }
    impl Host for DisableDuringCall {
        fn call(
            &mut self,
            symbol: &str,
            hash: SigHash,
            args: &[ValueRef<'_>],
            rets: &mut [Value],
            ctx: HostContext<'_, '_>,
        ) -> Result<u64, HostError> {
            self.runtime.set_level(InstrumentationLevel::Off);
            assert_eq!(
                FootfallSink::new(self.inner.context).mask(),
                TraceMask::NONE
            );
            self.inner.call(symbol, hash, args, rets, ctx)
        }
    }
    let mut disabling_vm = Vm::new(
        DisableDuringCall {
            runtime,
            inner: DemoHost {
                context: context.handle(),
                behavior: Behavior::Success,
            },
        },
        Limits::default(),
    );
    let start = runtime.snapshot().next_cursor();
    let mask_at_entry = sink.mask();
    assert_eq!(
        disabling_vm
            .run(&program, ENTRY, &arguments, mask_at_entry, Some(&mut sink))
            .unwrap(),
        [Value::I64(42)]
    );
    let lowered = runtime.snapshot_since(start);
    assert!(count(&lowered, "execution_tape.host") > 0);
    balanced(&lowered);
    assert_eq!(sink.mask(), TraceMask::NONE);

    runtime.set_level(InstrumentationLevel::Scoped);
    let start = runtime.snapshot().next_cursor();
    let mask_at_entry = sink.mask();
    vm.run(&program, ENTRY, &arguments, mask_at_entry, Some(&mut sink))
        .unwrap();
    let resumed = runtime.snapshot_since(start);
    assert_eq!(count(&resumed, "execution_tape.run"), 1);
    balanced(&resumed);
    runtime.set_level(InstrumentationLevel::Trace);

    let start = runtime.snapshot().next_cursor();
    for _ in 0..100 {
        vm.run(&program, ENTRY, &arguments, all, Some(&mut sink))
            .unwrap();
    }
    let truncated = runtime.snapshot_since(start);
    assert!(truncated.window().missed() > 0);
    assert!(truncated.drops().evicted_events() > 0);
    assert!(truncated.events().len() <= 1024);
    let exported = footfall::export::to_chrome_trace_json(&truncated.into());
    assert!(
        exported.contains("footfall_"),
        "export preserves capture metadata"
    );
}
