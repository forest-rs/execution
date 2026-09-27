// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Positional ports and indexed output storage.

use alloc::boxed::Box;

/// Positional input slot within a node, assigned in declaration order.
///
/// This is not an external input key. Two nodes can use the same slot number with unrelated
/// external resources. Pair it with a `NodeId` when addressing an input.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct InputId(u32);

impl InputId {
    /// Constructs a positional input ID. Operations validate it against the target node.
    pub const fn new(index: u32) -> Self {
        Self(index)
    }
    /// Returns the position in the node's declared inputs.
    pub const fn index(self) -> u32 {
        self.0
    }
}

/// Positional output slot within a node, assigned in declaration order.
///
/// Pair it with its producing `NodeId`. A removed node's identity is never reused, so an old
/// `(NodeId, OutputId)` cannot resolve to a replacement computation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct OutputId(u32);

impl OutputId {
    /// Constructs a positional output ID. Operations validate it against the producer.
    pub const fn new(index: u32) -> Self {
        Self(index)
    }
    /// Returns the position in the node's declared outputs.
    pub const fn index(self) -> u32 {
        self.0
    }
}

/// Last successfully published outputs, stored in declaration order.
///
/// Names support construction and inspection. Execution uses [`Self::get_by_id`] without string
/// lookup. Values may be cached from an earlier run; request the node before requiring freshness.
#[derive(Clone, Debug)]
pub struct NodeOutputs<V> {
    pub(crate) names: Box<[Box<str>]>,
    pub(crate) values: Box<[V]>,
}

impl<V> NodeOutputs<V> {
    /// Looks up a published value by its declared name.
    pub fn get(&self, name: &str) -> Option<&V> {
        self.names
            .iter()
            .position(|candidate| candidate.as_ref() == name)
            .and_then(|slot| self.values.get(slot))
    }
    /// Looks up a published value by position without comparing names.
    pub fn get_by_id(&self, output: OutputId) -> Option<&V> {
        self.values.get(output.0 as usize)
    }
    /// Number of published values. Zero before the first successful execution.
    pub fn len(&self) -> usize {
        self.values.len()
    }
    /// Whether this node has no published values.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
    /// Published names and values in declaration order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &V)> {
        self.names.iter().map(Box::as_ref).zip(&self.values)
    }
}
