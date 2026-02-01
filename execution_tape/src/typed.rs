// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Typed, verifier-produced execution artifacts.
//!
//! v1 bytecode uses a single virtual register index space (`r0..rN`). To make VM loads/stores
//! monomorphic and avoid runtime tag checks, the verifier assigns each virtual register a
//! [`RegClass`] and a class-local index. It also produces a typed instruction stream whose
//! operands are class-specific newtypes.

use alloc::vec::Vec;

use crate::bytecode::Instr;
use crate::program::HostSigId;
use crate::program::{ConstId, ValueType};
use crate::program::{ElemTypeId, TypeId};
use crate::value::FuncId;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum RegClass {
    Unit,
    Bool,
    I64,
    U64,
    F64,
    Decimal,
    Bytes,
    Str,
    Obj,
    Agg,
    Func,
}

impl RegClass {
    pub(crate) fn of(t: ValueType) -> Self {
        match t {
            ValueType::Unit => Self::Unit,
            ValueType::Bool => Self::Bool,
            ValueType::I64 => Self::I64,
            ValueType::U64 => Self::U64,
            ValueType::F64 => Self::F64,
            ValueType::Decimal => Self::Decimal,
            ValueType::Bytes => Self::Bytes,
            ValueType::Str => Self::Str,
            ValueType::Obj(_) => Self::Obj,
            ValueType::Agg => Self::Agg,
            ValueType::Func => Self::Func,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct UnitReg(pub u32);
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct BoolReg(pub u32);
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct I64Reg(pub u32);
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct U64Reg(pub u32);
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct F64Reg(pub u32);
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct DecimalReg(pub u32);
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct BytesReg(pub u32);
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct StrReg(pub u32);
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct ObjReg(pub u32);
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct AggReg(pub u32);
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct FuncReg(pub u32);

/// A typed register reference (class + class-local index).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum VReg {
    Unit(UnitReg),
    Bool(BoolReg),
    I64(I64Reg),
    U64(U64Reg),
    F64(F64Reg),
    Decimal(DecimalReg),
    Bytes(BytesReg),
    Str(StrReg),
    Obj(ObjReg),
    Agg(AggReg),
    Func(FuncReg),
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RegCounts {
    pub(crate) unit: usize,
    pub(crate) bools: usize,
    pub(crate) i64s: usize,
    pub(crate) u64s: usize,
    pub(crate) f64s: usize,
    pub(crate) decimals: usize,
    pub(crate) bytes: usize,
    pub(crate) strs: usize,
    pub(crate) objs: usize,
    pub(crate) aggs: usize,
    pub(crate) funcs: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RegLayout {
    pub(crate) reg_map: Vec<VReg>,
    pub(crate) counts: RegCounts,
    pub(crate) arg_regs: Vec<VReg>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VerifiedDecodedInstr {
    pub(crate) offset: u32,
    pub(crate) opcode: u8,
    pub(crate) instr: VerifiedInstr,
}

/// A verifier-produced instruction stream with typed register operands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum VerifiedInstr {
    /// No-op.
    Nop,
    /// Trap unconditionally with a trap code.
    Trap {
        code: u32,
    },

    MovUnit {
        dst: UnitReg,
        src: UnitReg,
    },
    MovBool {
        dst: BoolReg,
        src: BoolReg,
    },
    MovI64 {
        dst: I64Reg,
        src: I64Reg,
    },
    MovU64 {
        dst: U64Reg,
        src: U64Reg,
    },
    MovF64 {
        dst: F64Reg,
        src: F64Reg,
    },
    MovDecimal {
        dst: DecimalReg,
        src: DecimalReg,
    },
    MovBytes {
        dst: BytesReg,
        src: BytesReg,
    },
    MovStr {
        dst: StrReg,
        src: StrReg,
    },
    MovObj {
        dst: ObjReg,
        src: ObjReg,
    },
    MovAgg {
        dst: AggReg,
        src: AggReg,
    },
    MovFunc {
        dst: FuncReg,
        src: FuncReg,
    },

    ConstUnit {
        dst: UnitReg,
    },
    ConstBool {
        dst: BoolReg,
        imm: bool,
    },
    ConstI64 {
        dst: I64Reg,
        imm: i64,
    },
    ConstU64 {
        dst: U64Reg,
        imm: u64,
    },
    ConstF64 {
        dst: F64Reg,
        bits: u64,
    },
    ConstDecimal {
        dst: DecimalReg,
        mantissa: i64,
        scale: u8,
    },

    ConstPoolUnit {
        dst: UnitReg,
        idx: ConstId,
    },
    ConstPoolBool {
        dst: BoolReg,
        idx: ConstId,
    },
    ConstPoolI64 {
        dst: I64Reg,
        idx: ConstId,
    },
    ConstPoolU64 {
        dst: U64Reg,
        idx: ConstId,
    },
    ConstPoolF64 {
        dst: F64Reg,
        idx: ConstId,
    },
    ConstPoolDecimal {
        dst: DecimalReg,
        idx: ConstId,
    },
    ConstPoolBytes {
        dst: BytesReg,
        idx: ConstId,
    },
    ConstPoolStr {
        dst: StrReg,
        idx: ConstId,
    },

    DecAdd {
        dst: DecimalReg,
        a: DecimalReg,
        b: DecimalReg,
    },
    DecSub {
        dst: DecimalReg,
        a: DecimalReg,
        b: DecimalReg,
    },
    DecMul {
        dst: DecimalReg,
        a: DecimalReg,
        b: DecimalReg,
    },

    F64Add {
        dst: F64Reg,
        a: F64Reg,
        b: F64Reg,
    },
    F64Sub {
        dst: F64Reg,
        a: F64Reg,
        b: F64Reg,
    },
    F64Mul {
        dst: F64Reg,
        a: F64Reg,
        b: F64Reg,
    },
    F64Div {
        dst: F64Reg,
        a: F64Reg,
        b: F64Reg,
    },

    I64Add {
        dst: I64Reg,
        a: I64Reg,
        b: I64Reg,
    },
    I64Sub {
        dst: I64Reg,
        a: I64Reg,
        b: I64Reg,
    },
    I64Mul {
        dst: I64Reg,
        a: I64Reg,
        b: I64Reg,
    },
    I64And {
        dst: I64Reg,
        a: I64Reg,
        b: I64Reg,
    },
    I64Or {
        dst: I64Reg,
        a: I64Reg,
        b: I64Reg,
    },
    I64Xor {
        dst: I64Reg,
        a: I64Reg,
        b: I64Reg,
    },
    I64Shl {
        dst: I64Reg,
        a: I64Reg,
        b: I64Reg,
    },
    I64Shr {
        dst: I64Reg,
        a: I64Reg,
        b: I64Reg,
    },

    U64Add {
        dst: U64Reg,
        a: U64Reg,
        b: U64Reg,
    },
    U64Sub {
        dst: U64Reg,
        a: U64Reg,
        b: U64Reg,
    },
    U64Mul {
        dst: U64Reg,
        a: U64Reg,
        b: U64Reg,
    },
    U64And {
        dst: U64Reg,
        a: U64Reg,
        b: U64Reg,
    },
    U64Or {
        dst: U64Reg,
        a: U64Reg,
        b: U64Reg,
    },
    U64Xor {
        dst: U64Reg,
        a: U64Reg,
        b: U64Reg,
    },
    U64Shl {
        dst: U64Reg,
        a: U64Reg,
        b: U64Reg,
    },
    U64Shr {
        dst: U64Reg,
        a: U64Reg,
        b: U64Reg,
    },

    I64Eq {
        dst: BoolReg,
        a: I64Reg,
        b: I64Reg,
    },
    I64Lt {
        dst: BoolReg,
        a: I64Reg,
        b: I64Reg,
    },
    I64Gt {
        dst: BoolReg,
        a: I64Reg,
        b: I64Reg,
    },
    I64Le {
        dst: BoolReg,
        a: I64Reg,
        b: I64Reg,
    },
    I64Ge {
        dst: BoolReg,
        a: I64Reg,
        b: I64Reg,
    },

    U64Eq {
        dst: BoolReg,
        a: U64Reg,
        b: U64Reg,
    },
    U64Lt {
        dst: BoolReg,
        a: U64Reg,
        b: U64Reg,
    },
    U64Gt {
        dst: BoolReg,
        a: U64Reg,
        b: U64Reg,
    },
    U64Le {
        dst: BoolReg,
        a: U64Reg,
        b: U64Reg,
    },
    U64Ge {
        dst: BoolReg,
        a: U64Reg,
        b: U64Reg,
    },

    F64Eq {
        dst: BoolReg,
        a: F64Reg,
        b: F64Reg,
    },
    F64Lt {
        dst: BoolReg,
        a: F64Reg,
        b: F64Reg,
    },
    F64Gt {
        dst: BoolReg,
        a: F64Reg,
        b: F64Reg,
    },
    F64Le {
        dst: BoolReg,
        a: F64Reg,
        b: F64Reg,
    },
    F64Ge {
        dst: BoolReg,
        a: F64Reg,
        b: F64Reg,
    },

    BoolNot {
        dst: BoolReg,
        a: BoolReg,
    },
    BoolAnd {
        dst: BoolReg,
        a: BoolReg,
        b: BoolReg,
    },
    BoolOr {
        dst: BoolReg,
        a: BoolReg,
        b: BoolReg,
    },
    BoolXor {
        dst: BoolReg,
        a: BoolReg,
        b: BoolReg,
    },

    U64ToI64 {
        dst: I64Reg,
        a: U64Reg,
    },
    I64ToU64 {
        dst: U64Reg,
        a: I64Reg,
    },

    SelectUnit {
        dst: UnitReg,
        cond: BoolReg,
        a: UnitReg,
        b: UnitReg,
    },
    SelectBool {
        dst: BoolReg,
        cond: BoolReg,
        a: BoolReg,
        b: BoolReg,
    },
    SelectI64 {
        dst: I64Reg,
        cond: BoolReg,
        a: I64Reg,
        b: I64Reg,
    },
    SelectU64 {
        dst: U64Reg,
        cond: BoolReg,
        a: U64Reg,
        b: U64Reg,
    },
    SelectF64 {
        dst: F64Reg,
        cond: BoolReg,
        a: F64Reg,
        b: F64Reg,
    },
    SelectDecimal {
        dst: DecimalReg,
        cond: BoolReg,
        a: DecimalReg,
        b: DecimalReg,
    },
    SelectBytes {
        dst: BytesReg,
        cond: BoolReg,
        a: BytesReg,
        b: BytesReg,
    },
    SelectStr {
        dst: StrReg,
        cond: BoolReg,
        a: StrReg,
        b: StrReg,
    },
    SelectObj {
        dst: ObjReg,
        cond: BoolReg,
        a: ObjReg,
        b: ObjReg,
    },
    SelectAgg {
        dst: AggReg,
        cond: BoolReg,
        a: AggReg,
        b: AggReg,
    },
    SelectFunc {
        dst: FuncReg,
        cond: BoolReg,
        a: FuncReg,
        b: FuncReg,
    },

    Br {
        cond: BoolReg,
        pc_true: u32,
        pc_false: u32,
    },
    Jmp {
        pc_target: u32,
    },

    Call {
        eff_out: UnitReg,
        func_id: FuncId,
        eff_in: UnitReg,
        args: Vec<VReg>,
        rets: Vec<VReg>,
    },
    Ret {
        eff_in: UnitReg,
        rets: Vec<VReg>,
    },

    HostCall {
        eff_out: UnitReg,
        host_sig: HostSigId,
        eff_in: UnitReg,
        args: Vec<VReg>,
        rets: Vec<VReg>,
    },

    TupleNew {
        dst: AggReg,
        values: Vec<VReg>,
    },
    TupleGet {
        dst: VReg,
        tuple: AggReg,
        index: u32,
    },

    StructNew {
        dst: AggReg,
        type_id: TypeId,
        values: Vec<VReg>,
    },
    StructGet {
        dst: VReg,
        st: AggReg,
        field_index: u32,
    },

    ArrayNew {
        dst: AggReg,
        elem_type_id: ElemTypeId,
        len: u32,
        values: Vec<VReg>,
    },
    ArrayLen {
        dst: U64Reg,
        arr: AggReg,
    },
    ArrayGet {
        dst: VReg,
        arr: AggReg,
        index: U64Reg,
    },
    ArrayGetImm {
        dst: VReg,
        arr: AggReg,
        index: u32,
    },

    TupleLen {
        dst: U64Reg,
        tuple: AggReg,
    },
    StructFieldCount {
        dst: U64Reg,
        st: AggReg,
    },

    BytesLen {
        dst: U64Reg,
        bytes: BytesReg,
    },
    StrLen {
        dst: U64Reg,
        s: StrReg,
    },

    I64Div {
        dst: I64Reg,
        a: I64Reg,
        b: I64Reg,
    },
    I64Rem {
        dst: I64Reg,
        a: I64Reg,
        b: I64Reg,
    },
    U64Div {
        dst: U64Reg,
        a: U64Reg,
        b: U64Reg,
    },
    U64Rem {
        dst: U64Reg,
        a: U64Reg,
        b: U64Reg,
    },

    I64ToF64 {
        dst: F64Reg,
        a: I64Reg,
    },
    U64ToF64 {
        dst: F64Reg,
        a: U64Reg,
    },
    F64ToI64 {
        dst: I64Reg,
        a: F64Reg,
    },
    F64ToU64 {
        dst: U64Reg,
        a: F64Reg,
    },

    DecToI64 {
        dst: I64Reg,
        a: DecimalReg,
    },
    DecToU64 {
        dst: U64Reg,
        a: DecimalReg,
    },
    I64ToDec {
        dst: DecimalReg,
        a: I64Reg,
        scale: u8,
    },
    U64ToDec {
        dst: DecimalReg,
        a: U64Reg,
        scale: u8,
    },

    BytesEq {
        dst: BoolReg,
        a: BytesReg,
        b: BytesReg,
    },
    StrEq {
        dst: BoolReg,
        a: StrReg,
        b: StrReg,
    },
    BytesConcat {
        dst: BytesReg,
        a: BytesReg,
        b: BytesReg,
    },
    StrConcat {
        dst: StrReg,
        a: StrReg,
        b: StrReg,
    },
    BytesGet {
        dst: U64Reg,
        bytes: BytesReg,
        index: U64Reg,
    },
    BytesGetImm {
        dst: U64Reg,
        bytes: BytesReg,
        index: u32,
    },
    BytesSlice {
        dst: BytesReg,
        bytes: BytesReg,
        start: U64Reg,
        end: U64Reg,
    },
    StrSlice {
        dst: StrReg,
        s: StrReg,
        start: U64Reg,
        end: U64Reg,
    },
    StrToBytes {
        dst: BytesReg,
        s: StrReg,
    },
    BytesToStr {
        dst: StrReg,
        bytes: BytesReg,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VerifiedFunction {
    pub(crate) byte_len: u32,
    pub(crate) reg_layout: RegLayout,
    pub(crate) instrs: Vec<VerifiedDecodedInstr>,
}

impl VerifiedFunction {
    pub(crate) fn instr_ix_at_pc(&self, pc: u32) -> Option<usize> {
        self.instrs.binary_search_by_key(&pc, |di| di.offset).ok()
    }

    pub(crate) fn fetch_at_ix(&self, ix: usize) -> Option<(u8, &VerifiedInstr, u32, u32)> {
        let di = self.instrs.get(ix)?;
        let next_pc = self
            .instrs
            .get(ix + 1)
            .map(|n| n.offset)
            .unwrap_or(self.byte_len);
        Some((di.opcode, &di.instr, di.offset, next_pc))
    }
}

/// Returns the set of virtual registers written by `instr`.
pub(crate) fn instr_writes(instr: &Instr) -> Vec<u32> {
    let mut out = Vec::new();
    match instr {
        Instr::Nop
        | Instr::Trap { .. }
        | Instr::Br { .. }
        | Instr::Jmp { .. }
        | Instr::Ret { .. } => {}
        Instr::Mov { dst, .. }
        | Instr::ConstUnit { dst }
        | Instr::ConstBool { dst, .. }
        | Instr::ConstI64 { dst, .. }
        | Instr::ConstU64 { dst, .. }
        | Instr::ConstF64 { dst, .. }
        | Instr::ConstDecimal { dst, .. }
        | Instr::ConstPool { dst, .. }
        | Instr::DecAdd { dst, .. }
        | Instr::DecSub { dst, .. }
        | Instr::DecMul { dst, .. }
        | Instr::F64Add { dst, .. }
        | Instr::F64Sub { dst, .. }
        | Instr::F64Mul { dst, .. }
        | Instr::F64Div { dst, .. }
        | Instr::I64Add { dst, .. }
        | Instr::I64Sub { dst, .. }
        | Instr::I64Mul { dst, .. }
        | Instr::U64Add { dst, .. }
        | Instr::U64Sub { dst, .. }
        | Instr::U64Mul { dst, .. }
        | Instr::U64And { dst, .. }
        | Instr::U64Or { dst, .. }
        | Instr::U64Xor { dst, .. }
        | Instr::U64Shl { dst, .. }
        | Instr::U64Shr { dst, .. }
        | Instr::I64Eq { dst, .. }
        | Instr::I64Lt { dst, .. }
        | Instr::U64Eq { dst, .. }
        | Instr::U64Lt { dst, .. }
        | Instr::U64Gt { dst, .. }
        | Instr::U64Le { dst, .. }
        | Instr::U64Ge { dst, .. }
        | Instr::BoolNot { dst, .. }
        | Instr::BoolAnd { dst, .. }
        | Instr::BoolOr { dst, .. }
        | Instr::BoolXor { dst, .. }
        | Instr::I64And { dst, .. }
        | Instr::I64Or { dst, .. }
        | Instr::I64Xor { dst, .. }
        | Instr::I64Shl { dst, .. }
        | Instr::I64Shr { dst, .. }
        | Instr::I64Gt { dst, .. }
        | Instr::I64Le { dst, .. }
        | Instr::I64Ge { dst, .. }
        | Instr::F64Eq { dst, .. }
        | Instr::F64Lt { dst, .. }
        | Instr::F64Gt { dst, .. }
        | Instr::F64Le { dst, .. }
        | Instr::F64Ge { dst, .. }
        | Instr::U64ToI64 { dst, .. }
        | Instr::I64ToU64 { dst, .. }
        | Instr::Select { dst, .. }
        | Instr::TupleNew { dst, .. }
        | Instr::TupleGet { dst, .. }
        | Instr::StructNew { dst, .. }
        | Instr::StructGet { dst, .. }
        | Instr::ArrayNew { dst, .. }
        | Instr::ArrayLen { dst, .. }
        | Instr::ArrayGet { dst, .. }
        | Instr::ArrayGetImm { dst, .. }
        | Instr::TupleLen { dst, .. }
        | Instr::StructFieldCount { dst, .. }
        | Instr::BytesLen { dst, .. }
        | Instr::StrLen { dst, .. }
        | Instr::I64Div { dst, .. }
        | Instr::I64Rem { dst, .. }
        | Instr::U64Div { dst, .. }
        | Instr::U64Rem { dst, .. }
        | Instr::I64ToF64 { dst, .. }
        | Instr::U64ToF64 { dst, .. }
        | Instr::F64ToI64 { dst, .. }
        | Instr::F64ToU64 { dst, .. }
        | Instr::DecToI64 { dst, .. }
        | Instr::DecToU64 { dst, .. }
        | Instr::I64ToDec { dst, .. }
        | Instr::U64ToDec { dst, .. }
        | Instr::BytesEq { dst, .. }
        | Instr::StrEq { dst, .. }
        | Instr::BytesConcat { dst, .. }
        | Instr::StrConcat { dst, .. }
        | Instr::BytesGet { dst, .. }
        | Instr::BytesGetImm { dst, .. }
        | Instr::BytesSlice { dst, .. }
        | Instr::StrSlice { dst, .. }
        | Instr::StrToBytes { dst, .. }
        | Instr::BytesToStr { dst, .. } => out.push(*dst),
        Instr::Call { eff_out, rets, .. } | Instr::HostCall { eff_out, rets, .. } => {
            out.push(*eff_out);
            out.extend(rets.iter().copied());
        }
    }
    out
}
