// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Small reusable dataflow solvers for verifier/tooling analyses.
//!
//! The verifier currently needs several fixpoint computations over a CFG (must-init, must-types,
//! liveness, etc). This module provides a simple worklist-based engine so analyses can share the
//! iteration mechanics while keeping their lattice and transfer logic local.
//!
//! Notes:
//!
//! - These solvers intentionally don't try to be "smart" (RPO, bitset special-casing, etc). Keep
//!   them small and correct; optimize in specific analyses once a profiler says it's worth it.
//! - Correctness assumes the usual dataflow conditions: the `meet_into` and `transfer_block`
//!   functions are monotone over a finite-height lattice, so iteration reaches a fixpoint.

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;

use crate::analysis::cfg::BasicBlock;

/// Computes a forward dataflow fixpoint.
///
/// The analysis is defined by:
/// - `entry`: initial state at block 0 (only used if block 0 is reachable)
/// - `bottom`: initial state for all other blocks (usually "uninitialized" / "top")
/// - `meet_into`: in-place meet operation: `acc = meet(acc, incoming)`
/// - `transfer_block`: transfer function for a single basic block
///
/// The meet over an empty predecessor set is the current `bottom` value. (For analyses that want
/// a different identity element, pass it as `bottom`.)
///
/// Blocks marked unreachable in `reachable` are ignored and left as `bottom`.
pub(crate) fn solve_forward<State, MeetInto, TransferBlock>(
    blocks: &[BasicBlock],
    reachable: &[bool],
    entry: State,
    bottom: State,
    mut meet_into: MeetInto,
    mut transfer_block: TransferBlock,
) -> (Vec<State>, Vec<State>)
where
    State: Clone + PartialEq,
    MeetInto: FnMut(&mut State, &State),
    TransferBlock: FnMut(usize, &BasicBlock, &State) -> State,
{
    // Convention: `in_states[b]` is the fixpoint state at block entry, and `out_states[b]` is the
    // fixpoint state at block exit (after applying the block transfer).
    //
    // The solver is intentionally small and explicit:
    // - forward propagation along `succs`
    // - a worklist of blocks whose OUT changed
    // - callers provide the lattice meet and transfer functions
    let n = blocks.len();
    let mut in_states: Vec<State> = (0..n).map(|_| bottom.clone()).collect();
    let mut out_states: Vec<State> = (0..n).map(|_| bottom.clone()).collect();

    if n == 0 {
        return (in_states, out_states);
    }

    let mut work: VecDeque<usize> = VecDeque::new();

    if reachable.first().copied().unwrap_or(false) {
        // Seed entry.
        in_states[0] = entry;
        out_states[0] = transfer_block(0, &blocks[0], &in_states[0]);
        work.push_back(0);
    }

    while let Some(b_idx) = work.pop_front() {
        if !reachable.get(b_idx).copied().unwrap_or(false) {
            continue;
        }

        // Re-propagate this block's OUT to its successors.
        let out = out_states[b_idx].clone();
        for succ in blocks[b_idx].succs.iter().copied().flatten() {
            if !reachable.get(succ).copied().unwrap_or(false) {
                continue;
            }

            // IN_succ = meet(IN_succ, OUT_pred) for each predecessor.
            let mut new_in = in_states[succ].clone();
            meet_into(&mut new_in, &out);
            if new_in != in_states[succ] {
                in_states[succ] = new_in;

                // OUT_succ is derived purely from IN_succ via the per-block transfer.
                let new_out = transfer_block(succ, &blocks[succ], &in_states[succ]);
                if new_out != out_states[succ] {
                    out_states[succ] = new_out;
                    work.push_back(succ);
                }
            }
        }
    }

    (in_states, out_states)
}

/// Computes a backward dataflow fixpoint.
///
/// The analysis is defined by:
/// - `bottom`: initial state for all blocks (usually "empty"/"unreachable")
/// - `meet_into`: in-place meet operation used when combining successor IN states into OUT:
///   `acc = meet(acc, succ_in)`
/// - `transfer_block`: transfer function for a single basic block computing IN from OUT
///
/// The meet over an empty successor set is the current `bottom` value. (For analyses that want a
/// different identity element, pass it as `bottom`.)
///
/// Blocks marked unreachable in `reachable` are ignored and left as `bottom`.
pub(crate) fn solve_backward<State, MeetInto, TransferBlock>(
    blocks: &[BasicBlock],
    reachable: &[bool],
    bottom: State,
    mut meet_into: MeetInto,
    mut transfer_block: TransferBlock,
) -> (Vec<State>, Vec<State>)
where
    State: Clone + PartialEq,
    MeetInto: FnMut(&mut State, &State),
    TransferBlock: FnMut(usize, &BasicBlock, &State) -> State,
{
    // Convention: `out_states[b]` is the fixpoint state at block exit, and `in_states[b]` is the
    // fixpoint state at block entry (after applying the block transfer in reverse direction).
    //
    // The solver:
    // - computes predecessor lists once
    // - propagates changes backward via a worklist of blocks whose IN/OUT changed
    let n = blocks.len();
    let mut in_states: Vec<State> = (0..n).map(|_| bottom.clone()).collect();
    let mut out_states: Vec<State> = (0..n).map(|_| bottom.clone()).collect();

    if n == 0 {
        return (in_states, out_states);
    }

    // Precompute preds from succs.
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, b) in blocks.iter().enumerate() {
        for succ in b.succs.iter().copied().flatten() {
            if succ < n {
                preds[succ].push(i);
            }
        }
    }

    let mut work: VecDeque<usize> = VecDeque::new();
    for (i, &r) in reachable.iter().enumerate().take(n) {
        if r {
            work.push_back(i);
        }
    }

    while let Some(b_idx) = work.pop_front() {
        if !reachable.get(b_idx).copied().unwrap_or(false) {
            continue;
        }

        // OUT[b] = meet over IN[succ].
        let mut new_out = bottom.clone();
        for succ in blocks[b_idx].succs.iter().copied().flatten() {
            if !reachable.get(succ).copied().unwrap_or(false) {
                continue;
            }
            meet_into(&mut new_out, &in_states[succ]);
        }

        // IN[b] = transfer(b, OUT[b]).
        let mut changed = false;
        if new_out != out_states[b_idx] {
            out_states[b_idx] = new_out;
            changed = true;
        }
        let new_in = transfer_block(b_idx, &blocks[b_idx], &out_states[b_idx]);
        if new_in != in_states[b_idx] {
            in_states[b_idx] = new_in;
            changed = true;
        }

        if changed {
            // Any predecessor's OUT may have changed; revisit preds.
            for &p in &preds[b_idx] {
                work.push_back(p);
            }
        }
    }

    (in_states, out_states)
}
