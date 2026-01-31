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
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct AggHandle(pub u32);

/// Decimal value with per-value scale.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Decimal {
    /// Integer mantissa.
    pub mantissa: i64,
    /// Base-10 scale (fractional digits).
    pub scale: u8,
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
