// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use super::*;
use execution_tape::asm::{Asm, FunctionSig, ProgramBuilder};
use execution_tape::host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef};
use execution_tape::program::ValueType;
use execution_tape::trace::TraceSink;
use execution_tape::value::{FuncId, Value};
use execution_tape::vm::{Limits, Vm};
use execution_tape_recording::{RecordLimits, RecordedValue, Recorder};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct Echo {
    effects: Arc<AtomicUsize>,
    error: Option<HostError>,
}
impl Host for Echo {
    fn call(
        &mut self,
        _: &str,
        _: SigHash,
        args: &[ValueRef<'_>],
        rets: &mut [Value],
        _: HostContext<'_, '_>,
    ) -> Result<u64, HostError> {
        self.effects.fetch_add(1, Ordering::Relaxed);
        rets[0] = args[0].to_value();
        self.error.clone().map_or(Ok(0), Err)
    }
}

fn recording(limits: RecordLimits, error: Option<HostError>) -> (Recording, Arc<AtomicUsize>) {
    recording_with_calls(limits, error, true)
}

fn recording_with_calls(
    limits: RecordLimits,
    error: Option<HostError>,
    calls: bool,
) -> (Recording, Arc<AtomicUsize>) {
    recording_with_options(limits, error, calls, false)
}

fn recording_with_options(
    limits: RecordLimits,
    error: Option<HostError>,
    calls: bool,
    instructions: bool,
) -> (Recording, Arc<AtomicUsize>) {
    let mut builder = ProgramBuilder::new();
    let import = builder.host_sig_for(
        "echo",
        HostSig {
            args: vec![ValueType::Agg],
            rets: vec![ValueType::Agg],
        },
    );
    let mut body = Asm::new();
    body.set_register_types(vec![
        ValueType::Unit,
        ValueType::Agg,
        ValueType::Agg,
        ValueType::I64,
        ValueType::I64,
        ValueType::I64,
        ValueType::Bool,
    ]);
    let head = body.label();
    let exit = body.label();
    body.const_i64(3, 0)
        .const_i64(4, 3)
        .const_i64(5, 1)
        .jmp(head);
    body.place(head).unwrap();
    body.tuple_new(2, &[3, 1])
        .host_call_sig_id(0, import, 0, &[2], &[2]);
    body.i64_add(3, 3, 5).i64_lt(6, 3, 4).br(6, head, exit);
    body.place(exit).unwrap();
    body.ret(0, &[2]);
    builder
        .push_function_checked(
            body,
            FunctionSig {
                arg_types: vec![ValueType::Agg],
                ret_types: vec![ValueType::Agg],
            },
        )
        .unwrap();
    let program = Arc::new(builder.build_verified().unwrap());
    let effects = Arc::new(AtomicUsize::new(0));
    let mut vm = Vm::new(
        Echo {
            effects: Arc::clone(&effects),
            error,
        },
        Limits::default(),
    );
    let root = vm
        .aggregates_mut()
        .tuple_new(vec![Value::Str("line\n\u{1b}[2J".into()), Value::I64(42)])
        .unwrap();
    let mut recorder = Recorder::new(Arc::clone(&program), limits);
    if instructions {
        let source =
            execution_tape::disasm::disassemble_function(program.program(), FuncId(0)).unwrap();
        let watches = source
            .instrs()
            .map(|i| execution_tape_recording::RegisterWatch {
                location: execution_tape::trace::TraceLocation {
                    func: FuncId(0),
                    pc: i.pc(),
                },
                registers: i.reads().chain(i.writes()).collect(),
            })
            .collect();
        recorder = recorder.with_instructions(execution_tape_recording::InstructionCapture {
            max_steps: 1000,
            watches,
        });
    }
    let _ = vm.run(
        &program,
        FuncId(0),
        &[Value::Agg(root)],
        if calls {
            recorder.mask()
        } else {
            execution_tape::trace::TraceMask::RUN | execution_tape::trace::TraceMask::VALUES
        },
        Some(&mut recorder),
    );
    (recorder.finish(), effects)
}

fn press(view: &mut View<'_>, code: KeyCode) -> bool {
    view.key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn text(view: &mut View<'_>, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| render::draw(frame, view)).unwrap();
    let mut output = String::new();
    for y in 0..height {
        for x in 0..width {
            output.push_str(terminal.backend().buffer()[(x, y)].symbol());
        }
        output.push('\n');
    }
    output
}

#[test]
fn timeline_values_and_source_follow_real_boundary_ordinals() {
    let (record, effects) = recording(RecordLimits::default(), None);
    let mut view = View::new(&record);
    view.select(0);
    assert_eq!(view.calls.len(), 3);
    let location = view.calls[0].location;
    assert!(view.calls.iter().all(|call| call.location == location));
    press(&mut view, KeyCode::Down);
    assert_eq!(view.location(), Some(location));
    assert!(text(&mut view, 120, 36).contains("#0000  echo"));
    press(&mut view, KeyCode::Enter);
    assert!(matches!(
        view.values().next().unwrap().2,
        RecordedValue::I64(0)
    ));
    assert!(text(&mut view, 120, 36).contains("created during run"));
    press(&mut view, KeyCode::BackTab); // event focus again
    press(&mut view, KeyCode::Down);
    assert_eq!(view.selected, 2);
    press(&mut view, KeyCode::Enter);
    assert!(matches!(
        view.values().next().unwrap().2,
        RecordedValue::I64(1)
    ));
    let shown = text(&mut view, 120, 36);
    assert!(shown.contains("#0001  echo"));
    let source_row = shown
        .lines()
        .find(|line| line.contains(&format!("▸ {:06}", location.pc)))
        .unwrap();
    assert!(source_row.contains("r2 = host echo(r2)"));
    press(&mut view, KeyCode::BackTab);
    press(&mut view, KeyCode::End);
    let final_location = record.outcome_location().unwrap();
    assert_eq!(view.location(), Some(final_location));
    let shown = text(&mut view, 120, 36);
    let source_row = shown
        .lines()
        .find(|line| line.contains(&format!("▸ {:06}", final_location.pc)))
        .unwrap();
    assert!(source_row.contains("return r2"));
    press(&mut view, KeyCode::Char('r'));
    assert!(view.replay.starts_with("MATCHED"));
    assert_eq!(
        effects.load(Ordering::Relaxed),
        3,
        "fresh replay must not repeat live effects"
    );
}

#[test]
fn aggregate_navigation_escapes_control_text_and_clamps_selection() {
    let (record, _) = recording(RecordLimits::default(), None);
    let mut view = View::new(&record);
    view.select(0);
    press(&mut view, KeyCode::Enter);
    let shown = text(&mut view, 120, 36);
    assert!(shown.contains("present at start"));
    assert!(shown.contains("line\\n\\u{1b}[2J"));
    assert!(!shown.contains('\u{1b}'));
    for _ in 0..10 {
        press(&mut view, KeyCode::Down);
    }
    assert_eq!(view.value_selected, 1);
    press(&mut view, KeyCode::Backspace);
    assert!(view.path.is_empty());
    assert_eq!(view.value_selected, 0);
    assert!(!press(&mut view, KeyCode::Char('q')));
    assert!(!view.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));
}

#[test]
fn typed_host_failure_is_visible_and_replayable_without_formatting_payload() {
    struct Opaque;
    impl std::fmt::Debug for Opaque {
        fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            panic!("opaque Debug")
        }
    }
    impl std::fmt::Display for Opaque {
        fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            panic!("opaque Display")
        }
    }
    impl std::error::Error for Opaque {}
    let (record, effects) = recording(RecordLimits::default(), Some(HostError::custom(Opaque)));
    record.status().unwrap();
    let mut view = View::new(&record);
    press(&mut view, KeyCode::Down);
    assert!(text(&mut view, 120, 36).contains("typed host failure"));
    press(&mut view, KeyCode::End);
    assert_eq!(view.location(), record.outcome_location());
    assert!(text(&mut view, 120, 36).contains("typed host failure"));
    press(&mut view, KeyCode::Char('r'));
    assert!(view.replay.contains("MATCHED trap"));
    assert_eq!(effects.load(Ordering::Relaxed), 1);
}

