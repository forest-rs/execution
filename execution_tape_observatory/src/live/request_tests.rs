// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use super::*;
use execution_tape::{
    asm::{FunctionSig, ProgramBuilder},
    host::{HostContext, HostError, HostSig, SigHash},
    program::ValueType,
    source::{SourceDocument, SourceId, SourceLimits, SourceMapping},
    value::FuncId,
    verifier::VerifiedProgram,
    vm::{InvocationOptions, Limits, Vm},
};
use ratatui::crossterm::event::KeyModifiers;

#[derive(Default)]
struct Counter {
    calls: usize,
}
impl Host for Counter {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        _: &[ValueRef<'_>],
        rets: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        self.calls += 1;
        rets[0] = Value::I64(42);
        Ok(0)
    }
}
fn key(view: &mut LiveExplorer<'_, Counter>, code: KeyCode) {
    assert!(view.key(KeyEvent::new(code, KeyModifiers::NONE)));
}
fn program(parent: bool) -> (VerifiedProgram, FuncId, SourceMap) {
    let mut b = ProgramBuilder::new();
    b.set_program_name(if parent { "Caller code" } else { "Child code" });
    let entry = b
        .declare_function(FunctionSig {
            arg_types: if parent { vec![ValueType::Agg] } else { vec![] },
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let send = b.host_sig_for(
        "Object.send",
        HostSig {
            args: vec![ValueType::Agg],
            rets: vec![ValueType::I64],
        },
    );
    let mut a = b.function_asm(entry).unwrap();
    let result = a.alloc_register(ValueType::I64);
    let before = a.pc();
    if parent {
        a.host_call_sig_id(0, send, 0, &[1], &[result]);
    } else {
        a.const_i64(result, 42);
    }
    let after = a.pc();
    a.ret(0, &[result]);
    b.define_function(entry, a).unwrap();
    b.set_function_name(entry, if parent { "real_caller" } else { "method_body" })
        .unwrap();
    let p = b.build_verified().unwrap();
    let text = if parent {
        "receiver.increment()"
    } else {
        "return 42"
    };
    let sources = SourceMap::new(
        p.program(),
        vec![SourceDocument {
            name: if parent {
                "caller.expr".into()
            } else {
                "method.expr".into()
            },
            text: text.into(),
        }],
        [before, after]
            .map(|pc| SourceMapping {
                location: TraceLocation { func: entry, pc },
                document: SourceId(0),
                range: 0..text.len(),
            })
            .to_vec(),
        SourceLimits::default(),
    )
    .unwrap();
    (p, entry, sources)
}

#[test]
fn awaiting_host_is_explicit_stepping_waits_and_only_explicit_host_dispatch_has_effects() {
    let (p, entry, sources) = program(true);
    let mut vm = Vm::new(Counter::default(), Limits::default());
    let root = vm.aggregates_mut().tuple_new(vec![Value::I64(7)]).unwrap();
    let args = [Value::Agg(root)];
    let call = vm
        .start(
            &p,
            entry,
            &args,
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    let mut view = LiveExplorer::new(call).sources(&sources).unwrap();
    assert_eq!(view.step().unwrap(), RunStatus::AwaitingHost);
    let fuel = view.invocation.remaining_fuel();
    let text = view.render_text(140, 42).unwrap();
    assert!(text.contains("AWAITING HOST"));
    assert!(text.contains("Object.send"));
    assert!(text.contains("receiver.increment()"));
    assert!(text.contains("waiting for return"));
    assert_eq!(view.invocation.host().calls, 0);
    key(&mut view, KeyCode::Char('s'));
    key(&mut view, KeyCode::Char('c'));
    view.tick();
    assert_eq!(view.invocation.host().calls, 0);
    assert_eq!(view.invocation.remaining_fuel(), fuel);
    key(&mut view, KeyCode::Char('h'));
    assert_eq!(view.invocation.host().calls, 1);
    assert!(
        view.result().is_none(),
        "host dispatch does not execute the guest return"
    );
    assert!(
        view.render_text(140, 42)
            .unwrap()
            .contains("host instruction completed")
    );
    assert_eq!(view.step().unwrap(), RunStatus::Complete);
    let before = view.status.clone();
    key(&mut view, KeyCode::Char('h'));
    assert_eq!(
        view.status, before,
        "h without a request must not fabricate a host completion"
    );
    assert_eq!(view.result().unwrap().unwrap(), &[Value::I64(42)]);
}

#[test]
fn leaving_presentation_preserves_a_pending_invocation_for_external_completion() {
    let (p, entry, _) = program(true);
    let mut vm = Vm::new(Counter::default(), Limits::default());
    let root = vm.aggregates_mut().tuple_new(vec![]).unwrap();
    let args = [Value::Agg(root)];
    let call = vm
        .start(
            &p,
            entry,
            &args,
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    let mut view = LiveExplorer::new(call);
    view.step().unwrap();
    let mut call = view.into_invocation();
    call.pending_host_call()
        .unwrap()
        .complete(&mut [Value::I64(99)], Ok(0))
        .unwrap();
    assert_eq!(call.resume(u64::MAX).unwrap(), RunStatus::Complete);
    assert_eq!(call.result().unwrap().unwrap(), &[Value::I64(99)]);
    assert_eq!(call.host().calls, 0);
}

#[test]
fn external_caller_uses_its_actual_code_and_source_even_when_local_function_ids_collide() {
    let (caller, caller_entry, caller_sources) = program(true);
    let (callee, callee_entry, callee_sources) = program(false);
    assert_eq!(caller_entry, callee_entry);
    let mut parent_vm = Vm::new(Counter::default(), Limits::default());
    let arg = parent_vm
        .aggregates_mut()
        .tuple_new(vec![Value::I64(7)])
        .unwrap();
    let args = [Value::Agg(arg)];
    let mut parent = parent_vm
        .start(
            &caller,
            caller_entry,
            &args,
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    assert_eq!(parent.resume(u64::MAX).unwrap(), RunStatus::AwaitingHost);
    let request = parent.pending_host_call().unwrap();
    let mut vm = Vm::new(Counter::default(), Limits::default());
    vm.aggregates_mut()
        .tuple_new(vec![Value::I64(999)])
        .unwrap();
    let call = vm
        .start(&callee, callee_entry, &[], InvocationOptions::default())
        .unwrap();
    let mut view = LiveExplorer::new(call)
        .sources(&callee_sources)
        .unwrap()
        .requested_by(&request, Some(&caller_sources))
        .unwrap();
    assert_eq!(
        view.frames.len(),
        1,
        "external context is not a synthetic guest activation"
    );
    let text = view.render_text(160, 48).unwrap();
    assert!(text.contains("external caller"));
    assert!(text.contains("real_caller → host Object.send"));
    assert!(text.contains("method.expr"));
    assert!(text.contains("Child code"));
    view.focus = Focus::Stack;
    key(&mut view, KeyCode::Down);
    assert!(view.origin_selected);
    let text = view.render_text(160, 48).unwrap();
    assert!(text.contains("CALLER INSTRUCTIONS"));
    assert!(text.contains("caller.expr"));
    assert!(text.contains("receiver.increment()"));
    assert!(text.contains("host Object.send"));
    assert!(!text.contains("method.expr"));
    assert!(
        view.values
            .iter()
            .any(|v| v.contains("caller-owned aggregate"))
    );
    assert!(
        view.values
            .iter()
            .any(|v| v.starts_with("r2 · i64") && v.contains("unavailable")),
        "the caller's unreturned result is not a bank default"
    );
    assert!(view.value_targets.iter().all(Option::is_none));
    assert!(
        !view.values.iter().any(|v| v.contains("999")),
        "foreign handles are never read in the child's heap"
    );
    key(&mut view, KeyCode::Char('b'));
    assert!(view.breakpoints.is_empty());
    assert!(view.status.contains("another invocation"));
    key(&mut view, KeyCode::Char('h'));
    assert_eq!(
        view.invocation.host().calls,
        0,
        "h cannot dispatch an external ancestor's request"
    );
    key(&mut view, KeyCode::Char('g'));
    assert!(!view.showing_origin);
    assert!(view.render_text(160, 48).unwrap().contains("method.expr"));
    assert!(view.code.iter().any(|(_, text)| text.contains("42")));
    assert!(view.inspect_caller());
    assert!(view.showing_origin);
    key(&mut view, KeyCode::Char('g'));
    view.focus = Focus::Stack;
    key(&mut view, KeyCode::Down);
    view.step().unwrap(); // controls the child even with the parent selected
    assert!(!view.origin_selected);
    assert!(!view.showing_origin);
    view.step().unwrap();
    assert_eq!(view.result().unwrap().unwrap(), &[Value::I64(42)]);
    drop(view);
    request.complete(&mut [Value::I64(42)], Ok(0)).unwrap();
    assert_eq!(parent.resume(u64::MAX).unwrap(), RunStatus::Complete);
}

#[test]
fn stale_parent_sources_are_rejected_independently_of_the_child_source_map() {
    let (caller, entry, _) = program(true);
    let (callee, target, wrong_sources) = program(false);
    let mut parent_vm = Vm::new(Counter::default(), Limits::default());
    let root = parent_vm.aggregates_mut().tuple_new(vec![]).unwrap();
    let args = [Value::Agg(root)];
    let mut parent = parent_vm
        .start(
            &caller,
            entry,
            &args,
            InvocationOptions::default().yield_host_calls(),
        )
        .unwrap();
    parent.resume(1).unwrap();
    let request = parent.pending_host_call().unwrap();
    let mut vm = Vm::new(Counter::default(), Limits::default());
    let call = vm
        .start(&callee, target, &[], InvocationOptions::default())
        .unwrap();
    assert!(matches!(
        LiveExplorer::new(call).requested_by(&request, Some(&wrong_sources)),
        Err(SourceError::ProgramMismatch)
    ));
}
