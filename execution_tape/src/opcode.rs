// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Opcode byte values for the v1 instruction set.
//!
//! This is the single authoritative mapping between instruction names and their encoded opcode
//! bytes. Other modules (`asm`, `bytecode`) should avoid hardcoding numeric literals for known
//! opcodes.

/// Bytecode opcode byte for the v1 instruction set.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Opcode {
    /// No-op.
    Nop = 0x00,
    /// Register move (`dst = src`).
    Mov = 0x01,
    /// Trap unconditionally.
    Trap = 0x02,

    /// `dst = ()`.
    ConstUnit = 0x10,
    /// `dst = bool`.
    ConstBool = 0x11,
    /// `dst = i64`.
    ConstI64 = 0x12,
    /// `dst = u64`.
    ConstU64 = 0x13,
    /// `dst = f64` (encoded as raw IEEE bits).
    ConstF64 = 0x14,
    /// `dst = Decimal { mantissa, scale }`.
    ConstDecimal = 0x15,
    /// `dst = const_pool[idx]`.
    ConstPool = 0x16,

    /// `dst = a + b` (`Decimal`).
    DecAdd = 0x17,
    /// `dst = a - b` (`Decimal`).
    DecSub = 0x18,
    /// `dst = a * b` (`Decimal`).
    DecMul = 0x19,

    /// `dst = a + b` (`f64`).
    F64Add = 0x1A,
    /// `dst = a - b` (`f64`).
    F64Sub = 0x1B,
    /// `dst = a * b` (`f64`).
    F64Mul = 0x1C,

    /// `dst = a + b` (`i64`).
    I64Add = 0x20,
    /// `dst = a - b` (`i64`).
    I64Sub = 0x21,
    /// `dst = a * b` (`i64`).
    I64Mul = 0x22,

    /// `dst = a + b` (`u64`).
    U64Add = 0x23,
    /// `dst = a - b` (`u64`).
    U64Sub = 0x24,
    /// `dst = a * b` (`u64`).
    U64Mul = 0x25,
    /// `dst = a & b` (`u64`).
    U64And = 0x26,
    /// `dst = a | b` (`u64`).
    U64Or = 0x27,

    /// `dst = (a == b)` (`i64` -> `bool`).
    I64Eq = 0x28,
    /// `dst = (a < b)` (`i64` -> `bool`).
    I64Lt = 0x29,
    /// `dst = (a == b)` (`u64` -> `bool`).
    U64Eq = 0x2A,
    /// `dst = (a < b)` (`u64` -> `bool`).
    U64Lt = 0x2B,
    /// `dst = a ^ b` (`u64`).
    U64Xor = 0x2C,
    /// `dst = a << (b & 63)` (`u64`).
    U64Shl = 0x2D,
    /// `dst = a >> (b & 63)` (`u64`).
    U64Shr = 0x2E,
    /// `dst = (a > b)` (`u64` -> `bool`).
    U64Gt = 0x2F,

    /// `dst = !a` (`bool`).
    BoolNot = 0x30,
    /// `dst = (a <= b)` (`u64` -> `bool`).
    U64Le = 0x31,
    /// `dst = (a >= b)` (`u64` -> `bool`).
    U64Ge = 0x32,
    /// `dst = a & b` (`i64`).
    I64And = 0x33,
    /// `dst = u64_to_i64(a)` (traps on overflow).
    U64ToI64 = 0x34,
    /// `dst = i64_to_u64(a)` (traps on negative).
    I64ToU64 = 0x35,
    /// `dst = a | b` (`i64`).
    I64Or = 0x36,
    /// `dst = a ^ b` (`i64`).
    I64Xor = 0x37,
    /// `dst = if cond { a } else { b }`.
    Select = 0x38,
    /// `dst = (a > b)` (`i64` -> `bool`).
    I64Gt = 0x39,
    /// `dst = (a <= b)` (`i64` -> `bool`).
    I64Le = 0x3A,
    /// `dst = (a >= b)` (`i64` -> `bool`).
    I64Ge = 0x3B,
    /// `dst = a << (b & 63)` (`i64`).
    I64Shl = 0x3C,
    /// `dst = a >> (b & 63)` (`i64`).
    I64Shr = 0x3D,

    /// Conditional branch.
    Br = 0x40,
    /// Unconditional jump.
    Jmp = 0x41,

    /// Function call.
    Call = 0x50,
    /// Return from function.
    Ret = 0x51,
    /// Host function call.
    HostCall = 0x52,

    /// Allocate a tuple aggregate.
    TupleNew = 0x60,
    /// Read tuple element at an immediate index.
    TupleGet = 0x61,
    /// Allocate a struct aggregate.
    StructNew = 0x62,
    /// Read struct field at an immediate index.
    StructGet = 0x63,
    /// Allocate an array aggregate.
    ArrayNew = 0x64,
    /// Read array length.
    ArrayLen = 0x65,
    /// Read array element at an index register.
    ArrayGet = 0x66,
    /// Read tuple length.
    TupleLen = 0x67,
    /// Read struct field count.
    StructFieldCount = 0x68,
    /// Read array element at an immediate index.
    ArrayGetImm = 0x69,
    /// Read byte-string length.
    BytesLen = 0x6A,
    /// Read UTF-8 string length (in bytes).
    StrLen = 0x6B,
    /// `dst = a / b` (`i64`).
    I64Div = 0x6C,
    /// `dst = a % b` (`i64`).
    I64Rem = 0x6D,
    /// `dst = a / b` (`u64`).
    U64Div = 0x6E,
    /// `dst = a % b` (`u64`).
    U64Rem = 0x6F,
    /// `dst = (a as f64)` (`i64` -> `f64`).
    I64ToF64 = 0x70,
    /// `dst = (a as f64)` (`u64` -> `f64`).
    U64ToF64 = 0x71,
    /// `dst = trunc(a)` (`f64` -> `i64`).
    F64ToI64 = 0x72,
    /// `dst = trunc(a)` (`f64` -> `u64`).
    F64ToU64 = 0x73,
    /// `dst = dec_to_i64(a)` (traps if `scale != 0`).
    DecToI64 = 0x74,
    /// `dst = dec_to_u64(a)` (traps if `scale != 0`).
    DecToU64 = 0x75,
    /// `dst = i64_to_dec(a, scale)`.
    I64ToDec = 0x76,
    /// `dst = u64_to_dec(a, scale)`.
    U64ToDec = 0x77,
    /// `dst = (a == b)` (`bytes` -> `bool`).
    BytesEq = 0x78,
    /// `dst = (a == b)` (`str` -> `bool`).
    StrEq = 0x79,
    /// `dst = bytes_concat(a, b)`.
    BytesConcat = 0x7A,
    /// `dst = str_concat(a, b)`.
    StrConcat = 0x7B,
    /// `dst = bytes[index]` (traps on OOB).
    BytesGet = 0x7C,
    /// `dst = bytes[index]` with immediate index (traps on OOB).
    BytesGetImm = 0x7D,
    /// `dst = bytes[start..end]` (traps on invalid range).
    BytesSlice = 0x7E,
    /// `dst = s[start..end]` (traps on invalid range/non-boundary).
    StrSlice = 0x7F,
    /// `dst = s.as_bytes()`.
    StrToBytes = 0x80,
    /// `dst = String::from_utf8(bytes)` (traps on invalid UTF-8).
    BytesToStr = 0x81,

    /// `dst = a / b` (`f64`).
    F64Div = 0x82,
    /// `dst = (a == b)` (`f64` -> `bool`).
    F64Eq = 0x83,
    /// `dst = (a < b)` (`f64` -> `bool`).
    F64Lt = 0x84,
    /// `dst = (a > b)` (`f64` -> `bool`).
    F64Gt = 0x85,
    /// `dst = (a <= b)` (`f64` -> `bool`).
    F64Le = 0x86,
    /// `dst = (a >= b)` (`f64` -> `bool`).
    F64Ge = 0x87,

    /// `dst = a & b` (`bool`).
    BoolAnd = 0x88,
    /// `dst = a | b` (`bool`).
    BoolOr = 0x89,
    /// `dst = a ^ b` (`bool`).
    BoolXor = 0x8A,

    /// `dst = -a` (`f64`).
    F64Neg = 0x8B,
    /// `dst = abs(a)` (`f64`).
    F64Abs = 0x8C,
    /// `dst = min(a, b)` (`f64`, NaN-propagating).
    F64Min = 0x8D,
    /// `dst = max(a, b)` (`f64`, NaN-propagating).
    F64Max = 0x8E,
    /// `dst = min_num(a, b)` (`f64`, number-favoring).
    F64MinNum = 0x8F,
    /// `dst = max_num(a, b)` (`f64`, number-favoring).
    F64MaxNum = 0x90,
    /// `dst = a % b` (`f64`).
    F64Rem = 0x91,
    /// `dst = f64_to_bits(a)` (`f64` -> `u64`).
    F64ToBits = 0x92,
    /// `dst = f64_from_bits(a)` (`u64` -> `f64`).
    F64FromBits = 0x93,
}

