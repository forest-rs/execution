// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Static program navigation. No VM, host, recording or fabricated runtime state.

mod render;
#[cfg(test)]
mod tests;

use super::{Screen, describe, flow::Flow, sources::BoundSources};
use execution_tape::disasm::{
    CallTarget, DisasmError, FunctionDisassembly, Operands, disassemble_function,
};
use execution_tape::program::{HostSigId, Program, SpanId};
use execution_tape::value::FuncId;
use execution_tape::verifier::{FunctionAnalysis, ProgramAnalysis, VerifyConfig, VerifyError};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

const CONTEXT: &str =
    "Static program · no execution · source snapshots require explicit frontend mappings";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Section {
    Functions,
    Imports,
    Constants,
    Types,
}

impl Section {
    const ALL: [Self; 4] = [Self::Functions, Self::Imports, Self::Constants, Self::Types];

    fn title(self) -> &'static str {
        match self {
            Self::Functions => "FUNCTIONS",
            Self::Imports => "IMPORTS",
            Self::Constants => "CONSTANTS",
            Self::Types => "TYPES",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    Navigator,
    Code,
    Details,
}

pub(super) struct Browser<'a> {
    program: &'a Program,
    source_map: Option<BoundSources<'a>>,
    names: Vec<Option<&'a str>>,
    details_code: bool,
    error: Option<VerifyError>,
    analysis: Option<ProgramAnalysis<'a>>,
    function_facts: Option<FunctionAnalysis<'a>>,
    facts_pc: Option<u32>,
    fact_lines: Vec<String>,
    source_lines: Vec<String>,
    rejection: Option<describe::VerificationDiagnostic>,
    section: Section,
    focus: Focus,
    filter: String,
    searching: bool,
    entries: Vec<usize>,
    selected: usize,
    function: Option<FuncId>,
    signature: Option<String>,
    decode_error: Option<DisasmError>,
    source: Option<FunctionDisassembly<'a>>,
    instruction: usize,
    flow: Flow,
    spans: Vec<(u64, SpanId)>,
    history: Vec<(FuncId, usize)>,
    details_offset: u16,
    details_end: u16,
    page_rows: usize,
    message: String,
}

impl<'a> Browser<'a> {
    #[cfg(test)]
    pub(super) fn new(program: &'a Program) -> Self {
        Self::with_sources(program, None)
    }

    pub(super) fn with_sources(program: &'a Program, source_map: Option<BoundSources<'a>>) -> Self {
        let mut names = vec![None; program.functions.len()];
        for entry in &program.function_names {
            if let Some(slot) = names.get_mut(entry.func as usize)
                && slot.is_none()
            {
                *slot = program.symbol_str(entry.name).ok();
            }
        }
        let (analysis, error) = match ProgramAnalysis::new(program, &VerifyConfig::default()) {
            Ok(analysis) => (Some(analysis), None),
            Err(error) => (None, Some(error)),
        };
        let rejection = error
            .as_ref()
            .map(|error| describe::verification(program, error));
        let mut view = Self {
            program,
            source_map,
            names,
            details_code: false,
            error,
            analysis,
            function_facts: None,
            facts_pc: None,
            fact_lines: Vec::new(),
            source_lines: Vec::new(),
            rejection,
            section: Section::Functions,
            focus: Focus::Navigator,
            filter: String::new(),
            searching: false,
            entries: Vec::new(),
            selected: 0,
            function: None,
            signature: None,
            decode_error: None,
            source: None,
            instruction: 0,
            flow: Flow::default(),
            spans: Vec::new(),
            history: Vec::new(),
            details_offset: 0,
            details_end: 0,
            page_rows: 10,
            message: CONTEXT.into(),
        };
        view.refilter();
        view.diagnostic();
        view
    }

    fn count(&self) -> usize {
        match self.section {
            Section::Functions => self.program.functions.len(),
            Section::Imports => self.program.host_sigs.len(),
            Section::Constants => self.program.const_pool.len(),
            Section::Types => {
                self.program.types.structs.len() + self.program.types.array_elems.len()
            }
        }
    }

    fn function_name(&self, id: FuncId) -> String {
        self.names
            .get(id.0 as usize)
            .and_then(|name| *name)
            .map_or_else(
                || format!("function #{}", id.0),
                |name| describe::preview(name, 28),
            )
    }

    fn label(&self, index: usize) -> String {
        match self.section {
            Section::Functions => format!(
                "f{index} {}",
                self.function_name(FuncId(u32::try_from(index).unwrap_or(u32::MAX)))
            ),
            Section::Imports => format!(
                "h{index} {}",
                describe::host(
                    self.program,
                    HostSigId(u32::try_from(index).unwrap_or(u32::MAX))
                )
            ),
            Section::Constants => format!("c{index} {}", render::constant(self.program, index)),
            Section::Types => {
                if index < self.program.types.structs.len() {
                    format!("struct #{index}")
                } else {
                    format!(
                        "array element #{}",
                        index - self.program.types.structs.len()
                    )
                }
            }
        }
    }

