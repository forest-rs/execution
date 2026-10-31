// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Read-only composition of independently owned invocations and embedding decisions.

use crate::{Screen, describe, palette::*, render, source_context};
use execution_tape::{
    source::{BoundSources, SourceError, SourceMap},
    trace::TraceLocation,
    verifier::VerifiedProgram,
    vm::{ActivationId, ExecutionWork, InvocationInspection, PausedFrame},
};
use ratatui::{
    Frame,
    crossterm::event::{KeyCode, KeyEvent},
    layout::{Constraint, Layout},
    style::{Modifier, Style},
    text::Line,
    widgets::{Block, List, ListItem, ListState, Paragraph, Wrap},
};
use std::{fmt, io};

/// One exact prepared code domain with optional source snapshots, bound once and reused.
#[derive(Clone, Copy)]
pub struct TaskCode<'a> {
    program: &'a VerifiedProgram,
    sources: Option<BoundSources<'a>>,
}
impl fmt::Debug for TaskCode<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskCode")
            .field("program", &self.program.program().name())
            .field("sources", &self.sources.is_some())
            .finish()
    }
}
impl<'a> TaskCode<'a> {
    /// Selects the actual prepared code owner. Equal labels or IDs do not establish ownership.
    #[must_use]
    pub fn new(program: &'a VerifiedProgram) -> Self {
        Self {
            program,
            sources: None,
        }
    }
    /// Binds source snapshots against the complete encoded program identity.
    pub fn sources(mut self, sources: &'a SourceMap) -> Result<Self, SourceError> {
        self.sources = Some(sources.bind(self.program.program())?);
        Ok(self)
    }
    /// Prepared owner interpreting locations and nominal identities.
    #[must_use]
    pub fn program(self) -> &'a VerifiedProgram {
        self.program
    }
}

/// Invalid task inspection input, rejected before terminal modes are entered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskViewError {
    /// A supplied code owner differs from the invocation being inspected.
    ProgramMismatch,
    /// A task-frame input has no live VM activations.
    NoActiveFrames,
    /// Two inputs borrow the same invocation and would double-count its frames and work.
    DuplicateInvocation,
    /// A decision location is not an executable instruction in its supplied code domain.
    InvalidLocation(TraceLocation),
    /// More than 64 invocation inputs or 4,096 decision inputs were supplied.
    InputLimit,
}
impl fmt::Display for TaskViewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid task inspection: {self:?}")
    }
}
impl std::error::Error for TaskViewError {}

/// A real paused invocation, independent of its host type.
///
/// The inspection borrow prevents resumption through the borrowed invocation. External host state
/// retains its own synchronization and lifetime rules. At most
/// 64 innermost guest frames per invocation are presented; omitted outer frames are reported.
pub struct TaskFrame<'a> {
    label: String,
    code: TaskCode<'a>,
    inspection: InvocationInspection<'a>,
    root: ActivationId,
    frames: Vec<PausedFrame>,
    omitted: usize,
}
impl fmt::Debug for TaskFrame<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskFrame")
            .field("label", &self.label)
            .field("code", &self.code)
            .field("frames", &self.frames.len())
            .field("omitted", &self.omitted)
            .finish()
    }
}
impl<'a> TaskFrame<'a> {
    /// Borrows a paused invocation and checks its exact prepared owner before using source.
    pub fn new(
        label: &str,
        inspection: InvocationInspection<'a>,
        code: TaskCode<'a>,
    ) -> Result<Self, TaskViewError> {
        if !std::ptr::eq(inspection.program(), code.program) {
            return Err(TaskViewError::ProgramMismatch);
        }
        let root = inspection
            .frames()
            .next_back()
            .ok_or(TaskViewError::NoActiveFrames)?
            .activation;
        let omitted = inspection.frames().len().saturating_sub(64);
        let mut frames: Vec<_> = inspection.frames().take(64).collect();
        frames.reverse();
        Ok(Self {
            label: source_context::label_preview(label, 120),
            code,
            inspection,
            root,
            frames,
            omitted,
        })
    }
}

