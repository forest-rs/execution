// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Explore a borrowed in-memory VM recording in a terminal.
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

mod describe;
mod flow;
mod model;
mod palette;
mod render;
mod syntax;

#[cfg(test)]
mod tests;

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
    render_screen(View::new(record), width, height)
}

fn render_screen(mut screen: impl Screen, width: u16, height: u16) -> io::Result<String> {
    if !(40..=240).contains(&width) || !(12..=100).contains(&height) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "view dimensions outside 40..=240 by 12..=100",
        ));
    }
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
