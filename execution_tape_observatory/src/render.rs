// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use super::describe::host_error;
pub(super) use super::describe::preview;
use super::{
    describe,
    model::{Focus, View},
};
use super::{palette::*, syntax};
use execution_tape::vm::Trap;
use execution_tape_recording::{
    Divergence, HostOutcome, Outcome, RecordedValue, ReplayBoundary, ReplayError,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, List, ListItem, ListState, Paragraph, Wrap};

pub(super) fn trap_text(trap: &Trap) -> String {
    match trap {
        Trap::HostCallFailed { symbol, error, .. } => {
            format!("{}: {}", preview(symbol, 40), host_error(error))
        }
        _ => format!("{trap:?}"),
    }
}

pub(super) fn replay_error(error: &ReplayError) -> String {
    match error {
        ReplayError::Incomplete(fault) => format!("REPLAY FAILED · {fault}"),
        ReplayError::Aggregate(error) => format!("REPLAY FAILED · aggregate {error}"),
        ReplayError::Diverged {
            boundary,
            location,
            reason,
        } => {
            let boundary = match boundary {
                ReplayBoundary::Instruction { index } => format!("instruction #{index}"),
                ReplayBoundary::InitialState => "initial state".into(),
                ReplayBoundary::HostCall { index } => format!("host #{index}"),
                ReplayBoundary::ReturnCandidate => "return candidate".into(),
                ReplayBoundary::Completion => "completion".into(),
            };
            let location =
                location.map_or_else(String::new, |loc| format!(" f{}:{:06}", loc.func.0, loc.pc));
            let reason = match reason {
                Divergence::Value { aggregate, index } => aggregate.map_or_else(
                    || format!("value mismatch root[{index}]"),
                    |id| format!("value mismatch @{} field[{index}]", id.get()),
                ),
                Divergence::Arity { expected, actual } => {
                    format!("arity expected {expected}, got {actual}")
                }
                Divergence::Import { expected, actual } => {
                    format!("import expected {}, got {}", expected.0, actual.0)
                }
                Divergence::Location { expected, actual } => format!(
                    "PC expected f{}:{:06}, got {}",
                    expected.func.0,
                    expected.pc,
                    actual.map_or_else(
                        || "missing".into(),
                        |loc| format!("f{}:{:06}", loc.func.0, loc.pc)
                    )
                ),
                Divergence::Identity { aggregate } => {
                    format!("identity mismatch @{}", aggregate.get())
                }
                Divergence::UnresolvedAggregate { aggregate } => {
                    format!("unresolved @{}", aggregate.get())
                }
                Divergence::HostCalls { expected, actual } => {
                    format!("calls expected {expected}, got {actual}")
                }
                Divergence::Outcome => "outcome mismatch".into(),
                Divergence::CallStack => "guest call stack mismatch".into(),
                Divergence::Instruction => "instruction path mismatch".into(),
            };
            format!("FAILED · {boundary}{location} · {reason}")
        }
    }
}

fn import_name(view: &View<'_>, index: usize) -> String {
    describe::host(view.record.program().program(), view.calls[index].import)
}

fn history_status(view: &View<'_>) -> String {
    match view.record.instruction_status() {
        None => "not captured".into(),
        Some(execution_tape_recording::InstructionStatus::Complete) => {
            format!("complete · {} attempts", view.record.instructions().len())
        }
        Some(execution_tape_recording::InstructionStatus::Truncated { limit }) => {
            format!("TRUNCATED · first {limit} attempts")
        }
        Some(execution_tape_recording::InstructionStatus::Incomplete) => {
            "INCOMPLETE · missing observations".into()
        }
    }
}

fn step_description(
    view: &View<'_>,
    step: &execution_tape_recording::InstructionStep<'_>,
) -> String {
    use execution_tape::opcode::Opcode;
    use execution_tape_recording::InstructionOutcome;
    match step.outcome {
        InstructionOutcome::Pending => "completion not observed".into(),
        InstructionOutcome::Trapped => "trapped; no successful writes".into(),
        InstructionOutcome::Completed => match Opcode::from_byte(step.opcode) {
            Some(Opcode::Call | Opcode::CallIndirect) => {
                "dispatched into callee; results come later".into()
            }
            Some(Opcode::Ret) => if step.next.is_some() {
                "returned to caller"
            } else {
                "root return"
            }
            .into(),
            Some(Opcode::Br | Opcode::BrArgs) => {
                let condition = view
                    .source
                    .as_ref()
                    .filter(|s| s.func() == step.location.func)
                    .and_then(|s| s.instruction(step.location.pc))
                    .and_then(|i| match i.operands() {
                        execution_tape::disasm::Operands::Br { cond, .. }
                        | execution_tape::disasm::Operands::BrArgs { cond, .. } => Some(cond),
                        _ => None,
                    });
                step.inputs
                    .iter()
                    .find_map(|(reg, value)| match value {
                        RecordedValue::Bool(value) if Some(reg) == condition => Some(format!(
                            "took {} arm · r{reg} = {value}",
                            if *value { "true" } else { "false" }
                        )),
                        _ => None,
                    })
                    .unwrap_or_else(|| "branch completed; condition was not watched".into())
            }
            _ => "completed".into(),
        },
    }
}

