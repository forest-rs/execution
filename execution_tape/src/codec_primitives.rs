// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Shared bytecode encoding/decoding primitives.
//!
//! These helpers centralize the concrete byte encoding used by operands (ULEB/SLEB, fixed-width
//! little-endian `u64`, register lists, etc.). Higher-level codecs (operand-level and internal
//! decoded-IR codecs) should route through these helpers to avoid format drift.

extern crate alloc;

use alloc::vec::Vec;

use crate::format::{DecodeError, Reader, write_sleb128_i64, write_uleb128_u64};

/// Encoding failed because a value did not fit within the draft bytecode constraints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OutOfBounds;

pub(crate) fn read_u32_uleb(r: &mut Reader<'_>) -> Result<u32, DecodeError> {
    let v = r.read_uleb128_u64()?;
    u32::try_from(v).map_err(|_| DecodeError::OutOfBounds)
}

pub(crate) fn read_reg(r: &mut Reader<'_>) -> Result<u32, DecodeError> {
    read_u32_uleb(r)
}

pub(crate) fn read_bool_u8(r: &mut Reader<'_>) -> Result<bool, DecodeError> {
    Ok(r.read_u8()? != 0)
}

pub(crate) fn read_u8_raw(r: &mut Reader<'_>) -> Result<u8, DecodeError> {
    r.read_u8()
}

pub(crate) fn read_i64_sleb(r: &mut Reader<'_>) -> Result<i64, DecodeError> {
    r.read_sleb128_i64()
}

pub(crate) fn read_u64_uleb(r: &mut Reader<'_>) -> Result<u64, DecodeError> {
    r.read_uleb128_u64()
}

pub(crate) fn read_u64_le(r: &mut Reader<'_>) -> Result<u64, DecodeError> {
    r.read_u64_le()
}

pub(crate) fn read_reg_list(r: &mut Reader<'_>) -> Result<Vec<u32>, DecodeError> {
    let n = read_u32_uleb(r)? as usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(read_reg(r)?);
    }
    Ok(out)
}

pub(crate) fn write_u32_uleb(out: &mut Vec<u8>, v: u32) {
    write_uleb128_u64(out, u64::from(v));
}

pub(crate) fn write_reg(out: &mut Vec<u8>, r: u32) {
    write_u32_uleb(out, r);
}

pub(crate) fn write_bool_u8(out: &mut Vec<u8>, v: bool) {
    out.push(u8::from(v));
}

pub(crate) fn write_u8_raw(out: &mut Vec<u8>, v: u8) {
    out.push(v);
}

pub(crate) fn write_i64_sleb(out: &mut Vec<u8>, v: i64) {
    write_sleb128_i64(out, v);
}

pub(crate) fn write_u64_uleb(out: &mut Vec<u8>, v: u64) {
    write_uleb128_u64(out, v);
}

pub(crate) fn write_u64_le(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

pub(crate) fn write_reg_list(out: &mut Vec<u8>, regs: &[u32]) -> Result<(), OutOfBounds> {
    let n: u32 = regs.len().try_into().map_err(|_| OutOfBounds)?;
    write_u32_uleb(out, n);
    for &r in regs {
        write_reg(out, r);
    }
    Ok(())
}
