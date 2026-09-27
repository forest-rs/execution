// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Single-channel dependency storage with consumer-owned pending work.
//!
//! Invalidation walks existing consumers once. Scoped queries walk only pending dependencies
//! of their targets; unread host writes retain a revision, not a pending root. Revisions make
//! cutoff independent of drain boundaries. Cause links share prefixes across fanout.

use crate::ResourceKey;
use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::vec::Vec;
use hashbrown::{HashMap, HashSet};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub(crate) struct DirtyKey(usize);

#[derive(Debug)]
struct Entry {
    key: ResourceKey,
    dependencies: Vec<DirtyKey>,
    consumers: Vec<DirtyKey>,
    changed_at: u64,
    active: bool,
}

#[derive(Debug)]
struct Cause {
    key: ResourceKey,
    parent: Option<Rc<Self>>,
}

impl Drop for Cause {
    fn drop(&mut self) {
        // Release a uniquely owned prefix iteratively: a long chain must not consume the stack.
        let mut parent = self.parent.take();
        while let Some(link) = parent {
            match Rc::try_unwrap(link) {
                Ok(mut cause) => parent = cause.parent.take(),
                Err(_) => break,
            }
        }
    }
}

#[derive(Debug)]
struct Pending {
    cause: Rc<Cause>,
    forced: bool,
}

#[derive(Debug, Default)]
pub(crate) struct DirtyEngine {
    ids: HashMap<ResourceKey, DirtyKey>,
    entries: Vec<Entry>,
    pending: BTreeMap<DirtyKey, Pending>,
    revision: u64,
    free: Vec<DirtyKey>,
    unused: Vec<DirtyKey>,
}

impl DirtyEngine {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn intern(&mut self, key: ResourceKey) -> DirtyKey {
        if let Some(&id) = self.ids.get(&key) {
            return id;
        }
        let id = self.free.pop().unwrap_or(DirtyKey(self.entries.len()));
        self.ids.insert(key.clone(), id);
        let entry = Entry {
            key,
            dependencies: Vec::new(),
            consumers: Vec::new(),
            changed_at: 0,
            active: true,
        };
        if id.0 == self.entries.len() {
            self.entries.push(entry);
        } else {
            self.entries[id.0] = entry;
        }
        self.unused.push(id);
        id
    }

    pub(crate) fn stats(&self) -> (usize, usize, usize, usize) {
        (
            self.ids.len(),
            self.entries.iter().map(|e| e.dependencies.len()).sum(),
            self.pending.len(),
            self.entries.capacity(),
        )
    }

    pub(crate) fn lookup(&self, key: &ResourceKey) -> Option<DirtyKey> {
        self.ids.get(key).copied()
    }

    pub(crate) fn remove(&mut self, id: DirtyKey) -> Vec<DirtyKey> {
        let entry = &mut self.entries[id.0];
        if !entry.active {
            return Vec::new();
        }
        entry.active = false;
        self.ids.remove(&entry.key);
        entry.key = ResourceKey::InputId(0);
        self.pending.remove(&id);
        let dependencies = core::mem::take(&mut entry.dependencies);
        let consumers = core::mem::take(&mut entry.consumers);
        for dependency in dependencies {
            self.entries[dependency.0]
                .consumers
                .retain(|&key| key != id);
            self.unused.push(dependency);
        }
        for &consumer in &consumers {
            self.entries[consumer.0]
                .dependencies
                .retain(|&key| key != id);
        }
        self.free.push(id);
        consumers
    }

    pub(crate) fn collect_unused(&mut self) {
        while let Some(id) = self.unused.pop() {
            let entry = &self.entries[id.0];
            if entry.active
                && entry.consumers.is_empty()
                && !matches!(entry.key, ResourceKey::NodeOutput { .. })
            {
                self.remove(id);
            }
        }
    }

