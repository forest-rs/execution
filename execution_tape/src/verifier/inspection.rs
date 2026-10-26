// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Borrowed inspection of the exact artifact used by the interpreter.

use crate::contract::{ContractId, ContractKinds};
use core::fmt;
use core::mem::size_of;

use super::VerifiedProgram;
use crate::arena::{BytesHandle, StrHandle};
use crate::opcode::Opcode;
use crate::program::{Function, RegisterTypes, SpanId, ValueType};
use crate::typed::{ExecFunc, RegCounts};
use crate::value::{AggHandle, Closure, Decimal, FuncId, Obj};

/// A borrowed function in a verified program, without decoding or copying its instructions.
///
/// The view exposes supported execution facts, not the interpreter's internal instruction or
/// register layout. All counts describe this function, including unreachable instructions.
#[derive(Clone, Copy)]
pub struct VerifiedFunction<'a> {
    pub(super) program: &'a VerifiedProgram,
    pub(super) id: FuncId,
    pub(super) definition: &'a Function,
    pub(super) executable: &'a ExecFunc,
}

impl fmt::Debug for VerifiedFunction<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VerifiedFunction")
            .field("id", &self.id)
            .field("arg_types", &self.arg_types())
            .field("ret_types", &self.ret_types())
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

/// Function-local execution and storage requirements.
///
/// Byte counts use this build's layout and are not portable format sizes or allocator/RSS
/// measurements. Prepared-buffer counts exclude inline headers, the source program, and shared
/// signature caches. Frame counts exclude vector headers/capacity, call frames, payloads,
/// aggregates, and host-call scratch. They are a lower bound on execution-context storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct FunctionStats {
    /// Encoded source bytecode length in bytes.
    pub bytecode_bytes: u32,
    /// Number of prepared instructions, including unreachable instructions.
    pub instruction_count: usize,
    /// Number of entries in the packed variable-length register operand buffer.
    pub operand_count: usize,
    /// Declared virtual register count, including the effect token and unused registers.
    pub virtual_registers: u32,
    /// Total typed-bank slots needed by one frame, including unit registers.
    pub register_slots: usize,
    /// Bytes for the typed-bank elements of one frame; strings/bytes occupy handles only.
    /// Unit registers currently occupy `u32` slots although their public value has no payload.
    pub register_bytes: usize,
    /// Bytes occupied by populated elements of this function's prepared buffers.
    pub prepared_used_bytes: usize,
    /// Bytes reserved by this function's prepared buffers, including spare capacity.
    pub prepared_capacity_bytes: usize,
}

/// A prepared instruction's source location and opcode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InstructionInfo {
    /// Source byte offset within the function.
    pub pc: u32,
    /// Lexical end of the instruction, not its control-flow successor.
    pub next_pc: u32,
    /// Original bytecode opcode.
    pub opcode: Opcode,
    /// Resolved source span used by runtime tracing, if any.
    pub span: Option<SpanId>,
}

impl<'a> VerifiedFunction<'a> {
    /// Returns the function's identifier in its originating program.
    #[must_use]
    pub fn id(self) -> FuncId {
        self.id
    }