#[test]
fn partial_captures_and_small_screens_do_not_claim_success() {
    let (record, _) = recording(
        RecordLimits {
            values: 0,
            ..RecordLimits::default()
        },
        None,
    );
    assert!(record.status().is_err());
    assert!(record.outcome_location().is_none());
    let mut view = View::new(&record);
    press(&mut view, KeyCode::Char('r'));
    assert!(view.replay.starts_with("REPLAY FAILED"));
    let shown = text(&mut view, 120, 36);
    assert!(shown.contains("PARTIAL"));
    assert!(shown.contains("replay unavailable"));
    assert!(!shown.contains("MATCHED"));
    for (width, height) in [(40, 12), (79, 24), (100, 19)] {
        let shown = render_text(&record, width, height).unwrap();
        assert!(shown.contains("resize to at least 80 × 20"));
        assert!(shown.contains(&format!("current size: {width} × {height}")));
    }
    assert!(render_text(&record, 0, 0).is_err());
    assert!(render_text(&record, u16::MAX, u16::MAX).is_err());
}

#[test]
fn previews_and_immediate_node_rendering_stay_bounded() {
    let value = "\n".repeat(1_000_000);
    let shown = render::preview(&value, 60);
    assert_eq!(shown.len(), 123, "60 escaped newlines and a UTF-8 ellipsis");
    assert!(shown.ends_with('…'));
    let (record, _) = recording(RecordLimits::default(), None);
    let first = render_text(&record, 120, 36).unwrap();
    let second = render_text(&record, 120, 36).unwrap();
    assert_eq!(
        first, second,
        "headless renders use the same initial model deterministically"
    );
    assert!(first.contains("Replay: not checked"));
}

