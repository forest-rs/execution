// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use super::{Browser, Focus, Section};
use crate::{
    describe,
    palette::*,
    render::{panel, too_small},
    syntax,
};
use execution_tape::disasm::{CallTarget, FunctionDisassembly, InputIndex, InstrView, Operands};
use execution_tape::opcode::Opcode;
use execution_tape::program::{ConstEntry, ConstId, HostSigId, Program};
use execution_tape::verifier::{FactOrigin, InstructionFacts, ValueFact};
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

pub(super) fn function_signature(view: &Browser<'_>) -> Option<String> {
    let func = view.function?;
    let function = view.program.functions.get(func.0 as usize)?;
    Some(
        match (
            function.arg_contracts(view.program),
            function.ret_contracts(view.program),
        ) {
            (Ok(args), Ok(rets)) => format!(
                "{}{} → {}",
                view.function_name(func),
                describe::contracts(view.program, args),
                describe::contracts(view.program, rets)
            ),
            _ => "Malformed function signature".into(),
        },
    )
}

pub(super) fn details(view: &Browser<'_>) -> Vec<String> {
    let mut lines = Vec::new();
    let program = view.program;
    if let Some(rejection) = &view.rejection {
        lines.push(format!("VERIFICATION REJECTED: {}", rejection.summary));
        if let Some(path) = &rejection.path {
            lines.push(path.clone());
        }
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
            lines.push(view.signature.clone().unwrap_or_default());
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
                if view.facts_pc == Some(instruction.pc()) {
                    lines.extend(view.fact_lines.iter().cloned());
                }
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
                            if let Some(callee) = program.functions.get(id.0 as usize) {
                                result_contracts(
                                    &mut lines,
                                    program,
                                    call.rets,
                                    callee.ret_types,
                                    "Results on successful return:",
                                );
                            }
                        }
                        CallTarget::Indirect { call_sig, .. } => lines.push(format!(
                            "Indirect signature #{} · runtime callee unknown",
                            call_sig.0
                        )),
                    }
                }
                if let Operands::Ret { rets, .. } = instruction.operands() {
                    result_contracts(
                        &mut lines,
                        program,
                        rets,
                        function.ret_types,
                        "Required returned values:",
                    );
                }
                if instruction.edge(0).is_some() {
                    lines.push("Enter: jump / true arm · f: false arm · Backspace: return".into());
                }
                lines.push("Storage kinds (fixed register declarations):".into());
                let (registers, truncated) = operand_registers(instruction);
                for &reg in registers.iter().take(64) {
                    let kind = instruction
                        .register_type(reg)
                        .map_or_else(|| "invalid / unavailable".into(), describe::type_name);
                    lines.push(format!(
                        "  r{reg}: {kind}{}",
                        if reg == 0 { " · effect token" } else { "" }
                    ));
                }
                if truncated || registers.len() > 64 {
                    lines.push(
                        "… additional operands omitted (scan limit: 64 reads / 64 writes)".into(),
                    );
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

// Keep destinations visible even when a wide instruction has more than 64 inputs. Counts are
// exact for an untruncated sample; otherwise explicitly disclose the operand scan limit.
fn operand_registers(instruction: InstrView<'_>) -> (Vec<u32>, bool) {
    let mut writes = instruction.writes();
    let mut reads = instruction.reads();
    let mut seen = std::collections::BTreeSet::new();
    let mut registers = Vec::new();
    for reg in writes.by_ref().take(64).chain(reads.by_ref().take(64)) {
        if seen.insert(reg) {
            registers.push(reg);
        }
    }
    let truncated = writes.next().is_some() | reads.next().is_some();
    (registers, truncated)
}

fn value_fact(table: &execution_tape::contract::ContractTable, fact: ValueFact) -> String {
    match fact {
        ValueFact::Unreachable => "unreachable".into(),
        ValueFact::Uninitialized => "uninitialized".into(),
        ValueFact::Contract(id) => {
            if table.get(id)
                == Some(execution_tape::contract::Contract::Kind(
                    execution_tape::program::ValueType::Agg,
                ))
            {
                "aggregate (shape unknown)".into()
            } else {
                describe::contract(table, id)
            }
        }
    }
}

pub(super) fn facts(
    facts: &InstructionFacts<'_>,
    instruction: InstrView<'_>,
    program: &Program,
    source: &FunctionDisassembly<'_>,
) -> Vec<String> {
    if !facts.reachable() {
        return vec!["Verifier facts: unreachable instruction; no value guarantees.".into()];
    }
    let mut lines = vec!["Verifier facts · before → after local instruction:".into()];
    let (registers, truncated) = operand_registers(instruction);
    let registers: Vec<_> = registers.into_iter().filter(|&reg| reg != 0).collect();
    for &reg in registers.iter().take(4) {
        let Some(fact) = facts.register(reg) else {
            continue;
        };
        lines.push(format!(
            "  r{reg}: {} → {}",
            value_fact(facts.contracts(), fact.before),
            value_fact(facts.contracts(), fact.after)
        ));
        if fact.before != fact.after {
            let origin = match fact.origin_before {
                Some(FactOrigin::Argument { index }) => {
                    Some(format!("argument {}", u64::from(index) + 1))
                }
                Some(FactOrigin::Instruction { pc }) => source
                    .instruction(pc)
                    .map(|i| format!("{:?} at byte {pc}", i.opcode())),
                Some(FactOrigin::BlockEntry { pc }) => {
                    Some(format!("incoming control flow at byte {pc}"))
                }
                _ => None,
            };
            if let Some(origin) = origin {
                lines.push(format!("    Before from {origin}"));
            }
        }
    }
    if truncated {
        lines.push("  … additional operands omitted (scan limit: 64 reads / 64 writes)".into());
    } else if registers.len() > 4 {
        lines.push(format!(
            "  … {} more operand registers",
            registers.len() - 4
        ));
    }
    let first_read = instruction.reads().next();
    let index = match instruction.input_index() {
        Some(InputIndex::Index(index)) => Some(index),
        _ => None,
    };
    let why = match (instruction.opcode(), first_read, index) {
        (Opcode::TupleGet, Some(reg), Some(index)) => Some(format!(
            "project tuple element {index} from incoming r{reg}"
        )),
        (Opcode::StructGet, Some(reg), Some(index)) => {
            Some(format!("project field {index} from incoming r{reg}"))
        }
        (Opcode::ArrayGet | Opcode::ArrayGetImm, Some(reg), _) => {
            Some(format!("project an element from incoming r{reg}"))
        }
        (Opcode::BrArgs | Opcode::JmpArgs, _, _) => {
            Some("edge bindings are applied at the target, after this local state".into())
        }
        (Opcode::Mov, Some(reg), _) => Some(format!("copy incoming r{reg}")),
        (Opcode::Select, _, _) => Some("keep contracts common to both alternatives".into()),
        (Opcode::Block, _, _) => {
            Some("admit simultaneous edge transfers; keep common contracts".into())
        }
        (Opcode::TupleNew | Opcode::StructNew | Opcode::ArrayNew, _, _) => {
            Some("construct an aggregate from incoming register contracts".into())
        }
        _ => match instruction.operands() {
            Operands::Call(call) => Some(match call.callee {
                CallTarget::Func(id) => format!(
                    "result contracts enforced by {}",
                    describe::function(program, id)
                ),
                _ => "ABI kinds establish no nested aggregate structure".into(),
            }),
            _ => None,
        },
    };
    if let Some(why) = why {
        lines.push(format!("  Why: {why}"));
    }
    if facts.is_block_entry() && facts.incoming_edges().len() > 0 {
        lines.push("Static incoming arms (common facts above):".into());
        if facts.pc() == 0 {
            lines.push("  Also seeded by invocation arguments and the effect token.".into());
        }
        for edge in facts.incoming_edges().take(4) {
            let branch = source.instruction(edge.source_pc()).map(|i| i.opcode());
            let arm = if matches!(branch, Some(Opcode::Br | Opcode::BrArgs)) {
                if edge.arm() == 0 { "true" } else { "false" }
            } else {
                "jump"
            };
            lines.push(format!("  From byte {} · {arm} arm", edge.source_pc()));
            if edge.bindings().len() > 0 {
                for binding in edge.bindings().take(4) {
                    lines.push(format!(
                        "    r{} ← r{}: {}",
                        binding.parameter,
                        binding.argument,
                        value_fact(facts.contracts(), binding.value)
                    ));
                }
                if edge.bindings().len() > 4 {
                    lines.push(format!("    … {} more bindings", edge.bindings().len() - 4));
                }
            } else {
                for &reg in registers.iter().take(4) {
                    if let Some(value) = edge.register(reg) {
                        lines.push(format!(
                            "    r{reg}: {}",
                            value_fact(facts.contracts(), value)
                        ));
                    }
                }
            }
        }
        if facts.incoming_edges().len() > 4 {
            lines.push(format!(
                "  … {} more incoming arms",
                facts.incoming_edges().len() - 4
            ));
        }
    }
    lines
}

fn result_contracts(
    lines: &mut Vec<String>,
    program: &Program,
    registers: &[u32],
    range: execution_tape::program::ByteRange,
    title: &str,
) {
    if registers.is_empty() && range.len == 0 {
        return;
    }
    lines.push(title.into());
    // The verifier diagnoses the whole program once. Read only the bounded visible definitions
    // here rather than validating every ID in a large callee signature on every redraw.
    let Some(ids) = range.offset.checked_add(range.len).and_then(|end| {
        program
            .contract_ids
            .get(range.offset as usize..end as usize)
    }) else {
        lines.push("  <malformed result contract range>".into());
        return;
    };
    for (&register, &id) in registers.iter().zip(ids).take(4) {
        lines.push(format!(
            "  r{register}: {}",
            describe::contract(&program.contracts, id)
        ));
    }
    if registers.len() != ids.len() {
        lines.push(format!(
            "  Result count mismatch: {} registers, {} contracts",
            registers.len(),
            ids.len()
        ));
    }
    if ids.len().min(registers.len()) > 4 {
        lines.push(format!(
            "  … {} more results",
            ids.len().min(registers.len()) - 4
        ));
    }
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
    let summary = view.rejection.as_ref().map_or_else(
        || {
            format!(
                "{} functions · {} imports · {} constants",
                view.program.functions.len(),
                view.program.host_sigs.len(),
                view.program.const_pool.len()
            )
        },
        |rejection| rejection.summary.clone(),
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