    fn rebuild_entries(&mut self) {
        self.entries = (0..self.count())
            .filter(|&index| self.filter.is_empty() || self.label(index).contains(&self.filter))
            .collect();
        self.selected = 0;
    }

    fn refilter(&mut self) {
        self.rebuild_entries();
        self.select();
    }

    fn navigate(&mut self, function: FuncId, instruction: usize) {
        if self.section != Section::Functions || !self.filter.is_empty() {
            self.section = Section::Functions;
            self.filter.clear();
            self.rebuild_entries();
        }
        self.selected = function.0 as usize;
        self.load(function, instruction);
    }

    fn span(&self, pc: u32) -> Option<SpanId> {
        let index = self
            .spans
            .partition_point(|&(start, _)| start <= u64::from(pc));
        self.spans.get(index.checked_sub(1)?).map(|&(_, id)| id)
    }

    fn select(&mut self) {
        self.message = CONTEXT.into();
        self.details_code = false;
        self.details_offset = 0;
        if self.section == Section::Functions {
            if let Some(&index) = self.entries.get(self.selected) {
                self.load(FuncId(u32::try_from(index).unwrap_or(u32::MAX)), 0);
            } else {
                self.function = None;
                self.signature = None;
                self.function_facts = None;
                self.facts_pc = None;
                self.fact_lines.clear();
                self.source_lines.clear();
                self.decode_error = None;
                self.source = None;
                self.flow = Flow::default();
                self.spans.clear();
            }
        }
    }

    fn load(&mut self, function: FuncId, instruction: usize) {
        self.message = CONTEXT.into();
        if self.function != Some(function) {
            self.function = Some(function);
            self.signature = render::function_signature(self);
            self.function_facts = self
                .analysis
                .as_ref()
                .and_then(|analysis| analysis.function(function));
            self.facts_pc = None;
            self.spans = self
                .program
                .functions
                .get(function.0 as usize)
                .and_then(|function| function.spans(self.program).ok())
                .map_or_else(Vec::new, |spans| {
                    spans
                        .iter()
                        .scan(0_u64, |pc, entry| {
                            *pc = pc.checked_add(entry.pc_delta)?;
                            Some((*pc, entry.span_id))
                        })
                        .collect()
                });
            match disassemble_function(self.program, function) {
                Ok(source) => {
                    self.source = Some(source);
                    self.decode_error = None;
                }
                Err(error) => {
                    self.source = None;
                    self.decode_error = Some(error);
                }
            }
            self.flow = self.source.as_ref().map_or_else(Flow::default, Flow::new);
        }
        let count = self
            .source
            .as_ref()
            .map_or(0, |source| source.instrs().count());
        self.instruction = instruction.min(count.saturating_sub(1));
        self.details_offset = 0;
        self.update_facts();
    }

    fn update_facts(&mut self) {
        let instruction = self
            .source
            .as_ref()
            .and_then(|source| source.instrs().nth(self.instruction));
        let Some(instruction) = instruction else {
            self.fact_lines.clear();
            self.source_lines.clear();
            self.facts_pc = None;
            return;
        };
        let pc = instruction.pc();
        if self.facts_pc == Some(pc) {
            return;
        }
        self.fact_lines.clear();
        self.source_lines = self.source_map.map_or_else(Vec::new, |sources| {
            sources.context_lines(
                execution_tape::trace::TraceLocation {
                    func: self.function.expect("selected function"),
                    pc,
                },
                2,
                48,
            )
        });
        self.facts_pc = Some(pc);
        let Some(function) = self.function_facts.as_mut() else {
            self.fact_lines
                .push("Verifier facts unavailable: program rejected.".into());
            return;
        };
        let Some(facts) = function.instruction(pc) else {
            return;
        };
        self.fact_lines = render::facts(
            &facts,
            instruction,
            self.program,
            self.source.as_ref().expect("selected source"),
        );
    }

    fn diagnostic(&mut self) {
        let Some(function) = self.error.as_ref().and_then(VerifyError::function) else {
            return;
        };
        if function.0 as usize >= self.program.functions.len() {
            return;
        }
        let pc = self.error.as_ref().and_then(VerifyError::pc);
        self.navigate(function, 0);
        if let Some(index) = self.source.as_ref().and_then(|source| {
            source
                .instrs()
                .position(|instruction| Some(instruction.pc()) == pc)
        }) {
            self.instruction = index;
        }
        self.update_facts();
    }

    fn remember(&mut self) {
        if let Some(source) = &self.source {
            // Bound navigation memory independently of program size.
            if self.history.len() == 256 {
                self.history.remove(0);
            }
            self.history.push((source.func(), self.instruction));
        }
    }

