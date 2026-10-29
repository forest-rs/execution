// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use super::*;
impl<H: Host> LiveExplorer<'_, H> {
    pub(super) fn draw_view(&mut self, frame: &mut Frame<'_>) {
        self.refresh_object();
        let area = frame.area();
        frame.render_widget(Block::default().style(Style::default().bg(BG).fg(FG)), area);
        if render::too_small(frame) {
            return;
        }
        let rows = Layout::vertical([
            Constraint::Length(if self.origin.is_some() { 3 } else { 2 }),
            Constraint::Min(12),
            Constraint::Length(2),
        ])
        .split(area);
        let color = if self.invocation.result().is_some_and(|r| r.is_err()) {
            ROSE
        } else if self.motion.is_some() {
            GOLD
        } else {
            GREEN
        };
        let mut header = vec![
            Line::raw(format!(
                "LIVE · {} · {}",
                self.status,
                describe::preview(
                    self.invocation
                        .program()
                        .program()
                        .name()
                        .unwrap_or("unnamed program"),
                    80
                ),
            )),
            Line::raw(format!(
                "Fuel remaining: {} · work: {} aggregate units / {} payload bytes · {} breakpoints",
                self.invocation.remaining_fuel(),
                self.invocation.data_work().aggregate,
                self.invocation.data_work().payload,
                self.breakpoints.len()
            )),
        ];
        if let Some(origin) = &self.origin {
            header.push(Line::styled(
                format!(
                    "CALLER WAITING · {} · controls run this invocation",
                    describe::preview(&origin.symbol, 24)
                ),
                Style::default().fg(GOLD),
            ));
        }
        frame.render_widget(
            Paragraph::new(header).style(Style::default().fg(color).add_modifier(Modifier::BOLD)),
            rows[0],
        );
        let columns = Layout::horizontal([Constraint::Percentage(32), Constraint::Percentage(68)])
            .split(rows[1]);
        let left = Layout::vertical([Constraint::Percentage(36), Constraint::Percentage(64)])
            .split(columns[0]);
        let right = Layout::vertical([Constraint::Percentage(67), Constraint::Percentage(33)])
            .split(columns[1]);
        self.draw_stack(frame, left[0]);
        self.draw_values(frame, left[1]);
        self.draw_code(frame, right[0]);
        let lines: Vec<_> = self
            .source_lines
            .iter()
            .skip(self.source_offset)
            .map(|text| {
                Line::styled(
                    text.as_str(),
                    Style::default().fg(if text.starts_with("     |") {
                        GOLD
                    } else {
                        CYAN
                    }),
                )
            })
            .collect();
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(render::panel(
                    if self.showing_origin {
                        " AUTHORED SOURCE · external caller "
                    } else {
                        " AUTHORED SOURCE · selected instruction "
                    },
                    self.focus == Focus::Source,
                )),
            right[1],
        );
        frame.render_widget(Paragraph::new("s into  n over  u out  c continue  h dispatch host  Space pause  b breakpoint  g current\nTab pane  ↑↓/jk scroll  o object roots/registers  q leave (drop cancels unfinished execution)")
            .style(Style::default().fg(MUTED)), rows[2]);
    }
    fn draw_stack(&self, frame: &mut Frame<'_>, area: Rect) {
        let mut items: Vec<_> = self
            .frames
            .iter()
            .enumerate()
            .map(|(i, f)| {
                ListItem::new(Line::styled(
                    format!(
                        "#{} {} · {}",
                        f.activation.ordinal(),
                        describe::function(self.invocation.program().program(), f.location.func),
                        if f.waiting_for_return {
                            "waiting for return"
                        } else {
                            "next instruction"
                        }
                    ),
                    Style::default().fg(if i == 0 { GREEN } else { BLUE }),
                ))
            })
            .collect();
        if let Some(origin) = &self.origin {
            items.push(ListItem::new(vec![
                Line::styled(
                    "external caller · waiting for host reply",
                    Style::default().fg(GOLD),
                ),
                Line::styled(
                    format!(
                        "{} → host {}",
                        describe::function(origin.program.program(), origin.location.func),
                        origin.symbol
                    ),
                    Style::default().fg(BLUE),
                ),
            ]));
        }
        let block = render::panel(" LIVE STACK ", self.focus == Focus::Stack).title_bottom(
            if self.origin.is_some() {
                "root ← external host request"
            } else {
                "root ← Application"
            },
        );
        if items.is_empty() {
            frame.render_widget(Paragraph::new("No active guest frames.").block(block), area);
        } else {
            frame.render_stateful_widget(
                List::new(items)
                    .block(block)
                    .highlight_symbol("▸ ")
                    .highlight_style(Style::default().bg(Color::Indexed(23))),
                area,
                &mut ListState::default().with_selected(if self.origin_selected {
                    Some(self.frames.len())
                } else if self.frames.is_empty() {
                    None
                } else {
                    Some(self.selected_frame)
                }),
            );
        }
    }
    fn draw_values(&self, frame: &mut Frame<'_>, area: Rect) {
        let title = if self.origin_selected {
            " EXTERNAL CALLER · values "
        } else if matches!(self.value_path.last(), Some(ValueTarget::Object(_))) {
            " HOST OBJECT · read-only "
        } else if !self.value_path.is_empty() {
            " AGGREGATE CONTENTS "
        } else if self.roots {
            " HOST OBJECT REFERENCES "
        } else if self.invocation.result().is_some() {
            " FINAL OUTCOME "
        } else {
            " PAUSED REGISTERS "
        };
        let note = if matches!(self.value_path.last(), Some(ValueTarget::Object(_))) {
            "host owns state/lifetime · Backspace up"
        } else if self.roots {
            "conservative inventory · host owns collection"
        } else {
            "Enter open · Backspace up · read-only"
        };
        let lines: Vec<_> = self
            .values
            .iter()
            .skip(self.values_offset)
            .enumerate()
            .map(|(i, v)| {
                Line::styled(
                    format!("{}{}", if i == 0 { "▸ " } else { "  " }, v),
                    Style::default().fg(if v.contains("unavailable") {
                        MUTED
                    } else {
                        CYAN
                    }),
                )
            })
            .collect();
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(render::panel(title, self.focus == Focus::Values).title_bottom(note)),
            area,
        );
    }
    fn draw_code(&self, frame: &mut Frame<'_>, area: Rect) {
        let height = usize::from(area.height.saturating_sub(2));
        let start = self.code_selected.saturating_sub(height / 2);
        let first = self.code.get(start).map_or(0, |i| i.0);
        let last = self
            .code
            .iter()
            .skip(start)
            .take(height)
            .next_back()
            .map_or(first, |i| i.0);
        let current = if self.showing_origin {
            self.origin.as_ref().map(|origin| origin.location)
        } else {
            self.frames
                .get(self.selected_frame)
                .map(|f| f.location)
                .or_else(|| {
                    self.invocation
                        .result()
                        .and_then(Result::err)
                        .map(|t| TraceLocation {
                            func: t.func,
                            pc: t.pc,
                        })
                })
        };
        let lines: Vec<_> = self
            .code
            .iter()
            .enumerate()
            .skip(start)
            .take(height)
            .map(|(index, (pc, text))| {
                let active = current.is_some_and(|l| Some(l.func) == self.code_func && l.pc == *pc);
                let point = !self.showing_origin
                    && self
                        .code_func
                        .is_some_and(|f| self.breakpoints.contains(&(f.0, *pc)));
                let mut spans = vec![Span::styled(
                    format!(
                        "{}{} {:06} ",
                        if active { "▸" } else { " " },
                        if point { "●" } else { " " },
                        pc
                    ),
                    Style::default().fg(if point {
                        ROSE
                    } else if active {
                        GOLD
                    } else {
                        MUTED
                    }),
                )];
                spans.extend(self.flow.gutter(*pc, first, last));
                spans.extend(syntax::source(text).spans);
                Line::from(spans).style(if index == self.code_selected {
                    Style::default()
                        .bg(Color::Indexed(237))
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                })
            })
            .collect();
        let function = self.code_func.map_or_else(
            || "no function".into(),
            |f| describe::function(self.code_program(), f),
        );
        frame.render_widget(
            Paragraph::new(lines).block(
                render::panel(
                    format!(
                        " {} · {function} ",
                        if self.showing_origin {
                            "CALLER INSTRUCTIONS"
                        } else {
                            "INSTRUCTIONS"
                        }
                    ),
                    self.focus == Focus::Code,
                )
                .title_bottom(if self.showing_origin {
                    "external request · breakpoints belong to its own invocation"
                } else {
                    "▸ next/pending · ● breakpoint · static flow"
                }),
            ),
            area,
        );
    }
}
