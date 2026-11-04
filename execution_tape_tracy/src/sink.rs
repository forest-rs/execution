// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use crate::resolver::{
    DefaultLabelResolver, LabelResolver, default_call_frame_label, default_host_call_label,
};
use execution_tape::program::Program;
use execution_tape::source::{BoundSources, SourceError, SourceMap, SourceRange};
use execution_tape::trace::{ScopeKind, StackActivity, TraceLocation, TraceMask, TraceSink};
use execution_tape::vm::ActivationId;
use std::string::String;
use std::vec::Vec;

type BackendGuard = tracy_client::Span;

struct ScopeEntry {
    stack: Option<ActivationId>,
    file: Option<String>,
    line: u32,
    kind: ScopeKind,
    depth: usize,
    // Keep the label alive for backends that may borrow it.
    label: String,
    guard: Option<BackendGuard>,
}

/// A `TraceSink` that emits Tracy scopes via `tracy-client`.
pub struct ProfilingTraceSink<'a, R = DefaultLabelResolver> {
    resolver: R,
    stack: Vec<ScopeEntry>,
    parked: Vec<(ActivationId, Vec<ScopeEntry>)>,
    consistent: bool,
    sources: Option<ProgramSources<'a>>,
}

struct ProgramSources<'a> {
    program: &'a Program,
    bound: BoundSources<'a>,
}

impl ProfilingTraceSink<'_, DefaultLabelResolver> {
    /// Create a new sink with id-based labels.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl<'a, R: LabelResolver> ProfilingTraceSink<'a, R> {
    /// Create a new sink with a custom label resolver.
    #[must_use]
    pub fn with_resolver(resolver: R) -> Self {
        Self {
            resolver,
            stack: Vec::new(),
            parked: Vec::new(),
            consistent: true,
            sources: None,
        }
    }

    /// Attaches authored source locations for this exact program instance.
    ///
    /// Validates canonical program identity before execution. At captured call/host boundaries,
    /// Tracy receives the supplied document label and one-based source line where mapped.
    /// No source file is opened and no snapshot text is sent. Labels need not be filesystem paths.
    /// Reusing the sink with a different program instance falls back to bytecode locations;
    /// attach sources again for that instance. Binding is never repeated in tracing callbacks.
    pub fn sources(
        mut self,
        program: &'a Program,
        map: &'a SourceMap,
    ) -> Result<Self, SourceError> {
        self.sources = Some(ProgramSources {
            program,
            bound: map.bind(program)?,
        });
        Ok(self)
    }

    /// Resolves the same optional source range used for Tracy attribution, without a live client.
    ///
    /// Returns `None` for an unattached/different program instance or an unmapped location.
    /// The lookup borrows snapshot text and allocates no preview strings.
    pub fn source_location(
        &self,
        program: &Program,
        location: TraceLocation,
    ) -> Option<SourceRange<'a>> {
        self.sources
            .as_ref()
            .filter(|sources| core::ptr::eq(sources.program, program))?
            .bound
            .lookup(location)
    }

    fn on_scope_enter(
        &mut self,
        program: &Program,
        stack: Option<&ActivationId>,
        kind: ScopeKind,
        depth: usize,
        location: TraceLocation,
    ) {
        if !self.consistent {
            return;
        }
        let label = self.resolve_label(program, kind);
        let (file, line) = self.scope_source(program, location);
        let guard = Self::open_scope(
            kind,
            &label,
            file.as_deref().unwrap_or("execution_tape.bytecode"),
            line,
        );
        self.stack.push(ScopeEntry {
            stack: stack.cloned(),
            file,
            line,
            kind,
            depth,
            label,
            guard,
        });
    }

    fn on_scope_exit(&mut self, stack: Option<&ActivationId>, kind: ScopeKind, depth: usize) {
        if !self.consistent {
            return;
        }
        if let Some(index) = self.parked.iter().position(|(id, _)| Some(id) == stack) {
            let scopes = &mut self.parked[index].1;
            if scopes
                .last()
                .is_some_and(|scope| scope.kind == kind && scope.depth == depth)
            {
                scopes.pop();
                if scopes.is_empty() {
                    self.parked.swap_remove(index);
                }
                return;
            }
        } else if self.stack.last().is_some_and(|scope| {
            scope.stack.as_ref() == stack && scope.kind == kind && scope.depth == depth
        }) {
            self.stack.pop();
            return;
        }
        self.fail();
    }

    /// Whether delivered scope and stack transitions have remained structurally consistent.
    /// Inconsistent evidence closes physical guards and stops opening new scopes; recreate
    /// the sink before another execution rather than interpreting the profile as complete.
    #[must_use]
    pub fn is_consistent(&self) -> bool {
        self.consistent
    }

    fn fail(&mut self) {
        self.drop_active_scopes();
        self.parked.clear();
        self.consistent = false;
    }

    fn park(&mut self, id: &ActivationId) {
        if !self.consistent {
            return;
        }
        if self.parked.iter().any(|(saved, _)| saved == id) {
            self.fail();
            return;
        }
        let mut scopes = Vec::new();
        while self
            .stack
            .last()
            .is_some_and(|scope| scope.stack.as_ref() == Some(id))
        {
            let mut scope = self.stack.pop().expect("active scope");
            scope.guard.take();
            scopes.push(scope);
        }
        if self
            .stack
            .iter()
            .any(|scope| scope.stack.as_ref() == Some(id))
        {
            self.fail();
            return;
        }
        if !scopes.is_empty() {
            scopes.reverse();
            self.parked.push((id.clone(), scopes));
        }
    }

    fn attach(&mut self, id: &ActivationId) {
        if !self.consistent {
            return;
        }
        if self
            .stack
            .iter()
            .any(|scope| scope.stack.as_ref() == Some(id))
        {
            self.fail();
            return;
        }
        if let Some(index) = self.parked.iter().position(|(saved, _)| saved == id) {
            let (_, scopes) = self.parked.swap_remove(index);
            for mut scope in scopes {
                scope.guard = Self::open_scope(
                    scope.kind,
                    &scope.label,
                    scope.file.as_deref().unwrap_or("execution_tape.bytecode"),
                    scope.line,
                );
                self.stack.push(scope);
            }
        }
    }

    fn resolve_label(&mut self, program: &Program, kind: ScopeKind) -> String {
        match kind {
            ScopeKind::CallFrame { func, .. } => self
                .resolver
                .call_frame_label(func, program)
                .unwrap_or_else(|| default_call_frame_label(func)),
            ScopeKind::HostCall { host_sig, .. } => self
                .resolver
                .host_call_label(host_sig, program)
                .unwrap_or_else(|| default_host_call_label(host_sig, program)),
        }
    }

    fn scope_source(&self, program: &Program, location: TraceLocation) -> (Option<String>, u32) {
        if tracy_client::Client::running().is_none() {
            return (None, location.pc);
        }
        let mapped = self.source_location(program, location).and_then(|source| {
            u32::try_from(source.start().line)
                .ok()
                .map(|line| (Some(source.document().name.clone()), line))
        });
        mapped.unwrap_or((None, location.pc))
    }

    fn open_scope(kind: ScopeKind, label: &str, file: &str, line: u32) -> Option<BackendGuard> {
        let function_name = match kind {
            ScopeKind::CallFrame { .. } => "execution_tape.call_frame",
            ScopeKind::HostCall { .. } => "execution_tape.host_call",
        };
        let client = tracy_client::Client::running()?;
        Some(client.span_alloc(Some(label), function_name, file, line, 0))
    }

    // Drop in LIFO order so nested spans close inner-to-outer.
    fn drop_active_scopes(&mut self) {
        while let Some(entry) = self.stack.pop() {
            let ScopeEntry {
                label: _label,
                guard: _guard,
                ..
            } = entry;
            let _ = (_label, _guard);
        }
    }
}

