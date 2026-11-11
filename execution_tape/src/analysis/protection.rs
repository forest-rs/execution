// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Which catch clauses each raising instruction can transfer to.
//!
//! This assumes a function whose region tables have already been validated.

extern crate alloc;

use alloc::vec::Vec;

use crate::bytecode::{DecodedInstr, Instr};
use crate::program::{CatchClause, Function, Program, RegionId, RegionRun, TagId};

/// One catch clause reachable from a raising instruction.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct Handler {
    /// Function-local region declaring the clause.
    pub(crate) region: RegionId,
    /// Clause index within its region.
    pub(crate) clause: u32,
    /// The clause itself.
    pub(crate) catch: CatchClause,
}

/// Exceptional successors for every decoded instruction of one function.
#[derive(Debug, Default)]
pub(crate) struct Protection {
    handlers: Vec<Vec<Handler>>,
}

/// What an instruction can raise, if anything.
enum Raises {
    /// A callee, host or rethrown reference may raise any tag.
    Any,
    /// `throw` raises exactly this tag.
    Tag(TagId),
}

fn raises(instr: &Instr) -> Option<Raises> {
    match instr {
        Instr::Call { .. }
        | Instr::CallIndirect { .. }
        | Instr::HostCall { .. }
        | Instr::ThrowRef { .. } => Some(Raises::Any),
        Instr::Throw { tag, .. } => Some(Raises::Tag(TagId(*tag))),
        _ => None,
    }
}

/// Returns the innermost region covering `pc`, using sorted, non-overlapping runs.
pub(crate) fn innermost(runs: &[RegionRun], pc: u32) -> Option<RegionId> {
    let index = runs
        .partition_point(|run| run.start_pc <= pc)
        .checked_sub(1)?;
    let run = runs[index];
    (pc < run.end_pc).then_some(run.region)
}

impl Protection {
    /// Computes clause chains for `function`, whose regions must already be valid.
    pub(crate) fn new(program: &Program, function: &Function, decoded: &[DecodedInstr]) -> Self {
        let runs = function.region_runs(program).expect("verified region runs");
        if runs.is_empty() {
            return Self::default();
        }
        let regions = function.regions(program).expect("verified regions");
        let handlers = decoded
            .iter()
            .map(|di| {
                let mut out = Vec::new();
                let Some(raised) = raises(&di.instr) else {
                    return out;
                };
                let mut region = innermost(runs, di.offset);
                while let Some(id) = region {
                    let entry = &regions[id.0 as usize];
                    let clauses = program.region_clauses(entry).expect("verified clauses");
                    for (index, &catch) in clauses.iter().enumerate() {
                        let handler = Handler {
                            region: id,
                            clause: u32::try_from(index).expect("verified clause count"),
                            catch,
                        };
                        match raised {
                            Raises::Tag(tag) if catch.catches(tag) => {
                                out.push(handler);
                                return out;
                            }
                            Raises::Tag(_) => {}
                            Raises::Any => {
                                out.push(handler);
                                if catch.tag().is_none() {
                                    return out;
                                }
                            }
                        }
                    }
                    region = entry.parent;
                }
                out
            })
            .collect();
        Self { handlers }
    }

    /// Clauses that can receive an exception raised by decoded instruction `index`, in order.
    pub(crate) fn handlers(&self, index: usize) -> &[Handler] {
        self.handlers.get(index).map_or(&[], Vec::as_slice)
    }
}
