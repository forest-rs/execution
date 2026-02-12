// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Adapter for translating `execution_tape` access events into `execution_graph` keys.

use alloc::boxed::Box;
use alloc::string::ToString;
use alloc::vec::Vec;
use core::cell::Cell;

use execution_tape::host::SigHash;
use execution_tape::host::{AccessSink, ResourceKeyRef};
use execution_tape::trace::{ScopeKind, TraceMask, TraceSink};

use crate::access::{Access, AccessLog, HostOpId, ResourceKey};

/// Records `execution_tape` host access events as `execution_graph` [`Access`] entries.
#[derive(Clone, Debug, Default)]
pub(crate) struct TapeAccessLog {
    log: AccessLog,
}

impl TapeAccessLog {
    /// Creates an empty access log.
    #[must_use]
    #[inline]
    pub(crate) const fn new() -> Self {
        Self {
            log: AccessLog::new(),
        }
    }

    #[must_use]
    #[inline]
    pub(crate) fn into_log(self) -> AccessLog {
        self.log
    }

    #[inline]
    fn map_key(key: ResourceKeyRef<'_>) -> ResourceKey {
        match key {
            ResourceKeyRef::Input(name) => ResourceKey::input(name),
            ResourceKeyRef::HostState { op, key } => {
                ResourceKey::host_state(HostOpId::new(op.0), key)
            }
            ResourceKeyRef::OpaqueHost { op } => ResourceKey::opaque_host(HostOpId::new(op.0)),
        }
    }
}

impl AccessSink for TapeAccessLog {
    fn read(&mut self, key: ResourceKeyRef<'_>) {
        self.log.push(Access::read(Self::map_key(key)));
    }

    fn write(&mut self, key: ResourceKeyRef<'_>) {
        self.log.push(Access::write(Self::map_key(key)));
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StrictDepsViolation {
    pub(crate) symbol: Box<str>,
    pub(crate) sig_hash: SigHash,
}

/// Access sink wrapper that counts reads/writes.
#[derive(Debug)]
pub(crate) struct CountingAccessSink<'a> {
    inner: TapeAccessLog,
    counter: &'a Cell<usize>,
}

impl<'a> CountingAccessSink<'a> {
    #[must_use]
    #[inline]
    pub(crate) const fn new(counter: &'a Cell<usize>) -> Self {
        Self {
            inner: TapeAccessLog::new(),
            counter,
        }
    }

    #[must_use]
    #[inline]
    pub(crate) fn into_log(self) -> AccessLog {
        self.inner.into_log()
    }
}

impl AccessSink for CountingAccessSink<'_> {
    fn read(&mut self, key: ResourceKeyRef<'_>) {
        self.counter.set(self.counter.get().saturating_add(1));
        self.inner.read(key);
    }

    fn write(&mut self, key: ResourceKeyRef<'_>) {
        self.counter.set(self.counter.get().saturating_add(1));
        self.inner.write(key);
    }
}

/// Trace sink for strict dependency tracking: requires each host call to record at least one
/// access key.
#[derive(Debug)]
pub(crate) struct StrictDepsTrace<'a> {
    counter: &'a Cell<usize>,
    stack: Vec<(usize, execution_tape::program::SymbolId, SigHash)>,
    violation: Option<StrictDepsViolation>,
}

impl<'a> StrictDepsTrace<'a> {
    #[must_use]
    #[inline]
    pub(crate) const fn new(counter: &'a Cell<usize>) -> Self {
        Self {
            counter,
            stack: Vec::new(),
            violation: None,
        }
    }

    #[must_use]
    #[inline]
    pub(crate) fn violation(&self) -> Option<&StrictDepsViolation> {
        self.violation.as_ref()
    }
}

impl TraceSink for StrictDepsTrace<'_> {
    fn mask(&self) -> TraceMask {
        TraceMask::HOST
    }

    fn scope_enter(
        &mut self,
        _program: &execution_tape::program::Program,
        kind: ScopeKind,
        _depth: usize,
        _func: execution_tape::value::FuncId,
        _pc: u32,
        _span_id: Option<u64>,
    ) {
        let ScopeKind::HostCall {
            symbol, sig_hash, ..
        } = kind
        else {
            return;
        };
        self.stack.push((self.counter.get(), symbol, sig_hash));
    }

    fn scope_exit(
        &mut self,
        program: &execution_tape::program::Program,
        kind: ScopeKind,
        _depth: usize,
        _func: execution_tape::value::FuncId,
        _pc: u32,
        _span_id: Option<u64>,
    ) {
        let ScopeKind::HostCall { .. } = kind else {
            return;
        };

        let Some((start, symbol, sig_hash)) = self.stack.pop() else {
            return;
        };

        if self.violation.is_some() {
            return;
        }
        if self.counter.get() != start {
            return;
        }

        let sym = program
            .symbol_str(symbol)
            .unwrap_or("<invalid symbol>")
            .to_string()
            .into_boxed_str();

        self.violation = Some(StrictDepsViolation {
            symbol: sym,
            sig_hash,
        });
    }
}