/// An embedding-supplied language decision with an actual instruction and exact code owner.
///
/// Its text describes native policy; the VM does not certify that policy. Source may refer to
/// code whose invocation has already returned, independently of the currently paused stack.
#[derive(Clone, Debug)]
pub struct TaskDecision<'a> {
    text: String,
    code: TaskCode<'a>,
    location: TraceLocation,
}
impl<'a> TaskDecision<'a> {
    /// Validates an executable location and bounds/escapes the supplied text preview.
    pub fn new(
        text: &str,
        code: TaskCode<'a>,
        location: TraceLocation,
    ) -> Result<Self, TaskViewError> {
        if !code
            .program
            .function(location.func)
            .is_some_and(|function| {
                function
                    .instructions()
                    .any(|instruction| instruction.pc == location.pc)
            })
        {
            return Err(TaskViewError::InvalidLocation(location));
        }
        Ok(Self {
            text: source_context::label_preview(text, 240),
            code,
            location,
        })
    }
    /// Bounded native-policy text, distinct from a VM fact or checked replay conclusion.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }
    /// Exact code interpreting this decision's source location.
    #[must_use]
    pub fn code(&self) -> TaskCode<'a> {
        self.code
    }
    /// Actual executable instruction associated with the embedding decision.
    #[must_use]
    pub fn location(&self) -> TraceLocation {
        self.location
    }
}

/// One guest frame in the same bounded inventory shown by the task explorer.
#[derive(Debug)]
pub struct TaskFrameInfo<'a> {
    /// Index of the embedding-supplied invocation, in its supplied causal order.
    pub invocation: usize,
    /// Bounded invocation label, separate from the guest function name.
    pub label: &'a str,
    /// Exact prepared code owner.
    pub code: TaskCode<'a>,
    /// Actual activation, with its original invocation identity domain.
    pub frame: &'a PausedFrame,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Stack,
    Decisions,
    Source,
    Values,
}

