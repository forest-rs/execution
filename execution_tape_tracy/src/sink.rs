// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use crate::resolver::{
    DefaultLabelResolver, LabelResolver, default_call_frame_label, default_host_call_label,
};
use execution_tape::program::Program;
use execution_tape::source::{BoundSources, SourceError, SourceMap, SourceRange};
use execution_tape::trace::{ScopeKind, TraceLocation, TraceMask, TraceSink};
use std::string::String;
use std::vec::Vec;

type BackendGuard = tracy_client::Span;

struct ScopeEntry {
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
        kind: ScopeKind,
        depth: usize,
        location: TraceLocation,
    ) {
        let label = self.resolve_label(program, kind);
        let guard = self.start_scope(program, kind, &label, location);
        self.stack.push(ScopeEntry {
            kind,
            depth,
            label,
            guard,
        });
    }

    fn on_scope_exit(&mut self, kind: ScopeKind, depth: usize) {
        if let Some(top) = self.stack.last()
            && top.kind == kind
            && top.depth == depth
        {
            if let Some(entry) = self.stack.pop() {
                let ScopeEntry {
                    label: _label,
                    guard: _guard,
                    ..
                } = entry;
                let _ = (_label, _guard);
            }
            return;
        }
        // If the stack got out of sync, drop any active scopes to avoid leaking.
        self.drop_active_scopes();
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

    fn start_scope(
        &self,
        program: &Program,
        kind: ScopeKind,
        label: &str,
        location: TraceLocation,
    ) -> Option<BackendGuard> {
        let function_name = match kind {
            ScopeKind::CallFrame { .. } => "execution_tape.call_frame",
            ScopeKind::HostCall { .. } => "execution_tape.host_call",
        };
        let client = tracy_client::Client::running()?;
        let source = self.source_location(program, location);
        let mapped = source.as_ref().and_then(|source| {
            u32::try_from(source.start().line)
                .ok()
                .map(|line| (source.document().name.as_str(), line))
        });
        let (file, line) = mapped.unwrap_or(("execution_tape.bytecode", location.pc));
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

    fn scope_enter(
        &mut self,
        program: &Program,
        kind: ScopeKind,
        depth: usize,
        func: execution_tape::value::FuncId,
        pc: u32,
        _span_id: Option<u64>,
    ) {
        self.on_scope_enter(program, kind, depth, TraceLocation { func, pc });
    }

    fn scope_exit(
        &mut self,
        _program: &Program,
        kind: ScopeKind,
        depth: usize,
        _func: execution_tape::value::FuncId,
        _pc: u32,
        _span_id: Option<u64>,
        _outcome: execution_tape::trace::TraceOutcome<'_>,
    ) {
        self.on_scope_exit(kind, depth);
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
        let sink = ProfilingTraceSink::new();
        let program = execution_tape::asm::ProgramBuilder::new().build();
        let _guard = sink.start_scope(
            &program,
            ScopeKind::CallFrame {
                func: FuncId(0),
                caller: None,
            },
            "test",
            execution_tape::trace::TraceLocation {
                func: FuncId(0),
                pc: 0,
            },
        );
    }
}
