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
use crate::contract::ContractKinds;
use crate::format::DecodeError;
use crate::program::{LinkId, Program};

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
        // Contract IDs are normalized within a program, so equal signatures have equal IDs. A
        // malformed target signature is the function's error, which its own checks report.
        let (Ok(args), Ok(rets)) = (
            program.function_contracts(target.arg_types),
            program.function_contracts(target.ret_types),
        ) else {
            continue;
        };
        let same = program.call_sig_arg_contracts(signature).ok() == Some(args)
            && program.call_sig_ret_contracts(signature).ok() == Some(rets);
        if !same {
            return Err(VerifyError::LinkSignatureMismatch {
                link: LinkId(link),
                call_sig: entry.call_sig,
                target: entry.target,
            });
        }
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
