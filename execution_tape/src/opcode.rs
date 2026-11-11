// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Opcode byte values for the v1 instruction set.
//!
//! This module is a small wrapper around generated opcode tables.

include!("opcodes_gen.rs");

impl Opcode {
    /// Returns the opcode byte value.
    #[must_use]
    pub const fn byte(self) -> u8 {
        self as u8
    }

    /// Parses an opcode from its byte value.
    #[must_use]
    pub fn from_byte(b: u8) -> Option<Self> {
        Self::from_u8(b)
    }
}

#[cfg(test)]
mod tests {
    use super::Opcode;

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
