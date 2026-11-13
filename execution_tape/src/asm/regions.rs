// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Protected regions authored as a stack of open scopes over emitted instructions.
//!
//! Regions attribute instructions, not byte layouts: a frontend can close a region, emit
//! unrelated code, and resume the region later. The assembler records which region is innermost
//! at each instruction boundary and produces sorted runs when the body is finished.

use super::*;
use crate::program::CatchClause;

/// A handler clause whose target is an assembler label.
///
/// The target should be a block reserved with [`Asm::block`]. A tag clause's block declares one
/// parameter per payload value, in payload order; a catch-all block declares none. The `*Ref`
/// clauses append one `Exn` parameter receiving the exception, for [`Asm::throw_ref`].
///
/// [`Self::On`] handles suspensions of a continuation that a protected [`Asm::resume`] runs; its
/// block declares the tag's payload parameters and then one `Cont` parameter.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Catch {
    /// Catches exactly `tag`, binding its payload to the target block's parameters.
    Tag(TagId, Label),
    /// Catches every exception and binds nothing.
    All(Label),
    /// Catches exactly `tag`, binding its payload and then the exception reference.
    TagRef(TagId, Label),
    /// Catches every exception, binding only the exception reference.
    AllRef(Label),
    /// Handles a suspension to `tag`, binding its payload and then the suspended continuation.
    On(TagId, Label),
}

#[derive(Clone, Debug, Default)]
pub(super) struct Regions {
    declared: Vec<(Option<RegionId>, Vec<Catch>)>,
    open: Vec<RegionId>,
    marks: Vec<(u32, Option<RegionId>)>,
}

impl Regions {
    fn mark(&mut self, pc: u32) {
        let innermost = self.open.last().copied();
        if let Some((last_pc, last)) = self.marks.last_mut()
            && *last_pc == pc
        {
            *last = innermost;
            return;
        }
        if self
            .marks
            .last()
            .map_or(innermost.is_some(), |&(_, last)| last != innermost)
        {
            self.marks.push((pc, innermost));
        }
    }

    pub(super) fn finish(
        self,
        labels: &[Option<u32>],
        byte_len: usize,
    ) -> Result<(Vec<RegionDef>, Vec<RegionRun>), UnresolvedLabel> {
        let resolve = |label: Label| labels.get(label.0 as usize).copied().flatten();
        let mut regions = Vec::with_capacity(self.declared.len());
        for (parent, catches) in self.declared {
            let mut clauses = Vec::with_capacity(catches.len());
            for catch in catches {
                clauses.push(match catch {
                    Catch::Tag(tag, label) => CatchClause::Tag {
                        tag,
                        target: resolve(label).ok_or(UnresolvedLabel)?,
                    },
                    Catch::All(label) => CatchClause::All {
                        target: resolve(label).ok_or(UnresolvedLabel)?,
                    },
                    Catch::TagRef(tag, label) => CatchClause::TagRef {
                        tag,
                        target: resolve(label).ok_or(UnresolvedLabel)?,
                    },
                    Catch::AllRef(label) => CatchClause::AllRef {
                        target: resolve(label).ok_or(UnresolvedLabel)?,
                    },
                    Catch::On(tag, label) => CatchClause::On {
                        tag,
                        target: resolve(label).ok_or(UnresolvedLabel)?,
                    },
                });
            }
            regions.push(RegionDef { parent, clauses });
        }
        let end = u32::try_from(byte_len).unwrap_or(u32::MAX);
        let mut runs = Vec::new();
        for (index, &(start_pc, region)) in self.marks.iter().enumerate() {
            let end_pc = self.marks.get(index + 1).map_or(end, |&(pc, _)| pc);
            let Some(region) = region else { continue };
            if start_pc >= end_pc {
                continue;
            }
            match runs.last_mut() {
                Some(RegionRun {
                    end_pc: previous_end,
                    region: previous,
                    ..
                }) if *previous_end == start_pc && *previous == region => {
                    *previous_end = end_pc;
                }
                _ => runs.push(RegionRun {
                    start_pc,
                    end_pc,
                    region,
                }),
            }
        }
        Ok((regions, runs))
    }
}

impl Asm {
    /// Opens a protected region nested in the currently innermost one.
    ///
    /// Instructions emitted until the matching [`Self::end_region`] belong to the new region.
    /// An exception raised there tries `clauses` in order, then the parent region's clauses,
    /// then the calling activation. Only `call`, `call.indirect`, `host_call`, `throw`,
    /// `throw_ref` and `resume` can raise; traps are never caught.
    /// A suspension from a continuation run by a `resume` in the region tries the `on` clauses
    /// the same way, then the `resume` that runs this activation's own continuation.
    pub fn begin_region(&mut self, clauses: &[Catch]) -> RegionId {
        let id = RegionId(u32::try_from(self.regions.declared.len()).unwrap_or(u32::MAX));
        let parent = self.regions.open.last().copied();
        self.regions.declared.push((parent, clauses.to_vec()));
        self.resume_region(id);
        id
    }

    /// Reopens an existing region, so out-of-line code can belong to it.
    ///
    /// The region keeps the parent it was declared with. Close it with [`Self::end_region`].
    pub fn resume_region(&mut self, region: RegionId) -> &mut Self {
        self.regions.open.push(region);
        let pc = self.pc();
        self.regions.mark(pc);
        self
    }

    /// Closes the innermost open region, returning it, or `None` when no region is open.
    pub fn end_region(&mut self) -> Option<RegionId> {
        let region = self.regions.open.pop()?;
        let pc = self.pc();
        self.regions.mark(pc);
        Some(region)
    }
}
