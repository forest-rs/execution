// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use crate::{interact, model::View, render_screen, sources};
use execution_tape_recording::Recording;
use std::io;

/// Configured recording explorer for the same interactive and headless presentation.
///
/// Source snapshots bind to the recording's exact compiled program. Instruction history and
/// watched values must already have been captured; opening a view never reconstructs missing
/// observations or calls a live host. [`crate::explore`] and [`crate::render_text`] use defaults.
pub struct RecordingExplorer<'a> {
    record: &'a Recording,
    sources: Option<sources::BoundSources<'a>>,
    instructions: bool,
}

impl std::fmt::Debug for RecordingExplorer<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordingExplorer")
            .field("sources_attached", &self.sources.is_some())
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
            sources: None,
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
    pub fn sources(mut self, map: &'a sources::SourceMap) -> Result<Self, sources::SourceError> {
        self.sources = Some(map.bind(self.record.program().program())?);
        Ok(self)
    }

    fn view(self) -> View<'a> {
        View::with_options(self.record, self.sources, self.instructions)
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
