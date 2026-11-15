// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Durable control has no observer lease. Storage is traced and relocated as one domain.

use super::*;
use crate::{aggregates::AggRemap, arena::PayloadPlan, control::arena::Resumptions};
use invocation::delimited::Saved;

pub(super) struct Chain<'a> {
    pub(super) continuation: continuation::Continuation<'a>,
    pub(super) parents: Vec<continuation::Continuation<'a>>,
}
impl Chain<'_> {
    pub(super) fn into_owned(self) -> Chain<'static> {
        Chain {
            continuation: self.continuation.into_owned(),
            parents: self
                .parents
                .into_iter()
                .map(continuation::Continuation::into_owned)
                .collect(),
        }
    }
}

#[derive(Default)]
pub(super) struct Domain {
    pub(super) paused: Option<Chain<'static>>,
    pub(super) saved: Resumptions<Saved>,
    pub(super) next_segment: u64,
    pub(super) next_epoch: u64,
    pub(super) frames: usize,
}

pub(super) struct Plan {
    stacks: Vec<StackPlan>,
}
struct StackPlan {
    payloads: PayloadPlan,
}

fn add_values(values: &[Value], roots: &mut Vec<Value>) {
    roots.extend(
        values
            .iter()
            .filter_map(crate::aggregates::aggregate_handle)
            .map(Value::Agg),
    );
}

impl Domain {
    pub(super) fn has_control(&self) -> bool {
        self.paused.is_some() || self.saved.len() != 0
    }

    pub(super) fn plan_collection(
        &self,
        heap: &AggHeap,
        roots: &mut Vec<Value>,
    ) -> Result<Plan, AggError> {
        let mut stacks = Vec::new();
        if let Some(chain) = &self.paused {
            for stack in chain
                .parents
                .iter()
                .chain(core::iter::once(&chain.continuation))
            {
                stacks.push(StackPlan::new(stack, heap, roots)?);
            }
        }
        for (_, saved) in self.saved.iter() {
            match saved {
                Saved::New { callable, .. } => add_values(core::slice::from_ref(callable), roots),
                Saved::Suspended(chain) => {
                    for stack in chain {
                        stacks.push(StackPlan::new(stack, heap, roots)?);
                    }
                }
            }
        }
        Ok(Plan { stacks })
    }

    pub(super) fn apply_collection(&mut self, plan: Plan, remap: &AggRemap) {
        let mut plans = plan.stacks.into_iter();
        if let Some(chain) = &mut self.paused {
            for stack in chain
                .parents
                .iter_mut()
                .chain(core::iter::once(&mut chain.continuation))
            {
                plans.next().expect("planned stack").apply(stack, remap);
            }
        }
        for saved in self.saved.values_mut() {
            match saved {
                Saved::New { callable, .. } => remap
                    .remap_values_in_place(core::slice::from_mut(callable))
                    .expect("planned callable"),
                Saved::Suspended(chain) => {
                    for stack in chain {
                        plans
                            .next()
                            .expect("planned saved stack")
                            .apply(stack, remap);
                    }
                }
            }
        }
        let unconsumed = plans.next();
        debug_assert!(
            unconsumed.is_none(),
            "all plans applied to their owning stacks"
        );
    }
}

impl StackPlan {
    fn new(
        stack: &continuation::Continuation<'static>,
        heap: &AggHeap,
        roots: &mut Vec<Value>,
    ) -> Result<Self, AggError> {
        if stack
            .host_results
            .as_ref()
            .is_some_and(|results| results.delta.is_some())
        {
            return Err(AggError::UnresolvedStagedHandle);
        }
        add_values(&stack.args, roots);
        add_values(&stack.outputs, roots);
        if let Some(values) = &stack.control.callback_results {
            add_values(values, roots);
        }
        roots.extend(
            stack
                .ctx
                .aggs
                .iter()
                .copied()
                .filter(|handle| *handle != Registers::UNINITIALIZED_AGG)
                .map(Value::Agg),
        );
        roots.extend(
            stack
                .ctx
                .closures
                .iter()
                .flatten()
                .map(|closure| Value::Agg(closure.env)),
        );
        if let Some(delimiter) = &stack.delimiter {
            delimiter.add_roots(roots);
        }
        let payloads = stack.ctx.arena.plan_collection(
            stack
                .ctx
                .bytes
                .iter()
                .copied()
                .filter(|handle| *handle != BytesHandle::UNINITIALIZED),
            stack
                .ctx
                .strs
                .iter()
                .copied()
                .filter(|handle| *handle != StrHandle::UNINITIALIZED),
            ValueSources::new(stack.program.program(), &stack.args, heap),
            roots,
        )?;
        Ok(Self { payloads })
    }

    fn apply(self, stack: &mut continuation::Continuation<'static>, remap: &AggRemap) {
        remap
            .remap_values_in_place(stack.args.to_mut())
            .expect("planned inputs");
        remap
            .remap_values_in_place(&mut stack.outputs)
            .expect("planned outputs");
        if let Some(values) = &mut stack.control.callback_results {
            remap
                .remap_values_in_place(values)
                .expect("planned pending results");
        }
        for handle in &mut stack.ctx.aggs {
            if *handle != Registers::UNINITIALIZED_AGG {
                *handle = remap.remap_handle(*handle).expect("planned register");
            }
        }
        for closure in stack.ctx.closures.iter_mut().flatten() {
            closure.env = remap.remap_handle(closure.env).expect("planned closure");
        }
        if let Some(delimiter) = &mut stack.delimiter {
            delimiter.remap(remap);
        }
        for handle in &mut stack.ctx.bytes {
            if *handle != BytesHandle::UNINITIALIZED {
                *handle = self.payloads.bytes(*handle);
            }
        }
        for handle in &mut stack.ctx.strs {
            if *handle != StrHandle::UNINITIALIZED {
                *handle = self.payloads.str(*handle);
            }
        }
        stack.ctx.arena.apply_collection(self.payloads, remap);
    }
}

#[cfg(test)]
mod tests;
