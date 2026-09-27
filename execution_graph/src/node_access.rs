// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Per-run dependency recording handed to an executor.

use crate::access::{Access, AccessLog, HostOpId, ResourceKey};
use crate::dirty::{DirtyEngine, DirtyKey};
use crate::{NodeId, OutputId};
use alloc::vec::Vec;
use core::fmt;

/// Reads current graph outputs and records access to state outside the graph.
///
/// Input bindings are recorded by the graph. Other consulted state must be reported here.
/// Writes invalidate existing readers immediately, even if execution later fails; host effects
/// are not rolled back. Keys read and written by the same node are excluded from its dependency
/// set, so a read-modify-write node does not retrigger itself.
pub struct NodeAccess<'a, V> {
    dirty: &'a mut DirtyEngine,
    read_ids: &'a mut Vec<DirtyKey>,
    write_ids: &'a mut Vec<DirtyKey>,
    log: Option<&'a mut AccessLog>,
    outputs: &'a dyn OutputReader<V>,
    pending: &'a mut Vec<NodeId>,
}

impl<'a, V> NodeAccess<'a, V> {
    pub(crate) fn new(
        dirty: &'a mut DirtyEngine,
        read_ids: &'a mut Vec<DirtyKey>,
        write_ids: &'a mut Vec<DirtyKey>,
        log: Option<&'a mut AccessLog>,
        outputs: &'a dyn OutputReader<V>,
        pending: &'a mut Vec<NodeId>,
    ) -> Self {
        Self {
            dirty,
            read_ids,
            write_ids,
            log,
            outputs,
            pending,
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

impl<V: Clone> NodeAccess<'_, V> {
    /// Reads a current graph output and records the dependency on success.
    ///
    /// `Ok(None)` means the producer needs verification or execution; no cached value is
    /// returned. A restartable computation must then return `NodeOutcome::Pending`.
    /// Multiple pending producers may be requested in one attempt. Missing nodes and slots
    /// are errors, including references to removed producers. Names can be resolved before
    /// execution with `ExecutionGraph::output_id`.
    pub fn read_node_output(
        &mut self,
        node: NodeId,
        output: OutputId,
    ) -> Result<Option<V>, OutputReadError> {
        let view = self.outputs.output(node, output)?;
        if view.outputs.iter().any(|&key| self.dirty.is_pending(key)) {
            if !self.pending.contains(&node) {
                self.pending.push(node);
            }
            return Ok(None);
        }
        let value = view
            .value
            .ok_or(OutputReadError::Unpublished { node, output })?
            .clone();
        self.read_ids.push(view.key);
        if let Some(log) = &mut self.log {
            log.push(Access::Read(ResourceKey::node_output(node, output)));
        }
        Ok(Some(value))
    }
}

impl<V> fmt::Debug for NodeAccess<'_, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NodeAccess")
            .field("pending", &self.pending)
            .finish_non_exhaustive()
    }
}

/// A dynamic output read that cannot be satisfied by scheduling a live producer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputReadError {
    /// Producer identity is absent or permanently retired.
    MissingNode {
        /// Requested producer.
        node: NodeId,
    },
    /// The producer has no such declared output slot.
    MissingOutput {
        /// Requested producer.
        node: NodeId,
        /// Requested slot.
        output: OutputId,
    },
    /// No committed value exists although the producer is not pending.
    Unpublished {
        /// Requested producer.
        node: NodeId,
        /// Requested slot.
        output: OutputId,
    },
}
impl fmt::Display for OutputReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingNode { node } => write!(
                f,
                "output producer {} is absent; use a live node identity",
                node.as_u64()
            ),
            Self::MissingOutput { node, output } => write!(
                f,
                "producer {} has no output slot {}; resolve a declared output",
                node.as_u64(),
                output.index()
            ),
            Self::Unpublished { node, output } => write!(
                f,
                "producer {} output {} has no committed value",
                node.as_u64(),
                output.index()
            ),
        }
    }
}
impl core::error::Error for OutputReadError {}

pub(crate) struct OutputView<'a, V> {
    pub(crate) key: DirtyKey,
    pub(crate) outputs: &'a [DirtyKey],
    pub(crate) value: Option<&'a V>,
}
pub(crate) trait OutputReader<V> {
    fn output(&self, node: NodeId, output: OutputId) -> Result<OutputView<'_, V>, OutputReadError>;
}
