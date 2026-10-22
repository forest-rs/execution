// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use super::describe::host_error;
pub(super) use super::describe::preview;
use super::{describe, model::View};
use execution_tape::disasm::{CallTarget, Operands};
use execution_tape::vm::Trap;
use execution_tape_recording::{
    Divergence, HostOutcome, Outcome, RecordedValue, ReplayBoundary, ReplayError,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, List, ListItem, ListState, Paragraph, Wrap};

const BG: Color = Color::Rgb(12, 18, 27);
const FG: Color = Color::Rgb(198, 212, 219);
const MUTED: Color = Color::Rgb(105, 131, 148);
const GREEN: Color = Color::Rgb(120, 222, 174);
const BLUE: Color = Color::Rgb(123, 181, 233);
const GOLD: Color = Color::Rgb(242, 196, 115);

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
            };
            format!("FAILED · {boundary}{location} · {reason}")
        }
    }
}

fn import_name(view: &View<'_>, index: usize) -> String {
    describe::host(view.record, view.calls[index].import)
}

fn title(view: &View<'_>, index: usize) -> String {
    if index == 0 {
        return view.record.invocation().map_or_else(
            || "Start · not captured".into(),
            |entry| format!("Start {}", describe::function(view.record, entry.entry)),
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

fn panel(title: impl Into<Line<'static>>, focused: bool) -> Block<'static> {
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

pub(super) fn draw(frame: &mut Frame<'_>, view: &mut View<'_>) {
    let area = frame.area();
    frame.render_widget(Block::default().style(Style::default().bg(BG).fg(FG)), area);
    if area.width < 80 || area.height < 20 {
        frame.render_widget(
            Paragraph::new("Execution Observatory\nresize to at least 80 × 20\nq to quit")
                .style(Style::default().fg(GREEN)),
            area,
        );
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
                    " CAPTURE COMPLETE "
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
                    |entry| describe::function(view.record, entry.entry)
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
                " Events: start → {} host calls → {}. Guest code runs between events.",
                view.calls.len(),
                match view.record.outcome() {
                    Some(Outcome::Trapped(_)) => "trap",
                    Some(Outcome::Returned(_)) => "result",
                    None => "unknown outcome",
                }
            ),
            Style::default().fg(MUTED),
        ),
    ];
    frame.render_widget(Paragraph::new(header), rows[0]);
    let columns =
        Layout::horizontal([Constraint::Percentage(30), Constraint::Percentage(70)]).split(rows[1]);
    timeline(frame, view, columns[0]);
    let right = Layout::vertical([
        Constraint::Length(3),
        Constraint::Percentage(40),
        Constraint::Min(4),
    ])
    .split(columns[1]);
    view.page_rows = usize::from(if view.values_focused {
        right[1].height
    } else {
        columns[0].height
    })
    .saturating_sub(2)
    .max(1);
    let detail = if let Some(call) = view
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
            describe::signature(view.record, call.import)
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
        Paragraph::new("↑↓/jk move  Tab pane  ↵ open  ⌫ up  PgUp/Dn Home/End  r replay  q quit")
            .style(Style::default().fg(MUTED)),
        footer[1],
    );
}

fn timeline(frame: &mut Frame<'_>, view: &View<'_>, area: Rect) {
    let height = usize::from(area.height.saturating_sub(2));
    let start = view.selected.saturating_sub(height / 2);
    let items: Vec<_> = (start..view.calls.len() + 2)
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
    let mut state = ListState::default().with_selected(Some(view.selected - start));
    frame.render_stateful_widget(
        List::new(items)
            .block(panel(" EVENTS ", !view.values_focused))
            .highlight_symbol("▸ ")
            .highlight_style(Style::default().fg(GREEN).bg(Color::Rgb(23, 44, 43))),
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
    let block = panel(section.title, view.values_focused).title_bottom(section.note);
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
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{}[{index}]  ", section.label),
                    Style::default().fg(MUTED),
                ),
                Span::raw(describe::value(view.record, value)),
            ]))
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
                        |entry| describe::function(view.record, entry.entry)
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
                vec![
                    Span::styled(
                        format!("{direction} {label}[{index}]  "),
                        Style::default().fg(color),
                    ),
                    Span::raw(describe::value(view.record, value)),
                ]
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
                    view.values_focused,
                )
                .title_bottom("→ sent · ← returned before checks"),
            )
            .highlight_symbol("▸ ")
            .highlight_style(
                Style::default()
                    .add_modifier(Modifier::BOLD)
                    .bg(Color::Rgb(23, 44, 43)),
            ),
        area,
        &mut state,
    );
}

fn source(frame: &mut Frame<'_>, view: &View<'_>, area: Rect) {
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
    let lines: Vec<_> = source
        .instrs()
        .skip(start)
        .take(height)
        .map(|instruction| {
            let current = Some(instruction.pc()) == location.map(|loc| loc.pc);
            let (cue, color) = if let Some(edge) = instruction.edge(0) {
                let backwards = edge.target <= instruction.pc()
                    || instruction
                        .edge(1)
                        .is_some_and(|edge| edge.target <= instruction.pc());
                (if backwards { "↰" } else { "↳" }, BLUE)
            } else {
                match instruction.operands() {
                    Operands::Call(call) => (
                        "⇢",
                        match call.callee {
                            CallTarget::HostSig(..) => GREEN,
                            _ => BLUE,
                        },
                    ),
                    Operands::Ret { .. } => ("↵", GOLD),
                    Operands::Block { .. } => ("◆", BLUE),
                    _ => (" ", FG),
                }
            };
            Line::styled(
                format!(
                    "{} {:06}  {}  {cue}",
                    if current { "▸" } else { " " },
                    instruction.pc(),
                    describe::instruction(view.record, instruction)
                ),
                Style::default()
                    .fg(if current { GOLD } else { color })
                    .bg(if current { Color::Rgb(43, 35, 24) } else { BG }),
            )
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines).block(
            panel(
                format!(
                    " SOURCE · {} · {} ",
                    describe::function(view.record, source.func()),
                    if view.selected == 0 {
                        "entry context"
                    } else {
                        "rN = guest register"
                    }
                ),
                false,
            )
            .title_bottom("↰ back / ↳ forward edge · static code"),
        ),
        area,
    );
}