/// Read-only source-linked inspection of a paused language task.
///
/// Invocation inputs are in embedding-supplied causal order, outermost first. Their internal
/// guest frames, initialized values and code ownership come from real VM inspection. The view
/// does not certify cross-invocation relationships or run guest/native code. `q` returns to the
/// driver, which remains responsible for scheduling, recovery, cleanup and resource policy.
///
/// ```no_run
/// use execution_tape::{host::Host, source::SourceMap, vm::Invocation};
/// use execution_tape_observatory::{TaskCode, TaskExplorer, TaskFrame};
/// fn inspect<H: Host>(call: &Invocation<'_, H>, sources: &SourceMap)
///     -> Result<(), Box<dyn std::error::Error>>
/// {
///     let code = TaskCode::new(call.program()).sources(sources)?;
///     let frame = TaskFrame::new("waiting method", call.inspect(), code)?;
///     let mut view = TaskExplorer::new(vec![frame], vec![])?;
///     println!("{}", view.render_text(120, 40)?);
///     view.run()?;
///     Ok(())
/// }
/// ```
pub struct TaskExplorer<'a> {
    invocations: Vec<TaskFrame<'a>>,
    decisions: Vec<TaskDecision<'a>>,
    rows: Vec<(usize, usize)>,
    selected_frame: usize,
    selected_decision: usize,
    decision_source: bool,
    focus: Focus,
    source_offset: usize,
    values_offset: usize,
}
impl fmt::Debug for TaskExplorer<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskExplorer")
            .field("invocations", &self.invocations.len())
            .field("guest_frames", &self.rows.len())
            .field("decisions", &self.decisions.len())
            .finish()
    }
}
impl<'a> TaskExplorer<'a> {
    /// Checks bounded inputs and rejects duplicate invocation identities before presentation.
    pub fn new(
        invocations: Vec<TaskFrame<'a>>,
        decisions: Vec<TaskDecision<'a>>,
    ) -> Result<Self, TaskViewError> {
        if invocations.len() > 64 || decisions.len() > 4_096 {
            return Err(TaskViewError::InputLimit);
        }
        for (index, invocation) in invocations.iter().enumerate() {
            if invocations[..index]
                .iter()
                .any(|earlier| earlier.root == invocation.root)
            {
                return Err(TaskViewError::DuplicateInvocation);
            }
        }
        let rows: Vec<_> = invocations
            .iter()
            .enumerate()
            .flat_map(|(i, invocation)| (0..invocation.frames.len()).map(move |j| (i, j)))
            .collect();
        let selected_frame = rows.len().saturating_sub(1);
        let selected_decision = decisions.len().saturating_sub(1);
        let decision_source = rows.is_empty() && !decisions.is_empty();
        Ok(Self {
            invocations,
            decisions,
            rows,
            selected_frame,
            selected_decision,
            decision_source,
            focus: if decision_source {
                Focus::Decisions
            } else {
                Focus::Stack
            },
            source_offset: 0,
            values_offset: 0,
        })
    }
    /// Enumerates the exact bounded frame inventory shown by the UI, outermost first.
    pub fn frames(&self) -> impl ExactSizeIterator<Item = TaskFrameInfo<'_>> {
        self.rows.iter().map(|&(invocation, frame)| {
            let input = &self.invocations[invocation];
            TaskFrameInfo {
                invocation,
                label: &input.label,
                code: input.code,
                frame: &input.frames[frame],
            }
        })
    }
    /// Borrows the native decision inputs shown in their supplied order.
    pub fn decisions(&self) -> &[TaskDecision<'a>] {
        &self.decisions
    }
    /// Sums the live invocation work once. `None` means a numeric counter overflowed.
    #[must_use]
    pub fn work(&self) -> Option<ExecutionWork> {
        self.invocations
            .iter()
            .try_fold(ExecutionWork::default(), |sum, frame| {
                sum.checked_add(frame.inspection.work())
            })
    }
    /// Selects a guest frame and its exact source and initialized values. Invalid indices do nothing.
    pub fn select_frame(&mut self, index: usize) -> bool {
        if index >= self.rows.len() {
            return false;
        }
        self.selected_frame = index;
        self.decision_source = false;
        self.source_offset = 0;
        self.values_offset = 0;
        true
    }
    /// Selects native policy source; this does not select or mutate a live guest activation.
    pub fn select_decision(&mut self, index: usize) -> bool {
        if index >= self.decisions.len() {
            return false;
        }
        self.selected_decision = index;
        self.decision_source = true;
        self.source_offset = 0;
        self.values_offset = 0;
        true
    }
    /// Renders the current selection without terminal modes or guest execution.
    pub fn render_text(&mut self, width: u16, height: u16) -> io::Result<String> {
        crate::render_current(self, width, height)
    }
    /// Opens the read-only task view; leaving it returns control to the language driver.
    pub fn run(&mut self) -> io::Result<()> {
        crate::interact(self)
    }

    fn source_lines(&self) -> Vec<String> {
        let selection = if self.decision_source {
            self.decisions
                .get(self.selected_decision)
                .map(|decision| (decision.code, decision.location))
        } else {
            self.rows.get(self.selected_frame).map(|&(i, j)| {
                (
                    self.invocations[i].code,
                    self.invocations[i].frames[j].location,
                )
            })
        };
        let Some((code, location)) = selection else {
            return vec!["Select a frame or language decision.".into()];
        };
        let mut lines = vec![format!(
            "{} · {} · f{}:{:06}",
            if self.decision_source {
                "EMBEDDING DECISION"
            } else {
                "ACTUAL VM FRAME"
            },
            describe::function(code.program.program(), location.func),
            location.func.0,
            location.pc
        )];
        lines.push(format!(
            "Program: {}",
            source_context::label_preview(code.program.program().name().unwrap_or("unnamed"), 100)
        ));
        if self.decision_source {
            lines.push(format!(
                "Decision: {}",
                self.decisions[self.selected_decision].text
            ));
        }
        if let Some(instruction) = code.program.function(location.func).and_then(|function| {
            function
                .instructions()
                .find(|instruction| instruction.pc == location.pc)
        }) {
            lines.push(format!("Instruction: {:?}", instruction.opcode));
        }
        match code.sources {
            Some(sources) => lines.extend(source_context::context_lines(sources, location, 8, 100)),
            None => lines.push("No frontend source attached to this code owner.".into()),
        }
        lines
    }
    fn values(&mut self) -> Vec<String> {
        if self.decision_source {
            return vec![
                "Native policy input, not a VM register snapshot.".into(),
                "Select a task frame for initialized guest values.".into(),
            ];
        }
        let Some(&(i, j)) = self.rows.get(self.selected_frame) else {
            return vec!["No live guest frame selected.".into()];
        };
        let input = &mut self.invocations[i];
        let frame = &input.frames[j];
        let mut lines = vec![
            format!(
                "{} · activation #{}",
                input.label,
                frame.activation.ordinal()
            ),
            if frame.waiting_for_return {
                "Waiting: reply destinations are unavailable.".into()
            } else {
                "Paused before the selected instruction.".into()
            },
        ];
        if let Some(registers) = input.inspection.registers(&frame.activation) {
            for register in registers.take(128) {
                let value = register
                    .value
                    .map_or_else(|| "unavailable".into(), crate::live::value_preview);
                lines.push(format!(
                    "r{} · {} = {value}",
                    register.register,
                    describe::type_name(register.storage_kind)
                ));
            }
        }
        if frame.register_count > 128 {
            lines.push("… further registers omitted".into());
        }
        lines
    }
}

