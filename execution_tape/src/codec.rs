// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Operand-level bytecode codec.
//!
//! This module provides a minimal encode/decode API in terms of [`Opcode`] plus typed operands.
//! It intentionally does not expose the internal decoded instruction IR (`bytecode::Instr`).
//!
//! Encoding and decoding are validated against the JSON-derived opcode metadata:
//! - operand kinds (reg vs imm vs ids),
//! - and operand encodings (ULEB vs fixed-width, etc.).

extern crate alloc;

use alloc::vec::Vec;
use core::fmt;

use crate::format::{
    DecodeError as FormatDecodeError, Reader, write_sleb128_i64, write_uleb128_u64,
};
use crate::opcode::{Opcode, OperandEncoding, OperandKind};
use crate::program::{ConstId, ElemTypeId, HostSigId, TypeId};
use crate::value::FuncId;

/// An operand value for encoding an instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operand<'a> {
    /// A single virtual register.
    Reg(u32),
    /// A list of virtual registers.
    RegList(&'a [u32]),
    /// A bytecode PC (byte offset).
    Pc(u32),

    /// An immediate `bool`.
    ImmBool(bool),
    /// An immediate `u8`.
    ImmU8(u8),
    /// An immediate `u32`.
    ImmU32(u32),
    /// An immediate `i64`.
    ImmI64(i64),
    /// An immediate `u64`.
    ImmU64(u64),

    /// A constant pool index.
    ConstId(ConstId),
    /// A function index.
    FuncId(FuncId),
    /// A host signature index.
    HostSigId(HostSigId),
    /// A struct type index.
    TypeId(TypeId),
    /// An array element type index.
    ElemTypeId(ElemTypeId),
}

impl Operand<'_> {
    /// Returns the operand kind corresponding to this value.
    #[must_use]
    pub fn kind(self) -> OperandKind {
        match self {
            Self::Reg(_) => OperandKind::Reg,
            Self::RegList(_) => OperandKind::RegList,
            Self::Pc(_) => OperandKind::Pc,
            Self::ImmBool(_) => OperandKind::ImmBool,
            Self::ImmU8(_) => OperandKind::ImmU8,
            Self::ImmU32(_) => OperandKind::ImmU32,
            Self::ImmI64(_) => OperandKind::ImmI64,
            Self::ImmU64(_) => OperandKind::ImmU64,
            Self::ConstId(_) => OperandKind::ConstId,
            Self::FuncId(_) => OperandKind::FuncId,
            Self::HostSigId(_) => OperandKind::HostSigId,
            Self::TypeId(_) => OperandKind::TypeId,
            Self::ElemTypeId(_) => OperandKind::ElemTypeId,
        }
    }
}

/// A decoded operand value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodedOperand {
    /// A single virtual register.
    Reg(u32),
    /// A list of virtual registers.
    RegList(Vec<u32>),
    /// A bytecode PC (byte offset).
    Pc(u32),

    /// An immediate `bool`.
    ImmBool(bool),
    /// An immediate `u8`.
    ImmU8(u8),
    /// An immediate `u32`.
    ImmU32(u32),
    /// An immediate `i64`.
    ImmI64(i64),
    /// An immediate `u64`.
    ImmU64(u64),

    /// A constant pool index.
    ConstId(ConstId),
    /// A function index.
    FuncId(FuncId),
    /// A host signature index.
    HostSigId(HostSigId),
    /// A struct type index.
    TypeId(TypeId),
    /// An array element type index.
    ElemTypeId(ElemTypeId),
}

