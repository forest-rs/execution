// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Control-flow graph (CFG) construction for bytecode.

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

use crate::bytecode::{DecodedInstr, Instr};

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

        match &di.instr {
            Instr::Br {
                pc_true, pc_false, ..
            } => {
                if *pc_true >= byte_len {
                    return Err(InvalidJumpTarget {
                        src_pc: di.offset,
                        target_pc: *pc_true,
                        out_of_range: true,
                    });
                }
                if *pc_false >= byte_len {
                    return Err(InvalidJumpTarget {
                        src_pc: di.offset,
                        target_pc: *pc_false,
                        out_of_range: true,
                    });
                }
                leader[*pc_true as usize] = true;
                leader_src[*pc_true as usize].get_or_insert(di.offset);
                leader[*pc_false as usize] = true;
                leader_src[*pc_false as usize].get_or_insert(di.offset);
                if end <= byte_len as usize {
                    leader[end] = true;
                    leader_src[end].get_or_insert(di.offset);
                }
            }
            Instr::Jmp { pc_target } => {
                if *pc_target >= byte_len {
                    return Err(InvalidJumpTarget {
                        src_pc: di.offset,
                        target_pc: *pc_target,
                        out_of_range: true,
                    });
                }
                leader[*pc_target as usize] = true;
                leader_src[*pc_target as usize].get_or_insert(di.offset);
                if end <= byte_len as usize {
                    leader[end] = true;
                    leader_src[end].get_or_insert(di.offset);
                }
            }
            Instr::Ret { .. } | Instr::Trap { .. } => {
                if end <= byte_len as usize {
                    leader[end] = true;
                    leader_src[end].get_or_insert(di.offset);
                }
            }
            _ => {}
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
        let fallthrough = if blocks[i].end_pc < byte_len {
            Some(pc_to_block[blocks[i].end_pc as usize])
        } else {
            None
        };
        blocks[i].succs = match &di.instr {
            Instr::Br {
                pc_true, pc_false, ..
            } => [
                Some(pc_to_block[*pc_true as usize]),
                Some(pc_to_block[*pc_false as usize]),
            ],
            Instr::Jmp { pc_target } => [Some(pc_to_block[*pc_target as usize]), None],
            Instr::Ret { .. } | Instr::Trap { .. } => [None, None],
            _ => [fallthrough, None],
        };
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
