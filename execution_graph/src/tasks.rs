// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Owned deferred work and its provisional observations, outside the synchronous node layout.

use crate::dirty::{DirtyEngine, DirtyKey};
use crate::{AccessLog, NodeAccess, NodeId, ResourceKey};
use alloc::{
    boxed::Box,
    collections::{BTreeMap, BTreeSet, btree_map::Entry},
    vec::Vec,
};
use core::{
    fmt,
    task::{Context, Poll, Waker},
};
use hashbrown::HashMap;

/// One owned continuation. The graph drops it on cancellation, supersession or node removal.
///
/// Polling continues this attempt rather than starting its effects again. Keep inputs and
/// tentative results owned by the task. Only a successful ready result can publish outputs.
/// `Pending` registers the embedding's waker as needed; it does not block graph execution.
/// Performed host writes must be reported even when the task subsequently waits or fails.
/// Drop must release owned work/resources; it does not roll back external effects.
pub trait NodeTask<V, E>: fmt::Debug {
    /// Advances this task, appending tentative outputs and reporting actual resource accesses.
    /// Outputs survive pending polls; they remain invisible to consumers until validation.
    fn poll(
        &mut self,
        cx: &mut Context<'_>,
        outputs: &mut Vec<V>,
        access: &mut NodeAccess<'_, V>,
    ) -> Poll<Result<(), E>>;

    /// Optional concise description of the work currently awaited, without allocating a label.
    fn description(&self) -> Option<&str> {
        None
    }
}

/// Graph-local identity of an execution attempt, preserved across deferred polls.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct NodeAttempt {
    /// Node whose execution this identifies. IDs belong to their originating graph.
    pub node: NodeId,
    /// One-based admission count, including failed and restartable attempts.
    pub attempt: u64,
}

/// Read-only view of an owned task currently held by a graph.
#[derive(Clone, Copy, Debug)]
pub struct TaskInfo<'a> {
    /// Attempt being continued.
    pub id: NodeAttempt,
    /// Number of task polls so far, including the initial poll.
    pub polls: u64,
    /// A poll unwound. The continuation must be cancelled and will never be polled again.
    pub abandoned: bool,
    /// Optional description supplied by the task.
    pub description: Option<&'a str>,
}

/// Why an owned attempt was discarded without publishing a new result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskCancelReason {
    /// An observed resource changed while work was pending.
    Changed(ResourceKey),
    /// Authored input wiring or a bound value changed.
    Rebound,
    /// The producer was explicitly invalidated.
    Invalidated,
    /// The embedding explicitly cancelled the attempt.
    Requested,
    /// The node was removed.
    Removed,
    /// A poll unwound; its continuation cannot be retried safely.
    Abandoned,
    /// A task failed or its candidate result failed graph validation.
    Failed,
}

/// One observation from the optional task lifecycle stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskEvent {
    /// Originating node and attempt.
    pub id: NodeAttempt,
    /// Actual transition observed by the graph.
    pub kind: TaskEventKind,
}

/// Lifecycle transitions distinguish pending bodies from successful publication.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskEventKind {
    /// The graph accepted ownership of the continuation.
    Started,
    /// A poll returned pending and published nothing.
    Waiting,
    /// Output arity and dependency validation succeeded and the result was published.
    Published,
    /// The continuation and its tentative values were dropped without publication.
    Cancelled(TaskCancelReason),
}

#[derive(Debug)]
pub(crate) struct ActiveTask<V, E> {
    pub(crate) id: NodeAttempt,
    pub(crate) task: Box<dyn NodeTask<V, E>>,
    pub(crate) reads: BTreeMap<DirtyKey, u64>,
    pub(crate) writes: BTreeSet<DirtyKey>,
    pub(crate) outputs: Vec<V>,
    pub(crate) log: Option<AccessLog>,
    pub(crate) polls: u64,
    pub(crate) poisoned: bool,
    pub(crate) requested: Vec<NodeId>,
}
impl<V, E> ActiveTask<V, E> {
    pub(crate) fn observe(
        &mut self,
        reads: &[DirtyKey],
        writes: &[DirtyKey],
        revision: u64,
        dirty: &mut DirtyEngine,
    ) {
        for &key in reads {
            if let Entry::Vacant(entry) = self.reads.entry(key) {
                dirty.hold(key);
                entry.insert(revision);
            }
        }
        for &key in writes {
            if self.writes.insert(key) {
                dirty.hold(key);
            }
        }
    }
    pub(crate) fn changed(&self, dirty: &DirtyEngine) -> Option<DirtyKey> {
        self.reads.iter().find_map(|(&key, &revision)| {
            (!self.writes.contains(&key) && dirty.changed_since(key, revision)).then_some(key)
        })
    }
    pub(crate) fn release(&self, dirty: &mut DirtyEngine) {
        for &key in self.reads.keys() {
            dirty.release(key);
        }
        for &key in &self.writes {
            dirty.release(key);
        }
    }
}

#[derive(Debug)]
pub(crate) struct Tasks<V, E> {
    pub(crate) active: HashMap<NodeId, ActiveTask<V, E>>,
    pub(crate) waker: Waker,
    pub(crate) collect: bool,
    pub(crate) events: Vec<TaskEvent>,
    pub(crate) polls: u64,
}
impl<V, E> Default for Tasks<V, E> {
    fn default() -> Self {
        Self {
            active: HashMap::new(),
            waker: Waker::noop().clone(),
            collect: false,
            events: Vec::new(),
            polls: 0,
        }
    }
}
impl<V, E> Tasks<V, E> {
    pub(crate) fn event(&mut self, id: NodeAttempt, kind: TaskEventKind) {
        if self.collect {
            self.events.push(TaskEvent { id, kind });
        }
    }
    pub(crate) fn cancel(
        &mut self,
        node: NodeId,
        reason: TaskCancelReason,
        dirty: &mut DirtyEngine,
    ) -> bool {
        if let Some(task) = self.active.remove(&node) {
            task.release(dirty);
            self.event(task.id, TaskEventKind::Cancelled(reason));
            drop(task);
            return true;
        }
        false
    }
}
