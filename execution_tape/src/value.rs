// Copyright 2026 the Execution Tape Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Runtime value model for `execution_tape`.
//!
//! This is the representation used by the interpreter and host ABI. It is intentionally small and
//! `no_std + alloc` friendly.

use alloc::string::String;
use alloc::vec::Vec;

use crate::program::{ElemTypeId, HostTypeId, TypeId};

/// An opaque handle to a host-owned object.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct ObjHandle(pub u64);

/// A host object value.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Obj {
    /// Host-defined type id.
    pub host_type: HostTypeId,
    /// Host-defined handle.
    pub handle: ObjHandle,
}

/// A function identifier.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct FuncId(pub u32);

/// A handle to an aggregate heap value.
///
/// Aggregates are immutable and structural; v1 stores them out-of-line in an aggregate heap.
/// A handle belongs to its originating heap. Copying it does not retain data or transfer it to
/// another heap. Host-call handles are borrowed; [`crate::host::HostContext::retain`] creates
/// an independent snapshot. Returned VM handles stay valid until the heap is explicitly cleared.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct AggHandle(pub u32);

/// A closure value (function + captured environment).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Closure {
    /// Function identifier for the closure body.
    pub func: FuncId,
    /// Captured immutable environment.
    pub env: AggHandle,
}

/// Exact decimal with a signed 128-bit mantissa and per-value scale.
///
/// Its numeric value is `mantissa × 10^(-scale)`. Construction preserves both parts:
/// equality and hashing distinguish `1.0` from `1.00`. Addition and subtraction require
/// equal scales; multiplication adds scales. Rounding and division remain host policies.
///
/// The mantissa uses byte-aligned storage to avoid imposing 16-byte alignment on
/// [`Value`] and inspection records. This is an implementation detail, not a stable ABI.
#[derive(Copy, Clone, PartialEq, Eq, Hash)]
pub struct Decimal {
    mantissa: [u8; 16],
    scale: u8,
}

impl Decimal {
    /// Constructs a decimal without normalization or rounding.
    #[must_use]
    pub const fn new(mantissa: i128, scale: u8) -> Self {
        Self {
            mantissa: mantissa.to_le_bytes(),
            scale,
        }
    }

    /// Returns the signed integer mantissa.
    #[must_use]
    pub const fn mantissa(self) -> i128 {
        i128::from_le_bytes(self.mantissa)
    }

    /// Returns the base-10 scale (number of fractional digits).
    #[must_use]
    pub const fn scale(self) -> u8 {
        self.scale
    }

    pub(crate) fn packed(self) -> [u8; 17] {
        let mut bytes = [0; 17];
        bytes[..16].copy_from_slice(&self.mantissa);
        bytes[16] = self.scale;
        bytes
    }

    pub(crate) fn from_packed(bytes: &[u8]) -> Option<Self> {
        let bytes: &[u8; 17] = bytes.try_into().ok()?;
        Some(Self {
            mantissa: bytes[..16].try_into().ok()?,
            scale: bytes[16],
        })
    }
}

impl core::fmt::Debug for Decimal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Decimal")
            .field("mantissa", &self.mantissa())
            .field("scale", &self.scale())
            .finish()
    }
}

/// A runtime value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// `()`.
    Unit,
    /// Boolean.
    Bool(bool),
    /// Signed 64-bit integer.
    I64(i64),
    /// Unsigned 64-bit integer.
    U64(u64),
    /// 64-bit float.
    F64(f64),
    /// Decimal.
    Decimal(Decimal),
    /// Byte string.
    Bytes(Vec<u8>),
    /// UTF-8 string.
    Str(String),
    /// Host object.
    Obj(Obj),
    /// Aggregate handle (tuple/struct/array).
    Agg(AggHandle),
    /// Function reference.
    Func(FuncId),
    /// Closure reference (function + captured environment).
    Closure(Closure),
}

/// Aggregate type descriptor for host signatures and reflection.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum AggType {
    /// Tuple with fixed arity.
    Tuple {
        /// Number of elements.
        arity: u32,
    },
    /// Struct type by [`TypeId`].
    Struct {
        /// Struct type id.
        type_id: TypeId,
    },
    /// Array type by [`ElemTypeId`].
    Array {
        /// Element type id.
        elem_type_id: ElemTypeId,
    },
}
