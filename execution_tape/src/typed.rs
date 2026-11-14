// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Typed, verifier-produced execution artifacts.
//!
//! v1 bytecode uses a single virtual register index space (`r0..rN`). To make VM loads/stores
//! monomorphic and avoid runtime tag checks, the verifier assigns each virtual register a
//! [`RegClass`] and a class-local index. It also produces a typed instruction stream whose
//! operands are class-specific newtypes.

use alloc::vec::Vec;

use crate::program::{CallSigId, HostSigId, RegionId, TagId};
use crate::program::{ConstId, ValueType};
use crate::program::{ElemTypeId, SpanId, TypeId};
use crate::value::{Decimal, DecimalRounding, FuncId};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum RegClass {
    Unit,
    Bool,
    I64,
    U64,
    F64,
    I32,
    U32,
    F32,
    Decimal,
    Bytes,
    Str,
    Obj,
    Agg,
    Func,
    Closure,
    Exn,
    Cont,
}

impl RegClass {
    /// The number of classes, for tables indexed by `class as usize`.
    pub(crate) const COUNT: usize = Self::Cont as usize + 1;

    pub(crate) fn of(t: ValueType) -> Self {
        match t {
            ValueType::Unit => Self::Unit,
            ValueType::Bool => Self::Bool,
            ValueType::I64 => Self::I64,
            ValueType::U64 => Self::U64,
            ValueType::F64 => Self::F64,
            ValueType::I32 => Self::I32,
            ValueType::U32 => Self::U32,
            ValueType::F32 => Self::F32,
            ValueType::Decimal => Self::Decimal,
            ValueType::Bytes => Self::Bytes,
            ValueType::Str => Self::Str,
            ValueType::Obj(_) => Self::Obj,
            ValueType::Agg => Self::Agg,
            ValueType::Func => Self::Func,
            ValueType::Closure => Self::Closure,
            ValueType::Exn => Self::Exn,
            ValueType::Cont => Self::Cont,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct BoolReg(pub u32);
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct I64Reg(pub u32);
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct U64Reg(pub u32);
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct F64Reg(pub u32);
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct I32Reg(pub u32);
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct U32Reg(pub u32);
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct F32Reg(pub u32);
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
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct ClosureReg(pub u32);
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExnReg(pub u32);
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct ContReg(pub u32);

/// A typed register reference (class + class-local index).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum VReg {
    /// A verified Unit value; it has no runtime location or payload.
    Unit,
    Bool(BoolReg),
    I64(I64Reg),
    U64(U64Reg),
    F64(F64Reg),
    I32(I32Reg),
    U32(U32Reg),
    F32(F32Reg),
    Decimal(DecimalReg),
    Bytes(BytesReg),
    Str(StrReg),
    Obj(ObjReg),
    Agg(AggReg),
    Func(FuncReg),
    Closure(ClosureReg),
    Exn(ExnReg),
    Cont(ContReg),
}

impl VReg {
    /// A function-local ordering for preparation indexes, never a wire identity.
    pub(crate) fn key(self) -> (u8, u32) {
        match self {
            Self::Unit => unreachable!("Unit transfers are omitted before copy scheduling"),
            Self::Bool(reg) => (1, reg.0),
            Self::I64(reg) => (2, reg.0),
            Self::U64(reg) => (3, reg.0),
            Self::F64(reg) => (4, reg.0),
            Self::Decimal(reg) => (5, reg.0),
            Self::Bytes(reg) => (6, reg.0),
            Self::Str(reg) => (7, reg.0),
            Self::Obj(reg) => (8, reg.0),
            Self::Agg(reg) => (9, reg.0),
            Self::Func(reg) => (10, reg.0),
            Self::Closure(reg) => (11, reg.0),
            Self::Exn(reg) => (12, reg.0),
            Self::I32(reg) => (13, reg.0),
            Self::U32(reg) => (14, reg.0),
            Self::F32(reg) => (15, reg.0),
            Self::Cont(reg) => (16, reg.0),
        }
    }
}

/// A slice of typed registers stored in a per-function operand pool.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct VRegSlice {
    pub(crate) start: usize,
    pub(crate) len: usize,
}

impl VRegSlice {
    #[inline(always)]
    pub(crate) fn as_slice(self, pool: &[VReg]) -> &[VReg] {
        let start = self.start;
        let end = start + self.len;
        debug_assert!(end <= pool.len(), "VRegSlice out of bounds");
        &pool[start..end]
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RegCounts {
    /// Logical source Unit declarations, without runtime payload storage or copy scratch.
    pub(crate) unit: usize,
    pub(crate) bools: usize,
    pub(crate) i64s: usize,
    pub(crate) u64s: usize,
    pub(crate) f64s: usize,
    /// One bank of 32-bit slots shared by `i32`, `u32` and `f32` registers (stored as bits).
    pub(crate) w32s: usize,
    pub(crate) decimals: usize,
    pub(crate) bytes: usize,
    pub(crate) strs: usize,
    pub(crate) objs: usize,
    pub(crate) aggs: usize,
    pub(crate) funcs: usize,
    pub(crate) closures: usize,
    pub(crate) exns: usize,
    pub(crate) conts: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RegLayout {
    pub(crate) counts: RegCounts,
    pub(crate) arg_regs: Vec<VReg>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExecDecoded {
    pub(crate) offset: u32,
    pub(crate) end: u32,
    pub(crate) instr: ExecInstr,
}

/// A verifier-produced instruction stream with typed register operands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ExecInstr {
    /// No payload action. Source instruction, fuel and observations are preserved.
    Nop,
    /// Trap unconditionally with a trap code.
    Trap {
        code: u32,
    },
    /// Raise `tag` with its payload registers in tag order.
    Throw {
        tag: TagId,
        args: VRegSlice,
    },
    /// Rethrow a caught exception reference.
    ThrowRef {
        exn: ExnReg,
    },
    /// Create an unstarted continuation of a function value.
    ContNewFunc {
        dst: ContReg,
        call_sig: CallSigId,
        func: FuncReg,
    },
    /// Create an unstarted continuation of a closure, binding its environment.
    ContNewClosure {
        dst: ContReg,
        call_sig: CallSigId,
        func: ClosureReg,
    },
    /// Bind the leading inputs of a continuation.
    ContBind {
        dst: ContReg,
        from_sig: CallSigId,
        to_sig: CallSigId,
        cont: ContReg,
        args: VRegSlice,
    },
    /// Release a continuation without resuming it.
    ContDrop {
        cont: ContReg,
    },
    /// Run a continuation on top of this activation.
    Resume {
        call_sig: CallSigId,
        cont: ContReg,
        args: VRegSlice,
        rets: VRegSlice,
    },
    /// Resume a continuation by raising an exception at its suspension point.
    ResumeThrow {
        call_sig: CallSigId,
        cont: ContReg,
        tag: TagId,
        args: VRegSlice,
        rets: VRegSlice,
    },
    /// Suspend to the nearest handler of a tag.
    Suspend {
        tag: TagId,
        args: VRegSlice,
        rets: VRegSlice,
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
    MovI32 {
        dst: I32Reg,
        src: I32Reg,
    },
    MovU32 {
        dst: U32Reg,
        src: U32Reg,
    },
    MovF32 {
        dst: F32Reg,
        src: F32Reg,
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
    MovClosure {
        dst: ClosureReg,
        src: ClosureReg,
    },
    MovExn {
        dst: ExnReg,
        src: ExnReg,
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
        value: Decimal,
    },
    ConstFunc {
        dst: FuncReg,
        func_id: FuncId,
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
    DecDiv {
        dst: DecimalReg,
        a: DecimalReg,
        b: DecimalReg,
        scale: U64Reg,
        rounding: DecimalRounding,
    },
    DecRescale {
        dst: DecimalReg,
        a: DecimalReg,
        scale: U64Reg,
        rounding: DecimalRounding,
    },
    DecScale {
        dst: U64Reg,
        a: DecimalReg,
    },
    DecCmp {
        dst: I64Reg,
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
    F64Neg {
        dst: F64Reg,
        a: F64Reg,
    },
    F64Abs {
        dst: F64Reg,
        a: F64Reg,
    },
    F64Floor {
        dst: F64Reg,
        a: F64Reg,
    },
    F64Ceil {
        dst: F64Reg,
        a: F64Reg,
    },
    F64Trunc {
        dst: F64Reg,
        a: F64Reg,
    },
    F64Nearest {
        dst: F64Reg,
        a: F64Reg,
    },
    F64Sqrt {
        dst: F64Reg,
        a: F64Reg,
    },
    F64RoundHalfAway {
        dst: F64Reg,
        a: F64Reg,
    },
    F64Clamp {
        dst: F64Reg,
        a: F64Reg,
        lo: F64Reg,
        hi: F64Reg,
    },
    F64Pow {
        dst: F64Reg,
        a: F64Reg,
        b: F64Reg,
    },
    F64Exp {
        dst: F64Reg,
        a: F64Reg,
    },
    F64Ln {
        dst: F64Reg,
        a: F64Reg,
    },
    F64Sin {
        dst: F64Reg,
        a: F64Reg,
    },
    F64Cos {
        dst: F64Reg,
        a: F64Reg,
    },
    F64Atan2 {
        dst: F64Reg,
        y: F64Reg,
        x: F64Reg,
    },
    F64CopySign {
        dst: F64Reg,
        a: F64Reg,
        b: F64Reg,
    },
    F64Min {
        dst: F64Reg,
        a: F64Reg,
        b: F64Reg,
    },
    F64Max {
        dst: F64Reg,
        a: F64Reg,
        b: F64Reg,
    },
    F64MinNum {
        dst: F64Reg,
        a: F64Reg,
        b: F64Reg,
    },
    F64MaxNum {
        dst: F64Reg,
        a: F64Reg,
        b: F64Reg,
    },
    F64Rem {
        dst: F64Reg,
        a: F64Reg,
        b: F64Reg,
    },
    F64ToBits {
        dst: U64Reg,
        a: F64Reg,
    },
    F64FromBits {
        dst: F64Reg,
        a: U64Reg,
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
    I64AddOvf {
        dst: I64Reg,
        overflow: BoolReg,
        a: I64Reg,
        b: I64Reg,
    },
    I64SubOvf {
        dst: I64Reg,
        overflow: BoolReg,
        a: I64Reg,
        b: I64Reg,
    },
    I64MulOvf {
        dst: I64Reg,
        overflow: BoolReg,
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
    U64AddOvf {
        dst: U64Reg,
        overflow: BoolReg,
        a: U64Reg,
        b: U64Reg,
    },
    U64SubOvf {
        dst: U64Reg,
        overflow: BoolReg,
        a: U64Reg,
        b: U64Reg,
    },
    U64MulOvf {
        dst: U64Reg,
        overflow: BoolReg,
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
    U64Clz {
        dst: U64Reg,
        a: U64Reg,
    },
    U64Ctz {
        dst: U64Reg,
        a: U64Reg,
    },
    U64Popcnt {
        dst: U64Reg,
        a: U64Reg,
    },
    U64Rotl {
        dst: U64Reg,
        a: U64Reg,
        b: U64Reg,
    },
    U64Rotr {
        dst: U64Reg,
        a: U64Reg,
        b: U64Reg,
    },
    ConstI32 {
        dst: I32Reg,
        imm: i32,
    },
    ConstU32 {
        dst: U32Reg,
        imm: u32,
    },
    ConstF32 {
        dst: F32Reg,
        bits: u32,
    },
    I32Add {
        dst: I32Reg,
        a: I32Reg,
        b: I32Reg,
    },
    I32Sub {
        dst: I32Reg,
        a: I32Reg,
        b: I32Reg,
    },
    I32Mul {
        dst: I32Reg,
        a: I32Reg,
        b: I32Reg,
    },
    I32Div {
        dst: I32Reg,
        a: I32Reg,
        b: I32Reg,
    },
    I32Rem {
        dst: I32Reg,
        a: I32Reg,
        b: I32Reg,
    },
    I32AddOvf {
        dst: I32Reg,
        overflow: BoolReg,
        a: I32Reg,
        b: I32Reg,
    },
    I32SubOvf {
        dst: I32Reg,
        overflow: BoolReg,
        a: I32Reg,
        b: I32Reg,
    },
    I32MulOvf {
        dst: I32Reg,
        overflow: BoolReg,
        a: I32Reg,
        b: I32Reg,
    },
    I32And {
        dst: I32Reg,
        a: I32Reg,
        b: I32Reg,
    },
    I32Or {
        dst: I32Reg,
        a: I32Reg,
        b: I32Reg,
    },
    I32Xor {
        dst: I32Reg,
        a: I32Reg,
        b: I32Reg,
    },
    I32Shl {
        dst: I32Reg,
        a: I32Reg,
        b: I32Reg,
    },
    I32Shr {
        dst: I32Reg,
        a: I32Reg,
        b: I32Reg,
    },
    I32Eq {
        dst: BoolReg,
        a: I32Reg,
        b: I32Reg,
    },
    I32Lt {
        dst: BoolReg,
        a: I32Reg,
        b: I32Reg,
    },
    I32Gt {
        dst: BoolReg,
        a: I32Reg,
        b: I32Reg,
    },
    I32Le {
        dst: BoolReg,
        a: I32Reg,
        b: I32Reg,
    },
    I32Ge {
        dst: BoolReg,
        a: I32Reg,
        b: I32Reg,
    },
    U32Add {
        dst: U32Reg,
        a: U32Reg,
        b: U32Reg,
    },
    U32Sub {
        dst: U32Reg,
        a: U32Reg,
        b: U32Reg,
    },
    U32Mul {
        dst: U32Reg,
        a: U32Reg,
        b: U32Reg,
    },
    U32Div {
        dst: U32Reg,
        a: U32Reg,
        b: U32Reg,
    },
    U32Rem {
        dst: U32Reg,
        a: U32Reg,
        b: U32Reg,
    },
    U32AddOvf {
        dst: U32Reg,
        overflow: BoolReg,
        a: U32Reg,
        b: U32Reg,
    },
    U32SubOvf {
        dst: U32Reg,
        overflow: BoolReg,
        a: U32Reg,
        b: U32Reg,
    },
    U32MulOvf {
        dst: U32Reg,
        overflow: BoolReg,
        a: U32Reg,
        b: U32Reg,
    },
    U32And {
        dst: U32Reg,
        a: U32Reg,
        b: U32Reg,
    },
    U32Or {
        dst: U32Reg,
        a: U32Reg,
        b: U32Reg,
    },
    U32Xor {
        dst: U32Reg,
        a: U32Reg,
        b: U32Reg,
    },
    U32Shl {
        dst: U32Reg,
        a: U32Reg,
        b: U32Reg,
    },
    U32Shr {
        dst: U32Reg,
        a: U32Reg,
        b: U32Reg,
    },
    U32Clz {
        dst: U32Reg,
        a: U32Reg,
    },
    U32Ctz {
        dst: U32Reg,
        a: U32Reg,
    },
    U32Popcnt {
        dst: U32Reg,
        a: U32Reg,
    },
    U32Rotl {
        dst: U32Reg,
        a: U32Reg,
        b: U32Reg,
    },
    U32Rotr {
        dst: U32Reg,
        a: U32Reg,
        b: U32Reg,
    },
    U32Eq {
        dst: BoolReg,
        a: U32Reg,
        b: U32Reg,
    },
    U32Lt {
        dst: BoolReg,
        a: U32Reg,
        b: U32Reg,
    },
    U32Gt {
        dst: BoolReg,
        a: U32Reg,
        b: U32Reg,
    },
    U32Le {
        dst: BoolReg,
        a: U32Reg,
        b: U32Reg,
    },
    U32Ge {
        dst: BoolReg,
        a: U32Reg,
        b: U32Reg,
    },
    F32Add {
        dst: F32Reg,
        a: F32Reg,
        b: F32Reg,
    },
    F32Sub {
        dst: F32Reg,
        a: F32Reg,
        b: F32Reg,
    },
    F32Mul {
        dst: F32Reg,
        a: F32Reg,
        b: F32Reg,
    },
    F32Div {
        dst: F32Reg,
        a: F32Reg,
        b: F32Reg,
    },
    F32Rem {
        dst: F32Reg,
        a: F32Reg,
        b: F32Reg,
    },
    F32Neg {
        dst: F32Reg,
        a: F32Reg,
    },
    F32Abs {
        dst: F32Reg,
        a: F32Reg,
    },
    F32Min {
        dst: F32Reg,
        a: F32Reg,
        b: F32Reg,
    },
    F32Max {
        dst: F32Reg,
        a: F32Reg,
        b: F32Reg,
    },
    F32MinNum {
        dst: F32Reg,
        a: F32Reg,
        b: F32Reg,
    },
    F32MaxNum {
        dst: F32Reg,
        a: F32Reg,
        b: F32Reg,
    },
    F32CopySign {
        dst: F32Reg,
        a: F32Reg,
        b: F32Reg,
    },
    F32Floor {
        dst: F32Reg,
        a: F32Reg,
    },
    F32Ceil {
        dst: F32Reg,
        a: F32Reg,
    },
    F32Trunc {
        dst: F32Reg,
        a: F32Reg,
    },
    F32Nearest {
        dst: F32Reg,
        a: F32Reg,
    },
    F32Sqrt {
        dst: F32Reg,
        a: F32Reg,
    },
    F32Eq {
        dst: BoolReg,
        a: F32Reg,
        b: F32Reg,
    },
    F32Ne {
        dst: BoolReg,
        a: F32Reg,
        b: F32Reg,
    },
    F32Lt {
        dst: BoolReg,
        a: F32Reg,
        b: F32Reg,
    },
    F32Gt {
        dst: BoolReg,
        a: F32Reg,
        b: F32Reg,
    },
    F32Le {
        dst: BoolReg,
        a: F32Reg,
        b: F32Reg,
    },
    F32Ge {
        dst: BoolReg,
        a: F32Reg,
        b: F32Reg,
    },
    F32RoundHalfAway {
        dst: F32Reg,
        a: F32Reg,
    },
    F32Clamp {
        dst: F32Reg,
        a: F32Reg,
        lo: F32Reg,
        hi: F32Reg,
    },
    F32Pow {
        dst: F32Reg,
        a: F32Reg,
        b: F32Reg,
    },
    F32Exp {
        dst: F32Reg,
        a: F32Reg,
    },
    F32Ln {
        dst: F32Reg,
        a: F32Reg,
    },
    F32Sin {
        dst: F32Reg,
        a: F32Reg,
    },
    F32Cos {
        dst: F32Reg,
        a: F32Reg,
    },
    F32Atan2 {
        dst: F32Reg,
        y: F32Reg,
        x: F32Reg,
    },
    I32ToI64 {
        dst: I64Reg,
        a: I32Reg,
    },
    I64ToI32 {
        dst: I32Reg,
        a: I64Reg,
    },
    I64ToI32Wrap {
        dst: I32Reg,
        a: I64Reg,
    },
    U32ToU64 {
        dst: U64Reg,
        a: U32Reg,
    },
    U64ToU32 {
        dst: U32Reg,
        a: U64Reg,
    },
    U64ToU32Wrap {
        dst: U32Reg,
        a: U64Reg,
    },
    U32ToI64 {
        dst: I64Reg,
        a: U32Reg,
    },
    I32ToU32 {
        dst: U32Reg,
        a: I32Reg,
    },
    U32ToI32 {
        dst: I32Reg,
        a: U32Reg,
    },
    I32ToU32Wrap {
        dst: U32Reg,
        a: I32Reg,
    },
    U32ToI32Wrap {
        dst: I32Reg,
        a: U32Reg,
    },
    I64ToU64Wrap {
        dst: U64Reg,
        a: I64Reg,
    },
    U64ToI64Wrap {
        dst: I64Reg,
        a: U64Reg,
    },
    F32ToF64 {
        dst: F64Reg,
        a: F32Reg,
    },
    F64ToF32 {
        dst: F32Reg,
        a: F64Reg,
    },
    F32ToBits {
        dst: U32Reg,
        a: F32Reg,
    },
    F32FromBits {
        dst: F32Reg,
        a: U32Reg,
    },
    F32ToI32 {
        dst: I32Reg,
        a: F32Reg,
    },
    F32ToU32 {
        dst: U32Reg,
        a: F32Reg,
    },
    F32ToI32Sat {
        dst: I32Reg,
        a: F32Reg,
    },
    F32ToU32Sat {
        dst: U32Reg,
        a: F32Reg,
    },
    F64ToI32 {
        dst: I32Reg,
        a: F64Reg,
    },
    F64ToU32 {
        dst: U32Reg,
        a: F64Reg,
    },
    F64ToI32Sat {
        dst: I32Reg,
        a: F64Reg,
    },
    F64ToU32Sat {
        dst: U32Reg,
        a: F64Reg,
    },
    I32ToF32 {
        dst: F32Reg,
        a: I32Reg,
    },
    U32ToF32 {
        dst: F32Reg,
        a: U32Reg,
    },
    I64ToF32 {
        dst: F32Reg,
        a: I64Reg,
    },
    U64ToF32 {
        dst: F32Reg,
        a: U64Reg,
    },
    I32ToF64 {
        dst: F64Reg,
        a: I32Reg,
    },
    U32ToF64 {
        dst: F64Reg,
        a: U32Reg,
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
    F64Ne {
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
    SelectI32 {
        dst: I32Reg,
        cond: BoolReg,
        a: I32Reg,
        b: I32Reg,
    },
    SelectU32 {
        dst: U32Reg,
        cond: BoolReg,
        a: U32Reg,
        b: U32Reg,
    },
    SelectF32 {
        dst: F32Reg,
        cond: BoolReg,
        a: F32Reg,
        b: F32Reg,
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
    SelectClosure {
        dst: ClosureReg,
        cond: BoolReg,
        a: ClosureReg,
        b: ClosureReg,
    },

    Br {
        true_copies: VRegSlice,
        false_copies: VRegSlice,
        cond: BoolReg,
        true_ix: u32,
        false_ix: u32,
    },
    Jmp {
        copies: VRegSlice,
        target_ix: u32,
    },

    Call {
        func_id: FuncId,
        args: VRegSlice,
        rets: VRegSlice,
    },
    Ret {
        rets: VRegSlice,
    },

    HostCall {
        host_sig: HostSigId,
        args: VRegSlice,
        rets: VRegSlice,
    },
    CallIndirectFunc {
        call_sig: CallSigId,
        callee: FuncReg,
        args: VRegSlice,
        rets: VRegSlice,
    },
    CallIndirectClosure {
        call_sig: CallSigId,
        callee: ClosureReg,
        args: VRegSlice,
        rets: VRegSlice,
    },
    ClosureNew {
        dst: ClosureReg,
        func: FuncReg,
        env: AggReg,
    },

    TupleNew {
        dst: AggReg,
        values: VRegSlice,
    },
    TupleGet {
        dst: VReg,
        tuple: AggReg,
        index: u32,
    },

    StructNew {
        dst: AggReg,
        type_id: TypeId,
        values: VRegSlice,
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
        values: VRegSlice,
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
    F64ToI64Sat {
        dst: I64Reg,
        a: F64Reg,
    },
    F64ToU64Sat {
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
pub(crate) struct ExecFunc {
    /// Function-local boundary checks, arguments first, then results.
    pub(crate) contract_checks: Vec<ContractCheck>,
    pub(crate) arg_check_count: u32,
    pub(crate) byte_len: u32,
    pub(crate) reg_layout: RegLayout,
    pub(crate) operands: Vec<VReg>,
    pub(crate) instrs: Vec<ExecDecoded>,
    /// Optional source span id for each decoded instruction index.
    ///
    /// This is precomputed by the verifier from the function span table so VM execution can do
    /// O(1) span lookup in hot paths.
    ///
    pub(crate) span_by_instr_ix: Vec<Option<SpanId>>,
    /// Protected regions in execution-index form; `None` for unprotected functions.
    pub(crate) protection: Option<alloc::boxed::Box<ExecProtection>>,
}

/// Protected regions prepared for unwinding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExecProtection {
    /// Sorted, disjoint execution-index ranges and their innermost region.
    pub(crate) runs: Vec<(u32, u32, RegionId)>,
    /// Parent and clause range for each region.
    pub(crate) regions: Vec<(Option<RegionId>, core::ops::Range<u32>)>,
    pub(crate) clauses: Vec<ExecClause>,
}

/// A catch clause with its handler entry and parameter registers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ExecClause {
    /// Caught tag; `None` catches everything.
    pub(crate) tag: Option<TagId>,
    /// Whether the last parameter receives the exception reference.
    pub(crate) binds_exception: bool,
    /// Whether this is an `on` clause, which handles suspensions rather than exceptions.
    pub(crate) suspension: bool,
    /// First executable instruction of the handler block.
    pub(crate) target_ix: u32,
    /// Handler block parameters receiving the payload, in payload order.
    pub(crate) params: VRegSlice,
}

/// A clause selected for an exception raised at one instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ExecCatch {
    pub(crate) region: RegionId,
    pub(crate) clause: u32,
    pub(crate) handler: ExecClause,
}

impl ExecProtection {
    /// Selects the first clause catching `tag` for an exception raised by instruction `ix`.
    /// `catches(local)` says whether this program's tag `local` is the raised tag; catch-all
    /// clauses catch every exception.
    pub(crate) fn catch(&self, ix: usize, catches: impl Fn(TagId) -> bool) -> Option<ExecCatch> {
        self.select(ix, |clause| {
            !clause.suspension && clause.tag.is_none_or(&catches)
        })
    }

    /// Selects the first `on` clause handling a suspension to the tag `handles` accepts, for
    /// a continuation resumed by instruction `ix`.
    pub(crate) fn handle(&self, ix: usize, handles: impl Fn(TagId) -> bool) -> Option<ExecCatch> {
        self.select(ix, |clause| {
            clause.suspension && clause.tag.is_some_and(&handles)
        })
    }

    fn select(&self, ix: usize, selects: impl Fn(&ExecClause) -> bool) -> Option<ExecCatch> {
        let ix = u32::try_from(ix).ok()?;
        let run = self
            .runs
            .partition_point(|&(start, _, _)| start <= ix)
            .checked_sub(1)?;
        let (_, end, mut region) = self.runs[run];
        if ix >= end {
            return None;
        }
        loop {
            let (parent, clauses) = &self.regions[region.0 as usize];
            for (offset, clause) in self.clauses[clauses.start as usize..clauses.end as usize]
                .iter()
                .enumerate()
            {
                if selects(clause) {
                    return Some(ExecCatch {
                        region,
                        clause: u32::try_from(offset).expect("verified clause count"),
                        handler: *clause,
                    });
                }
            }
            region = (*parent)?;
        }
    }
}

impl ExecFunc {
    #[inline(always)]
    pub(crate) fn vregs(&self, s: VRegSlice) -> &[VReg] {
        s.as_slice(&self.operands)
    }

    #[inline]
    pub(crate) fn span_at_ix(&self, ix: usize) -> Option<SpanId> {
        self.span_by_instr_ix.get(ix).copied().flatten()
    }

    #[inline]
    pub(crate) fn pc_at_ix(&self, ix: usize) -> Option<u32> {
        self.instrs.get(ix).map(|di| di.offset)
    }

    pub(crate) fn fetch_at_ix(&self, ix: usize) -> Option<(&ExecInstr, u32, u32)> {
        let di = self.instrs.get(ix)?;
        Some((&di.instr, di.offset, di.end))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ContractCheck {
    pub(crate) index: u32,
    pub(crate) contract: crate::contract::ContractId,
}
