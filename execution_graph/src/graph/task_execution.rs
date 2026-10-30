// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Deferred polling uses the ordinary publication boundary after validating provisional reads.

use super::*;

impl<X: Executor> ExecutionGraph<X> {
    /// Installs the embedding's wake notification for future task polls.
    ///
    /// The default is a no-op waker: manually call `run_node` or `run_all` to poll again.
    /// A wake requests another query; it must not synchronously reenter this borrowed graph.
    /// Existing tasks receive the replacement waker on their next poll.
    pub fn set_task_waker(&mut self, waker: Waker) {
        self.tasks.waker = waker;
    }

    /// Enables task lifecycle collection. Disabled by default; no events are retained then.
    /// Disabling clears collected events. This does not change execution or task lifetimes.
    pub fn set_collect_task_events(&mut self, collect: bool) {
        self.tasks.collect = collect;
        if !collect {
            self.tasks.events.clear();
        }
    }

    /// Takes collected transitions, including cancellations caused by edits outside a query.
    pub fn take_task_events(&mut self) -> Vec<TaskEvent> {
        core::mem::take(&mut self.tasks.events)
    }

    /// Inspects the owned continuation currently held for a node, without polling it.
    #[must_use]
    pub fn task_info(&self, node: NodeId) -> Option<TaskInfo<'_>> {
        self.tasks.active.get(&node).map(|task| TaskInfo {
            id: task.id,
            polls: task.polls,
            abandoned: task.poisoned,
            description: task.task.description(),
        })
    }

    /// Borrows resources observed so far by an owned task, excluding resources it wrote.
    ///
    /// This provisional prefix is distinct from `node_dependencies`, which describes the last
    /// successful publication. It does not predict reads the continuation has not performed.
    pub fn task_dependencies(&self, node: NodeId) -> Option<impl Iterator<Item = &ResourceKey>> {
        let task = self.tasks.active.get(&node)?;
        Some(
            task.reads
                .keys()
                .filter(|key| !task.writes.contains(key))
                .map(|&key| self.dirty.key(key)),
        )
    }

    /// Cancels an owned attempt and drops its tentative values, retaining the committed cache.
    ///
    /// Returns false when there is no owned task. Already performed host effects remain.
    /// A later query can start a new attempt; cancellation does not remove or disable the node.
    pub fn cancel_task(&mut self, node: NodeId) -> Result<bool, GraphError<X::Error>> {
        let n = self
            .nodes
            .get(usize::try_from(node.as_u64()).map_err(|_| GraphError::BadNodeId)?)
            .ok_or(GraphError::BadNodeId)?;
        let cancelled = self
            .tasks
            .cancel(node, TaskCancelReason::Requested, &mut self.dirty);
        if cancelled {
            for &id in &n.output_ids {
                self.dirty.mark_dirty(id);
            }
            self.dirty.collect_unused();
        }
        Ok(cancelled)
    }

    pub(crate) fn cancel_changed_tasks(&mut self) {
        if self.tasks.active.is_empty() {
            return;
        }
        let mut changed: Vec<_> = self
            .tasks
            .active
            .iter()
            .filter_map(|(&node, task)| {
                task.changed(&self.dirty)
                    .map(|key| (node, self.dirty.key(key).clone()))
            })
            .collect();
        changed.sort_unstable_by_key(|(node, _)| *node);
        for (node, key) in changed {
            self.tasks
                .cancel(node, TaskCancelReason::Changed(key), &mut self.dirty);
        }
        self.dirty.collect_unused();
    }

    pub(crate) fn task_polls(&self) -> u64 {
        self.tasks.polls
    }

    pub(crate) fn cancel_failed_tasks(&mut self, nodes: &[NodeId]) {
        for &node in nodes {
            self.tasks
                .cancel(node, TaskCancelReason::Failed, &mut self.dirty);
        }
        self.dirty.collect_unused();
    }

    pub(super) fn poll_task(&mut self, node: NodeId) -> Result<NodeStep, GraphError<X::Error>> {
        let task = &self.tasks.active[&node];
        let mut pending: Vec<_> = task
            .requested
            .iter()
            .copied()
            .chain(
                task.reads
                    .keys()
                    .filter_map(|&key| match self.dirty.key(key) {
                        ResourceKey::NodeOutput { node: producer, .. } => Some(*producer),
                        _ => None,
                    }),
            )
            .filter(|producer| {
                self.nodes[usize::try_from(producer.as_u64()).unwrap()]
                    .output_ids
                    .iter()
                    .any(|&key| self.dirty.is_pending(key))
            })
            .collect();
        pending.sort_unstable();
        pending.dedup();
        if !pending.is_empty() {
            return Ok(NodeStep::Pending(pending));
        }
        self.scratch.read_ids.clear();
        self.scratch.write_ids.clear();
        let revision = self.dirty.revision();
        let mut pending = Vec::new();
        let outcome = {
            let task = self.tasks.active.get_mut(&node).expect("owned task");
            task.poisoned = true;
            task.polls = task
                .polls
                .checked_add(1)
                .expect("task poll count exhausted");
            self.tasks.polls = self.tasks.polls.saturating_add(1);
            let mut access = NodeAccess::new(
                node,
                task.id.attempt,
                &mut self.dirty,
                &mut self.scratch.read_ids,
                &mut self.scratch.write_ids,
                task.log.as_mut(),
                &self.nodes,
                &mut pending,
            );
            let outcome = task.task.poll(
                &mut Context::from_waker(&self.tasks.waker),
                &mut task.outputs,
                &mut access,
            );
            task.poisoned = false;
            task.observe(
                &self.scratch.read_ids,
                &self.scratch.write_ids,
                revision,
                &mut self.dirty,
            );
            task.requested = pending.clone();
            outcome
        };
        if let Some(key) = self.tasks.active[&node].changed(&self.dirty) {
            let resource = self.dirty.key(key).clone();
            self.tasks
                .cancel(node, TaskCancelReason::Changed(resource), &mut self.dirty);
            self.tasks.waker.wake_by_ref();
            self.dirty.collect_unused();
            return Ok(NodeStep::Blocked);
        }
        match outcome {
            Poll::Pending => {
                let task = &self.tasks.active[&node];
                self.tasks.event(task.id, TaskEventKind::Waiting);
                self.dirty.collect_unused();
                if pending.is_empty() {
                    Ok(NodeStep::Waiting)
                } else {
                    Ok(NodeStep::Pending(pending))
                }
            }
            Poll::Ready(Err(source)) => {
                self.tasks
                    .cancel(node, TaskCancelReason::Failed, &mut self.dirty);
                self.dirty.collect_unused();
                Err(GraphError::Node { node, source })
            }
            Poll::Ready(Ok(())) => {
                let task = self.tasks.active.remove(&node).expect("owned ready task");
                task.release(&mut self.dirty);
                self.scratch.read_ids.clear();
                self.scratch.read_ids.extend(task.reads.keys().copied());
                self.scratch.write_ids.clear();
                self.scratch.write_ids.extend(task.writes.iter().copied());
                let result = self.finish_attempt(node, task.outputs, task.log, pending, false);
                self.tasks.event(
                    task.id,
                    if matches!(result, Ok(NodeStep::Executed)) {
                        TaskEventKind::Published
                    } else {
                        TaskEventKind::Cancelled(TaskCancelReason::Failed)
                    },
                );
                result
            }
        }
    }
}
