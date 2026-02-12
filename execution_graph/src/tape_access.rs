// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Adapter for translating `execution_tape` access events into `execution_graph` keys.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::ToString;
use alloc::vec::Vec;
use core::cell::Cell;

use execution_tape::host::SigHash;
use execution_tape::host::{AccessSink, ResourceKeyRef};
use execution_tape::trace::{ScopeKind, TraceMask, TraceSink};
use hashbrown::HashMap;

use crate::access::{Access, AccessLog, HostOpId, ResourceKey};
use crate::dirty::{DirtyEngine, DirtyKey};

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

/// Access sink wrapper that counts reads/writes while also recording full access logs.
///
/// This is used when `ExecutionGraph` is collecting per-node [`AccessLog`] values.
#[derive(Debug)]
pub(crate) struct CountingAccessSink<'a> {
    inner: TapeAccessLog,
    counter: &'a Cell<usize>,
}

/// Builder and extraction helpers for [`CountingAccessSink`].
impl<'a> CountingAccessSink<'a> {
    /// Creates a sink that increments `counter` on every access event.
    ///
    /// The counter is shared with [`StrictDepsTrace`] so strict-deps validation can verify
    /// that each host call reported at least one key.
    #[must_use]
    #[inline]
    pub(crate) const fn new(counter: &'a Cell<usize>) -> Self {
        Self {
            inner: TapeAccessLog::new(),
            counter,
        }
    }

    /// Consumes the sink and returns the collected access log.
    #[must_use]
    #[inline]
    pub(crate) fn into_log(self) -> AccessLog {
        self.inner.into_log()
    }
}

/// Access recording behavior for the collection-enabled path.
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

#[inline]
pub(crate) fn intern_input_key_id(
    dirty: &mut DirtyEngine,
    input_ids: &mut BTreeMap<Box<str>, DirtyKey>,
    name: &str,
) -> DirtyKey {
    if let Some(&id) = input_ids.get(name) {
        return id;
    }

    // Note: we may allocate twice on first use (once for the lookup table key and once for
    // the `ResourceKey::Input` stored in the interner). Subsequent invalidations are
    // allocation-free.
    let boxed: Box<str> = name.into();
    let id = dirty.intern(ResourceKey::Input(boxed.clone()));
    input_ids.insert(boxed, id);
    id
}

#[inline]
pub(crate) fn intern_host_state_key_id(
    dirty: &mut DirtyEngine,
    host_state_ids: &mut HashMap<(HostOpId, u64), DirtyKey>,
    op: HostOpId,
    key: u64,
) -> DirtyKey {
    if let Some(&id) = host_state_ids.get(&(op, key)) {
        return id;
    }

    let id = dirty.intern(ResourceKey::host_state(op, key));
    host_state_ids.insert((op, key), id);
    id
}

#[inline]
pub(crate) fn intern_opaque_host_key_id(
    dirty: &mut DirtyEngine,
    opaque_host_ids: &mut HashMap<HostOpId, DirtyKey>,
    op: HostOpId,
) -> DirtyKey {
    if let Some(&id) = opaque_host_ids.get(&op) {
        return id;
    }

    let id = dirty.intern(ResourceKey::opaque_host(op));
    opaque_host_ids.insert(op, id);
    id
}

/// Fast-path access sink used when per-node access log collection is disabled.
///
/// It emits dependency read IDs directly into `read_ids`, avoiding intermediate `AccessLog`
/// allocation and remapping.
#[derive(Debug)]
pub(crate) struct DepsOnlyAccessSink<'a> {
    dirty: &'a mut DirtyEngine,
    input_ids: &'a mut BTreeMap<Box<str>, DirtyKey>,
    host_state_ids: &'a mut HashMap<(HostOpId, u64), DirtyKey>,
    opaque_host_ids: &'a mut HashMap<HostOpId, DirtyKey>,
    read_ids: &'a mut Vec<DirtyKey>,
    counter: &'a Cell<usize>,
}

/// Builder for the deps-only sink.
impl<'a> DepsOnlyAccessSink<'a> {
    /// Creates a sink that interns tape keys directly into dirty-key IDs.
    ///
    /// This is the hot path when `collect_access == false`.
    #[must_use]
    #[inline]
    pub(crate) const fn new(
        dirty: &'a mut DirtyEngine,
        input_ids: &'a mut BTreeMap<Box<str>, DirtyKey>,
        host_state_ids: &'a mut HashMap<(HostOpId, u64), DirtyKey>,
        opaque_host_ids: &'a mut HashMap<HostOpId, DirtyKey>,
        read_ids: &'a mut Vec<DirtyKey>,
        counter: &'a Cell<usize>,
    ) -> Self {
        Self {
            dirty,
            input_ids,
            host_state_ids,
            opaque_host_ids,
            read_ids,
            counter,
        }
    }
}

/// Access recording behavior for the deps-only fast path.
impl AccessSink for DepsOnlyAccessSink<'_> {
    #[inline]
    fn read(&mut self, key: ResourceKeyRef<'_>) {
        self.counter.set(self.counter.get().saturating_add(1));
        let read_id = match key {
            ResourceKeyRef::Input(name) => intern_input_key_id(self.dirty, self.input_ids, name),
            ResourceKeyRef::HostState { op, key } => {
                intern_host_state_key_id(self.dirty, self.host_state_ids, HostOpId::new(op.0), key)
            }
            ResourceKeyRef::OpaqueHost { op } => {
                intern_opaque_host_key_id(self.dirty, self.opaque_host_ids, HostOpId::new(op.0))
            }
        };
        self.read_ids.push(read_id);
    }

    #[inline]
    fn write(&mut self, _key: ResourceKeyRef<'_>) {
        // Strict-deps mode requires host scopes to emit at least one access event.
        self.counter.set(self.counter.get().saturating_add(1));
    }
}

/// Unified access sink used by node execution.
///
/// This allows `run_node_internal` to call `execute_kind` once while selecting either:
/// - [`NodeAccessSink::Collect`] for full access-log collection, or
/// - [`NodeAccessSink::Deps`] for direct dependency-ID emission.
#[derive(Debug)]
pub(crate) enum NodeAccessSink<'a> {
    /// Full access collection mode.
    Collect(CountingAccessSink<'a>),
    /// Deps-only fast mode (no `AccessLog` materialization).
    Deps(DepsOnlyAccessSink<'a>),
}

/// Helpers for post-execution handling of the unified sink.
impl NodeAccessSink<'_> {
    /// Returns a collected access log when the sink was in collection mode.
    ///
    /// In deps-only mode this returns `None`.
    #[must_use]
    #[inline]
    pub(crate) fn into_log(self) -> Option<AccessLog> {
        match self {
            Self::Collect(sink) => Some(sink.into_log()),
            Self::Deps(_) => None,
        }
    }
}

/// Dispatches read/write events to the active sink variant.
impl AccessSink for NodeAccessSink<'_> {
    #[inline]
    fn read(&mut self, key: ResourceKeyRef<'_>) {
        match self {
            Self::Collect(sink) => sink.read(key),
            Self::Deps(sink) => sink.read(key),
        }
    }

    #[inline]
    fn write(&mut self, key: ResourceKeyRef<'_>) {
        match self {
            Self::Collect(sink) => sink.write(key),
            Self::Deps(sink) => sink.write(key),
        }
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
