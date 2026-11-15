// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The common admission boundary for a host outcome, after its raw observation.

use super::*;

/// Identity of one admitted host request, stable across drive turns and guest callbacks.
///
/// Equal function/PC locations do not identify the same request. Retain this token with an
/// external job and present it when reacquiring completion authority; delayed replies must
/// not complete a later admission. It retains identity rather than the VM or request.
#[derive(Clone)]
pub struct HostRequestId {
    pub(super) owner: alloc::sync::Arc<()>,
    pub(super) ordinal: u64,
}
impl PartialEq for HostRequestId {
    fn eq(&self, other: &Self) -> bool {
        self.ordinal == other.ordinal && alloc::sync::Arc::ptr_eq(&self.owner, &other.owner)
    }
}
impl Eq for HostRequestId {}
impl core::hash::Hash for HostRequestId {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        core::hash::Hash::hash(&alloc::sync::Arc::as_ptr(&self.owner), state);
        core::hash::Hash::hash(&self.ordinal, state);
    }
}
impl fmt::Debug for HostRequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostRequestId")
            .field("owner", &alloc::sync::Arc::as_ptr(&self.owner))
            .field("admission", &self.ordinal)
            .finish()
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct PendingHost {
    pub(super) location: TraceLocation,
    pub(super) request: u64,
    pub(super) instruction: usize,
    // Admitting guest frame. A poisoned callback can leave this parent operation suspended
    // below a new guest leaf; inspection must not assign the parent's location to that leaf.
    pub(super) depth: usize,
    pub(super) abandoned: bool,
}

pub(super) struct HostReturnSite<'a> {
    pub(super) location: TraceLocation,
    pub(super) span: Option<u64>,
    pub(super) base: RegBase,
    pub(super) symbol: &'a str,
    pub(super) signature: crate::host::SigHash,
    pub(super) expected: &'a [ValueType],
    pub(super) destinations: &'a [VReg],
}

#[inline(always)]
pub(super) fn admit_return(
    ctx: &mut Registers,
    budget: &mut Budget,
    site: HostReturnSite<'_>,
    values: &mut [Value],
    outcome: Result<u64, HostError>,
) -> Result<(), TrapInfo> {
    let trap =
        |ctx: &Registers, trap| ctx.trap(site.location.func, site.location.pc, site.span, trap);
    let fuel = outcome.map_err(|error| {
        trap(
            ctx,
            Trap::HostCallFailed {
                symbol: Box::from(site.symbol),
                sig_hash: site.signature,
                error,
            },
        )
    })?;
    budget.fuel = budget.fuel.saturating_sub(fuel);
    // Ordinary hosts fill a VM-sized slice; external deliveries supply an owned vector.
    if values.len() != site.expected.len() {
        return Err(trap(
            ctx,
            Trap::HostReturnArityMismatch {
                expected: u32::try_from(site.expected.len()).expect("verified ABI arity"),
                actual: u32::try_from(values.len()).unwrap_or(u32::MAX),
            },
        ));
    }
    for (value, &expected) in values.iter().zip(site.expected) {
        value
            .check_type(expected)
            .map_err(|error| trap(ctx, error))?;
    }
    let bytes = values
        .iter()
        .try_fold(0_usize, |total, value| {
            let bytes = match value {
                Value::Bytes(bytes) => bytes.len(),
                Value::Str(text) => text.len(),
                _ => 0,
            };
            total.checked_add(bytes).ok_or(Trap::PayloadSizeOverflow)
        })
        .map_err(|error| trap(ctx, error))?;
    budget
        .data
        .charge_payload_bytes(bytes, 1)
        .map_err(|error| trap(ctx, error))?;
    for (&destination, value) in site.destinations.iter().zip(values) {
        let value = core::mem::replace(value, Value::Unit);
        ctx.write_owned_value(site.base, destination, value)
            .map_err(|error| trap(ctx, error))?;
    }
    Ok(())
}