fn title(view: &View<'_>, index: usize) -> String {
    if view.instruction.is_some() {
        return view.record.instruction(index).map_or_else(
            || "No instruction attempts retained".into(),
            |step| {
                format!(
                    "#{index} {}:{:06} {}",
                    describe::function(view.record.program().program(), step.location.func),
                    step.location.pc,
                    execution_tape::opcode::Opcode::from_byte(step.opcode)
                        .map_or_else(|| "unknown".into(), |op| format!("{op:?}"))
                )
            },
        );
    }
    if index == 0 {
        return view.record.invocation().map_or_else(
            || "Invocation · missing".into(),
            |entry| {
                format!(
                    "App → {}",
                    describe::function(view.record.program().program(), entry.entry)
                )
            },
        );
    }
    if index <= view.calls.len() {
        return format!("#{:04}  {}", index - 1, import_name(view, index - 1));
    }
    match view.record.outcome() {
        Some(Outcome::Returned(values)) => {
            format!("Result: {}", describe::values(view.record, values))
        }
        Some(Outcome::Trapped(_)) => "TRAP".into(),
        None => "OUTCOME · missing".into(),
    }
}

pub(super) fn panel(title: impl Into<Line<'static>>, focused: bool) -> Block<'static> {
    let mut title = title.into();
    if focused {
        title.spans.insert(0, Span::raw(" ACTIVE ·"));
    }
    Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_type(if focused {
            BorderType::Double
        } else {
            BorderType::Plain
        })
        .border_style(
            Style::default()
                .fg(if focused { GREEN } else { MUTED })
                .add_modifier(if focused {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        )
        .title_style(
            Style::default()
                .fg(if focused { GREEN } else { BLUE })
                .add_modifier(Modifier::BOLD),
        )
}

pub(super) fn too_small(frame: &mut Frame<'_>) -> bool {
    let area = frame.area();
    if area.width < 80 || area.height < 20 {
        frame.render_widget(
            Paragraph::new(format!(
                "Execution Observatory\nresize to at least 80 × 20\ncurrent size: {} × {}\nq to quit",
                area.width, area.height
            ))
            .style(Style::default().fg(GREEN)),
            area,
        );
        return true;
    }
    false
}

pub(super) fn draw(frame: &mut Frame<'_>, view: &mut View<'_>) {
    let area = frame.area();
    frame.render_widget(Block::default().style(Style::default().bg(BG).fg(FG)), area);
    if too_small(frame) {
        return;
    }
    let rows = Layout::vertical([
        Constraint::Length(4),
        Constraint::Min(10),
        Constraint::Length(3),
    ])
    .split(area);
    let complete = view.record.status().is_ok();
    let header = vec![
        Line::from(vec![
            Span::styled(
                " EXECUTION / OBSERVATORY ",
                Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                if complete {
                    if view.record.instruction_status().is_some() {
                        " BOUNDARY CAPTURE COMPLETE "
                    } else {
                        " CAPTURE COMPLETE "
                    }
                } else {
                    " PARTIAL CAPTURE "
                },
                Style::default()
                    .fg(BG)
                    .bg(if complete { GREEN } else { GOLD }),
            ),
        ]),
        Line::from(format!(
            " {} · {}",
            view.record.program().program().name().map_or_else(
                || view.record.invocation().map_or_else(
                    || "Unknown invocation".into(),
                    |entry| describe::function(view.record.program().program(), entry.entry)
                ),
                |name| preview(name, 44)
            ),
            match view.record.outcome() {
                Some(Outcome::Returned(values)) =>
                    format!("returned {}", describe::values(view.record, values)),
                Some(Outcome::Trapped(_)) => "run trapped".into(),
                None => "outcome not captured".into(),
            }
        )),
        Line::styled(
            format!(
                " Events: start → {} host calls → {}. i: history {}",
                view.calls.len(),
                match view.record.outcome() {
                    Some(Outcome::Trapped(_)) => "trap",
                    Some(Outcome::Returned(_)) => "result",
                    None => "unknown outcome",
                },
                history_status(view)
            ),
            Style::default().fg(MUTED),
        ),
        Line::styled(
            view.record.invocation().map_or_else(
                || " Invocation inputs unavailable".into(),
                |entry| {
                    format!(
                        " Application → {}({}) · VM invocation",
                        describe::function(view.record.program().program(), entry.entry),
                        describe::arguments(view.record, entry.args),
                    )
                },
            ),
            Style::default().fg(BLUE),
        ),
    ];
    frame.render_widget(Paragraph::new(header), rows[0]);
    let columns =
        Layout::horizontal([Constraint::Percentage(30), Constraint::Percentage(70)]).split(rows[1]);
    let left = Layout::vertical([Constraint::Percentage(60), Constraint::Min(4)]).split(columns[0]);
    timeline(frame, view, left[0]);
    stack(frame, view, left[1]);
    let value_count = view.value_count();
    let value_rows = if view.instruction.is_none()
        && view.path.is_empty()
        && let Some(call) = view
            .selected
            .checked_sub(1)
            .and_then(|index| view.calls.get(index))
    {
        let returns = match call.outcome {
            HostOutcome::Returned { values, .. } => values.len(),
            HostOutcome::Failed(_) => 0,
        };
        call.args
            .len()
            .max(1)
            .saturating_add(returns.max(1))
            .saturating_add(2)
    } else if value_count == 0 {
        6
    } else {
        value_count.saturating_add(2)
    };
    // Keep small calls compact; large aggregates get a viewport without crowding source.
    let value_height = value_rows.clamp(4, (usize::from(rows[1].height) * 40 / 100).max(4));
    let right = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(u16::try_from(value_height).expect("bounded by terminal height")),
        Constraint::Min(4),
    ])
    .split(columns[1]);
    view.page_rows = usize::from(match view.focus {
        Focus::Values => right[1].height,
        Focus::Events => left[0].height,
        Focus::Stack => left[1].height,
    })
    .saturating_sub(2)
    .max(1);
    let detail = if let Some(index) = view.instruction {
        view.record.instruction(index).map_or_else(
            || "No attempts retained; check instruction history status.".into(),
            |step| {
                format!(
                    "Attempt #{index}: {}\nActual successor: {} · history {}",
                    step_description(view, &step),
                    step.next.map_or_else(
                        || "none".into(),
                        |loc| format!(
                            "{}:{:06}",
                            describe::function(view.record.program().program(), loc.func),
                            loc.pc
                        )
                    ),
                    history_status(view)
                )
            },
        )
    } else if let Some(call) = view
        .selected
        .checked_sub(1)
        .and_then(|index| view.calls.get(index))
    {
        let name = import_name(view, view.selected - 1);
        let response = match call.outcome {
            HostOutcome::Returned { values, .. } => {
                format!("returned {}", describe::values(view.record, values))
            }
            HostOutcome::Failed(error) => format!("failed: {}", host_error(error)),
        };
        let mut detail = format!(
            "Call #{}: {name}({}) {response}\nSignature: {}",
            view.selected - 1,
            describe::arguments(view.record, call.args),
            describe::signature(view.record.program().program(), call.import)
        );
        if let HostOutcome::Returned { extra_fuel, .. } = call.outcome
            && extra_fuel != 0
        {
            detail.push_str(&format!("\nExtra fuel requested: {extra_fuel}"));
        }
        detail
    } else if let Some(Outcome::Trapped(trap)) = view
        .record
        .outcome()
        .filter(|_| view.selected > view.calls.len())
    {
        trap_text(&trap.trap)
    } else {
        format!(
            "{}\n{}",
            title(view, view.selected),
            if view.selected == 0 {
                "Requested invocation · source entry context"
            } else {
                "Committed VM outcome · recording-local identities"
            }
        )
    };
    frame.render_widget(
        Paragraph::new(detail).style(Style::default().fg(BLUE)),
        right[0],
    );
    values(frame, view, right[1]);
    source(frame, view, right[2]);
    let status = match view.record.status() {
        Ok(()) => format!("Replay: {}", view.replay),
        Err(error) => format!(
            "Capture: {error}{} · replay unavailable",
            view.record
                .fault_location()
                .map_or_else(String::new, |loc| format!(
                    " at f{}:{:06}",
                    loc.func.0, loc.pc
                ))
        ),
    };
    let footer = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(rows[2]);
    frame.render_widget(
        Paragraph::new(status)
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(if complete { GREEN } else { GOLD })),
        footer[0],
    );
    frame.render_widget(
        Paragraph::new("↑↓/jk move  Tab pane  ↵ open  ⌫ up  i history  r replay  q quit")
            .style(Style::default().fg(MUTED)),
        footer[1],
    );
}

