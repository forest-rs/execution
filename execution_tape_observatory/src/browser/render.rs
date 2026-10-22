// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use super::{Browser, Focus, Section};
use crate::{
    describe,
    palette::*,
    render::{panel, too_small},
    syntax,
};
use execution_tape::disasm::{CallTarget, Operands};
use execution_tape::program::{ConstEntry, ConstId, HostSigId, Program};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph, Wrap},
};

pub(super) fn constant(program: &Program, index: usize) -> String {
    let id = ConstId(u32::try_from(index).unwrap_or(u32::MAX));
    match program.const_pool.get(index) {
        Some(ConstEntry::Unit) => "()".into(),
        Some(ConstEntry::Bool(value)) => value.to_string(),
        Some(ConstEntry::I64(value)) => value.to_string(),
        Some(ConstEntry::U64(value)) => format!("{value}u64"),
        Some(ConstEntry::F64(bits)) => format!("{:?} · bits {bits:016x}", f64::from_bits(*bits)),
        Some(ConstEntry::Decimal { mantissa, scale }) => format!("{mantissa}e-{scale}"),
        Some(ConstEntry::Str(_)) => program.const_str(id).map_or_else(
            |_| "invalid text range".into(),
            |value| {
                format!(
                    "\"{}\" · {} bytes",
                    describe::preview(value, 80),
                    value.len()
                )
            },
        ),
        Some(ConstEntry::Bytes(_)) => program.const_bytes(id).map_or_else(
            |_| "invalid byte range".into(),
            |value| {
                format!(
                    "{} bytes [{}{}]",
                    value.len(),
                    value
                        .iter()
                        .take(16)
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<Vec<_>>()
                        .join(" "),
                    if value.len() > 16 { " …" } else { "" }
                )
            },
        ),
        None => "unavailable constant".into(),
    }
}

fn function_signature(view: &Browser<'_>) -> Option<String> {
    let func = view.function?;
    let function = view.program.functions.get(func.0 as usize)?;
    Some(
        match (
            function.arg_types(view.program),
            function.ret_types(view.program),
        ) {
            (Ok(args), Ok(rets)) => format!(
                "{}{} → {}",
                view.function_name(func),
                describe::types(args),
                describe::types(rets)
            ),
            _ => "Malformed function signature".into(),
        },
    )
}