impl DecodedOperand {
    /// Returns the operand kind corresponding to this value.
    #[must_use]
    pub fn kind(&self) -> OperandKind {
        match self {
            Self::Reg(_) => OperandKind::Reg,
            Self::RegList(_) => OperandKind::RegList,
            Self::Pc(_) => OperandKind::Pc,
            Self::ImmBool(_) => OperandKind::ImmBool,
            Self::ImmU8(_) => OperandKind::ImmU8,
            Self::ImmU32(_) => OperandKind::ImmU32,
            Self::ImmI64(_) => OperandKind::ImmI64,
            Self::ImmU64(_) => OperandKind::ImmU64,
            Self::ConstId(_) => OperandKind::ConstId,
            Self::FuncId(_) => OperandKind::FuncId,
            Self::HostSigId(_) => OperandKind::HostSigId,
            Self::TypeId(_) => OperandKind::TypeId,
            Self::ElemTypeId(_) => OperandKind::ElemTypeId,
        }
    }
}

/// A decoded instruction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedInstruction {
    /// Opcode for this instruction.
    pub opcode: Opcode,
    /// Operands for this instruction.
    pub operands: Vec<DecodedOperand>,
    /// Total instruction byte length (including opcode byte).
    pub byte_len: usize,
}

/// Encoding failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EncodeError {
    /// Operand count mismatch for an opcode.
    ArityMismatch {
        /// Opcode being encoded.
        opcode: Opcode,
        /// Expected operand count.
        expected: usize,
        /// Actual operand count.
        actual: usize,
    },
    /// Operand kind mismatch at a particular operand index.
    KindMismatch {
        /// Opcode being encoded.
        opcode: Opcode,
        /// Operand index.
        index: usize,
        /// Expected kind.
        expected: OperandKind,
        /// Provided kind.
        got: OperandKind,
    },
    /// Operand kind/encoding pair is not supported by this codec.
    UnsupportedEncoding {
        /// Opcode being encoded.
        opcode: Opcode,
        /// Operand index.
        index: usize,
        /// Expected encoding.
        encoding: OperandEncoding,
    },
    /// A value was too large to encode for this operand encoding.
    OutOfBounds,
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ArityMismatch {
                opcode,
                expected,
                actual,
            } => write!(
                f,
                "arity mismatch for {opcode:?}: expected {expected}, got {actual}"
            ),
            Self::KindMismatch {
                opcode,
                index,
                expected,
                got,
            } => write!(
                f,
                "operand {index} kind mismatch for {opcode:?}: expected {expected:?}, got {got:?}"
            ),
            Self::UnsupportedEncoding {
                opcode,
                index,
                encoding,
            } => write!(
                f,
                "operand {index} encoding unsupported for {opcode:?}: {encoding:?}"
            ),
            Self::OutOfBounds => write!(f, "out of bounds"),
        }
    }
}

impl core::error::Error for EncodeError {}

/// Decoding failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// Byte stream was malformed.
    Format(FormatDecodeError),
    /// Unknown opcode byte.
    UnknownOpcode {
        /// The unrecognized opcode byte.
        opcode: u8,
    },
    /// Operand kind/encoding pair is not supported by this codec.
    UnsupportedEncoding {
        /// Opcode being decoded.
        opcode: Opcode,
        /// Operand index.
        index: usize,
        /// Expected kind.
        kind: OperandKind,
        /// Expected encoding.
        encoding: OperandEncoding,
    },
    /// A decoded size was too large to represent.
    OutOfBounds,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Format(e) => write!(f, "decode error: {e}"),
            Self::UnknownOpcode { opcode } => write!(f, "unknown opcode {opcode:#04x}"),
            Self::UnsupportedEncoding {
                opcode,
                index,
                kind,
                encoding,
            } => write!(
                f,
                "unsupported operand encoding for {opcode:?} operand {index}: {kind:?}/{encoding:?}"
            ),
            Self::OutOfBounds => write!(f, "out of bounds"),
        }
    }
}

impl core::error::Error for DecodeError {}

impl From<FormatDecodeError> for DecodeError {
    fn from(e: FormatDecodeError) -> Self {
        Self::Format(e)
    }
}

