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
use execution_tape::trace::{
    ControlEvent, ScopeKind, TraceLocation, TraceMask, TraceOutcome, TraceSink,
};
use execution_tape::value::FuncId;
use execution_tape::verifier::VerifiedProgram;
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
static ENTER_INSTANCE: EventDesc = EventDesc::new("execution_tape.enter_instance");
static RETURN_INSTANCE: EventDesc = EventDesc::new("execution_tape.return_instance");
static TAIL_TRANSFER: EventDesc = EventDesc::new("execution_tape.tail_transfer");
static RETURN_ACROSS: EventDesc = EventDesc::new("execution_tape.return_across");
static REJECT_INSTANCE: EventDesc = EventDesc::new("execution_tape.reject_instance");

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
    runs: Vec<RunScope>,
    scopes: Vec<ScopeGuard>,
    programs: Vec<(VerifiedProgram, ContextHandle)>,
    thread: PhantomData<Rc<()>>,
}

struct RunScope {
    guard: ScopeGuard,
    scope_base: usize,
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
            runs: Vec::new(),
            scopes: Vec::new(),
            programs: Vec::new(),
            thread: PhantomData,
        }
    }

    /// Attributes one exact code definition to an application-registered source context.
    ///
    /// Use separate contexts and exported program/source pairs when independently compiled
    /// definitions execute together. Cloned verified handles share this mapping. The mapping
    /// retains code so address reuse cannot change its meaning; unmatched code uses the default
    /// context. The application keeps each context registered for the sink's lifetime.
    #[must_use]
    pub fn with_program_context(
        mut self,
        program: &VerifiedProgram,
        context: ContextHandle,
    ) -> Self {
        if let Some((_, bound)) = self
            .programs
            .iter_mut()
            .find(|(owner, _)| owner.shares_definition_with(program))
        {
            *bound = context;
        } else {
            self.programs.push((program.clone(), context));
        }
        self
    }

    fn context_for(&self, program: &Program) -> ContextHandle {
        self.programs
            .iter()
            .find(|(owner, _)| core::ptr::eq(owner.program(), program))
            .map_or(self.context, |(_, context)| *context)
    }

    fn outcome(
        &self,
        ok_desc: &'static EventDesc,
        trap_desc: &'static EventDesc,
        args: &Args,
        context: ContextHandle,
        outcome: TraceOutcome<'_>,
    ) {
        match outcome {
            TraceOutcome::Ok => footfall::message_with(ok_desc, context, "ok", args),
            TraceOutcome::Unwound { .. } => {
                footfall::message_with(ok_desc, context, "unwound", args);
            }
            TraceOutcome::TailTransferred { .. } => {
                footfall::message_with(ok_desc, context, "tail-transferred", args);
            }
            TraceOutcome::Cancelled => {
                footfall::message_with(ok_desc, context, "cancelled", args);
            }
            TraceOutcome::Trap(_) => {
                // Custom host diagnostics can be arbitrarily large. The VM result owns them;
                // profiling records the outcome/location without formatting or copying payloads.
                footfall::message_with(trap_desc, context, "trap", args);
            }
        }
    }

    fn close_run(&mut self) {
        if let Some(run) = self.runs.pop() {
            while self.scopes.len() > run.scope_base {
                self.scopes.pop();
            }
            drop(run.guard);
        }
    }

    fn close_scopes(&mut self) {
        while !self.runs.is_empty() {
            self.close_run();
        }
        while let Some(scope) = self.scopes.pop() {
            drop(scope);
        }
    }
}

