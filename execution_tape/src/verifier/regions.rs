// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Protected region tables: tree shape, instruction runs and catch clause targets.

use alloc::boxed::Box;

use super::VerifyError;
use super::metadata::MetadataTable;
use crate::bytecode::DecodedInstr;
use crate::program::{Function, Program, ValueType};

/// Why a protected region or one of its clauses is invalid.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RegionError {
    /// The parent is not an earlier region of the same function.
    Parent {
        /// Declared parent region.
        parent: u32,
    },
    /// A clause names a tag outside the program's tag table.
    ClauseTag {
        /// Clause index within the region.
        clause: u32,
        /// Requested tag id.
        tag: u32,
    },
    /// A clause target is not an instruction boundary of this function.
    ClauseTarget {
        /// Clause index within the region.
        clause: u32,
        /// Requested target offset.
        target: u32,
    },
    /// A clause target's block parameters do not receive exactly the clause's values.
    ///
    /// A tag clause's target declares one parameter per payload value; a catch-all target
    /// declares none. Reference clauses add one final `Exn` parameter and `on` clauses one
    /// final `Cont` parameter.
    ClauseParams {
        /// Clause index within the region.
        clause: u32,
        /// Caught tag id; `None` for catch-all.
        tag: Option<u32>,
        /// Caught tag name, when the clause names a tag.
        name: Option<Box<str>>,
        /// Values delivered by the clause.
        expected: u32,
        /// Parameters declared at the target.
        actual: u32,
    },
    /// A `catch_ref` or `catch_all_ref` target's last parameter is not an `Exn` register.
    ClauseException {
        /// Clause index within the region.
        clause: u32,
        /// The last parameter's declared kind.
        actual: ValueType,
    },
    /// An `on` target's last parameter is not a `Cont` register.
    ClauseContinuation {
        /// Clause index within the region.
        clause: u32,
        /// The last parameter's declared kind.
        actual: ValueType,
    },
    /// A clause target's parameter kind differs from the payload kind it receives.
    ClauseParamKind {
        /// Clause index within the region.
        clause: u32,
        /// Caught tag id.
        tag: u32,
        /// Caught tag name.
        name: Box<str>,
        /// Zero-based payload position.
        index: u32,
        /// Payload kind declared by the tag.
        expected: ValueType,
        /// Parameter register's declared kind.
        actual: ValueType,
    },
}

impl core::fmt::Display for RegionError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Parent { parent } => {
                write!(f, "parent region#{parent} is not an earlier region")
            }
            Self::ClauseTag { clause, tag } => {
                write!(
                    f,
                    "clause {clause} catches tag#{tag}, which is out of bounds"
                )
            }
            Self::ClauseTarget { clause, target } => write!(
                f,
                "clause {clause} target {target} is not an instruction boundary"
            ),
            Self::ClauseParams {
                clause,
                tag,
                name,
                expected,
                actual,
            } => {
                write!(f, "clause {clause} (")?;
                match (tag, name) {
                    (Some(tag), Some(name)) => write!(f, "tag#{tag} \"{name}\"")?,
                    (Some(tag), None) => write!(f, "tag#{tag}")?,
                    _ => f.write_str("catch_all")?,
                }
                write!(
                    f,
                    ") delivers {expected} values; its target declares {actual} parameters"
                )
            }
            Self::ClauseException { clause, actual } => write!(
                f,
                "clause {clause} binds the exception to a parameter declared {actual:?}, not Exn"
            ),
            Self::ClauseContinuation { clause, actual } => write!(
                f,
                "clause {clause} binds the continuation to a parameter declared {actual:?}, not Cont"
            ),
            Self::ClauseParamKind {
                clause,
                tag,
                name,
                index,
                expected,
                actual,
            } => write!(
                f,
                "clause {clause} (tag#{tag} \"{name}\") payload[{index}] is {expected:?}; its target parameter declares {actual:?}"
            ),
        }
    }
}

/// Why a region run is invalid.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RegionRunError {
    /// The run names a region outside the function's region table.
    UnknownRegion {
        /// Requested region.
        region: u32,
    },
    /// The run is empty, extends past the bytecode, or does not start and end at instruction
    /// boundaries.
    Range,
    /// The run starts before the previous run ends; runs must be sorted and disjoint.
    Order,
}

impl core::fmt::Display for RegionRunError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnknownRegion { region } => write!(f, "region#{region} does not exist"),
            Self::Range => f.write_str("range is empty or not on instruction boundaries"),
            Self::Order => f.write_str("overlaps or precedes the previous run"),
        }
    }
}

