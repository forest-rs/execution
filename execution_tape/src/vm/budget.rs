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
    /// Work admitted by all guest and host construction in this execution.
    pub(super) data: DataBudget,

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
        self.data = DataBudget::default();
        self.contract_scratch.reset();
        self.proof_owner = None;
    }

    pub(super) fn reset(&mut self, limits: &Limits) {
        self.clear();
        self.fuel = limits.fuel;
        self.data = DataBudget::new(DataWork {
            aggregate: limits.max_aggregate_work,
            payload: limits.max_payload_work,
        });
    }

    pub(super) fn select(&mut self, control: &ExecutionControl) {
        let Some(owner) = control.proof_owner() else {
            if self.proof_owner.take().is_some() {
                self.contract_scratch.clear_proofs();
            }
            return;
        };
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
        self.data.charge(work)
    }
}

/// Mutable resource authority lent to guest operations and host staging alike.
/// Staged results retain their own debit for observation, but never copy the remaining allowance.
#[derive(Debug, Default)]
pub(crate) struct DataBudget {
    pub(super) used: DataWork,
    pub(super) limits: DataWork,
}

impl DataBudget {
    pub(crate) fn new(limits: DataWork) -> Self {
        Self {
            used: DataWork::default(),
            limits,
        }
    }

    pub(crate) fn check(&self, work: DataWork) -> Result<(), Trap> {
        if work.aggregate > self.limits.aggregate - self.used.aggregate {
            return Err(Trap::AggregateWorkExceeded {
                limit: self.limits.aggregate,
                used: self.used.aggregate,
                requested: work.aggregate,
            });
        }
        if work.payload > self.limits.payload - self.used.payload {
            return Err(Trap::PayloadWorkExceeded {
                limit: self.limits.payload,
                used: self.used.payload,
                requested: work.payload,
            });
        }
        Ok(())
    }

    pub(crate) fn charge(&mut self, work: DataWork) -> Result<(), Trap> {
        self.check(work)?;
        self.used.aggregate += work.aggregate;
        self.used.payload += work.payload;
        Ok(())
    }

    pub(super) fn charge_payload_bytes(&mut self, len: usize, factor: u64) -> Result<(), Trap> {
        let requested = u64::try_from(len)
            .ok()
            .and_then(|len| len.checked_mul(factor))
            .ok_or(Trap::PayloadSizeOverflow)?;
        if requested > self.limits.payload - self.used.payload {
            return Err(Trap::PayloadWorkExceeded {
                limit: self.limits.payload,
                used: self.used.payload,
                requested,
            });
        }
        self.used.payload += requested;
        Ok(())
    }

    pub(super) fn charge_aggregate(&mut self, fields: usize) -> Result<(), Trap> {
        let requested = u64::try_from(fields)
            .ok()
            .and_then(|count| count.checked_add(1))
            .ok_or(Trap::AggregateSizeOverflow)?;
        if requested > self.limits.aggregate - self.used.aggregate {
            return Err(Trap::AggregateWorkExceeded {
                limit: self.limits.aggregate,
                used: self.used.aggregate,
                requested,
            });
        }
        self.used.aggregate += requested;
        Ok(())
    }
}
