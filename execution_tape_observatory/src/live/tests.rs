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
use ratatui::{Terminal, backend::TestBackend, crossterm::event::KeyModifiers};

#[derive(Default)]
struct Counter {
    calls: usize,
}
impl Host for Counter {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        args: &[ValueRef<'_>],
        rets: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        self.calls += 1;
        rets[0] = args[0].to_value();
        Ok(0)
    }
}

fn program(trap: bool) -> (VerifiedProgram, FuncId, u32, SourceMap) {
    let mut b = ProgramBuilder::new();
    let echo = b.host_sig_for(
        "publish",
        HostSig {
            args: vec![ValueType::I64],
            rets: vec![ValueType::I64],
        },
    );
    let child = b
        .declare_function(FunctionSig {
            arg_types: vec![ValueType::I64],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let mut a = b.function_asm(child).unwrap();
    let result = a.alloc_register(ValueType::I64);
    a.i64_add(result, 1, 1).ret(0, &[result]);
    b.define_function(child, a).unwrap();
    b.set_function_name(child, "double").unwrap();
    let entry = b
        .declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::Agg],
        })
        .unwrap();
    let mut a = b.function_asm(entry).unwrap();
    let arg = a.alloc_register(ValueType::I64);
    let result = a.alloc_register(ValueType::I64);
    let tuple = a.alloc_register(ValueType::Agg);
    a.const_i64(arg, 21);
    let call = a.pc();
    a.call(0, child, 0, &[arg], &[result]);
    let publish = a.pc();
    a.host_call_sig_id(0, echo, 0, &[result], &[result]);
    if trap {
        a.trap(17);
    }
    a.tuple_new(tuple, &[result]).ret(0, &[tuple]);
    b.define_function(entry, a).unwrap();
    b.set_function_name(entry, "main").unwrap();
    let p = b.build_verified().unwrap();
    let sources = SourceMap::new(
        p.program(),
        vec![SourceDocument {
            name: "example.expr".into(),
            text: "double(21)\npublish(answer)".into(),
        }],
        vec![
            SourceMapping {
                location: TraceLocation {
                    func: entry,
                    pc: call,
                },
                document: SourceId(0),
                range: 0..10,
            },
            SourceMapping {
                location: TraceLocation {
                    func: entry,
                    pc: publish,
                },
                document: SourceId(0),
                range: 11..26,
            },
        ],
        SourceLimits::default(),
    )
    .unwrap();
    (p, entry, publish, sources)
}
fn key(view: &mut LiveExplorer<'_, Counter>, key: KeyCode) {
    assert!(view.key(KeyEvent::new(key, KeyModifiers::NONE)));
}
fn finish_motion(view: &mut LiveExplorer<'_, Counter>) {
    for _ in 0..20 {
        if !view.running() {
            return;
        }
        view.tick();
    }
    panic!("finite demo did not stop");
}

#[test]
fn live_frames_source_stepping_and_aggregate_contents_are_real() {
    let (p, entry, _, sources) = program(false);
    let mut vm = Vm::new(Counter::default(), Limits::default());
    vm.aggregates_mut()
        .tuple_new(vec![Value::Obj(execution_tape::value::Obj {
            host_type: execution_tape::program::HostTypeId(1),
            handle: execution_tape::value::ObjHandle(42),
        })])
        .unwrap();
    let invocation = vm
        .start(&p, entry, &[], InvocationOptions::default())
        .unwrap();
    let mut view = LiveExplorer::new(invocation).sources(&sources).unwrap();
    assert_eq!(view.invocation.host().calls, 0);
    assert!(view.render_text(120, 36).unwrap().contains("unavailable"));
    view.step().unwrap(); // const
    let root = view.frames[0].activation.clone();
    view.step().unwrap(); // call
    assert_eq!(view.frames.len(), 2);
    assert_ne!(view.frames[0].activation, root);
    view.selected_frame = 1;
    view.refresh();
    let text = view.render_text(120, 36).unwrap();
    assert!(text.contains("double(21)"));
    assert!(text.contains("waiting for return"));
    assert!(
        view.values
            .iter()
            .any(|v| v.starts_with("r2") && v.contains("unavailable"))
    );
    // Step out follows activation identity, regardless of the frame selected in the UI.
    key(&mut view, KeyCode::Char('u'));
    finish_motion(&mut view);
    assert_eq!(view.frames[0].activation, root);
    assert_eq!(view.invocation.host().calls, 0);
    assert!(view.values.iter().any(|v| v == "r2 · i64 = 42"));
    key(&mut view, KeyCode::Char('n')); // host call is atomic
    finish_motion(&mut view);
    assert_eq!(view.invocation.host().calls, 1);
    view.resume(100).unwrap();
    assert!(view.render_text(120, 36).unwrap().contains("RETURNED"));
    view.focus = Focus::Values;
    key(&mut view, KeyCode::Enter);
    assert!(view.render_text(120, 36).unwrap().contains("Item[0] = 42"));
    key(&mut view, KeyCode::Backspace);
    assert!(view.render_text(120, 36).unwrap().contains("Output[0]"));
    key(&mut view, KeyCode::Char('o'));
    assert!(
        view.values
            .iter()
            .any(|v| v.contains("object #42") && v.contains("aggregate @0 field[0]"))
    );
    assert!(
        view.render_text(120, 36)
            .unwrap()
            .contains("HOST OBJECT REFERENCES")
    );
    key(&mut view, KeyCode::Char('o'));
    let outputs = view.take_outputs().unwrap();
    drop(view);
    let [Value::Agg(tuple)] = outputs.as_slice() else {
        panic!("expected tuple")
    };
    assert_eq!(
        vm.aggregates().tuple_get(*tuple, 0).unwrap(),
        Value::I64(42)
    );
}

