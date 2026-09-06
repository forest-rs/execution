// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Per-run dependency recording handed to an [`Executor`](crate::Executor).

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use hashbrown::HashMap;

use crate::access::{Access, AccessLog, HostOpId, ResourceKey};
use crate::dirty::{DirtyEngine, DirtyKey};

/// Records the reads and writes one node performs against state outside the graph.
///
/// The graph records input bindings itself. Everything else a node consults — configuration it
/// reads by name, or executor-managed state — must be reported here so incremental execution
/// knows what invalidates the node.
///
/// Reads become dependency edges. Writes mark the key dirty for *other* readers and are excluded
/// from this node's own dependency set, so a node that reads and writes the same key reaches a
/// fixpoint instead of re-triggering itself. Graph inputs are graph-owned and cannot be written
/// from a node; invalidate them with
/// [`ExecutionGraph::invalidate_input`](crate::ExecutionGraph::invalidate_input).
#[derive(Debug)]
pub struct NodeAccess<'a> {
    dirty: &'a mut DirtyEngine,
    input_ids: &'a mut BTreeMap<Box<str>, DirtyKey>,
    host_state_ids: &'a mut HashMap<(HostOpId, u64), DirtyKey>,
    opaque_host_ids: &'a mut HashMap<HostOpId, DirtyKey>,
    read_ids: &'a mut Vec<DirtyKey>,
    write_ids: &'a mut Vec<DirtyKey>,
    log: Option<&'a mut AccessLog>,
}

impl<'a> NodeAccess<'a> {
    #[must_use]
    #[inline]
    pub(crate) const fn new(
        dirty: &'a mut DirtyEngine,
        input_ids: &'a mut BTreeMap<Box<str>, DirtyKey>,
        host_state_ids: &'a mut HashMap<(HostOpId, u64), DirtyKey>,
        opaque_host_ids: &'a mut HashMap<HostOpId, DirtyKey>,
        read_ids: &'a mut Vec<DirtyKey>,
        write_ids: &'a mut Vec<DirtyKey>,
        log: Option<&'a mut AccessLog>,
    ) -> Self {
        Self {
            dirty,
            input_ids,
            host_state_ids,
            opaque_host_ids,
            read_ids,
            write_ids,
            log,
        }
    }

    /// Records a read of the external input `name`.
    ///
    /// This is the same key space as
    /// [`ExecutionGraph::set_input_value`](crate::ExecutionGraph::set_input_value): a node whose
    /// body consults a named external value without a declared input binding can still be
    /// invalidated by name.
    #[inline]
    pub fn read_input(&mut self, name: &str) {
        let id = intern_input_key_id(self.dirty, self.input_ids, name);
        self.read_ids.push(id);
        if let Some(log) = self.log.as_mut() {
            log.push(Access::Read(ResourceKey::input(name)));
        }
    }

    /// Records a read of executor-managed state `key` in the namespace of operation `op`.
    #[inline]
    pub fn read_host_state(&mut self, op: HostOpId, key: u64) {
        let id = intern_host_state_key_id(self.dirty, self.host_state_ids, op, key);
        self.read_ids.push(id);
        if let Some(log) = self.log.as_mut() {
            log.push(Access::Read(ResourceKey::host_state(op, key)));
        }
    }

    /// Records a conservative read of *some* state behind operation `op`.
    #[inline]
    pub fn read_opaque_host(&mut self, op: HostOpId) {
        let id = intern_opaque_host_key_id(self.dirty, self.opaque_host_ids, op);
        self.read_ids.push(id);
        if let Some(log) = self.log.as_mut() {
            log.push(Access::Read(ResourceKey::opaque_host(op)));
        }
    }

    /// Records a write of executor-managed state `key` in the namespace of operation `op`.
    ///
    /// Other nodes that read this key are invalidated; this node is not.
    #[inline]
    pub fn write_host_state(&mut self, op: HostOpId, key: u64) {
        let id = intern_host_state_key_id(self.dirty, self.host_state_ids, op, key);
        self.dirty.mark_dirty(id);
        self.write_ids.push(id);
        if let Some(log) = self.log.as_mut() {
            log.push(Access::Write(ResourceKey::host_state(op, key)));
        }
    }

    /// Records a conservative write of *some* state behind operation `op`.
    ///
    /// Other nodes that read the opaque key are invalidated; this node is not.
    #[inline]
    pub fn write_opaque_host(&mut self, op: HostOpId) {
        let id = intern_opaque_host_key_id(self.dirty, self.opaque_host_ids, op);
        self.dirty.mark_dirty(id);
        self.write_ids.push(id);
        if let Some(log) = self.log.as_mut() {
            log.push(Access::Write(ResourceKey::opaque_host(op)));
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
