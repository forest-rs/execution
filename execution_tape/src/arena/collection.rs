// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Prepare every index/source before moving storage. Applying a valid plan is infallible.

use super::*;
use crate::aggregates::{AggError, AggRemap};
use alloc::vec;

pub(crate) struct PayloadPlan {
    bytes: Vec<u32>,
    strs: Vec<u32>,
}

fn indices(length: usize, handles: impl Iterator<Item = u32>) -> Result<Vec<u32>, AggError> {
    let mut mapping = vec![u32::MAX; length];
    for handle in handles {
        *mapping
            .get_mut(handle as usize)
            .ok_or(AggError::BadHandle)? = 0;
    }
    let mut next = 0;
    for index in &mut mapping {
        if *index != u32::MAX {
            *index = next;
            next += 1; // The arena reserves MAX before insertion.
        }
    }
    Ok(mapping)
}

impl PayloadPlan {
    pub(crate) fn bytes(&self, handle: BytesHandle) -> BytesHandle {
        BytesHandle(self.bytes[handle.0 as usize])
    }
    pub(crate) fn str(&self, handle: StrHandle) -> StrHandle {
        StrHandle(self.strs[handle.0 as usize])
    }
}

impl ValueArena {
    /// Inputs contain initialized bank cells only; an explicit MAX handle is an error here.
    pub(crate) fn plan_collection(
        &self,
        bytes: impl Iterator<Item = BytesHandle>,
        strs: impl Iterator<Item = StrHandle>,
        sources: ValueSources<'_>,
        roots: &mut Vec<Value>,
    ) -> Result<PayloadPlan, AggError> {
        let bytes = indices(self.bytes.len(), bytes.map(|handle| handle.0))?;
        let strs = indices(self.strs.len(), strs.map(|handle| handle.0))?;
        for (index, &mapping) in bytes.iter().enumerate() {
            if mapping != u32::MAX {
                self.bytes(
                    BytesHandle(u32::try_from(index).expect("payload insertion bounds indices")),
                    sources,
                )
                .ok_or(AggError::BadHandle)?;
                add_source(&self.bytes[index], roots);
            }
        }
        for (index, &mapping) in strs.iter().enumerate() {
            if mapping != u32::MAX {
                self.str(
                    StrHandle(u32::try_from(index).expect("payload insertion bounds indices")),
                    sources,
                )
                .ok_or(AggError::BadHandle)?;
                add_source(&self.strs[index], roots);
            }
        }
        Ok(PayloadPlan { bytes, strs })
    }

    pub(crate) fn apply_collection(&mut self, plan: PayloadPlan, aggregates: &AggRemap) {
        compact(&mut self.bytes, plan.bytes, aggregates);
        compact(&mut self.strs, plan.strs, aggregates);
    }
}

fn add_source<T>(value: &Stored<T>, roots: &mut Vec<Value>) {
    if let Stored::Source(PayloadSource::Aggregate { handle, .. }) = value {
        roots.push(Value::Agg(*handle));
    }
}

fn compact<T>(values: &mut Vec<Stored<T>>, mapping: Vec<u32>, aggregates: &AggRemap) {
    let mut next = 0;
    for (index, mapped) in mapping.into_iter().enumerate() {
        if mapped != u32::MAX {
            debug_assert_eq!(
                mapped as usize, next,
                "live descriptors compact in stable index order"
            );
            if let Stored::Source(PayloadSource::Aggregate { handle, .. }) = &mut values[index] {
                *handle = aggregates
                    .remap_handle(*handle)
                    .expect("planned payload source is rooted");
            }
            values.swap(index, next);
            next += 1;
        }
    }
    values.truncate(next);
}
