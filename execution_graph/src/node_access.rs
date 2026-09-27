// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Per-run dependency recording handed to an executor.

use crate::access::{Access, AccessLog, HostOpId, ResourceKey};
use crate::dirty::{DirtyEngine, DirtyKey};
use alloc::vec::Vec;

/// Records reads and writes of state outside the graph.
///
/// Input bindings are recorded by the graph. Other consulted state must be reported here.
/// Writes invalidate existing readers immediately, even if execution later fails; host effects
/// are not rolled back. Keys read and written by the same node are excluded from its dependency
/// set, so a read-modify-write node does not retrigger itself.
#[derive(Debug)]
pub struct NodeAccess<'a> {
    dirty: &'a mut DirtyEngine,
    read_ids: &'a mut Vec<DirtyKey>,
    write_ids: &'a mut Vec<DirtyKey>,
    log: Option<&'a mut AccessLog>,
}

impl<'a> NodeAccess<'a> {
    pub(crate) fn new(
        dirty: &'a mut DirtyEngine,
        read_ids: &'a mut Vec<DirtyKey>,
        write_ids: &'a mut Vec<DirtyKey>,
        log: Option<&'a mut AccessLog>,
    ) -> Self {
        Self {
            dirty,
            read_ids,
            write_ids,
            log,
        }
    }
    fn read(&mut self, key: ResourceKey) {
        let id = self.dirty.intern(key.clone());
        self.read_ids.push(id);
        if let Some(log) = &mut self.log {
            log.push(Access::Read(key));
        }
    }
    fn write(&mut self, key: ResourceKey) {
        let id = self.dirty.intern(key.clone());
        self.dirty.mark_dirty(id);
        self.write_ids.push(id);
        if let Some(log) = &mut self.log {
            log.push(Access::Write(key));
        }
    }
    /// Records a named external input read.
    pub fn read_input(&mut self, name: &str) {
        self.read(ResourceKey::input(name));
    }
    /// Records an integer-identified external input read, independent of positional input slots.
    pub fn read_input_id(&mut self, key: u64) {
        self.read(ResourceKey::InputId(key));
    }
    /// Records a read of executor-managed state in an operation's namespace.
    pub fn read_host_state(&mut self, op: HostOpId, key: u64) {
        self.read(ResourceKey::host_state(op, key));
    }
    /// Records a conservative read of state behind an operation.
    pub fn read_opaque_host(&mut self, op: HostOpId) {
        self.read(ResourceKey::opaque_host(op));
    }
    /// Records an already-performed host-state write, invalidating other readers.
    pub fn write_host_state(&mut self, op: HostOpId, key: u64) {
        self.write(ResourceKey::host_state(op, key));
    }
    /// Records an already-performed opaque write, invalidating other readers.
    pub fn write_opaque_host(&mut self, op: HostOpId) {
        self.write(ResourceKey::opaque_host(op));
    }
}
