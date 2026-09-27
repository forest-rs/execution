// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Dense node storage with monotonic public identities and reclaimable physical slots.

use crate::graph::Node;
use crate::{Executor, NodeId};
use alloc::vec::Vec;
use core::ops::{Index, IndexMut};
use hashbrown::HashMap;

#[derive(Debug)]
pub(crate) struct Nodes<X: Executor> {
    values: Vec<Node<X>>,
    slots: HashMap<usize, usize>,
    next: usize,
}

impl<X: Executor> Nodes<X> {
    pub(crate) fn new() -> Self {
        Self {
            values: Vec::new(),
            slots: HashMap::new(),
            next: 0,
        }
    }
    pub(crate) fn len(&self) -> usize {
        self.values.len()
    }
    pub(crate) fn capacity(&self) -> usize {
        self.values.capacity()
    }
    pub(crate) fn next_id(&self) -> NodeId {
        NodeId::new(u64::try_from(self.next).expect("node identity exhausted"))
    }
    pub(crate) fn push(&mut self, node: Node<X>) {
        self.slots.insert(self.next, self.values.len());
        self.next = self.next.checked_add(1).expect("node identity exhausted");
        self.values.push(node);
    }
    pub(crate) fn get(&self, id: usize) -> Option<&Node<X>> {
        self.slots.get(&id).map(|&slot| &self.values[slot])
    }
    pub(crate) fn get_mut(&mut self, id: usize) -> Option<&mut Node<X>> {
        let slot = *self.slots.get(&id)?;
        self.values.get_mut(slot)
    }
    pub(crate) fn iter(&self) -> impl Iterator<Item = &Node<X>> {
        self.values.iter()
    }
    pub(crate) fn remove(&mut self, id: usize) -> Option<Node<X>> {
        let slot = self.slots.remove(&id)?;
        let node = self.values.swap_remove(slot);
        if let Some(moved) = self.values.get(slot) {
            self.slots
                .insert(usize::try_from(moved.id.as_u64()).unwrap(), slot);
        }
        Some(node)
    }
}

impl<X: Executor> Index<usize> for Nodes<X> {
    type Output = Node<X>;
    fn index(&self, id: usize) -> &Self::Output {
        self.get(id).expect("validated node id")
    }
}
impl<X: Executor> IndexMut<usize> for Nodes<X> {
    fn index_mut(&mut self, id: usize) -> &mut Self::Output {
        self.get_mut(id).expect("validated node id")
    }
}

impl<X: Executor> crate::node_access::OutputReader<X::Value> for Nodes<X> {
    fn output(
        &self,
        node: NodeId,
        output: crate::OutputId,
    ) -> Result<crate::node_access::OutputView<'_, X::Value>, crate::OutputReadError> {
        use crate::OutputReadError;
        let n = usize::try_from(node.as_u64())
            .ok()
            .and_then(|id| self.get(id))
            .ok_or(OutputReadError::MissingNode { node })?;
        let key = *n
            .output_ids
            .get(output.index() as usize)
            .ok_or(OutputReadError::MissingOutput { node, output })?;
        Ok(crate::node_access::OutputView {
            key,
            outputs: &n.output_ids,
            value: n.outputs.get_by_id(output),
        })
    }
}
