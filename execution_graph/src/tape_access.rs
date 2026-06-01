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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StrictDepsViolation {
    pub(crate) symbol: Box<str>,
    pub(crate) sig_hash: SigHash,
}

/// Access sink used when per-node access-log collection is enabled.
///
/// It performs all collect-mode host access work inline:
/// - increments the strict-deps access counter,
/// - interns read keys and appends dependency IDs to `read_ids`,
/// - appends host reads/writes directly into the caller-provided per-node [`AccessLog`].
#[derive(Debug)]
pub(crate) struct CollectingAccessSink<'a> {
    dirty: &'a mut DirtyEngine,
    input_ids: &'a mut BTreeMap<Box<str>, DirtyKey>,
    host_state_ids: &'a mut HashMap<(HostOpId, u64), DirtyKey>,
    opaque_host_ids: &'a mut HashMap<HostOpId, DirtyKey>,
    read_ids: &'a mut Vec<DirtyKey>,
    write_ids: &'a mut Vec<DirtyKey>,
    log: &'a mut AccessLog,
    counter: &'a Cell<usize>,
}

/// Builder for [`CollectingAccessSink`].
impl<'a> CollectingAccessSink<'a> {
    /// Creates a sink that increments `counter` on every access event and writes directly
    /// into dependency IDs and the provided per-node [`AccessLog`].
    ///
    /// The counter is shared with [`StrictDepsTrace`] so strict-deps validation can verify
    /// that each host call reported at least one key.
    #[must_use]
    #[inline]
    pub(crate) const fn new(
        dirty: &'a mut DirtyEngine,
        input_ids: &'a mut BTreeMap<Box<str>, DirtyKey>,
        host_state_ids: &'a mut HashMap<(HostOpId, u64), DirtyKey>,
        opaque_host_ids: &'a mut HashMap<HostOpId, DirtyKey>,
        read_ids: &'a mut Vec<DirtyKey>,
        write_ids: &'a mut Vec<DirtyKey>,
        log: &'a mut AccessLog,
        counter: &'a Cell<usize>,
    ) -> Self {
        Self {
            dirty,
            input_ids,
            host_state_ids,
            opaque_host_ids,
            read_ids,
            write_ids,
            log,
            counter,
        }
    }
}

/// Access recording behavior for the collection-enabled path.
impl AccessSink for CollectingAccessSink<'_> {
    fn read(&mut self, key: ResourceKeyRef<'_>) {
        self.counter.set(self.counter.get().saturating_add(1));
        match key {
            ResourceKeyRef::Input(name) => {
                let read_id = intern_input_key_id(self.dirty, self.input_ids, name);
                self.read_ids.push(read_id);
                self.log.push(Access::Read(ResourceKey::input(name)));
            }
            ResourceKeyRef::HostState { op, key } => {
                let op = HostOpId::new(op.0);
                let read_id = intern_host_state_key_id(self.dirty, self.host_state_ids, op, key);
                self.read_ids.push(read_id);
                self.log
                    .push(Access::Read(ResourceKey::host_state(op, key)));
            }
            ResourceKeyRef::OpaqueHost { op } => {
                let op = HostOpId::new(op.0);
                let read_id = intern_opaque_host_key_id(self.dirty, self.opaque_host_ids, op);
                self.read_ids.push(read_id);
                self.log.push(Access::Read(ResourceKey::opaque_host(op)));
            }
        }
    }

    fn write(&mut self, key: ResourceKeyRef<'_>) {
        if let Some((id, key)) =
            mark_tape_key_dirty(self.dirty, self.host_state_ids, self.opaque_host_ids, key)
        {
            // Count only accepted writes as strict-deps access events: an ignored write (e.g. to a
            // graph-owned Input key) reports nothing usable, so it must not satisfy strict-deps.
            self.counter.set(self.counter.get().saturating_add(1));
            // Record the write so a node is not re-triggered by its own write (see
            // `run_node_internal`, which excludes self-written keys from the node's dependency set).
            self.write_ids.push(id);
            self.log.push(Access::Write(key));
        }
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

/// Interns a host-written key, marks it dirty, and returns its [`DirtyKey`] id and owned
/// [`ResourceKey`].
///
/// Marking the key dirty is what invalidates *other* nodes that read it. The returned id lets the
/// caller record the write so the writing node can exclude its own writes from its dependency set
/// (preventing a read-modify-write node from re-triggering itself indefinitely).
///
/// Writes to [`ResourceKeyRef::Input`] are ignored (returns `None`): graph inputs are owned by the
/// graph (set via `set_input_value` / `invalidate_input`), not by host calls. An `Input` write
/// would intern to the same id as a node's input-binding dependency, and recording it would let
/// the self-write filter strip that binding — silently dropping a real dependency. Hosts should
/// only write the host-owned `HostState` / `OpaqueHost` namespaces.
#[inline]
fn mark_tape_key_dirty(
    dirty: &mut DirtyEngine,
    host_state_ids: &mut HashMap<(HostOpId, u64), DirtyKey>,
    opaque_host_ids: &mut HashMap<HostOpId, DirtyKey>,
    key: ResourceKeyRef<'_>,
) -> Option<(DirtyKey, ResourceKey)> {
    match key {
        // Graph-owned: not a valid host write target. Ignore rather than collide with bindings.
        ResourceKeyRef::Input(_) => None,
        ResourceKeyRef::HostState { op, key } => {
            let op = HostOpId::new(op.0);
            let id = intern_host_state_key_id(dirty, host_state_ids, op, key);
            dirty.mark_dirty(id);
            Some((id, ResourceKey::host_state(op, key)))
        }
        ResourceKeyRef::OpaqueHost { op } => {
            let op = HostOpId::new(op.0);
            let id = intern_opaque_host_key_id(dirty, opaque_host_ids, op);
            dirty.mark_dirty(id);
            Some((id, ResourceKey::opaque_host(op)))
        }
    }
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
    write_ids: &'a mut Vec<DirtyKey>,
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
        write_ids: &'a mut Vec<DirtyKey>,
        counter: &'a Cell<usize>,
    ) -> Self {
        Self {
            dirty,
            input_ids,
            host_state_ids,
            opaque_host_ids,
            read_ids,
            write_ids,
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
    fn write(&mut self, key: ResourceKeyRef<'_>) {
        if let Some((id, _key)) =
            mark_tape_key_dirty(self.dirty, self.host_state_ids, self.opaque_host_ids, key)
        {
            // Count only accepted writes as strict-deps access events: an ignored write (e.g. to a
            // graph-owned Input key) reports nothing usable, so it must not satisfy strict-deps.
            self.counter.set(self.counter.get().saturating_add(1));
            // Record the write so a node is not re-triggered by its own write (see
            // `run_node_internal`, which excludes self-written keys from the node's dependency set).
            self.write_ids.push(id);
        }
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
    Collect(CollectingAccessSink<'a>),
    /// Deps-only fast mode (no `AccessLog` materialization).
    Deps(DepsOnlyAccessSink<'a>),
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
