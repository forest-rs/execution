// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Exception tag declarations.
//!
//! A tag is a nominal identity: equal payload contracts never make two tags the same exception.
//! Names are unique so that diagnostics, disassembly and tooling can identify a tag without
//! reinterpreting its payload signature.

use alloc::collections::BTreeMap;

use super::VerifyError;
use super::metadata::MetadataTable;
use crate::format::DecodeError;
use crate::program::Program;

fn malformed(tag: usize, error: DecodeError) -> VerifyError {
    VerifyError::InvalidMetadata {
        table: MetadataTable::Tags,
        entry: u32::try_from(tag).unwrap_or(u32::MAX),
        error,
    }
}

/// Validates every tag, including tags no instruction throws.
pub(super) fn verify(program: &Program) -> Result<(), VerifyError> {
    let mut names: BTreeMap<&str, u32> = BTreeMap::new();
    for (index, entry) in program.tags.iter().enumerate() {
        let tag = u32::try_from(index).map_err(|_| malformed(index, DecodeError::OutOfBounds))?;
        let name = program
            .symbol_str(entry.name)
            .map_err(|error| malformed(index, error))?;
        let contracts = program
            .tag_payload_contracts(entry)
            .map_err(|error| malformed(index, error))?;
        if contracts
            .iter()
            .any(|&contract| program.contracts.get(contract).is_none())
        {
            return Err(malformed(index, DecodeError::OutOfBounds));
        }
        if let Some(&first) = names.get(name) {
            return Err(VerifyError::DuplicateTagName {
                tag,
                first,
                name: name.into(),
            });
        }
        names.insert(name, tag);
    }
    Ok(())
}
