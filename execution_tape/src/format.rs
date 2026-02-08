// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Encoding/decoding primitives for the `execution_tape` portable format.

mod leb128;

pub use leb128::{read_sleb128_i64, read_uleb128_u64, write_sleb128_i64, write_uleb128_u64};

use alloc::vec::Vec;
use core::fmt;
use core::num::NonZeroU32;

/// A decode error for `execution_tape` binary artifacts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// Input ended unexpectedly.
    UnexpectedEof,
    /// An integer encoding was invalid or overflowed.
    InvalidVarint,
    /// A length/offset was out of bounds.
    OutOfBounds,
    /// A UTF-8 string was invalid.
    InvalidUtf8,
    /// The binary format version is not supported by this decoder.
    UnsupportedVersion {
        /// Major format version.
        major: u16,
        /// Minor format version.
        minor: u16,
    },
    /// A magic header mismatch.
    BadMagic,
    /// An unknown section tag was encountered.
    UnknownSectionTag {
        /// The raw section tag byte.
        tag: u8,
    },
    /// A section was repeated when only one instance is allowed.
    DuplicateSection,
    /// A required section was missing.
    MissingSection {
        /// The required section tag.
        tag: u8,
    },
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedEof => write!(f, "unexpected end of input"),
            Self::InvalidVarint => write!(f, "invalid varint encoding"),
            Self::OutOfBounds => write!(f, "out of bounds"),
            Self::InvalidUtf8 => write!(f, "invalid UTF-8"),
            Self::UnsupportedVersion { major, minor } => {
                write!(f, "unsupported version {major}.{minor}")
            }
            Self::BadMagic => write!(f, "bad magic header"),
            Self::UnknownSectionTag { tag } => write!(f, "unknown section tag {tag}"),
            Self::DuplicateSection => write!(f, "duplicate section"),
            Self::MissingSection { tag } => write!(f, "missing required section {tag}"),
        }
    }
}

impl core::error::Error for DecodeError {}

/// A simple byte reader with bounds checks.
#[derive(Clone, Debug)]
pub struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    /// Creates a reader over `bytes`.
    #[must_use]
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    /// Returns the current cursor offset.
    #[must_use]
    pub fn offset(&self) -> usize {
        self.offset
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], DecodeError> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or(DecodeError::OutOfBounds)?;
        let slice = self
            .bytes
            .get(self.offset..end)
            .ok_or(DecodeError::UnexpectedEof)?;
        self.offset = end;
        Ok(slice)
    }

    /// Reads a `u8`.
    pub fn read_u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }

    /// Reads a little-endian `u16`.
    pub fn read_u16_le(&mut self) -> Result<u16, DecodeError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    /// Reads a little-endian `u32`.
    pub fn read_u32_le(&mut self) -> Result<u32, DecodeError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Reads a little-endian `u64`.
    pub fn read_u64_le(&mut self) -> Result<u64, DecodeError> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    /// Reads an unsigned LEB128 integer as `u64`.
    pub fn read_uleb128_u64(&mut self) -> Result<u64, DecodeError> {
        read_uleb128_u64(self.bytes, &mut self.offset)
    }

    /// Reads an unsigned LEB128 integer as `u32`.
    #[inline(always)]
    pub fn read_uleb128_u32(&mut self) -> Result<u32, DecodeError> {
        let v = self.read_uleb128_u64()?;
        u32::try_from(v).map_err(|_| DecodeError::OutOfBounds)
    }

    /// Reads an unsigned LEB128 integer as a non-zero `u32`.
    ///
    /// Returns [`DecodeError::OutOfBounds`] if the decoded value is `0` or does not fit in `u32`.
    #[inline(always)]
    pub fn read_uleb128_u32_nz(&mut self) -> Result<NonZeroU32, DecodeError> {
        let v = self.read_uleb128_u32()?;
        NonZeroU32::new(v).ok_or(DecodeError::OutOfBounds)
    }

    /// Reads a signed LEB128 integer as `i64`.
    pub fn read_sleb128_i64(&mut self) -> Result<i64, DecodeError> {
        read_sleb128_i64(self.bytes, &mut self.offset)
    }

    /// Reads `len` raw bytes.
    pub fn read_bytes(&mut self, len: usize) -> Result<&'a [u8], DecodeError> {
        self.take(len)
    }

    /// Reads `len` bytes and validates UTF-8.
    pub fn read_str(&mut self, len: usize) -> Result<&'a str, DecodeError> {
        let b = self.take(len)?;
        core::str::from_utf8(b).map_err(|_| DecodeError::InvalidUtf8)
    }
}

/// A simple byte writer.
#[derive(Clone, Debug, Default)]
pub struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    /// Creates an empty writer.
    #[must_use]
    pub fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    /// Returns a reference to the written bytes.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    /// Consumes the writer and returns the underlying byte buffer.
    #[must_use]
    pub fn into_vec(self) -> Vec<u8> {
        self.bytes
    }

    /// Appends a `u8`.
    pub fn write_u8(&mut self, v: u8) {
        self.bytes.push(v);
    }

    /// Appends a little-endian `u16`.
    pub fn write_u16_le(&mut self, v: u16) {
        self.bytes.extend_from_slice(&v.to_le_bytes());
    }

    /// Appends a little-endian `u32`.
    pub fn write_u32_le(&mut self, v: u32) {
        self.bytes.extend_from_slice(&v.to_le_bytes());
    }

    /// Appends a little-endian `u64`.
    pub fn write_u64_le(&mut self, v: u64) {
        self.bytes.extend_from_slice(&v.to_le_bytes());
    }

    /// Appends an unsigned LEB128 integer (`u64`).
    pub fn write_uleb128_u64(&mut self, v: u64) {
        write_uleb128_u64(&mut self.bytes, v);
    }

    /// Appends an unsigned LEB128 integer (`u32`).
    ///
    /// This is a convenience wrapper around [`Writer::write_uleb128_u64`].
    #[inline(always)]
    pub fn write_uleb128_u32(&mut self, v: u32) {
        self.write_uleb128_u64(u64::from(v));
    }

    /// Appends an unsigned LEB128 integer (non-zero `u32`).
    #[inline(always)]
    pub fn write_uleb128_u32_nz(&mut self, v: NonZeroU32) {
        self.write_uleb128_u32(v.get());
    }

    /// Appends a signed LEB128 integer (`i64`).
    pub fn write_sleb128_i64(&mut self, v: i64) {
        write_sleb128_i64(&mut self.bytes, v);
    }

    /// Appends raw bytes.
    pub fn write_bytes(&mut self, b: &[u8]) {
        self.bytes.extend_from_slice(b);
    }
}
