// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use super::model::View;
use execution_tape::host::HostError;
use execution_tape::vm::Trap;
use execution_tape_recording::{
    Divergence, HostOutcome, Outcome, RecordedValue, ReplayBoundary, ReplayError,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};

const BG: Color = Color::Rgb(12, 18, 27);
const FG: Color = Color::Rgb(198, 212, 219);
const MUTED: Color = Color::Rgb(105, 131, 148);
const GREEN: Color = Color::Rgb(120, 222, 174);
const BLUE: Color = Color::Rgb(123, 181, 233);
const GOLD: Color = Color::Rgb(242, 196, 115);

pub(super) fn preview(text: &str, limit: usize) -> String {
    let mut output: String = text
        .chars()
        .take(limit)
        .flat_map(char::escape_debug)
        .collect();
    if text.chars().nth(limit).is_some() {
        output.push('…');
    }
    output
}

fn value_text(value: &RecordedValue) -> String {
    match value {
        RecordedValue::Str(value) => format!("str[{}] \"{}\"", value.len(), preview(value, 60)),
        RecordedValue::Bytes(value) => {
            let bytes: String = value
                .iter()
                .take(16)
                .map(|byte| format!("{byte:02x} "))
                .collect();
            format!(
                "bytes[{}] {bytes}{}",
                value.len(),
                if value.len() > 16 { "…" } else { "" }
            )
        }
        RecordedValue::Agg(id) => format!("@{}  aggregate  ↵ open", id.get()),
        RecordedValue::Closure { func, env } => {
            format!("closure f{}  env @{}  ↵ open", func.0, env.get())
        }
        RecordedValue::F64(bits) => format!("f64 {:?}  bits {bits:016x}", f64::from_bits(*bits)),
        _ => format!("{value:?}"),
    }
}

fn host_error(error: &HostError) -> &'static str {
    match error {
        HostError::UnknownSymbol => "unknown symbol",
        HostError::SignatureMismatch => "signature mismatch",
        HostError::ProgramMismatch => "program mismatch",
        HostError::Failed => "host failed",
        HostError::Custom(_) => "typed host failure (payload retained)",
    }
}

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
            };
            format!("FAILED · {boundary}{location} · {reason}")
        }
    }
}

fn registers(mut values: impl Iterator<Item = u32>) -> String {
    let visible: Vec<_> = values.by_ref().take(8).collect();
    let omitted = values.count();
    if omitted == 0 {
        format!("{visible:?}")
    } else {
        format!("{visible:?} +{omitted} more")
    }
}

fn import_name(view: &View<'_>, index: usize) -> String {
    let program = view.record.program().program();
    program
        .host_sig(view.calls[index].import)
        .and_then(|entry| program.symbol_str(entry.symbol).ok())
        .map_or_else(|| "<unknown import>".into(), |name| preview(name, 32))
}

fn title(view: &View<'_>, index: usize) -> String {
    if index == 0 {
        return view.record.invocation().map_or_else(
            || "ENTRY · missing".into(),
            |entry| format!("ENTRY · f{} · {} args", entry.entry.0, entry.args.len()),
        );
    }
    if index <= view.calls.len() {
        return format!("#{:04}  {}", index - 1, import_name(view, index - 1));
    }
    match view.record.outcome() {
        Some(Outcome::Returned(values)) => format!("RETURN · {} values", values.len()),
        Some(Outcome::Trapped(_)) => "TRAP".into(),
        None => "OUTCOME · missing".into(),
    }
}

