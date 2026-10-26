// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

use crate::format::DecodeError;
use alloc::vec::Vec;

/// Reads an unsigned LEB128 integer as `u64`, updating `offset`.
pub fn read_uleb128_u64(bytes: &[u8], offset: &mut usize) -> Result<u64, DecodeError> {
    let mut value: u64 = 0;
    let mut shift: u32 = 0;
    for i in 0..10 {
        let b = *bytes.get(*offset).ok_or(DecodeError::UnexpectedEof)?;
        *offset = offset.checked_add(1).ok_or(DecodeError::OutOfBounds)?;

        let payload = b & 0x7f;
        if i == 9 && payload > 1 {
            return Err(DecodeError::InvalidVarint);
        }
        value |= u64::from(payload) << shift;
        if (b & 0x80) == 0 {
            return Ok(value);
        }
        shift = shift.checked_add(7).ok_or(DecodeError::InvalidVarint)?;
    }
    Err(DecodeError::InvalidVarint)
}

/// Reads a signed LEB128 integer as `i64`, updating `offset`.
pub fn read_sleb128_i64(bytes: &[u8], offset: &mut usize) -> Result<i64, DecodeError> {
    let mut value: i64 = 0;
    let mut shift: u32 = 0;
    let mut last: u8 = 0;

    for i in 0..10 {
        let b = *bytes.get(*offset).ok_or(DecodeError::UnexpectedEof)?;
        *offset = offset.checked_add(1).ok_or(DecodeError::OutOfBounds)?;
        last = b;

        let payload = b & 0x7f;
        if i == 9 && payload != 0x00 && payload != 0x7f {
            return Err(DecodeError::InvalidVarint);
        }
        value |= i64::from(payload) << shift;
        shift = shift.checked_add(7).ok_or(DecodeError::InvalidVarint)?;

        if (b & 0x80) == 0 {
            break;
        }
    }

    if (last & 0x80) != 0 {
        return Err(DecodeError::InvalidVarint);
    }

    // Sign extend if the sign bit of the last byte was set.
    if shift < 64 && (last & 0x40) != 0 {
        value |= (!0_i64) << shift;
    }

    Ok(value)
}

/// Writes an unsigned LEB128 integer.
pub fn write_uleb128_u64(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let mut b = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            b |= 0x80;
        }
        out.push(b);
        if value == 0 {
            break;
        }
    }
}

/// Writes a signed LEB128 integer.
pub fn write_sleb128_i64(out: &mut Vec<u8>, mut value: i64) {
    loop {
        let b = (value & 0x7f) as u8;
        let sign = (b & 0x40) != 0;
        value >>= 7;

        let done = (value == 0 && !sign) || (value == -1 && sign);
        if done {
            out.push(b);
            break;
        }
        out.push(b | 0x80);
    }
}

/// Reads a signed LEB128 integer as `i128`, updating `offset`.
pub fn read_sleb128_i128(bytes: &[u8], offset: &mut usize) -> Result<i128, DecodeError> {
    let mut value: i128 = 0;
    let mut shift: u32 = 0;
    let mut last: u8 = 0;

    for i in 0..19 {
        let b = *bytes.get(*offset).ok_or(DecodeError::UnexpectedEof)?;
        *offset = offset.checked_add(1).ok_or(DecodeError::OutOfBounds)?;
        last = b;

        let payload = b & 0x7f;
        if i == 18 && !matches!(payload, 0x00 | 0x01 | 0x7e | 0x7f) {
            return Err(DecodeError::InvalidVarint);
        }
        value |= i128::from(payload) << shift;
        shift = shift.checked_add(7).ok_or(DecodeError::InvalidVarint)?;

        if (b & 0x80) == 0 {
            break;
        }
    }

    if (last & 0x80) != 0 {
        return Err(DecodeError::InvalidVarint);
    }

    // Sign extend if the sign bit of the last byte was set.
    if shift < 128 && (last & 0x40) != 0 {
        value |= (!0_i128) << shift;
    }

    Ok(value)
}

