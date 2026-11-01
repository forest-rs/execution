// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use crate::{interact, model::View, render_screen};
use execution_tape::{recording::Recording, source, verifier::VerifiedProgram};
use std::io;

/// Configured recording explorer for the same interactive and headless presentation.
///
/// Source snapshots bind to the recording's exact compiled program. Instruction history and
/// watched values must already have been captured; opening a view never reconstructs missing
/// observations or calls a live host. [`crate::explore`] and [`crate::render_text`] use defaults.
pub struct RecordingExplorer<'a> {
    record: &'a Recording,
    sources: Vec<(VerifiedProgram, source::BoundSources<'a>)>,
    segment: u64,
    instructions: bool,
}

impl std::fmt::Debug for RecordingExplorer<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordingExplorer")
            .field("source_definitions", &self.sources.len())
            .field("segment", &self.segment)
            .field("instructions_selected", &self.instructions)
            .finish_non_exhaustive()
    }
}

impl<'a> RecordingExplorer<'a> {
    /// Selects an immutable recording. The default view opens its boundary timeline.
    #[must_use]
    pub fn new(record: &'a Recording) -> Self {
        Self {
            record,
            sources: Vec::new(),
            segment: 0,
            instructions: false,
        }
    }

    /// Opens on retained instruction history when present. Otherwise the boundary timeline stays
    /// selected and the header explicitly reports that instruction history was not captured.
    #[must_use]
    pub fn instructions(mut self) -> Self {
        self.instructions = true;
        self
    }

    /// Attaches explicit frontend source snapshots after exact compiled-program identity checks.
    /// A stale map is rejected before entering terminal modes.
    pub fn sources(mut self, map: &'a source::SourceMap) -> Result<Self, source::SourceError> {
        self.sources.push((
            self.record.program().clone(),
            map.bind(self.record.program().program())?,
        ));
        Ok(self)
    }

    /// Attaches authored source to one exact retained definition. Equal local function IDs
    /// in other definitions never select this map. Old revisions remain independently visible.
    pub fn sources_for(
        mut self,
        program: &VerifiedProgram,
        map: &'a source::SourceMap,
    ) -> Result<Self, source::SourceError> {
        self.sources
            .push((program.clone(), map.bind(program.program())?));
        Ok(self)
    }

