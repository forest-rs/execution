// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use super::{StrictDepsTrace, TapeError};
use crate::NodeId;
use alloc::sync::Arc;
use core::fmt;
use execution_tape::program::Program;
use execution_tape::trace::{
    RegisterEvent, RegisterPhase, ScopeKind, TraceLocation, TraceMask, TraceOutcome, TraceSink,
    ValueEvent,
};
use execution_tape::value::FuncId;
use execution_tape::verifier::VerifiedProgram;

/// Correlates a tape invocation with the graph attempt that requested it.
///
/// Node identities and attempt numbers belong to the graph. A custom executor may invoke more
/// than one tape body in an attempt; the observer can assign additional capture IDs if needed.
#[non_exhaustive]
#[derive(Clone, Copy)]
pub struct TapeInvocation<'a> {
    /// Executing graph node.
    pub node: NodeId,
    /// One-based execution attempt of that node, including earlier failed or pending attempts.
    pub attempt: u64,
    /// Exact prepared program; clone its Arc to retain program identity with a recording.
    pub program: &'a Arc<VerifiedProgram>,
    /// Entry function executed by this invocation.
    pub entry: FuncId,
}

impl fmt::Debug for TapeInvocation<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TapeInvocation")
            .field("node", &self.node)
            .field("attempt", &self.attempt)
            .field("entry", &self.entry)
            .field("program", &self.program.program().name())
            .finish()
    }
}

/// Selects and owns tracing or recording of individual tape invocations inside a graph.
///
/// Return `None` to leave that invocation unobserved, or an owned sink whose `mask()` selects
/// events. `finish` receives the sink after adapter completion so it can retain a recording.
/// This is invocation evidence, not graph publication: the graph can still reject the custom
/// executor's output arity or dependencies. Nodes skipped by cache/cutoff create no invocation;
/// an executed node returning an unchanged result is still observed. Ownership admission/import
/// failures occur before `begin`; host/sink panic unwinding is outside
/// the completion contract. Strict dependency checking composes with the selected sink.
pub trait TapeObserver {
    /// Owned sink for one selected invocation, for example a recorder or profiling adapter.
    type Sink: TraceSink;
    /// Selects an invocation after its inputs are admitted and imported, before VM execution.
    fn begin(&mut self, invocation: TapeInvocation<'_>) -> Option<Self::Sink>;
    /// Receives a selected sink and the tape adapter's result, before transient storage clears.
    /// Successful completion does not imply that the graph subsequently published the outputs.
    fn finish(
        &mut self,
        invocation: TapeInvocation<'_>,
        sink: Self::Sink,
        outcome: Result<(), &TapeError>,
    );
}

/// Default observer that skips every invocation and requests no trace events.
#[derive(Debug, Default)]
pub struct NoTapeObserver;
impl TraceSink for NoTapeObserver {}
impl TapeObserver for NoTapeObserver {
    type Sink = Self;
    fn begin(&mut self, _: TapeInvocation<'_>) -> Option<Self> {
        None
    }
    fn finish(&mut self, _: TapeInvocation<'_>, _: Self, _: Result<(), &TapeError>) {}
}

pub(super) struct CombinedTrace<'a, 'counter, S> {
    observer: Option<&'a mut S>,
    observer_mask: TraceMask,
    strict: &'a mut StrictDepsTrace<'counter>,
    strict_enabled: bool,
}

impl<'a, 'counter, S: TraceSink> CombinedTrace<'a, 'counter, S> {
    pub(super) fn new(
        observer: Option<&'a mut S>,
        strict: &'a mut StrictDepsTrace<'counter>,
        strict_enabled: bool,
    ) -> Self {
        let observer_mask = observer
            .as_ref()
            .map_or(TraceMask::NONE, |sink| sink.mask());
        Self {
            observer,
            observer_mask,
            strict,
            strict_enabled,
        }
    }
    fn observer(&mut self, event: TraceMask) -> Option<&mut S> {
        if self.observer_mask.contains(event) {
            self.observer.as_deref_mut()
        } else {
            None
        }
    }
}

impl<S: TraceSink> TraceSink for CombinedTrace<'_, '_, S> {
    fn mask(&self) -> TraceMask {
        self.observer_mask
            | if self.strict_enabled {
                TraceMask::HOST
            } else {
                TraceMask::NONE
            }
    }
    fn run_mask(&mut self, program: &Program, _: TraceMask) {
        // Internal strict-dependency events are filtered; report actual delivery to this sink.
        if self.observer_mask != TraceMask::NONE
            && let Some(observer) = self.observer.as_deref_mut()
        {
            observer.run_mask(program, self.observer_mask);
        }
    }
    fn wants_registers(&self, location: TraceLocation, phase: RegisterPhase) -> bool {
        self.observer_mask.contains(TraceMask::REGISTERS)
            && self
                .observer
                .as_ref()
                .is_some_and(|sink| sink.wants_registers(location, phase))
    }
    fn registers(&mut self, program: &Program, event: RegisterEvent<'_>) {
        if let Some(observer) = self.observer(TraceMask::REGISTERS) {
            observer.registers(program, event);
        }
    }
    fn run_start(&mut self, program: &Program, entry: FuncId, count: usize) {
        if let Some(observer) = self.observer(TraceMask::RUN) {
            observer.run_start(program, entry, count);
        }
    }
    fn values(&mut self, program: &Program, event: ValueEvent<'_>) {
        if let Some(observer) = self.observer(TraceMask::VALUES) {
            observer.values(program, event);
        }
    }
    fn instr(
        &mut self,
        program: &Program,
        func: FuncId,
        pc: u32,
        next: u32,
        span: Option<u64>,
        opcode: u8,
    ) {
        if let Some(observer) = self.observer(TraceMask::INSTR) {
            observer.instr(program, func, pc, next, span, opcode);
        }
    }
    fn instr_end(
        &mut self,
        program: &Program,
        location: TraceLocation,
        next: Option<TraceLocation>,
        outcome: TraceOutcome<'_>,
    ) {
        if let Some(observer) = self.observer(TraceMask::INSTR) {
            observer.instr_end(program, location, next, outcome);
        }
    }
    fn scope_enter(
        &mut self,
        program: &Program,
        kind: ScopeKind,
        depth: usize,
        func: FuncId,
        pc: u32,
        span: Option<u64>,
    ) {
        if self.strict_enabled {
            self.strict
                .scope_enter(program, kind, depth, func, pc, span);
        }
        let mask = match kind {
            ScopeKind::CallFrame { .. } => TraceMask::CALL,
            ScopeKind::HostCall { .. } => TraceMask::HOST,
        };
        if let Some(observer) = self.observer(mask) {
            observer.scope_enter(program, kind, depth, func, pc, span);
        }
    }
    fn scope_exit(
        &mut self,
        program: &Program,
        kind: ScopeKind,
        depth: usize,
        func: FuncId,
        pc: u32,
        span: Option<u64>,
        outcome: TraceOutcome<'_>,
    ) {
        if self.strict_enabled {
            self.strict
                .scope_exit(program, kind, depth, func, pc, span, outcome);
        }
        let mask = match kind {
            ScopeKind::CallFrame { .. } => TraceMask::CALL,
            ScopeKind::HostCall { .. } => TraceMask::HOST,
        };
        if let Some(observer) = self.observer(mask) {
            observer.scope_exit(program, kind, depth, func, pc, span, outcome);
        }
    }
    fn run_end(&mut self, program: &Program, outcome: TraceOutcome<'_>) {
        if let Some(observer) = self.observer(TraceMask::RUN) {
            observer.run_end(program, outcome);
        }
    }
}