#[test]
fn wide_nodes_render_only_the_selected_viewport() {
    let mut builder = ProgramBuilder::new();
    let mut body = Asm::new();
    body.set_register_types(vec![ValueType::Unit, ValueType::Agg]);
    body.ret(0, &[1]);
    builder
        .push_function_checked(
            body,
            FunctionSig {
                arg_types: vec![ValueType::Agg],
                ret_types: vec![ValueType::Agg],
            },
        )
        .unwrap();
    let program = Arc::new(builder.build_verified().unwrap());
    let mut vm = Vm::new(
        Echo {
            effects: Arc::new(AtomicUsize::new(0)),
            error: None,
        },
        Limits::default(),
    );
    let root = vm
        .aggregates_mut()
        .tuple_new((0..20_000).map(Value::I64).collect())
        .unwrap();
    let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
    vm.run(
        &program,
        FuncId(0),
        &[Value::Agg(root)],
        recorder.mask(),
        Some(&mut recorder),
    )
    .unwrap();
    let record = recorder.finish();
    let mut view = View::new(&record);
    press(&mut view, KeyCode::Enter);
    assert_eq!(view.values().size_hint(), (20_000, Some(20_000)));
    let first = text(&mut view, 120, 36);
    assert!(first.contains("Item[0]"));
    assert!(!first.contains("Item[19999]"));
    press(&mut view, KeyCode::End);
    let last = text(&mut view, 120, 36);
    assert!(last.contains("Item[19999]  19999 · i64"));
    assert!(last.len() < 120 * 36 * 4);
    press(&mut view, KeyCode::Down);
    assert_eq!(view.value_selected, 19_999);
    press(&mut view, KeyCode::PageUp);
    assert!(view.value_selected < 19_999);
    press(&mut view, KeyCode::PageDown);
    assert_eq!(view.value_selected, 19_999);
    press(&mut view, KeyCode::Home);
    assert_eq!(view.value_selected, 0);
}

#[test]
fn returning_to_parent_restores_selected_return_slot() {
    let (record, _) = recording(RecordLimits::default(), None);
    let mut view = View::new(&record);
    view.select(0);
    press(&mut view, KeyCode::Down);
    press(&mut view, KeyCode::Tab);
    press(&mut view, KeyCode::Down); // raw return slot
    assert_eq!(view.value_selected, 1);
    press(&mut view, KeyCode::Enter);
    assert_eq!(view.value_selected, 0);
    press(&mut view, KeyCode::Backspace);
    assert_eq!(view.value_selected, 1);
    assert!(view.path.is_empty());
}

