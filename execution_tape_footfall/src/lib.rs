// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Semantic Footfall observations for Execution Tape.
//!
//! [`FootfallSink`] translates VM tracing callbacks into run, call and host scopes, with
//! structured function/import IDs and source PCs. Pass its recommended [`TraceSink::mask`]
//! to the ordinary VM run. Add [`TraceMask::INSTR`] explicitly for instruction attempts,
//! outcomes and actual control-flow successors.
//!
//! The application installs a Footfall backend and owns the registered context, including
//! any program/revision identity needed to resolve program-local IDs. Keep that context alive
//! while using the sink. Host instrumentation can use the same context and backend.
//!
//! Authored source is resolved after capture using [`execution_tape::source::SourceMap`]. Export
//! the exact program and optional source sidecar together, keyed by the application's context.
//! Resolve recorded function/PC pairs through a map bound to that program; never interpret an
//! opaque span ID as a line or byte range. No source text is copied or looked up by this sink.
//!
//! Footfall capture is bounded: inspect its loss/window metadata before drawing conclusions.
//! These observations do not contain register values or complete host inputs and results;
//! they are not a deterministic replay record. Panic unwinding closes active profiling scopes
//! when the sink is dropped, without inventing successful VM completion events.
//! After catching a host panic, drop and recreate the sink before another run, regardless of mask.
//!
//! This adapter needs only `no_std + alloc`. Runtime capture and export dependencies belong
//! to the application; the core VM has no Footfall dependency. See the `footfall_capture`
//! binary in `execution_tape_examples` for an executable capture example.

#![no_std]

extern crate alloc;

use alloc::rc::Rc;
use alloc::vec::Vec;
use core::marker::PhantomData;
use execution_tape::program::Program;
use execution_tape::trace::{ScopeKind, TraceLocation, TraceMask, TraceOutcome, TraceSink};
use execution_tape::value::FuncId;
use footfall::{Args, ContextHandle, EventDesc, ScopeDesc, ScopeGuard};

static RUN: ScopeDesc = ScopeDesc::new("execution_tape.run");
static CALL: ScopeDesc = ScopeDesc::new("execution_tape.call");
static HOST: ScopeDesc = ScopeDesc::new("execution_tape.host");
static INSTR: EventDesc = EventDesc::new("execution_tape.instr");
static INSTR_END: EventDesc = EventDesc::new("execution_tape.instr_end");
static INSTR_TRAP: EventDesc = EventDesc::new("execution_tape.instr_trap");
static SCOPE_TRAP: EventDesc = EventDesc::new("execution_tape.scope_trap");
static RUN_TRAP: EventDesc = EventDesc::new("execution_tape.run_trap");
static SCOPE_END: EventDesc = EventDesc::new("execution_tape.scope_end");
static RUN_END: EventDesc = EventDesc::new("execution_tape.run_end");

/// Translates VM events into Footfall observations under an application-owned context.
///
/// Reuse a sink across sequential runs. Active scopes stay on the thread that starts them:
/// the sink is neither `Send` nor `Sync`. The mask passed to the VM controls delivery; the
/// recommended mask reflects the current Footfall level: it is empty when scopes are disabled,
/// and otherwise enables run, call and host scopes without per-instruction events. Recompute it
/// before each run if the level can change. Enabling capture during a run cannot enable VM
/// callbacks omitted by that run's mask; pass an explicit mask when that behavior is needed.
pub struct FootfallSink {
    context: ContextHandle,
    run: Option<ScopeGuard>,
    scopes: Vec<ScopeGuard>,
    thread: PhantomData<Rc<()>>,
}

impl FootfallSink {
    /// Creates a sink attributed to `context` on the current thread.
    ///
    /// Install the backend and register the context first. The caller keeps the context
    /// registered and gives it a program/revision identity when combining multiple programs.
    #[must_use]
    pub const fn new(context: ContextHandle) -> Self {
        Self {
            context,
            run: None,
            scopes: Vec::new(),
            thread: PhantomData,
        }
    }