/// Writes a signed LEB128 integer.
pub fn write_sleb128_i128(out: &mut Vec<u8>, mut value: i128) {
    loop {
        let b = (value & 0x7f) as u8;
        let sign = (b & 0x40) != 0;
        value >>= 7;

        let done = (value == 0 && !sign) || (value == -1 && sign);
        if done {
            out.push(b);
            break;
        }
        out.push(b | 0x80);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_signed_roundtrip_at_every_bit_boundary() {
        let mut values = alloc::vec![i128::MIN, i128::MAX, 0, -1];
        for bit in 0..127 {
            let value = 1_i128 << bit;
            values.extend([value - 1, value, -value, -value - 1]);
        }
        for value in values {
            let mut bytes = Vec::new();
            write_sleb128_i128(&mut bytes, value);
            assert!(bytes.len() <= 19);
            let mut offset = 0;
            assert_eq!(read_sleb128_i128(&bytes, &mut offset), Ok(value));
            assert_eq!(offset, bytes.len());
            for end in 0..bytes.len() {
                assert_eq!(
                    read_sleb128_i128(&bytes[..end], &mut 0),
                    Err(DecodeError::UnexpectedEof)
                );
            }
        }
    }

    #[test]
    fn wide_signed_final_byte_checks_sign_extension_and_termination() {
        for last in 0..=255_u8 {
            let mut bytes = [0x80; 20];
            bytes[18] = last;
            let mut offset = 0;
            let decoded = read_sleb128_i128(&bytes, &mut offset);
            assert_eq!(offset, 19);
            assert_eq!(decoded.is_ok(), matches!(last, 0x00 | 0x01 | 0x7e | 0x7f));
        }
        assert_eq!(read_sleb128_i128(&[0x80, 0], &mut 0), Ok(0));
        assert_eq!(read_sleb128_i128(&[0xff, 0x7f], &mut 0), Ok(-1));
    }

    #[test]
    fn uleb128_roundtrip() {
        let values = [0, 1, 2, 127, 128, 129, 16_384, u64::MAX];
        for &v in &values {
            let mut buf = Vec::new();
            write_uleb128_u64(&mut buf, v);
            let mut off = 0;
            let back = read_uleb128_u64(&buf, &mut off).unwrap();
            assert_eq!(back, v);
            assert_eq!(off, buf.len());
        }
    }

    #[test]
    fn sleb128_roundtrip() {
        let values = [0, 1, -1, 63, 64, -64, -65, i64::MIN, i64::MAX];
        for &v in &values {
            let mut buf = Vec::new();
            write_sleb128_i64(&mut buf, v);
            let mut off = 0;
            let back = read_sleb128_i64(&buf, &mut off).unwrap();
            assert_eq!(back, v);
            assert_eq!(off, buf.len());
        }
    }

    #[test]
    fn uleb128_accepts_non_canonical_zero() {
        let buf = [0x80, 0x00];
        let mut off = 0;
        let v = read_uleb128_u64(&buf, &mut off).unwrap();
        assert_eq!(v, 0);
        assert_eq!(off, buf.len());
    }

    #[test]
    fn uleb128_rejects_overflow_in_10th_byte() {
        // 10 bytes, but the last payload uses bits beyond u64's remaining capacity.
        let buf = [0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x02];
        let mut off = 0;
        assert_eq!(
            read_uleb128_u64(&buf, &mut off).unwrap_err(),
            DecodeError::InvalidVarint
        );
    }

    #[test]
    fn sleb128_accepts_non_canonical_zero() {
        let buf = [0x80, 0x00];
        let mut off = 0;
        let v = read_sleb128_i64(&buf, &mut off).unwrap();
        assert_eq!(v, 0);
        assert_eq!(off, buf.len());
    }

    #[test]
    fn sleb128_rejects_overflow_in_10th_byte() {
        // 10 bytes, but the last payload isn't a valid sign-extension for i64.
        let buf = [0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x01];
        let mut off = 0;
        assert_eq!(
            read_sleb128_i64(&buf, &mut off).unwrap_err(),
            DecodeError::InvalidVarint
        );
    }
}