pub(super) fn details(view: &Browser<'_>) -> Vec<String> {
    let mut lines = Vec::new();
    let program = view.program;
    if let Some(error) = &view.error {
        lines.push(format!(
            "VERIFICATION REJECTED: {}",
            describe::diagnostic(error)
        ));
        lines.push("d: go to diagnostic · decoded content remains inspectable".into());
    }
    // A focused instruction always describes that instruction, even after browsing another table.
    let section = if view.details_code {
        Section::Functions
    } else {
        view.section
    };
    let selected = view.entries.get(view.selected).copied();
    match section {
        Section::Functions => {
            let Some(func) = view.function else {
                lines.push("No selected function".into());
                return lines;
            };
            lines.push(function_signature(view).unwrap_or_default());
            let function = &program.functions[func.0 as usize];
            lines.push(format!(
                "{} declared registers · {} source bytes",
                function.reg_count, function.bytecode.len
            ));
            let Some(source) = &view.source else {
                if let Some(error) = &view.decode_error {
                    lines.push(format!("Cannot decode function: {error:?}"));
                }
                return lines;
            };
            if let Some(instruction) = source.instrs().nth(view.instruction) {
                lines.push(format!(
                    "f{}:{:06} · {:?}",
                    source.func().0,
                    instruction.pc(),
                    instruction.opcode()
                ));
                let span = view.span(instruction.pc());
                lines.push(span.map_or_else(
                    || "Source span: unavailable".into(),
                    |id| format!("Source span ID: {id} · file mapping unavailable"),
                ));
                if let Operands::Call(call) = instruction.operands() {
                    match call.callee {
                        CallTarget::HostSig(id, _) => lines.push(describe::signature(program, id)),
                        CallTarget::Func(id) => {
                            lines.push(format!("Enter: open {}", describe::function(program, id)));
                        }
                        CallTarget::Indirect { call_sig, .. } => lines.push(format!(
                            "Indirect signature #{} · runtime callee unknown",
                            call_sig.0
                        )),
                    }
                }
                if instruction.edge(0).is_some() {
                    lines.push("Enter: jump / true arm · f: false arm · Backspace: return".into());
                }
                lines.push("Register declarations (static types, not runtime values):".into());
                let mut operands = instruction.reads().chain(instruction.writes());
                let registers: std::collections::BTreeSet<_> = operands.by_ref().take(64).collect();
                for reg in registers {
                    let kind = instruction
                        .register_type(reg)
                        .map_or_else(|| "invalid / unavailable".into(), describe::type_name);
                    lines.push(format!(
                        "  r{reg}: {kind}{}",
                        if reg == 0 { " · effect token" } else { "" }
                    ));
                }
                if operands.next().is_some() {
                    lines.push("… additional operand registers omitted".into());
                }
            }
        }
        Section::Imports => {
            if let Some(index) = selected {
                lines.push(describe::signature(
                    program,
                    HostSigId(u32::try_from(index).unwrap_or(u32::MAX)),
                ));
                lines.push(format!("Program-local import #{index}"));
                lines.push("Host implementation is not loaded or called.".into());
            }
        }
        Section::Constants => {
            if let Some(index) = selected {
                lines.push(format!("Constant #{index}: {}", constant(program, index)));
            }
        }
        Section::Types => {
            if let Some(index) = selected {
                if let Some(st) = program.types.structs.get(index) {
                    match (
                        program.types.struct_field_name_ids(st),
                        program.types.struct_field_types(st),
                    ) {
                        (Ok(names), Ok(types)) => {
                            lines.push(format!("struct #{index} · {} fields", types.len()));
                            for (field, &kind) in types.iter().take(128).enumerate() {
                                let name = names
                                    .get(field)
                                    .and_then(|&id| program.types.field_name_str(id))
                                    .map_or_else(
                                        || format!("field[{field}]"),
                                        |name| describe::preview(name, 60),
                                    );
                                lines.push(format!("  {name}: {}", describe::type_name(kind)));
                            }
                            if types.len() > 128 {
                                lines.push(format!("… {} more fields omitted", types.len() - 128));
                            }
                        }
                        _ => lines.push("Malformed struct field metadata".into()),
                    }
                } else if let Some(&kind) = program
                    .types
                    .array_elems
                    .get(index - program.types.structs.len())
                {
                    lines.push(format!(
                        "Array element type #{}: {}",
                        index - program.types.structs.len(),
                        describe::type_name(kind)
                    ));
                }
            }
        }
    }
    lines
}

pub(super) fn draw(frame: &mut Frame<'_>, view: &mut Browser<'_>) {
    let area = frame.area();
    frame.render_widget(
        ratatui::widgets::Block::default().style(Style::default().bg(BG).fg(FG)),
        area,
    );
    if too_small(frame) {
        return;
    }
    let rows = Layout::vertical([
        Constraint::Length(4),
        Constraint::Min(10),
        Constraint::Length(3),
    ])
    .split(area);
    let status = if view.error.is_some() {
        "REJECTED"
    } else {
        "VERIFIED"
    };
    let summary = view.error.as_ref().map_or_else(
        || {
            format!(
                "{} functions · {} imports · {} constants",
                view.program.functions.len(),
                view.program.host_sigs.len(),
                view.program.const_pool.len()
            )
        },
        describe::diagnostic,
    );
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                format!(" EXECUTION / OBSERVATORY · PROGRAM · {status}"),
                Style::default()
                    .fg(if view.error.is_some() { ROSE } else { GREEN })
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(format!(
                " {}",
                view.program
                    .program_name
                    .and_then(|id| view.program.symbol_str(id).ok())
                    .map_or_else(
                        || "Unnamed compiled program".into(),
                        |name| describe::preview(name, 100)
                    )
            )),
            Line::from(format!(" {summary}")),
            Line::from(Span::styled(
                format!(" {}", view.message),
                Style::default().fg(MUTED),
            )),
        ]),
        rows[0],
    );
    let columns =
        Layout::horizontal([Constraint::Percentage(30), Constraint::Percentage(70)]).split(rows[1]);
    let right = Layout::vertical([Constraint::Percentage(65), Constraint::Percentage(35)])
        .split(columns[1]);
    view.page_rows = usize::from(
        match view.focus {
            Focus::Navigator => columns[0].height,
            Focus::Code => right[0].height,
            Focus::Details => right[1].height,
        }
        .saturating_sub(2),
    )
    .max(1);
    navigator(frame, view, columns[0]);
    code(frame, view, right[0]);
    let details = Paragraph::new(details(view).join("\n")).wrap(Wrap { trim: false });
    let detail_lines = details.line_count(right[1].width.saturating_sub(2));
    let visible_lines = usize::from(right[1].height.saturating_sub(2));
    view.details_end =
        u16::try_from(detail_lines.saturating_sub(visible_lines)).unwrap_or(u16::MAX);
    view.details_offset = view.details_offset.min(view.details_end);
    frame.render_widget(
        details.scroll((view.details_offset, 0)).block(
            panel(" DETAILS ", view.focus == Focus::Details).title_bottom(format!(
                " lines {}–{} / {detail_lines} · ↑/↓ scroll ",
                usize::from(view.details_offset) + usize::from(detail_lines != 0),
                (usize::from(view.details_offset) + visible_lines).min(detail_lines)
            )),
        ),
        right[1],
    );
    frame.render_widget(Paragraph::new("Tab focus · [ / ] tables · / search · arrows navigate · Enter follow\nf false arm · Backspace back · d diagnostic · q quit").style(Style::default().fg(MUTED)), rows[2]);
}