fn boundary(decoded: &[DecodedInstr], pc: u32) -> bool {
    decoded.binary_search_by_key(&pc, |di| di.offset).is_ok()
}

/// Validates every region, clause and run of one function, including unreachable ones.
pub(super) fn verify(
    program: &Program,
    func: u32,
    function: &Function,
    decoded: &[DecodedInstr],
    byte_len: u32,
) -> Result<(), VerifyError> {
    let malformed = |error| VerifyError::InvalidMetadata {
        table: MetadataTable::Regions,
        entry: func,
        error,
    };
    let regions = function.regions(program).map_err(malformed)?;
    let runs = function.region_runs(program).map_err(malformed)?;
    let invalid = |region: usize, error| VerifyError::InvalidRegion {
        func,
        region: u32::try_from(region).unwrap_or(u32::MAX),
        error,
    };
    for (index, region) in regions.iter().enumerate() {
        if let Some(parent) = region.parent
            && parent.0 as usize >= index
        {
            return Err(invalid(index, RegionError::Parent { parent: parent.0 }));
        }
        let clauses = program.region_clauses(region).map_err(malformed)?;
        for (clause, &catch) in clauses.iter().enumerate() {
            let clause = u32::try_from(clause).unwrap_or(u32::MAX);
            let payload = match catch.tag() {
                Some(tag) => {
                    let entry = program.tag(tag).ok_or_else(|| {
                        invalid(index, RegionError::ClauseTag { clause, tag: tag.0 })
                    })?;
                    Some((
                        tag,
                        program.tag_payload(entry).expect("verified tag payload"),
                    ))
                }
                None => None,
            };
            let target = catch.target();
            if target >= byte_len || !boundary(decoded, target) {
                return Err(invalid(index, RegionError::ClauseTarget { clause, target }));
            }
            let params = super::blocks::declaration(decoded, target).unwrap_or(&[]);
            let expected = payload.as_ref().map_or(0, |(_, payload)| payload.len())
                + usize::from(catch.binds_exception())
                + usize::from(catch.handles_suspension());
            if params.len() != expected {
                return Err(invalid(
                    index,
                    RegionError::ClauseParams {
                        clause,
                        tag: catch.tag().map(|tag| tag.0),
                        name: catch
                            .tag()
                            .and_then(|tag| program.tag_name(tag))
                            .map(Into::into),
                        expected: u32::try_from(expected).unwrap_or(u32::MAX),
                        actual: u32::try_from(params.len()).unwrap_or(u32::MAX),
                    },
                ));
            }
            if let Some((tag, payload)) = payload {
                for (position, (&param, &kind)) in params.iter().zip(payload.iter()).enumerate() {
                    let actual = function.register_type(program, param).map_err(malformed)?;
                    if actual != kind {
                        return Err(invalid(
                            index,
                            RegionError::ClauseParamKind {
                                clause,
                                tag: tag.0,
                                name: program.tag_name(tag).unwrap_or_default().into(),
                                index: u32::try_from(position).unwrap_or(u32::MAX),
                                expected: kind,
                                actual,
                            },
                        ));
                    }
                }
            }
            if catch.binds_exception() {
                let last = *params.last().expect("checked parameter count");
                let actual = function.register_type(program, last).map_err(malformed)?;
                if actual != ValueType::Exn {
                    return Err(invalid(
                        index,
                        RegionError::ClauseException { clause, actual },
                    ));
                }
            }
            if catch.handles_suspension() {
                let last = *params.last().expect("checked parameter count");
                let actual = function.register_type(program, last).map_err(malformed)?;
                if actual != ValueType::Cont {
                    return Err(invalid(
                        index,
                        RegionError::ClauseContinuation { clause, actual },
                    ));
                }
            }
        }
    }
    let mut previous_end = 0;
    for (index, run) in runs.iter().enumerate() {
        let run_error = |error| VerifyError::InvalidRegionRun {
            func,
            run: u32::try_from(index).unwrap_or(u32::MAX),
            error,
        };
        if run.region.0 as usize >= regions.len() {
            return Err(run_error(RegionRunError::UnknownRegion {
                region: run.region.0,
            }));
        }
        if run.start_pc >= run.end_pc
            || run.end_pc > byte_len
            || !boundary(decoded, run.start_pc)
            || (run.end_pc != byte_len && !boundary(decoded, run.end_pc))
        {
            return Err(run_error(RegionRunError::Range));
        }
        if index != 0 && run.start_pc < previous_end {
            return Err(run_error(RegionRunError::Order));
        }
        previous_end = run.end_pc;
    }
    Ok(())
}
