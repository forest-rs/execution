// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Link declarations: relinkable call targets and the signature that keeps them sound.
//!
//! `call.link` sites are checked against their link's call signature alone. A link's initial
//! target must have exactly that signature, and the VM accepts a new target only when its
//! canonical signature is the link's, so repointing a link never requires verifying its callers
//! again.

use alloc::collections::BTreeMap;

use super::VerifyError;
use super::metadata::MetadataTable;
use crate::contract::{Contract, ContractId, ContractKinds};
use crate::format::DecodeError;
use crate::program::{LinkGuardEntry, LinkId, MAX_GUARD_KEYS, Pc, Program, ValueType};
use crate::value::FuncId;

fn malformed(link: usize) -> VerifyError {
    VerifyError::InvalidMetadata {
        table: MetadataTable::Links,
        entry: u32::try_from(link).unwrap_or(u32::MAX),
        error: DecodeError::OutOfBounds,
    }
}

/// Validates every link, including links no instruction calls.
///
/// Call signatures must already be verified. A target with malformed signature ranges is left
/// to the function checks, which name the function.
pub(super) fn verify(program: &Program) -> Result<(), VerifyError> {
    let mut names: BTreeMap<&str, u32> = BTreeMap::new();
    for (index, entry) in program.links.iter().enumerate() {
        let link = u32::try_from(index).map_err(|_| malformed(index))?;
        let name = program
            .symbol_str(entry.name)
            .map_err(|_| malformed(index))?;
        if let Some(&first) = names.get(name) {
            return Err(VerifyError::DuplicateLinkName {
                link: LinkId(link),
                first: LinkId(first),
                name: name.into(),
            });
        }
        names.insert(name, link);
        let signature = program.call_sig(entry.call_sig).ok_or(malformed(index))?;
        let target = program
            .functions
            .get(entry.target.0 as usize)
            .ok_or(malformed(index))?;
        let link_args = program.call_sig_arg_contracts(signature).ok();
        let link_rets = program.call_sig_ret_contracts(signature).ok();
        if let Some(guard) = entry.guard {
            verify_guard(program, LinkId(link), guard, link_args, link_rets)?;
        }
        // Contract IDs are normalized within a program, so equal signatures have equal IDs. A
        // malformed target signature is the function's error, which its own checks report.
        let (Ok(args), Ok(rets)) = (
            program.function_contracts(target.arg_types),
            program.function_contracts(target.ret_types),
        ) else {
            continue;
        };
        if link_args != Some(args) || link_rets != Some(rets) {
            return Err(VerifyError::LinkSignatureMismatch {
                link: LinkId(link),
                call_sig: entry.call_sig,
                target: entry.target,
            });
        }
    }
    Ok(())
}

/// Checks a guarded link's key count and that its fallback takes a plain `u64` link index and
/// keys before the link's arguments, with the same contracts, and returns its results.
fn verify_guard(
    program: &Program,
    link: LinkId,
    guard: LinkGuardEntry,
    link_args: Option<&[ContractId]>,
    link_rets: Option<&[ContractId]>,
) -> Result<(), VerifyError> {
    let keys = guard.key_count as usize;
    let fallback = program
        .functions
        .get(guard.fallback.0 as usize)
        .filter(|_| (1..=MAX_GUARD_KEYS).contains(&keys))
        .ok_or(VerifyError::LinkGuardInvalid { link })?;
    let mismatch = VerifyError::LinkFallbackMismatch {
        link,
        fallback: guard.fallback,
    };
    let (Ok(args), Ok(rets), Some(link_args), Some(link_rets)) = (
        program.function_contracts(fallback.arg_types),
        program.function_contracts(fallback.ret_types),
        link_args,
        link_rets,
    ) else {
        return Err(mismatch);
    };
    let Some((prefix, rest)) = args.split_at_checked(1 + keys) else {
        return Err(mismatch);
    };
    let plain_u64 =
        |&id: &ContractId| program.contracts.get(id) == Some(Contract::Kind(ValueType::U64));
    if !prefix.iter().all(plain_u64) || rest != link_args || rets != link_rets {
        return Err(mismatch);
    }
    Ok(())
}

/// Checks that a call passes as many guard keys as `link` compares: none for an unguarded link.
pub(super) fn check_keys(
    program: &Program,
    func: FuncId,
    pc: u32,
    link: LinkId,
    actual: usize,
) -> Result<(), VerifyError> {
    let expected = program
        .link(link)
        .and_then(|entry| entry.guard)
        .map_or(0, |guard| guard.key_count as usize);
    if expected != actual {
        return Err(VerifyError::LinkGuardKeys {
            func,
            pc: Pc(pc),
            link,
            expected,
            actual,
        });
    }
    Ok(())
}

/// The argument and result kinds of a verified link.
pub(super) fn signature(program: &Program, link: LinkId) -> (ContractKinds<'_>, ContractKinds<'_>) {
    let entry = program.link(link).expect("verified link");
    let signature = program.call_sig(entry.call_sig).expect("verified link");
    (
        program
            .call_sig_args(signature)
            .expect("verified call signature"),
        program
            .call_sig_rets(signature)
            .expect("verified call signature"),
    )
}