/// Encodes a single instruction into `out`.
pub fn encode_instruction(
    opcode: Opcode,
    operands: &[Operand<'_>],
    out: &mut Vec<u8>,
) -> Result<(), EncodeError> {
    let schema = opcode.operands();
    if operands.len() != schema.len() {
        return Err(EncodeError::ArityMismatch {
            opcode,
            expected: schema.len(),
            actual: operands.len(),
        });
    }

    out.push(opcode as u8);
    for (i, (operand, spec)) in operands.iter().copied().zip(schema.iter()).enumerate() {
        let got_kind = operand.kind();
        if got_kind != spec.kind {
            return Err(EncodeError::KindMismatch {
                opcode,
                index: i,
                expected: spec.kind,
                got: got_kind,
            });
        }

        match (operand, spec.kind, spec.encoding) {
            (Operand::Reg(r), OperandKind::Reg, OperandEncoding::RegU32Uleb) => {
                write_uleb128_u64(out, u64::from(r));
            }
            (
                Operand::RegList(rs),
                OperandKind::RegList,
                OperandEncoding::RegListU32UlebCountThenRegs,
            ) => {
                let n: u32 = rs.len().try_into().map_err(|_| EncodeError::OutOfBounds)?;
                write_uleb128_u64(out, u64::from(n));
                for &r in rs {
                    write_uleb128_u64(out, u64::from(r));
                }
            }
            (Operand::Pc(pc), OperandKind::Pc, OperandEncoding::U32Uleb)
            | (Operand::ImmU32(pc), OperandKind::ImmU32, OperandEncoding::U32Uleb) => {
                write_uleb128_u64(out, u64::from(pc));
            }
            (Operand::ImmBool(b), OperandKind::ImmBool, OperandEncoding::BoolU8) => {
                out.push(u8::from(b));
            }
            (Operand::ImmU8(v), OperandKind::ImmU8, OperandEncoding::U8Raw) => out.push(v),
            (Operand::ImmI64(v), OperandKind::ImmI64, OperandEncoding::I64Sleb) => {
                write_sleb128_i64(out, v);
            }
            (Operand::ImmU64(v), OperandKind::ImmU64, OperandEncoding::U64Uleb) => {
                write_uleb128_u64(out, v);
            }
            (Operand::ImmU64(v), OperandKind::ImmU64, OperandEncoding::U64Le) => {
                out.extend_from_slice(&v.to_le_bytes());
            }

            (Operand::ConstId(id), OperandKind::ConstId, OperandEncoding::U32Uleb) => {
                write_uleb128_u64(out, u64::from(id.0));
            }
            (Operand::FuncId(id), OperandKind::FuncId, OperandEncoding::U32Uleb) => {
                write_uleb128_u64(out, u64::from(id.0));
            }
            (Operand::HostSigId(id), OperandKind::HostSigId, OperandEncoding::U32Uleb) => {
                write_uleb128_u64(out, u64::from(id.0));
            }
            (Operand::TypeId(id), OperandKind::TypeId, OperandEncoding::U32Uleb) => {
                write_uleb128_u64(out, u64::from(id.0));
            }
            (Operand::ElemTypeId(id), OperandKind::ElemTypeId, OperandEncoding::U32Uleb) => {
                write_uleb128_u64(out, u64::from(id.0));
            }

            _ => {
                return Err(EncodeError::UnsupportedEncoding {
                    opcode,
                    index: i,
                    encoding: spec.encoding,
                });
            }
        }
    }
    Ok(())
}

fn read_u32_uleb(r: &mut Reader<'_>) -> Result<u32, DecodeError> {
    let v = r.read_uleb128_u64()?;
    u32::try_from(v).map_err(|_| DecodeError::OutOfBounds)
}

/// Decodes a single instruction from the start of `bytes`.
pub fn decode_instruction(bytes: &[u8]) -> Result<DecodedInstruction, DecodeError> {
    let mut r = Reader::new(bytes);
    let op_b = r.read_u8()?;
    let opcode = Opcode::from_u8(op_b).ok_or(DecodeError::UnknownOpcode { opcode: op_b })?;
    let schema = opcode.operands();

    let mut operands: Vec<DecodedOperand> = Vec::with_capacity(schema.len());
    for (i, spec) in schema.iter().enumerate() {
        let operand = match (spec.kind, spec.encoding) {
            (OperandKind::Reg, OperandEncoding::RegU32Uleb) => {
                DecodedOperand::Reg(read_u32_uleb(&mut r)?)
            }
            (OperandKind::RegList, OperandEncoding::RegListU32UlebCountThenRegs) => {
                let n = read_u32_uleb(&mut r)? as usize;
                let mut regs = Vec::with_capacity(n);
                for _ in 0..n {
                    regs.push(read_u32_uleb(&mut r)?);
                }
                DecodedOperand::RegList(regs)
            }
            (OperandKind::Pc, OperandEncoding::U32Uleb) => {
                DecodedOperand::Pc(read_u32_uleb(&mut r)?)
            }
            (OperandKind::ImmBool, OperandEncoding::BoolU8) => {
                DecodedOperand::ImmBool(r.read_u8()? != 0)
            }
            (OperandKind::ImmU8, OperandEncoding::U8Raw) => DecodedOperand::ImmU8(r.read_u8()?),
            (OperandKind::ImmU32, OperandEncoding::U32Uleb) => {
                DecodedOperand::ImmU32(read_u32_uleb(&mut r)?)
            }
            (OperandKind::ImmI64, OperandEncoding::I64Sleb) => {
                DecodedOperand::ImmI64(r.read_sleb128_i64()?)
            }
            (OperandKind::ImmU64, OperandEncoding::U64Uleb) => {
                DecodedOperand::ImmU64(r.read_uleb128_u64()?)
            }
            (OperandKind::ImmU64, OperandEncoding::U64Le) => {
                DecodedOperand::ImmU64(r.read_u64_le()?)
            }

            (OperandKind::ConstId, OperandEncoding::U32Uleb) => {
                DecodedOperand::ConstId(ConstId(read_u32_uleb(&mut r)?))
            }
            (OperandKind::FuncId, OperandEncoding::U32Uleb) => {
                DecodedOperand::FuncId(FuncId(read_u32_uleb(&mut r)?))
            }
            (OperandKind::HostSigId, OperandEncoding::U32Uleb) => {
                DecodedOperand::HostSigId(HostSigId(read_u32_uleb(&mut r)?))
            }
            (OperandKind::TypeId, OperandEncoding::U32Uleb) => {
                DecodedOperand::TypeId(TypeId(read_u32_uleb(&mut r)?))
            }
            (OperandKind::ElemTypeId, OperandEncoding::U32Uleb) => {
                DecodedOperand::ElemTypeId(ElemTypeId(read_u32_uleb(&mut r)?))
            }

            (kind, encoding) => {
                return Err(DecodeError::UnsupportedEncoding {
                    opcode,
                    index: i,
                    kind,
                    encoding,
                });
            }
        };
        operands.push(operand);
    }

    Ok(DecodedInstruction {
        opcode,
        operands,
        byte_len: r.offset(),
    })
}

/// Decodes a bytecode stream into a list of operand-level instructions.
pub fn decode_all(bytes: &[u8]) -> Result<Vec<DecodedInstruction>, DecodeError> {
    let mut out: Vec<DecodedInstruction> = Vec::new();
    let mut offset: usize = 0;
    while offset < bytes.len() {
        let di = decode_instruction(&bytes[offset..])?;
        if di.byte_len == 0 {
            return Err(DecodeError::OutOfBounds);
        }
        offset = offset
            .checked_add(di.byte_len)
            .ok_or(DecodeError::OutOfBounds)?;
        out.push(di);
    }
    Ok(out)
}

/// Canonicalizes a byte stream by decoding all instructions and re-encoding them.
///
/// This is useful for normalizing non-canonical (but accepted) encodings like ULEB128 values with
/// redundant continuation bytes.
pub fn canonicalize(bytes: &[u8]) -> Result<Vec<u8>, DecodeError> {
    let decoded = decode_all(bytes)?;
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());

    for di in &decoded {
        let mut operands: Vec<Operand<'_>> = Vec::with_capacity(di.operands.len());
        for o in &di.operands {
            operands.push(match o {
                DecodedOperand::Reg(r) => Operand::Reg(*r),
                DecodedOperand::RegList(rs) => Operand::RegList(rs.as_slice()),
                DecodedOperand::Pc(pc) => Operand::Pc(*pc),
                DecodedOperand::ImmBool(b) => Operand::ImmBool(*b),
                DecodedOperand::ImmU8(v) => Operand::ImmU8(*v),
                DecodedOperand::ImmU32(v) => Operand::ImmU32(*v),
                DecodedOperand::ImmI64(v) => Operand::ImmI64(*v),
                DecodedOperand::ImmU64(v) => Operand::ImmU64(*v),
                DecodedOperand::ConstId(id) => Operand::ConstId(*id),
                DecodedOperand::FuncId(id) => Operand::FuncId(*id),
                DecodedOperand::HostSigId(id) => Operand::HostSigId(*id),
                DecodedOperand::TypeId(id) => Operand::TypeId(*id),
                DecodedOperand::ElemTypeId(id) => Operand::ElemTypeId(*id),
            });
        }

        encode_instruction(di.opcode, &operands, &mut out).map_err(|_| DecodeError::OutOfBounds)?;
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn encode_decode_roundtrip_smoke() {
        let mut bytes: Vec<u8> = Vec::new();

        encode_instruction(Opcode::Nop, &[], &mut bytes).unwrap();
        encode_instruction(Opcode::Trap, &[Operand::ImmU32(7)], &mut bytes).unwrap();
        encode_instruction(
            Opcode::ConstF64,
            &[Operand::Reg(1), Operand::ImmU64(0x3ff0_0000_0000_0000)],
            &mut bytes,
        )
        .unwrap();
        encode_instruction(
            Opcode::ArrayNew,
            &[
                Operand::Reg(1),
                Operand::ElemTypeId(ElemTypeId(0)),
                Operand::RegList(&[2, 3]),
            ],
            &mut bytes,
        )
        .unwrap();

        let decoded = decode_all(&bytes).unwrap();
        assert_eq!(decoded.len(), 4);
        assert_eq!(decoded[0].opcode, Opcode::Nop);
        assert_eq!(decoded[1].operands, vec![DecodedOperand::ImmU32(7)]);
        assert_eq!(
            decoded[2].operands,
            vec![
                DecodedOperand::Reg(1),
                DecodedOperand::ImmU64(0x3ff0_0000_0000_0000)
            ]
        );
        assert_eq!(decoded[3].opcode, Opcode::ArrayNew);
        assert_eq!(
            decoded[3].operands,
            vec![
                DecodedOperand::Reg(1),
                DecodedOperand::ElemTypeId(ElemTypeId(0)),
                DecodedOperand::RegList(vec![2, 3])
            ]
        );
    }

    #[test]
    fn encode_rejects_kind_mismatch() {
        let mut out = Vec::new();
        let err = encode_instruction(
            Opcode::Mov,
            &[Operand::ImmU32(1), Operand::Reg(2)],
            &mut out,
        )
        .unwrap_err();
        assert!(matches!(err, EncodeError::KindMismatch { .. }));
    }

    #[test]
    fn canonicalize_normalizes_noncanonical_uleb() {
        let mut canonical: Vec<u8> = Vec::new();
        encode_instruction(Opcode::ConstUnit, &[Operand::Reg(0)], &mut canonical).unwrap();
        assert_eq!(canonical.as_slice(), &[Opcode::ConstUnit as u8, 0x00]);

        let noncanonical = vec![Opcode::ConstUnit as u8, 0x80, 0x00];
        let out = canonicalize(&noncanonical).unwrap();
        assert_eq!(out, canonical);

        // Idempotent.
        let out2 = canonicalize(&out).unwrap();
        assert_eq!(out2, out);
    }
}