    /// Opens a segment from the owning execution. `[` and `]` navigate observed segments;
    /// checked replay always runs the complete owning execution. A request denied before
    /// entry has no segment timeline and returns `InvalidInput` here.
    pub fn segment(mut self, id: u64) -> io::Result<Self> {
        if !self
            .record
            .segments()
            .any(|segment| segment.id == id && segment.recording.is_some())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "segment has no observed execution evidence",
            ));
        }
        self.segment = id;
        Ok(self)
    }

    fn view(self) -> View<'a> {
        View::in_execution(self.record, self.segment, self.sources, self.instructions)
            .expect("selected observed segment")
    }

    /// Opens the configured explorer using [`crate::explore`]'s terminal lifecycle.
    pub fn run(self) -> io::Result<()> {
        interact(self.view())
    }

    /// Renders without terminal modes. Dimension bounds follow [`crate::render_text`].
    pub fn render_text(self, width: u16, height: u16) -> io::Result<String> {
        render_screen(|| self.view(), width, height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use execution_tape::{
        asm::{FunctionSig, ProgramBuilder},
        host::{Host, HostContext, HostError, HostSig, SigHash, ValueRef},
        program::ValueType,
        recording::{InstructionCapture, RecordLimits, Recorder, ReplayError},
        source::{SourceDocument, SourceId, SourceLimits, SourceMap, SourceMapping},
        trace::{TraceLocation, TraceSink},
        value::Value,
        vm::{InvocationOptions, Limits, Vm},
    };
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::sync::Arc;

    struct NoHost;
    impl Host for NoHost {
        fn call(
            &mut self,
            _: &str,
            _: SigHash,
            _: &[ValueRef<'_>],
            _: &mut [Value],
            _: HostContext<'_, '_>,
        ) -> Result<u64, HostError> {
            panic!("recorded control never dispatches a native host");
        }
    }

    fn fixture() -> (Recording, SourceMap, SourceMap, VerifiedProgram) {
        let build = |child: bool| {
            let mut builder = ProgramBuilder::new();
            builder.set_program_name(if child {
                "retained child revision"
            } else {
                "root caller"
            });
            let pause = builder.host_sig_for(
                "choose_method",
                HostSig {
                    args: vec![],
                    rets: vec![ValueType::I64],
                },
            );
            let function = builder
                .declare_function(FunctionSig {
                    arg_types: vec![],
                    ret_types: vec![ValueType::I64],
                })
                .unwrap();
            let mut body = builder.function_asm(function).unwrap();
            let answer = if child {
                let answer = body.alloc_register(ValueType::I64);
                body.const_i64(answer, 99);
                answer
            } else {
                let [answer] = builder.emit_call(&mut body, pause, &[]).unwrap();
                answer
            };
            body.ret(0, &[answer]);
            builder.define_function(function, body).unwrap();
            let program = builder.build_verified().unwrap();
            let text = if child {
                "child returns ninety nine"
            } else {
                "root sends to child"
            };
            let disasm =
                execution_tape::disasm::disassemble_function(program.program(), function).unwrap();
            let map = SourceMap::new(
                program.program(),
                vec![SourceDocument {
                    name: if child { "child.lang" } else { "root.lang" }.into(),
                    text: text.into(),
                }],
                disasm
                    .instrs()
                    .map(|instruction| SourceMapping {
                        location: TraceLocation {
                            func: function,
                            pc: instruction.pc(),
                        },
                        document: SourceId(0),
                        range: 0..text.len(),
                    })
                    .collect(),
                SourceLimits::default(),
            )
            .unwrap();
            (program, map, function)
        };
        let (parent, parent_map, entry) = build(false);
        let (child, child_map, method) = build(true);
        let parent = Arc::new(parent);
        let mut recorder = Recorder::new(parent.clone(), RecordLimits::default())
            .with_instructions(InstructionCapture::default());
        let mut vm = Vm::new(NoHost, Limits::default());
        let instance = vm.instantiate(&child).unwrap();
        {
            let mask = recorder.mask();
            let mut run = vm
                .start(
                    &parent,
                    entry,
                    &[],
                    InvocationOptions::default()
                        .yield_host_calls()
                        .trace(mask, &mut recorder),
                )
                .unwrap();
            run.resume(100).unwrap();
            run.pending_host_call()
                .unwrap()
                .call_instance(&instance, method, vec![], InvocationOptions::default())
                .unwrap();
            run.resume(100).unwrap();
            let mut request = run.pending_host_call().unwrap();
            let mut results = request.take_callback_results().unwrap();
            request.complete(&mut results, Ok(0)).unwrap();
            run.resume(100).unwrap();
        }
        (recorder.finish(), parent_map, child_map, child)
    }

    #[test]
    fn recorded_segments_select_exact_sources_and_replay_the_owning_execution() {
        let (record, root_map, child_map, child) = fixture();
        assert!(matches!(
            record
                .segments()
                .nth(1)
                .unwrap()
                .recording
                .unwrap()
                .replay(),
            Err(ReplayError::ExecutionContextRequired)
        ));
        let configured = || {
            RecordingExplorer::new(&record)
                .sources(&root_map)
                .unwrap()
                .sources_for(&child, &child_map)
                .unwrap()
                .segment(1)
                .unwrap()
                .instructions()
        };
        let text = configured().render_text(140, 40).unwrap();
        assert!(
            text.contains("#1 retained child revision ← #0 root caller"),
            "{text}"
        );
        assert!(
            text.contains("child.lang") && text.contains("child returns ninety nine"),
            "{text}"
        );
        let mut view = configured().view();
        view.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        assert!(view.replay.contains("MATCHED"), "{}", view.replay);
        view.key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE));
        assert_eq!(view.segment, 0);
        assert!(view.source_lines.join(" ").contains("root.lang"));
        view.instruction = None;
        let returned = view.boundaries.iter().position(|boundary| matches!(boundary, crate::model::Boundary::Transfer(i) if matches!(view.transfers[*i].action, execution_tape::recording::TransferAction::Returned { .. }))).unwrap();
        view.select(returned + 1);
        assert!(
            view.source_program().shares_definition_with(&child),
            "returned source resolves against the returning child's code"
        );
        assert!(view.source_lines.join(" ").contains("child.lang"));
        assert!(matches!(
            view.transfer_values(1),
            [execution_tape::recording::RecordedValue::I64(99)]
        ));
    }
}