    /// Borrows declared virtual-register kinds, including unused registers and r0.
    #[must_use]
    pub fn register_types(self) -> RegisterTypes<'a> {
        self.definition
            .verified_register_types(self.program.program())
    }

    /// Returns value argument types, excluding the implicit effect token.
    #[must_use]
    pub fn arg_types(self) -> ContractKinds<'a> {
        self.definition
            .arg_types(self.program.program())
            .expect("verified argument range")
    }

    /// Returns return-value types.
    #[must_use]
    pub fn ret_types(self) -> ContractKinds<'a> {
        self.definition
            .ret_types(self.program.program())
            .expect("verified return range")
    }

    /// Borrows this function's authoritative argument contracts.
    #[must_use]
    pub fn arg_contracts(self) -> &'a [ContractId] {
        self.definition
            .arg_contracts(self.program.program())
            .expect("verified contracts")
    }

    /// Borrows this function's authoritative result contracts.
    #[must_use]
    pub fn ret_contracts(self) -> &'a [ContractId] {
        self.definition
            .ret_contracts(self.program.program())
            .expect("verified contracts")
    }

    /// Returns the number of slots in the register bank used by `value_type` for one frame.
    ///
    /// All host object type IDs share one bank. Unit slots have zero element size. This is a
    /// bank requirement, not a mapping from source register numbers to physical locations.
    #[must_use]
    pub fn register_slots(self, value_type: ValueType) -> usize {
        let counts = self.executable.reg_layout.counts;
        match value_type {
            ValueType::Unit => counts.unit,
            ValueType::Bool => counts.bools,
            ValueType::I64 => counts.i64s,
            ValueType::U64 => counts.u64s,
            ValueType::F64 => counts.f64s,
            ValueType::Decimal => counts.decimals,
            ValueType::Bytes => counts.bytes,
            ValueType::Str => counts.strs,
            ValueType::Obj(_) => counts.objs,
            ValueType::Agg => counts.aggs,
            ValueType::Func => counts.funcs,
            ValueType::Closure => counts.closures,
        }
    }

    /// Iterates prepared instruction locations without reparsing source bytecode.
    pub fn instructions(self) -> impl ExactSizeIterator<Item = InstructionInfo> + 'a {
        let source = self
            .definition
            .bytecode(self.program.program())
            .expect("verified bytecode range");
        self.executable
            .instrs
            .iter()
            .enumerate()
            .map(move |(index, instruction)| InstructionInfo {
                pc: instruction.offset,
                next_pc: instruction.end,
                opcode: Opcode::from_byte(source[instruction.offset as usize])
                    .expect("verified opcode"),
                span: self.executable.span_at_ix(index),
            })
    }

    /// Returns constant-time function-local execution and buffer statistics.
    #[must_use]
    pub fn stats(self) -> FunctionStats {
        let executable = self.executable;
        let buffers = [
            buffer_bytes(&executable.contract_checks),
            buffer_bytes(&executable.reg_layout.arg_regs),
            buffer_bytes(&executable.operands),
            buffer_bytes(&executable.instrs),
            buffer_bytes(&executable.span_by_instr_ix),
        ];
        let counts = executable.reg_layout.counts;
        FunctionStats {
            bytecode_bytes: executable.byte_len,
            instruction_count: executable.instrs.len(),
            operand_count: executable.operands.len(),
            virtual_registers: self.definition.reg_count,
            register_slots: [
                counts.unit,
                counts.bools,
                counts.i64s,
                counts.u64s,
                counts.f64s,
                counts.decimals,
                counts.bytes,
                counts.strs,
                counts.objs,
                counts.aggs,
                counts.funcs,
                counts.closures,
            ]
            .into_iter()
            .sum(),
            register_bytes: register_bytes(counts),
            prepared_used_bytes: buffers.iter().map(|&(used, _)| used).sum(),
            prepared_capacity_bytes: buffers.iter().map(|&(_, capacity)| capacity).sum(),
        }
    }
}

fn buffer_bytes<T>(buffer: &alloc::vec::Vec<T>) -> (usize, usize) {
    (
        buffer.len() * size_of::<T>(),
        buffer.capacity() * size_of::<T>(),
    )
}

