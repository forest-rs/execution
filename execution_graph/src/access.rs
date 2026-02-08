// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Dependency keys and access logging for incremental execution.

use alloc::boxed::Box;
use alloc::vec::Vec;

/// Identifier for a node within an `ExecutionGraph`.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct NodeId(u64);

impl NodeId {
    /// Creates a new node id.
    #[inline]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw integer backing this id.
    #[inline]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

/// Identifier for a host operation.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct HostOpId(u64);

impl HostOpId {
    /// Creates a new host operation id.
    #[inline]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw integer backing this id.
    #[inline]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

/// An owned resource key used to model dependencies for incremental execution.
///
/// ## Relationship to `execution_tape`
///
/// Host calls in `execution_tape` can record accesses via [`execution_tape::host::AccessSink`],
/// using borrowed keys ([`execution_tape::host::ResourceKeyRef`]).
///
/// `execution_graph` converts those borrowed keys into this owned [`ResourceKey`] so it can store
/// them in an [`AccessLog`] and in dirty-tracking structures. This type also includes
/// [`ResourceKey::TapeOutput`], which is graph-local and has no direct `execution_tape` analog.
///
/// ## Read/write matching
///
/// Incremental systems treat keys as equal by simple structural equality. That means the
/// *producer* of keys is responsible for consistency:
/// - If a later mutation should invalidate a prior dependency, use the same key (same variant +
///   same payload values).
/// - If you choose a `u64` hash key, collisions are “aliasing”: unrelated resources can spuriously
///   invalidate each other (conservative but may be costly). Prefer stable, collision-resistant
///   hashing or interning when it matters.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum ResourceKey {
    /// An external input by name.
    ///
    /// This is for embedder-defined “semantic inputs”: values that are supplied *from outside* the
    /// VM/host boundary (configuration, environment, request parameters, etc.). The string is an
    /// embedder-chosen stable name.
    Input(Box<str>),
    /// A dependency on another node's output.
    ///
    /// This identifies a node output within a single [`ExecutionGraph`](crate::ExecutionGraph)
    /// instance. It is namespaced by the producing [`NodeId`] plus an output name. This key is
    /// intended for wiring graph edges (downstream nodes reading upstream outputs).
    ///
    /// Note: [`NodeId`] values are graph-local identities; they are not intended to be stable
    /// across reconstructing the graph.
    TapeOutput {
        /// The node that produced the output.
        node: NodeId,
        /// The output name within the node.
        output: Box<str>,
    },
    /// Host state consulted by an operation, with a key namespace local to the host op.
    ///
    /// This is the main “precise” form for host-managed state. It is explicitly namespaced by the
    /// host operation id ([`HostOpId`]), so different host ops can reuse the same numeric `key`
    /// without colliding. The `key: u64` should identify *which* piece of state was
    /// consulted/mutated for that operation (often a stable hash of a structured key, or an intern
    /// id managed by the embedder).
    HostState {
        /// The host operation that consulted state.
        op: HostOpId,
        /// Opaque per-op key identifying the consulted state.
        key: u64,
    },
    /// Conservative dependency for opaque host operations.
    ///
    /// This is a conservative escape hatch for operations that depend on (or mutate) host state
    /// but cannot (or choose not to) produce a more precise key. Use it when the best you can say
    /// is “this call depends on *something* behind op X”.
    ///
    /// The intended pattern is:
    /// - record [`Access::Read`] of [`ResourceKey::OpaqueHost`] for calls whose outputs depend on
    ///   opaque host state
    /// - record [`Access::Write`] of [`ResourceKey::OpaqueHost`] for calls that may invalidate
    ///   that opaque state
    ///
    /// This is always safe (it may cause extra re-runs), and it provides a predictable stepping
    /// stone until a host op can be keyed more precisely.
    OpaqueHost(HostOpId),
}

impl ResourceKey {
    /// Constructs an [`ResourceKey::Input`] key.
    #[inline]
    pub fn input(name: impl Into<Box<str>>) -> Self {
        Self::Input(name.into())
    }

