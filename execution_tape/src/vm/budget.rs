// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! One execution's resource authority, independent of any suspended register stack.

use super::*;
use alloc::sync::Arc;

#[derive(Debug, Default)]
pub(super) struct Budget {
    /// Remaining instruction budget for the current run.
    pub(super) fuel: u64,
    /// Frames retained by all stacks in this execution, including suspended parents.
    pub(super) frames: usize,
    /// Number of host calls performed so far in the current run.
    pub(super) host_calls: u64,
    /// Successfully charged logical byte/string work and the current run's limit.
    pub(super) payload_work: u64,
    pub(super) payload_limit: u64,
    /// Successfully charged guest aggregate nodes/fields and the current run's limit.
    pub(super) aggregate_work: u64,
    pub(super) aggregate_limit: u64,

    pub(super) contract_scratch: ContractScratch,
    proof_owner: Option<Arc<()>>,
}

impl Budget {
    pub(super) fn new(limits: &Limits) -> Self {
        let mut budget = Self::default();
        budget.reset(limits);
        budget
    }

    pub(super) fn clear(&mut self) {
        self.fuel = 0;
        self.frames = 0;
        self.host_calls = 0;
        self.payload_work = 0;
        self.payload_limit = 0;
        self.aggregate_work = 0;
        self.aggregate_limit = 0;
        self.contract_scratch.reset();
        self.proof_owner = None;
    }

    pub(super) fn reset(&mut self, limits: &Limits) {
        self.clear();
        self.fuel = limits.fuel;
        self.payload_limit = limits.max_payload_work;
        self.aggregate_limit = limits.max_aggregate_work;
    }

    pub(super) fn select(&mut self, control: &ExecutionControl) {
        let owner = control.owner();
        if !self
            .proof_owner
            .as_ref()
            .is_some_and(|prior| Arc::ptr_eq(prior, owner))
        {
            // Contract IDs and aggregate projections are code-local proof keys. Work already
            // admitted survives a stack switch, but proofs cannot cross that identity boundary.
            self.contract_scratch.clear_proofs();
            self.proof_owner = Some(Arc::clone(owner));
        }
    }

    pub(super) fn charge_import(&mut self, work: DataWork) -> Result<(), Trap> {
        if work.aggregate > self.aggregate_limit - self.aggregate_work {
            return Err(Trap::AggregateWorkExceeded {
                limit: self.aggregate_limit,
                used: self.aggregate_work,
                requested: work.aggregate,
            });
        }
        if work.payload > self.payload_limit - self.payload_work {
            return Err(Trap::PayloadWorkExceeded {
                limit: self.payload_limit,
                used: self.payload_work,
                requested: work.payload,
            });
        }
        self.aggregate_work += work.aggregate;
        self.payload_work += work.payload;
        Ok(())
    }

    #[inline]
    pub(super) fn host_results(&self) -> crate::host::HostResults {
        crate::host::HostResults::new(
            DataWork {
                aggregate: self.aggregate_work,
                payload: self.payload_work,
            },
            DataWork {
                aggregate: self.aggregate_limit,
                payload: self.payload_limit,
            },
        )
    }

    pub(super) fn charge_payload_bytes(&mut self, len: usize, factor: u64) -> Result<(), Trap> {
        let requested = u64::try_from(len)
            .ok()
            .and_then(|len| len.checked_mul(factor))
            .ok_or(Trap::PayloadSizeOverflow)?;
        if requested > self.payload_limit - self.payload_work {
            return Err(Trap::PayloadWorkExceeded {
                limit: self.payload_limit,
                used: self.payload_work,
                requested,
            });
        }
        self.payload_work += requested;
        Ok(())
    }

    pub(super) fn charge_aggregate(&mut self, fields: usize) -> Result<(), Trap> {
        let requested = u64::try_from(fields)
            .ok()
            .and_then(|count| count.checked_add(1))
            .ok_or(Trap::AggregateSizeOverflow)?;
        if requested > self.aggregate_limit - self.aggregate_work {
            return Err(Trap::AggregateWorkExceeded {
                limit: self.aggregate_limit,
                used: self.aggregate_work,
                requested,
            });
        }
        self.aggregate_work += requested;
        Ok(())
    }
}