fn register_bytes(counts: RegCounts) -> usize {
    counts.unit * size_of::<u32>()
        + counts.bools * size_of::<bool>()
        + counts.i64s * size_of::<i64>()
        + counts.u64s * size_of::<u64>()
        + counts.f64s * size_of::<f64>()
        + counts.decimals * size_of::<Decimal>()
        + counts.bytes * size_of::<BytesHandle>()
        + counts.strs * size_of::<StrHandle>()
        + counts.objs * size_of::<Obj>()
        + counts.aggs * size_of::<AggHandle>()
        + counts.funcs * size_of::<FuncId>()
        + counts.closures * size_of::<Closure>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asm::{Asm, FunctionSig, ProgramBuilder};
    use crate::disasm::disassemble_function;
    use crate::program::HostTypeId;
    use alloc::{format, vec, vec::Vec};
    use core::num::NonZeroU64;

    #[test]
    fn unit_bank_storage_is_included_in_frame_bytes() {
        let mut body = Asm::new();
        // Effect token, argument, and an unused declaration all reserve bank slots.
        body.set_register_types(vec![ValueType::Unit; 3]);
        body.ret(0, &[1]);
        let mut builder = ProgramBuilder::new();
        let id = builder
            .push_function_checked(
                body,
                FunctionSig {
                    arg_types: vec![ValueType::Unit],
                    ret_types: vec![ValueType::Unit],
                },
            )
            .unwrap();
        let program = builder.build_verified().unwrap();
        let function = program.function(id).unwrap();
        assert_eq!(function.register_slots(ValueType::Unit), 3);
        assert_eq!(function.stats().register_bytes, 3 * size_of::<u32>());
    }

    #[test]
    fn signatures_and_bank_requirements_describe_one_function() {
        let types = vec![
            ValueType::Unit,
            ValueType::Bool,
            ValueType::I64,
            ValueType::U64,
            ValueType::F64,
            ValueType::Decimal,
            ValueType::Bytes,
            ValueType::Str,
            ValueType::Obj(HostTypeId(11)),
            ValueType::Obj(HostTypeId(22)),
            ValueType::Agg,
            ValueType::Func,
            ValueType::Closure,
        ];
        let mut body = Asm::new();
        body.set_register_types(vec![
            ValueType::Unit,
            ValueType::Unit,
            ValueType::Bool,
            ValueType::I64,
            ValueType::U64,
            ValueType::F64,
            ValueType::Decimal,
            ValueType::Bytes,
            ValueType::Str,
            ValueType::Obj(HostTypeId(11)),
            ValueType::Obj(HostTypeId(22)),
            ValueType::Agg,
            ValueType::Func,
            ValueType::Closure,
        ]);
        body.ret(
            0,
            &(1..=u32::try_from(types.len()).unwrap()).collect::<Vec<_>>(),
        );
        let mut pb = ProgramBuilder::new();
        let id = pb
            .push_function_checked(
                body,
                FunctionSig {
                    arg_types: types.clone(),
                    ret_types: types.clone(),
                },
            )
            .unwrap();
        let mut unrelated = Asm::new();
        unrelated.set_register_types(vec![ValueType::Unit]);
        unrelated.ret(0, &[]);
        let other = pb
            .push_function_checked(
                unrelated,
                FunctionSig {
                    arg_types: vec![],
                    ret_types: vec![],
                },
            )
            .unwrap();
        pb.set_function_name(other, "unrelated sentinel").unwrap();
        let p = pb.build_verified().unwrap();
        let f = p.function(id).unwrap();
        assert_eq!(f.id(), id);
        assert_eq!(f.arg_types(), types);
        assert_eq!(f.ret_types(), types);
        assert_eq!(
            p.functions().map(|f| f.id()).collect::<Vec<_>>(),
            [id, other]
        );
        assert!(p.function(FuncId(99)).is_none());
        for ty in &types {
            assert_eq!(
                f.register_slots(*ty),
                if matches!(ty, ValueType::Unit | ValueType::Obj(_)) {
                    2
                } else {
                    1
                }
            );
        }
        assert_eq!(f.register_slots(ValueType::Obj(HostTypeId(999))), 2);
        let stats = f.stats();
        assert_eq!(stats.instruction_count, 1);
        assert_eq!(stats.operand_count, types.len());
        assert_eq!(stats.virtual_registers as usize, types.len() + 1);
        assert_eq!(stats.register_slots, types.len() + 1);
        assert!(stats.register_bytes > 0);
        assert!(stats.register_bytes < stats.register_slots * size_of::<crate::value::Value>());
        assert!(stats.prepared_used_bytes > 0);
        assert!(stats.prepared_capacity_bytes >= stats.prepared_used_bytes);
        assert!(!format!("{f:?}").contains("unrelated sentinel"));
    }

    #[test]
    fn contract_views_borrow_only_the_selected_signature() {
        use crate::asm::FunctionAbi;
        use crate::contract::Contract;
        let mut pb = ProgramBuilder::new();
        let integer = pb.contract(Contract::Kind(ValueType::I64)).unwrap();
        let opaque = pb.contract(Contract::Kind(ValueType::Agg)).unwrap();
        let first = pb.contract(Contract::Tuple(&[Some(integer)])).unwrap();
        let last = pb.contract(Contract::Tuple(&[])).unwrap();
        let mut body = Asm::new();
        body.set_register_types(vec![
            ValueType::Unit,
            ValueType::Agg,
            ValueType::Agg,
            ValueType::Agg,
        ]);
        body.ret(0, &[1]);
        let id = pb
            .push_function_checked(
                body,
                FunctionAbi {
                    args: vec![first.into(), opaque.into(), last.into()],
                    rets: vec![first.into()],
                },
            )
            .unwrap();
        let mut unrelated = Asm::new();
        unrelated.set_register_types(vec![ValueType::Unit]);
        unrelated.ret(0, &[]);
        pb.push_function_checked(
            unrelated,
            FunctionSig {
                arg_types: vec![],
                ret_types: vec![],
            },
        )
        .unwrap();
        let p = pb.build_verified().unwrap();
        let function = p.function(id).unwrap();
        assert_eq!(function.arg_contracts(), &[first, opaque, last]);
        assert_eq!(function.ret_contracts(), &[first]);
        assert_eq!(
            p.program().contracts.get(first),
            Some(Contract::Tuple(&[Some(integer)]))
        );
    }

    #[test]
    fn prepared_instruction_locations_match_source_decoding_and_runtime_spans() {
        let first_span = SpanId(NonZeroU64::new(5).unwrap());
        let last_span = SpanId(NonZeroU64::new(7).unwrap());
        let mut body = Asm::new();
        body.set_register_types(vec![ValueType::Unit, ValueType::Bool]);
        let yes = body.label();
        let no = body.label();
        body.const_bool(1, true);
        body.span(first_span);
        body.br(1, yes, no);
        body.place(yes).unwrap();
        body.ret(0, &[]);
        body.place(no).unwrap();
        body.span(last_span);
        body.ret(0, &[]);
        let mut pb = ProgramBuilder::new();
        let id = pb
            .push_function_checked(
                body,
                FunctionSig {
                    arg_types: vec![],
                    ret_types: vec![],
                },
            )
            .unwrap();
        let p = pb.build_verified().unwrap();
        let f = p.function(id).unwrap();
        let instructions: Vec<_> = f.instructions().collect();
        let source = disassemble_function(p.program(), id).unwrap();
        let source: Vec<_> = source.instrs().map(|i| (i.pc(), i.opcode())).collect();
        assert_eq!(
            instructions
                .iter()
                .map(|i| (i.pc, i.opcode))
                .collect::<Vec<_>>(),
            source
        );
        assert_eq!(
            instructions.iter().map(|i| i.span).collect::<Vec<_>>(),
            [None, Some(first_span), Some(first_span), Some(last_span)]
        );
        for pair in instructions.windows(2) {
            assert_eq!(pair[0].next_pc, pair[1].pc);
        }
        assert_eq!(
            instructions.last().unwrap().next_pc,
            f.stats().bytecode_bytes
        );
        assert_eq!(f.instructions().len(), f.stats().instruction_count);
    }
}