impl<R: LabelResolver> TraceSink for ProfilingTraceSink<'_, R> {
    fn mask(&self) -> TraceMask {
        TraceMask::CALL | TraceMask::HOST
    }

    fn stack_activity(
        &mut self,
        context: execution_tape::trace::TraceContext<'_>,
        activity: StackActivity<'_>,
    ) {
        if let Some(id) = context.stack() {
            match activity {
                StackActivity::Parked => self.park(id),
                StackActivity::Attached { .. } => self.attach(id),
            }
        }
    }

    fn scope_enter(
        &mut self,
        context: execution_tape::trace::TraceContext<'_>,
        kind: ScopeKind,
        depth: usize,
        func: execution_tape::value::FuncId,
        pc: u32,
        _span_id: Option<u64>,
    ) {
        let program = context.program();
        self.on_scope_enter(
            program,
            context.stack(),
            kind,
            depth,
            TraceLocation { func, pc },
        );
    }

    fn scope_exit(
        &mut self,
        context: execution_tape::trace::TraceContext<'_>,
        kind: ScopeKind,
        depth: usize,
        _func: execution_tape::value::FuncId,
        _pc: u32,
        _span_id: Option<u64>,
        _outcome: execution_tape::trace::TraceOutcome<'_>,
    ) {
        self.on_scope_exit(context.stack(), kind, depth);
    }
}

impl<R> Default for ProfilingTraceSink<'_, R>
where
    R: LabelResolver + Default,
{
    fn default() -> Self {
        Self::with_resolver(R::default())
    }
}

impl<R> std::fmt::Debug for ProfilingTraceSink<'_, R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProfilingTraceSink")
            .field("stack_depth", &self.stack.len())
            .field("parked_stacks", &self.parked.len())
            .field("consistent", &self.consistent)
            .field("sources_attached", &self.sources.is_some())
            .finish_non_exhaustive()
    }
}

impl<R> Drop for ProfilingTraceSink<'_, R> {
    fn drop(&mut self) {
        // Vec's ordinary element-drop order would close outer scopes before inner scopes.
        while let Some(scope) = self.stack.pop() {
            drop(scope);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ProfilingTraceSink;
    use execution_tape::trace::ScopeKind;
    use execution_tape::value::FuncId;

    #[test]
    fn start_scope_without_tracy_client_does_not_panic() {
        let _guard = ProfilingTraceSink::<super::DefaultLabelResolver>::open_scope(
            ScopeKind::CallFrame {
                func: FuncId(0),
                caller: None,
            },
            "test",
            "execution_tape.bytecode",
            0,
        );
    }
}