impl core::fmt::Debug for FootfallSink {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FootfallSink")
            .field("context", &self.context)
            .field("run_depth", &self.runs.len())
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
            TraceMask::RUN | TraceMask::CALL | TraceMask::HOST | TraceMask::CONTROL
        } else {
            TraceMask::NONE
        }
    }

    fn control(
        &mut self,
        context: execution_tape::trace::TraceContext<'_>,
        event: ControlEvent<'_>,
    ) {
        let program = context.program();
        let (kind, source, target) = match event {
            ControlEvent::EnterInstance { parent, child, .. } => (&ENTER_INSTANCE, parent, child),
            ControlEvent::RejectInstance { parent, child, .. } => (&REJECT_INSTANCE, parent, child),
            ControlEvent::ReturnInstance { child, parent, .. } => (&RETURN_INSTANCE, child, parent),
            ControlEvent::ReturnAcross { source, target, .. } => (&RETURN_ACROSS, source, target),
            ControlEvent::TailTransfer { source, target, .. } => (&TAIL_TRANSFER, source, target),
        };
        let args = Args::none()
            .with("from_segment", source.segment_ordinal())
            .with("from_activation", source.ordinal())
            .with("to_segment", target.segment_ordinal())
            .with("to_activation", target.ordinal());
        footfall::message_with(kind, self.context_for(program), "transfer", &args);
    }

    fn run_start(
        &mut self,
        context: execution_tape::trace::TraceContext<'_>,
        entry: FuncId,
        arg_count: usize,
    ) {
        let program = context.program();
        let context = self.context_for(program);
        let args = Args::none()
            .with("entry", entry.0)
            .with("arg_count", arg_count as u64);
        self.runs.push(RunScope {
            guard: footfall::scope_with(&RUN, context, &args),
            scope_base: self.scopes.len(),
        });
    }

    fn run_end(
        &mut self,
        context: execution_tape::trace::TraceContext<'_>,
        outcome: TraceOutcome<'_>,
    ) {
        let program = context.program();
        let mut args = Args::none();
        if let Some(run) = self.runs.last()
            && run.guard.token().is_valid()
        {
            args = args.with("scope_token", run.guard.token().get());
        }
        if let TraceOutcome::Trap(error) = outcome {
            args = args.with("func", error.func.0).with("pc", error.pc);
        }
        self.outcome(
            &RUN_END,
            &RUN_TRAP,
            &args,
            self.context_for(program),
            outcome,
        );
        self.close_run();
    }

    fn instr(
        &mut self,
        context: execution_tape::trace::TraceContext<'_>,
        func: FuncId,
        pc: u32,
        _next_pc: u32,
        span_id: Option<u64>,
        opcode: u8,
    ) {
        let program = context.program();
        let mut args = Args::none()
            .with("func", func.0)
            .with("pc", pc)
            .with("opcode", opcode);
        if let Some(span) = span_id {
            args = args.with("span_id", span);
        }
        footfall::message_with(&INSTR, self.context_for(program), "attempt", &args);
    }

    fn instr_end(
        &mut self,
        context: execution_tape::trace::TraceContext<'_>,
        location: TraceLocation,
        next: Option<TraceLocation>,
        outcome: TraceOutcome<'_>,
    ) {
        let program = context.program();
        let mut args = Args::none()
            .with("func", location.func.0)
            .with("pc", location.pc);
        if let Some(next) = next {
            args = args.with("next_func", next.func.0).with("next_pc", next.pc);
        }
        self.outcome(
            &INSTR_END,
            &INSTR_TRAP,
            &args,
            self.context_for(program),
            outcome,
        );
    }

    fn scope_enter(
        &mut self,
        context: execution_tape::trace::TraceContext<'_>,
        kind: ScopeKind,
        depth: usize,
        func: FuncId,
        pc: u32,
        _span_id: Option<u64>,
    ) {
        let program = context.program();
        let args = Args::none()
            .with("func", func.0)
            .with("pc", pc)
            .with("depth", depth as u64);
        let (desc, args) = match kind {
            ScopeKind::CallFrame { .. } => (&CALL, args),
            ScopeKind::HostCall { host_sig, .. } => (&HOST, args.with("host_sig", host_sig.0)),
        };
        self.scopes
            .push(footfall::scope_with(desc, self.context_for(program), &args));
    }

    fn scope_exit(
        &mut self,
        context: execution_tape::trace::TraceContext<'_>,
        _kind: ScopeKind,
        depth: usize,
        func: FuncId,
        pc: u32,
        _span_id: Option<u64>,
        outcome: TraceOutcome<'_>,
    ) {
        let program = context.program();
        let mut args = Args::none()
            .with("func", func.0)
            .with("pc", pc)
            .with("depth", depth as u64);
        if let Some(scope) = self.scopes.last()
            && scope.token().is_valid()
        {
            args = args.with("scope_token", scope.token().get());
        }
        self.outcome(
            &SCOPE_END,
            &SCOPE_TRAP,
            &args,
            self.context_for(program),
            outcome,
        );
        self.scopes.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use execution_tape::asm::ProgramBuilder;

    #[test]
    fn a_nested_run_retains_the_waiting_parent_scope() {
        let code = ProgramBuilder::new().build();
        let mut sink = FootfallSink::new(ContextHandle::ROOT);
        sink.run_start(
            execution_tape::trace::TraceContext::unscoped(&code),
            FuncId(0),
            0,
        );
        sink.scope_enter(
            execution_tape::trace::TraceContext::unscoped(&code),
            ScopeKind::CallFrame {
                func: FuncId(0),
                caller: None,
            },
            1,
            FuncId(0),
            0,
            None,
        );
        sink.run_start(
            execution_tape::trace::TraceContext::unscoped(&code),
            FuncId(0),
            0,
        );
        assert_eq!(sink.runs.len(), 2, "child entry retains the parent run");
        assert_eq!(
            sink.scopes.len(),
            1,
            "child entry retains the parent's actual waiting frame"
        );
        sink.scope_enter(
            execution_tape::trace::TraceContext::unscoped(&code),
            ScopeKind::CallFrame {
                func: FuncId(0),
                caller: None,
            },
            1,
            FuncId(0),
            0,
            None,
        );
        sink.run_end(
            execution_tape::trace::TraceContext::unscoped(&code),
            TraceOutcome::Ok,
        );
        assert_eq!(
            sink.runs.len(),
            1,
            "child completion closes only its own run"
        );
        assert_eq!(
            sink.scopes.len(),
            1,
            "parent scope remains live until its own outcome"
        );
        sink.run_end(
            execution_tape::trace::TraceContext::unscoped(&code),
            TraceOutcome::Cancelled,
        );
        assert!(
            sink.runs.is_empty() && sink.scopes.is_empty(),
            "root completion closes the remaining scopes"
        );
    }
}
