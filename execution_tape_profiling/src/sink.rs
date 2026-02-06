// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use crate::resolver::{
    DefaultLabelResolver, LabelResolver, default_call_frame_label, default_host_call_label,
};
use execution_tape::program::Program;
use execution_tape::trace::{ScopeKind, TraceMask, TraceSink};
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
pub struct ProfilingTraceSink<R = DefaultLabelResolver> {
    resolver: R,
    stack: Vec<ScopeEntry>,
}

impl ProfilingTraceSink<DefaultLabelResolver> {
    /// Create a new sink with id-based labels.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl<R: LabelResolver> ProfilingTraceSink<R> {
    /// Create a new sink with a custom label resolver.
    #[must_use]
    pub fn with_resolver(resolver: R) -> Self {
        Self {
            resolver,
            stack: Vec::new(),
        }
    }

    fn on_scope_enter(&mut self, program: &Program, kind: ScopeKind, depth: usize, pc: u32) {
        let label = self.resolve_label(program, kind);
        let guard = self.start_scope(kind, &label, pc);
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
            ScopeKind::CallFrame { func } => self
                .resolver
                .call_frame_label(func, program)
                .unwrap_or_else(|| default_call_frame_label(func)),
            ScopeKind::HostCall { host_sig, .. } => self
                .resolver
                .host_call_label(host_sig, program)
                .unwrap_or_else(|| default_host_call_label(host_sig, program)),
        }
    }

    fn start_scope(&self, kind: ScopeKind, label: &str, pc: u32) -> Option<BackendGuard> {
        let function_name = match kind {
            ScopeKind::CallFrame { .. } => "execution_tape.call_frame",
            ScopeKind::HostCall { .. } => "execution_tape.host_call",
        };
        let client = tracy_client::Client::running()?;
        Some(client.span_alloc(Some(label), function_name, "execution_tape", pc, 0))
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

impl<R: LabelResolver> TraceSink for ProfilingTraceSink<R> {
    fn mask(&self) -> TraceMask {
        TraceMask::CALL | TraceMask::HOST
    }

    fn scope_enter(
        &mut self,
        program: &Program,
        kind: ScopeKind,
        depth: usize,
        _func: execution_tape::value::FuncId,
        pc: u32,
        _span_id: Option<u64>,
    ) {
        self.on_scope_enter(program, kind, depth, pc);
    }

    fn scope_exit(
        &mut self,
        _program: &Program,
        kind: ScopeKind,
        depth: usize,
        _func: execution_tape::value::FuncId,
        _pc: u32,
        _span_id: Option<u64>,
    ) {
        self.on_scope_exit(kind, depth);
    }
}

impl<R> Default for ProfilingTraceSink<R>
where
    R: LabelResolver + Default,
{
    fn default() -> Self {
        Self::with_resolver(R::default())
    }
}

impl<R> std::fmt::Debug for ProfilingTraceSink<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProfilingTraceSink")
            .field("stack_depth", &self.stack.len())
            .finish_non_exhaustive()
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
        let _guard = sink.start_scope(ScopeKind::CallFrame { func: FuncId(0) }, "test", 0);
    }
}
