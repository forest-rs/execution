// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Exception tag declarations and the static obligations of `throw`.
//!
//! A tag is a nominal identity: equal payload contracts never make two tags the same exception.
//! Names are unique so that diagnostics, disassembly and tooling can identify a tag without
//! reinterpreting its payload signature.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;

use super::VerifyError;
use super::metadata::MetadataTable;
use crate::format::DecodeError;
use crate::program::{Program, RegisterTypes, TagId};

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
        let payload = program
            .tag_payload_contracts(entry)
            .map_err(|error| malformed(index, error))?;
        let results = program
            .tag_result_contracts(entry)
            .map_err(|error| malformed(index, error))?;
        if payload
            .iter()
            .chain(results)
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
    // Imports name existing tags, each once, in increasing order.
    let mut previous = None;
    for (index, &tag) in program.tag_imports.iter().enumerate() {
        if tag.0 as usize >= program.tags.len() || previous.is_some_and(|last| tag.0 <= last) {
            return Err(VerifyError::InvalidMetadata {
                table: MetadataTable::TagImports,
                entry: u32::try_from(index).unwrap_or(u32::MAX),
                error: DecodeError::OutOfBounds,
            });
        }
        previous = Some(tag.0);
    }
    Ok(())
}

fn name(program: &Program, tag: TagId) -> Box<str> {
    program.tag_name(tag).unwrap_or_default().into()
}

/// Checks the payload count of a `throw` whose tag is already known to exist.
pub(super) fn check_throw_arity(
    program: &Program,
    func: u32,
    pc: u32,
    tag: TagId,
    supplied: usize,
) -> Result<(), VerifyError> {
    let entry = program
        .tag(tag)
        .expect("validated by verify_id_operands_in_bounds");
    let expected = entry.payload.len;
    if u32::try_from(supplied).ok() != Some(expected) {
        return Err(VerifyError::ThrowArityMismatch {
            func,
            pc,
            tag: tag.0,
            name: name(program, tag),
            expected,
            actual: u32::try_from(supplied).unwrap_or(u32::MAX),
        });
    }
    Ok(())
}

/// Checks that each payload register declares its tag's payload storage kind.
pub(super) fn check_throw_payload(
    program: &Program,
    func: u32,
    pc: u32,
    tag: TagId,
    args: &[u32],
    types: RegisterTypes<'_>,
) -> Result<(), VerifyError> {
    check_throw_arity(program, func, pc, tag, args.len())?;
    let entry = program
        .tag(tag)
        .expect("validated by verify_id_operands_in_bounds");
    let payload = program.tag_payload(entry).expect("validated tag payload");
    for (index, (&reg, &expected)) in args.iter().zip(payload.iter()).enumerate() {
        let actual = types
            .get(reg)
            .ok_or(VerifyError::RegOutOfBounds { func, pc, reg })?;
        if actual != expected {
            return Err(VerifyError::ThrowPayloadMismatch {
                func,
                pc,
                tag: tag.0,
                name: name(program, tag),
                index: u32::try_from(index).expect("verified payload arity"),
                reg,
                expected,
                actual,
            });
        }
    }
    Ok(())
}
