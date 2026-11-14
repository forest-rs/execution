// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Definitely initialized registers where a paused frame can stand.
//!
//! The verifier's must-init analysis already knows which registers are initialized on entry to
//! every block. This table keeps that state at each block entry and before each instruction at
//! which a frame can wait for another activation or the embedding: calls, host calls, `resume`
//! and `suspend`. A frame waiting at one of those reads its row directly. A frame paused by a
//! quantum between two checkpoints replays the writes of the instructions in between, which
//! never crosses a block boundary.
//!
//! The effect token r0 is initialized on entry and stays initialized on every path, so a row
//! without it marks an unreachable block, where nothing is initialized whatever the
//! instructions write.
//!
//! The rows match the initialization facts of the verifier's optional function analysis, which
//! inspection used to rebuild on every view. They are also the initialization half of a stack
//! map: a compiled frame needs the same set at the same call sites to know which registers hold
//! values.

use super::bitset::BitSet;
use crate::bytecode::decode_instructions;
use crate::opcode::Opcode;
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::mem::size_of_val;

/// Registers definitely initialized before each block entry and wait point of one function.
///
/// It costs 4 bytes per checkpoint plus one 64-bit word per 64 registers per checkpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InitTable {
    /// Source PCs of the checkpoints, ascending: every block entry and wait point.
    pcs: Box<[u32]>,
    /// One row per checkpoint of `reg_count.div_ceil(64)` words; all zero for unreachable blocks.
    rows: Box<[u64]>,
}

/// Collects an [`InitTable`] while the verifier walks each block's instructions in order.
pub(crate) struct InitTableBuilder {
    words: usize,
    pcs: Vec<u32>,
    rows: Vec<u64>,
}

impl InitTableBuilder {
    /// Starts a table for a function with `reg_count` registers and `blocks` basic blocks,
    /// reserving one checkpoint per block.
    pub(crate) fn new(reg_count: usize, blocks: usize) -> Self {
        let words = reg_count.div_ceil(64);
        Self {
            words,
            pcs: Vec::with_capacity(blocks),
            rows: Vec::with_capacity(blocks * words),
        }
    }

    /// Records the entry of a reachable block: `state` on entry plus its declared `params`.
    ///
    /// Checkpoints are recorded in ascending PC order, block by block.
    pub(crate) fn entry(&mut self, pc: u32, state: &BitSet, params: &[u32]) {
        let row = self.rows.len();
        self.wait_point(pc, state);
        for &register in params {
            self.rows[row + register as usize / 64] |= 1 << (register % 64);
        }
    }

    /// Records `state` before the wait point ([`is_wait_point`]) at `pc` of a reachable block.
    pub(crate) fn wait_point(&mut self, pc: u32, state: &BitSet) {
        debug_assert!(
            self.pcs.last().is_none_or(|&last| last < pc),
            "checkpoints ascend"
        );
        debug_assert_eq!(state.words().len(), self.words, "row width");
        debug_assert!(
            self.words == 0 || state.get(0),
            "reachable code has its effect token"
        );
        self.pcs.push(pc);
        self.rows.extend_from_slice(state.words());
    }

    /// Records the entry of an unreachable block, where nothing is initialized.
    pub(crate) fn unreachable(&mut self, pc: u32) {
        debug_assert!(
            self.pcs.last().is_none_or(|&last| last < pc),
            "checkpoints ascend"
        );
        self.pcs.push(pc);
        self.rows.extend(core::iter::repeat_n(0, self.words));
    }

    /// The exactly sized table.
    pub(crate) fn finish(self) -> InitTable {
        InitTable {
            pcs: self.pcs.into(),
            rows: self.rows.into(),
        }
    }
}

impl InitTable {
    /// Registers definitely initialized before the instruction at `pc` of `bytecode`.
    ///
    /// Returns `None` when `pc` is not an instruction boundary of the function.
    pub(crate) fn before(&self, bytecode: &[u8], reg_count: usize, pc: u32) -> Option<BitSet> {
        if pc as usize >= bytecode.len() {
            return None;
        }
        let checkpoint = self
            .pcs
            .partition_point(|&start| start <= pc)
            .checked_sub(1)?;
        let words = reg_count.div_ceil(64);
        let mut state = BitSet::from_words(
            &self.rows[checkpoint * words..(checkpoint + 1) * words],
            reg_count,
        );
        let start = self.pcs[checkpoint];
        if start == pc {
            return Some(state);
        }
        // The slice ends inside an instruction, and fails to decode, unless `pc` is a boundary.
        let between = decode_instructions(&bytecode[start as usize..pc as usize]).ok()?;
        // Nothing in an unreachable block is initialized.
        if state.get(0) {
            for instruction in &between {
                for register in instruction.instr.writes() {
                    state.set(register as usize);
                }
            }
        }
        Some(state)
    }

