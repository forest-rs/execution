// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Raising typed exceptions.
//!
//! `throw` materializes its payload, checks the tag's payload contracts and then unwinds. No
//! protected region exists yet, so every exception escapes the invocation and is reported as
//! [`Trap::Uncaught`]. The payload is detached into an owned [`ValueSnapshot`] before the failed
//! invocation rolls back its aggregates, so the embedding can still inspect it.

use super::*;
use crate::aggregates::ValueSnapshot;
use crate::program::TagId;

/// A guest exception that no protected region caught before it left the invocation.
///
/// The payload is an independent snapshot: aggregate handles refer to
/// [`ValueSnapshot::aggregates`], not to the VM heap, which discards the failed invocation's
/// allocations. Host objects remain host-owned handles; function IDs still refer to the
/// throwing program.
#[derive(Clone, Debug)]
pub struct UncaughtException {
    tag: TagId,
    name: Box<str>,
    payload: ValueSnapshot,
}

impl UncaughtException {
    /// Program-local identity of the thrown tag.
    #[must_use]
    pub fn local_tag(&self) -> TagId {
        self.tag
    }

    /// The thrown tag's declared name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Payload values in tag order, with their reachable aggregate graph.
    #[must_use]
    pub fn payload(&self) -> &ValueSnapshot {
        &self.payload
    }
}

impl fmt::Display for UncaughtException {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "uncaught exception tag#{} (\"{}\") with {} payload values",
            self.tag.0,
            self.name,
            self.payload.values().len()
        )
    }
}

impl<H: Host> Vm<H> {
    /// Raises `tag` at the throwing instruction and returns the trap that ends the invocation.
    ///
    /// Payload materialization, observation and contract checks happen at the throw site, so a
    /// rejected payload reports its own trap there rather than an uncaught exception.
    #[allow(
        clippy::too_many_arguments,
        reason = "mirrors the interpreter state borrowed by other instruction helpers"
    )]
    pub(super) fn raise<P: TracePolicy>(
        &self,
        ctx: &mut Registers,
        budget: &mut Budget,
        program: &Program,
        tag: TagId,
        payload: &[VReg],
        base: RegBase,
        sources: ValueSources<'_>,
        trace: &mut TraceCtx<'_>,
        location: TraceLocation,
        span_id: Option<u64>,
    ) -> TrapInfo {
        let fail = |ctx: &Registers, trap| ctx.trap(location.func, location.pc, span_id, trap);
        let mut values = Vec::with_capacity(payload.len());
        for &register in payload {
            match ctx.materialize_vreg(budget, base, register, sources) {
                Ok(value) => values.push(value),
                Err(trap) => return fail(ctx, trap),
            }
        }
        P::values(trace, program, || ValueEvent::Thrown {
            location,
            tag,
            values: &values,
            aggregates: self.memory.aggs(),
        });
        let entry = program.tag(tag).expect("verified tag");
        let contracts = program
            .tag_payload_contracts(entry)
            .expect("verified tag payload");
        for (index, (value, &contract)) in values.iter().zip(contracts).enumerate() {
            if matches!(program.contracts.get(contract), Some(Contract::Kind(_))) {
                continue;
            }
            if let Err(error) = budget.contract_scratch.check_value(
                program,
                self.memory.aggs(),
                value,
                contract,
                self.limits.max_contract_work,
            ) {
                let position = ContractPosition::Payload {
                    tag,
                    index: u32::try_from(index).expect("verified payload arity"),
                };
                return fail(ctx, self.contract_trap(location.func, position, error));
            }
        }
        let payload = match self.memory.aggs().snapshot(&values) {
            Ok(payload) => payload,
            Err(error) => return fail(ctx, Trap::AggError(error)),
        };
        let name = program.tag_name(tag).unwrap_or_default().into();
        fail(
            ctx,
            Trap::Uncaught(Box::new(UncaughtException { tag, name, payload })),
        )
    }
}