#[test]
fn compact_failures_keep_boundary_location_and_reason_visible() {
    use execution_tape::trace::TraceLocation;
    use execution_tape_recording::{Divergence, ReplayBoundary, ReplayError};
    let (record, _) = recording(RecordLimits::default(), None);
    let mut view = View::new(&record);
    view.select(0);
    view.replay = render::replay_error(&ReplayError::Diverged {
        boundary: ReplayBoundary::HostCall { index: 123 },
        location: Some(TraceLocation {
            func: FuncId(7),
            pc: 42,
        }),
        reason: Divergence::Value {
            aggregate: None,
            index: 3,
        },
    });
    let shown = text(&mut view, 80, 24);
    assert!(shown.contains("host #123"));
    assert!(shown.contains("f7:000042"));
    assert!(shown.contains("value mismatch root[3]"));
    assert!(shown.contains("entry context"));

    let (partial, _) = recording(
        RecordLimits {
            host_calls: 0,
            ..RecordLimits::default()
        },
        None,
    );
    let location = partial.fault_location().unwrap();
    let shown = render_text(&partial, 80, 24).unwrap();
    let shown = shown.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(shown.contains(&format!("at f{}:{:06}", location.func.0, location.pc)));
}

#[test]
fn source_register_previews_report_omissions() {
    let mut builder = ProgramBuilder::new();
    let mut body = Asm::new();
    let mut kinds = vec![ValueType::Unit];
    kinds.extend([ValueType::I64; 12]);
    kinds.push(ValueType::Agg);
    body.set_register_types(kinds);
    let registers: Vec<_> = (1..=12).collect();
    body.tuple_new(13, &registers).ret(0, &[13]);
    builder
        .push_function_checked(
            body,
            FunctionSig {
                arg_types: vec![ValueType::I64; 12],
                ret_types: vec![ValueType::Agg],
            },
        )
        .unwrap();
    let program = Arc::new(builder.build_verified().unwrap());
    let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
    Vm::new(
        Echo {
            effects: Arc::new(AtomicUsize::new(0)),
            error: None,
        },
        Limits::default(),
    )
    .run(
        &program,
        FuncId(0),
        &vec![Value::I64(42); 12],
        recorder.mask(),
        Some(&mut recorder),
    )
    .unwrap();
    let shown = render_text(&recorder.finish(), 180, 36).unwrap();
    assert!(shown.contains("r1, r2, r3, r4, r5, r6, r7, r8, … +4 more"));
}

#[test]
fn scalar_previews_keep_exact_bits_lengths_and_argument_arity() {
    let (record, _) = recording(RecordLimits::default(), None);
    let arguments = [RecordedValue::I64(1), RecordedValue::I64(2)];
    assert_eq!(
        describe::arguments(&record, &arguments),
        "1, 2",
        "two scalar arguments must not be presented as one tuple argument"
    );
    let bits = 0x7ff8_0000_0000_0042;
    assert!(describe::value(&record, &RecordedValue::F64(bits)).contains("7ff8000000000042"));
    assert!(describe::value(&record, &RecordedValue::F64((-0.0_f64).to_bits())).contains("-0.0"));
    let text = RecordedValue::Str("\n".repeat(1000).into());
    let preview = describe::value(&record, &text);
    assert!(preview.contains("1000 bytes"));
    assert!(preview.contains('…'));
    assert!(!preview.contains('\n'));
    assert!(preview.len() < 200);
}

#[test]
fn successful_void_return_is_not_presented_as_missing_capture() {
    let mut builder = ProgramBuilder::new();
    let mut body = Asm::new();
    body.set_register_types(vec![ValueType::Unit]);
    body.ret(0, &[]);
    builder
        .push_function_checked(
            body,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![],
            },
        )
        .unwrap();
    let program = Arc::new(builder.build_verified().unwrap());
    let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default());
    Vm::new(
        Echo {
            effects: Arc::new(AtomicUsize::new(0)),
            error: None,
        },
        Limits::default(),
    )
    .run(
        &program,
        FuncId(0),
        &[],
        recorder.mask(),
        Some(&mut recorder),
    )
    .unwrap();
    let record = recorder.finish();
    let initial = render_text(&record, 80, 24).unwrap();
    assert!(initial.contains("End selects the guest outcome"));
    assert!(!initial.contains("Select a host call"));
    let mut view = View::new(&record);
    press(&mut view, KeyCode::End);
    assert!(text(&mut view, 80, 24).contains("returned successfully without values"));
}

