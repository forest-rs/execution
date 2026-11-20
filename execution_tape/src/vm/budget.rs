// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! One execution's resource authority, independent of any suspended register stack.

use super::*;

#[derive(Debug, Default)]
pub(super) struct Budget {
    /// Remaining instruction budget for the current run.
    pub(super) fuel: u64,
    /// Frames retained by all stacks in this execution, including suspended parents.
    pub(super) frames: usize,
    /// How many frames this execution may hold: the smaller of its call-depth limit and what
    /// the VM's frame limit leaves after saved computations.
    frame_limit: usize,
    /// The call-depth limit, which decides which trap refuses a frame.
    call_depth: usize,
    /// What a call past the call-depth limit throws, if anything.
    depth_exception: Option<CallDepthException>,
    /// Whether the depth exception was thrown and its headroom is in use: until a catch lands
    /// under the call-depth limit, calls may go `headroom` deeper and throw nothing.
    depth_headroom: bool,
    /// The VM's frame limit and the frames saved computations hold.
    max_frames: usize,
    resident_elsewhere: usize,
    /// Number of host calls performed so far in the current run, and how many it may make.
    pub(super) host_calls: u64,
    pub(super) host_call_limit: u64,
    /// The contract work the current run may do.
    pub(super) contract_limit: u64,
    /// Successfully charged logical byte/string work and the current run's limit.
    pub(super) payload_work: u64,
    pub(super) payload_limit: u64,
    /// Successfully charged guest aggregate nodes/fields and the current run's limit.
    pub(super) aggregate_work: u64,
    pub(super) aggregate_limit: u64,

    pub(super) contract_scratch: ContractScratch,
    /// The stack whose contract proofs `contract_scratch` holds.
    proof_owner: Option<u64>,
}

impl Budget {
    #[cfg(test)]
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
        self.host_call_limit = limits.max_host_calls;
        self.contract_limit = limits.max_contract_work;
        self.call_depth = limits.max_call_depth;
        self.depth_exception = limits.call_depth_exception;
        self.depth_headroom = false;
        self.max_frames = limits.max_frames;
        self.limit_frames(0);
        self.payload_limit = limits.max_payload_work;
        self.aggregate_limit = limits.max_aggregate_work;
    }

    /// Accounts for `resident` frames saved computations hold under the VM's frame limit.
    pub(super) fn limit_frames(&mut self, resident: usize) {
        self.resident_elsewhere = resident;
        self.frame_limit = self
            .depth_limit()
            .min(self.max_frames.saturating_sub(resident));
    }

    /// The frames this computation may hold now: the call-depth limit, and the depth
    /// exception's headroom while it is in use.
    fn depth_limit(&self) -> usize {
        match self.depth_exception {
            Some(exception) if self.depth_headroom => {
                self.call_depth.saturating_add(exception.headroom)
            }
            _ => self.call_depth,
        }
    }

    /// Admits one more frame, or names the limit that refuses it.
    #[inline]
    pub(super) fn admit_frame(&self) -> Result<(), Trap> {
        if self.frames < self.frame_limit {
            return Ok(());
        }
        Err(if self.frames >= self.depth_limit() {
            Trap::CallDepthExceeded
        } else {
            Trap::FrameLimitExceeded {
                limit: self.max_frames,
                resident: self.frames + self.resident_elsewhere,
            }
        })
    }

    /// The tag a guest call refused with `trap` throws instead, when `trap` is the call-depth
    /// limit's and the depth exception is not in use; the computation then has its headroom.
    #[cold]
    pub(super) fn throw_for_depth(&mut self, trap: &Trap) -> Option<TagRef> {
        let exception = self.depth_exception?;
        if self.depth_headroom || !matches!(trap, Trap::CallDepthExceeded) {
            return None;
        }
        self.depth_headroom = true;
        self.limit_frames(self.resident_elsewhere);
        Some(exception.tag)
    }

    /// Takes the depth exception's headroom back once a catch of `tag` has unwound the
    /// computation under the call-depth limit, so the next call past it throws again.
    ///
    /// A catch of the depth exception itself keeps the headroom unless the catcher has the
    /// whole headroom below the limit: the handlers it runs may need all of it, and a catcher
    /// a frame or two under the limit, below the frames that made the refused call, would
    /// otherwise leave them almost none and throw again inside them.
    pub(super) fn caught(&mut self, tag: TagRef) {
        let Some(exception) = self.depth_exception.filter(|_| self.depth_headroom) else {
            return;
        };
        let rearm = if tag == exception.tag {
            self.frames.saturating_add(exception.headroom) <= self.call_depth
        } else {
            self.frames < self.call_depth
        };
        if rearm {
            self.depth_headroom = false;
            self.limit_frames(self.resident_elsewhere);
        }
    }

    pub(super) fn select(&mut self, control: &ExecutionControl) {
        let owner = control.owner();
        if self.proof_owner != Some(owner) {
            // Contract IDs and aggregate projections are code-local proof keys. Work already
            // admitted survives a stack switch, but proofs cannot cross that identity boundary.
            self.contract_scratch.clear_proofs();
            self.proof_owner = Some(owner);
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