fn timeline(frame: &mut Frame<'_>, view: &View<'_>, area: Rect) {
    let height = usize::from(area.height.saturating_sub(2));
    let selected = view.instruction.unwrap_or(view.selected);
    let count = if view.instruction.is_some() {
        view.record.instructions().len()
    } else {
        view.calls.len() + 2
    };
    let start = selected.saturating_sub(height / 2);
    let items: Vec<_> = (start..count)
        .take(height)
        .map(|index| {
            ListItem::new(Line::styled(
                title(view, index),
                Style::default().fg(if index == 0 {
                    BLUE
                } else if index <= view.calls.len() {
                    FG
                } else {
                    GOLD
                }),
            ))
        })
        .collect();
    let mut state = ListState::default().with_selected((count != 0).then_some(selected - start));
    frame.render_stateful_widget(
        List::new(items)
            .block(panel(
                if view.instruction.is_some() {
                    " EXECUTED · i boundaries "
                } else {
                    " EVENTS · i instructions "
                },
                view.focus == Focus::Events,
            ))
            .highlight_symbol("▸ ")
            .highlight_style(Style::default().fg(GREEN).bg(Color::Indexed(23))),
        area,
        &mut state,
    );
}

fn stack(frame: &mut Frame<'_>, view: &View<'_>, area: Rect) {
    let title = view.stack().map_or_else(
        || " STACK ".into(),
        |stack| format!(" STACK · {} ", stack.len()),
    );
    let block = panel(title, view.focus == Focus::Stack).title_bottom(Span::styled(
        if view.stack().is_some() {
            "root ← Application"
        } else {
            "guest frames"
        },
        Style::default().fg(GOLD).add_modifier(Modifier::BOLD),
    ));
    let Some(stack) = view.stack() else {
        frame.render_widget(
            Paragraph::new("No guest stack observed at this event.")
                .wrap(Wrap { trim: false })
                .block(block),
            area,
        );
        return;
    };
    let height = usize::from(area.height.saturating_sub(2));
    let start = view.frame_selected.saturating_sub(height / 2);
    let items: Vec<_> = stack
        .enumerate()
        .skip(start)
        .take(height)
        .map(|(index, location)| {
            ListItem::new(Line::styled(
                format!(
                    "#{index} {}",
                    describe::function(view.record.program().program(), location.func)
                ),
                Style::default().fg(if index == 0 { GREEN } else { BLUE }),
            ))
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(view.frame_selected - start));
    frame.render_stateful_widget(
        List::new(items)
            .block(block)
            .highlight_symbol("▸ ")
            .highlight_style(
                Style::default()
                    .bg(Color::Indexed(23))
                    .add_modifier(Modifier::BOLD),
            ),
        area,
        &mut state,
    );
}

struct ValueSection<'a> {
    title: String,
    note: String,
    label: &'static str,
    values: &'a [RecordedValue],
    base: usize,
    empty: String,
}