impl Screen for TaskExplorer<'_> {
    fn draw(&mut self, frame: &mut Frame<'_>) {
        let area = frame.area();
        frame.render_widget(Block::default().style(Style::default().bg(BG).fg(FG)), area);
        if render::too_small(frame) {
            return;
        }
        let layout = Layout::vertical([
            Constraint::Length(2),
            Constraint::Percentage(35),
            Constraint::Min(8),
            Constraint::Length(2),
        ])
        .split(area);
        let work = self.work().map_or_else(
            || "live work counters overflowed".into(),
            |work| {
                format!(
                    "Live VM work: {} fuel · {} host · {} contract · {} aggregate · {} payload",
                    work.fuel,
                    work.host_calls,
                    work.contract,
                    work.data.aggregate,
                    work.data.payload
                )
            },
        );
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(
                    "PAUSED LANGUAGE TASK · read-only · embedding owns scheduling",
                    Style::default().fg(GREEN).add_modifier(Modifier::BOLD),
                ),
                Line::styled(work, Style::default().fg(CYAN)),
            ]),
            layout[0],
        );
        let top = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(layout[1]);
        let bottom = Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(layout[2]);
        let items: Vec<_> = self
            .frames()
            .map(|row| {
                ListItem::new(vec![
                    Line::styled(
                        format!("{} · {}", row.invocation, row.label),
                        Style::default().fg(GOLD),
                    ),
                    Line::styled(
                        format!(
                            "  {} · f{}:{:06}",
                            describe::function(row.code.program.program(), row.frame.location.func),
                            row.frame.location.func.0,
                            row.frame.location.pc
                        ),
                        Style::default().fg(CYAN),
                    ),
                ])
            })
            .collect();
        let omitted: usize = self.invocations.iter().map(|input| input.omitted).sum();
        frame.render_stateful_widget(
            List::new(items)
                .block(
                    render::panel(" TASK STACK ", self.focus == Focus::Stack).title_bottom(
                        format!("supplied causal order · {omitted} outer guest frames omitted"),
                    ),
                )
                .highlight_symbol("▸ ")
                .highlight_style(Style::default().bg(ratatui::style::Color::Indexed(23))),
            top[0],
            &mut ListState::default()
                .with_selected((!self.rows.is_empty()).then_some(self.selected_frame)),
        );
        let items: Vec<_> = self
            .decisions
            .iter()
            .map(|decision| {
                ListItem::new(Line::styled(
                    decision.text.as_str(),
                    Style::default().fg(PURPLE),
                ))
            })
            .collect();
        frame.render_stateful_widget(
            List::new(items)
                .block(
                    render::panel(" LANGUAGE DECISIONS ", self.focus == Focus::Decisions)
                        .title_bottom("embedding supplied · separate from checked VM replay"),
                )
                .highlight_symbol("▸ ")
                .highlight_style(Style::default().bg(ratatui::style::Color::Indexed(23))),
            top[1],
            &mut ListState::default()
                .with_selected((!self.decisions.is_empty()).then_some(self.selected_decision)),
        );
        let source: Vec<_> = self
            .source_lines()
            .into_iter()
            .skip(self.source_offset)
            .map(|line| Line::styled(line, Style::default().fg(FG)))
            .collect();
        frame.render_widget(
            Paragraph::new(source)
                .block(render::panel(
                    " EXACT SOURCE & INSTRUCTION ",
                    self.focus == Focus::Source,
                ))
                .wrap(Wrap { trim: false }),
            bottom[0],
        );
        let values: Vec<_> = self
            .values()
            .into_iter()
            .skip(self.values_offset)
            .map(|line| Line::styled(line, Style::default().fg(CYAN)))
            .collect();
        frame.render_widget(
            Paragraph::new(values)
                .block(render::panel(
                    " INITIALIZED VALUES ",
                    self.focus == Focus::Values,
                ))
                .wrap(Wrap { trim: false }),
            bottom[1],
        );
        frame.render_widget(Paragraph::new("Tab active panel · ↑/↓ select or scroll · q return to driver\nFrame selection shows that invocation's source/values; decisions show native policy source.").style(Style::default().fg(MUTED)), layout[3]);
    }
    fn key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return false,
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Stack => Focus::Decisions,
                    Focus::Decisions => Focus::Source,
                    Focus::Source => Focus::Values,
                    Focus::Values => Focus::Stack,
                }
            }
            KeyCode::Up | KeyCode::Down => {
                let down = key.code == KeyCode::Down;
                let move_by = |value: usize, count: usize| {
                    if down {
                        value.saturating_add(1).min(count.saturating_sub(1))
                    } else {
                        value.saturating_sub(1)
                    }
                };
                match self.focus {
                    Focus::Stack => {
                        self.select_frame(move_by(self.selected_frame, self.rows.len()));
                    }
                    Focus::Decisions => {
                        self.select_decision(move_by(self.selected_decision, self.decisions.len()));
                    }
                    Focus::Source => {
                        self.source_offset = move_by(self.source_offset, self.source_lines().len());
                    }
                    Focus::Values => {
                        self.values_offset = move_by(self.values_offset, self.values().len());
                    }
                }
            }
            _ => {}
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use execution_tape::{
        asm::{FunctionSig, ProgramBuilder},
        host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef},
        program::ValueType,
        source::{SourceDocument, SourceId, SourceLimits},
        value::{FuncId, Value},
        vm::{InvocationOptions, Limits, RunStatus, Vm},
    };
    use ratatui::{Terminal, backend::TestBackend, crossterm::event::KeyModifiers};

    struct Never;
    impl Host for Never {
        fn call(
            &mut self,
            _: &str,
            _: SigHash,
            _: &[ValueRef<'_>],
            _: &mut [Value],
            _: HostContext<'_, '_>,
        ) -> Result<u64, HostError> {
            panic!("read-only inspection must never dispatch a host")
        }
    }
    fn fixture(number: i64, name: &str) -> (VerifiedProgram, SourceMap) {
        let mut builder = ProgramBuilder::new();
        builder.set_program_name(name);
        let entry = builder
            .declare_function(FunctionSig {
                arg_types: vec![],
                ret_types: vec![ValueType::I64],
            })
            .unwrap();
        let host = builder.host_sig_for(
            "wait",
            HostSig {
                args: vec![ValueType::I64],
                rets: vec![ValueType::I64],
            },
        );
        let mut body = builder.function_asm(entry).unwrap();
        let input = body.alloc_register(ValueType::I64);
        body.const_i64(input, number);
        body.source(SourceId(0), 0..name.len());
        let [answer] = builder.emit_call(&mut body, host, &[input]).unwrap();
        body.ret(0, &[answer]);
        let mappings = body.source_mappings(entry).collect();
        builder.define_function(entry, body).unwrap();
        let program = builder.build_verified().unwrap();
        let sources = SourceMap::new(
            program.program(),
            vec![SourceDocument {
                name: format!("{name}.expr"),
                text: name.into(),
            }],
            mappings,
            SourceLimits::default(),
        )
        .unwrap();
        (program, sources)
    }

    #[test]
    fn identity_and_location_errors_precede_presentation() {
        let (alpha, alpha_sources) = fixture(11, "alpha");
        let (beta, beta_sources) = fixture(99, "beta");
        assert!(TaskCode::new(&alpha).sources(&beta_sources).is_err());
        let code = TaskCode::new(&alpha).sources(&alpha_sources).unwrap();
        let mut vm = Vm::new(Never, Limits::default());
        let mut run = vm
            .start(
                &alpha,
                FuncId(0),
                &[],
                InvocationOptions::default().yield_host_calls(),
            )
            .unwrap();
        assert_eq!(run.resume(64).unwrap(), RunStatus::AwaitingHost);
        assert!(matches!(
            TaskFrame::new("wrong", run.inspect(), TaskCode::new(&beta)),
            Err(TaskViewError::ProgramMismatch)
        ));
        let first = TaskFrame::new("first", run.inspect(), code).unwrap();
        let duplicate = TaskFrame::new("duplicate", run.inspect(), code).unwrap();
        assert!(matches!(
            TaskExplorer::new(vec![first, duplicate], vec![]),
            Err(TaskViewError::DuplicateInvocation)
        ));
        assert!(matches!(
            TaskDecision::new(
                "bad instruction",
                code,
                TraceLocation {
                    func: FuncId(0),
                    pc: 1
                }
            ),
            Err(TaskViewError::InvalidLocation(_))
        ));
        let location = run.inspect().frames().next().unwrap().location;
        let decision = TaskDecision::new("late policy source", code, location).unwrap();
        assert!(matches!(
            TaskExplorer::new(vec![], vec![decision.clone(); 4_097]),
            Err(TaskViewError::InputLimit)
        ));
        run.pending_host_call().unwrap().abandon();
        assert!(matches!(
            TaskFrame::new("finished", run.inspect(), code),
            Err(TaskViewError::NoActiveFrames)
        ));
        let mut native = TaskExplorer::new(vec![], vec![decision]).unwrap();
        assert!(
            native
                .render_text(120, 40)
                .unwrap()
                .contains("EMBEDDING DECISION")
        );
    }

    #[test]
    fn task_selection_focus_and_small_screen_are_read_only_and_source_bound() {
        let (alpha, alpha_sources) = fixture(11, "alpha");
        let (beta, beta_sources) = fixture(99, "beta");
        let mut first_vm = Vm::new(Never, Limits::default());
        let mut second_vm = Vm::new(Never, Limits::default());
        let mut first = first_vm
            .start(
                &alpha,
                FuncId(0),
                &[],
                InvocationOptions::default().yield_host_calls(),
            )
            .unwrap();
        let mut second = second_vm
            .start(
                &beta,
                FuncId(0),
                &[],
                InvocationOptions::default().yield_host_calls(),
            )
            .unwrap();
        first.resume(64).unwrap();
        second.resume(64).unwrap();
        let work = first.work().checked_add(second.work()).unwrap();
        let alpha_code = TaskCode::new(&alpha).sources(&alpha_sources).unwrap();
        let beta_code = TaskCode::new(&beta).sources(&beta_sources).unwrap();
        let location = first.inspect().frames().next().unwrap().location;
        let decision =
            TaskDecision::new("driver's recovery\u{1b}[31m", alpha_code, location).unwrap();
        assert!(decision.text().contains("driver's recovery"));
        assert!(!decision.text().contains('\u{1b}'));
        let mut view = TaskExplorer::new(
            vec![
                TaskFrame::new("sender", first.inspect(), alpha_code).unwrap(),
                TaskFrame::new("callee", second.inspect(), beta_code).unwrap(),
            ],
            vec![decision],
        )
        .unwrap();
        assert_eq!(view.work(), Some(work));
        assert_eq!(view.frames().len(), 2);
        view.select_frame(0);
        let text = view.render_text(120, 40).unwrap();
        assert!(text.contains("alpha.expr"));
        assert!(text.contains("i64 = 11"));
        view.select_frame(1);
        let text = view.render_text(120, 40).unwrap();
        assert!(text.contains("beta.expr"));
        assert!(text.contains("i64 = 99"));
        assert!(view.select_decision(0));
        assert!(
            view.render_text(120, 40)
                .unwrap()
                .contains("EMBEDDING DECISION")
        );
        let text = view.render_text(79, 19).unwrap();
        assert!(text.contains("current size: 79 × 19"));
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| view.draw(frame)).unwrap();
        assert_eq!(terminal.backend().buffer()[(0, 2)].symbol(), "╔");
        assert_eq!(terminal.backend().buffer()[(0, 2)].fg, GREEN);
        assert!(view.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
        terminal.draw(|frame| view.draw(frame)).unwrap();
        assert_eq!(terminal.backend().buffer()[(60, 2)].symbol(), "╔");
        assert_eq!(terminal.backend().buffer()[(0, 2)].symbol(), "┌");
        assert!(view.key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE)));
        assert!(!view.key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)));
        drop(view);
        assert_eq!(first.work().checked_add(second.work()), Some(work));
    }
}