    /// Constructs an [`ResourceKey::TapeOutput`] key.
    #[inline]
    pub fn tape_output(node: NodeId, output: impl Into<Box<str>>) -> Self {
        Self::TapeOutput {
            node,
            output: output.into(),
        }
    }

    /// Constructs an [`ResourceKey::HostState`] key.
    #[inline]
    pub const fn host_state(op: HostOpId, key: u64) -> Self {
        Self::HostState { op, key }
    }

    /// Constructs an [`ResourceKey::OpaqueHost`] key.
    #[inline]
    pub const fn opaque_host(op: HostOpId) -> Self {
        Self::OpaqueHost(op)
    }
}

/// An access to a [`ResourceKey`] during execution.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum Access {
    /// Execution read from a resource (dependency edge).
    Read(ResourceKey),
    /// Execution wrote to a resource (invalidation source).
    Write(ResourceKey),
}

impl Access {
    /// Constructs an [`Access::Read`] access.
    #[inline]
    pub fn read(key: ResourceKey) -> Self {
        Self::Read(key)
    }

    /// Constructs an [`Access::Write`] access.
    #[inline]
    pub fn write(key: ResourceKey) -> Self {
        Self::Write(key)
    }
}

/// Append-only log of accesses captured during a run.
#[derive(Clone, Debug, Default)]
pub struct AccessLog {
    accesses: Vec<Access>,
}

impl AccessLog {
    /// Creates an empty access log.
    #[inline]
    pub const fn new() -> Self {
        Self {
            accesses: Vec::new(),
        }
    }

    /// Appends an access entry.
    #[inline]
    pub fn push(&mut self, access: Access) {
        self.accesses.push(access);
    }

    /// Records a read of `key`.
    #[inline]
    pub fn read(&mut self, key: ResourceKey) {
        self.push(Access::Read(key));
    }

    /// Records a write of `key`.
    #[inline]
    pub fn write(&mut self, key: ResourceKey) {
        self.push(Access::Write(key));
    }

    /// Returns the number of recorded accesses.
    #[inline]
    pub fn len(&self) -> usize {
        self.accesses.len()
    }

    /// Returns `true` if the log contains no accesses.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.accesses.is_empty()
    }

    /// Returns all accesses in order.
    #[inline]
    pub fn as_slice(&self) -> &[Access] {
        &self.accesses
    }

    /// Returns an iterator over recorded accesses in order.
    #[inline]
    pub fn iter(&self) -> core::slice::Iter<'_, Access> {
        self.accesses.iter()
    }

    /// Consumes the log and returns the underlying access vector.
    #[inline]
    pub fn into_vec(self) -> Vec<Access> {
        self.accesses
    }
}

impl IntoIterator for AccessLog {
    type Item = Access;
    type IntoIter = alloc::vec::IntoIter<Access>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.accesses.into_iter()
    }
}

impl<'a> IntoIterator for &'a AccessLog {
    type Item = &'a Access;
    type IntoIter = core::slice::Iter<'a, Access>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use core::hash::{Hash, Hasher};
    use std::collections::hash_map::DefaultHasher;

    #[test]
    fn access_log_preserves_append_order() {
        let mut log = AccessLog::new();
        log.read(ResourceKey::input("in"));
        log.write(ResourceKey::host_state(HostOpId::new(7), 42));

        assert_eq!(
            log.as_slice(),
            &[
                Access::Read(ResourceKey::input("in")),
                Access::Write(ResourceKey::host_state(HostOpId::new(7), 42)),
            ]
        );
    }

    #[test]
    fn resource_keys_hash_and_eq() {
        fn hash<T: Hash>(value: &T) -> u64 {
            let mut hasher = DefaultHasher::new();
            value.hash(&mut hasher);
            hasher.finish()
        }

        let a = ResourceKey::tape_output(NodeId::new(1), "out");
        let b = ResourceKey::tape_output(NodeId::new(1), "out");
        let c = ResourceKey::tape_output(NodeId::new(2), "out");

        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(hash(&a), hash(&b));
    }
}
