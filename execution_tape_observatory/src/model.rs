// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use execution_tape::disasm::{FunctionDisassembly, disassemble_function};
use execution_tape::trace::TraceLocation;
use execution_tape_recording::{
    AggregateId, HostCall, HostOutcome, Outcome, RecordedValue, Recording, ReplayOutcome,
};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub(super) struct View<'a> {
    pub(super) record: &'a Recording,
    pub(super) calls: Vec<HostCall<'a>>,
    pub(super) selected: usize,
    pub(super) value_selected: usize,
    pub(super) values_focused: bool,
    pub(super) path: Vec<(AggregateId, usize)>,
    pub(super) replay: String,
    pub(super) source: Option<FunctionDisassembly<'a>>,
    pub(super) source_index: usize,
    pub(super) page_rows: usize,
}

impl<'a> View<'a> {
    pub(super) fn new(record: &'a Recording) -> Self {
        let mut view = Self {
            record,
            calls: record.host_calls().collect(),
            selected: 0,
            value_selected: 0,
            values_focused: false,
            path: Vec::new(),
            replay: "not checked · r to replay".into(),
            source: None,
            source_index: 0,
            page_rows: 10,
        };
        view.select(0);
        view
    }

    pub(super) fn location(&self) -> Option<TraceLocation> {
        if self.selected == 0 {
            return self.record.invocation().map(|entry| TraceLocation {
                func: entry.entry,
                pc: 0,
            });
        }
        if let Some(call) = self.calls.get(self.selected - 1) {
            return Some(call.location);
        }
        self.record.outcome_location()
    }

    pub(super) fn select(&mut self, selected: usize) {
        let previous = self.location();
        self.selected = selected.min(self.calls.len() + 1);
        self.path.clear();
        self.value_selected = 0;
        let location = self.location();
        if self.source.as_ref().map(FunctionDisassembly::func) != location.map(|loc| loc.func) {
            self.source = location.and_then(|loc| {
                disassemble_function(self.record.program().program(), loc.func).ok()
            });
        }
        if location != previous {
            self.source_index = self
                .source
                .as_ref()
                .and_then(|source| {
                    source.instrs().position(|instruction| {
                        Some(instruction.pc()) == location.map(|loc| loc.pc)
                    })
                })
                .unwrap_or(0);
        }
    }

    /// Root values or the immediate fields of one opened aggregate; no recursive expansion.
    pub(super) fn values(
        &self,
    ) -> impl Iterator<Item = (&'static str, usize, &'a RecordedValue)> + '_ {
        let (label, first, second): (_, &[RecordedValue], &[RecordedValue]) =
            if let Some(&(id, _)) = self.path.last() {
                (
                    "field",
                    self.record.aggregate(id).map_or(&[], |node| node.values),
                    &[],
                )
            } else if self.selected == 0 {
                (
                    "arg",
                    self.record.invocation().map_or(&[], |entry| entry.args),
                    &[],
                )
            } else if let Some(call) = self.calls.get(self.selected - 1) {
                let returns = match call.outcome {
                    HostOutcome::Returned { values, .. } => values,
                    HostOutcome::Failed(_) => &[],
                };
                ("arg", call.args, returns)
            } else {
                (
                    "ret",
                    match self.record.outcome() {
                        Some(Outcome::Returned(values)) => values,
                        _ => &[],
                    },
                    &[],
                )
            };
        first
            .iter()
            .enumerate()
            .map(move |(index, value)| (label, index, value))
            .chain(
                second
                    .iter()
                    .enumerate()
                    .map(|(index, value)| ("ret", index, value)),
            )
    }

    fn move_by(&mut self, amount: usize, forward: bool) {
        let current = if self.values_focused {
            self.value_selected
        } else {
            self.selected
        };
        let next = if forward {
            current.saturating_add(amount)
        } else {
            current.saturating_sub(amount)
        };
        if self.values_focused {
            let count = self.values().size_hint().0;
            self.value_selected = next.min(count.saturating_sub(1));
        } else {
            self.select(next);
        }
    }

    pub(super) fn key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('q') => return false,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return false,
            KeyCode::Tab => self.values_focused = !self.values_focused,
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1, true),
            KeyCode::Up | KeyCode::Char('k') => self.move_by(1, false),
            KeyCode::PageDown => self.move_by(self.page_rows, true),
            KeyCode::PageUp => self.move_by(self.page_rows, false),
            KeyCode::Home => self.move_by(usize::MAX, false),
            KeyCode::End => self.move_by(usize::MAX, true),
            KeyCode::Enter => {
                let id =
                    self.values()
                        .nth(self.value_selected)
                        .and_then(|(_, _, value)| match value {
                            RecordedValue::Agg(id) => Some(*id),
                            RecordedValue::Closure { env, .. } => Some(*env),
                            _ => None,
                        });
                if let Some(id) = id {
                    self.path.push((id, self.value_selected));
                    self.value_selected = 0;
                    self.values_focused = true;
                }
            }
            KeyCode::Backspace | KeyCode::Esc => {
                self.value_selected = self.path.pop().map_or(0, |(_, selected)| selected);
            }
            KeyCode::Char('r') => {
                self.replay = match self.record.replay() {
                    Ok(ReplayOutcome::Returned(values)) => format!(
                        "MATCHED · {} roots · {} nodes",
                        values.values().len(),
                        values.aggregates().len_u32()
                    ),
                    Ok(ReplayOutcome::Trapped(trap)) => format!(
                        "MATCHED trap · f{}:{:06} · {}",
                        trap.func.0,
                        trap.pc,
                        super::render::trap_text(&trap.trap)
                    ),
                    Err(error) => super::render::replay_error(&error),
                };
            }
            _ => {}
        }
        true
    }
}
