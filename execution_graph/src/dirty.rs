// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Dependency storage with consumer-owned pending work and sparse authoring overrides.
//!
//! Invalidation walks existing consumers once. Scoped queries walk only pending dependencies
//! of their targets; unread host writes retain a revision, not a pending root. Revisions make
//! cutoff independent of drain boundaries. Cause links share prefixes across fanout.

use crate::ResourceKey;
use crate::key_arena::{KeyArena, KeyRange};
use alloc::borrow::Cow;
use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::vec::Vec;
use hashbrown::{HashMap, HashSet};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub(crate) struct DirtyKey(pub(crate) usize);

#[derive(Debug)]
struct Entry {
    key: ResourceKey,
    dependencies: KeyRange,
    consumers: KeyRange,
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
    edges: KeyArena,
    // Only rewired, previously published outputs need an overlay. Ordinary/cold graph storage
    // keeps its existing per-key representation; committed reads still own historical keys.
    prospective: BTreeMap<DirtyKey, KeyRange>,
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
            dependencies: KeyRange::default(),
            consumers: KeyRange::default(),
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

    pub(crate) fn stats(&self) -> (usize, usize, usize, usize, usize) {
        (
            self.ids.len(),
            self.entries.iter().map(|e| e.consumers.len()).sum(),
            self.pending.len(),
            self.entries.capacity(),
            self.edges.capacity(),
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
        let prospective = self.prospective.remove(&id).unwrap_or_default();
        for slot in 0..dependencies.len() {
            let dependency = self.edges.get(dependencies)[slot];
            self.edges
                .remove(&mut self.entries[dependency.0].consumers, id);
            self.unused.push(dependency);
        }
        for slot in 0..prospective.len() {
            let dependency = self.edges.get(prospective)[slot];
            if !self.edges.get(dependencies).contains(&dependency) {
                self.edges
                    .remove(&mut self.entries[dependency.0].consumers, id);
                self.unused.push(dependency);
            }
        }
        let readers = self.edges.get(consumers).to_vec();
        for &consumer in &readers {
            self.edges
                .remove(&mut self.entries[consumer.0].dependencies, id);
            if let Some(prospective) = self.prospective.get_mut(&consumer) {
                self.edges.remove(prospective, id);
            }
        }
        self.edges.release(dependencies);
        self.edges.release(prospective);
        self.edges.release(consumers);
        self.free.push(id);
        readers
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
    pub(crate) fn is_pending(&self, id: DirtyKey) -> bool {
        self.pending.contains_key(&id)
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
            for &consumer in self.edges.get(self.entries[key.0].consumers) {
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
        // A clean scoped query needs no traversal workspace, even while other roots remain
        // pending. Check the requested outputs before cloning roots or allocating the DFS stack.
        if self.pending.is_empty()
            || roots.is_some_and(|roots| roots.iter().all(|key| !self.pending.contains_key(key)))
        {
            return Vec::new();
        }
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
                for &dependency in self.effective_dependencies(key).iter().rev() {
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
        let to = Self::canonical_keys(to);
        self.validate(from, &to)?;
        for &key in from {
            self.replace_dependencies(key, &to, false);
        }
        Ok(())
    }

    /// Installs current authored inputs while retaining the last committed read set.
    pub(crate) fn set_prospective(
        &mut self,
        from: &[DirtyKey],
        to: &[DirtyKey],
    ) -> Result<(), (ResourceKey, ResourceKey)> {
        let to = Self::canonical_keys(to);
        self.validate(from, &to)?;
        for &key in from {
            self.replace_dependencies(key, &to, true);
        }
        Ok(())
    }

    fn effective_dependencies(&self, key: DirtyKey) -> &[DirtyKey] {
        let range = if self.prospective.is_empty() {
            self.entries[key.0].dependencies
        } else {
            self.prospective
                .get(&key)
                .copied()
                .unwrap_or(self.entries[key.0].dependencies)
        };
        self.edges.get(range)
    }

    fn canonical_keys(keys: &[DirtyKey]) -> Cow<'_, [DirtyKey]> {
        if keys.windows(2).all(|pair| pair[0] < pair[1]) {
            Cow::Borrowed(keys)
        } else {
            let mut keys = keys.to_vec();
            keys.sort_unstable();
            keys.dedup();
            Cow::Owned(keys)
        }
    }

    fn make_range(&mut self, keys: &[DirtyKey]) -> KeyRange {
        let mut range = KeyRange::default();
        for &key in keys {
            self.edges.push(&mut range, key);
        }
        range
    }

    fn replace_dependencies(&mut self, key: DirtyKey, to: &[DirtyKey], prospective: bool) {
        let committed = self.entries[key.0].dependencies;
        let previous = self.prospective.get(&key).copied().unwrap_or_default();
        // Reverse consumers represent the union. Historical keys cannot retire until either
        // publication replaces the committed reads or the node itself is removed.
        if !prospective {
            for slot in 0..committed.len() {
                let dependency = self.edges.get(committed)[slot];
                if to.binary_search(&dependency).is_err() {
                    self.edges
                        .remove(&mut self.entries[dependency.0].consumers, key);
                    self.unused.push(dependency);
                }
            }
        }
        for slot in 0..previous.len() {
            let dependency = self.edges.get(previous)[slot];
            if self
                .edges
                .get(committed)
                .binary_search(&dependency)
                .is_err()
                && to.binary_search(&dependency).is_err()
            {
                self.edges
                    .remove(&mut self.entries[dependency.0].consumers, key);
                self.unused.push(dependency);
            }
        }
        for &dependency in to {
            if self
                .edges
                .get(committed)
                .binary_search(&dependency)
                .is_err()
                && self.edges.get(previous).binary_search(&dependency).is_err()
            {
                self.edges
                    .push(&mut self.entries[dependency.0].consumers, key);
            }
        }
        if let Some(previous) = self.prospective.remove(&key) {
            self.edges.release(previous);
        }
        if prospective {
            let range = self.make_range(to);
            // An empty override is meaningful: do not reactivate historical prerequisites.
            self.prospective.insert(key, range);
        } else if self.edges.get(committed) != to {
            self.edges.release(committed);
            self.entries[key.0].dependencies = self.make_range(to);
        }
    }

    fn validate(
        &self,
        from: &[DirtyKey],
        to: &[DirtyKey],
    ) -> Result<(), (ResourceKey, ResourceKey)> {
        let mut visited = HashSet::new();
        let mut stack = Vec::new();
        for &dependency in to {
            // Removing edges cannot introduce a cycle. An edge already present on every
            // destination was checked when installed; only new effective edges need traversal.
            // This keeps a wide join from revisiting all earlier upstream chains on each edit.
            if from.iter().all(|&key| {
                self.effective_dependencies(key)
                    .binary_search(&dependency)
                    .is_ok()
            }) {
                continue;
            }
            stack.push(dependency);
            while let Some(key) = stack.pop() {
                if from.contains(&key) {
                    return Err((self.key(key).clone(), self.key(dependency).clone()));
                }
                if visited.insert(key) {
                    stack.extend(self.effective_dependencies(key).iter().copied());
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
        assert_eq!(e.edges.get(e.entries[a.0].dependencies), [old]);
        assert_eq!(e.edges.get(e.entries[b.0].dependencies), [old]);
    }
    #[test]
    fn rejected_prospective_replacement_keeps_all_outputs_and_reverse_edges() {
        let mut e = DirtyEngine::new();
        let old = e.intern(ResourceKey::input("old"));
        let next = e.intern(ResourceKey::input("next"));
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
        e.set_prospective(&[a, b], &[next, next]).unwrap();
        assert!(e.set_prospective(&[a, b], &[c]).is_err());
        for output in [a, b] {
            assert_eq!(e.effective_dependencies(output), [next]);
            assert_eq!(e.edges.get(e.entries[output.0].dependencies), [old]);
        }
        assert_eq!(e.edges.get(e.entries[old.0].consumers), [a, b]);
        assert_eq!(e.edges.get(e.entries[next.0].consumers), [a, b]);
        assert!(e.edges.get(e.entries[c.0].consumers).is_empty());
        e.set_dependencies(&[a, b], &[next]).unwrap();
        assert!(e.prospective.is_empty());
        assert!(e.edges.get(e.entries[old.0].consumers).is_empty());
        assert_eq!(e.edges.get(e.entries[next.0].consumers), [a, b]);
        e.collect_unused();
        assert_eq!(e.lookup(&ResourceKey::input("old")), None);
    }

    #[test]
    fn removing_prospective_producer_keeps_an_empty_override_until_publication() {
        let mut e = DirtyEngine::new();
        let old = e.intern(ResourceKey::input("old"));
        let a = e.intern(ResourceKey::node_output(
            NodeId::new(0),
            crate::OutputId::new(0),
        ));
        let b = e.intern(ResourceKey::node_output(
            NodeId::new(1),
            crate::OutputId::new(0),
        ));
        e.set_dependencies(&[a], &[old]).unwrap();
        e.set_prospective(&[a], &[b]).unwrap();
        assert_eq!(e.remove(b), [a]);
        assert!(e.effective_dependencies(a).is_empty());
        assert!(e.prospective.contains_key(&a));
        assert_eq!(e.edges.get(e.entries[a.0].dependencies), [old]);
        let recycled = e.intern(ResourceKey::input("recycled"));
        assert_eq!(recycled, b);
        assert!(e.edges.get(e.entries[recycled.0].consumers).is_empty());
        e.set_dependencies(&[a], &[]).unwrap();
        e.collect_unused();
        assert!(e.prospective.is_empty());
        assert_eq!(e.lookup(&ResourceKey::input("old")), None);
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