fn value_section(frame: &mut Frame<'_>, view: &View<'_>, area: Rect, section: ValueSection<'_>) {
    let selected = view
        .value_selected
        .checked_sub(section.base)
        .filter(|&index| index < section.values.len());
    let block = panel(section.title, view.focus == Focus::Values).title_bottom(section.note);
    if section.values.is_empty() {
        frame.render_widget(
            Paragraph::new(section.empty)
                .wrap(Wrap { trim: false })
                .block(block),
            area,
        );
        return;
    }
    let height = usize::from(area.height.saturating_sub(2));
    let start = selected.unwrap_or(0).saturating_sub(height / 2);
    let items: Vec<_> = section
        .values
        .iter()
        .enumerate()
        .skip(start)
        .take(height)
        .map(|(index, value)| {
            let mut spans = vec![Span::styled(
                format!("{}[{index}]  ", section.label),
                Style::default().fg(BLUE),
            )];
            spans.extend(syntax::value(view.record, value));
            ListItem::new(Line::from(spans))
        })
        .collect();
    let mut state = ListState::default().with_selected(selected.map(|index| index - start));
    frame.render_stateful_widget(
        List::new(items)
            .block(block)
            .highlight_symbol("▸ ")
            .highlight_style(Style::default().fg(GREEN)),
        area,
        &mut state,
    );
}

