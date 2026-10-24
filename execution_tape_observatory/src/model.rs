// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use super::flow::Flow;
use execution_tape::disasm::{FunctionDisassembly, disassemble_function};
use execution_tape::trace::TraceLocation;
use execution_tape_recording::{
    AggregateId, CallStack, HostCall, HostOutcome, Outcome, RecordedValue, Recording, ReplayOutcome,
};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Focus {
    Events,
    Values,
    Stack,
}

pub(super) struct View<'a> {
    pub(super) record: &'a Recording,
    pub(super) calls: Vec<HostCall<'a>>,
    pub(super) selected: usize,
    pub(super) instruction: Option<usize>,
    pub(super) value_selected: usize,
    pub(super) focus: Focus,
    pub(super) frame_selected: usize,
    pub(super) path: Vec<(AggregateId, usize)>,
    pub(super) replay: String,
    pub(super) source: Option<FunctionDisassembly<'a>>,
    pub(super) source_index: usize,
    pub(super) flow: Flow,
    pub(super) fact_lines: Vec<String>,
    pub(super) source_lines: Vec<String>,
    pub(super) step_condition: Option<u32>,
    pub(super) step_successors: Option<[Option<TraceLocation>; 2]>,
    step_location: Option<TraceLocation>,
    sources: Option<super::sources::BoundSources<'a>>,
    fact_analysis: Option<execution_tape::verifier::FunctionAnalysis<'a>>,
    fact_function: Option<execution_tape::value::FuncId>,
    pub(super) page_rows: usize,
}

impl<'a> View<'a> {
    #[cfg(test)]
    pub(super) fn new(record: &'a Recording) -> Self {
        Self::with_options(record, None, false)
    }

    pub(super) fn with_options(
        record: &'a Recording,
        sources: Option<super::sources::BoundSources<'a>>,
        instructions: bool,
    ) -> Self {
        let mut view = Self {
            record,
            calls: record.host_calls().collect(),
            selected: 0,
            instruction: (instructions && record.instruction_status().is_some()).then_some(0),
            value_selected: 0,
            focus: Focus::Events,
            frame_selected: 0,
            path: Vec::new(),
            replay: "not checked · r to replay".into(),
            source: None,
            source_index: 0,
            flow: Flow::default(),
            fact_lines: Vec::new(),
            source_lines: Vec::new(),
            step_condition: None,
            step_successors: None,
            step_location: None,
            sources,
            fact_analysis: None,
            fact_function: None,
            page_rows: 10,
        };
        view.select(if view.calls.is_empty() { 0 } else { 1 });
        view
    }

