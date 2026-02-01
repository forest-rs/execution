// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Per-VM arena storage for alloc-backed runtime values.
//!
//! v1 uses simple `Vec`-backed arenas for bytes and strings. Register values store compact
//! handles into these arenas.

use alloc::string::String;
use alloc::vec::Vec;

/// Handle to a byte string stored in a [`ValueArena`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct BytesHandle(pub(crate) u32);

/// Handle to a UTF-8 string stored in a [`ValueArena`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct StrHandle(pub(crate) u32);

/// Arena storage for bytes and strings.
#[derive(Clone, Debug, Default)]
pub(crate) struct ValueArena {
    bytes: Vec<Vec<u8>>,
    strs: Vec<String>,
}

impl ValueArena {
    pub(crate) fn clear(&mut self) {
        self.bytes.clear();
        self.strs.clear();
    }

    pub(crate) fn alloc_bytes(&mut self, bytes: Vec<u8>) -> BytesHandle {
        let idx = u32::try_from(self.bytes.len()).unwrap_or(u32::MAX);
        self.bytes.push(bytes);
        BytesHandle(idx)
    }

    pub(crate) fn alloc_bytes_from_slice(&mut self, bytes: &[u8]) -> BytesHandle {
        self.alloc_bytes(bytes.to_vec())
    }

    pub(crate) fn alloc_str(&mut self, s: String) -> StrHandle {
        let idx = u32::try_from(self.strs.len()).unwrap_or(u32::MAX);
        self.strs.push(s);
        StrHandle(idx)
    }

    pub(crate) fn alloc_str_from_str(&mut self, s: &str) -> StrHandle {
        self.alloc_str(s.into())
    }

    pub(crate) fn bytes(&self, h: BytesHandle) -> Option<&[u8]> {
        self.bytes.get(h.0 as usize).map(|b| b.as_slice())
    }

    pub(crate) fn str(&self, h: StrHandle) -> Option<&str> {
        self.strs.get(h.0 as usize).map(|s| s.as_str())
    }
}
