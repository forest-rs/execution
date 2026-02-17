// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Program-scoped runtime memory.
//!
//! `ProgramMemory` is the shared memory base used by graph/runtime schedulers.
//! In the staged execute/commit model:
//! - execute reads from `&ProgramMemory` (read-only),
//! - commit is the only phase that mutates `ProgramMemory`.
//!
//! v1 scope is aggregate storage only.

use crate::aggregates::AggHeap;

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

    /// Returns aggregate storage as a read-only view for execute-time access.
    #[must_use]
    pub fn aggs(&self) -> &AggHeap {
        &self.aggs
    }

    /// Returns mutable aggregate storage for commit-time updates.
    pub fn aggs_mut(&mut self) -> &mut AggHeap {
        &mut self.aggs
    }
}