fn panel(title: impl Into<Line<'static>>, focused: bool) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(if focused { GREEN } else { MUTED }))
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
        Constraint::Length(3),
        Constraint::Min(10),
        Constraint::Length(3),
    ])
    .split(area);
    let stats = view.record.stats();
    let complete = view.record.status().is_ok();
    let header = vec![
        Line::from(vec![
            Span::styled(
                " EXECUTION / OBSERVATORY ",
                Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                if complete { " COMPLETE " } else { " PARTIAL " },
                Style::default()
                    .fg(BG)
                    .bg(if complete { GREEN } else { GOLD }),
            ),
        ]),
        Line::from(format!(
            " {} calls  ·  {} graph nodes  ·  {} values  ·  {} payload bytes",
            stats.host_calls, stats.aggregates, stats.values, stats.payload_bytes
        )),
    ];
    frame.render_widget(Paragraph::new(header), rows[0]);
    let columns =
        Layout::horizontal([Constraint::Percentage(30), Constraint::Percentage(70)]).split(rows[1]);
    timeline(frame, view, columns[0]);
    let right = Layout::vertical([
        Constraint::Length(3),
        Constraint::Percentage(50),
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
        let outcome = match call.outcome {
            HostOutcome::Returned { values, extra_fuel } => {
                format!("{} raw returns · extra fuel {extra_fuel}", values.len())
            }
            HostOutcome::Failed(error) => host_error(error).into(),
        };
        format!(
            "{} · f{} pc {:06} · import {}\n{outcome}",
            title(view, view.selected),
            call.location.func.0,
            call.location.pc,
            call.import.0
        )
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
        Paragraph::new(
            "↑↓/jk move  Tab focus  Enter open  Backspace up  PgUp/Dn Home/End  r replay  q quit",
        )
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
            let position = index
                .checked_sub(1)
                .and_then(|index| view.calls.get(index))
                .map_or_else(String::new, |call| {
                    format!("   f{}:{:06}", call.location.func.0, call.location.pc)
                });
            ListItem::new(Line::from(vec![
                Span::raw(title(view, index)),
                Span::styled(position, Style::default().fg(MUTED)),
            ]))
        })
        .collect();
    let mut state = ListState::default().with_selected(Some(view.selected - start));
    frame.render_stateful_widget(
        List::new(items)
            .block(panel(" BOUNDARIES ", !view.values_focused))
            .highlight_symbol("▸ ")
            .highlight_style(Style::default().fg(GREEN).bg(Color::Rgb(23, 44, 43))),
        area,
        &mut state,
    );
}

fn values(frame: &mut Frame<'_>, view: &View<'_>, area: Rect) {
    let height = usize::from(area.height.saturating_sub(2));
    let start = view.value_selected.saturating_sub(height / 2);
    let path = view
        .path
        .last()
        .and_then(|&(id, _)| view.record.aggregate(id))
        .map_or_else(
            || " VALUES · roots ".into(),
            |node| {
                format!(
                    " VALUES · @{} · {:?} · {} ",
                    node.id.get(),
                    node.kind,
                    if node.initial {
                        "initial"
                    } else {
                        "guest-created"
                    }
                )
            },
        );
    let items: Vec<_> = view
        .values()
        .skip(start)
        .take(height)
        .map(|(label, index, value)| {
            ListItem::new(Line::from(vec![
                Span::styled(format!("{label}[{index}]  "), Style::default().fg(BLUE)),
                Span::raw(value_text(value)),
            ]))
        })
        .collect();
    let mut state = ListState::default().with_selected(if items.is_empty() {
        None
    } else {
        Some(view.value_selected - start)
    });
    frame.render_stateful_widget(
        List::new(items)
            .block(panel(path, view.values_focused))
            .highlight_symbol("▸ ")
            .highlight_style(Style::default().fg(GREEN)),
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
            let reads = registers(instruction.reads());
            let writes = registers(instruction.writes());
            Line::styled(
                format!(
                    "{} {:06}  {:?}  read {reads} → write {writes}",
                    if current { "▸" } else { " " },
                    instruction.pc(),
                    instruction.opcode()
                ),
                Style::default().fg(if current { GOLD } else { MUTED }),
            )
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines).block(panel(
            format!(
                " SOURCE · f{} · {} ",
                source.func().0,
                if view.selected == 0 {
                    "entry context"
                } else {
                    "register IDs"
                }
            ),
            false,
        )),
        area,
    );
}
