// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Tracy profiling scopes for Execution Tape.
//!
//! This crate is `std`-only and keeps `execution_tape` itself free of profiling dependencies.
//! It listens for scope enter/exit callbacks and emits matching profiling scopes. Dropping the
//! sink closes active scopes from inner to outer, including after host panic unwinding.
//! After catching a host panic, drop and recreate the sink before another run.
//!
//! ## Backend
//! This crate currently supports the Tracy backend via `tracy-client`.
//!
//! ## Example
//! ```no_run
//! use execution_tape::{host::Host, trace::TraceSink, value::FuncId,
//!     verifier::VerifiedProgram, vm::{TrapInfo, Vm}};
//! use execution_tape_tracy::ProfilingTraceSink;
//! # fn profile<H: Host>(vm: &mut Vm<H>, program: &VerifiedProgram, entry: FuncId) -> Result<(), TrapInfo> {
//! let mut sink = ProfilingTraceSink::new();
//! let mask = sink.mask();
//! vm.run(program, entry, &[], mask, Some(&mut sink))?;
//! # Ok(()) }
//! ```
//!
//! [`ProfilingTraceSink::sources`] explicitly attaches a core source map to a program instance.
//! Mapped boundaries use the supplied document label and one-based authored line in Tracy.
//! Missing mappings and other program instances use `execution_tape.bytecode` and the source PC.
//! Snapshots are never opened as files or sent as source text. Unmapped bytecode PCs are not
//! authored line numbers. Binding validates identity once; per-event lookup runs only with a
//! running Tracy client. [`ProfilingTraceSink::source_location`] exposes the same mapping for
//! headless inspection without a live client.

mod resolver;
mod sink;

pub use resolver::{DefaultLabelResolver, LabelResolver, ProgramSymbolResolver};
pub use sink::ProfilingTraceSink;
