// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Control-flow graph (CFG) construction for bytecode.

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

use crate::bytecode::DecodedInstr;
use crate::instr_operands;
use crate::opcode::Opcode;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BasicBlock {
    pub(crate) start_pc: u32,
    pub(crate) end_pc: u32,
    pub(crate) instr_start: usize,
    pub(crate) instr_end: usize,
    pub(crate) succs: [Option<usize>; 2],
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct InvalidJumpTarget {
    pub(crate) src_pc: u32,
    pub(crate) target_pc: u32,
    pub(crate) out_of_range: bool,
}

#[must_use]
pub(crate) fn compute_boundaries(byte_len: usize, decoded: &[DecodedInstr]) -> Vec<bool> {
    let mut b = vec![false; byte_len + 1];
    for di in decoded {
        let o = di.offset as usize;
        if o <= byte_len {
            b[o] = true;
        }
    }
    b[byte_len] = true;
    b
}

pub(crate) fn build_basic_blocks(
    byte_len: u32,
    decoded: &[DecodedInstr],
    boundaries: &[bool],
) -> Result<Vec<BasicBlock>, InvalidJumpTarget> {
    // Leaders are: entry, jump targets, and the next instruction after a terminator.
    let mut leader = vec![false; (byte_len as usize) + 1];
    let mut leader_src: Vec<Option<u32>> = vec![None; (byte_len as usize) + 1];
    leader[0] = true;

    for (i, di) in decoded.iter().enumerate() {
        let end = if i + 1 < decoded.len() {
            decoded[i + 1].offset as usize
        } else {
            byte_len as usize
        };

        let op = Opcode::from_u8(di.opcode).expect("decoded instruction opcode must be known");

        let mut err: Option<InvalidJumpTarget> = None;
        instr_operands::visit_pcs(&di.instr, |target_pc| {
            if err.is_some() {
                return;
            }
            if target_pc >= byte_len {
                err = Some(InvalidJumpTarget {
                    src_pc: di.offset,
                    target_pc,
                    out_of_range: true,
                });
                return;
            }
            leader[target_pc as usize] = true;
            leader_src[target_pc as usize].get_or_insert(di.offset);
        });
        if let Some(err) = err {
            return Err(err);
        }

        if op.is_terminator() && end <= byte_len as usize {
            leader[end] = true;
            leader_src[end].get_or_insert(di.offset);
        }
    }

    // Validate that all leaders (targets) are instruction boundaries (or end).
    for (pc, &is_leader) in leader.iter().enumerate() {
        if is_leader && pc <= byte_len as usize && !boundaries[pc] {
            let target_pc = u32::try_from(pc).unwrap_or(byte_len);
            let src_pc = leader_src[pc].unwrap_or(target_pc);
            return Err(InvalidJumpTarget {
                src_pc,
                target_pc,
                out_of_range: false,
            });
        }
    }

    // Map from pc to instruction index.
    let mut pc_to_instr = vec![usize::MAX; (byte_len as usize) + 1];
    for (idx, di) in decoded.iter().enumerate() {
        pc_to_instr[di.offset as usize] = idx;
    }

    // Collect leader pcs in order.
    let mut leader_pcs: Vec<u32> = leader
        .iter()
        .enumerate()
        .filter_map(|(pc, &v)| v.then_some(u32::try_from(pc).unwrap_or(byte_len)))
        .collect();
    leader_pcs.sort_unstable();
    leader_pcs.dedup();

    let mut blocks: Vec<BasicBlock> = Vec::new();
    for (i, &start_pc) in leader_pcs.iter().enumerate() {
        if start_pc == byte_len {
            continue;
        }
        let end_pc = leader_pcs.get(i + 1).copied().unwrap_or(byte_len);
        let instr_start = pc_to_instr[start_pc as usize];
        let instr_end = if end_pc == byte_len {
            decoded.len()
        } else {
            pc_to_instr[end_pc as usize]
        };
        blocks.push(BasicBlock {
            start_pc,
            end_pc,
            instr_start,
            instr_end,
            succs: [None, None],
        });
    }

    // Map pc -> block index.
    let mut pc_to_block = vec![usize::MAX; (byte_len as usize) + 1];
    for (i, b) in blocks.iter().enumerate() {
        pc_to_block[b.start_pc as usize] = i;
    }

    // Fill successors.
    for i in 0..blocks.len() {
        let last = blocks[i].instr_end.saturating_sub(1);
        let Some(di) = decoded.get(last) else {
            continue;
        };
        let op = Opcode::from_u8(di.opcode).expect("decoded instruction opcode must be known");
        let fallthrough = if blocks[i].end_pc < byte_len {
            Some(pc_to_block[blocks[i].end_pc as usize])
        } else {
            None
        };

        if !op.is_terminator() {
            blocks[i].succs = [fallthrough, None];
            continue;
        }

        let mut pcs: [Option<u32>; 2] = [None, None];
        let mut pc_len: usize = 0;
        instr_operands::visit_pcs(&di.instr, |pc| {
            if pc_len < pcs.len() {
                pcs[pc_len] = Some(pc);
                pc_len += 1;
            }
        });
        blocks[i].succs = [
            pcs[0].map(|pc| pc_to_block[pc as usize]),
            pcs[1].map(|pc| pc_to_block[pc as usize]),
        ];
    }

    Ok(blocks)
}

#[must_use]
pub(crate) fn compute_reachable(blocks: &[BasicBlock]) -> Vec<bool> {
    let mut reachable = vec![false; blocks.len()];
    if blocks.is_empty() {
        return reachable;
    }
    let mut stack = vec![0_usize];
    reachable[0] = true;
    while let Some(b) = stack.pop() {
        for &succ in &blocks[b].succs {
            let Some(s) = succ else { continue };
            if s >= reachable.len() {
                continue;
            }
            if !reachable[s] {
                reachable[s] = true;
                stack.push(s);
            }
        }
    }
    reachable
}