    fn follow(&mut self, arm: usize) {
        let Some(instruction) = self
            .source
            .as_ref()
            .and_then(|source| source.instrs().nth(self.instruction))
        else {
            return;
        };
        if let Some(edge) = instruction.edge(arm) {
            let target = self.source.as_ref().and_then(|source| {
                source
                    .instrs()
                    .position(|instruction| instruction.pc() == edge.target)
            });
            if let Some(target) = target {
                self.remember();
                self.message = CONTEXT.into();
                self.instruction = target;
                self.details_offset = 0;
            } else {
                self.message = format!(
                    "Branch target {:06} is not a decoded instruction",
                    edge.target
                );
            }
            return;
        }
        if arm != 0 {
            self.message = "Selected instruction has no false branch arm".into();
            return;
        }
        match instruction.operands() {
            Operands::Call(call) => match call.callee {
                CallTarget::Func(function)
                    if (function.0 as usize) < self.program.functions.len() =>
                {
                    self.remember();
                    self.navigate(function, 0);
                }
                CallTarget::HostSig(id, _) if (id.0 as usize) < self.program.host_sigs.len() => {
                    self.remember();
                    self.section = Section::Imports;
                    self.filter.clear();
                    self.refilter();
                    self.selected = self
                        .entries
                        .iter()
                        .position(|&index| index == id.0 as usize)
                        .unwrap_or(0);
                    self.focus = Focus::Navigator;
                    self.details_code = false;
                }
                _ => {
                    self.message =
                        "Call target is indirect or invalid; its runtime callee is unknown".into();
                }
            },
            _ => {
                self.message =
                    "Enter follows a direct call or branch; f follows a false arm".into();
            }
        }
    }

    fn back(&mut self) {
        if let Some((function, instruction)) = self.history.pop() {
            self.navigate(function, instruction);
            self.focus = Focus::Code;
            self.details_code = true;
        }
    }

    fn move_by(&mut self, amount: usize, forward: bool) {
        let (current, count) = match self.focus {
            Focus::Navigator => (self.selected, self.entries.len()),
            Focus::Code => (
                self.instruction,
                self.source
                    .as_ref()
                    .map_or(0, |source| source.instrs().count()),
            ),
            Focus::Details => {
                self.details_offset = if forward {
                    self.details_offset
                        .saturating_add(u16::try_from(amount).unwrap_or(u16::MAX))
                        .min(self.details_end)
                } else {
                    self.details_offset
                        .saturating_sub(u16::try_from(amount).unwrap_or(u16::MAX))
                };
                return;
            }
        };
        let next = if forward {
            current.saturating_add(amount)
        } else {
            current.saturating_sub(amount)
        };
        let next = next.min(count.saturating_sub(1));
        match self.focus {
            Focus::Navigator => {
                self.selected = next;
                self.select();
            }
            Focus::Code => {
                if self.instruction != next {
                    self.message = CONTEXT.into();
                }
                self.instruction = next;
                self.details_offset = 0;
            }
            Focus::Details => unreachable!(),
        }
    }
}

impl Screen for Browser<'_> {
    fn draw(&mut self, frame: &mut ratatui::Frame<'_>) {
        self.update_facts();
        render::draw(frame, self);
    }

    fn key(&mut self, key: KeyEvent) -> bool {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return false;
        }
        if self.searching {
            match key.code {
                KeyCode::Esc => {
                    self.filter.clear();
                    self.searching = false;
                    self.refilter();
                }
                KeyCode::Enter => self.searching = false,
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.refilter();
                }
                KeyCode::Char(ch) if !ch.is_control() && self.filter.len() < 128 => {
                    self.filter.push(ch);
                    self.refilter();
                }
                _ => {}
            }
            return true;
        }
        match key.code {
            KeyCode::Char('q') => return false,
            KeyCode::Char('/') => {
                self.searching = true;
                self.focus = Focus::Navigator;
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.focus = match (self.focus, key.code == KeyCode::Tab) {
                    (Focus::Navigator, true) | (Focus::Details, false) => Focus::Code,
                    (Focus::Code, true) | (Focus::Navigator, false) => Focus::Details,
                    _ => Focus::Navigator,
                };
            }
            KeyCode::Char('[') | KeyCode::Char(']') => {
                let current = Section::ALL
                    .iter()
                    .position(|&section| section == self.section)
                    .unwrap();
                self.section = Section::ALL
                    [(current + if key.code == KeyCode::Char(']') { 1 } else { 3 }) % 4];
                self.filter.clear();
                self.focus = Focus::Navigator;
                self.refilter();
            }
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1, true),
            KeyCode::Up | KeyCode::Char('k') => self.move_by(1, false),
            KeyCode::PageDown => self.move_by(self.page_rows, true),
            KeyCode::PageUp => self.move_by(self.page_rows, false),
            KeyCode::Home => self.move_by(usize::MAX, false),
            KeyCode::End => self.move_by(usize::MAX, true),
            KeyCode::Enter
                if self.focus == Focus::Navigator && self.section == Section::Functions =>
            {
                self.focus = Focus::Code;
            }
            KeyCode::Enter if self.focus == Focus::Code => self.follow(0),
            KeyCode::Char('f') if self.focus == Focus::Code => self.follow(1),
            KeyCode::Backspace => self.back(),
            KeyCode::Esc => {
                self.filter.clear();
                self.refilter();
            }
            KeyCode::Char('d') => self.diagnostic(),
            _ => {}
        }
        if self.focus != Focus::Details {
            self.details_code = self.focus == Focus::Code;
        }
        self.update_facts();
        true
    }
}
