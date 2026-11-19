// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Decoding one UTF-8 scalar at a byte position, for `obj.utf8_next` and `obj.utf8_prev`.
//!
//! The bytes come from a host and are not trusted to be UTF-8: a position where no well-formed
//! scalar starts (or ends) decodes to nothing, whether it is out of bounds, inside a scalar, or
//! at malformed bytes. Well-formed means what [`core::str::from_utf8`] accepts: no overlong
//! encodings, surrogates or values past `U+10FFFF`.

/// The scalar starting at `at` and its length in bytes, or `None` when no well-formed scalar
/// starts there.
#[inline(always)]
fn decode_at(bytes: &[u8], at: usize) -> Option<(u32, u8)> {
    let lead = *bytes.get(at)?;
    if lead < 0x80 {
        return Some((u32::from(lead), 1));
    }
    let width: u8 = match lead {
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => return None,
    };
    let encoded = bytes.get(at..at.checked_add(usize::from(width))?)?;
    // `from_utf8` rejects overlong encodings, surrogates and values past `U+10FFFF`.
    let scalar = core::str::from_utf8(encoded).ok()?.chars().next()?;
    Some((u32::from(scalar), width))
}

/// The scalar starting at byte `pos` and the position after it.
#[inline(always)]
pub(crate) fn next(bytes: &[u8], pos: u64) -> Option<(u32, u64)> {
    let (scalar, width) = decode_at(bytes, usize::try_from(pos).ok()?)?;
    Some((scalar, pos + u64::from(width)))
}

/// The scalar ending at byte `pos` and the position where it starts.
#[inline(always)]
pub(crate) fn prev(bytes: &[u8], pos: u64) -> Option<(u32, u64)> {
    let end = usize::try_from(pos).ok()?;
    if end == 0 || end > bytes.len() {
        return None;
    }
    // A scalar is a lead byte and at most three continuation bytes.
    let start = (end.saturating_sub(4)..end)
        .rev()
        .find(|&at| bytes[at] & 0xC0 != 0x80)?;
    let (scalar, width) = decode_at(bytes, start)?;
    (start + usize::from(width) == end).then(|| (scalar, pos - u64::from(width)))
}

#[cfg(test)]
mod tests {
    use super::{next, prev};
    use alloc::vec::Vec;

    /// What `next` should answer: the first scalar of the longest valid UTF-8 prefix at `pos`.
    fn oracle_next(bytes: &[u8], pos: usize) -> Option<(u32, u64)> {
        let rest = bytes.get(pos..)?;
        let valid = match core::str::from_utf8(rest) {
            Ok(text) => text,
            Err(error) => core::str::from_utf8(&rest[..error.valid_up_to()]).unwrap(),
        };
        let c = valid.chars().next()?;
        Some((u32::from(c), (pos + c.len_utf8()) as u64))
    }

    /// What `prev` should answer: a scalar whose encoding is valid and ends exactly at `pos`.
    fn oracle_prev(bytes: &[u8], pos: usize) -> Option<(u32, u64)> {
        (1..=4.min(pos)).find_map(|width| {
            let start = pos - width;
            let text = core::str::from_utf8(bytes.get(start..pos)?).ok()?;
            let mut chars = text.chars();
            let c = chars.next()?;
            chars.next().is_none().then(|| (u32::from(c), start as u64))
        })
    }

    #[test]
    fn every_scalar_round_trips_both_ways() {
        let mut buffer = [0_u8; 6];
        for c in (0..=0x10FFFF_u32).filter_map(char::from_u32) {
            buffer[0] = b'<';
            let len = c.encode_utf8(&mut buffer[1..]).len();
            buffer[1 + len] = b'>';
            let bytes = &buffer[..2 + len];
            let end = (1 + len) as u64;
            assert_eq!(next(bytes, 1), Some((u32::from(c), end)), "{c:?}");
            assert_eq!(prev(bytes, end), Some((u32::from(c), 1)), "{c:?}");
        }
    }

    #[test]
    fn arbitrary_bytes_match_the_standard_library() {
        // Bytes biased toward UTF-8's interesting values, from a fixed linear congruential
        // sequence so failures reproduce.
        const PICKS: [u8; 16] = [
            0x00, 0x41, 0x7F, 0x80, 0x8F, 0x90, 0x9F, 0xA0, 0xBF, 0xC0, 0xC2, 0xE0, 0xED, 0xF0,
            0xF4, 0xF5,
        ];
        let mut state = 0x2545_F491_4F6C_DD1D_u64;
        let mut random = || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            state >> 33
        };
        for _ in 0..20_000 {
            let len = usize::try_from(random() % 9).unwrap();
            let bytes: Vec<u8> = (0..len)
                .map(|_| {
                    let r = random().to_le_bytes();
                    if r[0] & 1 == 0 {
                        PICKS[usize::from(r[1]) % PICKS.len()]
                    } else {
                        r[2]
                    }
                })
                .collect();
            for pos in 0..=len + 1 {
                assert_eq!(
                    next(&bytes, pos as u64),
                    oracle_next(&bytes, pos),
                    "next {bytes:x?} at {pos}"
                );
                assert_eq!(
                    prev(&bytes, pos as u64),
                    oracle_prev(&bytes, pos),
                    "prev {bytes:x?} at {pos}"
                );
            }
        }
    }

    #[test]
    fn positions_past_any_slice_decode_to_nothing() {
        assert_eq!(next(b"a", u64::MAX), None);
        assert_eq!(prev(b"a", u64::MAX), None);
        assert_eq!(next(b"", 0), None);
        assert_eq!(prev(b"", 0), None);
    }
}
