// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Backwards liveness analysis for bytecode.
//!
//! We currently track liveness only for "general" registers (`RegId != 0`). Register 0 is reserved
//! by convention (effect token plumbing), and treating it as a normal value reg would add noise to
//! liveness results without enabling useful optimizations.

extern crate alloc;

use alloc::vec::Vec;

use crate::analysis::bitset::BitSet;
use crate::analysis::cfg::BasicBlock;
use crate::analysis::dataflow;
use crate::bytecode::DecodedInstr;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Liveness {
    pub(crate) live_in: Vec<BitSet>,
    pub(crate) live_out: Vec<BitSet>,
}

pub(crate) fn compute_use_def(
    reg_count: usize,
    decoded: &[DecodedInstr],
    blocks: &[BasicBlock],
) -> (Vec<BitSet>, Vec<BitSet>) {
    let mut use_sets: Vec<BitSet> = Vec::with_capacity(blocks.len());
    let mut def_sets: Vec<BitSet> = Vec::with_capacity(blocks.len());

    for b in blocks {
        let mut use_set = BitSet::new_empty(reg_count);
        let mut def_set = BitSet::new_empty(reg_count);
        for di in decoded.iter().take(b.instr_end).skip(b.instr_start) {
            for r in di.instr.reads() {
                if r == 0 {
                    continue;
                }
                if !def_set.get(r as usize) {
                    use_set.set(r as usize);
                }
            }
            for w in di.instr.writes() {
                if w == 0 {
                    continue;
                }
                def_set.set(w as usize);
            }
        }
        use_sets.push(use_set);
        def_sets.push(def_set);
    }

    (use_sets, def_sets)
}

pub(crate) fn compute_liveness(
    reg_count: usize,
    decoded: &[DecodedInstr],
    blocks: &[BasicBlock],
    reachable: &[bool],
) -> Liveness {
    let (use_sets, def_sets) = compute_use_def(reg_count, decoded, blocks);

    let bottom = BitSet::new_empty(reg_count);
    let (live_in, live_out) = dataflow::solve_backward(
        blocks,
        reachable,
        bottom,
        |acc, succ_in| acc.union_with(succ_in),
        |b_idx, _b, out_state| {
            // IN = USE ∪ (OUT \ DEF)
            let mut in_set = use_sets[b_idx].clone();
            let mut tmp = out_state.clone();
            tmp.subtract_with(&def_sets[b_idx]);
            in_set.union_with(&tmp);
            in_set
        },
    );

    Liveness { live_in, live_out }
}