    /// Bytes held by this table's buffers, which are exactly sized.
    pub(crate) fn bytes(&self) -> usize {
        size_of_val(&*self.pcs) + size_of_val(&*self.rows)
    }
}

/// Whether a frame can stand at an instruction with `opcode` while another activation, a host
/// operation or a handler runs: the call-like opcodes other than `ret`.
///
/// Spelled out rather than read from the opcode table, which the verifier would consult for
/// every instruction. An opcode missing here would only make inspection replay to reach it.
pub(crate) fn is_wait_point(opcode: Opcode) -> bool {
    matches!(
        opcode,
        Opcode::Call
            | Opcode::CallIndirect
            | Opcode::HostCall
            | Opcode::Resume
            | Opcode::ResumeThrow
            | Opcode::Suspend
    )
}

#[cfg(test)]
mod tests {
    use crate::asm::{Catch, FunctionSig, ProgramBuilder};
    use crate::host::HostSig;
    use crate::opcode::Opcode;
    use crate::program::ValueType;
    use crate::value::FuncId;
    use crate::verifier::{ProgramAnalysis, ValueFact, VerifiedProgram};
    use alloc::vec;
    use alloc::vec::Vec;

    fn sig(args: &[ValueType], rets: &[ValueType]) -> FunctionSig {
        FunctionSig {
            arg_types: args.to_vec(),
            ret_types: rets.to_vec(),
        }
    }

    /// Checks the table against the optional function analysis at every byte offset of every
    /// function: the same registers are initialized, and the same offsets are not instructions.
    fn assert_matches_analysis(verified: &VerifiedProgram) {
        let program = verified.program();
        let analysis = ProgramAnalysis::from(verified);
        for (index, definition) in program.functions.iter().enumerate() {
            let func = FuncId(u32::try_from(index).unwrap());
            let mut reference = analysis.function(func).unwrap();
            let table = verified.init_table(func).unwrap();
            let bytecode = definition.bytecode(program).unwrap();
            let registers = definition.reg_count;
            for pc in 0..=u32::try_from(bytecode.len()).unwrap() {
                let expected: Option<Vec<bool>> = reference.instruction(pc).map(|facts| {
                    (0..registers)
                        .map(|r| {
                            matches!(facts.register(r).unwrap().before, ValueFact::Contract(_))
                        })
                        .collect()
                });
                let actual: Option<Vec<bool>> = table
                    .before(bytecode, registers as usize, pc)
                    .map(|set| (0..registers as usize).map(|r| set.get(r)).collect());
                assert_eq!(actual, expected, "function {index}, pc {pc}");
            }
        }
    }

    #[test]
    fn calls_branches_loops_and_unreachable_code_match_the_analysis() {
        let mut b = ProgramBuilder::new();
        let helper = b
            .declare_function(sig(&[ValueType::I64], &[ValueType::I64]))
            .unwrap();
        let mut a = b.function_asm(helper).unwrap();
        a.ret(0, &[1]);
        b.define_function(helper, a).unwrap();
        let probe = b.host_sig_for(
            "probe",
            HostSig {
                args: vec![ValueType::I64],
                rets: vec![ValueType::Str],
            },
        );
        let entry = b
            .declare_function(sig(&[ValueType::I64], &[ValueType::I64]))
            .unwrap();
        let mut a = b.function_asm(entry).unwrap();
        // More than 64 registers, so rows span several words.
        let wide: Vec<u32> = (0..70).map(|_| a.alloc_register(ValueType::I64)).collect();
        let (i, one, done, text, late) = (
            a.alloc_register(ValueType::I64),
            a.alloc_register(ValueType::I64),
            a.alloc_register(ValueType::Bool),
            a.alloc_register(ValueType::Str),
            a.alloc_register(ValueType::I64),
        );
        let carried = a.alloc_register(ValueType::I64);
        let (check, body, exit, dead) = (a.label(), a.label(), a.block(&[carried]), a.label());
        a.const_i64(i, 0).const_i64(one, 1);
        for (n, &register) in wide.iter().enumerate() {
            if n % 16 == 0 {
                a.call(0, helper, 0, &[i], &[register]);
            } else {
                a.mov(register, i);
            }
        }
        a.jmp(check);
        a.place(check).unwrap();
        a.i64_ge(done, i, 1);
        a.br(done, exit.with_args(&[i]), body);
        a.place(body).unwrap();
        a.host_call(0, probe, 0, &[i], &[text])
            .call(0, helper, 0, &[i], &[late])
            .i64_add(i, i, one)
            .jmp(check);
        a.place(exit).unwrap();
        a.i64_add(carried, carried, wide[69]).ret(0, &[carried]);
        a.place(dead).unwrap();
        a.const_i64(late, 3)
            .call(0, helper, 0, &[late], &[late])
            .ret(0, &[late]);
        b.define_function(entry, a).unwrap();
        assert_matches_analysis(&b.build_verified().unwrap());
    }

