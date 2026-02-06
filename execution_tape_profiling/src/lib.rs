// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Profiling adapters for `execution_tape` (currently Tracy).
//!
//! This crate is `std`-only and keeps `execution_tape` itself free of profiling dependencies.
//! It listens for scope enter/exit callbacks and emits matching profiling scopes.
//!
//! ## Backend
//! This crate currently supports the Tracy backend via `tracy-client`.
//!
//! ## Example
//! ```ignore
//! use execution_tape::trace::TraceSink;
//! use execution_tape_profiling::ProfilingTraceSink;
//!
//! let mut sink = ProfilingTraceSink::new();
//! let mask = sink.mask();
//! vm.run(&program, entry, &[], mask, Some(&mut sink))?;
//! # Ok::<(), execution_tape::vm::TrapInfo>(())
//! ```

mod resolver;
mod sink;

pub use resolver::{DefaultLabelResolver, LabelResolver, ProgramSymbolResolver};
pub use sink::ProfilingTraceSink;