#[test]
fn keyboard_focus_has_a_double_border_and_explicit_target_at_compact_width() {
    let (record, _) = recording(RecordLimits::default(), None);
    let mut view = View::new(&record);
    let events = text(&mut view, 80, 24);
    assert!(events.contains("ACTIVE · EVENTS"));
    assert_eq!(events.lines().nth(4).unwrap().chars().next(), Some('╔'));
    assert!(events.contains("CALL VALUES · echo"));
    assert!(events.contains("→ Argument[0]"));
    assert!(events.contains("← Return[0]"));
    press(&mut view, KeyCode::Tab);
    let values = text(&mut view, 80, 24);
    assert!(values.contains("ACTIVE · CALL VALUES"));
    assert_eq!(values.lines().nth(4).unwrap().chars().next(), Some('┌'));
    assert_eq!(values.lines().nth(7).unwrap().chars().nth(24), Some('╔'));
    press(&mut view, KeyCode::BackTab);
    assert!(text(&mut view, 80, 24).contains("ACTIVE · EVENTS"));
}

#[test]
fn selecting_a_caller_frame_moves_source_to_its_recorded_call_site() {
    let mut b = ProgramBuilder::new();
    let import = b.host_sig_for(
        "echo",
        HostSig {
            args: vec![ValueType::I64],
            rets: vec![ValueType::I64],
        },
    );
    let mut worker = Asm::new();
    worker.set_register_types(vec![ValueType::Unit, ValueType::I64, ValueType::I64]);
    worker
        .host_call_sig_id(0, import, 0, &[1], &[2])
        .ret(0, &[2]);
    let worker = b
        .push_function_checked(
            worker,
            FunctionSig {
                arg_types: vec![ValueType::I64],
                ret_types: vec![ValueType::I64],
            },
        )
        .unwrap();
    b.set_function_name(worker, "worker").unwrap();
    let mut caller = Asm::new();
    caller.set_register_types(vec![ValueType::Unit, ValueType::I64, ValueType::I64]);
    caller
        .const_i64(1, 7)
        .call(0, worker, 0, &[1], &[2])
        .ret(0, &[2]);
    let caller = b
        .push_function_checked(
            caller,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
            },
        )
        .unwrap();
    b.set_function_name(caller, "caller").unwrap();
    let p = Arc::new(b.build_verified().unwrap());
    let mut recorder = Recorder::new(Arc::clone(&p), RecordLimits::default());
    Vm::new(
        Echo {
            effects: Arc::new(AtomicUsize::new(0)),
            error: None,
        },
        Limits::default(),
    )
    .run(&p, caller, &[], recorder.mask(), Some(&mut recorder))
    .unwrap();
    let record = recorder.finish();
    let mut view = View::new(&record);
    let locations: Vec<_> = view.stack().unwrap().collect();
    assert_eq!(locations.len(), 2);
    assert_eq!(view.location(), Some(locations[0]));
    press(&mut view, KeyCode::Tab);
    press(&mut view, KeyCode::Tab);
    assert_eq!(view.focus, model::Focus::Stack);
    press(&mut view, KeyCode::Down);
    assert_eq!(view.location(), Some(locations[1]));
    assert_eq!(view.source.as_ref().unwrap().func(), caller);
    let shown = text(&mut view, 80, 24);
    assert!(shown.contains("ACTIVE · STACK"));
    assert!(shown.contains("call worker(r1)"));
    assert!(shown.contains("suspended caller"));
    assert!(shown.contains("7 · i64"));
    press(&mut view, KeyCode::Up);
    assert_eq!(view.location(), Some(locations[0]));
    press(&mut view, KeyCode::Tab); // events
    press(&mut view, KeyCode::End);
    assert_eq!(view.frame_selected, 0);
    assert_eq!(view.location(), record.outcome_location());
    assert_eq!(view.stack().unwrap().len(), 1);
}

#[test]
fn complete_value_only_capture_has_source_but_explicitly_unavailable_stack() {
    let (record, _) = recording_with_calls(RecordLimits::default(), None, false);
    record.status().unwrap();
    let mut view = View::new(&record);
    let location = view.calls[0].location;
    assert!(view.stack().is_none());
    press(&mut view, KeyCode::Tab);
    press(&mut view, KeyCode::Tab);
    let shown = text(&mut view, 80, 24);
    assert!(shown.contains("CAPTURE COMPLETE"));
    assert!(shown.contains("ACTIVE · STACK"));
    assert!(shown.contains("No guest stack"));
    assert!(shown.contains("observed at this"));
    assert!(shown.contains("host echo(r2)"));
    press(&mut view, KeyCode::Down);
    assert_eq!(view.location(), Some(location));
    assert_eq!(view.frame_selected, 0);
}

