// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! The common admission boundary for a host outcome, after its raw observation.

use super::*;
use crate::host::SigHash;
use crate::program::{HostSigId, SymbolId};
use crate::typed::VRegSlice;

/// An admitted host instruction waiting for the embedding, decoded once at admission.
///
/// Holds no borrows, so a request view keeps a copy beside its exclusive invocation borrow and
/// borrows code-owned slices on demand.
#[derive(Clone, Copy, Debug)]
pub(super) struct PendingHost {
    pub(super) location: TraceLocation,
    pub(super) span: Option<u64>,
    pub(super) import: HostSigId,
    pub(super) symbol: SymbolId,
    pub(super) signature: SigHash,
    pub(super) args: VRegSlice,
    pub(super) rets: VRegSlice,
    // Admitting guest frame. A poisoned callback can leave this parent operation suspended
    // below a new guest leaf; inspection must not assign the parent's location to that leaf.
    pub(super) depth: usize,
    pub(super) abandoned: bool,
    // A guest callback returned to this request, which is waiting again.
    pub(super) callback_returned: bool,
}

pub(super) struct HostReturnSite<'a> {
    pub(super) location: TraceLocation,
    pub(super) span: Option<u64>,
    pub(super) base: RegBase,
    pub(super) symbol: &'a str,
    pub(super) signature: SigHash,
    pub(super) expected: &'a [ValueType],
    pub(super) destinations: &'a [VReg],
}

/// Values whose staged aggregates an outcome adopts: results, or a raised payload.
///
/// A failed call adopts nothing.
pub(super) fn adopted_roots<'a>(
    values: &'a mut [Value],
    outcome: &'a mut Result<u64, HostError>,
) -> Option<&'a mut [Value]> {
    match outcome {
        Ok(_) => Some(values),
        Err(HostError::Throw(thrown)) => Some(&mut thrown.payload),
        Err(_) => None,
    }
}

/// The raw outcome as observers see it.
///
/// `unit` is the calling unit, which names a raised tag locally when it binds it.
pub(super) fn observed<'a>(
    units: &UnitTable,
    unit: UnitId,
    values: &'a [Value],
    outcome: &'a Result<u64, HostError>,
) -> HostCallOutcome<'a> {
    match outcome {
        Ok(extra_fuel) => HostCallOutcome::Returned {
            values,
            extra_fuel: *extra_fuel,
        },
        Err(HostError::Throw(thrown)) => HostCallOutcome::Threw {
            tag: thrown.tag,
            local: units.unit(unit).local_tag(thrown.tag),
            payload: &thrown.payload,
        },
        Err(error) => HostCallOutcome::Failed(error),
    }
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
        .charge_payload_bytes(bytes, 1)
        .map_err(|error| trap(ctx, error))?;
    for (&destination, value) in site.destinations.iter().zip(values) {
        let value = core::mem::replace(value, Value::Unit);
        ctx.write_owned_value(site.base, destination, value)
            .map_err(|error| trap(ctx, error))?;
    }
    Ok(())
}
