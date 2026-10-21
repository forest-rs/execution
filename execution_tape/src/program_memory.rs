// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Program-scoped runtime memory.
//!
//! Owns aggregate storage for a VM or scheduler. VM runs append temporary nodes, then retain
//! only the new graph reachable from returned values. A trap discards all nodes from that run.
//! Existing retained nodes stay valid until the owner explicitly clears storage.

use crate::aggregates::{AggError, AggHeap, aggregate_handle};
use crate::value::Value;

/// Private boundary between retained nodes and the current run's allocations.
#[derive(Clone, Copy)]
pub(crate) struct Checkpoint(u32);

/// Shared runtime memory for program execution.
#[derive(Clone, Debug, Default)]
pub struct ProgramMemory {
    aggs: AggHeap,
}

impl ProgramMemory {
    /// Creates empty program memory.
    #[must_use]
    pub fn new() -> Self {
        Self {
            aggs: AggHeap::new(),
        }
    }

    /// Returns aggregate storage as a read-only view.
    #[must_use]
    pub fn aggs(&self) -> &AggHeap {
        &self.aggs
    }

    /// Returns mutable aggregate storage.
    pub fn aggs_mut(&mut self) -> &mut AggHeap {
        &mut self.aggs
    }

    pub(crate) fn checkpoint(&self) -> Checkpoint {
        Checkpoint(self.aggs.len_u32())
    }

    pub(crate) fn rollback(&mut self, checkpoint: Checkpoint) {
        self.aggs.truncate(checkpoint.0);
    }

    pub(crate) fn commit(
        &mut self,
        checkpoint: Checkpoint,
        roots: &mut [Value],
    ) -> Result<(), AggError> {
        // Retained nodes cannot refer to future allocations. Scalar and retained-root returns
        // therefore need no graph traversal, marking storage, or handle remapping.
        if !roots
            .iter()
            .filter_map(aggregate_handle)
            .any(|handle| handle.0 >= checkpoint.0)
        {
            self.rollback(checkpoint);
            return Ok(());
        }
        self.aggs.retain_since(checkpoint.0, roots)
    }
}