#[test]
fn breakpoints_stop_before_effects_and_step_over_stops_after_callee_return() {
    let (p, entry, publish, _) = program(false);
    let mut vm = Vm::new(Counter::default(), Limits::default());
    let invocation = vm
        .start(&p, entry, &[], InvocationOptions::default())
        .unwrap();
    let mut view = LiveExplorer::new(invocation);
    view.step().unwrap();
    let root = view.frames[0].activation.clone();
    key(&mut view, KeyCode::Char('n'));
    finish_motion(&mut view);
    assert_eq!(view.frames[0].activation, root);
    assert_eq!(view.frames[0].location.pc, publish);
    // Move back to entry in a fresh run; set a future instruction breakpoint through UI keys.
    drop(view);
    let invocation = vm
        .start(&p, entry, &[], InvocationOptions::default())
        .unwrap();
    let mut view = LiveExplorer::new(invocation);
    view.code_selected = view.code.iter().position(|i| i.0 == publish).unwrap();
    key(&mut view, KeyCode::Char('b'));
    key(&mut view, KeyCode::Char('c'));
    finish_motion(&mut view);
    assert_eq!(view.frames[0].location.pc, publish);
    assert_eq!(view.invocation.host().calls, 0);
    assert!(view.status.contains("breakpoint"));
    key(&mut view, KeyCode::Char('c')); // intentionally pass the current breakpoint once
    finish_motion(&mut view);
    assert!(view.result().unwrap().is_ok());
    assert_eq!(view.invocation.host().calls, 1);
}

#[test]
fn terminal_trap_has_no_fabricated_live_values_and_keeps_source_location() {
    let (p, entry, _, sources) = program(true);
    let mut vm = Vm::new(Counter::default(), Limits::default());
    let invocation = vm
        .start(&p, entry, &[], InvocationOptions::default())
        .unwrap();
    let mut view = LiveExplorer::new(invocation).sources(&sources).unwrap();
    let error = view.resume(100).unwrap_err();
    assert!(view.frames.is_empty());
    assert_eq!(
        view.location(),
        Some(TraceLocation {
            func: error.func,
            pc: error.pc
        })
    );
    let text = view.render_text(120, 36).unwrap();
    assert!(text.contains("TRAPPED"));
    assert!(text.contains("No active guest frames"));
    assert!(!text.contains("r2 · i64 ="));
    assert!(view.values[0].contains("discarded"));
    let calls = view.invocation.host().calls;
    assert_eq!(view.resume(100).unwrap_err().to_string(), error.to_string());
    assert_eq!(view.invocation.host().calls, calls);
}

#[test]
fn focus_uses_double_borders_and_color_and_small_terminal_reports_current_size() {
    let (p, entry, _, sources) = program(false);
    let mut vm = Vm::new(Counter::default(), Limits::default());
    let invocation = vm
        .start(&p, entry, &[], InvocationOptions::default())
        .unwrap();
    let mut view = LiveExplorer::new(invocation).sources(&sources).unwrap();
    let mut terminal = Terminal::new(TestBackend::new(120, 36)).unwrap();
    terminal.draw(|frame| view.draw(frame)).unwrap();
    let buffer = terminal.backend().buffer();
    assert!(
        buffer
            .content
            .iter()
            .any(|cell| cell.symbol() == "═" && cell.fg == GREEN)
    );
    assert!(buffer.content.iter().any(|cell| cell.fg == CYAN));
    key(&mut view, KeyCode::Tab);
    assert!(view.focus == Focus::Values);
    key(&mut view, KeyCode::BackTab);
    assert!(view.focus == Focus::Code);
    let text = view.render_text(70, 18).unwrap();
    assert!(text.contains("at least 80 × 20"));
    assert!(text.contains("current size: 70 × 18"));
    assert!(view.render_text(1, 1).is_err());
}

#[test]
fn join_declarations_cannot_create_unreachable_breakpoints_and_transfers_are_atomic() {
    let mut b = ProgramBuilder::new();
    let entry = b
        .declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64, ValueType::I64],
        })
        .unwrap();
    let mut a = b.function_asm(entry).unwrap();
    let x = a.alloc_register(ValueType::I64);
    let y = a.alloc_register(ValueType::I64);
    let join = a.block(&[x, y]);
    a.const_i64(x, 11)
        .const_i64(y, 22)
        .jmp(join.with_args(&[y, x]));
    let declaration = a.pc();
    a.place(join).unwrap();
    a.ret(0, &[x, y]);
    b.define_function(entry, a).unwrap();
    let p = b.build_verified().unwrap();
    let mut vm = Vm::new(Counter::default(), Limits::default());
    let invocation = vm
        .start(&p, entry, &[], InvocationOptions::default())
        .unwrap();
    let mut view = LiveExplorer::new(invocation);
    view.code_selected = view.code.iter().position(|i| i.0 == declaration).unwrap();
    key(&mut view, KeyCode::Char('b'));
    assert!(view.breakpoints.is_empty());
    assert!(view.status.contains("no execution step"));
    // Two constants and one simultaneous transfer. The declaration consumes no quantum.
    view.resume(3).unwrap();
    assert!(view.values.iter().any(|v| v == "r1 · i64 = 22"));
    assert!(view.values.iter().any(|v| v == "r2 · i64 = 11"));
    assert_eq!(view.step().unwrap(), RunStatus::Complete);
    assert_eq!(
        view.result().unwrap().unwrap(),
        &[Value::I64(22), Value::I64(11)]
    );
    key(&mut view, KeyCode::Char(' '));
    assert!(view.status.starts_with("RETURNED"));
}