impl Opcode {
    /// Decodes an opcode byte.
    #[must_use]
    pub fn from_u8(b: u8) -> Option<Self> {
        Some(match b {
            0x00 => Self::Nop,
            0x01 => Self::Mov,
            0x02 => Self::Trap,

            0x10 => Self::ConstUnit,
            0x11 => Self::ConstBool,
            0x12 => Self::ConstI64,
            0x13 => Self::ConstU64,
            0x14 => Self::ConstF64,
            0x15 => Self::ConstDecimal,
            0x16 => Self::ConstPool,

            0x17 => Self::DecAdd,
            0x18 => Self::DecSub,
            0x19 => Self::DecMul,

            0x1A => Self::F64Add,
            0x1B => Self::F64Sub,
            0x1C => Self::F64Mul,

            0x20 => Self::I64Add,
            0x21 => Self::I64Sub,
            0x22 => Self::I64Mul,

            0x23 => Self::U64Add,
            0x24 => Self::U64Sub,
            0x25 => Self::U64Mul,
            0x26 => Self::U64And,
            0x27 => Self::U64Or,

            0x28 => Self::I64Eq,
            0x29 => Self::I64Lt,
            0x2A => Self::U64Eq,
            0x2B => Self::U64Lt,
            0x2C => Self::U64Xor,
            0x2D => Self::U64Shl,
            0x2E => Self::U64Shr,
            0x2F => Self::U64Gt,

            0x30 => Self::BoolNot,
            0x31 => Self::U64Le,
            0x32 => Self::U64Ge,
            0x33 => Self::I64And,
            0x34 => Self::U64ToI64,
            0x35 => Self::I64ToU64,
            0x36 => Self::I64Or,
            0x37 => Self::I64Xor,
            0x38 => Self::Select,
            0x39 => Self::I64Gt,
            0x3A => Self::I64Le,
            0x3B => Self::I64Ge,
            0x3C => Self::I64Shl,
            0x3D => Self::I64Shr,

            0x40 => Self::Br,
            0x41 => Self::Jmp,

            0x50 => Self::Call,
            0x51 => Self::Ret,
            0x52 => Self::HostCall,

            0x60 => Self::TupleNew,
            0x61 => Self::TupleGet,
            0x62 => Self::StructNew,
            0x63 => Self::StructGet,
            0x64 => Self::ArrayNew,
            0x65 => Self::ArrayLen,
            0x66 => Self::ArrayGet,
            0x67 => Self::TupleLen,
            0x68 => Self::StructFieldCount,
            0x69 => Self::ArrayGetImm,
            0x6A => Self::BytesLen,
            0x6B => Self::StrLen,
            0x6C => Self::I64Div,
            0x6D => Self::I64Rem,
            0x6E => Self::U64Div,
            0x6F => Self::U64Rem,
            0x70 => Self::I64ToF64,
            0x71 => Self::U64ToF64,
            0x72 => Self::F64ToI64,
            0x73 => Self::F64ToU64,
            0x74 => Self::DecToI64,
            0x75 => Self::DecToU64,
            0x76 => Self::I64ToDec,
            0x77 => Self::U64ToDec,
            0x78 => Self::BytesEq,
            0x79 => Self::StrEq,
            0x7A => Self::BytesConcat,
            0x7B => Self::StrConcat,
            0x7C => Self::BytesGet,
            0x7D => Self::BytesGetImm,
            0x7E => Self::BytesSlice,
            0x7F => Self::StrSlice,
            0x80 => Self::StrToBytes,
            0x81 => Self::BytesToStr,

            0x82 => Self::F64Div,
            0x83 => Self::F64Eq,
            0x84 => Self::F64Lt,
            0x85 => Self::F64Gt,
            0x86 => Self::F64Le,
            0x87 => Self::F64Ge,

            0x88 => Self::BoolAnd,
            0x89 => Self::BoolOr,
            0x8A => Self::BoolXor,

            0x8B => Self::F64Neg,
            0x8C => Self::F64Abs,
            0x8D => Self::F64Min,
            0x8E => Self::F64Max,
            0x8F => Self::F64MinNum,
            0x90 => Self::F64MaxNum,
            0x91 => Self::F64Rem,
            0x92 => Self::F64ToBits,
            0x93 => Self::F64FromBits,

            _ => return None,
        })
    }

    /// Returns `true` if this opcode terminates the current basic block.
    #[must_use]
    pub fn is_terminator(self) -> bool {
        matches!(self, Self::Br | Self::Jmp | Self::Ret | Self::Trap)
    }
}

#[cfg(test)]
mod tests {
    use super::Opcode;

    #[test]
    fn opcode_values_are_stable() {
        assert_eq!(Opcode::Call as u8, 0x50);
        assert_eq!(Opcode::Ret as u8, 0x51);
        assert_eq!(Opcode::BoolNot as u8, 0x30);
        assert_eq!(Opcode::BoolAnd as u8, 0x88);
        assert_eq!(Opcode::F64Neg as u8, 0x8B);
    }

    #[test]
    fn opcode_terminator_classification() {
        assert!(Opcode::Br.is_terminator());
        assert!(Opcode::Jmp.is_terminator());
        assert!(Opcode::Ret.is_terminator());
        assert!(Opcode::Trap.is_terminator());
        assert!(!Opcode::Nop.is_terminator());
    }
}
