// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Closure-backed executor for native Rust node bodies.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use core::marker::PhantomData;

use crate::executor::Executor;
use crate::node_access::NodeAccess;

/// Body signature for an [`FnNode`].
///
/// The closure receives the bound input values, an empty output buffer to fill with exactly one
/// value per declared output, and the [`NodeAccess`] for reporting any reads or writes of state
/// outside the graph.
pub type NodeFn<V, E> = dyn FnMut(&[V], &mut Vec<V>, &mut NodeAccess<'_>) -> Result<(), E>;

/// A node body implemented by a Rust closure.
pub struct FnNode<V, E> {
    name: Option<Box<str>>,
    run: Box<NodeFn<V, E>>,
}

impl<V, E> FnNode<V, E> {
    /// Wraps `run` as an anonymous node body.
    pub fn new(
        run: impl FnMut(&[V], &mut Vec<V>, &mut NodeAccess<'_>) -> Result<(), E> + 'static,
    ) -> Self {
        Self {
            name: None,
            run: Box::new(run),
        }
    }

    /// Wraps `run` as a node body described by `name` in reports and DOT output.
    pub fn named(
        name: impl Into<Box<str>>,
        run: impl FnMut(&[V], &mut Vec<V>, &mut NodeAccess<'_>) -> Result<(), E> + 'static,
    ) -> Self {
        Self {
            name: Some(name.into()),
            run: Box::new(run),
        }
    }

    /// Returns the advisory name, if one was given.
    #[must_use]
    #[inline]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }
}

impl<V, E> fmt::Debug for FnNode<V, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FnNode")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// Value comparison used by [`FnExecutor::with_value_eq`].
pub type ValueEq<V> = fn(&V, &V) -> bool;

/// Executor whose nodes are [`FnNode`] closures over value type `V` failing with `E`.
///
/// This executor holds no state of its own. Closures that consult external state capture it and
/// must report the reads through the [`NodeAccess`] they are given.
///
/// Early cutoff is off by default. [`with_value_eq`](Self::with_value_eq) turns it on with a
/// comparison that decides when a re-run output is unchanged.
pub struct FnExecutor<V, E> {
    value_eq: Option<ValueEq<V>>,
    _marker: PhantomData<fn() -> (V, E)>,
}

impl<V, E> FnExecutor<V, E> {
    /// Creates the executor, without early cutoff.
    #[must_use]
    #[inline]
    pub const fn new() -> Self {
        Self {
            value_eq: None,
            _marker: PhantomData,
        }
    }

    /// Enables early cutoff: a re-run output for which `eq(previous, next)` holds counts as
    /// unchanged, so dependents scheduled only because of it are skipped.
    ///
    /// `eq` must only return `true` when no reader can tell the two values apart; see
    /// [`Executor::values_equal`].
    ///
    /// ```
    /// use execution_graph::{ExecutionGraph, FnExecutor};
    ///
    /// let graph = ExecutionGraph::new(FnExecutor::<i64, ()>::new().with_value_eq(|a, b| a == b));
    /// # let _ = graph;
    /// ```
    #[must_use]
    #[inline]
    pub const fn with_value_eq(mut self, eq: ValueEq<V>) -> Self {
        self.value_eq = Some(eq);
        self
    }
}

impl<V, E> Default for FnExecutor<V, E> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<V, E> fmt::Debug for FnExecutor<V, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FnExecutor")
            .field("early_cutoff", &self.value_eq.is_some())
            .finish()
    }
}

impl<V: Clone + fmt::Debug, E: fmt::Debug> Executor for FnExecutor<V, E> {
    type Value = V;
    type Node = FnNode<V, E>;
    type Error = E;

    #[inline]
    fn execute(
        &mut self,
        node: &mut Self::Node,
        inputs: &[Self::Value],
        outputs: &mut Vec<Self::Value>,
        access: &mut NodeAccess<'_>,
    ) -> Result<(), Self::Error> {
        (node.run)(inputs, outputs, access)
    }

    #[inline]
    fn values_equal(&self, previous: &Self::Value, next: &Self::Value) -> bool {
        self.value_eq.is_some_and(|eq| eq(previous, next))
    }

    fn describe(&self, node: &Self::Node) -> Option<String> {
        node.name.as_deref().map(String::from)
    }
}