fn values(frame: &mut Frame<'_>, view: &View<'_>, area: Rect) {
    if let Some(node) = view
        .path
        .last()
        .and_then(|&(id, _)| view.record.aggregate(id))
    {
        value_section(
            frame,
            view,
            area,
            ValueSection {
                title: format!(
                    " {} ITEMS · {} ",
                    describe::aggregate_kind(node.kind).to_uppercase(),
                    node.values.len()
                ),
                note: format!(
                    "id @{} · {} · Backspace up",
                    node.id.get(),
                    if node.initial {
                        "present at start"
                    } else {
                        "created during run"
                    }
                ),
                label: "Item",
                values: node.values,
                base: 0,
                empty: "This aggregate has no items.".into(),
            },
        );
    } else if view.instruction.is_some() {
        let count = view.value_count();
        let function = view
            .instruction
            .and_then(|index| view.record.instruction(index))
            .map_or_else(
                || "unavailable".into(),
                |step| describe::function(view.record.program().program(), step.location.func),
            );
        let block = panel(
            format!(" OBSERVED REGISTERS · {function} "),
            view.focus == Focus::Values,
        )
        .title_bottom("before: inputs · after: local writes · Enter: open aggregate");
        if count == 0 {
            frame.render_widget(Paragraph::new("No register values captured at this attempt.\nConfigure explicit register watches; unwatched values remain unknown.").wrap(Wrap { trim: false }).block(block), area);
        } else {
            let height = usize::from(area.height.saturating_sub(2));
            let start = view.value_selected.saturating_sub(height / 2);
            let items: Vec<_> = view
                .values()
                .skip(start)
                .take(height)
                .map(|(phase, reg, value)| {
                    let mut spans = vec![Span::styled(
                        format!("{phase} r{reg}  "),
                        Style::default().fg(if phase == "before" { BLUE } else { GREEN }),
                    )];
                    spans.extend(syntax::value(view.record, value));
                    ListItem::new(Line::from(spans))
                })
                .collect();
            let mut state = ListState::default().with_selected(Some(view.value_selected - start));
            frame.render_stateful_widget(
                List::new(items)
                    .block(block)
                    .highlight_symbol("▸ ")
                    .highlight_style(Style::default().bg(Color::Indexed(23))),
                area,
                &mut state,
            );
        }
    } else if view.selected == 0 {
        value_section(
            frame,
            view,
            area,
            ValueSection {
                title: format!(
                    " INPUTS TO {} ",
                    view.record.invocation().map_or_else(
                        || "GUEST".into(),
                        |entry| describe::function(view.record.program().program(), entry.entry)
                    )
                ),
                note: "Values supplied when execution started".into(),
                label: "Input",
                values: view.record.invocation().map_or(&[], |entry| entry.args),
                base: 0,
                empty: if view.calls.is_empty() {
                    "No entry arguments.\nEnd selects the guest outcome."
                } else {
                    "No entry arguments.\nSelect a host call to inspect what was sent and returned."
                }
                .into(),
            },
        );
    } else if let Some(call) = view.calls.get(view.selected - 1) {
        call_values(frame, view, area, call);
    } else {
        let returned = match view.record.outcome() {
            Some(Outcome::Returned(values)) => values,
            _ => &[],
        };
        value_section(
            frame,
            view,
            area,
            ValueSection {
                title: " FINAL GUEST RESULT ".into(),
                note: "Committed execution outcome".into(),
                label: "Output",
                values: returned,
                base: 0,
                empty: match view.record.outcome() {
                    Some(Outcome::Returned(_)) => "Execution returned successfully without values.",
                    Some(Outcome::Trapped(_)) => "Execution trapped; no successful return values.",
                    None => "Final outcome not captured.",
                }
                .into(),
            },
        );
    }
}