    fn outcome(
        &self,
        ok_desc: &'static EventDesc,
        trap_desc: &'static EventDesc,
        args: &Args,
        outcome: TraceOutcome<'_>,
    ) {
        match outcome {
            TraceOutcome::Ok => footfall::message_with(ok_desc, self.context, "ok", args),
            TraceOutcome::Unwound { .. } => {
                footfall::message_with(ok_desc, self.context, "unwound", args);
            }
            TraceOutcome::Cancelled => {
                footfall::message_with(ok_desc, self.context, "cancelled", args);
            }
            TraceOutcome::Abandoned(_) => {
                footfall::message_with(ok_desc, self.context, "abandoned", args);
            }
            TraceOutcome::Trap(_) => {
                // Custom host diagnostics can be arbitrarily large. The VM result owns them;
                // profiling records the outcome/location without formatting or copying payloads.
                footfall::message_with(trap_desc, self.context, "trap", args);
            }
        }
    }

    fn close_scopes(&mut self) {
        while let Some(scope) = self.scopes.pop() {
            drop(scope);
        }
        self.run.take();
    }
}

impl core::fmt::Debug for FootfallSink {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FootfallSink")
            .field("context", &self.context)
            .field("run_active", &self.run.is_some())
            .field("scope_depth", &self.scopes.len())
            .finish_non_exhaustive()
    }
}

impl Drop for FootfallSink {
    fn drop(&mut self) {
        self.close_scopes();
    }
}

impl TraceSink for FootfallSink {
    fn mask(&self) -> TraceMask {
        if footfall::max_level().captures_scopes() {
            TraceMask::RUN | TraceMask::CALL | TraceMask::HOST
        } else {
            TraceMask::NONE
        }
    }

    fn run_start(&mut self, _program: &Program, entry: FuncId, arg_count: usize) {
        self.close_scopes();
        let args = Args::none()
            .with("entry", entry.0)
            .with("arg_count", arg_count as u64);
        self.run = Some(footfall::scope_with(&RUN, self.context, &args));
    }

    fn run_end(&mut self, _program: &Program, outcome: TraceOutcome<'_>) {
        let mut args = Args::none();
        if let Some(run) = &self.run
            && run.token().is_valid()
        {
            args = args.with("scope_token", run.token().get());
        }
        if let TraceOutcome::Trap(error) | TraceOutcome::Abandoned(error) = outcome {
            args = args.with("func", error.func.0).with("pc", error.pc);
        }
        self.outcome(&RUN_END, &RUN_TRAP, &args, outcome);
        self.close_scopes();
    }

    fn instr(
        &mut self,
        _program: &Program,
        func: FuncId,
        pc: u32,
        _next_pc: u32,
        span_id: Option<u64>,
        opcode: u8,
    ) {
        let mut args = Args::none()
            .with("func", func.0)
            .with("pc", pc)
            .with("opcode", opcode);
        if let Some(span) = span_id {
            args = args.with("span_id", span);
        }
        footfall::message_with(&INSTR, self.context, "attempt", &args);
    }

    fn instr_end(
        &mut self,
        _program: &Program,
        location: TraceLocation,
        next: Option<TraceLocation>,
        outcome: TraceOutcome<'_>,
    ) {
        let mut args = Args::none()
            .with("func", location.func.0)
            .with("pc", location.pc);
        if let Some(next) = next {
            args = args.with("next_func", next.func.0).with("next_pc", next.pc);
        }
        self.outcome(&INSTR_END, &INSTR_TRAP, &args, outcome);
    }

    fn scope_enter(
        &mut self,
        _program: &Program,
        kind: ScopeKind,
        depth: usize,
        func: FuncId,
        pc: u32,
        _span_id: Option<u64>,
    ) {
        let args = Args::none()
            .with("func", func.0)
            .with("pc", pc)
            .with("depth", depth as u64);
        let (desc, args) = match kind {
            ScopeKind::CallFrame { .. } => (&CALL, args),
            ScopeKind::HostCall { host_sig, .. } => (&HOST, args.with("host_sig", host_sig.0)),
        };
        self.scopes
            .push(footfall::scope_with(desc, self.context, &args));
    }

    fn scope_exit(
        &mut self,
        _program: &Program,
        _kind: ScopeKind,
        depth: usize,
        func: FuncId,
        pc: u32,
        _span_id: Option<u64>,
        outcome: TraceOutcome<'_>,
    ) {
        let mut args = Args::none()
            .with("func", func.0)
            .with("pc", pc)
            .with("depth", depth as u64);
        if let Some(scope) = self.scopes.last()
            && scope.token().is_valid()
        {
            args = args.with("scope_token", scope.token().get());
        }
        self.outcome(&SCOPE_END, &SCOPE_TRAP, &args, outcome);
        self.scopes.pop();
    }
}