    pub(super) fn stack(&self) -> Option<CallStack<'a>> {
        if let Some(index) = self.instruction {
            return self.record.instruction(index).and_then(|step| step.stack);
        }
        if self.selected == 0 {
            None
        } else if let Some(call) = self.calls.get(self.selected - 1) {
            call.stack.clone()
        } else {
            self.record.outcome_stack()
        }
    }

    pub(super) fn location(&self) -> Option<TraceLocation> {
        if let Some(location) = self
            .stack()
            .and_then(|mut stack| stack.nth(self.frame_selected))
        {
            return Some(location);
        }
        if let Some(index) = self.instruction {
            return self.record.instruction(index).map(|step| step.location);
        }
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
        self.frame_selected = 0;
        self.update_source(previous);
    }

    fn update_source(&mut self, previous: Option<TraceLocation>) {
        let location = self.location();
        let source_changed =
            self.source.as_ref().map(FunctionDisassembly::func) != location.map(|loc| loc.func);
        if source_changed {
            self.source = location.and_then(|loc| {
                disassemble_function(self.record.program().program(), loc.func).ok()
            });
            self.flow = self.source.as_ref().map_or_else(Flow::default, Flow::new);
        }
        if location != previous || source_changed {
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
        self.source_lines = self.sources.map_or_else(Vec::new, |sources| {
            location.map_or_else(
                || vec!["Source: no captured instruction location".into()],
                |location| sources.context_lines(location, 2, 48),
            )
        });
        let step_location = self
            .instruction
            .and_then(|index| self.record.instruction(index))
            .map(|step| step.location);
        if step_location != self.step_location {
            self.step_location = step_location;
            let metadata = step_location.and_then(|location| {
                let extract = |source: &FunctionDisassembly<'_>| {
                    let instruction = source.instruction(location.pc)?;
                    let (cond, yes, no) = match instruction.operands() {
                        execution_tape::disasm::Operands::Br {
                            cond,
                            pc_true,
                            pc_false,
                        }
                        | execution_tape::disasm::Operands::BrArgs {
                            cond,
                            pc_true,
                            pc_false,
                            ..
                        } => (cond, pc_true, pc_false),
                        _ => return None,
                    };
                    Some((
                        cond,
                        [source.executable_pc(yes), source.executable_pc(no)].map(|pc| {
                            pc.map(|pc| TraceLocation {
                                func: location.func,
                                pc,
                            })
                        }),
                    ))
                };
                if let Some(source) = self
                    .source
                    .as_ref()
                    .filter(|source| source.func() == location.func)
                {
                    extract(source)
                } else {
                    disassemble_function(self.record.program().program(), location.func)
                        .ok()
                        .and_then(|source| extract(&source))
                }
            });
            self.step_condition = metadata.map(|(cond, _)| cond);
            self.step_successors = metadata.map(|(_, successors)| successors);
        }
        self.fact_lines.clear();
        if self.instruction.is_none() {
            return;
        }
        let Some(location) = location else {
            return;
        };
        if self.fact_function != Some(location.func) {
            self.fact_analysis =
                execution_tape::verifier::ProgramAnalysis::from(self.record.program())
                    .function(location.func);
            self.fact_function = Some(location.func);
        }
        let Some(facts) = self
            .fact_analysis
            .as_mut()
            .and_then(|a| a.instruction(location.pc))
        else {
            return;
        };
        let Some(instruction) = self
            .source
            .as_ref()
            .and_then(|s| s.instruction(location.pc))
        else {
            return;
        };
        let mut seen = std::collections::BTreeSet::new();
        let summary = |value| match value {
            execution_tape::verifier::ValueFact::Unreachable => "unreachable".into(),
            execution_tape::verifier::ValueFact::Uninitialized => "uninitialized".into(),
            execution_tape::verifier::ValueFact::Contract(id) => {
                super::describe::contract(facts.contracts(), id)
            }
        };
        for reg in instruction
            .writes()
            .take(64)
            .chain(instruction.reads().take(64))
            .filter(|&r| r != 0)
            .filter(|r| seen.insert(*r))
            .take(2)
        {
            if let Some(fact) = facts.register(reg) {
                self.fact_lines.push(format!(
                    "r{reg}: {} → {}",
                    summary(fact.before),
                    summary(fact.after)
                ));
            }
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
            } else if self.instruction.is_some() {
                ("reg", &[], &[])
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
        let step = self
            .instruction
            .filter(|_| self.path.is_empty())
            .and_then(|index| self.record.instruction(index));
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
            .chain(step.into_iter().flat_map(|step| {
                step.inputs
                    .iter()
                    .map(|(reg, value)| ("before", reg as usize, value))
                    .chain(
                        step.outputs
                            .iter()
                            .map(|(reg, value)| ("after", reg as usize, value)),
                    )
            }))
    }

    pub(super) fn value_count(&self) -> usize {
        if self.path.is_empty()
            && let Some(index) = self.instruction
        {
            return self
                .record
                .instruction(index)
                .map_or(0, |step| step.inputs.len() + step.outputs.len());
        }
        self.values().size_hint().0
    }

    fn move_by(&mut self, amount: usize, forward: bool) {
        let current = match self.focus {
            Focus::Values => self.value_selected,
            Focus::Events => self.instruction.unwrap_or(self.selected),
            Focus::Stack => self.frame_selected,
        };
        let next = if forward {
            current.saturating_add(amount)
        } else {
            current.saturating_sub(amount)
        };
        match self.focus {
            Focus::Events => {
                if self.instruction.is_some() {
                    let previous = self.location();
                    self.instruction =
                        Some(next.min(self.record.instructions().len().saturating_sub(1)));
                    self.frame_selected = 0;
                    self.value_selected = 0;
                    self.path.clear();
                    self.update_source(previous);
                } else {
                    self.select(next);
                }
            }
            Focus::Values => {
                let count = self.value_count();
                self.value_selected = next.min(count.saturating_sub(1));
            }
            Focus::Stack => {
                let previous = self.location();
                self.frame_selected = next.min(
                    self.stack()
                        .map_or(0, |stack| stack.len())
                        .saturating_sub(1),
                );
                self.update_source(previous);
            }
        }
    }

    pub(super) fn key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('i') => {
                let previous = self.location();
                self.instruction = if self.instruction.is_some() {
                    None
                } else if self.record.instruction_status().is_some() {
                    Some(0)
                } else {
                    None
                };
                self.frame_selected = 0;
                self.value_selected = 0;
                self.path.clear();
                self.update_source(previous);
            }
            KeyCode::Char('q') => return false,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return false,
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Events => Focus::Values,
                    Focus::Values => Focus::Stack,
                    Focus::Stack => Focus::Events,
                }
            }
            KeyCode::BackTab => {
                self.focus = match self.focus {
                    Focus::Events => Focus::Stack,
                    Focus::Stack => Focus::Values,
                    Focus::Values => Focus::Events,
                }
            }
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1, true),
            KeyCode::Up | KeyCode::Char('k') => self.move_by(1, false),
            KeyCode::PageDown => self.move_by(self.page_rows, true),
            KeyCode::PageUp => self.move_by(self.page_rows, false),
            KeyCode::Home => self.move_by(usize::MAX, false),
            KeyCode::End => self.move_by(usize::MAX, true),
            KeyCode::Enter if self.focus != Focus::Stack => {
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
                    self.focus = Focus::Values;
                }
            }
            KeyCode::Backspace | KeyCode::Esc => {
                self.value_selected = self.path.pop().map_or(0, |(_, selected)| selected);
            }
            KeyCode::Char('r') => {
                self.replay = match self.record.replay() {
                    Ok(ReplayOutcome::Returned(values)) => format!(
                        "MATCHED · guest result verified ({} value{})",
                        values.values().len(),
                        if values.values().len() == 1 { "" } else { "s" }
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