fn call_values(
    frame: &mut Frame<'_>,
    view: &View<'_>,
    area: Rect,
    call: &execution_tape_recording::HostCall<'_>,
) {
    let (returns, failure) = match call.outcome {
        HostOutcome::Returned { values, .. } => (values, None),
        HostOutcome::Failed(error) => (&[][..], Some(host_error(error))),
    };
    // Empty groups still get a descriptive row, but keyboard selection only visits values.
    let sent_rows = call.args.len().max(1);
    let row_count = sent_rows + returns.len().max(1);
    let selected = if view.value_selected < call.args.len() {
        Some(view.value_selected)
    } else if view.value_selected - call.args.len() < returns.len() {
        Some(sent_rows + view.value_selected - call.args.len())
    } else {
        None
    };
    let height = usize::from(area.height.saturating_sub(2));
    let start = selected.unwrap_or(0).saturating_sub(height / 2);
    let items: Vec<_> = (start..row_count)
        .take(height)
        .map(|row| {
            let sent = row < sent_rows;
            let index = if sent { row } else { row - sent_rows };
            let value = if sent {
                call.args.get(index)
            } else {
                returns.get(index)
            };
            let direction = if sent { "→" } else { "←" };
            let label = if sent { "Argument" } else { "Return" };
            let color = if sent {
                BLUE
            } else if failure.is_some() {
                GOLD
            } else {
                GREEN
            };
            let spans = if let Some(value) = value {
                let mut spans = vec![Span::styled(
                    format!("{direction} {label}[{index}]  "),
                    Style::default().fg(color),
                )];
                spans.extend(syntax::value(view.record, value));
                spans
            } else {
                let message = if sent {
                    "No arguments sent"
                } else {
                    failure.unwrap_or("Host returned no values")
                };
                vec![Span::styled(
                    format!("{direction} {message}"),
                    Style::default().fg(color),
                )]
            };
            ListItem::new(Line::from(spans))
        })
        .collect();
    let mut state = ListState::default().with_selected(selected.map(|index| index - start));
    frame.render_stateful_widget(
        List::new(items)
            .block(
                panel(
                    format!(" CALL VALUES · {} ", import_name(view, view.selected - 1)),
                    view.focus == Focus::Values,
                )
                .title_bottom("→ sent · ← returned before checks"),
            )
            .highlight_symbol("▸ ")
            .highlight_style(
                Style::default()
                    .add_modifier(Modifier::BOLD)
                    .bg(Color::Indexed(23)),
            ),
        area,
        &mut state,
    );
}

fn source(frame: &mut Frame<'_>, view: &View<'_>, mut area: Rect) {
    if !view.fact_lines.is_empty() && area.height >= 10 {
        let parts = Layout::vertical([Constraint::Min(4), Constraint::Length(4)]).split(area);
        frame.render_widget(
            Paragraph::new(view.fact_lines.join("\n"))
                .block(panel(" VERIFIER · static before → after ", false)),
            parts[1],
        );
        area = parts[0];
    }
    let location = view.location();
    let Some(source) = &view.source else {
        frame.render_widget(
            Paragraph::new("No captured instruction location").block(panel(" SOURCE ", false)),
            area,
        );
        return;
    };
    let height = usize::from(area.height.saturating_sub(2));
    let start = view.source_index.saturating_sub(height / 2);
    let first = source
        .instrs()
        .nth(start)
        .map_or(0, |instruction| instruction.pc());
    let last = source
        .instrs()
        .skip(start)
        .take(height)
        .last()
        .map_or(first, |instruction| instruction.pc());
    let lines: Vec<_> = source
        .instrs()
        .skip(start)
        .take(height)
        .map(|instruction| {
            let current = Some(instruction.pc()) == location.map(|loc| loc.pc);
            let mut spans = vec![Span::styled(
                format!(
                    "{} {:06}  ",
                    if current { "▸" } else { " " },
                    instruction.pc()
                ),
                Style::default().fg(if current { GOLD } else { MUTED }),
            )];
            spans.extend(view.flow.gutter(instruction.pc(), first, last));
            spans.push(Span::raw(" "));
            spans.extend(
                syntax::source(&describe::instruction(
                    view.record.program().program(),
                    instruction,
                ))
                .spans,
            );
            Line::from(spans).style(if current {
                Style::default()
                    .bg(Color::Indexed(237))
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            })
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines).block(
            panel(
                format!(
                    " SOURCE · {} · {} ",
                    describe::function(view.record.program().program(), source.func()),
                    if view.frame_selected > 0 {
                        "suspended caller"
                    } else if view.selected == 0 {
                        "entry context"
                    } else {
                        "rN = guest register"
                    }
                ),
                false,
            )
            .title_bottom(if view.flow.omitted() == 0 {
                "T/F arms · ▶ target · static flow".into()
            } else {
                format!("static flow · +{} edges hidden", view.flow.omitted())
            }),
        ),
        area,
    );
}
