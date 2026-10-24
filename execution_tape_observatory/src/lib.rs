// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Browse a compiled program or explore a borrowed in-memory VM recording in a terminal.
//!
//! [`browse`] opens functions, instructions, imports, constants and aggregate types without a VM
//! or host. Verification runs once on opening; rejected programs remain navigable. [`render_program`]
//! provides the same initial view without terminal modes. [`ProgramBrowser`] attaches optional
//! frontend source snapshots through [`sources::SourceMap`]. Span IDs remain opaque; static
//! register declarations are not runtime values.
//!
//! [`explore`] connects invocation/host-call/outcome navigation, immediate aggregate inspection,
//! source instruction context, captured guest callers and checked replay. [`render_text`] renders the same initial view
//! without taking over a terminal. Incomplete captures remain visible; replay reports their fault.
//! This optional `std` adapter does not add dependencies to the VM or recording crates.
//!
//! The initial view selects the first host call, when present, with sent/returned values.
//! Tab cycles events, values and guest stack; the active pane has a double border and title.
//! Selecting a caller frame moves source to its actual suspended call instruction.
//! The timeline contains boundary observations, not per-instruction or register history. Values
//! use recording-local graph identities; object tokens do not reconstruct external objects.
//! Replay uses the recording's ordinary checked replay and invokes no live host. Opaque typed
//! error payloads are never formatted. Text/payload previews and graph expansion are bounded.

mod browser;
mod describe;
mod flow;
mod model;
mod palette;
mod render;
pub mod sources;
mod syntax;

#[cfg(test)]
mod tests;

use execution_tape::program::Program;
use execution_tape_recording::Recording;
use model::View;
use ratatui::Terminal;
use ratatui::backend::{CrosstermBackend, TestBackend};
use ratatui::crossterm::{event, execute, terminal};
use std::io;

/// Opens the recording explorer on standard output until the user quits.
///
/// Requires an interactive terminal. Owns raw mode and the alternate screen for this call;
/// restores both on return or unwind, without changing the process's panic hook. It borrows
/// the recording and never invokes the original host. Press `r` to perform checked replay
/// synchronously with the recorded guest limits. Heap reconstruction and output retention
/// add work and memory beyond guest fuel; a long replay pauses the UI.
pub fn explore(record: &Recording) -> io::Result<()> {
    interact(View::new(record))
}

/// Opens a compiled program browser without executing guest code or invoking a host.
///
/// Verifies once on opening; rejected programs remain inspectable with the first diagnostic.
/// Function names, types, instructions and span identifiers come from the program. This convenience
/// opens without source snapshots; use [`ProgramBrowser::sources`] to attach explicit frontend
/// mappings. Runtime register values are not reconstructed. Uses [`explore`] terminal lifecycle.
pub fn browse(program: &Program) -> io::Result<()> {
    ProgramBrowser::new(program).run()
}

/// Configured compiled-program browser. Source snapshots are optional, frontend-owned sidecars.
///
/// Existing [`browse`] and [`render_program`] are conveniences for a browser without sources.
/// Use this builder to attach sources once, then choose interactive or headless presentation.
pub struct ProgramBrowser<'a> {
    program: &'a Program,
    sources: Option<sources::BoundSources<'a>>,
}
impl std::fmt::Debug for ProgramBrowser<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProgramBrowser")
            .field("functions", &self.program.functions.len())
            .field("sources_attached", &self.sources.is_some())
            .finish_non_exhaustive()
    }
}
impl<'a> ProgramBrowser<'a> {
    /// Selects a compiled program. It is verified when the view is opened or rendered.
    #[must_use]
    pub fn new(program: &'a Program) -> Self {
        Self {
            program,
            sources: None,
        }
    }

    /// Attaches frontend source snapshots after comparing the full encoded program identity.
    /// A stale map is rejected before terminal modes are entered.
    pub fn sources(mut self, map: &'a sources::SourceMap) -> Result<Self, sources::SourceError> {
        self.sources = Some(map.bind(self.program)?);
        Ok(self)
    }

    /// Opens this browser using the terminal lifecycle documented by [`browse`].
    pub fn run(self) -> io::Result<()> {
        interact(browser::Browser::with_sources(self.program, self.sources))
    }

    /// Renders this browser without terminal modes. Dimensions follow [`render_program`].
    pub fn render_text(self, width: u16, height: u16) -> io::Result<String> {
        render_screen(
            || browser::Browser::with_sources(self.program, self.sources),
            width,
            height,
        )
    }
}

trait Screen {
    fn draw(&mut self, frame: &mut ratatui::Frame<'_>);
    fn key(&mut self, key: event::KeyEvent) -> bool;
}

impl Screen for View<'_> {
    fn draw(&mut self, frame: &mut ratatui::Frame<'_>) {
        render::draw(frame, self);
    }

    fn key(&mut self, key: event::KeyEvent) -> bool {
        View::key(self, key)
    }
}

fn interact(mut screen: impl Screen) -> io::Result<()> {
    terminal::enable_raw_mode()?;
    let guard = TerminalGuard;
    execute!(io::stdout(), terminal::EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    loop {
        terminal.draw(|frame| screen.draw(frame))?;
        if let event::Event::Key(key) = event::read()?
            && key.is_press()
            && !screen.key(key)
        {
            break;
        }
    }
    drop(terminal);
    drop(guard);
    Ok(())
}

struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            terminal::LeaveAlternateScreen,
            ratatui::crossterm::cursor::Show
        );
    }
}

/// Renders the explorer's initial view as plain text without terminal modes or live replay.
///
/// Useful for headless inspection and reproducible review artifacts. The replay status remains
/// "not checked". Dimensions must be within 40..=240 columns and 12..=100 rows.
pub fn render_text(record: &Recording, width: u16, height: u16) -> io::Result<String> {
    render_screen(|| View::new(record), width, height)
}

/// Renders the compiled program browser's initial view as plain text without terminal modes.
///
/// Performs the same verification as [`browse`], including navigation to a located diagnostic.
/// Dimensions have the same bounds as [`render_text`]. Does not execute guest code.
pub fn render_program(program: &Program, width: u16, height: u16) -> io::Result<String> {
    ProgramBrowser::new(program).render_text(width, height)
}

fn render_screen<S: Screen>(
    screen: impl FnOnce() -> S,
    width: u16,
    height: u16,
) -> io::Result<String> {
    if !(40..=240).contains(&width) || !(12..=100).contains(&height) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "view dimensions outside 40..=240 by 12..=100",
        ));
    }
    let mut screen = screen();
    let mut terminal =
        Terminal::new(TestBackend::new(width, height)).unwrap_or_else(|never| match never {});
    terminal
        .draw(|frame| screen.draw(frame))
        .unwrap_or_else(|never| match never {});
    let buffer = terminal.backend().buffer();
    let mut output = String::new();
    for y in 0..height {
        for x in 0..width {
            output.push_str(buffer[(x, y)].symbol());
        }
        output.push('\n');
    }
    Ok(output)
}