#[test]
fn application_entry_is_separate_from_guest_frames_and_uses_captured_inputs() {
    let (record, _) = recording(RecordLimits::default(), None);
    let shown = render_text(&record, 120, 36).unwrap();
    let entry_line = shown.lines().nth(3).unwrap();
    assert!(entry_line.contains("Application → function #0(("));
    assert!(entry_line.contains("42"));
    assert!(entry_line.contains("VM invocation"));
    assert!(shown.contains("root ← Application"));
    assert_eq!(record.host_calls().next().unwrap().stack.unwrap().len(), 1);
    let (partial, _) = recording(
        RecordLimits {
            values: 0,
            ..RecordLimits::default()
        },
        None,
    );
    assert!(partial.invocation().is_none());
    let shown = render_text(&partial, 80, 24).unwrap();
    assert!(shown.contains("Invocation inputs unavailable"));
    assert!(!shown.contains("Application →"));
}

#[test]
fn actual_source_and_values_use_distinct_indexed_colors() {
    let (record, _) = recording(RecordLimits::default(), None);
    let mut view = View::new(&record);
    let mut terminal = Terminal::new(TestBackend::new(120, 36)).unwrap();
    terminal
        .draw(|frame| render::draw(frame, &mut view))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let mut found = false;
    for y in 0..36 {
        let symbols: Vec<_> = (0..120).map(|x| buffer[(x, y)].symbol()).collect();
        if symbols.concat().contains("host echo(r2)") {
            let reg = symbols
                .windows(2)
                .position(|word| word == ["r", "2"])
                .unwrap();
            let host = symbols
                .windows(4)
                .position(|word| word == ["h", "o", "s", "t"])
                .unwrap();
            assert_eq!(buffer[(u16::try_from(reg).unwrap(), y)].fg, palette::CYAN);
            assert_eq!(buffer[(u16::try_from(host).unwrap(), y)].fg, palette::GREEN);
            found = true;
        }
    }
    assert!(found, "source must retain the actual host-call instruction");
    let colors: std::collections::HashSet<_> = buffer.content.iter().map(|cell| cell.fg).collect();
    assert!(colors.contains(&palette::GOLD));
    assert!(colors.contains(&palette::BLUE));
    assert!(
        colors
            .iter()
            .all(|color| !matches!(color, ratatui::style::Color::Rgb(..)))
    );
}

#[test]
fn actual_instruction_navigation_shows_local_values_and_preserves_boundary_selection() {
    let (record, _) = recording_with_options(RecordLimits::default(), None, true, true);
    assert_eq!(
        record.instruction_status(),
        Some(execution_tape_recording::InstructionStatus::Complete)
    );
    let mut view = View::new(&record);
    let boundary = view.selected;
    press(&mut view, KeyCode::Char('i'));
    let shown = text(&mut view, 120, 40);
    assert!(shown.contains("EXECUTED"), "{shown}");
    assert!(shown.contains("Actual successor:"), "{shown}");
    assert!(shown.contains("after r3"), "{shown}");
    let before = view.location();
    press(&mut view, KeyCode::Down);
    assert_ne!(before, view.location());
    press(&mut view, KeyCode::End);
    assert_eq!(view.instruction, Some(record.instructions().len() - 1));
    assert_eq!(view.stack().unwrap().len(), 1);
    press(&mut view, KeyCode::Char('i'));
    assert_eq!(view.instruction, None);
    assert_eq!(view.selected, boundary);
    assert!(text(&mut view, 120, 40).contains("CALL VALUES"));
}