    #[test]
    fn protected_regions_and_continuations_match_the_analysis() {
        let mut b = ProgramBuilder::new();
        let failed = b.declare_tag("failed", &[ValueType::I64.into()]).unwrap();
        let yield_tag = b.declare_tag("yield", &[ValueType::I64.into()]).unwrap();
        let start = b.call_sig(sig(&[ValueType::I64], &[])).unwrap();
        let next = b.call_sig(sig(&[], &[])).unwrap();

        // A generator that suspends in a loop and throws once it is done.
        let generator = b.declare_function(sig(&[ValueType::I64], &[])).unwrap();
        let mut a = b.function_asm(generator).unwrap();
        let (i, one, done) = (
            a.alloc_register(ValueType::I64),
            a.alloc_register(ValueType::I64),
            a.alloc_register(ValueType::Bool),
        );
        let (check, body, exit) = (a.label(), a.label(), a.label());
        a.const_i64(i, 0).const_i64(one, 1).jmp(check);
        a.place(check).unwrap();
        a.i64_ge(done, i, 1);
        a.br(done, exit, body);
        a.place(body).unwrap();
        a.suspend(0, yield_tag, 0, &[i], &[])
            .i64_add(i, i, one)
            .jmp(check);
        a.place(exit).unwrap();
        a.throw(0, failed, &[i]);
        b.define_function(generator, a).unwrap();

        // Drives the generator under `on` and `catch` clauses; a raise mid-block leaves the
        // writes before it initialized at the handler.
        let driver = b
            .declare_function(sig(&[ValueType::I64], &[ValueType::I64]))
            .unwrap();
        let mut a = b.function_asm(driver).unwrap();
        let (f, cont, total, value, rest, caught, before, after) = (
            a.alloc_register(ValueType::Func),
            a.alloc_register(ValueType::Cont),
            a.alloc_register(ValueType::I64),
            a.alloc_register(ValueType::I64),
            a.alloc_register(ValueType::Cont),
            a.alloc_register(ValueType::I64),
            a.alloc_register(ValueType::I64),
            a.alloc_register(ValueType::I64),
        );
        let yielded = a.block(&[value, rest]);
        let handler = a.block(&[caught]);
        a.const_func(f, generator).cont_new(cont, start, f);
        a.begin_region(&[Catch::On(yield_tag, yielded), Catch::Tag(failed, handler)]);
        a.const_i64(before, 1)
            .resume(0, start, cont, 0, &[1], &[])
            .const_i64(after, 2);
        a.end_region();
        a.const_i64(total, 0).ret(0, &[total]);
        a.place(yielded).unwrap();
        a.begin_region(&[Catch::On(yield_tag, yielded), Catch::Tag(failed, handler)]);
        a.resume(0, next, rest, 0, &[], &[]);
        a.end_region();
        a.ret(0, &[value]);
        a.place(handler).unwrap();
        a.i64_add(caught, caught, before).ret(0, &[caught]);
        b.define_function(driver, a).unwrap();
        assert_matches_analysis(&b.build_verified().unwrap());
    }

    #[test]
    fn wait_points_are_the_call_like_opcodes_other_than_ret() {
        for code in 0..=u16::MAX {
            if let Some(opcode) = Opcode::from_code(code) {
                assert_eq!(
                    super::is_wait_point(opcode),
                    opcode.is_call_like() && opcode != Opcode::Ret,
                    "{opcode:?}"
                );
            }
        }
    }

    #[test]
    fn wait_points_and_block_entries_need_no_replay() {
        let mut b = ProgramBuilder::new();
        let helper = b.declare_function(sig(&[], &[])).unwrap();
        let mut a = b.function_asm(helper).unwrap();
        a.ret(0, &[]);
        b.define_function(helper, a).unwrap();
        let entry = b.declare_function(sig(&[], &[ValueType::I64])).unwrap();
        let mut a = b.function_asm(entry).unwrap();
        let value = a.alloc_register(ValueType::I64);
        a.const_i64(value, 1);
        let call = a.pc();
        a.call(0, helper, 0, &[], &[]).ret(0, &[value]);
        b.define_function(entry, a).unwrap();
        let verified = b.build_verified().unwrap();
        let table = verified.init_table(entry).unwrap();
        assert_eq!(&*table.pcs, &[0, call]);
        assert_eq!(table.bytes(), 2 * 4 + 2 * 8);
    }
}