    pub(crate) fn key(&self, id: DirtyKey) -> &ResourceKey {
        &self.entries[id.0].key
    }
    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }
    pub(crate) fn changed_since(&self, id: DirtyKey, revision: u64) -> bool {
        self.entries[id.0].changed_at > revision
    }
    pub(crate) fn is_forced(&self, id: DirtyKey) -> bool {
        self.pending.get(&id).is_some_and(|p| p.forced)
    }

    fn change(&mut self, id: DirtyKey) {
        // Exhausting a 64-bit revision clock is not recoverable without resetting every reader.
        self.revision = self
            .revision
            .checked_add(1)
            .expect("graph revision exhausted");
        self.entries[id.0].changed_at = self.revision;
    }

    pub(crate) fn mark_dirty(&mut self, id: DirtyKey) {
        let cause = Rc::new(Cause {
            key: self.key(id).clone(),
            parent: None,
        });
        if matches!(self.key(id), ResourceKey::NodeOutput { .. }) {
            self.pending
                .entry(id)
                .or_insert_with(|| Pending {
                    cause: cause.clone(),
                    forced: true,
                })
                .forced = true;
        } else {
            self.change(id);
        }
        self.propagate(id, cause);
    }

    fn propagate(&mut self, id: DirtyKey, cause: Rc<Cause>) {
        let mut queue = alloc::vec![(id, cause)];
        while let Some((key, parent)) = queue.pop() {
            for &consumer in &self.entries[key.0].consumers {
                if self.pending.contains_key(&consumer) {
                    continue;
                }
                let cause = Rc::new(Cause {
                    key: self.entries[consumer.0].key.clone(),
                    parent: Some(parent.clone()),
                });
                self.pending.insert(
                    consumer,
                    Pending {
                        cause: cause.clone(),
                        forced: false,
                    },
                );
                queue.push((consumer, cause));
            }
        }
    }

    /// Publishes an output's revision, retaining pending consumers until they verify their reads.
    pub(crate) fn publish(&mut self, id: DirtyKey, changed: bool) {
        if changed {
            self.change(id);
            let cause = self
                .pending
                .get(&id)
                .map(|p| p.cause.clone())
                .unwrap_or_else(|| {
                    Rc::new(Cause {
                        key: self.key(id).clone(),
                        parent: None,
                    })
                });
            self.propagate(id, cause);
        }
        self.pending.remove(&id);
    }

    pub(crate) fn complete(&mut self, ids: &[DirtyKey]) {
        for id in ids {
            self.pending.remove(id);
        }
    }

    /// Deterministic topological order over pending keys, without storage indexed by key-space size.
    pub(crate) fn schedule(&self, roots: Option<&[DirtyKey]>) -> Vec<DirtyKey> {
        let mut starts = match roots {
            Some(roots) => roots.to_vec(),
            None => self.pending.keys().copied().collect(),
        };
        starts.sort_unstable();
        let mut visited = HashSet::new();
        let mut stack = Vec::new();
        let mut order = Vec::new();
        for root in starts {
            stack.push((root, false));
            while let Some((key, finish)) = stack.pop() {
                if finish {
                    order.push(key);
                    continue;
                }
                if !self.pending.contains_key(&key) || !visited.insert(key) {
                    continue;
                }
                stack.push((key, true));
                for &dependency in self.entries[key.0].dependencies.iter().rev() {
                    stack.push((dependency, false));
                }
            }
        }
        order
    }

    pub(crate) fn explain_path(&self, key: DirtyKey) -> Option<Vec<ResourceKey>> {
        let mut cause = Some(self.pending.get(&key)?.cause.as_ref());
        let mut path = Vec::new();
        while let Some(step) = cause {
            path.push(step.key.clone());
            cause = step.parent.as_deref();
        }
        path.reverse();
        Some(path)
    }

    /// Validates the whole replacement before touching any output. Every output receives the
    /// same read set, so reaching any member of `from` from a proposed read proves a cycle.
    pub(crate) fn set_dependencies(
        &mut self,
        from: &[DirtyKey],
        to: &[DirtyKey],
    ) -> Result<(), (ResourceKey, ResourceKey)> {
        self.validate(from, to)?;
        for &key in from {
            let old = core::mem::take(&mut self.entries[key.0].dependencies);
            for dependency in old {
                self.entries[dependency.0]
                    .consumers
                    .retain(|&consumer| consumer != key);
                self.unused.push(dependency);
            }
            for &dependency in to {
                if !self.entries[key.0].dependencies.contains(&dependency) {
                    self.entries[key.0].dependencies.push(dependency);
                    self.entries[dependency.0].consumers.push(key);
                }
            }
            self.entries[key.0].dependencies.sort_unstable();
        }
        Ok(())
    }

    pub(crate) fn add_dependencies(
        &mut self,
        from: &[DirtyKey],
        to: DirtyKey,
    ) -> Result<(), (ResourceKey, ResourceKey)> {
        self.validate(from, &[to])?;
        for &key in from {
            if !self.entries[key.0].dependencies.contains(&to) {
                self.entries[key.0].dependencies.push(to);
                self.entries[key.0].dependencies.sort_unstable();
                self.entries[to.0].consumers.push(key);
            }
        }
        Ok(())
    }

    fn validate(
        &self,
        from: &[DirtyKey],
        to: &[DirtyKey],
    ) -> Result<(), (ResourceKey, ResourceKey)> {
        let mut visited = HashSet::new();
        let mut stack = Vec::new();
        for &dependency in to {
            stack.push(dependency);
            while let Some(key) = stack.pop() {
                if from.contains(&key) {
                    return Err((self.key(key).clone(), self.key(dependency).clone()));
                }
                if visited.insert(key) {
                    stack.extend(self.entries[key.0].dependencies.iter().copied());
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NodeId;
    #[test]
    fn failed_multi_output_replacement_restores_every_output() {
        let mut e = DirtyEngine::new();
        let old = e.intern(ResourceKey::input("old"));
        let a = e.intern(ResourceKey::node_output(
            NodeId::new(0),
            crate::OutputId::new(0),
        ));
        let b = e.intern(ResourceKey::node_output(
            NodeId::new(0),
            crate::OutputId::new(1),
        ));
        let c = e.intern(ResourceKey::node_output(
            NodeId::new(1),
            crate::OutputId::new(0),
        ));
        e.set_dependencies(&[a, b], &[old]).unwrap();
        e.set_dependencies(&[c], &[b]).unwrap();
        assert!(e.set_dependencies(&[a, b], &[c]).is_err());
        assert_eq!(e.entries[a.0].dependencies, [old]);
        assert_eq!(e.entries[b.0].dependencies, [old]);
    }
    #[test]
    fn unread_writes_have_no_pending_work() {
        let mut e = DirtyEngine::new();
        let key = e.intern(ResourceKey::input("unread"));
        e.mark_dirty(key);
        assert!(e.pending.is_empty());
        assert!(e.changed_since(key, 0));
    }
}