fn navigator(frame: &mut Frame<'_>, view: &Browser<'_>, area: Rect) {
    let block = panel(
        format!(" {} · {} ", view.section.title(), view.entries.len()),
        view.focus == Focus::Navigator,
    )
    .title_bottom(if view.searching {
        format!(
            " /{}_ · Enter apply · Esc clear ",
            describe::preview(&view.filter, 24)
        )
    } else if !view.filter.is_empty() {
        format!(" /{} · Esc clear ", describe::preview(&view.filter, 24))
    } else {
        " [ / ] switch table · / search ".into()
    });
    let height = usize::from(area.height.saturating_sub(2));
    let start = view.selected.saturating_sub(height / 2);
    let items: Vec<_> = view
        .entries
        .iter()
        .skip(start)
        .take(height)
        .map(|&index| ListItem::new(view.label(index)))
        .collect();
    let mut state = ListState::default().with_selected(if view.entries.is_empty() {
        None
    } else {
        Some(view.selected - start)
    });
    frame.render_stateful_widget(
        List::new(items)
            .block(block)
            .highlight_symbol("▸ ")
            .highlight_style(Style::default().fg(GOLD).add_modifier(Modifier::BOLD)),
        area,
        &mut state,
    );
}

fn code(frame: &mut Frame<'_>, view: &Browser<'_>, area: Rect) {
    let title = view.source.as_ref().map_or_else(
        || " INSTRUCTIONS ".into(),
        |source| format!(" INSTRUCTIONS · {} ", view.function_name(source.func())),
    );
    let block = panel(title, view.focus == Focus::Code).title_bottom(if view.flow.omitted() == 0 {
        " static flow · T/F arms · ▶ target ".into()
    } else {
        format!(" static flow · {} edges hidden ", view.flow.omitted())
    });
    let Some(source) = &view.source else {
        if let Some(error) = &view.decode_error {
            frame.render_widget(
                Paragraph::new(format!("Cannot decode function instructions: {error:?}"))
                    .block(block),
                area,
            );
            return;
        }
        frame.render_widget(
            Paragraph::new("Select a function to inspect its instructions").block(block),
            area,
        );
        return;
    };
    let height = usize::from(area.height.saturating_sub(2));
    let start = view.instruction.saturating_sub(height / 2);
    let first = source.instrs().nth(start).map_or(0, |i| i.pc());
    let last = source
        .instrs()
        .skip(start)
        .take(height)
        .last()
        .map_or(first, |i| i.pc());
    let lines: Vec<_> = source
        .instrs()
        .enumerate()
        .skip(start)
        .take(height)
        .map(|(index, instruction)| {
            let selected = index == view.instruction;
            let mut spans = vec![Span::styled(
                format!(
                    "{} {:06} ",
                    if selected { "▸" } else { " " },
                    instruction.pc()
                ),
                Style::default().fg(if selected { GOLD } else { MUTED }),
            )];
            spans.extend(view.flow.gutter(instruction.pc(), first, last));
            spans.push(Span::raw(" "));
            spans.extend(syntax::source(&describe::instruction(view.program, instruction)).spans);
            Line::from(spans).style(if selected {
                Style::default()
                    .bg(ratatui::style::Color::Indexed(237))
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            })
        })
        .collect();
    if let Some(error) = source.error() {
        frame.render_widget(
            Paragraph::new(format!("Cannot decode function instructions: {error:?}")).block(block),
            area,
        );
    } else {
        frame.render_widget(Paragraph::new(lines).block(block), area);
    }
}
