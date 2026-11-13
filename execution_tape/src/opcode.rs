// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Opcode values for the v1 instruction set.
//!
//! This module is a small wrapper around generated opcode tables.
//!
//! An opcode is either a single byte or a [prefix](OpcodePrefix) byte followed by one more byte.
//! Every opcode also has a 16-bit *code* that names it without reference to the byte stream: the
//! byte itself for single-byte opcodes and `prefix << 8 | byte` for prefixed ones. Traces,
//! recordings and diagnostics report opcodes by code.

use alloc::vec::Vec;
use core::fmt;

include!("opcodes_gen.rs");

impl Opcode {
    /// Returns this opcode's code: its byte, or `prefix << 8 | byte` when prefixed.
    ///
    /// Codes below `0x100` are single-byte opcodes. Prefix bytes are never `0x00`, so the two
    /// ranges do not overlap.
    #[must_use]
    pub const fn code(self) -> u16 {
        self as u16
    }

    /// Returns the prefix that introduces this opcode, or `None` for a single-byte opcode.
    #[must_use]
    pub const fn prefix(self) -> Option<OpcodePrefix> {
        OpcodePrefix::from_u8(self.code().to_be_bytes()[0])
    }

    /// Returns the opcode's final byte: the whole opcode when it is single-byte, otherwise the
    /// byte that follows [`Opcode::prefix`].
    #[must_use]
    pub const fn byte(self) -> u8 {
        self.code().to_be_bytes()[1]
    }

    /// Parses a single-byte opcode from its byte value.
    ///
    /// Returns `None` for prefix bytes; see [`Opcode::from_prefixed`].
    #[must_use]
    pub fn from_byte(b: u8) -> Option<Self> {
        Self::from_u8(b)
    }

    /// Returns the number of bytes this opcode occupies in bytecode (1 or 2).
    #[must_use]
    pub const fn encoded_len(self) -> usize {
        if self.prefix().is_some() { 2 } else { 1 }
    }

    /// Appends this opcode's bytes (its prefix, if any, then its byte) to `out`.
    pub fn encode(self, out: &mut Vec<u8>) {
        if let Some(prefix) = self.prefix() {
            out.push(prefix.byte());
        }
        out.push(self.byte());
    }
}

impl OpcodePrefix {
    /// Returns the prefix byte.
    #[must_use]
    pub const fn byte(self) -> u8 {
        self as u8
    }
}

/// Returns the code of the opcode at the start of `bytes`, without checking that it is assigned.
///
/// Reads a second byte only after an assigned prefix. Returns `None` if `bytes` ends first.
#[inline]
pub(crate) fn code_at(bytes: &[u8]) -> Option<u16> {
    let lead = *bytes.first()?;
    if OpcodePrefix::from_u8(lead).is_none() {
        return Some(u16::from(lead));
    }
    Some(u16::from_be_bytes([lead, *bytes.get(1)?]))
}

/// Formats an opcode code as its bytecode bytes, e.g. `0x50` or `0xFC 0x05`.
#[derive(Copy, Clone, Debug)]
pub(crate) struct CodeBytes(pub(crate) u16);

impl fmt::Display for CodeBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0.to_be_bytes() {
            [0, byte] => write!(f, "{byte:#04X}"),
            [prefix, byte] => write!(f, "{prefix:#04X} {byte:#04X}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::format;
    use alloc::vec::Vec;

    use super::{CodeBytes, OPCODE_INFO_BY_BYTE, Opcode, OpcodePrefix, code_at};

    #[test]
    fn opcode_values_are_stable() {
        assert_eq!(Opcode::Call as u8, 0x50);
        assert_eq!(Opcode::Ret as u8, 0x51);
        assert_eq!(Opcode::CallIndirect as u8, 0x54);
        assert_eq!(Opcode::ClosureNew as u8, 0x55);
        assert_eq!(Opcode::BoolNot as u8, 0x30);
        assert_eq!(Opcode::BoolAnd as u8, 0x88);
        assert_eq!(Opcode::F64Neg as u8, 0x8B);
        assert_eq!(Opcode::F64Floor as u8, 0x94);
        assert_eq!(Opcode::F64Sqrt as u8, 0x98);
        assert_eq!(Opcode::F64CopySign as u8, 0x99);
        assert_eq!(Opcode::F64Ne as u8, 0x9C);
        assert_eq!(Opcode::DecCmp as u8, 0x9D);
        assert_eq!(Opcode::DecDiv as u8, 0xA0);
        assert_eq!(Opcode::I64AddOvf as u8, 0xA1);
        assert_eq!(Opcode::U64MulOvf as u8, 0xA6);
        assert_eq!(OpcodePrefix::Extended.byte(), 0xFC);
    }

    #[test]
    fn opcode_terminator_classification() {
        assert!(Opcode::Br.is_terminator());
        assert!(Opcode::Jmp.is_terminator());
        assert!(Opcode::Ret.is_terminator());
        assert!(Opcode::Trap.is_terminator());
        assert!(!Opcode::Nop.is_terminator());
    }

    /// Every assigned code decodes back to its opcode through each entry point.
    #[test]
    fn codes_bytes_and_prefixes_agree() {
        for code in 0..=u16::MAX {
            let Some(op) = Opcode::from_code(code) else {
                continue;
            };
            assert_eq!(op.code(), code, "{op:?}");
            let mut bytes = Vec::new();
            op.encode(&mut bytes);
            assert_eq!(bytes.len(), op.encoded_len(), "{op:?}");
            assert_eq!(code_at(&bytes), Some(code), "{op:?}");
            match op.prefix() {
                None => {
                    assert_eq!(Opcode::from_u8(op.byte()), Some(op), "{op:?}");
                    assert_eq!(&OPCODE_INFO_BY_BYTE[usize::from(op.byte())], op.info());
                }
                Some(prefix) => {
                    assert_eq!(bytes, [prefix.byte(), op.byte()], "{op:?}");
                    assert_eq!(Opcode::from_prefixed(prefix, op.byte()), Some(op));
                    assert_eq!(Opcode::from_u8(prefix.byte()), None);
                }
            }
            assert_ne!(op.mnemonic(), "<invalid>", "{op:?}");
        }
    }

    /// The reserved prefix space never names a single-byte opcode.
    #[test]
    fn prefix_space_is_reserved() {
        for b in 0xF0..=0xFF {
            assert_eq!(Opcode::from_u8(b), None, "{b:#04X}");
        }
        assert_eq!(code_at(&[0xFC]), None);
        assert_eq!(code_at(&[0xFC, 0x7F]), Some(0xFC7F));
        assert_eq!(code_at(&[0xFD, 0x7F]), Some(0x00FD));
        assert_eq!(code_at(&[]), None);
    }

    #[test]
    fn code_bytes_display_shows_the_wire_form() {
        assert_eq!(format!("{}", CodeBytes(0x50)), "0x50");
        assert_eq!(format!("{}", CodeBytes(0xFC05)), "0xFC 0x05");
    }
}
