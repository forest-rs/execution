// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Optional register inspection. No observation indexes enter prepared definitions.

use super::*;
use crate::bytecode::{DecodedInstr, Instr, decode_instructions};
use crate::disasm::RegIter;
use crate::trace::{RegisterEvent, RegisterPhase, RegisterValues};
use alloc::collections::btree_map::Entry;

/// Per-run indexes owned only by the trace-capable interpreter policy.
pub(super) type RegisterState = alloc::collections::BTreeMap<u32, FunctionRegisters>;

pub(super) struct FunctionRegisters {
    instructions: Vec<DecodedInstr>,
    registers: Vec<VReg>,
}
impl FunctionRegisters {
    fn new(program: &Program, func: FuncId) -> Option<Self> {
        let function = program.functions.get(func.0 as usize)?;
        let instructions = decode_instructions(function.bytecode(program).ok()?).ok()?;
        let registers = register_map(program, func)?;
        Some(Self {
            instructions,
            registers,
        })
    }
}

pub(super) fn emit(
    register_state: &mut RegisterState,
    trace: &mut TraceCtx<'_>,
    program: &Program,
    ctx: &ExecutionContext,
    func: FuncId,
    pc: u32,
    base: RegBase,
    sources: ValueSources<'_>,
    phase: RegisterPhase,
) {
    if !trace.enabled(TraceMask::REGISTERS) {
        return;
    }
    let location = TraceLocation { func, pc };
    let Some(sink) = &mut trace.sink else {
        return;
    };
    if !sink.wants_registers(location, phase) {
        return;
    }
    let map = match register_state.entry(func.0) {
        Entry::Vacant(entry) => {
            let Some(map) = FunctionRegisters::new(program, func) else {
                return;
            };
            entry.insert(map)
        }
        Entry::Occupied(entry) => entry.into_mut(),
    };
    let Ok(index) = map.instructions.binary_search_by_key(&pc, |i| i.offset) else {
        return;
    };
    let instruction = &map.instructions[index].instr;
    // Calls have only dispatched; their caller outputs do not exist yet. Ret can have released
    // its frame. Do not read a bank slot just because its storage was allocated or reused.
    if phase == RegisterPhase::Outputs
        && matches!(
            instruction,
            Instr::Call { .. } | Instr::CallIndirect { .. } | Instr::Ret { .. }
        )
    {
        return;
    }
    let registers = match phase {
        RegisterPhase::Inputs => RegIter::from_operands(instruction.reads()),
        RegisterPhase::Outputs => RegIter::from_operands(instruction.writes()),
    };
    let read = |reg: u32| {
        let reg = *map.registers.get(reg as usize)?;
        read_value_ref_at(
            &ctx.arena,
            sources,
            &ctx.bools,
            &ctx.i64s,
            &ctx.u64s,
            &ctx.f64s,
            &ctx.decimals,
            &ctx.bytes,
            &ctx.strs,
            &ctx.objs,
            &ctx.aggs,
            &ctx.funcs,
            &ctx.closures,
            base,
            reg,
        )
        .ok()
    };
    sink.registers(
        program,
        RegisterEvent {
            location,
            phase,
            values: RegisterValues {
                registers,
                read: &read,
            },
            aggregates: sources.aggregates(),
        },
    );
}

/// Source-to-bank mapping requested by optional tracing or paused inspection.
pub(super) fn register_map(program: &Program, func: FuncId) -> Option<Vec<VReg>> {
    let function = program.functions.get(func.0 as usize)?;
    let mut counts = [0_u32; 12];
    let mut registers = Vec::with_capacity(function.reg_count as usize);
    for kind in function.register_types(program).ok()?.iter() {
        let class = crate::typed::RegClass::of(kind);
        let index = class as usize;
        let n = counts[index];
        counts[index] += 1;
        registers.push(match kind {
            ValueType::Unit => VReg::Unit,
            ValueType::Bool => VReg::Bool(BoolReg(n)),
            ValueType::I64 => VReg::I64(I64Reg(n)),
            ValueType::U64 => VReg::U64(U64Reg(n)),
            ValueType::F64 => VReg::F64(F64Reg(n)),
            ValueType::Decimal => VReg::Decimal(DecimalReg(n)),
            ValueType::Bytes => VReg::Bytes(BytesReg(n)),
            ValueType::Str => VReg::Str(StrReg(n)),
            ValueType::Obj(_) => VReg::Obj(ObjReg(n)),
            ValueType::Agg => VReg::Agg(AggReg(n)),
            ValueType::Func => VReg::Func(FuncReg(n)),
            ValueType::Closure => VReg::Closure(ClosureReg(n)),
        });
    }
    Some(registers)
}