#[test]
fn recorded_instructions_follow_exact_source_snapshots_and_reject_stale_bundles() {
    use crate::sources::{
        SourceDocument, SourceError, SourceId, SourceLimits, SourceMap, SourceMapping,
    };
    use execution_tape::trace::TraceLocation;
    let (record, _) = recording_with_options(RecordLimits::default(), None, true, true);
    let source =
        execution_tape::disasm::disassemble_function(record.program().program(), FuncId(0))
            .unwrap();
    let first = source.instrs().next().unwrap().pc();
    let last = source.instrs().last().unwrap().pc();
    let map = SourceMap::new(
        record.program().program(),
        vec![SourceDocument {
            name: "steps.expr".into(),
            text: "first\nlast\n".into(),
        }],
        vec![
            SourceMapping {
                location: TraceLocation {
                    func: FuncId(0),
                    pc: first,
                },
                document: SourceId(0),
                range: 0..5,
            },
            SourceMapping {
                location: TraceLocation {
                    func: FuncId(0),
                    pc: last,
                },
                document: SourceId(0),
                range: 6..10,
            },
        ],
        SourceLimits::default(),
    )
    .unwrap();
    let shown = RecordingExplorer::new(&record)
        .sources(&map)
        .unwrap()
        .instructions()
        .render_text(160, 60)
        .unwrap();
    assert!(shown.contains("FRONTEND SOURCE SNAPSHOT"), "{shown}");
    assert!(shown.contains("steps.expr"), "{shown}");
    assert!(shown.contains("bytes 0..5"), "{shown}");
    assert!(shown.contains("VERIFIER · static"), "{shown}");
    let mut view = View::with_options(
        &record,
        Some(map.bind(record.program().program()).unwrap()),
        true,
    );
    press(&mut view, KeyCode::End);
    assert!(
        view.source_lines[0].contains("bytes 6..10"),
        "{:?}",
        view.source_lines
    );
    press(&mut view, KeyCode::Home);
    press(&mut view, KeyCode::Down);
    assert!(view.source_lines[0].contains("no frontend mapping"));
    let mut other = record.program().program().clone();
    other.bytecode_data[0] ^= 1;
    let stale = SourceMap::new(&other, vec![], vec![], SourceLimits::default()).unwrap();
    assert!(matches!(
        RecordingExplorer::new(&record).sources(&stale),
        Err(SourceError::ProgramMismatch)
    ));
}

#[test]
fn branch_explanation_survives_selecting_its_suspended_caller() {
    let mut b = ProgramBuilder::new();
    let root = b
        .declare_function(FunctionSig {
            arg_types: vec![],
            ret_types: vec![ValueType::I64],
        })
        .unwrap();
    let mut child = Asm::new();
    child.set_register_types(vec![ValueType::Unit, ValueType::Bool, ValueType::I64]);
    let yes = child.label();
    let no = child.label();
    child.br(1, yes, no);
    child.place(yes).unwrap();
    child.const_i64(2, 42).ret(0, &[2]);
    child.place(no).unwrap();
    child.const_i64(2, 7).ret(0, &[2]);
    let callee = b
        .push_function_checked(
            child,
            FunctionSig {
                arg_types: vec![ValueType::Bool],
                ret_types: vec![ValueType::I64],
            },
        )
        .unwrap();
    let mut body = Asm::new();
    body.set_register_types(vec![ValueType::Unit, ValueType::Bool, ValueType::I64]);
    body.const_bool(1, true)
        .call(0, callee, 0, &[1], &[2])
        .ret(0, &[2]);
    b.define_function(root, body).unwrap();
    let program = Arc::new(b.build_verified().unwrap());
    let source = execution_tape::disasm::disassemble_function(program.program(), callee).unwrap();
    let mut recorder = Recorder::new(Arc::clone(&program), RecordLimits::default())
        .with_instructions(execution_tape_recording::InstructionCapture {
            max_steps: 100,
            watches: vec![execution_tape_recording::RegisterWatch {
                location: execution_tape::trace::TraceLocation {
                    func: callee,
                    pc: 0,
                },
                registers: vec![1],
            }],
        });
    assert_eq!(
        source.instrs().next().unwrap().opcode(),
        execution_tape::opcode::Opcode::Br
    );
    let effects = Arc::new(AtomicUsize::new(0));
    let mut vm = Vm::new(
        Echo {
            effects,
            error: None,
        },
        Limits::default(),
    );
    vm.run(&program, root, &[], recorder.mask(), Some(&mut recorder))
        .unwrap();
    let record = recorder.finish();
    let mut view = View::with_options(&record, None, true);
    press(&mut view, KeyCode::Down);
    press(&mut view, KeyCode::Down);
    assert!(text(&mut view, 160, 50).contains("took true arm · r1 = true"));
    press(&mut view, KeyCode::Tab);
    press(&mut view, KeyCode::Tab);
    press(&mut view, KeyCode::Down);
    assert_eq!(view.location().unwrap().func, root);
    assert!(text(&mut view, 160, 50).contains("took true arm · r1 = true"));
}
